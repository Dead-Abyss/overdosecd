//! Terminal-safety rules in one place: what may be shown, and how.
//!
//! v0.5.1 guarded the values a user can *type* (paths, names, aliases, tags).
//! That cannot be enough: a value read from a planted index, from a cloned
//! repository's `.git` files, from the current directory, or from an
//! environment variable never passed an entry point, and `info` reads a live
//! branch on every invocation. So safety is a property of the renderer, and
//! this module holds the two answers a sink can give:
//!
//! - human-facing output **escapes** dangerous characters so the value renders
//!   as one visible line ([`text`], [`path`], [`quoted`]);
//! - machine-facing output — `goto`'s stdout, completion candidates — refuses
//!   a dangerous value, because an escaped path is not a path.
//!
//! [`foreign_escape`] backs the output funnel: the only escape this program
//! emits is an SGR colour change, so anything else reaching
//! [`crate::output::print`] is a renderer bug and is refused rather than
//! trusted.

use std::path::Path;

/// Characters a terminal would interpret, or that make text render as
/// something other than the bytes it contains.
///
/// C0/DEL/C1 come from [`char::is_control`] — C1 includes `U+009B`, a
/// single-byte CSI in some encodings, and control characters cover newlines,
/// tabs, and escape itself. The rest are the bidirectional controls (the
/// "trojan source" class: an override makes a displayed name differ from the
/// stored one), characters that are never visible (zero-width space, word
/// joiner, invisible operators, BOM), and the bidi marks.
///
/// `U+200C` (ZWNJ) and `U+200D` (ZWJ) are deliberately **not** dangerous:
/// they are load-bearing in Arabic/Indic text and in emoji sequences, and
/// rejecting them would refuse legitimate directory names.
pub fn is_dangerous(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{200b}'                 // zero-width space
            | '\u{200e}' | '\u{200f}'  // left-to-right / right-to-left mark
            | '\u{202a}'..='\u{202e}'  // bidi embeddings and overrides
            | '\u{2060}'..='\u{2064}'  // word joiner and invisible operators
            | '\u{2066}'..='\u{2069}'  // bidi isolates
            | '\u{feff}'               // BOM / zero-width no-break space
        )
}

/// Whether `value` carries a character that must not reach a terminal raw.
pub fn contains_dangerous(value: &str) -> bool {
    value.chars().any(is_dangerous)
}

/// Escapes every dangerous character so the result is one visible line.
///
/// Clean text is returned unchanged (combining marks and emoji included), so
/// this is safe to apply to every value a renderer prints.
pub fn text(value: &str) -> String {
    if !contains_dangerous(value) {
        return value.to_owned();
    }
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if is_dangerous(c) {
            out.extend(c.escape_debug());
        } else {
            out.push(c);
        }
    }
    out
}

/// [`text`] for a filesystem path.
pub fn path(value: &Path) -> String {
    text(&value.to_string_lossy())
}

/// A quoted, fully escaped rendering for diagnostics: always one line, and
/// unambiguous even when the value contains quotes, backslashes, or control
/// characters. Rust's debug formatting escapes everything a terminal could
/// interpret, bidi controls and zero-width characters included.
pub fn quoted(value: &str) -> String {
    format!("{value:?}")
}

/// Byte index of the first terminal control sequence that withercd never
/// emits, if any.
///
/// The only escape this program writes is an SGR colour change
/// (`ESC [ <digits and separators> m`). OSC sequences (clipboard writes,
/// window titles), CSI cursor and screen operations, a bare `ESC`, and `BEL`
/// are all foreign: a value slipped through a renderer unsanitized.
pub fn foreign_escape(value: &str) -> Option<usize> {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            0x07 => return Some(index), // BEL
            0x1b => {
                let Some(after) = sgr_end(bytes, index) else {
                    return Some(index);
                };
                index = after;
                continue;
            }
            _ => index += 1,
        }
    }
    None
}

