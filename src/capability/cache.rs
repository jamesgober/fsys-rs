//! On-disk capability cache file IO (1.1.0).
//!
//! Loads, validates, and writes the TOML-formatted capability cache.
//! See [`super`] for the schema, location, and invalidation policy.
//!
//! ## Atomicity
//!
//! Writes go through an atomic-replace pattern: the document is written
//! to a uniquely named temporary file in the cache directory (created
//! with `create_new`, so an existing file or symlink at that name is
//! never opened or followed; mode `0600` on Unix), flushed with
//! `sync_all`, then renamed over `capabilities.toml`. A crash leaves
//! either the old file or the new one, never a partial file. The
//! directory itself is not fsynced, so after a crash the rename may not
//! have happened, which only costs a re-probe.
//!
//! ## Trust boundary
//!
//! The cache is parsed defensively: unparseable files, files missing any
//! schema key, out-of-range integers, and stale entries (see
//! [`super`]) all read as "no cache" and trigger a re-probe rather than
//! an error. The *values* of a well-formed, fresh cache are not
//! re-verified against the live system: anyone who can write the cache
//! file (the current user, or whoever controls `FSYS_CACHE_DIR`) can
//! change what [`super::capabilities()`] reports, including
//! `spdk_eligible`. Keep the cache directory private to the user that
//! runs fsys.

use super::toml_lite::{Document, Value};
use super::types::{Capabilities, HardwareSummary, IoUringFeature, PciAddress, SpdkSkipReason};
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Maximum age of a cached entry before it is considered stale, in
/// seconds. Mirrors [`super::CAPABILITY_CACHE_MAX_AGE_DAYS`].
pub(crate) const MAX_AGE_SECS: u64 = super::CAPABILITY_CACHE_MAX_AGE_DAYS * 24 * 60 * 60;

/// Returns the canonical cache file path for this platform.
///
/// Honors `FSYS_CACHE_DIR` first; otherwise per-OS XDG / Local-AppData
/// conventions. Returns `None` only when no candidate directory could
/// be derived (no `HOME`, no `LOCALAPPDATA`, no override) — that case
/// disables caching for the process.
#[must_use]
pub fn cache_file_path() -> Option<PathBuf> {
    cache_dir().map(|d| d.join("capabilities.toml"))
}

/// Returns the cache directory.
#[must_use]
pub fn cache_dir() -> Option<PathBuf> {
    if let Ok(v) = std::env::var("FSYS_CACHE_DIR") {
        if !v.is_empty() {
            return Some(PathBuf::from(v));
        }
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("fsys"))
    }
    #[cfg(not(target_os = "windows"))]
    {
        if let Some(d) = std::env::var_os("XDG_CACHE_HOME") {
            return Some(PathBuf::from(d).join("fsys"));
        }
        if let Some(home) = std::env::var_os("HOME") {
            return Some(PathBuf::from(home).join(".cache").join("fsys"));
        }
        None
    }
}

/// Attempts to load the cached capabilities.
///
/// Returns `Ok(Some(_))` when a valid, fresh cache was loaded.
/// Returns `Ok(None)` when the cache is missing, stale (any
/// invalidation trigger fired), or unparseable — in every "Ok(None)"
/// case the caller should re-probe.
/// Returns `Err(_)` only for unexpected IO failures (the path was
/// found and the file was present but reading it failed for a reason
/// we want to surface rather than swallow).
///
/// # Errors
///
/// Surfaces underlying [`io::Error`] for unexpected read failures
/// (other than `NotFound`, which collapses to `Ok(None)`).
pub fn load() -> io::Result<Option<Capabilities>> {
    let Some(path) = cache_file_path() else {
        return Ok(None);
    };
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let contents = match std::str::from_utf8(&bytes) {
        Ok(s) => s,
        Err(_) => return Ok(None), // not valid UTF-8 → re-probe
    };
    let doc = match Document::parse(contents) {
        Some(d) => d,
        None => return Ok(None),
    };
    let caps = match document_to_capabilities(&doc) {
        Some(c) => c,
        None => return Ok(None),
    };
    if is_stale(&caps) {
        return Ok(None);
    }
    Ok(Some(caps))
}

