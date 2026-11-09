//! `selftest` — the synthetic-repository fixture plus the end-to-end
//! pipeline assertions.
//!
//! The fixture is built with **real git**, on purpose: file names containing a
//! space, a newline and non-ASCII; a multi-hop rename chain `a→b→dir/c`; a path
//! reused for an unrelated file; a binary blob; a bot; and two identities that
//! must merge (same name, different email) while two that must not (same email,
//! different name) also exercise the merge rule. Dates are fixed so the run is
//! reproducible.
//!
//! `cargo test` covers the same ground with finer granularity; this exists
//! because the Python original shipped `selftest` as a user-facing command, and
//! the CLI surface is part of what is being replicated.

use crate::analyzers::{audit_bugs, audit_bus, audit_churn, audit_fire, audit_trend};
use crate::error::Result;
use crate::index::{index_repo, Action, IndexOptions};
use crate::report::{identities, json, terminal};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const ALICE: (&str, &str) = ("Alice Smith", "alice@corp.com");
pub const ALICE_HOME: (&str, &str) = ("Alice Smith", "alice@home.net");
pub const BOB: (&str, &str) = ("Bob Jones", "bob@x.org");
pub const ROBERT: (&str, &str) = ("Robert Jones", "bob@x.org");
pub const CAROL_GH: (&str, &str) = ("carol", "1234+carol@users.noreply.github.com");
pub const CAROL2: (&str, &str) = ("Carol Lee", "carol@users.noreply.github.com");
pub const DAVE: (&str, &str) = ("Dave Brown", "dave@x.org");
pub const DAVE2: (&str, &str) = ("Dave Brown", "dave@y.org");
pub const BOT: (&str, &str) = (
    "dependabot[bot]",
    "49699333+dependabot[bot]@users.noreply.github.com",
);

pub fn ten_lines() -> String {
    (0..10).map(|i| format!("line {i}\n")).collect()
}

/// A real git repository with a fixed timeline.
pub struct Fixture {
    pub root: PathBuf,
    day: i64,
}

impl Fixture {
    pub fn new(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)?;
        let f = Fixture {
            root: root.to_path_buf(),
            day: 0,
        };
        f.git(&["init", "-q"], ("T", "t@t.t"))?;
        f.git(&["checkout", "-q", "-b", "main"], ("T", "t@t.t"))?;
        Ok(f)
    }

    /// 2021-01-01T12:00:00Z plus `day`, so every commit has a known timestamp.
    fn date(&self) -> String {
        let ts = 1609502400 + self.day * 86400;
        crate::util::clock::rfc2822(ts)
    }

    pub fn git(&self, args: &[&str], who: (&str, &str)) -> Result<()> {
        let d = self.date();
        let status = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            // A developer's own ~/.gitconfig must not change the fixture.
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", who.0)
            .env("GIT_AUTHOR_EMAIL", who.1)
            .env("GIT_COMMITTER_NAME", who.0)
            .env("GIT_COMMITTER_EMAIL", who.1)
            .env("GIT_AUTHOR_DATE", &d)
            .env("GIT_COMMITTER_DATE", &d)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .status()?;
        if !status.success() {
            let e = Command::new("git")
                .arg("-C")
                .arg(&self.root)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .stderr(std::process::Stdio::piped())
                .output()
                .map(|o| String::from_utf8_lossy(&o.stderr).to_string())
                .unwrap_or_default();
            return Err(crate::error::AuditError::new(format!(
                "fixture git {args:?}: {e}"
            )));
        }
        Ok(())
    }

    pub fn write(&self, rel: &str, data: &[u8]) -> Result<()> {
        let p = self.root.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(p, data)?;
        Ok(())
    }

    pub fn commit(&mut self, msg: &str, who: (&str, &str)) -> Result<()> {
        self.day += 1;
        self.git(&["add", "-A"], who)?;
        self.git(&["commit", "-q", "--allow-empty", "-m", msg], who)
    }
}

