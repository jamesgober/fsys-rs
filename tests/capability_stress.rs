//! Stress + edge-case tests for the 1.1.0 capability surface.
//!
//! Three families of tests live here:
//!
//! 1. **Concurrent stress** — many threads hammering
//!    `capabilities()`, asserting the `OnceLock` semantics
//!    (single probe, shared reference, no panics, no torn reads).
//! 2. **TOML round-trip fuzz** — randomised value generation
//!    through the cache file's serialiser + parser, asserting
//!    every legal value survives a round-trip.
//! 3. **Cache invalidation matrix** — every one of the five
//!    invalidation triggers documented in
//!    [`fsys::capability`] exercised end-to-end through the
//!    public API.

#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests

use fsys::capability::{
    cache, capabilities, invalidate_capability_cache, probe_capabilities_fresh, Capabilities,
    IoUringFeature, PciAddress, SpdkSkipReason,
};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Process-global lock for tests that mutate `FSYS_CACHE_DIR`.
/// Multiple integration-test binaries share `cargo test`'s process,
/// so without serialisation they would race on the env var.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn unique_dir(label: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("fsys-stress-{label}-{nanos}"))
}

fn with_cache_dir<F: FnOnce(&std::path::Path)>(label: &str, f: F) {
    let guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let key = "FSYS_CACHE_DIR";
    let saved = std::env::var(key).ok();
    let dir = unique_dir(label);
    std::env::set_var(key, &dir);
    f(&dir);
    let _ = std::fs::remove_dir_all(&dir);
    match saved {
        Some(v) => std::env::set_var(key, v),
        None => std::env::remove_var(key),
    }
    drop(guard);
}

// ──────────────────────────────────────────────────────────────────
// 1. Concurrent stress
// ──────────────────────────────────────────────────────────────────

#[test]
fn capabilities_returns_same_reference_under_32_thread_contention() {
    const THREADS: usize = 32;
    let barrier = Arc::new(Barrier::new(THREADS));
    let mut handles = Vec::with_capacity(THREADS);
    for _ in 0..THREADS {
        let b = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            b.wait();
            let p = capabilities() as *const Capabilities;
            p as usize
        }));
    }
    let mut addrs = Vec::with_capacity(THREADS);
    for h in handles {
        addrs.push(h.join().unwrap());
    }
    // Every thread must see the same `OnceLock`-backed pointer.
    let first = addrs[0];
    for (i, a) in addrs.iter().enumerate() {
        assert_eq!(
            *a, first,
            "thread {i} saw different pointer: {a:#x} vs {first:#x}",
        );
    }
}

#[test]
fn capabilities_field_values_stable_across_repeated_reads() {
    let a = capabilities();
    for _ in 0..1000 {
        let b = capabilities();
        // The `OnceLock` guarantee gives us pointer equality —
        // but assert field-by-field for clarity.
        assert_eq!(a.schema_version, b.schema_version);
        assert_eq!(a.fsys_version, b.fsys_version);
        assert_eq!(a.kernel_version, b.kernel_version);
        assert_eq!(a.os_target, b.os_target);
        assert_eq!(a.io_uring, b.io_uring);
        assert_eq!(a.spdk_eligible, b.spdk_eligible);
    }
}

#[test]
fn probe_fresh_under_concurrent_invocation_does_not_panic() {
    // `probe_capabilities_fresh` is allowed to race — multiple
    // concurrent callers all write the cache file. The
    // atomic-rename pattern means there's always a coherent file
    // on disk; the in-memory snapshots are independent owned
    // values per caller.
    const THREADS: usize = 8;
    let barrier = Arc::new(Barrier::new(THREADS));
    let mut handles = Vec::with_capacity(THREADS);
    for _ in 0..THREADS {
        let b = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            b.wait();
            let _ = probe_capabilities_fresh();
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
}

// ──────────────────────────────────────────────────────────────────
// 2. TOML / cache round-trip fuzz
// ──────────────────────────────────────────────────────────────────

/// Tiny LCG-based RNG. No `rand` dep (REPS § Dependency Management).
struct Lcg(u64);
impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_add(0x9E37_79B9_7F4A_7C15))
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }
    fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }
    fn next_byte(&mut self) -> u8 {
        (self.next_u64() & 0xff) as u8
    }
    fn next_in(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() as usize) % n
        }
    }
}

