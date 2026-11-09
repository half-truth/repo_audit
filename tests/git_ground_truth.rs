//! Gate layer 2: check the numbers against **git itself**, not against a golden
//! file.
//!
//! Layer 1 (the golden baseline gate in AGENTS.md) compares the tool's output
//! byte-for-byte with a baseline of its own past output. That catches regressions,
//! but it cannot catch a metric definition that was wrong from the first commit —
//! the baseline would simply agree with the bug. The only independent authority
//! left is `git`: it cannot be edited to match us, and every metric can be
//! expressed as a plain `git log` invocation. This file is that comparison,
//! made executable.
//!
//! ## What is asserted with `==`
//!
//! Everything the tool must reproduce bit for bit, and does:
//!
//! | property | oracle |
//! |---|---|
//! | every `commits` row: sha, is_merge, author_ts, commit_ts, subject, body | `git log --format=%H %P %at %ct %ad %s %b` |
//! | the monthly commit histogram | `git log --no-merges --date=format-local:%Y-%m --format=%cd` |
//! | every `file_changes` row: sha, path, added, deleted, is_binary | `git log --no-merges -M -z --numstat` |
//! | total insertions / deletions | the same numstat, summed |
//! | every `files_at_head` path | `git ls-tree -r`, minus submodule gitlinks |
//! | the fix-commit **set** and the firefight-commit **set** | git's messages, through the tool's own UDFs |
//!
//! ## What cannot be asserted with `==`
//!
//! **Per-file churn.** Rename folding moves lines onto the file's *latest* name,
//! and path reuse (an `x.txt` deleted, an unrelated `x.txt` created later) splits
//! one git path across two DB paths. Both are deliberate. Measured on the six
//! targets, restricting to files that were never renamed and are neither
//! `SKIP_FILE` nor binary still leaves 29/20/3/3/0/20 rows differing, in both
//! directions — so only the *total* is invariant. Asserting per-file equality
//! would be asserting a bug.
//!
//! **Regex selection against a naive grep.** A plain `grep='fix'` sketch would
//! also match `prefix`; the tool uses `\bfix(e[sd])?\b` instead. Counting
//! across that gap measures the regex, not the pipeline. So the fix and fire
//! tests load git's messages into a temp table in the tool's own connection and
//! apply the tool's own `rx`/`isrevert`: the *selection* is held fixed while the
//! *ingest* is what gets tested. What those regexes should be is a settled
//! policy question, not this gate's to re-judge.
//!
//! ## Two metric-definition traps this file has to respect
//!
//! - **`BODY_LIMIT = 2000`.** Commit bodies are truncated at ingest
//!   (`src/ingest/record.rs`). Any git-side reproduction of a regex over
//!   `subject || char(10) || body` must truncate identically or the commit sets
//!   differ — 5 of the 6 targets have bodies over 2000 chars, up to 100 000.
//!   The constant is **hard-coded** below rather than imported, so that bumping
//!   it in the source fails this gate instead of silently redefining it.
//! - **`git -E` has no `\b`.** POSIX ERE has no word boundaries, so
//!   `git log -E --grep='\bfix\b'` silently matches nothing. Use `-P`, or — as
//!   here — let SQLite's registered Rust regex do the matching.
//!
//! Cost: one full index per target (0.4–2.2 s, ~7.5 s total) plus three `git
//! log` passes per target, all memoized, so the file runs in well under a
//! minute. The cache is wiped at the start of every test-binary run — a stale
//! cache would make this gate agree with itself, which is the exact failure it
//! exists to prevent.

use repo_audit::analyzers::{bus_rows, human_non_merge_count, non_merge_count};
use repo_audit::index::{index_repo, IndexOptions};
use repo_audit::normalize::patterns::{FIRE_RE, FIX_RE};
use repo_audit::store::SqliteStore;
use repo_audit::util::text::py_strip;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};

