//! Per-OS default paths plus formatting and sanitisation helpers.
//!
//! [`PathSet`] is the single source of truth for where fsys believes
//! data, logs, configuration, etc. should live. The active mode
//! ([`Mode::Dev`] vs [`Mode::Prod`]) determines which set is used —
//! [`Mode::Auto`] inspects `FSYS_MODE` then `RUST_ENV` and falls back
//! to [`Mode::Prod`].
//!
//! The module exposes:
//!
//! - [`set`] — the cached [`PathSet`] for the active mode.
//! - [`data`], [`bin`], [`config`], [`logs`], [`cache`], [`libs`],
//!   [`runtime`], [`temp`], [`state`], [`locks`] — convenience
//!   accessors that clone the matching field of [`set`].
//! - [`data_for`], [`bin_for`], etc. — accessors that join a
//!   normalised relative suffix onto the corresponding base path.
//! - [`normalize`] — separator normalisation.
//! - [`sanitize_segment`] — single-segment sanitisation.
//!
//! ```
//! use fsys::Mode;
//!
//! // Active mode — derived from env, defaulting to Prod.
//! let _mode = fsys::path::mode();
//!
//! // Active path set.
//! let _set = fsys::path::set();
//!
//! // Compose a sub-path under data.
//! let p = fsys::path::data_for("hivedb/docs/info");
//! assert!(p.ends_with("info"));
//!
//! // Mode resolution is explicit and predictable.
//! assert_eq!(Mode::Dev.resolve(), Mode::Dev);
//! ```

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

mod default;
mod format;
mod mode;

pub use self::format::{normalize, sanitize_segment};
pub use self::mode::{current as mode, Mode};

/// Bundle of OS-aware default paths.
///
/// Probed once at first access and cached. Field semantics are
/// documented per OS in `.dev/PLANNING.md`. Every field is owned —
/// callers that need to mutate clone the field they care about
/// rather than reaching into the cache.
#[derive(Debug, Clone)]
pub struct PathSet {
    /// Application data directory.
    pub data: PathBuf,
    /// Executable / binary directory.
    pub bin: PathBuf,
    /// Configuration directory.
    pub config: PathBuf,
    /// Log directory.
    pub logs: PathBuf,
    /// Cache directory (safe to evict).
    pub cache: PathBuf,
    /// Shared-library directory.
    pub libs: PathBuf,
    /// Runtime / volatile state directory.
    pub runtime: PathBuf,
    /// Scratch / temporary directory.
    pub temp: PathBuf,
    /// Persistent state (similar to `data` but explicitly mutable
    /// state rather than read-mostly data).
    pub state: PathBuf,
    /// Lock-file directory.
    pub locks: PathBuf,
}

static PATH_SET: OnceLock<PathSet> = OnceLock::new();

/// Returns a reference to the cached [`PathSet`] for the active mode.
///
/// First call resolves the mode via [`Mode::Auto`]`.resolve()` and
/// builds the corresponding set. Subsequent calls return the same
/// reference.
#[must_use]
pub fn set() -> &'static PathSet {
    PATH_SET.get_or_init(build_active)
}

fn build_active() -> PathSet {
    match Mode::Auto.resolve() {
        Mode::Dev => default::build_dev(),
        // `resolve` never returns `Auto`; treat unknown as `Prod`.
        Mode::Prod | Mode::Auto => default::build_prod(),
    }
}

macro_rules! base_accessors {
    ($($field:ident, $for_fn:ident, $doc_base:literal, $doc_for:literal);+ $(;)?) => {
        $(
            #[doc = $doc_base]
            #[must_use]
            pub fn $field() -> PathBuf {
                set().$field.clone()
            }

            #[doc = $doc_for]
            #[must_use]
            pub fn $for_fn(suffix: impl AsRef<str>) -> PathBuf {
                join_relative(&set().$field, suffix.as_ref())
            }
        )+
    };
}