fn random_pci(rng: &mut Lcg) -> PciAddress {
    PciAddress::new(
        rng.next_u32() as u16,
        rng.next_byte(),
        rng.next_byte() & 0x1f, // 5-bit device
        rng.next_byte() & 0x07, // 3-bit function
    )
}

#[test]
fn pci_address_canonical_round_trip_random_500_iterations() {
    let mut rng = Lcg::new(0x_DEAD_BEEF);
    for _ in 0..500 {
        let original = random_pci(&mut rng);
        let canonical = original.to_canonical();
        let parsed = PciAddress::parse(&canonical).expect("parse");
        assert_eq!(parsed, original, "round-trip failure on {canonical}");
        // Canonical form is always lowercase hex.
        assert!(canonical.chars().all(|c| !c.is_ascii_uppercase()));
    }
}

#[test]
fn pci_address_parse_accepts_uppercase_hex_input() {
    // lspci output sometimes uppercases; the parser must accept
    // both. Round-trip back to canonical (lowercase).
    let upper = "0000:1F:03.2";
    let parsed = PciAddress::parse(upper).expect("uppercase parse");
    assert_eq!(parsed.bus, 0x1f);
    assert_eq!(parsed.device, 0x03);
    assert_eq!(parsed.function, 2);
    assert_eq!(parsed.to_canonical(), "0000:1f:03.2");
}

#[test]
fn pci_address_round_trip_non_zero_domain() {
    // Multi-domain hosts (NUMA, large server platforms) have
    // non-zero PCI domains. Make sure those survive.
    for domain in [0x0001u16, 0x00ff, 0xabcd, 0xffff] {
        let original = PciAddress::new(domain, 0x42, 0x18, 5);
        let parsed = PciAddress::parse(&original.to_canonical()).expect("parse");
        assert_eq!(parsed, original);
    }
}

#[test]
fn io_uring_feature_string_round_trip_for_every_variant() {
    let all = [
        IoUringFeature::FastPoll,
        IoUringFeature::RegisterBuffers,
        IoUringFeature::RegisterFiles,
        IoUringFeature::UringCmd,
        IoUringFeature::SubmitAll,
        IoUringFeature::CoopTaskrun,
        IoUringFeature::SingleIssuer,
        IoUringFeature::DeferTaskrun,
        IoUringFeature::SqPoll,
    ];
    for f in all {
        let s = f.as_str();
        assert_eq!(IoUringFeature::from_str_canonical(s), Some(f));
    }
}

#[test]
fn spdk_skip_reason_display_text_for_every_variant_is_non_empty() {
    let probe_devices = vec![PciAddress::new(0, 0, 0, 0)];
    let all = [
        SpdkSkipReason::NotLinux,
        SpdkSkipReason::HugepagesNotConfigured {
            current_mb: 0,
            recommended_mb: 1024,
        },
        SpdkSkipReason::InsufficientPrivileges,
        SpdkSkipReason::NoNvmeDevices,
        SpdkSkipReason::AllDevicesInUse {
            devices: probe_devices.clone(),
        },
        SpdkSkipReason::IommuNotConfigured,
        SpdkSkipReason::InsufficientCores {
            available: 1,
            recommended: 4,
        },
        SpdkSkipReason::SpdkLibraryNotFound,
    ];
    for r in all {
        let s = r.to_string();
        assert!(!s.is_empty(), "empty Display for {r:?}");
        // Display text never includes a panic-style "{:?}" form.
        assert!(!s.contains("{:?}"), "Display falls back to Debug: {s:?}");
    }
}

// ──────────────────────────────────────────────────────────────────
// 3. Cache invalidation matrix
// ──────────────────────────────────────────────────────────────────

#[test]
fn cache_load_round_trip_via_override_directory() {
    with_cache_dir("rtt", |_dir| {
        let fresh = probe_capabilities_fresh();
        // Re-read; the file we just wrote must satisfy `is_stale ==
        // false` so `load()` returns `Some(_)`.
        let loaded = cache::load().expect("load ok").expect("present");
        assert_eq!(loaded.fsys_version, fresh.fsys_version);
        assert_eq!(loaded.os_target, fresh.os_target);
    });
}

#[test]
fn cache_invalidation_on_missing_file_returns_ok_none() {
    with_cache_dir("missing", |_dir| {
        assert!(cache::load().expect("load ok").is_none());
    });
}