/// Persists `caps` to the cache file via atomic temp + rename.
///
/// Returns `Ok(())` on success or when the cache directory could not
/// be resolved (caching disabled for this process — non-fatal).
/// Returns `Err(_)` on disk IO failures.
///
/// # Errors
///
/// Surfaces the underlying [`io::Error`] when the destination
/// directory exists but writing fails (permissions, disk full, etc.).
pub fn store(caps: &Capabilities) -> io::Result<()> {
    let Some(path) = cache_file_path() else {
        return Ok(());
    };
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    std::fs::create_dir_all(parent)?;
    let doc = capabilities_to_document(caps);
    let (tmp, mut file) = create_unique_temp(parent)?;
    // Write, flush to stable storage, then rename over the destination.
    // On any failure (disk full, EXDEV, the destination held open by
    // another process on Windows) the temp file is removed rather than
    // left next to the cache.
    let written = file
        .write_all(doc.serialize().as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, &path)) {
        // Best-effort cleanup; if the removal also fails, the original
        // error is the one worth reporting.
        let _cleanup = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Creates a new, uniquely named temporary file in `dir` for [`store`].
///
/// The name combines the process id, a timestamp and a process-wide
/// counter, so concurrent writers (threads or processes) never share a
/// temp file. `create_new` fails instead of opening an existing file or
/// following a symlink planted at that name. On Unix the file is created
/// with mode `0600`; on Windows it inherits the ACL of the per-user
/// cache directory (`%LOCALAPPDATA%`), which has no mode bits to set.
fn create_unique_temp(dir: &Path) -> io::Result<(PathBuf, File)> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    for _ in 0..16 {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = dir.join(format!(
            "capabilities.toml.{}.{nanos}.{n}.tmp",
            std::process::id()
        ));
        let mut options = std::fs::OpenOptions::new();
        let _ = options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let _ = options.mode(0o600);
        }
        match options.open(&candidate) {
            Ok(f) => return Ok((candidate, f)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a unique capability cache temp file",
    ))
}

/// Deletes the cache file.
///
/// Returns `Ok(())` whether the file existed or not.
///
/// # Errors
///
/// Surfaces the underlying [`io::Error`] when the file existed but
/// could not be deleted (permissions, hardware error).
pub fn invalidate() -> io::Result<()> {
    let Some(path) = cache_file_path() else {
        return Ok(());
    };
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Applies the freshness checks documented in [`super`]. Returns
/// `true` if the cache entry is stale and should be re-probed.
pub(crate) fn is_stale(caps: &Capabilities) -> bool {
    // 1. Schema mismatch.
    if caps.schema_version != super::CAPABILITY_CACHE_SCHEMA_VERSION {
        return true;
    }
    // 2. fsys version mismatch.
    if caps.fsys_version != env!("CARGO_PKG_VERSION") {
        return true;
    }
    // 3. Kernel / OS version mismatch.
    if caps.kernel_version != super::probe::kernel_version_string() {
        return true;
    }
    // 4. OS target mismatch — defends against shared $HOME across
    //    cross-compiled builds (rare in practice, defensive).
    if caps.os_target != std::env::consts::OS {
        return true;
    }
    // 5. Age — entries older than MAX_AGE_SECS are considered stale.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    if now.saturating_sub(caps.probed_at_unix_secs) >= MAX_AGE_SECS {
        return true;
    }
    false
}

// ──────────────────────────────────────────────────────────────────
// Serialisation
// ──────────────────────────────────────────────────────────────────

