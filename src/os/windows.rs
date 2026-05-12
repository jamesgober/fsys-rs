//! Windows-specific OS probes.
//!
//! Version probing via `RtlGetVersion` from `ntdll`. The newer
//! `GetVersionExW` returns the manifested version (lies about
//! Windows 10/11 unless the binary is manifested for it), so
//! `RtlGetVersion` is the only path that returns the *real* running
//! Windows version. The function is documented by Microsoft as the
//! recommended driver-side replacement; user-mode code is not
//! forbidden from calling it (the deprecation is contextual to
//! `GetVersionExW`, not to `RtlGetVersion`).

#![cfg(target_os = "windows")]

/// Returns the running Windows version string in the form
/// `"<major>.<minor>.<build>"` (e.g. `"10.0.22631"`).
///
/// Resolution order:
/// 1. `RtlGetVersion` from `ntdll.dll` — returns the real OS
///    version regardless of application manifest.
/// 2. `"unknown"` on any failure (sandboxed runtime, dynamic
///    linker error).
pub(super) fn probe_version() -> String {
    // 0.9.6 — Replaces the pre-0.9.6 `"unknown"` stub. Marked
    // `TODO(0.0.5)` historically; resolved here as part of the
    // divine-level audit pass.

    /// OSVERSIONINFOEXW layout — repr(C) with `dwOSVersionInfoSize`
    /// as the first field. Declared inline so we don't need an
    /// additional windows-sys feature flag.
    #[repr(C)]
    struct OsVersionInfoExW {
        dw_os_version_info_size: u32,
        dw_major_version: u32,
        dw_minor_version: u32,
        dw_build_number: u32,
        dw_platform_id: u32,
        sz_csd_version: [u16; 128],
        w_service_pack_major: u16,
        w_service_pack_minor: u16,
        w_suite_mask: u16,
        w_product_type: u8,
        w_reserved: u8,
    }

    // ntdll's `RtlGetVersion` — NTSTATUS RtlGetVersion(PRTL_OSVERSIONINFOW).
    // NTSTATUS 0 = STATUS_SUCCESS.
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn RtlGetVersion(lp_version_information: *mut OsVersionInfoExW) -> i32;
    }

    // Zero-initialise the struct (POD bit pattern), then set the
    // size field as the Microsoft contract requires. `zeroed` is
    // sound here because every field of OsVersionInfoExW is a
    // POD type (integers + integer array) — no invalid bit
    // patterns.
    //
    // SAFETY: every field is a POD integer type; the all-zeros
    // bit pattern is valid for every one of them.
    let mut info: OsVersionInfoExW = unsafe { std::mem::zeroed() };
    info.dw_os_version_info_size = std::mem::size_of::<OsVersionInfoExW>() as u32;

    // SAFETY: `info` is a stack-allocated OsVersionInfoExW with
    // its `dw_os_version_info_size` field correctly set per the
    // Microsoft contract. `RtlGetVersion` writes to the struct
    // through the pointer and returns STATUS_SUCCESS (0) on
    // success. The struct lives for the duration of the call.
    let status = unsafe { RtlGetVersion(&mut info) };
    if status != 0 {
        return "unknown".to_string();
    }

    format!(
        "{}.{}.{}",
        info.dw_major_version, info.dw_minor_version, info.dw_build_number
    )
}

/// Returns the distro identifier.
///
/// Windows has no notion of distributions; this always returns `None`.
pub(super) fn probe_distro() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_probe_version_returns_non_empty_default() {
        assert!(!probe_version().is_empty());
    }

    #[test]
    fn test_probe_version_returns_real_version_or_unknown() {
        let v = probe_version();
        assert!(!v.is_empty());
        if v != "unknown" {
            // Real version should be major.minor.build form —
            // three dotted integer fields.
            let parts: Vec<&str> = v.split('.').collect();
            assert_eq!(parts.len(), 3, "expected major.minor.build, got {:?}", v);
            for p in parts {
                assert!(
                    p.chars().all(|c| c.is_ascii_digit()),
                    "expected all-digit component, got {:?}",
                    p
                );
            }
        }
    }

    #[test]
    fn test_probe_distro_returns_none() {
        assert!(probe_distro().is_none());
    }
}
