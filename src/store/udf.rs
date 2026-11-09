//! Python regexes and predicates exposed to SQLite as scalar functions.
//!
//! File-level rankings run over hundreds of thousands of rows, so the *filter*
//! has to happen inside SQLite rather than by pulling every row into memory and
//! matching in the host. SQL does the grouping, Python/Rust does the regex —
//! this module is the seam.

use regex::{Regex, RegexBuilder};
use rusqlite::functions::{Context, FunctionFlags};
use rusqlite::Connection;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// git's own revert marker is far more precise than keyword matching: `git
/// revert` always writes the subject as `Revert "<original>"` and the body as
/// `This reverts commit <sha>`.
fn revert_subject_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"^(?:Revert "|Revert ')"#).unwrap())
}

fn revert_ref_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    // Python's `re.M` — `^` matches after any newline, not just at the start.
    RE.get_or_init(|| {
        RegexBuilder::new(r"^This reverts commit ([0-9a-f]{7,40})")
            .multi_line(true)
            .build()
            .unwrap()
    })
}

/// Patterns arrive as bind parameters, so the same few strings recur across
/// every row. Compiling per row would dominate the runtime of a large scan.
type PatternCache = Arc<Mutex<HashMap<String, Option<Arc<Regex>>>>>;

pub struct Udf {
    cache: PatternCache,
}

impl Default for Udf {
    fn default() -> Self {
        Self::new()
    }
}

impl Udf {
    pub fn new() -> Self {
        Self {
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// `re.search(pattern, s, re.I)`, cached by `(pattern, case_insensitive)`.
    ///
    /// A pattern that fails to compile is cached as `None` so the failure costs
    /// one parse, not one per row.
    pub fn compiled(&self, pattern: &str, ci: bool) -> Option<Arc<Regex>> {
        let key = format!("{pattern}\u{0}{ci}");
        let mut c = self.cache.lock().unwrap();
        if let Some(hit) = c.get(&key) {
            return hit.clone();
        }
        let compiled = RegexBuilder::new(pattern)
            .case_insensitive(ci)
            .build()
            .ok()
            .map(Arc::new);
        c.insert(key, compiled.clone());
        compiled
    }

    pub fn register(&self, conn: &Connection) -> rusqlite::Result<()> {
        let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;

        let cache = Arc::clone(&self.cache);
        conn.create_scalar_function("rx", 2, flags, move |ctx: &Context| {
            let pat: Option<String> = ctx.get(0)?;
            let text: Option<String> = ctx.get(1)?;
            // Python: `1 if (pattern and s and re.search(...)) else 0`
            let hit = match (pat, text) {
                (Some(p), Some(s)) if !p.is_empty() && !s.is_empty() => {
                    let mut c = cache.lock().unwrap();
                    let key = format!("{p}\u{0}true");
                    let re = c
                        .entry(key.clone())
                        .or_insert_with(|| {
                            RegexBuilder::new(&p)
                                .case_insensitive(true)
                                .build()
                                .ok()
                                .map(Arc::new)
                        })
                        .clone();
                    re.map(|r| r.is_match(&s)).unwrap_or(false)
                }
                _ => false,
            };
            Ok(hit as i64)
        })?;

        conn.create_scalar_function("isrevert", 2, flags, move |ctx: &Context| {
            let subject: Option<String> = ctx.get(0)?;
            let body: Option<String> = ctx.get(1)?;
            // Python's `re.match`/`re.search` raise on None, so the SQL layer
            // must be passing empty strings, not NULL — mirror that: a NULL
            // (or absent) subject simply is not a revert.
            let hit = subject
                .as_deref()
                .is_some_and(|s| revert_subject_re().is_match(s))
                || body.as_deref().is_some_and(|b| revert_ref_re().is_match(b));
            Ok(hit as i64)
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rx_matches_case_insensitively_and_caches() {
        let u = Udf::new();
        let r = u.compiled("\\b(fix|bug)\\b", true).unwrap();
        assert!(r.is_match("Fix the parser"));
        assert!(r.is_match("fix: 123"));
        // No word boundary inside a longer word — verified against Python's
        // `re`, which agrees here: `\b` needs a non-word char on both sides.
        assert!(!r.is_match("BUGFIX-1"));
        assert!(!r.is_match("prefix change"));
        // Second call must hit the cache, not recompile.
        assert!(Arc::ptr_eq(
            &r,
            &u.compiled("\\b(fix|bug)\\b", true).unwrap()
        ));
    }

    #[test]
    fn a_broken_pattern_is_cached_as_none_not_panicked() {
        let u = Udf::new();
        assert!(u.compiled("(", true).is_none());
        assert!(u.compiled("(", true).is_none());
    }

    #[test]
    fn revert_detection_matches_gits_own_format() {
        assert!(revert_subject_re().is_match(r#"Revert "add feature""#));
        assert!(revert_subject_re().is_match("Revert 'old thing'"));
        assert!(!revert_subject_re().is_match("reverted the thing"));
        // Multi-line: the marker line need not be the first line.
        assert!(revert_ref_re().is_match("some noise\nThis reverts commit 1234567abc"));
        assert!(!revert_ref_re().is_match("This reverts commit 12345")); // 5 hex < 7
    }
}