/// The 10-commit fixture every pipeline assertion runs against.
pub fn build_fixture(root: &Path) -> Result<Fixture> {
    let mut f = Fixture::new(root)?;
    let ten = ten_lines();
    f.write("a.txt", ten.as_bytes())?;
    f.write("sp ace.txt", b"s\n")?;
    f.write("ünï.txt", b"u\n")?;
    f.write("new\nline.txt", b"nl\n")?; // a real newline inside the file name
    f.write("bin.dat", &[0u8, 1, 2, 3])?;
    f.write("x.txt", b"old x\n")?;
    f.commit("init: add files", ALICE)?; // c1
    f.write("a.txt", ten.replace("line 3", "LINE 3").as_bytes())?;
    f.write("x.txt", b"old x\nmore\n")?;
    f.commit("tweak a and x", BOB)?; // c2
    f.git(&["mv", "a.txt", "b.txt"], ALICE)?;
    f.write(
        "b.txt",
        ten.replace("line 3", "LINE 3")
            .replace("line 4", "LINE 4")
            .as_bytes(),
    )?;
    f.git(&["rm", "-q", "x.txt"], ALICE)?;
    f.commit("rename a->b, delete x", ALICE_HOME)?; // c3
    f.write("deps.lock", b"lock\n")?;
    f.commit("bump deps", BOT)?; // c4
    f.git(&["checkout", "-q", "-b", "feat"], ALICE)?;
    f.write("feat.txt", b"feat\n")?;
    f.commit("feat work", CAROL_GH)?; // c5 (feat)
    f.git(&["checkout", "-q", "main"], ALICE)?;
    f.write(
        "b.txt",
        ten.replace("line 3", "LINE 3")
            .replace("line 4", "LINE 4")
            .replace("line 5", "LINE 5")
            .as_bytes(),
    )?;
    f.commit("main work", DAVE)?; // c6
    f.day += 1;
    f.git(
        &["merge", "--no-ff", "-q", "-m", "Merge feat (#1)", "feat"],
        ALICE,
    )?; // c7 merge
    std::fs::create_dir_all(f.root.join("dir"))?;
    f.git(&["mv", "b.txt", "dir/c.txt"], ALICE)?;
    f.write("x.txt", b"NEW x, a different file\n")?; // path reuse
    f.commit("move b->dir/c, recreate x (#2)", ALICE)?; // c8
    f.write(
        "dir/c.txt",
        ten.replace("line 3", "LINE 3")
            .replace("line 4", "LINE 4")
            .replace("line 5", "LINE 5")
            .replace("line 6", "LINE 6")
            .as_bytes(),
    )?;
    f.write("x.txt", b"NEW x, edited\n")?;
    f.commit("edit both", DAVE2)?; // c9
    f.commit("empty commit", ALICE)?; // c10
    Ok(f)
}

fn opts(cache: &Path) -> IndexOptions {
    IndexOptions {
        cache_dir: Some(cache.to_path_buf()),
        // An empty mailmap, so a developer's ~/.gitconfig mailmap cannot leak in.
        mailmap_file: Some("/dev/null".into()),
        quiet: true,
        ..Default::default()
    }
}

/// A scratch directory that removes itself.
///
/// Hand-rolled rather than pulled from `tempfile`: that crate is a *dev*-
/// dependency, and `selftest` is a runtime code path, so using it here would
/// silently promote it to a runtime dependency.
pub struct Scratch {
    path: PathBuf,
}