fn capabilities_to_document(caps: &Capabilities) -> Document {
    let mut doc = Document::new();
    doc.set_root("schema_version", Value::Integer(caps.schema_version as i64));
    doc.set_root("fsys_version", Value::String(caps.fsys_version.clone()));
    doc.set_root("kernel_version", Value::String(caps.kernel_version.clone()));
    doc.set_root("os_target", Value::String(caps.os_target.clone()));
    doc.set_root(
        "probed_at_unix_secs",
        Value::Integer(caps.probed_at_unix_secs as i64),
    );

    doc.set("capabilities", "io_uring", Value::Boolean(caps.io_uring));
    doc.set(
        "capabilities",
        "io_uring_features",
        Value::StringArray(
            caps.io_uring_features
                .iter()
                .map(|f| f.as_str().to_string())
                .collect(),
        ),
    );
    doc.set(
        "capabilities",
        "nvme_passthrough",
        Value::Boolean(caps.nvme_passthrough),
    );
    doc.set("capabilities", "direct_io", Value::Boolean(caps.direct_io));
    doc.set(
        "capabilities",
        "plp_detected",
        Value::Boolean(caps.plp_detected),
    );
    doc.set(
        "capabilities",
        "spdk_eligible",
        Value::Boolean(caps.spdk_eligible),
    );
    doc.set(
        "capabilities",
        "spdk_skip_reasons",
        Value::StringArray(
            caps.spdk_skip_reasons
                .iter()
                .map(skip_reason_to_token)
                .collect(),
        ),
    );
    doc.set(
        "capabilities",
        "spdk_eligible_devices",
        Value::StringArray(
            caps.spdk_eligible_devices
                .iter()
                .map(|p| p.to_canonical())
                .collect(),
        ),
    );

    doc.set(
        "hardware",
        "drive_type",
        Value::String(caps.hardware.drive_type.clone()),
    );
    doc.set(
        "hardware",
        "optimal_block_size",
        Value::Integer(caps.hardware.optimal_block_size as i64),
    );
    doc.set(
        "hardware",
        "queue_depth",
        Value::Integer(caps.hardware.queue_depth as i64),
    );
    doc.set(
        "hardware",
        "sector_size_logical",
        Value::Integer(caps.hardware.sector_size_logical as i64),
    );
    doc.set(
        "hardware",
        "sector_size_physical",
        Value::Integer(caps.hardware.sector_size_physical as i64),
    );
    doc
}

/// Rebuilds a [`Capabilities`] from a parsed cache document.
///
/// Every key the writer emits is required: a missing key, a value of the
/// wrong kind, or an integer outside its field's range returns `None`, so
/// a truncated or hand-edited file is treated as "no cache" and
/// re-probed instead of being filled in with defaults.
fn document_to_capabilities(doc: &Document) -> Option<Capabilities> {
    let schema_version = u32::try_from(doc.root_int("schema_version")?).ok()?;
    let fsys_version = doc.root_string("fsys_version")?;
    let kernel_version = doc.root_string("kernel_version")?;
    let os_target = doc.root_string("os_target")?;
    let probed_at_unix_secs = u64::try_from(doc.root_int("probed_at_unix_secs")?).ok()?;

    let io_uring = doc.section_bool("capabilities", "io_uring")?;
    let io_uring_features: Vec<IoUringFeature> = doc
        .section_strings("capabilities", "io_uring_features")?
        .iter()
        .filter_map(|s| IoUringFeature::from_str_canonical(s))
        .collect();
    let nvme_passthrough = doc.section_bool("capabilities", "nvme_passthrough")?;
    let direct_io = doc.section_bool("capabilities", "direct_io")?;
    let plp_detected = doc.section_bool("capabilities", "plp_detected")?;
    let spdk_eligible = doc.section_bool("capabilities", "spdk_eligible")?;
    let spdk_skip_reasons: Vec<SpdkSkipReason> = doc
        .section_strings("capabilities", "spdk_skip_reasons")?
        .iter()
        .filter_map(|s| skip_reason_from_token(s))
        .collect();
    let spdk_eligible_devices: Vec<PciAddress> = doc
        .section_strings("capabilities", "spdk_eligible_devices")?
        .iter()
        .filter_map(|s| PciAddress::parse(s))
        .collect();

    let section_u32 =
        |key: &str| -> Option<u32> { u32::try_from(doc.section_int("hardware", key)?).ok() };
    let drive_type = doc.section_string("hardware", "drive_type")?;
    let optimal_block_size = section_u32("optimal_block_size")?;
    let queue_depth = section_u32("queue_depth")?;
    let sector_size_logical = section_u32("sector_size_logical")?;
    let sector_size_physical = section_u32("sector_size_physical")?;

    let hardware = HardwareSummary {
        drive_type,
        optimal_block_size,
        queue_depth,
        sector_size_logical,
        sector_size_physical,
        io_uring,
        nvme_passthrough,
        direct_io,
        plp_detected,
    };

    Some(Capabilities {
        schema_version,
        fsys_version,
        kernel_version,
        os_target,
        probed_at_unix_secs,
        io_uring,
        io_uring_features,
        nvme_passthrough,
        direct_io,
        plp_detected,
        spdk_eligible,
        spdk_skip_reasons,
        spdk_eligible_devices,
        hardware,
    })
}