/// The regression targets, in the order §5.1 lists them. Skipped wholesale if
/// they are not checked out — the gate must stay green on a machine that only
/// has the crate.
const TARGETS: &[(&str, &str)] = &[
    ("proxyClient/amnezia-client", "pc-amnezia-client"),
    ("proxyClient/clash-verge-rev", "pc-clash-verge-rev"),
    ("proxyClient/v2rayN", "pc-v2rayN"),
    ("proxyServer/3x-ui", "ps-3x-ui"),
    ("proxyServer/Xray-core", "ps-Xray-core"),
    ("proxyServer/sing-box", "ps-sing-box"),
];

/// See `src/ingest/record.rs::BODY_LIMIT`. Hard-coded deliberately — see the
/// module docs. Checked against all six targets; every one except Xray-core
/// has commits whose body exceeds it.
const BODY_LIMIT: usize = 2000;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn present_targets() -> Vec<(PathBuf, &'static str)> {
    let root = repo_root();
    TARGETS
        .iter()
        .filter_map(|(rel, name)| {
            let p = root.join(rel);
            // A local-path target is either a work tree or a bare clone.
            let present = p.join(".git").exists() || p.join("HEAD").exists();
            present.then_some((p, *name))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// git
// ---------------------------------------------------------------------------

/// Every `git` invocation in this file, memoized by (repo, argv).
///
/// Bytes, not `String`: `capture_output(text=True)` translates `\r\n`, which
/// silently rewrote commit bodies in an earlier draft of this file and produced
/// a `body_diff` of 2 per target that had nothing to do with the tool.
fn git_raw(dir: &Path, args: &[&str]) -> Vec<u8> {
    type Memo = OnceLock<Mutex<HashMap<(PathBuf, String), Vec<u8>>>>;
    static CACHE: Memo = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (dir.to_path_buf(), args.join(" "));
    if let Some(hit) = cache.lock().unwrap().get(&key) {
        return hit.clone();
    }

    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        // The §4 untrusted-repository guards. These targets are real clones with
        // their own config, and a stray `core.fsmonitor` or alias would let the
        // oracle itself lie.
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "protocol.allow=never",
            "-c",
            "protocol.https.allow=always",
            "-c",
            "protocol.ssh.allow=always",
            "--no-pager",
        ])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_PAGER", "cat")
        // SQLite's `strftime(..., 'unixepoch')` is UTC, so `--date=format-local`
        // has to be too. Without this the month buckets shift wherever the
        // machine is not on UTC.
        .env("TZ", "UTC")
        .output()
        .unwrap_or_else(|e| panic!("git {args:?} in {}: {e}", dir.display()));
    assert!(
        out.status.success(),
        "git {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );

    cache.lock().unwrap().insert(key, out.stdout.clone());
    out.stdout
}

fn git_str(dir: &Path, args: &[&str]) -> String {
    String::from_utf8_lossy(&git_raw(dir, args)).into_owned()
}

/// `py_strip` + the ingest truncation — the transformation the tool applies to
/// git's `%b` before storing it.
fn as_stored_body(raw: &[u8]) -> String {
    py_strip(&String::from_utf8_lossy(raw))
        .chars()
        .take(BODY_LIMIT)
        .collect()
}

fn strip_newlines(b: &[u8]) -> &[u8] {
    let mut i = 0;
    while i < b.len() && b[i] == b'\n' {
        i += 1;
    }
    &b[i..]
}

fn to_i64(b: &[u8]) -> i64 {
    to_opt_i64(b).unwrap_or_else(|| panic!("not an integer: {:?}", String::from_utf8_lossy(b)))
}

fn to_opt_i64(b: &[u8]) -> Option<i64> {
    String::from_utf8_lossy(b).trim().parse().ok()
}

// ---------------------------------------------------------------------------
// the shared index
// ---------------------------------------------------------------------------

/// Wiped on the first call of every test-binary run, so each `cargo test` run
/// re-ingests from git instead of inheriting a warm cache.
fn cache_root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let p = repo_root().join("target").join("git-gate");
        let _ = std::fs::remove_dir_all(&p);
        p
    })
}

/// `index_repo` takes a non-blocking `flock`, so two tests indexing the same
/// target at once would race; one at a time, and the losers get a no-op.
fn index_lock() -> MutexGuard<'static, ()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn indexed(name: &str, spec: &Path) -> SqliteStore {
    let opts = IndexOptions {
        cache_dir: Some(cache_root().join(name)),
        // A developer's own ~/.mailmap must not leak into the oracle.
        mailmap_file: Some("/dev/null".into()),
        quiet: true,
        ..Default::default()
    };
    let _g = index_lock();
    let res = index_repo(&spec.to_string_lossy(), &opts)
        .unwrap_or_else(|e| panic!("{name}: index failed: {}", e.message));
    res.store
}

