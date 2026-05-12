//! Operating-system detection and metadata.
//!
//! [`OsInfo`] captures the host platform's family, kind, distro (Linux only),
//! version, architecture, byte order, and memory page size. It is probed
//! once on first access and cached for the lifetime of the process.
//!
//! ```
//! let os = fsys::os::info();
//! assert!(!os.version.is_empty());
//!
//! // Compile-time predicates short-circuit to the active target_os.
//! if fsys::os::is_linux() {
//!     assert_eq!(fsys::os::name(), "linux");
//! }
//! ```

use std::sync::OnceLock;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod unknown;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
use self::linux as platform;
#[cfg(target_os = "macos")]
use self::macos as platform;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
use self::unknown as platform;
#[cfg(target_os = "windows")]
use self::windows as platform;

/// High-level family the active OS belongs to.
///
/// Distinguishes Unix-likes from Windows for code that needs only that
/// granularity. Use [`OsKind`] when finer detail matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OsFamily {
    /// POSIX / Unix-like family (Linux, macOS, the BSDs, ...).
    Unix,
    /// Microsoft Windows family.
    Windows,
    /// Anything outside the two recognised families.
    Other,
}

/// Specific operating system the binary is running on.
///
/// Use this to gate behaviour or pick syscalls that only exist on one
/// platform. The `Other` variant carries a `'static` name supplied by
/// `std::env::consts::OS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OsKind {
    /// Linux (any distribution, any kernel).
    Linux,
    /// Apple macOS.
    Macos,
    /// Microsoft Windows.
    Windows,
    /// FreeBSD.
    FreeBsd,
    /// OpenBSD.
    OpenBsd,
    /// NetBSD.
    NetBsd,
    /// An OS not enumerated above. The inner string is the value of
    /// `std::env::consts::OS`.
    Other(&'static str),
}

impl OsKind {
    /// Returns a stable, lowercase name for this OS kind.
    ///
    /// Names match `std::env::consts::OS` for the common cases. The
    /// `Other` variant returns whatever the toolchain reported.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            OsKind::Linux => "linux",
            OsKind::Macos => "macos",
            OsKind::Windows => "windows",
            OsKind::FreeBsd => "freebsd",
            OsKind::OpenBsd => "openbsd",
            OsKind::NetBsd => "netbsd",
            OsKind::Other(name) => name,
        }
    }
}

/// CPU architecture the crate was compiled for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Arch {
    /// 32-bit x86 (i686 / i386).
    X86,
    /// 64-bit x86 (AMD64 / Intel 64).
    X86_64,
    /// 64-bit ARM (AArch64).
    Aarch64,
    /// 32-bit ARM.
    Arm,
    /// 64-bit RISC-V.
    Riscv64,
    /// WebAssembly (32-bit).
    Wasm32,
    /// An architecture not enumerated above.
    Other(&'static str),
}

impl Arch {
    /// Returns the canonical short name for this architecture.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Arch::X86 => "x86",
            Arch::X86_64 => "x86_64",
            Arch::Aarch64 => "aarch64",
            Arch::Arm => "arm",
            Arch::Riscv64 => "riscv64",
            Arch::Wasm32 => "wasm32",
            Arch::Other(name) => name,
        }
    }
}

/// Byte endianness of the active target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Endianness {
    /// Little-endian byte order.
    Little,
    /// Big-endian byte order.
    Big,
}

/// Snapshot of operating-system metadata.
///
/// Probed once at first access via [`info`] and cached. Fields that the
/// platform does not expose are filled with conservative defaults
/// (e.g. `version = "unknown"`, `distro = None`). No field of an
/// [`OsInfo`] is allowed to be misleading: if the value is not real, it
/// is the documented default.
#[derive(Debug, Clone)]
pub struct OsInfo {
    /// High-level family (`Unix` or `Windows`).
    pub family: OsFamily,
    /// Specific OS kind.
    pub kind: OsKind,
    /// Linux distribution identifier (parsed from `/etc/os-release`),
    /// or `None` on non-Linux platforms or when probing fails.
    pub distro: Option<String>,
    /// Platform version string. Kernel release on Linux, OS version
    /// elsewhere. `"unknown"` if the platform-specific probe is
    /// deferred (see TODO markers in the per-OS modules).
    pub version: String,
    /// Compile-time CPU architecture.
    pub arch: Arch,
    /// Compile-time byte order.
    pub endianness: Endianness,
    /// Memory page size in bytes. Defaulted from the target triple
    /// (4 KiB everywhere except 64-bit Apple Silicon, which is 16 KiB).
    /// Real `sysconf` / `GetSystemInfo` probing is deferred to
    /// `0.0.5`.
    pub page_size: usize,
}

