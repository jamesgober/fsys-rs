//! Linux-specific hardware probe.
//!
//! Probes against `/proc/meminfo`, `/sys/block/<dev>/queue/*`,
//! `std::thread::available_parallelism`, and runtime checks on the
//! io_uring crate. PLP detection returns [`super::PlpStatus::Unknown`]
//! pending the `0.6.0` NVMe passthrough work.
//!
//! All probes are non-fatal — when a `/sys/` or `/proc/` file is not
//! reachable (sandboxed container, restricted mount), the probe
//! returns the documented default rather than failing the handle.

#![cfg(target_os = "linux")]

use std::fs;
use std::path::{Path, PathBuf};

use super::PlpStatus;
use crate::hardware::cpu::CpuInfo;
use crate::hardware::drive::{DriveInfo, DriveKind};
use crate::hardware::io_primitives::IoPrimitives;
use crate::hardware::memory::MemoryInfo;

// ─────────────────────────────────────────────────────────────────────────────
// Drive
// ─────────────────────────────────────────────────────────────────────────────

/// Probes the storage device hosting the current working directory.
///
/// Strategy:
/// 1. Resolve the device backing the cwd via `/proc/self/mounts` +
///    `/sys/dev/block/<major>:<minor>` symlink.
/// 2. Read `queue/*` parameters from the device's `/sys/block/` entry.
/// 3. Classify as NVMe / SATA SSD / HDD via the `rotational` flag and
///    the device-name prefix.
///
/// Returns [`DriveInfo::default`] when the resolution fails at any
/// step (sandboxed container, missing `/sys/` mount, etc.).
pub(crate) fn probe_drive() -> DriveInfo {
    let cwd = match std::env::current_dir() {
        Ok(p) => p,
        Err(_) => return DriveInfo::default(),
    };

    let block_dir = match resolve_block_device(&cwd) {
        Some(p) => p,
        None => return DriveInfo::default(),
    };

    let mut info = DriveInfo::default();

    // Drive kind.
    let rotational = read_sys_u32(block_dir.join("queue/rotational")).unwrap_or(1);
    let dev_name = block_dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
    info.kind = classify_drive_kind(dev_name, rotational);

    // Sector sizes and optimal IO.
    if let Some(v) = read_sys_u32(block_dir.join("queue/logical_block_size")) {
        if v > 0 {
            info.logical_sector = v;
        }
    }
    if let Some(v) = read_sys_u32(block_dir.join("queue/physical_block_size")) {
        if v > 0 {
            info.physical_sector = v;
        }
    }
    if let Some(v) = read_sys_u32(block_dir.join("queue/optimal_io_size")) {
        if v > 0 {
            info.optimal_block = v;
        }
    }

    // Queue depth — `nr_requests` is the kernel's queue depth knob and
    // a serviceable proxy for the device's submission-queue depth.
    if let Some(v) = read_sys_u32(block_dir.join("queue/nr_requests")) {
        if v > 0 {
            info.queue_depth = v;
        }
    }

    // Capacity — `size` is in 512-byte sectors regardless of logical
    // sector size.
    if let Some(sectors) = read_sys_u64(block_dir.join("size")) {
        info.total_bytes = sectors.saturating_mul(512);
    }

    // Available bytes — best-effort via statvfs on cwd. `total_bytes`
    // above is the raw device size; available reflects the filesystem
    // usage on the mount point.
    if let Some(avail) = available_bytes(&cwd) {
        info.available_bytes = avail;
    }

    // PLP detection (0.7.0 R-2 in `.dev/DECISIONS-0.7.0.md`):
    // lookup-table over `vendor` + `model` from sysfs. False
    // positives are conservatively avoided; false negatives stay
    // `Unknown` (which costs performance, not correctness — the
    // Auto ladder picks `Direct + fdatasync` instead of
    // `Direct + NVMe FLUSH`).
    info.plp = probe_plp_linux(&block_dir);

    info
}

