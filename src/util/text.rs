//! Terminal-output safety. The audited repo is untrusted input: file names and
//! commit messages can carry ANSI escapes or bidi overrides (Trojan Source).

use std::sync::OnceLock;

fn ctrl_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Python: C0 controls, DEL/C1, then the bidi embedding/override and
        // isolate ranges. Written as `\x{...}` escapes on purpose: literal bidi
        // characters in Rust source would reproduce the exact Trojan Source
        // attack this regex exists to stop (and rustc rejects them outright).
        regex::Regex::new(r"[\x{00}-\x{1f}\x{7f}-\x{9f}\x{202A}-\x{202E}\x{2066}-\x{2069}]")
            .expect("control regex")
    })
}

/// Replace control characters with `?` before anything reaches a terminal.
pub fn safe(s: &str) -> String {
    ctrl_re().replace_all(s, "?").into_owned()
}

/// Python slices by *character*; Rust's `&s[..n]` slices by byte and panics on a
/// non-char-boundary. Every truncation in the report goes through this.
pub fn chars_take(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Python's `%-28s`: left-justified in a fixed field, truncated first.
///
/// ⚠ Counts **characters**, not terminal columns. Keep using it only where the
/// field is guaranteed ASCII; for anything that can hold a CJK author name use
/// [`pad_display`], which measures the way a terminal renders.
pub fn pad_left(s: &str, width: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    let body: String = if chars.len() > width {
        chars[..width].iter().collect()
    } else {
        s.to_string()
    };
    let pad = width - body.chars().count();
    format!("{}{}", body, " ".repeat(pad))
}

/// Terminal columns a string occupies.
///
/// `chars().count()` mis-measures in both directions: a CJK ideograph, a
/// fullwidth form and an emoji are two columns wide, and a combining mark is
/// zero. Padding by character count therefore breaks the alignment of every row
/// that contains a Chinese author name — which is most of them in this report.
///
/// East Asian Wide/Fullwidth plus the emoji blocks the report actually emits
/// (🟢🟡🔴⚪). Ambiguous-width characters (box drawing, `→`, `⚠`, `…`, the
/// sparkline blocks `▁▂▃…█`) stay at one column: that is what terminals do by
/// default, and it keeps this deterministic instead of locale-dependent.
pub fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

fn char_width(c: char) -> usize {
    let cp = c as u32;
    if cp == 0 || is_zero_width(cp) {
        0
    } else if is_wide(cp) {
        2
    } else {
        1
    }
}

fn is_zero_width(cp: u32) -> bool {
    matches!(cp,
        0x200B..=0x200F          // zero-width space, ZWNJ, ZWJ, LRM, RLM
        | 0x0300..=0x036F        // combining diacritical marks
        | 0x0483..=0x0489
        | 0x0591..=0x05BD
        | 0x0610..=0x061A
        | 0x064B..=0x065F
        | 0x06D6..=0x06DC
        | 0x0E31 | 0x0E34..=0x0E3A | 0x0E47..=0x0E4E
        | 0x1AB0..=0x1AFF
        | 0x1DC0..=0x1DFF
        | 0x20D0..=0x20F0
        | 0xFE20..=0xFE2F)
}

fn is_wide(cp: u32) -> bool {
    matches!(cp,
        0x1100..=0x115F   // Hangul Jamo
        | 0x2329..=0x232A
        | 0x2E80..=0x303E // CJK radicals, Kangxi, CJK symbols and punctuation
        | 0x3041..=0x33FF // Hiragana … CJK compatibility
        | 0x3400..=0x4DBF // CJK ext A
        | 0x4E00..=0x9FFF // CJK unified ideographs
        | 0xA000..=0xA4CF // Yi
        | 0xA960..=0xA97F
        | 0xAC00..=0xD7A3 // Hangul syllables
        | 0xF900..=0xFAFF // CJK compatibility ideographs
        | 0xFE10..=0xFE19
        | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60 // fullwidth forms
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F
        | 0x1F680..=0x1F6FF // transport and map symbols
        | 0x1F7E0..=0x1F7FF // 🟢🟡🔴⚪ — the traffic lights this report ships
        | 0x1F900..=0x1F9FF
        | 0x1FA70..=0x1FAFF
        | 0x20000..=0x3FFFD)
}

/// Left-justify to `width` **terminal columns**, truncating if longer.
pub fn pad_display(s: &str, width: usize) -> String {
    let body = truncate_display(s, width);
    let pad = width.saturating_sub(display_width(&body));
    format!("{body}{}", " ".repeat(pad))
}

/// Right-justify to `width` **terminal columns**, truncating if longer.
pub fn pad_display_right(s: &str, width: usize) -> String {
    let body = truncate_display(s, width);
    let pad = width.saturating_sub(display_width(&body));
    format!("{}{body}", " ".repeat(pad))
}

/// Cut to at most `width` terminal columns, marking a cut with `…`.
///
/// The ellipsis itself costs one column, so at `width == 1` the result is `…`
/// and at `width == 0` it is empty. Never splits a grapheme: it walks chars.
pub fn truncate_display(s: &str, width: usize) -> String {
    if display_width(s) <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for c in s.chars() {
        let w = char_width(c);
        if used + w > width - 1 {
            break;
        }
        used += w;
        out.push(c);
    }
    out.push('…');
    out
}

/// A `width`-column bar filled in proportion to `value / max`.
///
/// Always exactly `width` columns wide, so the next column still lines up.
pub fn bar(value: f64, max: f64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let filled = if max <= 0.0 || value <= 0.0 {
        0
    } else {
        ((value / max * width as f64).round() as usize).clamp(1, width)
    };
    let mut s = "█".repeat(filled);
    if filled < width {
        s.push_str(&" ".repeat(width - filled));
    }
    s
}

/// [`bar`] without the trailing pad: for inline mini-charts whose next group
/// supplies its own separator.
pub fn hbar(value: f64, max: f64, width: usize) -> String {
    if max <= 0.0 || value <= 0.0 {
        return String::new();
    }
    "█".repeat(((value / max * width as f64).round() as usize).clamp(1, width))
}

/// Python's `Py_UNICODE_ISSPACE`, which `str.strip()` and `str.split()` use.
/// It differs from Rust's `char::is_whitespace` by also treating U+001C-U+001F
/// (the ASCII file/group/record/unit separators) as whitespace.
pub fn py_isspace(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}')
}