/// SPDK skip-reason tokenisation. The cache file stores reasons as
/// short snake_case identifiers (`"not_linux"`, `"no_nvme_devices"`,
/// `"hugepages_lt_<mb>"`, etc.) rather than the full Display strings
/// so the cache format is stable across Display-text refinements.
fn skip_reason_to_token(r: &SpdkSkipReason) -> String {
    match r {
        SpdkSkipReason::NotLinux => "not_linux".to_string(),
        SpdkSkipReason::HugepagesNotConfigured {
            current_mb,
            recommended_mb,
        } => format!("hugepages_lt:{current_mb}:{recommended_mb}"),
        SpdkSkipReason::InsufficientPrivileges => "insufficient_privileges".to_string(),
        SpdkSkipReason::NoNvmeDevices => "no_nvme_devices".to_string(),
        SpdkSkipReason::AllDevicesInUse { devices } => {
            let list: Vec<String> = devices.iter().map(|d| d.to_canonical()).collect();
            format!("all_devices_in_use:{}", list.join(","))
        }
        SpdkSkipReason::IommuNotConfigured => "iommu_not_configured".to_string(),
        SpdkSkipReason::InsufficientCores {
            available,
            recommended,
        } => format!("insufficient_cores:{available}:{recommended}"),
        SpdkSkipReason::SpdkLibraryNotFound => "spdk_library_not_found".to_string(),
    }
}

fn skip_reason_from_token(s: &str) -> Option<SpdkSkipReason> {
    if s == "not_linux" {
        return Some(SpdkSkipReason::NotLinux);
    }
    if s == "insufficient_privileges" {
        return Some(SpdkSkipReason::InsufficientPrivileges);
    }
    if s == "no_nvme_devices" {
        return Some(SpdkSkipReason::NoNvmeDevices);
    }
    if s == "iommu_not_configured" {
        return Some(SpdkSkipReason::IommuNotConfigured);
    }
    if s == "spdk_library_not_found" {
        return Some(SpdkSkipReason::SpdkLibraryNotFound);
    }
    if let Some(rest) = s.strip_prefix("hugepages_lt:") {
        let parts: Vec<&str> = rest.split(':').collect();
        if parts.len() == 2 {
            let current_mb = parts[0].parse::<u64>().ok()?;
            let recommended_mb = parts[1].parse::<u64>().ok()?;
            return Some(SpdkSkipReason::HugepagesNotConfigured {
                current_mb,
                recommended_mb,
            });
        }
    }
    if let Some(rest) = s.strip_prefix("all_devices_in_use:") {
        let devices: Vec<PciAddress> = rest.split(',').filter_map(PciAddress::parse).collect();
        return Some(SpdkSkipReason::AllDevicesInUse { devices });
    }
    if let Some(rest) = s.strip_prefix("insufficient_cores:") {
        let parts: Vec<&str> = rest.split(':').collect();
        if parts.len() == 2 {
            let available = parts[0].parse::<usize>().ok()?;
            let recommended = parts[1].parse::<usize>().ok()?;
            return Some(SpdkSkipReason::InsufficientCores {
                available,
                recommended,
            });
        }
    }
    None
}