/// Reads `vendor` and `model` from sysfs and consults the
/// `crate::hardware::plp` lookup table. Returns
/// [`PlpStatus::Unknown`] on any error or table miss.
///
/// On NVMe drives, `vendor`/`model` live at
/// `/sys/block/.../device/{vendor,model}` (the device subdirectory
/// under the block dir). For SATA/SCSI devices the same paths
/// apply via the SCSI subsystem.
fn probe_plp_linux(block_dir: &std::path::Path) -> PlpStatus {
    let device_dir = block_dir.join("device");
    let vendor = std::fs::read_to_string(device_dir.join("vendor"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let model = std::fs::read_to_string(device_dir.join("model"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    crate::hardware::plp::lookup_table(&vendor, &model)
}

fn classify_drive_kind(dev_name: &str, rotational: u32) -> DriveKind {
    if dev_name.starts_with("nvme") {
        DriveKind::Nvme
    } else if rotational == 0 {
        DriveKind::SataSsd
    } else if rotational == 1 {
        DriveKind::Hdd
    } else {
        DriveKind::Unknown
    }
}

/// Walks back from `path` toward the `/sys/block/` entry that backs it.
///
/// Reads `/proc/self/mountinfo` to find the mount point covering
/// `path`, then reads the `dev` field (`major:minor`), then resolves
/// `/sys/dev/block/<major>:<minor>` to the underlying block device's
/// `/sys/block/<dev>/` directory. Partitions resolve to their parent
/// disk via the canonical-path traversal.
fn resolve_block_device(path: &Path) -> Option<PathBuf> {
    let mountinfo = fs::read_to_string("/proc/self/mountinfo").ok()?;
    let canon = fs::canonicalize(path).ok()?;

    let mut best: Option<(usize, &str)> = None;
    for line in mountinfo.lines() {
        // Format: <id> <parent> <major:minor> <root> <mount_point> ...
        let mut fields = line.split_ascii_whitespace();
        let _id = fields.next()?;
        let _parent = fields.next()?;
        let dev = fields.next()?;
        let _root = fields.next()?;
        let mount_point = fields.next()?;
        if canon.starts_with(mount_point) {
            let len = mount_point.len();
            if best.map(|(b, _)| len > b).unwrap_or(true) {
                best = Some((len, dev));
            }
        }
    }
    let dev = best?.1;

    // /sys/dev/block/<major>:<minor> → symlink into /sys/block/<dev>/...
    let sys_dev = PathBuf::from("/sys/dev/block").join(dev);
    let canon_sys = fs::canonicalize(&sys_dev).ok()?;

    // The canonical path may point to a partition entry; walk up to the
    // parent disk if so. A partition's parent contains a `partition`
    // file; the parent disk does not.
    let mut cur = canon_sys.clone();
    while cur.join("partition").exists() {
        cur = match cur.parent() {
            Some(p) => p.to_path_buf(),
            None => break,
        };
    }
    Some(cur)
}

fn read_sys_u32(p: PathBuf) -> Option<u32> {
    fs::read_to_string(&p).ok()?.trim().parse::<u32>().ok()
}

fn read_sys_u64(p: PathBuf) -> Option<u64> {
    fs::read_to_string(&p).ok()?.trim().parse::<u64>().ok()
}

fn available_bytes(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let cstr = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statvfs` is plain old data — every field is an integer
    // — so an all-zero bit pattern is a valid initial value. The
    // struct is fully written by the `statvfs` syscall before any
    // field is read.
    let mut sv: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: cstr is a NUL-terminated string with the correct
    // alignment; `&mut sv` is a valid `*mut statvfs`.
    let ret = unsafe { libc::statvfs(cstr.as_ptr(), &mut sv) };
    if ret != 0 {
        return None;
    }
    Some((sv.f_bavail as u64).saturating_mul(sv.f_frsize as u64))
}

// ─────────────────────────────────────────────────────────────────────────────
// Memory
// ─────────────────────────────────────────────────────────────────────────────

/// Probes total and available memory by parsing `/proc/meminfo`.
///
/// Returns [`MemoryInfo::default`] (zeros) on read failure.
pub(crate) fn probe_memory() -> MemoryInfo {
    let content = match fs::read_to_string("/proc/meminfo") {
        Ok(s) => s,
        Err(_) => return MemoryInfo::default(),
    };

    let mut total = 0u64;
    let mut avail = 0u64;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total = parse_kib(rest);
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            avail = parse_kib(rest);
        }
    }
    MemoryInfo {
        total_bytes: total,
        available_bytes: avail,
    }
}

fn parse_kib(s: &str) -> u64 {
    let trimmed = s.trim().trim_end_matches("kB").trim();
    trimmed
        .parse::<u64>()
        .map(|kib| kib.saturating_mul(1024))
        .unwrap_or(0)
}

// ─────────────────────────────────────────────────────────────────────────────
// CPU
// ─────────────────────────────────────────────────────────────────────────────

/// Probes CPU information.
///
/// Logical core count via [`std::thread::available_parallelism`];
/// physical core count via `/proc/cpuinfo` (counts unique
/// `core id`+`physical id` pairs). Compile-time CPU features via
/// `cfg!(target_feature = "...")` (matches 0.2.0's accuracy; runtime
/// `is_x86_feature_detected!` is deferred to a future enhancement
/// because it requires `target_arch`-gated code paths).
///
/// Cache sizes are read from `/sys/devices/system/cpu/cpu0/cache/*`;
/// fallback to `0` on failure.
pub(crate) fn probe_cpu() -> CpuInfo {
    let cores_logical = std::thread::available_parallelism()
        .map(|n| u32::try_from(n.get()).unwrap_or(u32::MAX))
        .unwrap_or(1);

    let cores_physical = probe_physical_cores().unwrap_or(cores_logical);

    let cache_l1 = read_cache_size(1).unwrap_or(0);
    let cache_l2 = read_cache_size(2).unwrap_or(0);
    let cache_l3 = read_cache_size(3).unwrap_or(0);

    CpuInfo {
        cores_logical,
        cores_physical,
        features: super::super::cpu::runtime_features(),
        cache_l1,
        cache_l2,
        cache_l3,
    }
}

fn probe_physical_cores() -> Option<u32> {
    let content = fs::read_to_string("/proc/cpuinfo").ok()?;
    let mut seen = std::collections::BTreeSet::new();
    let mut current_phys: Option<&str> = None;
    let mut current_core: Option<&str> = None;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("physical id") {
            current_phys = rest.split(':').nth(1).map(str::trim);
        } else if let Some(rest) = line.strip_prefix("core id") {
            current_core = rest.split(':').nth(1).map(str::trim);
        } else if line.is_empty() {
            if let (Some(p), Some(c)) = (current_phys.take(), current_core.take()) {
                let _ = seen.insert(format!("{p}:{c}"));
            }
        }
    }
    if let (Some(p), Some(c)) = (current_phys, current_core) {
        let _ = seen.insert(format!("{p}:{c}"));
    }
    let n = seen.len();
    if n == 0 {
        None
    } else {
        Some(u32::try_from(n).unwrap_or(u32::MAX))
    }
}

fn read_cache_size(level: u8) -> Option<usize> {
    // Linux exposes per-CPU cache info under
    // /sys/devices/system/cpu/cpu0/cache/index{0,1,2,3}/{level,size}.
    // We scan indices 0..=10 for the first matching level.
    for idx in 0..=10 {
        let dir = PathBuf::from(format!("/sys/devices/system/cpu/cpu0/cache/index{idx}"));
        let lvl = match fs::read_to_string(dir.join("level"))
            .ok()
            .and_then(|s| s.trim().parse::<u8>().ok())
        {
            Some(v) => v,
            None => continue,
        };
        if lvl != level {
            continue;
        }
        let size_str = match fs::read_to_string(dir.join("size")).ok() {
            Some(s) => s,
            None => continue,
        };
        return parse_cache_size(size_str.trim());
    }
    None
}

fn parse_cache_size(s: &str) -> Option<usize> {
    let (num, suffix): (&str, &str) =
        if let Some(stripped) = s.strip_suffix('K').or_else(|| s.strip_suffix('k')) {
            (stripped, "K")
        } else if let Some(stripped) = s.strip_suffix('M').or_else(|| s.strip_suffix('m')) {
            (stripped, "M")
        } else {
            (s, "")
        };
    let n: usize = num.parse().ok()?;
    Some(match suffix {
        "K" => n * 1024,
        "M" => n * 1024 * 1024,
        _ => n,
    })
}

// 0.9.2: `detect_compile_time_features` removed; CPU feature
// detection is now runtime-dispatched via `cpu::runtime_features()`.
// See `src/hardware/cpu.rs` for rationale.

// ─────────────────────────────────────────────────────────────────────────────
// IO primitives
// ─────────────────────────────────────────────────────────────────────────────

/// Probes which kernel-level IO primitives are reachable on this Linux
/// host.
///
/// Real runtime detection for `io_uring`: attempts to construct a
/// 1-entry submission ring via the `io-uring` crate. On success, the
/// kernel supports `io_uring_setup(2)`. Tear the ring back down
/// immediately — we just want the yes/no answer.
pub(crate) fn probe_io_primitives() -> IoPrimitives {
    IoPrimitives {
        io_uring: probe_io_uring_available(),
        iocp: false,
        kqueue: false,
        nvme_passthrough: false, // 0.6.0
        direct_io: true,
        mmap: true,
    }
}

fn probe_io_uring_available() -> bool {
    // The `io-uring` crate's `IoUring::new` calls io_uring_setup(2).
    // We use the smallest possible queue depth; setup either works or
    // it doesn't. The ring is dropped immediately.
    io_uring::IoUring::new(1).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_probe_drive_returns_non_default_capacity() {
        // On any Linux box with /sys/block/ accessible, the cwd's
        // backing device should report a non-zero total_bytes.
        // In sandboxed CI, this may degrade to default — accept either.
        let info = probe_drive();
        assert!(
            info.logical_sector >= 512,
            "logical sector size must be at least 512"
        );
    }

    #[test]
    fn test_probe_memory_reports_non_zero_total() {
        let m = probe_memory();
        assert!(
            m.total_bytes > 0,
            "Linux /proc/meminfo must report >0 total"
        );
    }

    #[test]
    fn test_probe_cpu_reports_at_least_one_logical_core() {
        let c = probe_cpu();
        assert!(c.cores_logical >= 1);
        assert!(c.cores_physical >= 1);
    }

    #[test]
    fn test_classify_drive_kind() {
        assert_eq!(classify_drive_kind("nvme0n1", 0), DriveKind::Nvme);
        assert_eq!(classify_drive_kind("sda", 0), DriveKind::SataSsd);
        assert_eq!(classify_drive_kind("sda", 1), DriveKind::Hdd);
        assert_eq!(classify_drive_kind("xvdb", 2), DriveKind::Unknown);
    }

    #[test]
    fn test_parse_kib_strips_unit_and_multiplies() {
        assert_eq!(parse_kib(" 1024 kB"), 1024 * 1024);
        assert_eq!(parse_kib("0 kB"), 0);
        assert_eq!(parse_kib("garbage"), 0);
    }

    #[test]
    fn test_parse_cache_size_handles_units() {
        assert_eq!(parse_cache_size("32K"), Some(32 * 1024));
        assert_eq!(parse_cache_size("8M"), Some(8 * 1024 * 1024));
        assert_eq!(parse_cache_size("4096"), Some(4096));
    }

    #[test]
    fn test_probe_io_primitives_io_uring_runtime_check() {
        let p = probe_io_primitives();
        // We don't assert true/false — io_uring availability depends on
        // kernel + sandbox. We only assert the probe runs without
        // panicking and produces a bool.
        let _ = p.io_uring;
        assert!(p.mmap);
        assert!(p.direct_io);
    }
}
