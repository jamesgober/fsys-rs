//! Smoke test — verifies the crate links and exposes its version constant.

#[test]
fn version_is_exposed() {
    assert!(!fsys::VERSION.is_empty());
    assert_eq!(fsys::VERSION, env!("CARGO_PKG_VERSION"));
}
