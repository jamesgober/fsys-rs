//! Path formatting helpers: separator normalisation and segment
//! sanitisation.
//!
//! These helpers do **not** touch the filesystem. They operate purely
//! on string-shaped paths, so they can be used to validate caller
//! input before it ever reaches an `open(2)`.

use std::path::PathBuf;

/// Normalises mixed-separator path strings into the active platform's
/// separator.
///
/// Splits on both `/` and `\\`, drops empty segments (which collapse
/// repeated separators), and rebuilds a [`PathBuf`] using
/// [`PathBuf::push`]. The result is always relative — leading
/// separators are absorbed by the empty-segment filter — which makes
/// it safe to feed to [`PathBuf::push`] from a caller-controlled
/// base path.
///
/// # Examples
///
/// ```
/// use std::path::PathBuf;
///
/// let p = fsys::path::normalize("a/b\\c");
/// assert_eq!(p, PathBuf::from("a").join("b").join("c"));
/// ```
#[must_use]
pub fn normalize(s: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for seg in split_segments(s) {
        out.push(seg);
    }
    out
}

/// Sanitises a single path segment so the result is safe on every
/// supported platform.
///
/// - Path separators (`/`, `\\`) and the NUL byte are replaced with
///   `_`.
/// - Windows-reserved characters (`<>:"|?*`) are replaced with `_`.
/// - ASCII control characters (`U+0000`–`U+001F`, `U+007F`) are
///   replaced with `_`.
/// - Leading and trailing whitespace and `.` are trimmed (Windows
///   silently strips trailing dots and spaces from filenames).
///
/// The function never errors. An empty input — or input that becomes
/// empty after sanitisation — produces an empty string. Callers that
/// require a non-empty result must check before using the value.
///
/// # Examples
///
/// ```
/// assert_eq!(fsys::path::sanitize_segment("my file"), "my file");
/// assert_eq!(fsys::path::sanitize_segment("a/b"), "a_b");
/// assert_eq!(fsys::path::sanitize_segment("  trail.  "), "trail");
/// ```
#[must_use]
pub fn sanitize_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        let mapped = match ch {
            '/' | '\\' | '\0' => '_',
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        };
        out.push(mapped);
    }
    out.trim_matches(|c: char| c.is_whitespace() || c == '.')
        .to_string()
}

pub(super) fn split_segments(s: &str) -> impl Iterator<Item = &str> {
    s.split(['/', '\\']).filter(|seg| !seg.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_handles_forward_only() {
        let p = normalize("a/b/c");
        assert_eq!(p, PathBuf::from("a").join("b").join("c"));
    }

    #[test]
    fn test_normalize_handles_backward_only() {
        let p = normalize("a\\b\\c");
        assert_eq!(p, PathBuf::from("a").join("b").join("c"));
    }

    #[test]
    fn test_normalize_handles_mixed() {
        let p = normalize("a/b\\c/d");
        assert_eq!(p, PathBuf::from("a").join("b").join("c").join("d"));
    }

    #[test]
    fn test_normalize_collapses_repeated_separators() {
        let p = normalize("a//b\\\\c");
        assert_eq!(p, PathBuf::from("a").join("b").join("c"));
    }

    #[test]
    fn test_normalize_strips_leading_separators() {
        let p = normalize("/a/b");
        assert_eq!(p, PathBuf::from("a").join("b"));
        assert!(p.is_relative());
    }

    #[test]
    fn test_normalize_returns_empty_for_empty_input() {
        let p = normalize("");
        assert_eq!(p, PathBuf::new());
    }

    #[test]
    fn test_normalize_returns_empty_for_only_separators() {
        let p = normalize("////\\\\");
        assert_eq!(p, PathBuf::new());
    }

    #[test]
    fn test_sanitize_segment_passthrough_for_safe_input() {
        assert_eq!(sanitize_segment("hivedb"), "hivedb");
        assert_eq!(sanitize_segment("my-file_2"), "my-file_2");
    }

    #[test]
    fn test_sanitize_segment_replaces_path_separators() {
        assert_eq!(sanitize_segment("a/b\\c"), "a_b_c");
    }

    #[test]
    fn test_sanitize_segment_replaces_windows_reserved() {
        assert_eq!(sanitize_segment("a<b>c:d\"e|f?g*h"), "a_b_c_d_e_f_g_h");
    }

    #[test]
    fn test_sanitize_segment_replaces_nul() {
        assert_eq!(sanitize_segment("a\0b"), "a_b");
    }

    #[test]
    fn test_sanitize_segment_replaces_control_chars() {
        assert_eq!(sanitize_segment("a\x01b\x1fc"), "a_b_c");
    }

    #[test]
    fn test_sanitize_segment_trims_trailing_dots_and_whitespace() {
        assert_eq!(sanitize_segment("  trail.  "), "trail");
        assert_eq!(sanitize_segment("...stack..."), "stack");
    }

    #[test]
    fn test_sanitize_segment_handles_empty_input() {
        assert_eq!(sanitize_segment(""), "");
    }

    #[test]
    fn test_sanitize_segment_handles_unicode_passthrough() {
        assert_eq!(sanitize_segment("résumé"), "résumé");
        assert_eq!(sanitize_segment("名前"), "名前");
    }
}