/// If an SGR sequence starts at `at`, the index just past its terminating
/// `m`.
fn sgr_end(bytes: &[u8], at: usize) -> Option<usize> {
    if bytes.get(at + 1) != Some(&b'[') {
        return None;
    }
    let mut index = at + 2;
    while let Some(&byte) = bytes.get(index) {
        if byte.is_ascii_digit() || byte == b';' || byte == b':' {
            index += 1;
        } else {
            break;
        }
    }
    (bytes.get(index) == Some(&b'm')).then_some(index + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ESC: char = '\u{1b}';

    #[test]
    fn detects_every_dangerous_class() {
        for bad in [
            '\u{0}', '\n', '\r', '\t', ESC, '\u{7f}', '\u{9b}', // C0/DEL/C1
            '\u{200b}', '\u{200e}', '\u{200f}', // invisible and bidi marks
            '\u{202a}', '\u{202e}', '\u{2066}', '\u{2069}', // bidi controls
            '\u{2060}', '\u{2064}', '\u{feff}', // invisible operators, BOM
        ] {
            assert!(is_dangerous(bad), "{bad:?} must be dangerous");
        }
    }

    #[test]
    fn leaves_legitimate_text_alone() {
        for fine in ['a', 'Z', 'é', '日', '☕', '👨', '\u{200c}', '\u{200d}'] {
            assert!(!is_dangerous(fine), "{fine:?} must stay displayable");
        }
        // An emoji sequence built with ZWJ keeps rendering as itself.
        let family = "👨\u{200d}👩\u{200d}👧";
        assert_eq!(text(family), family);
        assert_eq!(text("café"), "café");
        assert_eq!(text("日本語"), "日本語");
    }

    #[test]
    fn escapes_without_losing_the_value() {
        assert_eq!(text("plain"), "plain");
        assert_eq!(text(&format!("a{ESC}[31mb")), r"a\u{1b}[31mb");
        assert_eq!(text("two\nlines"), r"two\nlines");
        assert_eq!(text("tab\there"), r"tab\there");
        assert_eq!(text("bidi\u{202e}name"), r"bidi\u{202e}name");
        for case in ["a\u{200b}b", "a\u{feff}b", "a\u{9b}b", "a\u{7f}b"] {
            let escaped = text(case);
            assert_eq!(escaped.lines().count(), 1, "one line: {escaped:?}");
            assert!(!contains_dangerous(&escaped), "escaped output is clean");
        }
    }

    #[test]
    fn quoted_is_one_unambiguous_line() {
        assert_eq!(quoted("plain"), r#""plain""#);
        assert_eq!(quoted("a\"b"), r#""a\"b""#);
        assert_eq!(quoted(&format!("x{ESC}]52;c;QQ")), r#""x\u{1b}]52;c;QQ""#);
        assert_eq!(quoted("bidi\u{202e}x"), r#""bidi\u{202e}x""#);
    }

    #[test]
    fn foreign_escape_allows_only_colour_changes() {
        // What `colored` emits.
        assert_eq!(foreign_escape("\u{1b}[31mred\u{1b}[0m"), None);
        assert_eq!(foreign_escape("\u{1b}[1;38;5;196mbold"), None);
        assert_eq!(foreign_escape("\u{1b}[m"), None);
        assert_eq!(foreign_escape("no escapes here"), None);

        // OSC 52 clipboard, OSC 8 hyperlink, OSC 0 title, CSI clear/cursor,
        // a bare ESC, and BEL are all refused.
        for bad in [
            "\u{1b}]52;c;cGF5bG9hZA==\u{7}",
            "\u{1b}]8;;http://evil.example\u{1b}\\",
            "\u{1b}]0;title\u{7}",
            "\u{1b}[2J",
            "\u{1b}[1A",
            "\u{1b}",
            "text\u{7}bell",
        ] {
            assert!(
                foreign_escape(bad).is_some(),
                "{bad:?} must be refused by the funnel"
            );
        }
    }

    #[test]
    fn foreign_escape_reports_the_first_offending_byte() {
        let text = format!("ok {ESC}]0;t{ESC}");
        assert_eq!(foreign_escape(&text), Some(3));
    }

    #[test]
    fn path_escaping_handles_control_paths() {
        assert_eq!(path(Path::new("/tmp/plain")), "/tmp/plain");
        assert_eq!(
            path(Path::new(&format!("/tmp/bad{ESC}name"))),
            r"/tmp/bad\u{1b}name"
        );
    }

    proptest::proptest! {
        /// The display invariant in property form: whatever the value, the
        /// escaped rendering is one visible line a terminal cannot
        /// misinterpret, and the diagnostic form is too.
        #[test]
        fn escaping_always_yields_visible_lines(
            chars in proptest::collection::vec(proptest::char::any(), 0..32)
        ) {
            let value: String = chars.into_iter().collect();
            let escaped = text(&value);
            proptest::prop_assert!(!contains_dangerous(&escaped), "clean: {escaped:?}");
            proptest::prop_assert!(foreign_escape(&escaped).is_none(), "no sequences: {escaped:?}");
            proptest::prop_assert!(!escaped.contains('\n') && !escaped.contains('\r'));
            let quoted = quoted(&value);
            proptest::prop_assert_eq!(quoted.lines().count(), 1);
            proptest::prop_assert!(foreign_escape(&quoted).is_none());
        }
    }
}
