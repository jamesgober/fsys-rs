//! CPU probe.
//!
//! Logical core count is detected via [`std::thread::available_parallelism`].
//!
//! 0.9.2: CPU feature detection is now **runtime-dispatched** on x86,
//! x86_64, and aarch64 via [`std::arch::is_x86_feature_detected`] and
//! [`std::arch::is_aarch64_feature_detected`]. Pre-0.9.2 detection
//! was compile-time only — `cfg!(target_feature = "…")` reflected what
//! the binary was *built* for, not what the host CPU could actually
//! execute. A binary compiled with `target-cpu=x86-64-v1` would never
//! report SSE4.2 even on a v3 host. This regression is closed in
//! 0.9.2: feature flags now mirror real silicon, so the journal's
//! hardware CRC-32C path engages whenever the CPU supports SSE4.2,
//! independent of build flags.

use std::ops::{BitOr, BitOrAssign};

/// Bitset of CPU features that fsys cares about.
///
/// Stored as a single `u64`, so cheap to copy and compare. Use
/// [`CpuFeatures::contains`] to test, [`BitOr`] / [`BitOrAssign`] to
/// combine. Because new features may be added without bumping the
/// crate's MAJOR version, the type is opaque: callers compare against
/// the named constants rather than against raw bit patterns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CpuFeatures(u64);

impl CpuFeatures {
    /// Streaming SIMD Extensions (x86).
    pub const SSE: Self = Self(1 << 0);
    /// SSE2 (x86, baseline on x86_64).
    pub const SSE2: Self = Self(1 << 1);
    /// SSE3 (x86).
    pub const SSE3: Self = Self(1 << 2);
    /// Supplemental SSE3 (x86).
    pub const SSSE3: Self = Self(1 << 3);
    /// SSE 4.1 (x86).
    pub const SSE4_1: Self = Self(1 << 4);
    /// SSE 4.2 (x86).
    pub const SSE4_2: Self = Self(1 << 5);
    /// Advanced Vector Extensions (x86).
    pub const AVX: Self = Self(1 << 6);
    /// AVX2 (x86).
    pub const AVX2: Self = Self(1 << 7);
    /// AVX-512 Foundation (x86).
    pub const AVX512F: Self = Self(1 << 8);
    /// AES-NI (x86).
    pub const AES: Self = Self(1 << 9);
    /// Carryless multiplication (x86).
    pub const PCLMULQDQ: Self = Self(1 << 10);
    /// ARM NEON / AArch64 ASIMD.
    pub const NEON: Self = Self(1 << 11);

    /// Empty feature set.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Returns the underlying bit pattern.
    ///
    /// The bit layout is **not** part of the public API; use only for
    /// debug printing or hashing.
    #[must_use]
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Returns `true` when no features are present.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Returns `true` when every feature in `other` is also present in
    /// `self`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
}

impl BitOr for CpuFeatures {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for CpuFeatures {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Snapshot of CPU information.
///
/// `cores_logical` is real. The remaining fields carry stubbed
/// defaults in `0.0.2`; physical-core enumeration and cache-size
/// reporting land in `0.0.5`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuInfo {
    /// Number of logical cores (hardware threads), as reported by
    /// [`std::thread::available_parallelism`]. Always `>= 1`.
    pub cores_logical: u32,
    /// Number of physical cores. Equal to `cores_logical` until real
    /// enumeration lands.
    pub cores_physical: u32,
    /// Compile-time CPU feature set.
    pub features: CpuFeatures,
    /// L1 cache size in bytes. `0` while the probe is stubbed.
    pub cache_l1: usize,
    /// L2 cache size in bytes. `0` while the probe is stubbed.
    pub cache_l2: usize,
    /// L3 cache size in bytes. `0` while the probe is stubbed.
    pub cache_l3: usize,
}

impl Default for CpuInfo {
    fn default() -> Self {
        Self {
            cores_logical: 1,
            cores_physical: 1,
            features: CpuFeatures::empty(),
            cache_l1: 0,
            cache_l2: 0,
            cache_l3: 0,
        }
    }
}

/// Runs the per-platform CPU probe.
///
/// Delegates to the crate-internal `probe::platform::probe_cpu` which
/// reads `/proc/cpuinfo` + `/sys/devices/system/cpu/.../cache/...`
/// (Linux), `GetLogicalProcessorInformationEx` (Windows), or
/// `sysctlbyname` (macOS) for accurate physical-core counts and cache
/// sizes.
#[must_use]
pub(super) fn probe() -> CpuInfo {
    super::probe::platform::probe_cpu()
}

/// 0.9.2: runtime CPU-feature detection.
///
/// Returns the bitset of features the **host CPU** supports, as
/// queried via the `is_x86_feature_detected!` (x86 / x86_64) and
/// `is_aarch64_feature_detected!` (aarch64) standard-library macros.
/// On unknown architectures returns [`CpuFeatures::empty`].
///
/// This is the canonical detection path for the per-platform probes
/// — `probe_cpu` on every platform calls this function rather than
/// reading `cfg!(target_feature = …)`. Build-flag-independent: a
/// binary compiled with `target-cpu=x86-64-v1` will still report
/// SSE4.2 / AES / AVX2 etc. when the host actually has them.
#[must_use]
pub(crate) fn runtime_features() -> CpuFeatures {
    let mut f = CpuFeatures::empty();

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        if std::arch::is_x86_feature_detected!("sse") {
            f |= CpuFeatures::SSE;
        }
        if std::arch::is_x86_feature_detected!("sse2") {
            f |= CpuFeatures::SSE2;
        }
        if std::arch::is_x86_feature_detected!("sse3") {
            f |= CpuFeatures::SSE3;
        }
        if std::arch::is_x86_feature_detected!("ssse3") {
            f |= CpuFeatures::SSSE3;
        }
        if std::arch::is_x86_feature_detected!("sse4.1") {
            f |= CpuFeatures::SSE4_1;
        }
        if std::arch::is_x86_feature_detected!("sse4.2") {
            f |= CpuFeatures::SSE4_2;
        }
        if std::arch::is_x86_feature_detected!("avx") {
            f |= CpuFeatures::AVX;
        }
        if std::arch::is_x86_feature_detected!("avx2") {
            f |= CpuFeatures::AVX2;
        }
        if std::arch::is_x86_feature_detected!("avx512f") {
            f |= CpuFeatures::AVX512F;
        }
        if std::arch::is_x86_feature_detected!("aes") {
            f |= CpuFeatures::AES;
        }
        if std::arch::is_x86_feature_detected!("pclmulqdq") {
            f |= CpuFeatures::PCLMULQDQ;
        }
    }

