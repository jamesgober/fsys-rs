//! macOS-specific OS probes.
//!
//! Version probing via `sysctlbyname(3)` for `kern.osproductversion`
//! — the user-facing macOS marketing string (e.g. "14.4.1" on Sonoma).
//! Conservative: returns `"unknown"` on any failure rather than
//! panicking; this layer must never block initialization on a
//! sandboxed runtime that denies sysctl.

#![cfg(target_os = "macos")]

/// Returns the running macOS version string (e.g. `"14.4.1"`).
///
/// Resolution order:
/// 1. `sysctlbyname("kern.osproductversion", ...)` — the marketing
///    version string (since macOS 10.13.4 / Mojave).
/// 2. `"unknown"` on any failure (sandboxed runtime, denied sysctl,
///    truncated buffer).
pub(super) fn probe_version() -> String {
    // 0.9.6 — Replaces the pre-0.9.6 `"unknown"` stub. Marked
    // `TODO(0.0.5)` historically; resolved here as part of the
    // divine-level audit pass.

    // Two-call pattern: first call with NULL out-buffer to learn
    // the required size; second call to fetch the value. The
    // returned string is NUL-terminated.
    let name = b"kern.osproductversion\0";
    let mut size: libc::size_t = 0;

    // SAFETY: `name` is a NUL-terminated C string. Passing
    // `std::ptr::null_mut()` with size=0 asks sysctlbyname to
    // populate `*size` with the required output buffer size.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr().cast::<libc::c_char>(),
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || size == 0 {
        return "unknown".to_string();
    }

    // Cap the size at a sane upper bound — even pre-release
    // versions don't need more than 64 bytes for the version
    // string. Defends against an honest-but-pathological return.
    if size > 64 {
        return "unknown".to_string();
    }

    let mut buf: Vec<u8> = vec![0u8; size];
    // SAFETY: `name` is a NUL-terminated C string; `buf` has
    // `size` bytes; sysctlbyname writes ≤ size bytes (and
    // updates *size to the actual written length).
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr().cast::<libc::c_char>(),
            buf.as_mut_ptr().cast::<libc::c_void>(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return "unknown".to_string();
    }
    // Strip the trailing NUL byte if present.
    if let Some(&0) = buf.last() {
        let _ = buf.pop();
    }
    match String::from_utf8(buf) {
        Ok(s) if !s.is_empty() => s,
        _ => "unknown".to_string(),
    }
}

/// Returns the distro identifier.
///
/// macOS has no concept of distributions; this always returns `None`.
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
        // Either we get a real macOS version string (typical CI
        // runner) or the sandboxed-runtime "unknown" fallback. Both
        // are valid outcomes — what we're checking is that the
        // function neither panics nor returns garbage.
        let v = probe_version();
        assert!(!v.is_empty());
        // If it's not "unknown", it should look like a version
        // string (digits + dots, maybe a "-beta" suffix).
        if v != "unknown" {
            let first = v.chars().next().unwrap();
            assert!(
                first.is_ascii_digit(),
                "expected version to start with a digit, got {:?}",
                v
            );
        }
    }

    #[test]
    fn test_probe_distro_returns_none() {
        assert!(probe_distro().is_none());
    }
}
