//! Minimal zero-dependency TOML for the capability cache schema.
//!
//! REPS § Dependency Management — "Every dependency is a liability.
//! The default answer is no. When functionality can be implemented
//! locally at reasonable cost, it SHOULD be." The capability cache
//! file format is **frozen** at schema version 1: a flat root table
//! plus two named subtables (`[capabilities]`, `[hardware]`) carrying
//! strings, integers, booleans, and string arrays. Nothing else.
//! Pulling in a full TOML crate (`toml` ≈ 80 KB compiled, plus
//! `serde` + `serde_derive` if we used the standard idioms) to
//! serialise five hundred bytes of fixed-shape data would be
//! exactly the kind of dependency creep REPS forbids.
//!
//! This module is **only** a faithful round-trip serializer/parser
//! for the exact subset of TOML the cache uses. It does **not**
//! claim to be a general TOML implementation; behaviour outside
//! the schema is undefined.
//!
//! ## Supported syntax
//!
//! - Comments: `# ...` to end of line.
//! - Top-level pairs: `key = <value>` where `<value>` is one of:
//!   - String: `"text"` (with `\"`, `\\`, `\n`, `\r`, `\t` escapes).
//!   - Integer: `123` (decimal only).
//!   - Boolean: `true` / `false`.
//!   - String array: `["a", "b", "c"]` (possibly empty).
//! - Section headers: `[section]` (no nested tables).
//! - Whitespace: any combination of spaces / tabs around the `=` and
//!   around array commas. Blank lines are allowed anywhere.
//!
//! ## NOT supported
//!
//! - Multi-line strings, raw strings, dates, floats, hex / octal /
//!   binary literals, inline tables, array-of-tables, nested
//!   sections, dotted keys, mixed-type arrays. None of these appear
//!   in the cache schema.

use std::collections::BTreeMap;

/// Parsed value in the cache document. Schema-only — no other TOML
/// value kinds are recognised.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    /// A UTF-8 string. Escapes are unescaped on parse and re-escaped
    /// on serialise.
    String(String),
    /// A signed 64-bit integer. The cache schema uses unsigned
    /// values but TOML integers are signed; we convert at the
    /// consumer.
    Integer(i64),
    /// A boolean.
    Boolean(bool),
    /// A homogeneous string array. Element strings follow the same
    /// escaping rules as [`Value::String`].
    StringArray(Vec<String>),
}

/// In-memory representation of the parsed/serialisable document.
#[derive(Debug, Default, Clone)]
pub(crate) struct Document {
    root: BTreeMap<String, Value>,
    sections: BTreeMap<String, BTreeMap<String, Value>>,
}

impl Document {
    /// Returns an empty document.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Sets a key in the root (above any section header).
    pub(crate) fn set_root(&mut self, key: &str, value: Value) {
        let _previous = self.root.insert(key.to_string(), value);
    }

    /// Sets a key inside a named section. Creates the section if it
    /// doesn't already exist.
    pub(crate) fn set(&mut self, section: &str, key: &str, value: Value) {
        let tbl = self.sections.entry(section.to_string()).or_default();
        let _previous = tbl.insert(key.to_string(), value);
    }

    /// Returns a borrowed integer from the root, if present and of
    /// the right kind.
    pub(crate) fn root_int(&self, key: &str) -> Option<i64> {
        match self.root.get(key)? {
            Value::Integer(i) => Some(*i),
            _ => None,
        }
    }

    /// Returns an owned string from the root, if present and of the
    /// right kind.
    pub(crate) fn root_string(&self, key: &str) -> Option<String> {
        match self.root.get(key)? {
            Value::String(s) => Some(s.clone()),
            _ => None,
        }
    }

    /// Returns a boolean from a section, if present and of the right
    /// kind.
    pub(crate) fn section_bool(&self, section: &str, key: &str) -> Option<bool> {
        match self.sections.get(section)?.get(key)? {
            Value::Boolean(b) => Some(*b),
            _ => None,
        }
    }

    /// Returns an integer from a section, if present and of the
    /// right kind.
    pub(crate) fn section_int(&self, section: &str, key: &str) -> Option<i64> {
        match self.sections.get(section)?.get(key)? {
            Value::Integer(i) => Some(*i),
            _ => None,
        }
    }

    /// Returns a string from a section, if present and of the right
    /// kind.
    pub(crate) fn section_string(&self, section: &str, key: &str) -> Option<String> {
        match self.sections.get(section)?.get(key)? {
            Value::String(s) => Some(s.clone()),
            _ => None,
        }
    }

    /// Returns a string array from a section, if present and of the
    /// right kind.
    pub(crate) fn section_strings(&self, section: &str, key: &str) -> Option<Vec<String>> {
        match self.sections.get(section)?.get(key)? {
            Value::StringArray(v) => Some(v.clone()),
            _ => None,
        }
    }