/// Run `f` for every checked-out target, skipping cleanly when none are present.
fn each_target(mut f: impl FnMut(&'static str, &Path, &SqliteStore, &GitTruth)) {
    let targets = present_targets();
    if targets.is_empty() {
        eprintln!("skip: no regression targets checked out");
        return;
    }
    for (path, name) in &targets {
        let store = indexed(name, path);
        let truth = GitTruth::new(path);
        f(name, path, &store, &truth);
    }
}

// ---------------------------------------------------------------------------
// git's own view of a target
// ---------------------------------------------------------------------------

struct Commit {
    is_merge: bool,
    author_ts: i64,
    commit_ts: i64,
    subject: String,
    body: String,
}

#[derive(Default)]
struct GitTruth {
    commits: HashMap<String, Commit>,
    /// `(sha, path) -> (added, deleted)`, `None` for a binary blob.
    changes: HashMap<(String, String), Option<(i64, i64)>>,
    head_blobs: HashSet<String>,
    gitlinks: usize,
    ls_tree_entries: usize,
    /// Bucketed by git itself, so the histogram assertion cannot be satisfied by
    /// a mistake in our own month arithmetic.
    months: BTreeMap<String, i64>,
}

impl GitTruth {
    fn new(dir: &Path) -> Self {
        let mut t = GitTruth::default();

        // Pass 1: the commit rows and the month histogram. `%cd` is the
        // committer date, which is what `audit_trend` buckets on.
        let raw = git_raw(
            dir,
            &[
                "log",
                // `-z` NUL-separates the records. Without it git uses newlines,
                // which `%b` fills — a commit message would run into the next one.
                "-z",
                "--date=format-local:%Y-%m",
                "--format=%x01%H%x02%P%x02%at%x02%ct%x02%cd%x02%s%x02%b",
                "HEAD",
            ],
        );
        for rec in raw.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            assert_eq!(
                rec[0],
                0x01,
                "{}: record does not start with \\x01",
                dir.display()
            );
            let f: Vec<&[u8]> = rec[1..].split(|b| *b == 0x02).collect();
            assert_eq!(
                f.len(),
                7,
                "{}: commit record has {} fields instead of 7; the message may contain \\x02",
                dir.display(),
                f.len()
            );
            let sha = String::from_utf8_lossy(f[0]).into_owned();
            let is_merge = String::from_utf8_lossy(f[1]).split_whitespace().count() > 1;
            let month = String::from_utf8_lossy(f[4]).into_owned();
            if !is_merge {
                *t.months.entry(month.clone()).or_default() += 1;
            }
            t.commits.insert(
                sha,
                Commit {
                    is_merge,
                    author_ts: to_i64(f[2]),
                    commit_ts: to_i64(f[3]),
                    subject: String::from_utf8_lossy(f[5]).into_owned(),
                    body: as_stored_body(f[6]),
                },
            );
        }

        // Pass 2: the per-commit file changes. `-M` is the same rename detection
        // the tool asks for, and `-z` is what makes the token layout positionally
        // parseable — the whole point of §7.5.
        let raw = git_raw(
            dir,
            &[
                "log",
                "--no-merges",
                "-M",
                "-z",
                "--numstat",
                "--format=%x1e%H",
                "HEAD",
            ],
        );
        let toks: Vec<&[u8]> = raw.split(|b| *b == 0).collect();
        let mut sha = String::new();
        let mut i = 0;
        while i < toks.len() {
            // The first numstat token after a header carries a leading \n.
            let tok = strip_newlines(toks[i]);
            i += 1;
            if tok.is_empty() {
                continue;
            }
            if tok[0] == 0x1e {
                sha = String::from_utf8_lossy(&tok[1..]).trim().to_owned();
                continue;
            }
            let parts: Vec<&[u8]> = tok.split(|b| *b == b'\t').collect();
            let (added, deleted, path) = match parts.as_slice() {
                [a, d, p] => (*a, *d, *p),
                _ => continue,
            };
            let path = if path.is_empty() {
                // A rename prints `added\tdeleted\t` followed by two NUL-separated
                // paths. The new name is the one the tool keys on.
                let new = toks.get(i + 1).copied().unwrap_or_default();
                i += 2;
                new
            } else {
                path
            };
            // `-` in either column marks a binary blob: stored as 0/0 + is_binary.
            let val = match (to_opt_i64(added), to_opt_i64(deleted)) {
                (Some(a), Some(d)) => Some((a, d)),
                _ => None,
            };
            t.changes.insert(
                (sha.clone(), String::from_utf8_lossy(path).into_owned()),
                val,
            );
        }

        // Pass 3: the HEAD snapshot. `-r` lists submodule gitlinks as entries;
        // only blobs can be in `files_at_head`, so they come off first.
        for line in git_str(
            dir,
            &["ls-tree", "-r", "--format=%(objectmode) %(path)", "HEAD"],
        )
        .lines()
        {
            t.ls_tree_entries += 1;
            let (mode, path) = line.split_once(' ').unwrap_or((line, ""));
            if mode.trim() == "160000" {
                t.gitlinks += 1;
            } else {
                t.head_blobs.insert(path.to_string());
            }
        }

        t
    }

    /// Binary blobs contribute no lines, so they are skipped rather than counted
    /// as 0 — a `-` row and a genuine `0 0` row mean different things and the
    /// binary flag has to come from the same source either way.
    fn insertions(&self) -> i64 {
        self.changes.values().flatten().map(|v| v.0).sum()
    }

    fn deletions(&self) -> i64 {
        self.changes.values().flatten().map(|v| v.1).sum()
    }
}

