//! `--json` output.
//!
//! Key order is the summary's key order followed by the run-specific keys,
//! because Python's `dict.update` appends and `json.dumps` preserves insertion
//! order. `serde_json` is built with `preserve_order` for the same reason.

use crate::analyzers::bus_rows;
use crate::index::IndexResult;
use crate::normalize::patterns::{FIRE_RE, FIX_RE};
use crate::store::sqlite::Summary;
use crate::SCHEMA_VERSION;
use rusqlite::params;
use serde_json::{json, Map, Value};

/// The summary fields Python's `summary()` returns, in its own key order.
fn summary_value(s: &Summary) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("commits".into(), json!(s.commits));
    m.insert("merges".into(), json!(s.merges));
    m.insert("first_ts".into(), json!(s.first_ts));
    m.insert("last_ts".into(), json!(s.last_ts));
    m.insert("humans".into(), json!(s.humans));
    m.insert("bots".into(), json!(s.bots));
    m.insert("pr_suffix_share".into(), json!(s.pr_suffix_share));
    m.insert("workflow_hint".into(), json!(s.workflow_hint));
    m.insert(
        "top_authors".into(),
        Value::Array(
            s.top_authors
                .iter()
                .map(|a| {
                    json!({
                        "name": a.name,
                        "email": a.email,
                        "commits": a.commits,
                        "share": a.share,
                        "aliases": a.aliases,
                        "last_ts": a.last_ts,
                    })
                })
                .collect(),
        ),
    );
    m.insert(
        "top_files".into(),
        Value::Array(
            s.top_files
                .iter()
                .map(|f| json!({"path": f.path, "touches": f.touches, "churn": f.churn, "loc": f.loc}))
                .collect(),
        ),
    );
    m.insert(
        "files_with_rename_history".into(),
        json!(s.files_with_rename_history),
    );
    m.insert("head_files".into(), json!(s.head_files));
    m
}

pub fn render_json(res: &IndexResult) -> Value {
    let store = &res.store;
    let s = store.summary().unwrap();
    let mut m = summary_value(&s);

    let mut churn: Vec<(String, i64)> = Vec::new();
    {
        let mut st = store
            .conn
            .prepare(
                "SELECT f.path_current, SUM(f.added+f.deleted) FROM file_changes f
                 JOIN commits c ON c.sha=f.sha JOIN files_at_head h ON h.path=f.path_current
                 WHERE c.is_merge=0 AND f.is_binary=0
                 GROUP BY f.path_current ORDER BY 2 DESC LIMIT 200",
            )
            .unwrap();
        let rows = st
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .unwrap();
        for r in rows {
            churn.push(r.unwrap());
        }
    }
    let fix_commits: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM commits WHERE is_merge=0 AND rx(?1, subject || char(10) || body)",
            params![FIX_RE],
            |r| r.get(0),
        )
        .unwrap();
    let (fire_total, fire_native): (i64, i64) = store
        .conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(isrevert(subject, body)),0) FROM commits
             WHERE is_merge=0 AND (isrevert(subject, body) OR rx(?1, subject))",
            params![FIRE_RE],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();

    m.insert("repo".into(), json!(res.display));
    m.insert("ref".into(), json!(res.git_ref));
    m.insert("head".into(), json!(res.head));
    m.insert("shallow".into(), json!(res.shallow));
    m.insert("action".into(), json!(res.action.as_str()));
    m.insert("warnings".into(), json!(res.warnings));
    m.insert("schema_version".into(), json!(SCHEMA_VERSION));
    m.insert("fix_commits".into(), json!(fix_commits));
    m.insert("firefight_commits".into(), json!(fire_total));
    m.insert("firefight_native".into(), json!(fire_native));
    m.insert(
        "top_churn".into(),
        Value::Array(
            churn
                .iter()
                .map(|(f, n)| json!({"path": f, "churn": n}))
                .collect(),
        ),
    );
    let _ = bus_rows(store);
    Value::Object(m)
}

/// Python printed with `indent=2` and `ensure_ascii=False`; serde_json's
/// pretty printer is 2-space and never escapes non-ASCII.
pub fn to_pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
}