    /// Renders the document as TOML text.
    pub(crate) fn serialize(&self) -> String {
        // Pre-size for the typical 1.1 cache shape (~700 bytes).
        // The capacity is a hint, not a cap — String will grow if
        // a value is larger than expected. The reservation amortises
        // the small per-`push` reallocations that a default
        // `String::new()` would incur as each key/value/section is
        // appended.
        let mut out = String::with_capacity(1024);
        // Header comment so casual readers know what the file is.
        out.push_str("# fsys capability cache (schema 1.1.0+).\n");
        out.push_str("# Regenerated on probe; safe to delete.\n\n");

        for (k, v) in &self.root {
            push_pair(&mut out, k, v);
        }
        for (section, table) in &self.sections {
            if !out.is_empty() && !out.ends_with("\n\n") {
                out.push('\n');
            }
            out.push('[');
            out.push_str(section);
            out.push_str("]\n");
            for (k, v) in table {
                push_pair(&mut out, k, v);
            }
        }
        out
    }

    /// Parses a TOML document. Returns `None` on any structural
    /// problem (the caller treats parse failure as "re-probe").
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let mut doc = Document::new();
        let mut current_section: Option<String> = None;
        for raw_line in text.lines() {
            let line = strip_comment(raw_line).trim();
            if line.is_empty() {
                continue;
            }
            if let Some(stripped) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                let name = stripped.trim();
                if name.is_empty() || !is_valid_key(name) {
                    return None;
                }
                current_section = Some(name.to_string());
                continue;
            }
            let (key, value) = parse_pair(line)?;
            match &current_section {
                None => doc.set_root(&key, value),
                Some(s) => doc.set(s, &key, value),
            }
        }
        Some(doc)
    }
}

fn push_pair(out: &mut String, key: &str, value: &Value) {
    out.push_str(key);
    out.push_str(" = ");
    match value {
        Value::String(s) => {
            out.push('"');
            push_escaped(out, s);
            out.push('"');
        }
        Value::Integer(i) => {
            out.push_str(&i.to_string());
        }
        Value::Boolean(b) => {
            out.push_str(if *b { "true" } else { "false" });
        }
        Value::StringArray(v) => {
            out.push('[');
            for (i, s) in v.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push('"');
                push_escaped(out, s);
                out.push('"');
            }
            out.push(']');
        }
    }
    out.push('\n');
}

fn push_escaped(out: &mut String, s: &str) {
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                // Other control chars — drop. The cache schema
                // never carries them; defensive against tampering.
            }
            c => out.push(c),
        }
    }
}

fn strip_comment(s: &str) -> &str {
    // Comments start at `#` outside string literals. The cache
    // schema never has `#` inside a value, so the simple scan
    // suffices.
    let mut in_string = false;
    let mut escape = false;
    for (idx, ch) in s.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        match ch {
            '\\' if in_string => escape = true,
            '"' => in_string = !in_string,
            '#' if !in_string => return &s[..idx],
            _ => {}
        }
    }
    s
}

fn is_valid_key(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn parse_pair(line: &str) -> Option<(String, Value)> {
    let eq_idx = line.find('=')?;
    let key = line[..eq_idx].trim();
    let rest = line[eq_idx + 1..].trim();
    if !is_valid_key(key) {
        return None;
    }
    let value = parse_value(rest)?;
    Some((key.to_string(), value))
}

fn parse_value(s: &str) -> Option<Value> {
    if s == "true" {
        return Some(Value::Boolean(true));
    }
    if s == "false" {
        return Some(Value::Boolean(false));
    }
    if let Some(stripped) = s.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
        return Some(Value::String(unescape(stripped)?));
    }
    if let Some(stripped) = s.strip_prefix('[').and_then(|t| t.strip_suffix(']')) {
        return parse_string_array(stripped);
    }
    // Integer.
    if let Some(rest) = s.strip_prefix('-') {
        let n: i64 = rest.parse().ok()?;
        return Some(Value::Integer(-n));
    }
    let n: i64 = s.parse().ok()?;
    Some(Value::Integer(n))
}

fn parse_string_array(inner: &str) -> Option<Value> {
    let inner = inner.trim();
    if inner.is_empty() {
        return Some(Value::StringArray(Vec::new()));
    }
    let mut items: Vec<String> = Vec::new();
    let mut chars = inner.chars().peekable();
    loop {
        // Skip whitespace.
        while matches!(chars.peek(), Some(c) if c.is_whitespace()) {
            // Whitespace skip — we already confirmed via `peek()`
            // that the next character is whitespace; the iterator
            // advance is what we want and there is no value to
            // propagate.
            let _ = chars.next();
        }
        // Expect '"'.
        if chars.next() != Some('"') {
            return None;
        }
        // Read string up to unescaped closing '"'.
        let mut current = String::new();
        let mut closed = false;
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                let next = chars.next()?;
                match next {
                    '\\' => current.push('\\'),
                    '"' => current.push('"'),
                    'n' => current.push('\n'),
                    'r' => current.push('\r'),
                    't' => current.push('\t'),
                    _ => return None,
                }
            } else if ch == '"' {
                closed = true;
                break;
            } else {
                current.push(ch);
            }
        }
        if !closed {
            return None;
        }
        items.push(current);
        // Optional comma + whitespace, or end.
        while matches!(chars.peek(), Some(c) if c.is_whitespace()) {
            // Whitespace skip — we already confirmed via `peek()`
            // that the next character is whitespace; the iterator
            // advance is what we want and there is no value to
            // propagate.
            let _ = chars.next();
        }
        match chars.next() {
            None => break,
            Some(',') => continue,
            Some(_) => return None,
        }
    }
    Some(Value::StringArray(items))
}

