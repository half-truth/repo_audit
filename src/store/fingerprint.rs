//! Cache fingerprints must be **byte-identical to Python's**, or every existing
//! `.audit.db` is silently invalidated and the whole history gets re-scanned.
//!
//! `serde_json` cannot be used here for two independent reasons: it emits `,`
//! and `:` without the space Python's default separators include, and its
//! `Value::Object` is a `BTreeMap` unless `preserve_order` is enabled. There
//! are no dicts in the fingerprint inputs, so neither matters — a hand-rolled
//! encoder makes the fingerprint a compile-time-checked function of its inputs
//! instead of a runtime JSON encoding that can drift.
//!
//! Used only for fingerprints. User-visible `--json` output goes through
//! `serde_json` and is a different (much looser) compatibility surface.

/// Reproduce CPython's `json.encoder.encode_basestring_ascii`.
///
/// `ensure_ascii` defaults to True, so every non-ASCII character becomes
/// `\uXXXX`. The trap: CPython emits a **surrogate pair** for astral-plane
/// characters, not `\U0001F600`.
pub fn py_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if (c as u32) < 0x7f => out.push(c),
            c => {
                let cp = c as u32;
                if cp <= 0xffff {
                    out.push_str(&format!("\\u{:04x}", cp));
                } else {
                    let v = cp - 0x10000;
                    out.push_str(&format!(
                        "\\u{:04x}\\u{:04x}",
                        0xd800 + (v >> 10),
                        0xdc00 + (v & 0x3ff)
                    ));
                }
            }
        }
    }
    out.push('"');
    out
}

/// The only shapes `_fingerprint` was ever called with: fixed-arity tuples of
/// `int | bool | list[str] | str`. No dicts, so `sort_keys=True` is a no-op and
/// `default=str` is never reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FpPart {
    Int(i64),
    Bool(bool),
    Str(String),
    StrList(Vec<String>),
}

impl FpPart {
    fn render(&self) -> String {
        match self {
            FpPart::Int(i) => i.to_string(),
            FpPart::Bool(b) => if *b { "true" } else { "false" }.to_string(),
            FpPart::Str(s) => py_str(s),
            FpPart::StrList(v) => {
                let items: Vec<String> = v.iter().map(|s| py_str(s)).collect();
                format!("[{}]", items.join(", "))
            }
        }
    }
}

/// `hashlib.sha1(json.dumps(parts)).hexdigest()[:16]`.
///
/// The `", "` join is Python's default `json.dumps` item separator and is the
/// entire reason `serde_json::to_string` is unusable here.
pub fn fingerprint(parts: &[FpPart]) -> String {
    use sha1::{Digest, Sha1};
    let payload = format!(
        "[{}]",
        parts
            .iter()
            .map(FpPart::render)
            .collect::<Vec<_>>()
            .join(", ")
    );
    let digest = Sha1::digest(payload.as_bytes());
    digest
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()[..16]
        .to_string()
}

/// `sha1(file bytes)[:16]`, or a sentinel when the file cannot be read.
pub fn file_fp(path: Option<&str>) -> String {
    use sha1::{Digest, Sha1};
    let Some(p) = path.filter(|s| !s.is_empty()) else {
        return String::new();
    };
    match std::fs::read(p) {
        Ok(bytes) => {
            let d = Sha1::digest(&bytes);
            d.iter().map(|b| format!("{b:02x}")).collect::<String>()[..16].to_string()
        }
        Err(_) => "unreadable".to_string(),
    }
}

/// `json.dumps(x, ensure_ascii=False)` — the encoding used for the
/// `aliases_json` / `reasons_json` columns, which the report prints verbatim.
///
/// Same `", "` / `": "` separators as [`py_str`]'s caller, but non-ASCII
/// characters stay literal instead of becoming `\uXXXX`. A lone `\` or `"` in a
/// name still has to be escaped, so this is `py_str` with the ASCII branch
/// widened to all of Unicode.
pub fn py_json(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => "null".into(),
        serde_json::Value::Bool(b) => {
            if *b {
                "true".into()
            } else {
                "false".into()
            }
        }
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => json_str_literal(s),
        serde_json::Value::Array(a) => {
            let items: Vec<String> = a.iter().map(py_json).collect();
            format!("[{}]", items.join(", "))
        }
        // `serde_json` is built with `preserve_order`, so object key order is
        // the insertion order Python's dict would have had.
        serde_json::Value::Object(o) => {
            let items: Vec<String> = o
                .iter()
                .map(|(k, v)| format!("{}: {}", json_str_literal(k), py_json(v)))
                .collect();
            format!("{{{}}}", items.join(", "))
        }
    }
}

