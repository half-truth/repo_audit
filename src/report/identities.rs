//! `identities` — the merge decisions, for review.
//!
//! This is the audit trail for the one heuristic that can silently get a
//! project wrong: two different people merged into one identity, or one person
//! split across several. It shows only merged groups unless `--all`.
//!
//! Rendering is fallible on purpose: an unreadable database must fail with
//! message + hint, never render as an empty "No merges require review" — the
//! caller could not tell that apart from a clean run.

use crate::error::Result;
use crate::index::IndexResult;
use crate::util::text::safe;

pub fn render_identities(res: &IndexResult, show_all: bool) -> Result<String> {
    let mut l: Vec<String> = Vec::new();
    for g in res.store.author_groups()? {
        let aliases = g["aliases"].as_array().cloned().unwrap_or_default();
        if !show_all && aliases.len() < 2 {
            continue;
        }
        let is_bot = g["is_bot"].as_bool().unwrap_or(false);
        l.push(format!(
            "{} <{}>  {} commits{}",
            safe(g["name"].as_str().unwrap_or("")),
            safe(g["email"].as_str().unwrap_or("")),
            g["commits"].as_i64().unwrap_or(0),
            if is_bot { "  [bot]" } else { "" }
        ));
        for a in &aliases {
            l.push(format!(
                "    · {} <{}>  ×{}",
                safe(a["name"].as_str().unwrap_or("")),
                safe(a["email"].as_str().unwrap_or("")),
                a["commits"].as_i64().unwrap_or(0)
            ));
        }
        if let Some(reasons) = g["reasons"].as_array() {
            if !reasons.is_empty() {
                let joined: Vec<String> = reasons
                    .iter()
                    .map(|r| safe(r.as_str().unwrap_or("")))
                    .collect();
                l.push(format!("    Merge basis: {}", joined.join(", ")));
            }
        }
    }
    if l.is_empty() {
        l.push("No merges require review.".to_string());
    } else {
        l.push(String::new());
        l.push("If identities were merged incorrectly, use --no-name-merge; alternatively, add override rules in git mailmap format to ~/.config/repo-audit/mailmap and rerun with --reindex.".to_string());
    }
    Ok(l.join("\n"))
}
