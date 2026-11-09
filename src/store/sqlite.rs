//! The only data interface. Analyzers query this; SQL never leaks into the
//! render layer.

use super::schema::{DATA_TABLES, SCHEMA};
use super::udf::Udf;
use crate::error::{AuditError, Result};
use crate::ingest::record::CommitRecord;
use crate::normalize::identity::{AuthorGroup, IdentityRow};
use crate::SCHEMA_VERSION;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

pub struct SqliteStore {
    pub conn: Connection,
}

pub struct Summary {
    pub commits: i64,
    pub merges: i64,
    pub first_ts: Option<i64>,
    pub last_ts: Option<i64>,
    pub humans: i64,
    pub bots: i64,
    pub pr_suffix_share: f64,
    pub workflow_hint: String,
    pub top_authors: Vec<TopAuthor>,
    pub top_files: Vec<TopFile>,
    pub files_with_rename_history: i64,
    pub head_files: i64,
}

pub struct TopAuthor {
    pub name: String,
    pub email: String,
    pub commits: i64,
    pub share: f64,
    pub aliases: i64,
    pub last_ts: Option<i64>,
}

pub struct TopFile {
    pub path: String,
    pub touches: i64,
    pub churn: i64,
    pub loc: Option<i64>,
}

impl SqliteStore {
    /// Open, or rebuild from scratch when the cached schema is a different
    /// version. Deleting is the right move *because this is a cache*.
    pub fn open(path: &str) -> Result<Self> {
        if path != ":memory:" {
            if let Some(p) = Path::new(path).parent() {
                std::fs::create_dir_all(p).map_err(|e| {
                    AuditError::with_hint(
                        format!("Failed to create database directory {}: {e}", p.display()),
                        "Check directory permissions, or use --cache-dir to choose another location",
                    )
                })?;
            }
        }
        let udf = Udf::new();
        let conn = Self::connect(path, &udf)?;
        let mut st = SqliteStore { conn };
        let ver: i64 = st
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap_or(0);
        if ver != 0 && ver != SCHEMA_VERSION {
            drop(st);
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{path}{suffix}"));
            }
            st = SqliteStore {
                conn: Self::connect(path, &udf)?,
            };
        }
        let ver2: i64 = st
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap_or(0);
        if ver2 == 0 {
            st.conn.execute_batch(SCHEMA)?;
            st.conn
                .execute_batch(&format!("PRAGMA user_version={SCHEMA_VERSION}"))?;
        }
        Ok(st)
    }

    fn connect(path: &str, udf: &Udf) -> Result<Connection> {
        let conn = Connection::open(path)?;
        // Python passed `isolation_level=None` (autocommit) and manages BEGIN
        // explicitly; rusqlite's Transaction API does the same thing.
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "synchronous", "NORMAL").ok();
        udf.register(&conn)?;
        Ok(conn)
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key=?1", params![key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn set_meta(&self, pairs: &[(&str, String)]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut s = tx.prepare(
                "INSERT INTO meta(key,value) VALUES(?1,?2)
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            )?;
            for (k, v) in pairs {
                s.execute(params![k, v])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn reset(&self) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for t in DATA_TABLES {
            tx.execute(&format!("DELETE FROM {t}"), [])?;
        }
        tx.execute("DELETE FROM meta", [])?;
        tx.commit()?;
        Ok(())
    }

    // —— ingest ——

    pub fn existing_shas(&self) -> Result<std::collections::HashSet<String>> {
        let mut s = self.conn.prepare("SELECT sha FROM commits")?;
        let rows = s.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = std::collections::HashSet::new();
        for r in rows {
            out.insert(r?);
        }
        Ok(out)
    }

    pub fn min_seq(&self) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row("SELECT MIN(seq) FROM commits", [], |r| {
                r.get::<_, Option<i64>>(0)
            })?)
    }

    pub fn commit_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM commits", [], |r| r.get(0))?)
    }

    /// One transaction per batch. `fault_after` is test-only injection: Python's
    /// `test_resume_after_interrupt` monkey-patched this method, and Rust has no
    /// equivalent, so the hook is explicit rather than hidden.
    pub fn insert_commit_batch(
        &mut self,
        batch: &[(i64, CommitRecord)],
        fault_after: Option<&dyn Fn(usize) -> Result<()>>,
    ) -> Result<()> {
        let mut crow: Vec<Box<dyn rusqlite::ToSql>> = Vec::with_capacity(batch.len());
        let mut frow: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        for (seq, c) in batch {
            let ins: i64 = c.files.iter().map(|f| f.added).sum();
            let dele: i64 = c.files.iter().map(|f| f.deleted).sum();
            crow.push(Box::new(c.sha.clone()));
            crow.push(Box::new(*seq));
            crow.push(Box::new(c.parents.join(" ")));
            crow.push(Box::new(if c.parents.len() > 1 { 1i64 } else { 0i64 }));
            crow.push(Box::new(c.author_name.clone()));
            crow.push(Box::new(c.author_email.clone()));
            crow.push(Box::new(c.author_ts));
            crow.push(Box::new(c.commit_ts));
            crow.push(Box::new(c.subject.clone()));
            crow.push(Box::new(c.body.clone()));
            crow.push(Box::new(c.files.len() as i64));
            crow.push(Box::new(ins));
            crow.push(Box::new(dele));
            for f in &c.files {
                frow.push(Box::new(c.sha.clone()));
                frow.push(Box::new(f.status.clone()));
                frow.push(Box::new(f.path.clone()));
                frow.push(Box::new(f.old_path.clone()));
                frow.push(Box::new(f.added));
                frow.push(Box::new(f.deleted));
                frow.push(Box::new(if f.is_binary { 1i64 } else { 0i64 }));
            }
        }
        let tx = self.conn.transaction()?;
        {
            let mut s = tx.prepare(
                "INSERT OR IGNORE INTO commits(sha,seq,parents,is_merge,author_name,author_email,
                   author_ts,commit_ts,subject,body,files_changed,insertions,deletions)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            )?;
            for row in crow.chunks(13) {
                s.execute(rusqlite::params_from_iter(row.iter().map(|b| b.as_ref())))?;
            }
        }
        {
            let mut s = tx.prepare(
                "INSERT INTO file_changes(sha,status,path,old_path,added,deleted,is_binary)
                 VALUES(?1,?2,?3,?4,?5,?6,?7)",
            )?;
            let mut n_done = 0usize;
            for row in frow.chunks(7) {
                s.execute(rusqlite::params_from_iter(row.iter().map(|b| b.as_ref())))?;
                if let Some(f) = fault_after {
                    // Fires *inside* the transaction so the rollback path is
                    // exercised the same way a real interrupt would.
                    f(n_done)?;
                    n_done += 1;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Resolve every file's current name by replaying the history newest-first.
    ///
    /// `alias` maps an old path to the name it has today (`None` = no longer in
    /// HEAD). The direction matters: renames chain (`a.kt → b.kt → c.kt`) and
    /// walking the other way leaves `alias` parked on the intermediate name,
    /// so each file only inherits its last leg of history — **silently**.
    /// See the Appended Rules entry for the original `Log.kt` case.
    pub fn rebuild_lineage(&self) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute_batch(
            "UPDATE file_changes SET path_current = CASE WHEN status='D' THEN NULL ELSE path END;
             CREATE TEMP TABLE IF NOT EXISTS k(path TEXT PRIMARY KEY);
             DELETE FROM k;
             INSERT OR IGNORE INTO k SELECT old_path FROM file_changes WHERE status='R' AND old_path IS NOT NULL;
             INSERT OR IGNORE INTO k SELECT path FROM file_changes WHERE status='D';",
        )?;
        let mut s = tx.prepare(
            "SELECT c.seq, f.id, f.status, f.path, f.old_path FROM file_changes f
             JOIN commits c ON c.sha=f.sha
             WHERE f.status IN ('D','R') OR f.path IN (SELECT path FROM k)
             ORDER BY c.seq, f.id",
        )?;
        let rows: Vec<(i64, i64, String, String, Option<String>)> = s
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
        drop(s);

        let mut alias: std::collections::HashMap<String, Option<String>> =
            std::collections::HashMap::new();
        let mut updates: Vec<(Option<String>, i64)> = Vec::new();
        let mut i = 0usize;
        while i < rows.len() {
            let seq = rows[i].0;
            let mut j = i;
            while j < rows.len() && rows[j].0 == seq {
                j += 1;
            }
            let grp = &rows[i..j];
            // Resolve every row against the pre-commit alias state first...
            let mut curs: Vec<Option<String>> = Vec::with_capacity(grp.len());
            for (_s, fid, st, p, _op) in grp {
                let cur = if st == "D" {
                    None
                } else {
                    alias.get(p).cloned().unwrap_or_else(|| Some(p.clone()))
                };
                if cur.as_deref() != Some(p.as_str()) {
                    updates.push((cur.clone(), *fid));
                }
                curs.push(cur);
            }
            // ...then apply the commit: clear first ("delete then re-create at
            // the same path" must not inherit the dead file's history)...
            for (_s, _fid, st, p, _op) in grp {
                if st == "A" || st == "R" {
                    alias.remove(p);
                }
            }
            // ...then write the new mappings.
            for ((_s, _fid, st, p, op), cur) in grp.iter().zip(&curs) {
                if st == "D" {
                    alias.insert(p.clone(), None);
                } else if st == "R" {
                    if let Some(op) = op {
                        alias.insert(op.clone(), cur.clone());
                    }
                }
            }
            i = j;
        }
        {
            let mut s = tx.prepare("UPDATE file_changes SET path_current=?1 WHERE id=?2")?;
            for (cur, fid) in &updates {
                s.execute(params![cur, fid])?;
            }
        }
        tx.execute_batch("DROP TABLE k;")?;
        tx.commit()?;
        Ok(())
    }

    // —— identity ——

    pub fn identity_rows(&self) -> Result<Vec<IdentityRow>> {
        let mut s = self.conn.prepare(
            "SELECT author_name, author_email, COUNT(*), MIN(author_ts), MAX(author_ts)
             FROM commits GROUP BY author_name, author_email",
        )?;
        let rows = s.query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn write_authors(&self, groups: &[AuthorGroup]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM authors", [])?;
        tx.execute("DELETE FROM identities", [])?;
        {
            let mut a = tx.prepare("INSERT INTO authors VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)")?;
            let mut i = tx.prepare("INSERT INTO identities VALUES(?1,?2,?3)")?;
            for (idx, g) in groups.iter().enumerate() {
                let aid = (idx + 1) as i64;
                // `ensure_ascii=False` — these strings land in the report.
                let aliases = crate::store::fingerprint::py_json(&serde_json::json!(g
                    .members
                    .iter()
                    .map(|(n, e, k)| serde_json::json!({"name": n, "email": e, "commits": k}))
                    .collect::<Vec<_>>()));
                let reasons = crate::store::fingerprint::py_json(&serde_json::json!(g.reasons));
                a.execute(params![
                    aid,
                    g.canonical_name,
                    g.canonical_email,
                    if g.is_bot { 1i64 } else { 0i64 },
                    g.commit_count,
                    g.first_ts,
                    g.last_ts,
                    aliases,
                    reasons
                ])?;
                for (n, e, _k) in &g.members {
                    i.execute(params![n, e, aid])?;
                }
            }
        }
        tx.execute_batch(
            "UPDATE commits SET author_id=(SELECT author_id FROM identities i
               WHERE i.name=commits.author_name AND i.email=commits.author_email)",
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn replace_head_files(&self, rows: &[(String, Option<i64>, i64, i64)]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM files_at_head", [])?;
        {
            let mut s = tx.prepare("INSERT OR REPLACE INTO files_at_head VALUES(?1,?2,?3,?4)")?;
            for (p, loc, size, bin) in rows {
                s.execute(params![p, loc, size, bin])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // —— read-only queries ——

    pub fn summary(&self) -> Result<Summary> {
        let (total, merges, first_ts, last_ts): (i64, i64, Option<i64>, Option<i64>) = self
            .conn
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(is_merge),0), MIN(author_ts), MAX(author_ts) FROM commits",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;
        let humans: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM authors WHERE is_bot=0", [], |r| {
                    r.get(0)
                })?;
        let bots: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM authors WHERE is_bot=1", [], |r| {
                    r.get(0)
                })?;
        let pr_suffix: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM commits WHERE subject GLOB '*(#[0-9]*)'",
            [],
            |r| r.get(0),
        )?;
        let human_total: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM commits c JOIN authors a USING(author_id) WHERE c.is_merge=0 AND a.is_bot=0",
            [],
            |r| r.get(0),
        )?;
        let mut top_authors = Vec::new();
        {
            let mut s = self.conn.prepare(
                "SELECT a.canonical_name, a.canonical_email, COUNT(*) n, json_array_length(a.aliases_json), MAX(c.author_ts)
                 FROM commits c JOIN authors a USING(author_id) WHERE c.is_merge=0 AND a.is_bot=0
                 GROUP BY a.author_id ORDER BY n DESC, a.author_id LIMIT 10",
            )?;
            let rows = s.query_map([], |r| {
                Ok(TopAuthor {
                    name: r.get(0)?,
                    email: r.get(1)?,
                    commits: r.get(2)?,
                    share: 0.0,
                    aliases: r.get(3)?,
                    last_ts: r.get(4)?,
                })
            })?;
            for a in rows {
                let mut a = a?;
                a.share = if human_total > 0 {
                    a.commits as f64 / human_total as f64
                } else {
                    0.0
                };
                top_authors.push(a);
            }
        }
        let mut top_files = Vec::new();
        {
            let mut s = self.conn.prepare(
                "SELECT f.path_current, COUNT(*) n, SUM(f.added+f.deleted), h.loc FROM file_changes f
                 JOIN commits c ON c.sha=f.sha JOIN files_at_head h ON h.path=f.path_current
                 WHERE c.is_merge=0 GROUP BY f.path_current ORDER BY n DESC, f.path_current LIMIT 10",
            )?;
            let rows = s.query_map([], |r| {
                Ok(TopFile {
                    path: r.get(0)?,
                    touches: r.get(1)?,
                    churn: r.get(2)?,
                    loc: r.get(3)?,
                })
            })?;
            for r in rows {
                top_files.push(r?);
            }
        }
        let renamed: i64 = self.conn.query_row(
            "SELECT COUNT(DISTINCT path_current) FROM file_changes
             WHERE path_current IS NOT NULL AND path<>path_current",
            [],
            |r| r.get(0),
        )?;
        let head_files: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM files_at_head", [], |r| r.get(0))?;

        let mut workflow = "linear/unknown".to_string();
        if total > 0 {
            let mt = merges as f64 / total as f64;
            let pt = pr_suffix as f64 / total as f64;
            if mt > 0.15 {
                workflow = "merge-commits".into();
            } else if mt < 0.05 && pt > 0.5 {
                workflow = "squash-merge".into();
            }
        }
        Ok(Summary {
            commits: total,
            merges,
            first_ts,
            last_ts,
            humans,
            bots,
            pr_suffix_share: if total > 0 {
                pr_suffix as f64 / total as f64
            } else {
                0.0
            },
            workflow_hint: workflow,
            top_authors,
            top_files,
            files_with_rename_history: renamed,
            head_files,
        })
    }

    pub fn author_groups(&self) -> Result<Vec<serde_json::Value>> {
        let mut s = self.conn.prepare(
            "SELECT author_id, canonical_name, canonical_email, is_bot, commit_count, first_ts,
                    last_ts, aliases_json, reasons_json
             FROM authors ORDER BY commit_count DESC, author_id",
        )?;
        let mut out = Vec::new();
        let rows = s.query_map([], |r| {
            Ok(serde_json::json!({
                "id": r.get::<_, i64>(0)?,
                "name": r.get::<_, String>(1)?,
                "email": r.get::<_, String>(2)?,
                "is_bot": r.get::<_, i64>(3)? != 0,
                "commits": r.get::<_, i64>(4)?,
                "first_ts": r.get::<_, Option<i64>>(5)?,
                "last_ts": r.get::<_, Option<i64>>(6)?,
                "aliases": serde_json::from_str::<serde_json::Value>(&r.get::<_, String>(7)?)
                    .unwrap_or(serde_json::Value::Null),
                "reasons": serde_json::from_str::<serde_json::Value>(&r.get::<_, String>(8)?)
                    .unwrap_or(serde_json::Value::Null),
            }))
        })?;
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::record::FileChange;

    fn store() -> SqliteStore {
        SqliteStore::open(":memory:").unwrap()
    }

    fn rec(sha: &str, files: Vec<FileChange>) -> CommitRecord {
        CommitRecord {
            sha: sha.into(),
            parents: vec![],
            author_name: "A B".into(),
            author_email: "a@b.c".into(),
            author_ts: 1000,
            commit_ts: 1000,
            subject: "s".into(),
            body: "b".into(),
            files,
        }
    }

    #[test]
    fn round_trips_a_commit_batch() {
        let mut st = store();
        let batch = vec![
            (
                1i64,
                rec(
                    "aaa",
                    vec![FileChange {
                        status: "M".into(),
                        path: "a.txt".into(),
                        old_path: None,
                        added: 3,
                        deleted: 1,
                        is_binary: false,
                    }],
                ),
            ),
            (
                2,
                rec(
                    "bbb",
                    vec![FileChange {
                        status: "A".into(),
                        path: "b.txt".into(),
                        old_path: None,
                        added: 1,
                        deleted: 0,
                        is_binary: false,
                    }],
                ),
            ),
        ];
        st.insert_commit_batch(&batch, None).unwrap();
        assert_eq!(st.commit_count().unwrap(), 2);
        assert_eq!(st.min_seq().unwrap(), Some(1));
        let n: i64 = st
            .conn
            .query_row("SELECT SUM(insertions) FROM commits", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 4);
        let shas = st.existing_shas().unwrap();
        assert!(shas.contains("aaa") && shas.contains("bbb"));
    }

    #[test]
    fn reinserting_the_same_sha_is_ignored_not_duplicated() {
        // This is what makes a resumed index idempotent.
        let mut st = store();
        let b = vec![(1i64, rec("aaa", vec![]))];
        st.insert_commit_batch(&b, None).unwrap();
        st.insert_commit_batch(&b, None).unwrap();
        assert_eq!(st.commit_count().unwrap(), 1);
    }

    #[test]
    fn a_failing_batch_leaves_nothing_behind() {
        let mut st = store();
        let boom: Option<&dyn Fn(usize) -> Result<()>> =
            Some(&|_| Err(AuditError::new("injected")));
        let b = vec![(
            1i64,
            rec(
                "aaa",
                vec![FileChange {
                    status: "M".into(),
                    path: "x".into(),
                    old_path: None,
                    added: 1,
                    deleted: 1,
                    is_binary: false,
                }],
            ),
        )];
        assert!(st.insert_commit_batch(&b, boom).is_err());
        assert_eq!(
            st.commit_count().unwrap(),
            0,
            "the transaction must roll back"
        );
    }

    /// Ingest a real git repository. Python's pipeline tests did the same
    /// (shell out to git in a temp dir) rather than hand-fabricating rows —
    /// a hand-written fixture gets the `seq` ordering wrong and then "proves"
    /// a lineage rule that the real log order never exercises.
    fn ingest_fixture_repo(st: &mut SqliteStore, dir: &Path, script: &[&str]) {
        let g = crate::acquire::Git::new(Some(dir.join(".git")));
        let commits: Vec<CommitRecord> =
            crate::ingest::stream::stream_commits(&g, &["--no-merges", "HEAD"], &mut Vec::new())
                .unwrap()
                .collect();
        let batch: Vec<(i64, CommitRecord)> = commits
            .into_iter()
            .enumerate()
            .map(|(i, c)| (i as i64 + 1, c))
            .collect();
        st.insert_commit_batch(&batch, None).unwrap();
        let _ = script;
        let _ = &g;
    }

    #[test]
    fn rename_chain_resolves_to_the_final_name() -> Result<()> {
        // a.kt -> b.kt -> c.kt. The regression this traversal direction exists
        // for: walking oldest-first parks `alias` on the intermediate name, so
        // `a.kt` keeps its own history instead of inheriting `c.kt` — silently.
        let dir = std::env::temp_dir().join(format!("ra-lineage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            assert!(std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success());
        };
        run(&["init", "-q", "-b", "main", "."]);
        run(&["config", "user.name", "T"]);
        run(&["config", "user.email", "t@t"]);
        std::fs::write(dir.join("a.kt"), "fn main() {}\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "1"]);
        run(&["mv", "a.kt", "b.kt"]);
        run(&["commit", "-q", "-m", "2"]);
        run(&["mv", "b.kt", "c.kt"]);
        run(&["commit", "-q", "-m", "3"]);

        let mut st = SqliteStore::open(":memory:")?;
        ingest_fixture_repo(&mut st, &dir, &[]);
        assert_eq!(st.commit_count()?, 3);
        st.rebuild_lineage()?;
        let mut s = st
            .conn
            .prepare("SELECT path, path_current FROM file_changes ORDER BY path")?;
        let got: Vec<(String, Option<String>)> = s
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        assert_eq!(
            got,
            vec![
                ("a.kt".to_string(), Some("c.kt".to_string())),
                ("b.kt".to_string(), Some("c.kt".to_string())),
                ("c.kt".to_string(), Some("c.kt".to_string())),
            ],
            "every leg of the chain must land on the final name"
        );
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn a_deleted_file_has_no_current_path() {
        let mut st = store();
        st.insert_commit_batch(
            &[(
                1i64,
                rec(
                    "c1",
                    vec![FileChange {
                        status: "D".into(),
                        path: "gone.txt".into(),
                        old_path: None,
                        added: 0,
                        deleted: 5,
                        is_binary: false,
                    }],
                ),
            )],
            None,
        )
        .unwrap();
        st.rebuild_lineage().unwrap();
        let cur: Option<String> = st
            .conn
            .query_row("SELECT path_current FROM file_changes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cur, None);
    }

    #[test]
    fn meta_round_trips_and_overwrites() {
        let st = store();
        assert_eq!(st.get_meta("k").unwrap(), None);
        st.set_meta(&[("k", "v1".into()), ("other", "9".into())])
            .unwrap();
        assert_eq!(st.get_meta("k").unwrap().as_deref(), Some("v1"));
        st.set_meta(&[("k", "v2".into())]).unwrap();
        assert_eq!(st.get_meta("k").unwrap().as_deref(), Some("v2"));
        assert_eq!(st.get_meta("other").unwrap().as_deref(), Some("9"));
    }

    #[test]
    fn reset_clears_data_and_meta() {
        let mut st = store();
        st.insert_commit_batch(&[(1i64, rec("aaa", vec![]))], None)
            .unwrap();
        st.set_meta(&[("fp", "abc".into())]).unwrap();
        st.reset().unwrap();
        assert_eq!(st.commit_count().unwrap(), 0);
        assert_eq!(st.get_meta("fp").unwrap(), None);
    }

    #[test]
    fn a_version_mismatch_rebuilds_rather_than_migrates() {
        let dir = std::env::temp_dir().join(format!("ra-ver-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("x.audit.db");
        let path = p.to_string_lossy().into_owned();
        {
            let st = SqliteStore::open(&path).unwrap();
            let mut st = st;
            st.insert_commit_batch(&[(1i64, rec("aaa", vec![]))], None)
                .unwrap();
        }
        // Stamp a foreign version.
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch("PRAGMA user_version=999").unwrap();
        }
        let st = SqliteStore::open(&path).unwrap();
        assert_eq!(
            st.commit_count().unwrap(),
            0,
            "a stale cache must be discarded"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn identity_rows_group_by_name_and_email() {
        let mut st = store();
        let mut r1 = rec("a", vec![]);
        r1.author_name = "Alice".into();
        r1.author_email = "a@x.org".into();
        r1.author_ts = 5;
        let mut r2 = rec("b", vec![]);
        r2.author_name = "Alice".into();
        r2.author_email = "a@x.org".into();
        r2.author_ts = 50;
        let mut r3 = rec("c", vec![]);
        r3.author_name = "Bob".into();
        r3.author_email = "b@x.org".into();
        st.insert_commit_batch(&[(1, r1), (2, r2), (3, r3)], None)
            .unwrap();
        let rows = st.identity_rows().unwrap();
        assert_eq!(rows.len(), 2);
        let alice = rows.iter().find(|r| r.0 == "Alice").unwrap();
        assert_eq!((alice.2, alice.3, alice.4), (2, 5, 50));
    }
}
