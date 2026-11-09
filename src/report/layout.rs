//! Shared layout for the terminal report.
//!
//! Everything here measures in **terminal columns**, not characters or bytes:
//! author names may contain wide characters, and `char::count()` padding
//! silently misaligns every row that mixes narrow and wide text. See
//! [`crate::util::text::display_width`].
//!
//! No ANSI colour. Colour would make the report depend on whether stdout is a
//! TTY, and the byte-for-byte golden gate (`AGENTS.md`) compares piped output —
//! a TTY-conditional escape would either vanish from the gate or bleed into
//! scripts. Structure (rules, bars, alignment) carries the emphasis instead.

use crate::util::text::{display_width, pad_display, truncate_display};

/// Longest line the report will emit. Paths and messages are truncated past it.
pub const MAX_WIDTH: usize = 100;

/// A thin rule of exactly `n` columns.
pub fn rule(n: usize) -> String {
    "─".repeat(n)
}

/// The width of the widest line, so a rule never comes up short.
pub fn widest(lines: &[String]) -> usize {
    lines.iter().map(|l| display_width(l)).max().unwrap_or(0)
}

/// Titled, aligned header block inside a heavy rule that actually closes.
///
/// The old header was `"═".repeat(64)` under a line that was often twice that,
/// so the box leaked open on every non-ASCII path. The rule is now sized to the
/// content.
pub fn header_block(fields: &[(&str, String)]) -> String {
    let label_w = fields
        .iter()
        .map(|(k, _)| display_width(k))
        .max()
        .unwrap_or(0);
    let body: Vec<String> = fields
        .iter()
        .map(|(k, v)| {
            format!(
                "  {}  {}",
                pad_display(k, label_w),
                truncate_display(v, MAX_WIDTH - 4)
            )
        })
        .collect();
    let w = widest(&body).max(24);
    let mut out = String::new();
    out.push_str(&"═".repeat(w));
    out.push('\n');
    for line in &body {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&"═".repeat(w));
    out
}

/// `[1/5 Title] Subtitle` followed by a thin rule of the same width.
///
/// The rule is sized to the header it belongs to, which is what makes a section
/// boundary greppable at a glance.
pub fn section(num: usize, title: &str, subtitle: &str) {
    let head = truncate_display(&format!("[{num}/5 {title}] {subtitle}"), MAX_WIDTH);
    println!("\n{head}");
    println!("{}", rule(display_width(&head)));
}

/// A labelled sub-block inside a section, for the two rankings in item 3.
pub fn sub_head(label: &str) {
    println!("  ── {}", truncate_display(label, MAX_WIDTH - 6));
}

/// The trailing disclaimer, with the prose kept short enough to read.
pub fn footer(lines: &[&str]) {
    println!();
    for line in lines {
        println!("{}", truncate_display(line, MAX_WIDTH));
    }
}