static OS_INFO: OnceLock<OsInfo> = OnceLock::new();

/// Returns a reference to the cached [`OsInfo`], probing on first call.
///
/// The probe never panics. Fields that cannot be determined fall back
/// to documented defaults.
///
/// # Examples
///
/// ```
/// let os = fsys::os::info();
/// assert!(matches!(
///     os.family,
///     fsys::os::OsFamily::Unix | fsys::os::OsFamily::Windows | fsys::os::OsFamily::Other,
/// ));
/// ```
#[must_use]
pub fn info() -> &'static OsInfo {
    OS_INFO.get_or_init(probe)
}

/// Returns the canonical short name of the running OS.
///
/// Equivalent to `info().kind.as_str()`. Allocations-free; the returned
/// reference has `'static` lifetime.
#[must_use]
pub fn name() -> &'static str {
    info().kind.as_str()
}

/// Returns `true` when the active build target is Linux.
///
/// Resolved at compile time; the call has zero runtime cost.
#[must_use]
pub const fn is_linux() -> bool {
    cfg!(target_os = "linux")
}

/// Returns `true` when the active build target is macOS.
///
/// Resolved at compile time; the call has zero runtime cost.
#[must_use]
pub const fn is_macos() -> bool {
    cfg!(target_os = "macos")
}

/// Returns `true` when the active build target is Windows.
///
/// Resolved at compile time; the call has zero runtime cost.
#[must_use]
pub const fn is_windows() -> bool {
    cfg!(target_os = "windows")
}

fn probe() -> OsInfo {
    OsInfo {
        family: current_family(),
        kind: current_kind(),
        distro: platform::probe_distro(),
        version: platform::probe_version(),
        arch: current_arch(),
        endianness: current_endianness(),
        page_size: default_page_size(),
    }
}

fn current_family() -> OsFamily {
    if cfg!(unix) {
        OsFamily::Unix
    } else if cfg!(windows) {
        OsFamily::Windows
    } else {
        OsFamily::Other
    }
}

fn current_kind() -> OsKind {
    if cfg!(target_os = "linux") {
        OsKind::Linux
    } else if cfg!(target_os = "macos") {
        OsKind::Macos
    } else if cfg!(target_os = "windows") {
        OsKind::Windows
    } else if cfg!(target_os = "freebsd") {
        OsKind::FreeBsd
    } else if cfg!(target_os = "openbsd") {
        OsKind::OpenBsd
    } else if cfg!(target_os = "netbsd") {
        OsKind::NetBsd
    } else {
        OsKind::Other(std::env::consts::OS)
    }
}

fn current_arch() -> Arch {
    if cfg!(target_arch = "x86_64") {
        Arch::X86_64
    } else if cfg!(target_arch = "x86") {
        Arch::X86
    } else if cfg!(target_arch = "aarch64") {
        Arch::Aarch64
    } else if cfg!(target_arch = "arm") {
        Arch::Arm
    } else if cfg!(target_arch = "riscv64") {
        Arch::Riscv64
    } else if cfg!(target_arch = "wasm32") {
        Arch::Wasm32
    } else {
        Arch::Other(std::env::consts::ARCH)
    }
}

fn current_endianness() -> Endianness {
    if cfg!(target_endian = "little") {
        Endianness::Little
    } else {
        Endianness::Big
    }
}

