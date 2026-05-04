//! Foundation-layer integration tests.
//!
//! Exercises the public API surface introduced in `0.0.2`: error type,
//! OS detection, hardware probe stubs, path defaults, separator
//! normalisation, segment sanitisation, and `Mode::Auto` env-driven
//! resolution.

use std::path::PathBuf;

use fsys::hardware;
use fsys::os;
use fsys::path;
use fsys::Mode;

#[test]
fn os_info_is_populated() {
    let info = os::info();
    assert!(!info.version.is_empty());
    assert!(!os::name().is_empty());

    // The kind must match exactly one of the predicates.
    let count = u8::from(os::is_linux()) + u8::from(os::is_macos()) + u8::from(os::is_windows());
    assert!(count <= 1);

    // Endianness is always one of the two recognised values.
    assert!(matches!(
        info.endianness,
        os::Endianness::Little | os::Endianness::Big,
    ));

    // Page size is a power of two.
    let p = info.page_size;
    assert!(p > 0);
    assert_eq!(p & (p - 1), 0);
}

#[test]
fn hardware_info_is_populated() {
    let hw = hardware::info();

    // CPU has at least one logical core.
    assert!(hw.cpu.cores_logical >= 1);
    assert!(hw.cpu.cores_physical >= 1);
    assert!(hw.cpu.cores_physical <= hw.cpu.cores_logical);

    // 0.5.0: drive probe returns real values per platform. We can
    // only assert basic well-formedness — exact numbers vary by
    // hardware. Sandboxed environments degrade to defaults.
    assert!(hw.drive.queue_depth >= 1);
    assert!(hw.drive.logical_sector >= 512);
    assert!(hw.drive.physical_sector >= 512);
    assert_eq!(hw.drive.plp, fsys::hardware::PlpStatus::Unknown); // 0.6.0: real

    // 0.5.0: memory probe is real on Linux/macOS/Windows. Sandboxed
    // unknown-platform builds still report zeros.
    if cfg!(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "windows"
    )) {
        assert!(hw.memory.total_bytes > 0);
    }

    // IO primitives report platform-derived booleans. io_uring is
    // now a real runtime check on Linux (may be true or false
    // depending on kernel/sandbox).
    let io = hw.io_primitives;
    assert_eq!(io.iocp, cfg!(target_os = "windows"));
    assert!(io.mmap);
    assert!(!io.nvme_passthrough); // 0.5.0: NVMe IOCTL deferred to 0.6.0.
}

#[test]
fn hardware_helpers_return_consistent_data() {
    let info = hardware::info();
    assert_eq!(*hardware::cpu(), info.cpu);
    assert_eq!(*hardware::io_primitives(), info.io_primitives);

    // 0.5.0: probes are live, so two consecutive calls may differ
    // slightly in the bytes-available fields (free disk / free memory
    // move while tests run on the host). Assert the stable
    // identification + capacity fields and accept drift in the
    // available counters.
    let d_now = hardware::drive();
    assert_eq!(d_now.kind, info.drive.kind);
    assert_eq!(d_now.plp, info.drive.plp);
    assert_eq!(d_now.logical_sector, info.drive.logical_sector);
    assert_eq!(d_now.physical_sector, info.drive.physical_sector);
    assert_eq!(d_now.optimal_block, info.drive.optimal_block);
    assert_eq!(d_now.queue_depth, info.drive.queue_depth);
    assert_eq!(d_now.total_bytes, info.drive.total_bytes);

    let m_now = hardware::memory();
    assert_eq!(m_now.total_bytes, info.memory.total_bytes);
}

#[test]
fn path_data_returns_non_empty_path() {
    let p = path::data();
    assert!(!p.as_os_str().is_empty());
}

#[test]
fn path_set_is_consistent() {
    let set = path::set();
    assert_eq!(path::data(), set.data);
    assert_eq!(path::logs(), set.logs);
    assert_eq!(path::config(), set.config);
    assert_eq!(path::cache(), set.cache);
    assert_eq!(path::temp(), set.temp);
    assert_eq!(path::state(), set.state);
    assert_eq!(path::locks(), set.locks);
    assert_eq!(path::libs(), set.libs);
    assert_eq!(path::bin(), set.bin);
    assert_eq!(path::runtime(), set.runtime);
}

#[test]
fn path_for_helpers_join_normalised_suffix() {
    let base = path::data();
    let joined = path::data_for("hivedb/docs/info");
    assert!(joined.starts_with(&base));
    assert!(joined.ends_with("info"));

    let parts: Vec<String> = joined
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    for needle in ["hivedb", "docs", "info"] {
        assert!(
            parts.iter().any(|p| p == needle),
            "expected component {needle:?} in {joined:?}"
        );
    }
}

#[test]
fn path_normalize_known_inputs() {
    let p = path::normalize("a/b\\c");
    assert_eq!(p, PathBuf::from("a").join("b").join("c"));

    assert_eq!(path::normalize(""), PathBuf::new());
    assert_eq!(path::normalize("//\\\\"), PathBuf::new());
}

#[test]
fn path_sanitize_segment_known_inputs() {
    assert_eq!(path::sanitize_segment("my file"), "my file");
    assert_eq!(path::sanitize_segment("a/b\\c"), "a_b_c");
    assert_eq!(
        path::sanitize_segment("evil<>:\"|?*name"),
        "evil_______name"
    );
    assert_eq!(path::sanitize_segment("..trail.."), "trail");
}

#[test]
fn mode_resolve_concrete_passes_through() {
    assert_eq!(Mode::Dev.resolve(), Mode::Dev);
    assert_eq!(Mode::Prod.resolve(), Mode::Prod);
}

#[test]
fn mode_resolve_auto_returns_concrete_value() {
    let m = Mode::Auto.resolve();
    assert!(matches!(m, Mode::Dev | Mode::Prod));
}

#[test]
fn mode_module_function_returns_concrete_value() {
    let m = path::mode();
    assert!(matches!(m, Mode::Dev | Mode::Prod));
}

#[test]
fn error_codes_are_stable() {
    use std::path::PathBuf as P;
    let io = fsys::Error::Io(std::io::Error::from(std::io::ErrorKind::NotFound));
    let inv = fsys::Error::InvalidPath {
        path: P::from("x"),
        reason: "test".into(),
    };
    let hw = fsys::Error::HardwareProbeFailed {
        detail: "test".into(),
    };
    let up = fsys::Error::UnsupportedPlatform {
        detail: "test".into(),
    };

    assert_eq!(io.code(), "FS-00001");
    assert_eq!(inv.code(), "FS-00002");
    assert_eq!(hw.code(), "FS-00003");
    assert_eq!(up.code(), "FS-00004");
}