/// Python's `str.strip()` with no arguments.
pub fn py_strip(s: &str) -> String {
    s.trim_matches(py_isspace).to_string()
}

/// Python's `str.split()` with no arguments: split on runs of whitespace and
/// discard empty fields.
pub fn py_split_whitespace(s: &str) -> Vec<&str> {
    s.split(py_isspace).filter(|t| !t.is_empty()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi_and_bidi() {
        assert_eq!(safe("a\u{1b}[31mred\u{1b}[0m"), "a?[31mred?[0m");
        assert_eq!(safe("a\u{202e}b"), "a?b");
        assert_eq!(safe("plain"), "plain");
        assert_eq!(safe("ＡＢＣ"), "ＡＢＣ");
    }

    #[test]
    fn safe_takes_str_so_display_uses_it() {
        // Python's `safe(s: object)` accepted non-str; every call site passed a
        // str, so the Rust signature is narrower on purpose.
        assert_eq!(safe(&format!("{}", 42)), "42");
    }

    #[test]
    fn chars_take_splits_multibyte_cleanly() {
        assert_eq!(chars_take("ＡＢＣＤＥＦ", 2), "ＡＢ");
        // `&s[..2]` would panic here.
        assert_eq!(chars_take("éabc", 1), "é");
    }

    #[test]
    fn pad_left_matches_python_percent_minus() {
        assert_eq!(pad_left("ab", 5), "ab   ");
        assert_eq!(pad_left("abcdefgh", 5), "abcde");
        assert_eq!(pad_left("ＡＢ", 4), "ＡＢ  ");
    }

    #[test]
    fn display_width_counts_cjk_as_two_columns() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("ＡＢ"), 4);
        assert_eq!(display_width("🟢"), 2);
        assert_eq!(display_width("→"), 1);
        // "9 columns of glyph, 7 columns of terminal" is the bug this fixes.
        assert_eq!(display_width("ＡＢabc"), 7);
    }

    #[test]
    fn pad_display_aligns_cjk_and_ascii_in_one_column() {
        // `pad_left(.., 6)` would pad by character count, not terminal columns.
        assert_eq!(pad_display("ＡＢ", 6), "ＡＢ  ");
        assert_eq!(pad_display("abc", 6), "abc   ");
        assert_eq!(display_width(&pad_display("ＡＢ", 6)), 6);
        assert_eq!(pad_display_right("12", 5), "   12");
        assert_eq!(pad_display("ＡＢabc", 6), "ＡＢa…");
        assert_eq!(pad_display("ＡＢＣＤＥＦＧ", 6), "ＡＢ… ");
    }

    #[test]
    fn truncate_display_marks_the_cut_and_stays_within_width() {
        assert_eq!(truncate_display("abcdef", 4), "abc…");
        assert_eq!(truncate_display("ＡＢＣＤ", 5), "ＡＢ…");
        assert_eq!(truncate_display("ＡＢＣＤ", 4), "Ａ…");
        assert_eq!(truncate_display("ＡＢＣＤ", 6), "ＡＢ…");
        assert_eq!(truncate_display("ＡＢab", 6), "ＡＢab");
        assert_eq!(truncate_display("abc", 0), "");
        for w in 1..=8 {
            assert!(display_width(&truncate_display("ＡＢabc", w)) <= w);
        }
    }

    #[test]
    fn bar_is_fixed_width_and_proportional() {
        assert_eq!(bar(0.0, 10.0, 4), "    ");
        assert_eq!(bar(10.0, 10.0, 4), "████");
        assert_eq!(bar(5.0, 10.0, 4), "██  ");
        assert_eq!(bar(1.0, 100.0, 10), "█         ");
        assert_eq!(bar(5.0, 0.0, 4), "    ");
        for v in [0.0, 1.0, 7.0, 99.0] {
            assert_eq!(display_width(&bar(v, 100.0, 12)), 12);
        }
    }

    #[test]
    fn hbar_is_unpadded_but_never_empty_for_a_positive_value() {
        assert_eq!(hbar(0.0, 10.0, 6), "");
        assert_eq!(hbar(1.0, 10.0, 6), "█");
        assert_eq!(hbar(10.0, 10.0, 6), "██████");
        assert_eq!(hbar(5.0, 10.0, 6), "███");
    }

    #[test]
    fn python_whitespace_includes_the_ascii_separators() {
        for ws in [
            "\u{1c}", "\u{1d}", "\u{1e}", "\u{1f}", "\u{0b}", "\u{0c}", "\u{a0}",
        ] {
            assert!(py_isspace(ws.chars().next().unwrap()), "{ws:?}");
        }
        assert!(!py_isspace('x'));
        assert_eq!(py_strip("\u{1e}\u{a0} body \u{1f}"), "body");
        assert_eq!(py_split_whitespace(" a\u{1e}b  c "), vec!["a", "b", "c"]);
    }
}