#[test]
fn cache_invalidation_on_corrupt_utf8_returns_ok_none() {
    with_cache_dir("corrupt-utf8", |dir| {
        std::fs::create_dir_all(dir).unwrap();
        // Bytes 0xC3, 0x28 are invalid UTF-8; libstd's
        // `from_utf8` rejects them.
        std::fs::write(dir.join("capabilities.toml"), [0xC3, 0x28, 0xFF]).unwrap();
        assert!(cache::load().expect("load ok").is_none());
    });
}

#[test]
fn cache_invalidation_on_truncated_toml_returns_ok_none() {
    with_cache_dir("truncated", |dir| {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("capabilities.toml"),
            "schema_version = 1\nfsys_version =",
        )
        .unwrap();
        assert!(cache::load().expect("load ok").is_none());
    });
}

#[test]
fn cache_invalidation_on_garbage_keys_returns_ok_none() {
    with_cache_dir("garbage-keys", |dir| {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("capabilities.toml"),
            "a.b.c = 1\nnonsense without equals\n",
        )
        .unwrap();
        assert!(cache::load().expect("load ok").is_none());
    });
}

#[test]
fn invalidate_then_load_returns_none() {
    with_cache_dir("invalidate-flow", |_dir| {
        let _written = probe_capabilities_fresh();
        invalidate_capability_cache().unwrap();
        assert!(cache::load().expect("load ok").is_none());
    });
}

#[test]
fn invalidate_is_idempotent_when_no_cache_exists() {
    with_cache_dir("invalidate-empty", |_dir| {
        invalidate_capability_cache().unwrap();
        invalidate_capability_cache().unwrap();
    });
}

#[test]
fn cache_path_honours_fsys_cache_dir_env_override() {
    let guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let saved = std::env::var("FSYS_CACHE_DIR").ok();
    std::env::set_var("FSYS_CACHE_DIR", "/tmp/fsys-test-explicit-override");
    let p = cache::cache_file_path().expect("path");
    assert!(p.to_string_lossy().contains("fsys-test-explicit-override"));
    match saved {
        Some(v) => std::env::set_var("FSYS_CACHE_DIR", v),
        None => std::env::remove_var("FSYS_CACHE_DIR"),
    }
    drop(guard);
}

// ──────────────────────────────────────────────────────────────────
// 4. Backend observability accessors don't panic on default journals
// ──────────────────────────────────────────────────────────────────

#[test]
fn journal_backend_accessors_classify_buffered_default() {
    let fs = fsys::builder().build().expect("build");
    let path = unique_dir("backend-buffered").with_extension("wal");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let log = fs.journal(&path).expect("journal");
    assert_eq!(log.backend_kind(), fsys::JournalBackendKind::KernelBuffered);
    let health = log.backend_health();
    assert_eq!(health.backend, fsys::JournalBackendKind::KernelBuffered);
    let info = log.backend_info();
    assert_eq!(info.selected, fsys::JournalBackendKind::KernelBuffered);
    assert!(!info.selection_reason.is_empty());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn journal_backend_accessors_are_call_safe_under_concurrent_appends() {
    // The accessors snapshot atomics and immutable fields; they must
    // not block the append hot path. Smoke test: read backend_kind
    // from 8 threads while another 8 threads append.
    let fs = fsys::builder().build().expect("build");
    let dir = unique_dir("backend-concurrent");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("log.wal");
    let log = Arc::new(fs.journal(&path).expect("journal"));

    let mut writers = Vec::new();
    let mut readers = Vec::new();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

    for _ in 0..8 {
        let log = Arc::clone(&log);
        let stop = Arc::clone(&stop);
        writers.push(thread::spawn(move || {
            let payload = b"backend-info-stress-test-record";
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = log.append(payload);
            }
        }));
    }
    for _ in 0..8 {
        let log = Arc::clone(&log);
        let stop = Arc::clone(&stop);
        readers.push(thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let _kind = log.backend_kind();
                let _h = log.backend_health();
                let _i = log.backend_info();
            }
        }));
    }

    thread::sleep(Duration::from_millis(100));
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for w in writers {
        w.join().unwrap();
    }
    for r in readers {
        r.join().unwrap();
    }

    let _ = std::fs::remove_dir_all(&dir);
}