fn json_str_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::patterns::DEFAULT_BOT_RE;

    #[test]
    fn py_str_matches_cpython_ensure_ascii() {
        // Every right-hand side was dumped from CPython 3.14's json.dumps and is
        // written here as the literal *escape text* it produces.
        for (input, cpython) in [
            ("plain", r#""plain""#),
            ("quote\"back\\slash", r#""quote\"back\\slash""#),
            ("tab\tnl\n", r#""tab\tnl\n""#),
            // The trap: a surrogate pair, not \U0001F600.
            ("emoji 😀", r#""emoji \ud83d\ude00""#),
            ("ünï", r#""\u00fcn\u00ef""#),
            ("ctrl\u{1}", r#""ctrl\u0001""#),
            ("", r#""""#),
            ("slash/", r#""slash/""#),
        ] {
            assert_eq!(py_str(input), cpython, "py_str({input:?})");
        }
    }

    #[test]
    fn list_rendering_uses_python_separators() {
        let v = [
            FpPart::StrList(vec!["a".into(), "b".into()]),
            FpPart::StrList(vec![]),
        ];
        assert_eq!(
            v.iter().map(FpPart::render).collect::<Vec<_>>().join(", "),
            r#"["a", "b"], []"#
        );
    }

    #[test]
    fn fingerprints_match_cpython_byte_for_byte() {
        // Golden constants dumped from the Python implementation. If these move,
        // every existing cache is invalidated and the whole history is rescanned.
        //
        // The Python original was deleted on 2026-10-05; it is still recoverable
        // with `git show dc79b69^:repo_audit_mvp.py` if these ever need re-dumping.
        // They are load-bearing either way: `ingest_fp` gates every cache, so a
        // silent change here means a full rescan of every repository, not a diff.
        assert_eq!(
            fingerprint(&[
                FpPart::Int(1),
                FpPart::Int(1),
                FpPart::Str("HEAD".into()),
                FpPart::Str("0123456789abcdef".into()),
            ]),
            "a27bb13d51b83c27"
        );
        assert_eq!(
            fingerprint(&[
                FpPart::Int(1),
                FpPart::Int(1),
                FpPart::Str("HEAD".into()),
                FpPart::Str(String::new()),
            ]),
            "a99a82f2d809958e"
        );
        assert_eq!(
            fingerprint(&[
                FpPart::Int(3),
                FpPart::Bool(true),
                FpPart::StrList(vec![]),
                FpPart::Str(DEFAULT_BOT_RE.into()),
                FpPart::Str("0123456789abcdef".into()),
            ]),
            "12bc7ed8a7ea9790"
        );
        assert_eq!(
            fingerprint(&[
                FpPart::Int(3),
                FpPart::Bool(false),
                FpPart::StrList(vec!["^a$".into()]),
                FpPart::Str(DEFAULT_BOT_RE.into()),
                FpPart::Str("0123456789abcdef".into()),
            ]),
            "ae8e090e6bb4f55f"
        );
        assert_eq!(
            fingerprint(&[
                FpPart::Int(3),
                FpPart::Bool(true),
                FpPart::StrList(vec!["^ci$".into(), "^bot2$".into()]),
                FpPart::Str(DEFAULT_BOT_RE.into()),
                FpPart::Str("deadbeefdeadbeef".into()),
            ]),
            "128285caf66858ef"
        );
    }

    #[test]
    fn file_fp_handles_missing_and_absent_paths() {
        assert_eq!(file_fp(None), "");
        assert_eq!(file_fp(Some("")), "");
        assert_eq!(file_fp(Some("/definitely/not/here")), "unreadable");
    }
}