/// Select commits out of **git's** messages with the **tool's own** UDFs.
///
/// Loading git's text into a temp table on the tool's connection is the point:
/// `rx` / `isrevert` are the same Rust closures `audit_bugs` / `audit_fire` run,
/// so this compares whether the *stored* history selects the same commits as
/// git's history does — not whether the regexes are well chosen.
fn shas_selected_by(store: &SqliteStore, truth: &GitTruth, predicate: &str) -> Vec<String> {
    store
        .conn
        .execute_batch("DROP TABLE IF EXISTS temp.gate_git")
        .unwrap();
    store
        .conn
        .execute_batch("CREATE TEMP TABLE gate_git(sha TEXT PRIMARY KEY, subject TEXT, body TEXT)")
        .unwrap();
    {
        let mut st = store
            .conn
            .prepare("INSERT INTO temp.gate_git VALUES (?1, ?2, ?3)")
            .unwrap();
        // Merge commits are never selected by either predicate, so they are not
        // loaded at all.
        for (sha, c) in &truth.commits {
            if c.is_merge {
                continue;
            }
            st.execute([sha.as_str(), c.subject.as_str(), c.body.as_str()])
                .unwrap();
        }
    }
    let sql = format!("SELECT sha FROM temp.gate_git WHERE ({predicate}) ORDER BY sha");
    let mut st = store.conn.prepare(&sql).unwrap();
    let rows = st.query_map([], |r| r.get::<_, String>(0)).unwrap();
    rows.map(|r| r.unwrap()).collect()
}

fn db_shas(store: &SqliteStore, predicate: &str) -> Vec<String> {
    let sql = format!("SELECT sha FROM commits WHERE is_merge=0 AND ({predicate}) ORDER BY sha");
    let mut st = store.conn.prepare(&sql).unwrap();
    let rows = st.query_map([], |r| r.get::<_, String>(0)).unwrap();
    rows.map(|r| r.unwrap()).collect()
}

/// Exact SHA-set equality, with a failure message that names the commits that
/// drifted instead of only reporting a count.
#[track_caller]
fn assert_same_set(name: &str, what: &str, want: &[String], got: &[String]) {
    let want_set: HashSet<&String> = want.iter().collect();
    let got_set: HashSet<&String> = got.iter().collect();
    let only_git: Vec<&String> = want.iter().filter(|s| !got_set.contains(s)).collect();
    let only_db: Vec<&String> = got.iter().filter(|s| !want_set.contains(s)).collect();
    assert!(
        only_git.is_empty() && only_db.is_empty(),
        "{name}: {what} in DB: {got_n} / in git: {want_n}\
         — only in git: {only_git:?}; only in DB: {only_db:?}",
        got_n = got.len(),
        want_n = want.len(),
    );
}

