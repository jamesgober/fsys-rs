//! Kernel-level IO primitives availability map.
//!
//! Each field is `true` only when the active build target's kernel
//! exposes the primitive at all. A `true` does **not** guarantee the
//! call will succeed for a given file descriptor — for example, on
//! Linux every kernel since 2.6 supports `O_DIRECT` *as a syscall*,
//! but tmpfs and several network filesystems will reject the flag at
//! open time.
//!
//! `0.0.2` returns a static, target-driven snapshot. Real runtime
//! verification (probing for `io_uring_setup`, NVMe device-character
//! files, etc.) is deferred to `0.0.5`. Each conservative field is
//! marked with a `TODO(0.0.5)` comment in the internal probe routine.

/// Availability of kernel-level IO primitives.
///
/// Stored as plain bools rather than a bitset because each field has
/// independent semantics and exposing them by name keeps callers
/// honest about which primitive they are checking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct IoPrimitives {
    /// Linux `io_uring` interface (kernel 5.1+).
    ///
    /// `0.0.2` reports `false` unconditionally; runtime probing of
    /// `io_uring_setup(2)` lands in `0.0.5`.
    pub io_uring: bool,
    /// Windows I/O Completion Ports.
    ///
    /// Always `true` on Windows targets: IOCP has been part of the
    /// kernel since NT 3.5.
    pub iocp: bool,
    /// BSD `kqueue` (macOS, FreeBSD, NetBSD, OpenBSD).
    pub kqueue: bool,
    /// NVMe passthrough flush via `IORING_OP_URING_CMD` (Linux 5.19+)
    /// or `IOCTL_STORAGE_PROTOCOL_COMMAND` (Windows). `0.0.2` reports
    /// `false` until real device-class probing lands in `0.0.5`.
    pub nvme_passthrough: bool,
    /// Direct (page-cache-bypassing) IO. Available on Linux
    /// (`O_DIRECT`), macOS (`F_NOCACHE`), and Windows
    /// (`FILE_FLAG_NO_BUFFERING`).
    pub direct_io: bool,
    /// Memory-mapped IO. Available on every supported target.
    pub mmap: bool,
}

/// Runs the IO-primitives probe.
///
/// Returns a static snapshot derived from the build target's
/// `target_os`. Conservative for primitives that need a kernel
/// version check (`io_uring`, `nvme_passthrough`).
#[must_use]
pub(super) fn probe() -> IoPrimitives {
    IoPrimitives {
        // TODO(0.0.5): probe `io_uring_setup` to confirm kernel 5.1+
        // and check for ring registration.
        io_uring: false,
        iocp: cfg!(target_os = "windows"),
        kqueue: cfg!(any(
            target_os = "macos",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly",
        )),
        // TODO(0.0.5): require NVMe + appropriate caps/admin rights.
        nvme_passthrough: false,
        // The flag exists on every supported target; the *filesystem*
        // may still reject it at open time. Callers fall back as
        // documented in PLANNING.md when that happens.
        direct_io: cfg!(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "windows",
        )),
        // mmap / MapViewOfFile is universal across the supported set.
        mmap: cfg!(any(unix, windows)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_has_all_fields_false() {
        let p = IoPrimitives::default();
        assert!(!p.io_uring);
        assert!(!p.iocp);
        assert!(!p.kqueue);
        assert!(!p.nvme_passthrough);
        assert!(!p.direct_io);
        assert!(!p.mmap);
    }

    #[test]
    fn test_probe_reports_iocp_only_on_windows() {
        let p = probe();
        if cfg!(target_os = "windows") {
            assert!(p.iocp);
        } else {
            assert!(!p.iocp);
        }
    }

    #[test]
    fn test_probe_reports_kqueue_on_bsds_and_macos() {
        let p = probe();
        let expected = cfg!(any(
            target_os = "macos",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly",
        ));
        assert_eq!(p.kqueue, expected);
    }

    #[test]
    fn test_probe_reports_mmap_on_supported_targets() {
        let p = probe();
        if cfg!(any(unix, windows)) {
            assert!(p.mmap);
        }
    }

    #[test]
    fn test_probe_does_not_lie_about_io_uring_in_foundation_phase() {
        assert!(!probe().io_uring);
    }

    #[test]
    fn test_probe_does_not_lie_about_nvme_passthrough_in_foundation_phase() {
        assert!(!probe().nvme_passthrough);
    }
}