/// Probes the host page size at runtime.
///
/// 0.9.6 — replaces the pre-0.9.6 const-fallback (`16_384` on Apple
/// Silicon, `4_096` elsewhere) with a real probe:
/// - **Unix**: `sysconf(_SC_PAGESIZE)`.
/// - **Windows**: `GetSystemInfo` via `windows-sys`.
/// - **Other**: fall back to the architecture-aware default.
///
/// The probe runs once and is cached by the parent `OnceLock` —
/// per-call cost is the OnceLock load, not the sysconf hit.
fn default_page_size() -> usize {
    #[cfg(unix)]
    {
        // SAFETY: `sysconf(2)` is a thread-safe syscall with no
        // side effects beyond returning the configured value or
        // `-1` on error. `_SC_PAGESIZE` is a well-known mandatory
        // sysconf name on every Unix.
        let v = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if v > 0 {
            return v as usize;
        }
    }
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
        // SAFETY: `info` is a stack-allocated SYSTEM_INFO; the
        // all-zeros bit pattern is valid (POD struct). GetSystemInfo
        // writes into the struct through the pointer.
        let mut info: SYSTEM_INFO = unsafe { std::mem::zeroed() };
        // SAFETY: pointer is valid for the call duration; the
        // function has no failure return (writes the struct
        // unconditionally).
        unsafe { GetSystemInfo(&mut info) };
        let p = info.dwPageSize as usize;
        if p > 0 {
            return p;
        }
    }
    // Unknown / probe-failure fallback. Apple Silicon uses 16 KiB,
    // everything else 4 KiB.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        16_384
    }
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    {
        4_096
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_info_returns_non_empty_version() {
        let os = info();
        assert!(!os.version.is_empty());
    }

    #[test]
    fn test_info_caches_same_reference_on_repeat_call() {
        let a = info() as *const OsInfo;
        let b = info() as *const OsInfo;
        assert_eq!(a, b);
    }

    #[test]
    fn test_name_returns_lowercase_kind_string() {
        let n = name();
        assert!(!n.is_empty());
        assert_eq!(n, n.to_lowercase());
    }

    #[test]
    fn test_kind_as_str_round_trips_known_variants() {
        assert_eq!(OsKind::Linux.as_str(), "linux");
        assert_eq!(OsKind::Macos.as_str(), "macos");
        assert_eq!(OsKind::Windows.as_str(), "windows");
        assert_eq!(OsKind::FreeBsd.as_str(), "freebsd");
        assert_eq!(OsKind::OpenBsd.as_str(), "openbsd");
        assert_eq!(OsKind::NetBsd.as_str(), "netbsd");
        assert_eq!(OsKind::Other("haiku").as_str(), "haiku");
    }

    #[test]
    fn test_arch_as_str_round_trips_known_variants() {
        assert_eq!(Arch::X86_64.as_str(), "x86_64");
        assert_eq!(Arch::X86.as_str(), "x86");
        assert_eq!(Arch::Aarch64.as_str(), "aarch64");
        assert_eq!(Arch::Arm.as_str(), "arm");
        assert_eq!(Arch::Riscv64.as_str(), "riscv64");
        assert_eq!(Arch::Wasm32.as_str(), "wasm32");
        assert_eq!(Arch::Other("loongarch64").as_str(), "loongarch64");
    }

    #[test]
    fn test_predicates_are_mutually_exclusive() {
        let n = u8::from(is_linux()) + u8::from(is_macos()) + u8::from(is_windows());
        assert!(n <= 1, "at most one OS predicate may be true at a time");
    }

    #[test]
    fn test_page_size_is_a_power_of_two() {
        let p = info().page_size;
        assert!(p > 0);
        assert_eq!(p & (p - 1), 0, "page size must be a power of two");
    }

    #[test]
    fn test_endianness_matches_target_endian_cfg() {
        let info = info();
        if cfg!(target_endian = "little") {
            assert_eq!(info.endianness, Endianness::Little);
        } else {
            assert_eq!(info.endianness, Endianness::Big);
        }
    }

    #[test]
    fn test_kind_matches_active_target_os() {
        let info = info();
        if cfg!(target_os = "linux") {
            assert_eq!(info.kind, OsKind::Linux);
        } else if cfg!(target_os = "macos") {
            assert_eq!(info.kind, OsKind::Macos);
        } else if cfg!(target_os = "windows") {
            assert_eq!(info.kind, OsKind::Windows);
        }
    }

    #[test]
    fn test_distro_is_none_on_non_linux() {
        if !cfg!(target_os = "linux") {
            assert!(info().distro.is_none());
        }
    }
}