// ---------------------------------------------------------------------------
// the assertions
// ---------------------------------------------------------------------------

/// The denominator of nearly every metric, checked first and exactly.
#[test]
fn non_merge_commit_count_matches_git() {
    each_target(|name, _, store, truth| {
        let want = truth.commits.values().filter(|c| !c.is_merge).count() as i64;
        assert_eq!(
            non_merge_count(store),
            want,
            "{name}: non-merge commit count differs from git"
        );
    });
}

/// Every column of every commit, byte for byte. This is the assertion that makes
/// the rest of the file meaningful: if the commit table is right, the analyzers
/// are reading real history rather than a lossy copy of it.
#[test]
fn commits_table_matches_git_field_for_field() {
    each_target(|name, _, store, truth| {
        let mut st = store
            .conn
            .prepare("SELECT sha, is_merge, author_ts, commit_ts, subject, body FROM commits")
            .unwrap();
        let rows = st
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                ))
            })
            .unwrap();

        let mut mismatches: Vec<String> = Vec::new();
        let mut seen = 0usize;
        for r in rows {
            let (sha, is_merge, ats, cts, subject, body) = r.unwrap();
            seen += 1;
            let Some(w) = truth.commits.get(&sha) else {
                mismatches.push(format!("{sha} only in DB"));
                continue;
            };
            if is_merge != i64::from(w.is_merge) {
                mismatches.push(format!("{sha} is_merge {is_merge} != {}", w.is_merge));
            }
            if ats != Some(w.author_ts) {
                mismatches.push(format!("{sha} author_ts {ats:?} != {}", w.author_ts));
            }
            if cts != Some(w.commit_ts) {
                mismatches.push(format!("{sha} commit_ts {cts:?} != {}", w.commit_ts));
            }
            if subject.as_deref() != Some(w.subject.as_str()) {
                mismatches.push(format!("{sha} subject differs"));
            }
            if body.as_deref() != Some(w.body.as_str()) {
                mismatches.push(format!("{sha} body differs"));
            }
        }

        assert_eq!(
            seen,
            truth.commits.len(),
            "{name}: commits row count differs from git commit count"
        );
        assert!(
            mismatches.is_empty(),
            "{name}: {} fields differ; first mismatches: {:?}",
            mismatches.len(),
            &mismatches[..mismatches.len().min(5)]
        );
    });
}

/// The monthly histogram, month by month, with no slack. Git formats the month,
/// SQLite formats it again from the stored timestamp: two different engines, so
/// a timezone or truncation error on either side shows up here.
#[test]
fn monthly_commit_counts_match_git_committer_date() {
    each_target(|name, _, store, truth| {
        let mut st = store
            .conn
            .prepare(
                "SELECT strftime('%Y-%m', commit_ts, 'unixepoch') m, COUNT(*)
                   FROM commits WHERE is_merge=0 GROUP BY m",
            )
            .unwrap();
        let rows = st
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .unwrap();
        let mut got: BTreeMap<String, i64> = BTreeMap::new();
        for r in rows {
            let (m, n) = r.unwrap();
            got.insert(m, n);
        }

        assert!(
            !truth.months.is_empty(),
            "{name}: git returned no monthly commits"
        );
        assert_eq!(
            got, truth.months,
            "{name}: monthly commit distribution differs from git log --format=%cd"
        );
    });
}

