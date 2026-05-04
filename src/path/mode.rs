//! [`Mode`] enum and environment-driven detection.
//!
//! [`Mode::Auto`] inspects `FSYS_MODE` first and `RUST_ENV` as a
//! fallback, defaulting to [`Mode::Prod`] when neither is set or
//! recognised. The detection runs every time [`Mode::resolve`] is
//! called; the path module caches the result on first use.

/// Operating mode for path resolution.
///
/// Affects which [`super::PathSet`] is returned by the standalone
/// helpers. [`Auto`](Mode::Auto) is the recommended default and
/// resolves at first call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Mode {
    /// Development paths — relative to the current working directory
    /// (`./data`, `./logs`, ...). Useful for working trees and tests.
    Dev,
    /// Production paths — OS-standard system paths (`/var/lib`,
    /// `%ProgramData%`, ...).
    Prod,
    /// Inspect `FSYS_MODE` then `RUST_ENV`; fall back to
    /// [`Prod`](Mode::Prod). This is the default.
    #[default]
    Auto,
}

impl Mode {
    /// Returns the canonical short name for this mode.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Mode::Dev => "dev",
            Mode::Prod => "prod",
            Mode::Auto => "auto",
        }
    }

    /// Resolves [`Auto`](Mode::Auto) to a concrete [`Dev`](Mode::Dev)
    /// or [`Prod`](Mode::Prod) by inspecting environment variables.
    /// Concrete modes are returned unchanged.
    ///
    /// # Examples
    ///
    /// ```
    /// use fsys::Mode;
    /// // explicit modes pass through
    /// assert_eq!(Mode::Dev.resolve(), Mode::Dev);
    /// assert_eq!(Mode::Prod.resolve(), Mode::Prod);
    ///
    /// // Auto resolves to Dev or Prod (never Auto)
    /// let resolved = Mode::Auto.resolve();
    /// assert!(matches!(resolved, Mode::Dev | Mode::Prod));
    /// ```
    #[must_use]
    pub fn resolve(self) -> Self {
        match self {
            Mode::Auto => detect_from_env(),
            other => other,
        }
    }
}

/// Returns the active resolved [`Mode`] for this process, derived from
/// `FSYS_MODE` then `RUST_ENV` then the [`Mode::Prod`] default.
///
/// Equivalent to [`Mode::Auto`]`.resolve()`. Provided as a convenience
/// for callers that want to introspect the active mode without
/// constructing a [`Mode`] value first.
#[must_use]
pub fn current() -> Mode {
    detect_from_env()
}

fn detect_from_env() -> Mode {
    if let Some(m) = read("FSYS_MODE") {
        return m;
    }
    if let Some(m) = read("RUST_ENV") {
        return m;
    }
    Mode::Prod
}

fn read(var: &str) -> Option<Mode> {
    let raw = std::env::var(var).ok()?;
    parse(&raw)
}

fn parse(raw: &str) -> Option<Mode> {
    let normalised = raw.trim().to_ascii_lowercase();
    match normalised.as_str() {
        "dev" | "development" | "debug" => Some(Mode::Dev),
        "prod" | "production" | "release" => Some(Mode::Prod),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_is_auto() {
        assert_eq!(Mode::default(), Mode::Auto);
    }

    #[test]
    fn test_as_str_round_trips() {
        assert_eq!(Mode::Dev.as_str(), "dev");
        assert_eq!(Mode::Prod.as_str(), "prod");
        assert_eq!(Mode::Auto.as_str(), "auto");
    }

    #[test]
    fn test_resolve_concrete_returns_self() {
        assert_eq!(Mode::Dev.resolve(), Mode::Dev);
        assert_eq!(Mode::Prod.resolve(), Mode::Prod);
    }

    #[test]
    fn test_resolve_auto_returns_concrete() {
        let m = Mode::Auto.resolve();
        assert!(matches!(m, Mode::Dev | Mode::Prod));
    }

    #[test]
    fn test_parse_recognises_dev_synonyms() {
        assert_eq!(parse("dev"), Some(Mode::Dev));
        assert_eq!(parse("DEV"), Some(Mode::Dev));
        assert_eq!(parse("development"), Some(Mode::Dev));
        assert_eq!(parse("debug"), Some(Mode::Dev));
        assert_eq!(parse("  dev  "), Some(Mode::Dev));
    }

    #[test]
    fn test_parse_recognises_prod_synonyms() {
        assert_eq!(parse("prod"), Some(Mode::Prod));
        assert_eq!(parse("PROD"), Some(Mode::Prod));
        assert_eq!(parse("production"), Some(Mode::Prod));
        assert_eq!(parse("release"), Some(Mode::Prod));
    }

    #[test]
    fn test_parse_rejects_unknown() {
        assert!(parse("staging").is_none());
        assert!(parse("").is_none());
        assert!(parse("garbage").is_none());
    }
}