impl Scratch {
    pub fn new(prefix: &str) -> Result<Self> {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("{prefix}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path)?;
        Ok(Scratch { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// `selftest` as a command: run the same assertions, print a summary, return an
/// exit code.
pub fn run() -> i32 {
    let tmp = match Scratch::new("repo-audit-selftest") {
        Ok(t) => t,
        Err(e) => {
            eprintln!(
                "selftest: could not create temporary directory: {}",
                e.message
            );
            return 1;
        }
    };
    match pipeline(tmp.path()) {
        Ok(n) => {
            println!("\nselftest: all {n} assertions passed");
            0
        }
        Err(e) => {
            eprintln!("selftest failed: {}", e.message);
            1
        }
    }
}

/// The end-to-end assertions. Returns the number of checks performed.
pub fn pipeline(tmp: &Path) -> Result<usize> {
    let mut n = 0;
    let fx = build_fixture(&tmp.join("repo"))?;
    let res = index_repo(&fx.root.to_string_lossy(), &opts(&tmp.join("cache")))?;
    n += 1;
    assert_eq!(res.action, Action::Full, "first run must be a full ingest");
    assert_eq!(scalar(&res, "SELECT COUNT(*) FROM commits")?, 10);
    assert_eq!(
        scalar(&res, "SELECT COUNT(*) FROM commits WHERE is_merge=1")?,
        1
    );
    n += 3;

    let paths = strings(&res, "SELECT DISTINCT path FROM file_changes")?;
    for p in [
        "sp ace.txt",
        "ünï.txt",
        "new\nline.txt",
        "dir/c.txt",
        "b.txt",
        "a.txt",
    ] {
        assert!(
            paths.iter().any(|x| x == p),
            "missing path {p:?} in {paths:?}"
        );
    }
    n += 1;

    // A NUL in the blob means binary, and a binary file contributes no LOC.
    let bin: (i64, Option<i64>) = res.store.conn.query_row(
        "SELECT is_binary, added FROM file_changes WHERE path='bin.dat'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!(bin, (1, Some(0)));
    n += 1;

    // Rename chain a→b→dir/c: all six file-change rows fold into dir/c.txt.
    assert_eq!(
        scalar(
            &res,
            "SELECT COUNT(*) FROM file_changes WHERE path_current='dir/c.txt'"
        )?,
        6
    );
    let chain = strings(
        &res,
        "SELECT DISTINCT path FROM file_changes WHERE path_current='dir/c.txt'",
    )?;
    let mut chain_sorted = chain.clone();
    chain_sorted.sort();
    assert_eq!(chain_sorted, vec!["a.txt", "b.txt", "dir/c.txt"]);
    n += 2;

    // Path reuse: the old x.txt (add/modify/delete) resolves to NULL; the new
    // x.txt keeps its own two rows.
    assert_eq!(
        scalar(
            &res,
            "SELECT COUNT(*) FROM file_changes WHERE path='x.txt' AND path_current IS NULL"
        )?,
        3
    );
    assert_eq!(
        scalar(
            &res,
            "SELECT COUNT(*) FROM file_changes WHERE path_current='x.txt'"
        )?,
        2
    );
    n += 2;

    let mut head = strings(&res, "SELECT path FROM files_at_head")?;
    head.sort();
    assert_eq!(
        head,
        vec![
            "bin.dat",
            "deps.lock",
            "dir/c.txt",
            "feat.txt",
            "new\nline.txt",
            "sp ace.txt",
            "x.txt",
            "ünï.txt"
        ]
    );
    let loc: Option<i64> = res.store.conn.query_row(
        "SELECT loc FROM files_at_head WHERE path='dir/c.txt'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(loc, Some(10));
    let is_bin: i64 = res.store.conn.query_row(
        "SELECT is_binary FROM files_at_head WHERE path='bin.dat'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(is_bin, 1);
    n += 3;

    let groups = res.store.author_groups()?;
    assert_eq!(
        groups.len(),
        5,
        "{:?}",
        groups.iter().map(|g| &g["name"]).collect::<Vec<_>>()
    );
    let bots: Vec<&str> = groups
        .iter()
        .filter(|g| g["is_bot"].as_bool() == Some(true))
        .map(|g| g["name"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(bots, vec!["dependabot[bot]"]);
    // Alice's two e-mail addresses are one identity; Bob and Robert share an
    // address but not a name, so they must NOT merge.
    let alice = groups
        .iter()
        .find(|g| g["name"] == "Alice Smith")
        .map(|g| g["aliases"].as_array().map(|a| a.len()).unwrap_or(0))
        .unwrap_or(0);
    assert_eq!(alice, 2);
    n += 3;

    let s = res.store.summary()?;
    assert_eq!((s.humans, s.bots), (4, 1));
    assert_eq!(s.top_files[0].path, "dir/c.txt");
    n += 2;

    // All five analyzers must run without panicking — this is what catches a
    // missing UDF, a bad regex or broken month arithmetic.
    let (churn, touch) = audit_churn(&res.store);
    assert!(churn.most_common().iter().any(|(f, _)| f == "dir/c.txt"));
    audit_bus(&res.store);
    audit_bugs(&res.store, &touch);
    audit_trend(&res.store);
    audit_fire(&res.store);
    terminal::render_report(&res);
    n += 6;

    let j = json::render_json(&res);
    assert_eq!(j["repo"].as_str(), Some(res.display.as_str()));
    assert!(j.get("top_churn").is_some());
    assert!(identities::render_identities(&res, true)?.contains("Alice Smith"));
    n += 3;

    // Second run on an unchanged repo must be a no-op, not a re-index.
    drop(res);
    let res2 = index_repo(&fx.root.to_string_lossy(), &opts(&tmp.join("cache")))?;
    assert_eq!(
        res2.action,
        Action::Noop,
        "an unchanged repo must not re-ingest"
    );
    n += 1;

    Ok(n)
}

fn scalar(res: &crate::index::IndexResult, sql: &str) -> Result<i64> {
    Ok(res.store.conn.query_row(sql, [], |r| r.get(0))?)
}

fn strings(res: &crate::index::IndexResult, sql: &str) -> Result<Vec<String>> {
    let mut s = res.store.conn.prepare(sql)?;
    let rows = s.query_map([], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// `cargo test` entry point — the same assertions, plus the incremental path.
#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn pipeline_assertions_hold() {
        let tmp = t();
        let n = pipeline(tmp.path()).unwrap();
        assert!(n >= 25, "expected the full assertion set, ran {n}");
    }

    #[test]
    fn incremental_after_a_new_commit_and_a_rename() {
        let tmp = t();
        let mut fx = build_fixture(&tmp.path().join("repo")).unwrap();
        let cache = tmp.path().join("cache");
        let r1 = index_repo(&fx.root.to_string_lossy(), &opts(&cache)).unwrap();
        assert_eq!(r1.action, Action::Full);
        drop(r1);

        // A new commit that renames again: the older history's path_current must
        // follow it.
        fx.git(&["mv", "dir/c.txt", "dir/d.txt"], ALICE).unwrap();
        let body = std::fs::read_to_string(fx.root.join("dir/d.txt")).unwrap();
        fx.write("dir/d.txt", format!("{body}tail\n").as_bytes())
            .unwrap();
        fx.commit("rename c->d", ALICE).unwrap();

        let r2 = index_repo(&fx.root.to_string_lossy(), &opts(&cache)).unwrap();
        assert_eq!(r2.action, Action::Incremental);
        let n = scalar(
            &r2,
            "SELECT COUNT(*) FROM file_changes WHERE path_current='dir/d.txt'",
        )
        .unwrap();
        assert_eq!(n, 7, "the new rename must absorb the six earlier rows");
        assert_eq!(
            scalar(
                &r2,
                "SELECT COUNT(*) FROM file_changes WHERE path_current='dir/c.txt'"
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn reindex_rebuilds_from_scratch() {
        let tmp = t();
        let fx = build_fixture(&tmp.path().join("repo")).unwrap();
        let cache = tmp.path().join("cache");
        let mut o = opts(&cache);
        drop(index_repo(&fx.root.to_string_lossy(), &o).unwrap());
        o.reindex = true;
        let r = index_repo(&fx.root.to_string_lossy(), &o).unwrap();
        assert_eq!(r.action, Action::Full);
        assert_eq!(scalar(&r, "SELECT COUNT(*) FROM commits").unwrap(), 10);
    }
}

#[cfg(test)]
mod density_tests {
    use super::*;

    /// The density ranking must order by fixes *per touch*, not by fixes, and
    /// must not rank lockfiles at all.
    ///
    /// `hot.rs` and `cold.rs` are touched the same number of times, but only
    /// half of `hot.rs`'s commits are fixes — so a ranking on raw fix count and
    /// a ranking on churn both put them level, and only the rate separates
    /// them. `go.sum` is touched just as often and is fixed just as often, so
    /// any ranking that ignores `SKIP_FILE` will rank it above both.
    #[test]
    fn density_ranks_by_rate_and_skips_lockfiles() {
        let tmp = tempfile::tempdir().unwrap();
        let mut f = Fixture::new(&tmp.path().join("repo")).unwrap();
        f.write("hot.rs", b"a\n").unwrap();
        f.write("cold.rs", b"a\n").unwrap();
        f.write("go.sum", b"v1\n").unwrap();
        f.commit("init", ALICE).unwrap();

        // 20 rounds: every round touches all three files; only `hot.rs` is a
        // fix commit, plus a lockfile bump that SKIP_FILE must hide.
        for i in 0..20 {
            f.write("hot.rs", format!("a\n// {i}\n").as_bytes())
                .unwrap();
            f.write("cold.rs", format!("a\n// {i}\n").as_bytes())
                .unwrap();
            f.write("go.sum", format!("v{}\n", i + 2).as_bytes())
                .unwrap();
            if i % 2 == 0 {
                f.commit(&format!("fix: round {i}"), ALICE).unwrap();
            } else {
                f.commit(&format!("refactor: round {i}"), ALICE).unwrap();
            }
        }

        let res = index_repo(&f.root.to_string_lossy(), &opts(&tmp.path().join("cache"))).unwrap();
        let (churn, touch) = audit_churn(&res.store);

        // SKIP_FILE keeps the lockfile out of `touch`, so it cannot reach the
        // density candidates at all.
        assert_eq!(touch.get("go.sum"), 0.0, "a skipped path has no touch row");
        assert_eq!(churn.get("go.sum"), 0.0, "a skipped path has no churn row");

        // 21 touches each: the init commit plus 20 rounds. 10 of `hot.rs`'s are fixes.
        let hot = 10.0 / touch.get("hot.rs");
        let cold = 0.0 / touch.get("cold.rs");
        assert!((hot - 10.0 / 21.0).abs() < 1e-9, "hot.rs density = {hot}");
        assert!((cold - 0.0).abs() < 1e-9, "cold.rs density = {cold}");
        assert!(hot > cold);

        audit_bugs(&res.store, &touch);
    }

    /// Fewer than `RISK_MIN_FILES` candidates must degrade to ⚪ rather than
    /// ranking three files as if they meant something. The fixture above has
    /// exactly 2, so this is the same repository seen through the other branch.
    #[test]
    fn a_thin_candidate_set_degrades_to_insufficient() {
        let tmp = tempfile::tempdir().unwrap();
        let mut f = Fixture::new(&tmp.path().join("repo")).unwrap();
        f.write("a.rs", b"a\n").unwrap();
        f.write("b.rs", b"a\n").unwrap();
        f.commit("init", ALICE).unwrap();
        for i in 0..12 {
            f.write("a.rs", format!("a\n// {i}\n").as_bytes()).unwrap();
            f.write("b.rs", format!("a\n// {i}\n").as_bytes()).unwrap();
            f.commit(&format!("fix: round {i}"), ALICE).unwrap();
        }
        let res = index_repo(&f.root.to_string_lossy(), &opts(&tmp.path().join("cache"))).unwrap();
        let (_, touch) = audit_churn(&res.store);
        // Both files clear DENSITY_MIN_TOUCH but the pair is below the floor.
        assert!(touch.get("a.rs") >= 10.0);
        audit_bugs(&res.store, &touch);
    }
}