base_accessors! {
    data, data_for,
        "Returns the active data directory.",
        "Returns the active data directory joined with the normalised relative suffix.";
    bin, bin_for,
        "Returns the active binary directory.",
        "Returns the active binary directory joined with the normalised relative suffix.";
    config, config_for,
        "Returns the active configuration directory.",
        "Returns the active configuration directory joined with the normalised relative suffix.";
    logs, logs_for,
        "Returns the active log directory.",
        "Returns the active log directory joined with the normalised relative suffix.";
    cache, cache_for,
        "Returns the active cache directory.",
        "Returns the active cache directory joined with the normalised relative suffix.";
    libs, libs_for,
        "Returns the active shared-library directory.",
        "Returns the active shared-library directory joined with the normalised relative suffix.";
    runtime, runtime_for,
        "Returns the active runtime directory.",
        "Returns the active runtime directory joined with the normalised relative suffix.";
    temp, temp_for,
        "Returns the active temporary directory.",
        "Returns the active temporary directory joined with the normalised relative suffix.";
    state, state_for,
        "Returns the active persistent-state directory.",
        "Returns the active persistent-state directory joined with the normalised relative suffix.";
    locks, locks_for,
        "Returns the active lock-file directory.",
        "Returns the active lock-file directory joined with the normalised relative suffix.";
}

fn join_relative(base: &Path, suffix: &str) -> PathBuf {
    let mut out = base.to_path_buf();
    for seg in self::format::split_segments(suffix) {
        out.push(seg);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_set_returns_consistent_reference() {
        let a = set() as *const PathSet;
        let b = set() as *const PathSet;
        assert_eq!(a, b);
    }

    #[test]
    fn test_data_returns_owned_clone() {
        let a = data();
        let b = data();
        assert_eq!(a, b);
    }

    #[test]
    fn test_data_for_appends_segments() {
        let base = data();
        let joined = data_for("hivedb");
        assert!(joined.starts_with(&base));
        assert!(joined.ends_with("hivedb"));
    }

    #[test]
    fn test_data_for_normalizes_separators() {
        let p = data_for("a/b\\c");
        assert!(p.ends_with("a/b/c") || p.ends_with("a\\b\\c"));
        let parts: Vec<_> = p
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        assert!(parts.iter().any(|s| s == "a"));
        assert!(parts.iter().any(|s| s == "b"));
        assert!(parts.iter().any(|s| s == "c"));
    }

    #[test]
    fn test_data_for_strips_leading_separator_to_stay_under_base() {
        let base = data();
        let joined = data_for("/escape/attempt");
        // Leading "/" splits to empty segment, which is filtered;
        // we should remain rooted under `base`.
        assert!(joined.starts_with(&base));
    }

    #[test]
    fn test_logs_for_uses_logs_base() {
        let base = logs();
        let joined = logs_for("app");
        assert!(joined.starts_with(&base));
        assert!(joined.ends_with("app"));
    }

    #[test]
    fn test_each_accessor_clones_corresponding_field() {
        let set = set();
        assert_eq!(data(), set.data);
        assert_eq!(bin(), set.bin);
        assert_eq!(config(), set.config);
        assert_eq!(logs(), set.logs);
        assert_eq!(cache(), set.cache);
        assert_eq!(libs(), set.libs);
        assert_eq!(runtime(), set.runtime);
        assert_eq!(temp(), set.temp);
        assert_eq!(state(), set.state);
        assert_eq!(locks(), set.locks);
    }

    #[test]
    fn test_normalize_re_export_works() {
        let p = normalize("a/b\\c");
        assert!(p.ends_with("c"));
    }

    #[test]
    fn test_sanitize_segment_re_export_works() {
        assert_eq!(sanitize_segment("a/b"), "a_b");
    }

    #[test]
    fn test_mode_re_export_returns_concrete_mode() {
        let m = mode();
        assert!(matches!(m, Mode::Dev | Mode::Prod));
    }
}
