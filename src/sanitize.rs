//! Terminal-safety rules in one place: what may be shown, and how.
//!
//! Guarding the values a user can *type* (paths, names, aliases, tags) is not
//! enough: a value read from a planted index, from a cloned
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
            '\u{061c}'                 // arabic letter mark (a bidi mark)
            | '\u{200b}'               // zero-width space
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

/// A quoted, fully escaped rendering of an arbitrary path, including bytes
/// that are not UTF-8 (`\xNN`). [`path`] goes through `to_string_lossy`, which
/// would replace exactly the bytes [`quoted_path`] exists to show.
pub fn quoted_path(value: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;

    let mut out = String::from("\"");
    for &byte in value.as_os_str().as_bytes() {
        match byte {
            b'\\' => out.push_str("\\\\"),
            b'"' => out.push_str("\\\""),
            0x20..=0x7e => out.push(char::from(byte)),
            _ => out.push_str(&format!("\\x{byte:02x}")),
        }
    }
    out.push('"');
    out
}

/// A quoted, fully escaped rendering for diagnostics: always one line, and
/// unambiguous even when the value contains quotes, backslashes, or control
/// characters. Rust's debug formatting escapes everything a terminal could
/// interpret, bidi controls and zero-width characters included.
pub fn quoted(value: &str) -> String {
    format!("{value:?}")
}

/// Byte index of the first terminal control sequence that overdosecd never
/// emits, if any.
///
/// The only escape this program writes is an SGR colour change
/// (`ESC [ <colour parameters> m`); everything else is foreign: OSC sequences
/// (clipboard writes, window titles), CSI cursor and screen operations, a bare
/// `ESC`, `BEL`, every control character (a raw `\n` would break the
/// line-oriented `goto` contract), C1 controls such as `U+009B`, and the bidi /
/// invisible characters [`is_dangerous`] names. This is the last line of
/// defense for a renderer that forgot [`text`], so it is deliberately strict:
/// even a well-formed SGR that is not a colour change (blink, conceal,
/// reverse, underline) is refused.
pub fn foreign_escape(value: &str) -> Option<usize> {
    foreign_sequence(value, false)
}

/// [`foreign_escape`] for a sink that legitimately carries embedded newlines
/// (the generated shell scripts and the multi-line debug/table listings): a
/// `\n` is allowed, every other control character and every non-colour escape
/// is not.
pub fn foreign_escape_multiline(value: &str) -> Option<usize> {
    foreign_sequence(value, true)
}

fn foreign_sequence(value: &str, allow_newline: bool) -> Option<usize> {
    let bytes = value.as_bytes();
    for (index, ch) in value.char_indices() {
        if ch == '\u{1b}' {
            if sgr_end(bytes, index).is_none() {
                return Some(index);
            }
            continue;
        }
        if allow_newline && ch == '\n' {
            continue;
        }
        if is_dangerous(ch) {
            return Some(index);
        }
    }
    None
}

/// If a colour SGR sequence starts at `at`, the index just past its
/// terminating `m`.
fn sgr_end(bytes: &[u8], at: usize) -> Option<usize> {
    if bytes.get(at + 1) != Some(&b'[') {
        return None;
    }
    let start = at + 2;
    let mut index = start;
    while let Some(&byte) = bytes.get(index) {
        if byte.is_ascii_digit() || byte == b';' {
            index += 1;
        } else {
            break;
        }
    }
    if bytes.get(index) != Some(&b'm') {
        return None;
    }
    let params = std::str::from_utf8(&bytes[start..index]).ok()?;
    sgr_params_allowed(params).then_some(index + 1)
}

/// Whether an SGR parameter string is one [`colored`] can produce for the
/// theme: resets, bold/dim, and foreground colours (named, 256, truecolor).
///
/// Anything else — `4` underline, `5`/`6` blink, `7` reverse, `8` conceal,
/// `9` strike, `53` overline — hides or inverts text, which is exactly what a
/// planted value would want, and this program never emits it.
fn sgr_params_allowed(params: &str) -> bool {
    if params.is_empty() {
        return true; // `ESC [ m` is SGR 0
    }
    let mut tokens = params.split(';');
    while let Some(token) = tokens.next() {
        match token {
            "0" | "1" | "2" | "22" => {}
            "38" => {
                // Extended foreground colour: `38;5;n` or `38;2;r;g;b`.
                match tokens.next() {
                    Some("5") => {
                        if !tokens.next().is_some_and(is_u8_decimal) {
                            return false;
                        }
                    }
                    Some("2") => {
                        for _ in 0..3 {
                            if !tokens.next().is_some_and(is_u8_decimal) {
                                return false;
                            }
                        }
                    }
                    _ => return false,
                }
            }
            other => {
                let Ok(value) = other.parse::<u8>() else {
                    return false;
                };
                let named_foreground =
                    (30..=37).contains(&value) || value == 39 || (90..=97).contains(&value);
                if !named_foreground {
                    return false;
                }
            }
        }
    }
    true
}