/// Process-global test-only lock for cache tests that mutate
/// `FSYS_CACHE_DIR` or call functions that read it
/// (`store` / `load` / `invalidate` / `probe_capabilities_fresh`).
/// Shared between `capability::cache::tests` and
/// `capability::tests` so concurrent test execution within the
/// same test binary doesn't race on the env var.
///
/// Pulling in `serial_test` for this would violate REPS § Dependency
/// Management; one static `Mutex<()>` is enough.
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    use TEST_ENV_LOCK as ENV_LOCK;

    fn fixture_caps() -> Capabilities {
        Capabilities {
            schema_version: super::super::CAPABILITY_CACHE_SCHEMA_VERSION,
            fsys_version: env!("CARGO_PKG_VERSION").to_string(),
            kernel_version: super::super::probe::kernel_version_string(),
            os_target: std::env::consts::OS.to_string(),
            probed_at_unix_secs: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            io_uring: true,
            io_uring_features: vec![IoUringFeature::CoopTaskrun, IoUringFeature::SingleIssuer],
            nvme_passthrough: false,
            direct_io: true,
            plp_detected: false,
            spdk_eligible: false,
            spdk_skip_reasons: vec![SpdkSkipReason::NotLinux],
            spdk_eligible_devices: vec![],
            hardware: HardwareSummary {
                drive_type: "nvme".to_string(),
                optimal_block_size: 4096,
                queue_depth: 64,
                sector_size_logical: 512,
                sector_size_physical: 4096,
                io_uring: true,
                nvme_passthrough: false,
                direct_io: true,
                plp_detected: false,
            },
        }
    }

    #[test]
    fn test_round_trip_through_document() {
        let caps = fixture_caps();
        let doc = capabilities_to_document(&caps);
        let serialised = doc.serialize();
        let parsed = Document::parse(&serialised).expect("parse");
        let reconstructed = document_to_capabilities(&parsed).expect("reconstruct");
        assert_eq!(reconstructed.schema_version, caps.schema_version);
        assert_eq!(reconstructed.fsys_version, caps.fsys_version);
        assert_eq!(reconstructed.kernel_version, caps.kernel_version);
        assert_eq!(reconstructed.os_target, caps.os_target);
        assert_eq!(reconstructed.probed_at_unix_secs, caps.probed_at_unix_secs);
        assert_eq!(reconstructed.io_uring, caps.io_uring);
        assert_eq!(
            reconstructed.io_uring_features.len(),
            caps.io_uring_features.len()
        );
        assert_eq!(reconstructed.nvme_passthrough, caps.nvme_passthrough);
        assert_eq!(reconstructed.direct_io, caps.direct_io);
        assert_eq!(reconstructed.plp_detected, caps.plp_detected);
        assert_eq!(reconstructed.spdk_eligible, caps.spdk_eligible);
        assert_eq!(
            reconstructed.spdk_skip_reasons.len(),
            caps.spdk_skip_reasons.len()
        );
        assert_eq!(reconstructed.hardware.drive_type, caps.hardware.drive_type);
        assert_eq!(
            reconstructed.hardware.optimal_block_size,
            caps.hardware.optimal_block_size
        );
    }

    #[test]
    fn test_is_stale_when_schema_version_mismatches() {
        let mut caps = fixture_caps();
        caps.schema_version = caps.schema_version.wrapping_add(1);
        assert!(is_stale(&caps));
    }

    #[test]
    fn test_is_stale_when_fsys_version_mismatches() {
        let mut caps = fixture_caps();
        caps.fsys_version = "999.999.999".to_string();
        assert!(is_stale(&caps));
    }

    #[test]
    fn test_is_stale_when_kernel_version_mismatches() {
        let mut caps = fixture_caps();
        caps.kernel_version = "fictional-kernel-9.9.9".to_string();
        assert!(is_stale(&caps));
    }

    #[test]
    fn test_is_stale_when_os_target_mismatches() {
        let mut caps = fixture_caps();
        caps.os_target = "freebsd".to_string();
        assert!(is_stale(&caps));
    }

    #[test]
    fn test_is_stale_when_older_than_max_age() {
        let mut caps = fixture_caps();
        caps.probed_at_unix_secs = 1; // Jan 1 1970 +1s
        assert!(is_stale(&caps));
    }

    #[test]
    fn test_is_not_stale_with_fresh_fixture() {
        let caps = fixture_caps();
        assert!(!is_stale(&caps));
    }

    #[test]
    fn test_skip_reason_token_round_trip_simple_variants() {
        for r in [
            SpdkSkipReason::NotLinux,
            SpdkSkipReason::InsufficientPrivileges,
            SpdkSkipReason::NoNvmeDevices,
            SpdkSkipReason::IommuNotConfigured,
            SpdkSkipReason::SpdkLibraryNotFound,
        ] {
            let tok = skip_reason_to_token(&r);
            let parsed = skip_reason_from_token(&tok).expect("parse");
            assert_eq!(parsed, r);
        }
    }

    #[test]
    fn test_skip_reason_token_hugepages_round_trip() {
        let r = SpdkSkipReason::HugepagesNotConfigured {
            current_mb: 64,
            recommended_mb: 1024,
        };
        let tok = skip_reason_to_token(&r);
        assert!(tok.starts_with("hugepages_lt:"));
        let parsed = skip_reason_from_token(&tok).expect("parse");
        assert_eq!(parsed, r);
    }

    #[test]
    fn test_skip_reason_token_cores_round_trip() {
        let r = SpdkSkipReason::InsufficientCores {
            available: 2,
            recommended: 4,
        };
        let tok = skip_reason_to_token(&r);
        let parsed = skip_reason_from_token(&tok).expect("parse");
        assert_eq!(parsed, r);
    }

    #[test]
    fn test_skip_reason_token_all_devices_in_use_round_trip() {
        let r = SpdkSkipReason::AllDevicesInUse {
            devices: vec![
                PciAddress::new(0, 0x1f, 0x03, 2),
                PciAddress::new(0, 0x20, 0x00, 0),
            ],
        };
        let tok = skip_reason_to_token(&r);
        let parsed = skip_reason_from_token(&tok).expect("parse");
        assert_eq!(parsed, r);
    }

    #[test]
    fn test_skip_reason_token_unknown_returns_none() {
        assert!(skip_reason_from_token("not_a_real_token").is_none());
        assert!(skip_reason_from_token("hugepages_lt:not_a_number").is_none());
        assert!(skip_reason_from_token("insufficient_cores:abc:def").is_none());
    }

    /// Wraps a test that mutates `FSYS_CACHE_DIR`. Holds [`ENV_LOCK`]
    /// for the body so concurrent tests don't race on the global var.
    fn with_cache_dir<F: FnOnce(&std::path::Path)>(label: &str, f: F) {
        let guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let key = "FSYS_CACHE_DIR";
        let saved = std::env::var(key).ok();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("fsys-cache-{label}-{nanos}"));
        std::env::set_var(key, &dir);

        f(&dir);

        let _ = std::fs::remove_dir_all(&dir);
        match saved {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        drop(guard);
    }

    #[test]
    fn test_cache_dir_honours_override() {
        let guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let key = "FSYS_CACHE_DIR";
        let saved = std::env::var(key).ok();
        std::env::set_var(key, "/tmp/fsys-cache-test-override");
        let resolved = cache_dir();
        assert_eq!(
            resolved,
            Some(PathBuf::from("/tmp/fsys-cache-test-override"))
        );
        match saved {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        drop(guard);
    }

    #[test]
    fn test_store_and_load_round_trip_via_temp_override() {
        with_cache_dir("rtt", |_dir| {
            let caps = fixture_caps();
            store(&caps).expect("store");
            let loaded = load().expect("load").expect("present");
            assert_eq!(loaded.schema_version, caps.schema_version);
            assert_eq!(loaded.fsys_version, caps.fsys_version);
            assert_eq!(loaded.kernel_version, caps.kernel_version);
            assert_eq!(loaded.os_target, caps.os_target);
        });
    }

    #[test]
    fn test_invalidate_removes_existing_file() {
        with_cache_dir("inv", |_dir| {
            let caps = fixture_caps();
            store(&caps).expect("store");
            invalidate().expect("invalidate");
            assert!(matches!(load(), Ok(None)));
            // Idempotent — second invalidate on a missing file is Ok.
            invalidate().expect("invalidate idempotent");
        });
    }

    #[test]
    fn test_document_missing_any_key_is_rejected() {
        let full = capabilities_to_document(&fixture_caps()).serialize();
        assert!(document_to_capabilities(&Document::parse(&full).expect("parse")).is_some());
        // Drop each `key = value` line in turn; every key is required.
        let lines: Vec<&str> = full.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            if !line.contains(" = ") {
                continue;
            }
            let partial: String = lines
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, l)| format!("{l}\n"))
                .collect();
            let doc = Document::parse(&partial).expect("parse partial");
            assert!(
                document_to_capabilities(&doc).is_none(),
                "cache without `{line}` must be rejected"
            );
        }
    }

    #[test]
    fn test_document_rejects_out_of_range_integers() {
        let text = capabilities_to_document(&fixture_caps())
            .serialize()
            .replace("queue_depth = 64", "queue_depth = -1");
        let doc = Document::parse(&text).expect("parse");
        assert!(document_to_capabilities(&doc).is_none());
    }

    #[test]
    fn test_store_leaves_only_the_cache_file() {
        with_cache_dir("tidy", |dir| {
            let caps = fixture_caps();
            store(&caps).expect("store 1");
            store(&caps).expect("store 2");
            let names: Vec<String> = std::fs::read_dir(dir)
                .expect("read_dir")
                .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
                .collect();
            assert_eq!(names, vec!["capabilities.toml".to_string()]);
        });
    }

    #[test]
    fn test_create_unique_temp_never_reuses_a_name() {
        with_cache_dir("unique", |dir| {
            std::fs::create_dir_all(dir).expect("mkdir");
            let (a, fa) = create_unique_temp(dir).expect("a");
            let (b, fb) = create_unique_temp(dir).expect("b");
            assert_ne!(a, b);
            drop((fa, fb));
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_store_creates_owner_only_file() {
        use std::os::unix::fs::PermissionsExt;
        with_cache_dir("mode", |dir| {
            store(&fixture_caps()).expect("store");
            let mode = std::fs::metadata(dir.join("capabilities.toml"))
                .expect("meta")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        });
    }

    #[test]
    fn test_load_returns_ok_none_on_corrupt_cache() {
        with_cache_dir("corrupt", |dir| {
            std::fs::create_dir_all(dir).expect("mkdir");
            std::fs::write(
                dir.join("capabilities.toml"),
                "this is not toml\x00\x01\x02",
            )
            .expect("write");
            assert!(matches!(load(), Ok(None)));
        });
    }
}
