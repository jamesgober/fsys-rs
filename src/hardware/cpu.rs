//! CPU probe.
//!
//! Logical core count is detected via [`std::thread::available_parallelism`]
//! and is real. CPU feature detection in `0.0.2` is **compile-time**:
//! features active in the build target's `target_feature` list are
//! reported. Runtime detection on x86 (via the `is_x86_feature_detected!`
//! macro) and full physical-core / cache enumeration land in `0.0.5`.

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
}