fn unescape(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            let next = chars.next()?;
            match next {
                '\\' => out.push('\\'),
                '"' => out.push('"'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                _ => return None,
            }
        } else {
            out.push(ch);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_round_trip_simple_pairs() {
        let mut doc = Document::new();
        doc.set_root("string_key", Value::String("hello world".to_string()));
        doc.set_root("int_key", Value::Integer(42));
        doc.set_root("bool_key", Value::Boolean(true));
        let serialised = doc.serialize();
        let parsed = Document::parse(&serialised).expect("parse");
        assert_eq!(
            parsed.root_string("string_key"),
            Some("hello world".to_string())
        );
        assert_eq!(parsed.root_int("int_key"), Some(42));
        assert!(!parsed.sections.contains_key("nonexistent"));
    }

    #[test]
    fn test_round_trip_section() {
        let mut doc = Document::new();
        doc.set("hardware", "drive_type", Value::String("nvme".to_string()));
        doc.set("hardware", "queue_depth", Value::Integer(64));
        let serialised = doc.serialize();
        let parsed = Document::parse(&serialised).expect("parse");
        assert_eq!(
            parsed.section_string("hardware", "drive_type"),
            Some("nvme".to_string())
        );
        assert_eq!(parsed.section_int("hardware", "queue_depth"), Some(64));
    }

    #[test]
    fn test_round_trip_string_array() {
        let mut doc = Document::new();
        doc.set(
            "capabilities",
            "io_uring_features",
            Value::StringArray(vec![
                "coop_taskrun".to_string(),
                "single_issuer".to_string(),
            ]),
        );
        let serialised = doc.serialize();
        let parsed = Document::parse(&serialised).expect("parse");
        let v = parsed
            .section_strings("capabilities", "io_uring_features")
            .expect("present");
        assert_eq!(
            v,
            vec!["coop_taskrun".to_string(), "single_issuer".to_string()]
        );
    }

    #[test]
    fn test_round_trip_empty_string_array() {
        let mut doc = Document::new();
        doc.set("section", "arr", Value::StringArray(Vec::new()));
        let serialised = doc.serialize();
        let parsed = Document::parse(&serialised).expect("parse");
        let v = parsed.section_strings("section", "arr").expect("present");
        assert!(v.is_empty());
    }

    #[test]
    fn test_round_trip_negative_integer() {
        let mut doc = Document::new();
        doc.set_root("neg", Value::Integer(-7));
        let serialised = doc.serialize();
        let parsed = Document::parse(&serialised).expect("parse");
        assert_eq!(parsed.root_int("neg"), Some(-7));
    }

    #[test]
    fn test_round_trip_string_with_escapes() {
        let mut doc = Document::new();
        doc.set_root(
            "key",
            Value::String("line\nbreak \"quoted\" and \\ slash".to_string()),
        );
        let serialised = doc.serialize();
        let parsed = Document::parse(&serialised).expect("parse");
        assert_eq!(
            parsed.root_string("key"),
            Some("line\nbreak \"quoted\" and \\ slash".to_string())
        );
    }

    #[test]
    fn test_parse_skips_comments_and_blank_lines() {
        let doc = Document::parse(
            "# header comment\n\nkey = 1\n   # indented comment\n[section]\n# in section\nval = \"hi\"\n",
        )
        .expect("parse");
        assert_eq!(doc.root_int("key"), Some(1));
        assert_eq!(doc.section_string("section", "val"), Some("hi".to_string()));
    }

    #[test]
    fn test_parse_handles_inline_comment_after_value() {
        let doc = Document::parse("k = 5 # inline\n").expect("parse");
        assert_eq!(doc.root_int("k"), Some(5));
    }

    #[test]
    fn test_parse_returns_none_on_garbage() {
        assert!(Document::parse("nonsense without equals\n").is_none());
        assert!(Document::parse("[\nunclosed section header").is_none());
        assert!(Document::parse("key = \"unterminated\n").is_none());
        assert!(Document::parse("key = [\"a\", broken,]\n").is_none());
    }

    #[test]
    fn test_parse_returns_none_on_invalid_key_chars() {
        assert!(Document::parse("a.b = 1\n").is_none());
        assert!(Document::parse("a b = 1\n").is_none());
        assert!(Document::parse(" = 1\n").is_none());
    }

    #[test]
    fn test_strip_comment_respects_strings() {
        assert_eq!(
            strip_comment("k = \"hash # inside\""),
            "k = \"hash # inside\""
        );
        assert_eq!(strip_comment("k = 1 # tail"), "k = 1 ");
        assert_eq!(strip_comment("# leading"), "");
    }

    #[test]
    fn test_unescape_unknown_escape_returns_none() {
        assert!(unescape("bad \\x escape").is_none());
    }
}