    #[cfg(target_arch = "aarch64")]
    {
        if std::arch::is_aarch64_feature_detected!("neon") {
            f |= CpuFeatures::NEON;
        }
    }

    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_features_empty_contains_only_empty() {
        let empty = CpuFeatures::empty();
        assert!(empty.is_empty());
        assert!(empty.contains(CpuFeatures::empty()));
        assert!(!empty.contains(CpuFeatures::SSE2));
    }

    #[test]
    fn test_features_bitor_combines_flags() {
        let f = CpuFeatures::SSE | CpuFeatures::SSE2;
        assert!(f.contains(CpuFeatures::SSE));
        assert!(f.contains(CpuFeatures::SSE2));
        assert!(!f.contains(CpuFeatures::AVX));
    }

    #[test]
    fn test_features_bitor_assign_inserts_flag() {
        let mut f = CpuFeatures::empty();
        f |= CpuFeatures::AVX;
        assert!(f.contains(CpuFeatures::AVX));
    }

    #[test]
    fn test_features_bits_round_trip_through_constants() {
        let combined = CpuFeatures::SSE | CpuFeatures::AVX2;
        assert_eq!(
            combined.bits(),
            CpuFeatures::SSE.bits() | CpuFeatures::AVX2.bits()
        );
    }

    #[test]
    fn test_features_contains_subset() {
        let all = CpuFeatures::SSE | CpuFeatures::SSE2 | CpuFeatures::AVX;
        assert!(all.contains(CpuFeatures::SSE | CpuFeatures::AVX));
    }

    #[test]
    fn test_default_cpu_info_has_one_core() {
        let i = CpuInfo::default();
        assert_eq!(i.cores_logical, 1);
        assert_eq!(i.cores_physical, 1);
    }

    #[test]
    fn test_probe_reports_at_least_one_logical_core() {
        // 0.5.0: probe is real per-platform. Physical and logical
        // counts differ on SMT/Hyper-Threaded CPUs and only need to
        // satisfy `physical <= logical` and both `>= 1`.
        let i = probe();
        assert!(i.cores_logical >= 1);
        assert!(i.cores_physical >= 1);
        assert!(
            i.cores_physical <= i.cores_logical,
            "physical cores ({}) cannot exceed logical cores ({})",
            i.cores_physical,
            i.cores_logical,
        );
    }

    #[test]
    fn test_probe_features_include_sse2_on_x86_64() {
        if cfg!(all(target_arch = "x86_64", target_feature = "sse2")) {
            assert!(probe().features.contains(CpuFeatures::SSE2));
        }
    }

    #[test]
    fn test_probe_features_include_neon_on_aarch64() {
        if cfg!(all(target_arch = "aarch64", target_feature = "neon")) {
            assert!(probe().features.contains(CpuFeatures::NEON));
        }
    }

    /// 0.9.2: runtime CPUID detection sanity. On any reasonable x86_64
    /// host (every chip since ~2003), `runtime_features()` must report
    /// at least SSE2 — it's part of the x86_64 baseline. This pins
    /// that the runtime probe is engaged (vs the previous
    /// compile-time `cfg!` behaviour, which would have been a
    /// constant-true at build time but a constant-false on a
    /// `target-cpu=x86-64-v1` build).
    #[test]
    fn test_runtime_features_includes_sse2_on_any_x86_64_host() {
        if cfg!(target_arch = "x86_64") {
            let f = runtime_features();
            assert!(
                f.contains(CpuFeatures::SSE2),
                "every x86_64 host has SSE2 in its baseline ISA; \
                 runtime_features returned 0x{:x}",
                f.bits()
            );
        }
    }

    /// 0.9.2: runtime CPUID detection must reflect actual CPU
    /// capabilities, not build-time `target_feature` flags.
    /// This pins the runtime path engages by confirming the
    /// reported feature set is a *superset* of (or equal to)
    /// the compile-time set — i.e. the runtime probe never
    /// reports *fewer* features than the compiler had to use.
    #[test]
    fn test_runtime_features_includes_compile_time_baseline() {
        let runtime = runtime_features();
        // Compile-time baseline — every feature the compiler
        // committed to using.
        let mut compile_time = CpuFeatures::empty();
        if cfg!(target_feature = "sse2") {
            compile_time |= CpuFeatures::SSE2;
        }
        if cfg!(target_feature = "sse4.2") {
            compile_time |= CpuFeatures::SSE4_2;
        }
        if cfg!(target_feature = "aes") {
            compile_time |= CpuFeatures::AES;
        }
        if cfg!(target_feature = "neon") {
            compile_time |= CpuFeatures::NEON;
        }
        assert!(
            runtime.contains(compile_time),
            "runtime feature set 0x{:x} missing compile-time baseline 0x{:x}",
            runtime.bits(),
            compile_time.bits(),
        );
    }
}
