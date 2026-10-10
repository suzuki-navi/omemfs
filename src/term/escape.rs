//! Display escaping for untrusted names (design/04 "Terminal output safety").
//!
//! Path and entry names come from the remote and the working tree. Printed
//! verbatim they can move the cursor, recolour or retitle the terminal, or
//! forge extra output lines. These helpers render such characters as visible
//! escapes. The output is for display only and is never parsed back.

use std::borrow::Cow;
use std::fmt::Write;

/// Escape every character that is unsafe to print, including LF and TAB.
pub fn escape_display(s: &str) -> Cow<'_, str> {
    escape(s, false)
}

/// Like [`escape_display`] but keeps LF and TAB. For whole messages that are
/// legitimately multi-line (e.g. an error listing one problem per line).
pub fn escape_display_multiline(s: &str) -> Cow<'_, str> {
    escape(s, true)
}

fn needs_escape(c: char, keep_lf_tab: bool) -> bool {
    match c {
        '\n' | '\t' => !keep_lf_tab,
        '\r' => true,
        // C0 controls (including ESC and NUL), DEL, C1 controls.
        '\u{00}'..='\u{1f}' | '\u{7f}' | '\u{80}'..='\u{9f}' => true,
        // Line/paragraph separators and bidirectional format controls.
        '\u{2028}' | '\u{2029}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => true,
        _ => false,
    }
}

fn escape(s: &str, keep_lf_tab: bool) -> Cow<'_, str> {
    if !s.chars().any(|c| needs_escape(c, keep_lf_tab)) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if !needs_escape(c, keep_lf_tab) {
            out.push(c);
            continue;
        }
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{00}'..='\u{1f}' | '\u{7f}' => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            _ => {
                let _ = write!(out, "\\u{{{:04x}}}", c as u32);
            }
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_borrowed_unchanged() {
        let s = "dir/file name.txt";
        assert!(matches!(escape_display(s), Cow::Borrowed(_)));
        assert_eq!(escape_display(s), s);
    }

    #[test]
    fn printable_non_ascii_is_untouched() {
        let s = "日本語/ファイル é ✓ 😀.txt";
        assert_eq!(escape_display(s), s);
    }

    #[test]
    fn backslash_is_not_doubled() {
        assert_eq!(escape_display("a\\nb"), "a\\nb");
    }

    #[test]
    fn newline_cr_and_tab_are_named_escapes() {
        assert_eq!(escape_display("a\nb\rc\td"), "a\\nb\\rc\\td");
    }

    #[test]
    fn esc_and_other_c0_controls_are_hex_escaped() {
        assert_eq!(escape_display("x\u{1b}[2Jy"), "x\\x1b[2Jy");
        assert_eq!(escape_display("\u{00}\u{07}\u{08}"), "\\x00\\x07\\x08");
    }

    #[test]
    fn del_is_hex_escaped() {
        assert_eq!(escape_display("a\u{7f}b"), "a\\x7fb");
    }

    #[test]
    fn c1_controls_are_unicode_escaped() {
        // U+009B is the single-character CSI: the same threat as ESC [.
        assert_eq!(escape_display("a\u{9b}31mb"), "a\\u{009b}31mb");
        assert_eq!(escape_display("\u{80}\u{9f}"), "\\u{0080}\\u{009f}");
    }

    #[test]
    fn line_separators_and_bidi_controls_are_unicode_escaped() {
        assert_eq!(escape_display("a\u{2028}b\u{2029}c"), "a\\u{2028}b\\u{2029}c");
        assert_eq!(
            escape_display("exe\u{202e}txt.gpj"),
            "exe\\u{202e}txt.gpj"
        );
        assert_eq!(escape_display("\u{2066}x\u{2069}"), "\\u{2066}x\\u{2069}");
    }

    #[test]
    fn neighbouring_printable_code_points_are_untouched() {
        // U+00A0 (NBSP) and U+2027 / U+202F sit next to escaped ranges.
        let s = "\u{a0}\u{2027}\u{202f}\u{2065}\u{206a}";
        assert_eq!(escape_display(s), s);
    }

    #[test]
    fn result_never_contains_a_control_character() {
        let nasty: String = ('\u{0}'..='\u{a0}').chain('\u{2000}'..='\u{206f}').collect();
        let out = escape_display(&nasty);
        assert!(
            !out.chars().any(|c| c.is_control()),
            "escaped output must contain no control characters: {out:?}"
        );
        assert!(!out.contains('\u{2028}') && !out.contains('\u{202e}'));
    }

    #[test]
    fn multiline_keeps_lf_and_tab_but_escapes_the_rest() {
        assert_eq!(
            escape_display_multiline("a\nb\tc\rd\u{1b}e"),
            "a\nb\tc\\rd\\x1be"
        );
    }

    #[test]
    fn multiline_of_plain_text_is_borrowed() {
        assert!(matches!(
            escape_display_multiline("line one\nline two"),
            Cow::Borrowed(_)
        ));
    }
}