/// `audit_trend` counts in `months_ago`, not calendar months. Relabelling git's
/// histogram the same way must land on the same numbers, and the buckets must
/// be consecutive — a gap means the arithmetic dropped or duplicated a month,
/// which a calendar-month comparison cannot see.
#[test]
fn trend_months_ago_buckets_map_onto_git_calendar_months() {
    each_target(|name, _, store, truth| {
        let now: String = store
            .conn
            .query_row("SELECT strftime('%Y-%m','now')", [], |r| r.get(0))
            .unwrap();
        let (ny, nm) = split_month(&now);
        let ago = |m: &str| {
            let (y, mo) = split_month(m);
            (ny - y) * 12 + (nm - mo)
        };

        let mut want: BTreeMap<i64, i64> = BTreeMap::new();
        for (m, n) in &truth.months {
            *want.entry(ago(m)).or_default() += n;
        }

        let mut st = store
            .conn
            .prepare(
                "SELECT (CAST(strftime('%Y','now') AS INT) - CAST(strftime('%Y',commit_ts,'unixepoch') AS INT)) * 12
                        + (CAST(strftime('%m','now') AS INT) - CAST(strftime('%m',commit_ts,'unixepoch') AS INT)) AS m,
                        COUNT(*) FROM commits WHERE is_merge=0 GROUP BY m",
            )
            .unwrap();
        let rows = st
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
            .unwrap();
        let mut got: BTreeMap<i64, i64> = BTreeMap::new();
        for r in rows {
            let (m, n) = r.unwrap();
            got.insert(m, n);
        }

        assert_eq!(
            got, want,
            "{name}: months_ago buckets do not match git calendar months"
        );

        // The window the report plots is months_ago 0..=23 anchored on the current
        // month, which need not *start* at 0: a repository whose newest commit
        // lands in the previous month has nothing in month 0. `audit_trend` reads
        // an absent bucket as 0, so absence is not a defect — only a negative
        // bucket is, and that means a commit dated in the future.
        assert!(
            got.keys().all(|k| *k >= 0),
            "{name}: negative months_ago {:?}; a commit is dated in the future",
            got.keys().collect::<Vec<_>>()
        );
        let r12: i64 = (0..12).map(|i| got.get(&i).copied().unwrap_or(0)).sum();
        let p12: i64 = (12..24).map(|i| got.get(&i).copied().unwrap_or(0)).sum();
        let total: i64 = got.values().sum();
        assert!(
            r12 + p12 <= total,
            "{name}: last 12 months {r12} + previous 12 months {p12} exceeds total commits in window {total}"
        );
    });
}

fn split_month(m: &str) -> (i64, i64) {
    let (y, mo) = m
        .split_once('-')
        .unwrap_or_else(|| panic!("invalid month format: {m:?}"));
    (
        y.parse().expect("year is not an integer"),
        mo.parse().expect("month is not an integer"),
    )
}

/// Every per-commit file change, row for row: the key set, the line counts, and
/// the binary flag. This is what feeds churn, so it is asserted as an exact set
/// rather than through the analyzer that consumes it.
#[test]
fn file_changes_rows_match_git_numstat() {
    each_target(|name, _, store, truth| {
        let mut st = store
            .conn
            .prepare(
                "SELECT c.sha, f.path, f.added, f.deleted, f.is_binary
                   FROM file_changes f JOIN commits c ON c.sha = f.sha
                  WHERE c.is_merge = 0",
            )
            .unwrap();
        let rows = st
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            })
            .unwrap();

        let mut got: HashMap<(String, String), (i64, i64, i64)> = HashMap::new();
        let mut n_rows = 0usize;
        for r in rows {
            let (sha, path, added, deleted, is_binary) = r.unwrap();
            n_rows += 1;
            got.insert(
                (sha, path),
                (added.unwrap_or(0), deleted.unwrap_or(0), is_binary),
            );
        }

        let mut wrong: Vec<String> = Vec::new();
        for ((sha, path), val) in &truth.changes {
            let want = match val {
                Some((a, d)) => (*a, *d, 0),
                None => (0, 0, 1),
            };
            match got.remove(&(sha.clone(), path.clone())) {
                Some(v) if v == want => {}
                Some(v) => wrong.push(format!("{sha}:{path} DB {v:?} != git {want:?}")),
                None => wrong.push(format!("{sha}:{path} present in git, missing from DB")),
            }
        }
        for (sha, path) in got.keys() {
            wrong.push(format!("{sha}:{path} present in DB, missing from git"));
        }

        assert_eq!(
            n_rows,
            truth.changes.len(),
            "{name}: file_changes row count differs from git"
        );
        assert!(
            wrong.is_empty(),
            "{name}: {} rows differ; first mismatches: {:?}",
            wrong.len(),
            &wrong[..wrong.len().min(5)]
        );
    });
}