/// `token` is a decimal 0..=255 (an SGR channel).
fn is_u8_decimal(token: &str) -> bool {
    !token.is_empty()
        && token.bytes().all(|byte| byte.is_ascii_digit())
        && token.parse::<u8>().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ESC: char = '\u{1b}';

    #[test]
    fn detects_every_dangerous_class() {
        for bad in [
            '\u{0}', '\n', '\r', '\t', ESC, '\u{7f}', '\u{9b}', // C0/DEL/C1
            '\u{061c}', '\u{200b}', '\u{200e}', '\u{200f}', // bidi mark, invisible
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
    fn foreign_escape_refuses_controls_and_invisibles() {
        // The funnel used to inspect only ESC and BEL; a leaked `\r` could
        // overwrite the terminal line and a `\n` breaks the stdout contract.
        for bad in ["a\rb", "a\tb", "a\u{7f}b", "a\u{9b}b", "a\u{202e}b"] {
            assert!(foreign_escape(bad).is_some(), "{bad:?} must be refused");
            assert!(
                foreign_escape_multiline(bad).is_some(),
                "{bad:?} must be refused even in a multi-line sink"
            );
        }
        // The multi-line sink is exactly for embedded newlines.
        assert_eq!(foreign_escape_multiline("line\ntwo"), None);
        assert!(foreign_escape("line\ntwo").is_some());
    }

    #[test]
    fn only_colour_sgr_parameters_pass() {
        for fine in [
            "\u{1b}[0m",
            "\u{1b}[1m",
            "\u{1b}[2m",
            "\u{1b}[22m",
            "\u{1b}[39m",
            "\u{1b}[97m",
            "\u{1b}[38;5;0m",
            "\u{1b}[38;2;255;0;128m",
            "\u{1b}[1;31m",
            "\u{1b}[m",
        ] {
            assert_eq!(foreign_escape(fine), None, "{fine:?} must pass");
        }
        // Text-hiding or inverting parameters never reach a terminal.
        for bad in [
            "\u{1b}[4m",
            "\u{1b}[5m",
            "\u{1b}[6m",
            "\u{1b}[7m",
            "\u{1b}[8m",
            "\u{1b}[9m",
            "\u{1b}[53m",
            "\u{1b}[48;5;196m",
            "\u{1b}[38;5;999m",
            "\u{1b}[38;2;1;2m",
            "\u{1b}[38;2;1;2;3;4m",
            "\u{1b}[999m",
            "\u{1b}[;;m",
            "\u{1b}[1:2m",
        ] {
            assert!(foreign_escape(bad).is_some(), "{bad:?} must be refused");
        }
    }

    #[cfg(unix)]
    #[test]
    fn quoted_path_shows_raw_bytes_instead_of_replacing_them() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let hostile = Path::new(OsStr::from_bytes(b"/tmp/\xff\"quote\\"));
        let rendered = quoted_path(hostile);
        assert!(rendered.contains("\\xff"), "raw byte survives: {rendered}");
        assert!(!rendered.contains('\u{fffd}'), "no lossy replacement");
        assert!(rendered.contains("\\\""), "quote is escaped: {rendered}");
        assert_eq!(rendered.lines().count(), 1);
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

        /// The funnel in property form: any dangerous character outside an
        /// SGR sequence is refused by both checks, and an embedded newline
        /// (with no other hazard) is refused only by the line-oriented one.
        #[test]
        fn the_funnel_refuses_unsanitized_values(
            chars in proptest::collection::vec(proptest::char::any(), 0..32)
        ) {
            let value: String = chars.into_iter().collect();
            let hazardous =
                value
                    .chars()
                    .any(|c| c != ESC && c != '\n' && is_dangerous(c));
            if hazardous {
                proptest::prop_assert!(foreign_escape(&value).is_some(), "{value:?}");
                proptest::prop_assert!(foreign_escape_multiline(&value).is_some(), "{value:?}");
            }
            if value.contains('\n') && !hazardous && !value.contains(ESC) {
                proptest::prop_assert!(foreign_escape(&value).is_some(), "{value:?}");
                proptest::prop_assert!(
                    foreign_escape_multiline(&value).is_none(),
                    "a raw sink may carry newlines: {value:?}"
                );
            }
        }
    }
}