/// Per-file totals are *not* comparable to git — rename folding and path reuse
/// move lines between keys — but the grand total is, and it is what the report
/// prints. Asserting it here means the deliberately-different per-path view
/// cannot have silently broken the aggregate.
#[test]
fn insertion_and_deletion_totals_match_git() {
    each_target(|name, _, store, truth| {
        let (ins, del): (i64, i64) = store
            .conn
            .query_row(
                "SELECT COALESCE(SUM(f.added),0), COALESCE(SUM(f.deleted),0)
                   FROM file_changes f JOIN commits c ON c.sha = f.sha
                  WHERE c.is_merge = 0",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            ins,
            truth.insertions(),
            "{name}: total inserted lines differ from git"
        );
        assert_eq!(
            del,
            truth.deletions(),
            "{name}: total deleted lines differ from git"
        );
    });
}

/// The HEAD snapshot must be exactly `git ls-tree -r`, minus submodule
/// gitlinks: those have mode 160000 and no blob, so `files_at_head` cannot hold
/// them. This is the join §5.2 warns about — counting churn without it inflates
/// totals by tens of thousands of lines.
#[test]
fn head_file_list_matches_git_ls_tree() {
    each_target(|name, _, store, truth| {
        let mut st = store
            .conn
            .prepare("SELECT path FROM files_at_head")
            .unwrap();
        let rows = st.query_map([], |r| r.get::<_, String>(0)).unwrap();
        let got: HashSet<String> = rows.map(|r| r.unwrap()).collect();

        let only_git: Vec<&String> = truth.head_blobs.difference(&got).collect();
        let only_db: Vec<&String> = got.difference(&truth.head_blobs).collect();
        assert!(
            only_git.is_empty() && only_db.is_empty(),
            "{name}: only in git {only_git:?}; only in DB {only_db:?}"
        );
        assert_eq!(
            got.len() + truth.gitlinks,
            truth.ls_tree_entries,
            "{name}: files_at_head has {} entries + {} gitlinks != {} ls-tree entries",
            got.len(),
            truth.gitlinks,
            truth.ls_tree_entries
        );
    });
}

/// The fix-commit set that `audit_bugs` ranks on, compared against git's messages
/// run through the same predicate. Exact SHA-set equality.
#[test]
fn fix_commit_set_matches_git_messages() {
    each_target(|name, _, store, truth| {
        let predicate = format!("rx('{FIX_RE}', subject || char(10) || body)");
        let want = shas_selected_by(store, truth, &predicate);
        let got = db_shas(store, &predicate);
        assert_same_set(name, "fix commits", &want, &got);
    });
}

/// Same discipline for firefighting. Note the two clauses: `isrevert` first
/// recognises git's own `Revert "..."` subjects and `This reverts commit <sha>`
/// bodies — far more precise than keyword matching — and `FIRE_RE` is the
/// keyword fallback.
#[test]
fn firefight_commit_set_matches_git_messages() {
    each_target(|name, _, store, truth| {
        let predicate = format!(
            "(isrevert(subject, body) OR rx('{}', subject))",
            FIRE_RE.replace('\'', "''")
        );
        let want = shas_selected_by(store, truth, &predicate);
        let got = db_shas(store, &predicate);
        assert_same_set(name, "firefighting commits", &want, &got);
    });
}

/// Every non-merge commit belongs to exactly one non-bot author row, so the
/// per-author counts must sum to the non-bot total. Identity *merging* cannot
/// change that sum, which makes it the one bus-factor invariant that survives
/// both the bot filter and the name folding.
#[test]
fn bus_rows_sum_to_the_non_bot_commit_total() {
    each_target(|name, _, store, _| {
        let total = human_non_merge_count(store);
        let sum: i64 = bus_rows(store).iter().map(|r| r.n).sum();
        assert_eq!(
            sum, total,
            "{name}: bus factor row sum {sum} != non-bot non-merge commit count {total}\
             — a commit has no identity or was counted more than once"
        );
    });
}
