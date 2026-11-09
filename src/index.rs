//! Orchestration: acquire → ingest → normalize → (analyze) → report.
//!
//! The full/incremental/resume/noop decision is driven entirely by two
//! fingerprints stored in `meta`. They bind the cache to the code that wrote
//! it: bump the matching version constant and the cache is invalidated
//! automatically instead of being silently reused as if it were current.

use crate::acquire::git::{resolve_local_git_dir, Git};
use crate::acquire::paths::{default_cache_dir, default_config_dir, local_db_name};
use crate::acquire::{clone_or_update, file_lock, parse_repo_spec, RepoRef};
use crate::error::{AuditError, Result};
use crate::ingest::head_files::snapshot_head_files;
use crate::ingest::record::CommitRecord;
use crate::ingest::stream::stream_commits;
use crate::normalize::identity::build_identity_groups;
use crate::normalize::patterns::{compile_bot_re, DEFAULT_BOT_RE};
use crate::store::fingerprint::{file_fp, fingerprint, FpPart};
use crate::store::SqliteStore;
use crate::util::progress::Progress;
use crate::util::text::safe;
use crate::{IDENTITY_VERSION, PARSER_VERSION, SCHEMA_VERSION};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct IndexOptions {
    pub git_ref: String,
    pub reindex: bool,
    pub name_merge: bool,
    pub bot_patterns: Vec<String>,
    pub mailmap_file: Option<String>,
    pub cache_dir: Option<PathBuf>,
    /// Mandatory for a fresh remote clone: the staging dir where
    /// `clone --bare` lands before the atomic rename into the cache.
    /// Checked for writability and same-volume-ness, never fallen back.
    pub tmp_dir: Option<PathBuf>,
    pub timeout: Duration,
    pub batch_size: usize,
    pub quiet: bool,
    /// Only for tests: permits `file://` clones. Never set from the CLI.
    pub allow_file: bool,
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            git_ref: "HEAD".into(),
            reindex: false,
            name_merge: true,
            bot_patterns: Vec::new(),
            mailmap_file: None,
            cache_dir: None,
            tmp_dir: None,
            timeout: Duration::from_secs(1800),
            batch_size: 2000,
            quiet: true,
            allow_file: false,
        }
    }
}

pub struct IndexResult {
    pub store: SqliteStore,
    pub action: Action,
    pub head: String,
    pub display: String,
    pub git_ref: String,
    pub shallow: bool,
    pub seconds: f64,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Full,
    Incremental,
    Resume,
    Noop,
}

impl Action {
    pub fn as_str(&self) -> &'static str {
        match self {
            Action::Full => "full",
            Action::Incremental => "incremental",
            Action::Resume => "resume",
            Action::Noop => "noop",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Action::Full => "full",
            Action::Incremental => "incremental",
            Action::Resume => "resume",
            Action::Noop => "no new commits",
        }
    }
}

/// Test-only fault injection. Python's `test_resume_after_interrupt`
/// monkey-patched `insert_commit_batch`; Rust has no equivalent, so the hook is
/// an explicit parameter rather than hidden behaviour.
pub type FaultHook<'a> = Option<&'a dyn Fn(usize) -> Result<()>>;

pub fn index_repo(spec: &str, opts: &IndexOptions) -> Result<IndexResult> {
    index_repo_with(spec, opts, None)
}

pub fn index_repo_with(
    spec: &str,
    opts: &IndexOptions,
    fault: FaultHook<'_>,
) -> Result<IndexResult> {
    let t0 = Instant::now();
    let repo = parse_repo_spec(spec)?;
    let cache = opts.cache_dir.clone().unwrap_or_else(default_cache_dir);

    let (git_dir, db_path) = match &repo {
        RepoRef::Remote(r) => {
            let d = cache.join(&r.host).join(&r.owner);
            (
                d.join(format!("{}.git", r.name)),
                d.join(format!("{}.audit.db", r.name)),
            )
        }
        RepoRef::Local(l) => (
            resolve_local_git_dir(&l.path)?,
            cache
                .join("local")
                .join(local_db_name(&l.path, &sanitize(&l.name))),
        ),
    };

    let mailmap = match &opts.mailmap_file {
        Some(m) => Some(m.clone()),
        None => {
            let d = default_config_dir().join("mailmap");
            d.is_file().then(|| d.to_string_lossy().into_owned())
        }
    };

    let _lock = file_lock(&db_path.with_file_name(format!(
        "{}.lock",
        db_path.file_name().unwrap_or_default().to_string_lossy()
    )))?;

    if let RepoRef::Remote(r) = &repo {
        say(
            opts,
            &format!("[1/4] Fetching repository {}", safe(&r.display)),
        );
        clone_or_update(
            &r.clone_url,
            &git_dir,
            opts.tmp_dir.as_deref(),
            opts.timeout,
            opts.allow_file,
            !opts.quiet,
        )?;
    } else if let RepoRef::Local(_) = &repo {
        // The path is deliberately not printed: progress lines get pasted into issues
        // and CI logs, and a local path leaks the user's directory layout.
        say(opts, "[1/4] Using local repository (read-only)");
    }

    let mut git = Git::new(Some(git_dir));
    git.allow_file = opts.allow_file;
    git.mailmap_file = mailmap.clone();

    let head = match git.try_run_quiet(&[
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("{}^{{commit}}", opts.git_ref),
    ])? {
        true => git.text(
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{}^{{commit}}", opts.git_ref),
            ],
            None,
        )?,
        false => String::new(),
    };
    if !(40..=64).contains(&head.len()) || !head.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(AuditError::with_hint(
            format!("Could not resolve ref: {}", safe(&opts.git_ref)),
            "The repository may be empty, or the branch may not exist",
        ));
    }
    let shallow = git
        .text(
            &["rev-parse", "--is-shallow-repository"],
            Some(Duration::from_secs(60)),
        )
        .map(|t| t == "true")
        .unwrap_or(false);
    let mut warnings: Vec<String> = Vec::new();
    if shallow {
        warnings.push(
            "Shallow clone: history is incomplete, so confidence in all time-series conclusions is reduced"
                .into(),
        );
    }

    let mut store = SqliteStore::open(&db_path.to_string_lossy())?;

    let ingest_fp = fingerprint(&[
        FpPart::Int(SCHEMA_VERSION),
        FpPart::Int(PARSER_VERSION),
        FpPart::Str(opts.git_ref.clone()),
        FpPart::Str(file_fp(mailmap.as_deref())),
    ]);
    let ident_fp = fingerprint(&[
        FpPart::Int(IDENTITY_VERSION),
        FpPart::Bool(opts.name_merge),
        FpPart::StrList(opts.bot_patterns.clone()),
        FpPart::Str(DEFAULT_BOT_RE.into()),
        FpPart::Str(ingest_fp.clone()),
    ]);

    let state = store.get_meta("state")?;
    let old_head = store.get_meta("indexed_head")?;
    let action = if opts.reindex
        || (store.get_meta("ingest_fp")?.as_deref() != Some(ingest_fp.as_str()) && state.is_some())
    {
        Action::Full
    } else if state.as_deref() == Some("complete") {
        if old_head.as_deref() == Some(head.as_str()) {
            Action::Noop
        } else if is_ancestor(&git, old_head.as_deref(), &head) {
            Action::Incremental
        } else {
            // History was rewritten (force-push / rebase): old seq numbers no
            // longer describe the same commits, so a partial append would
            // produce a corrupt ordering.
            Action::Full
        }
    } else if state.as_deref() == Some("partial")
        && store.get_meta("mode")?.as_deref() == Some("full")
        && store.get_meta("target_head")?.as_deref() == Some(head.as_str())
    {
        Action::Resume
    } else {
        Action::Full
    };

    if action == Action::Full && state.is_some() {
        store.reset()?;
    }
    say(
        opts,
        &format!("[2/4] Indexing commit history ({})", action.label()),
    );

    if action != Action::Noop {
        let (rev_args, count_arg, seq_base, skip) = if action == Action::Incremental {
            let range = format!("{}..{}", old_head.clone().unwrap_or_default(), head);
            let n_new: i64 = git
                .text(&["rev-list", "--count", &range], None)?
                .parse()
                .unwrap_or(0);
            (
                vec![range.clone()],
                range,
                (store.min_seq()?.unwrap_or(0)) - n_new,
                Default::default(),
            )
        } else {
            git.text(&["rev-list", "--count", &head], None)?;
            let skip = if action == Action::Resume {
                store.existing_shas()?
            } else {
                Default::default()
            };
            (vec![head.clone()], head.clone(), 0, skip)
        };
        let _ = count_arg;
        store.set_meta(&[
            ("state", "partial".into()),
            (
                "mode",
                if action == Action::Incremental {
                    "incremental".into()
                } else {
                    "full".to_string()
                },
            ),
            ("target_head", head.clone()),
            ("ingest_fp", ingest_fp.clone()),
            ("ref", opts.git_ref.clone()),
            ("schema_version", SCHEMA_VERSION.to_string()),
        ])?;
        let n_new: i64 = git
            .text(&["rev-list", "--count", &rev_args[0]], None)?
            .parse()
            .unwrap_or(0);

        let mut prog = Progress::new("commits", n_new.max(0) as usize, !opts.quiet);
        let mut batch: Vec<(i64, CommitRecord)> = Vec::with_capacity(opts.batch_size);
        let mut i: i64 = 0;
        let mut consumed = 0usize;
        let result = (|| -> Result<()> {
            let mut warnings_local = Vec::new();
            let revs: Vec<&str> = rev_args.iter().map(String::as_str).collect();
            let it = stream_commits(&git, &revs, &mut warnings_local)?;
            for rec in it {
                let seq = seq_base + i;
                i += 1;
                if !skip.contains(&rec.sha) {
                    batch.push((seq, rec));
                    if batch.len() >= opts.batch_size {
                        store.insert_commit_batch(&batch, fault)?;
                        consumed += batch.len();
                        batch.clear();
                    }
                }
                prog.update(i.max(0) as usize);
            }
            if !batch.is_empty() {
                store.insert_commit_batch(&batch, fault)?;
            }
            warnings.extend(warnings_local);
            Ok(())
        })();
        prog.done();
        result?;
        if i != n_new {
            warnings.push(format!(
                "Commit count differs from rev-list ({i} vs {n_new}); graft/replace refs may be present"
            ));
        }
        say(opts, "[3/4] Merging rename history and author identities");
        store.rebuild_lineage()?;
    } else {
        say(opts, "[3/4] Data is up to date");
    }

    if action != Action::Noop || store.get_meta("ident_fp")?.as_deref() != Some(ident_fp.as_str()) {
        let rows = store.identity_rows()?;
        let bot_re = compile_bot_re(&opts.bot_patterns)?;
        let groups = build_identity_groups(&rows, opts.name_merge, Some(&bot_re));
        store.write_authors(&groups)?;
        store.set_meta(&[("ident_fp", ident_fp)])?;
    }
    if store.get_meta("head_files_for")?.as_deref() != Some(head.as_str()) {
        say(opts, "[4/4] Snapshotting HEAD file list");
        let files = snapshot_head_files(&git, &head)?;
        store.replace_head_files(&files)?;
        store.set_meta(&[("head_files_for", head.clone())])?;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    store.set_meta(&[
        ("state", "complete".into()),
        ("indexed_head", head.clone()),
        ("indexed_at", now.to_string()),
        ("repo", repo_display(&repo)),
        ("shallow", if shallow { "1".into() } else { "0".into() }),
    ])?;

    Ok(IndexResult {
        store,
        action,
        head,
        display: repo_display(&repo),
        git_ref: opts.git_ref.clone(),
        shallow,
        seconds: t0.elapsed().as_secs_f64(),
        warnings,
    })
}

fn repo_display(r: &RepoRef) -> String {
    match r {
        RepoRef::Remote(x) => x.display.clone(),
        RepoRef::Local(x) => x.path.clone(),
    }
}

fn say(opts: &IndexOptions, msg: &str) {
    if !opts.quiet {
        eprintln!("{msg}");
    }
}

/// Python's `re.sub(r"[^A-Za-z0-9._-]", "_", name)` for the local cache filename.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn is_ancestor(git: &Git, old: Option<&str>, new: &str) -> bool {
    let Some(old) = old.filter(|s| !s.is_empty()) else {
        return false;
    };
    git.try_run_quiet(&["merge-base", "--is-ancestor", old, new])
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn sh(dir: &Path, args: &[&str]) {
        assert!(std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success());
    }

    /// A repo with the awkward cases that matter: a rename, a path with a space,
    /// and a non-ASCII name.
    fn fixture(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ra-idx-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        sh(&d, &["init", "-q", "-b", "main", "."]);
        sh(&d, &["config", "user.name", "Alice Smith"]);
        // A real domain on purpose: `example.org` is on the generic-email list, so
        // an author using it is correctly judged unattributable.
        sh(&d, &["config", "user.email", "alice@realdomain.tld"]);
        std::fs::write(d.join("a.kt"), "fn a() {}\n").unwrap();
        std::fs::write(d.join("name with space.txt"), "x\n").unwrap();
        sh(&d, &["add", "-A"]);
        sh(&d, &["commit", "-q", "-m", "initial"]);
        sh(&d, &["mv", "a.kt", "b.kt"]);
        sh(&d, &["commit", "-q", "-m", "rename"]);
        sh(&d, &["config", "user.name", "dependabot[bot]"]);
        sh(
            &d,
            &[
                "config",
                "user.email",
                "49699333+dependabot[bot]@users.noreply.github.com",
            ],
        );
        std::fs::write(d.join("c.txt"), "y\n").unwrap();
        sh(&d, &["add", "-A"]);
        sh(&d, &["commit", "-q", "-m", "bot commit"]);
        d
    }

    fn opts_for() -> IndexOptions {
        IndexOptions {
            quiet: true,
            allow_file: true,
            ..Default::default()
        }
    }

    #[test]
    fn indexes_then_reports_noop_on_a_second_run() {
        let d = fixture("noop");
        let mut o = opts_for();
        o.cache_dir =
            Some(std::env::temp_dir().join(format!("ra-cache-noop-{}", std::process::id())));
        let _ = std::fs::remove_dir_all(o.cache_dir.clone().unwrap());
        let spec = d.to_string_lossy().into_owned();

        let r1 = index_repo(&spec, &o).unwrap();
        assert_eq!(r1.action, Action::Full);
        assert_eq!(r1.store.commit_count().unwrap(), 3);
        assert!(r1.head.len() >= 40);

        // Fingerprint unchanged and HEAD unmoved -> nothing to do.
        let r2 = index_repo(&spec, &o).unwrap();
        assert_eq!(
            r2.action,
            Action::Noop,
            "second run must be a noop, not a rescan"
        );
        assert_eq!(r2.store.commit_count().unwrap(), 3);

        let _ = std::fs::remove_dir_all(o.cache_dir.clone().unwrap());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_new_commit_takes_the_incremental_path() {
        let d = fixture("incr");
        let mut o = opts_for();
        o.cache_dir =
            Some(std::env::temp_dir().join(format!("ra-cache-incr-{}", std::process::id())));
        let _ = std::fs::remove_dir_all(o.cache_dir.clone().unwrap());
        let spec = d.to_string_lossy().into_owned();
        index_repo(&spec, &o).unwrap();

        std::fs::write(d.join("d.txt"), "z\n").unwrap();
        sh(&d, &["add", "-A"]);
        sh(&d, &["commit", "-q", "-m", "fourth"]);

        let r = index_repo(&spec, &o).unwrap();
        assert_eq!(
            r.action,
            Action::Incremental,
            "a descendant head is incremental"
        );
        assert_eq!(r.store.commit_count().unwrap(), 4);

        let _ = std::fs::remove_dir_all(o.cache_dir.clone().unwrap());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_rewritten_history_forces_a_full_reindex() {
        let d = fixture("rewrite");
        let mut o = opts_for();
        o.cache_dir =
            Some(std::env::temp_dir().join(format!("ra-cache-rw-{}", std::process::id())));
        let _ = std::fs::remove_dir_all(o.cache_dir.clone().unwrap());
        let spec = d.to_string_lossy().into_owned();
        index_repo(&spec, &o).unwrap();

        // Reset onto a single root commit: the old commits are no longer
        // ancestors, so appending would leave seq numbers describing commits
        // that are not in this history.
        sh(&d, &["checkout", "-q", "--orphan", "fresh"]);
        sh(&d, &["rm", "-rq", "--cached", "."]);
        std::fs::write(d.join("only.txt"), "new\n").unwrap();
        sh(&d, &["add", "-A"]);
        sh(&d, &["commit", "-q", "-m", "fresh root"]);

        let r = index_repo(&spec, &o).unwrap();
        assert_eq!(r.action, Action::Full);
        assert_eq!(
            r.store.commit_count().unwrap(),
            1,
            "stale commits must be dropped"
        );

        let _ = std::fs::remove_dir_all(o.cache_dir.clone().unwrap());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn bot_and_human_lands_in_different_groups() {
        let d = fixture("ident");
        let mut o = opts_for();
        o.cache_dir =
            Some(std::env::temp_dir().join(format!("ra-cache-id-{}", std::process::id())));
        let _ = std::fs::remove_dir_all(o.cache_dir.clone().unwrap());
        let spec = d.to_string_lossy().into_owned();
        let r = index_repo(&spec, &o).unwrap();
        let groups = r.store.author_groups().unwrap();
        let bots = groups
            .iter()
            .filter(|g| g["is_bot"] == serde_json::Value::Bool(true))
            .count();
        let humans = groups.len() - bots;
        assert_eq!(humans, 1, "Alice is the only human");
        assert!(bots >= 1, "dependabot must be recognised as a bot");
        let _ = std::fs::remove_dir_all(o.cache_dir.clone().unwrap());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_interrupted_run_resumes_instead_of_restarting() {
        let d = fixture("resume");
        let mut o = opts_for();
        o.cache_dir =
            Some(std::env::temp_dir().join(format!("ra-cache-res-{}", std::process::id())));
        o.batch_size = 1;
        let _ = std::fs::remove_dir_all(o.cache_dir.clone().unwrap());
        let spec = d.to_string_lossy().into_owned();

        let boom: FaultHook = Some(&|_| Err(AuditError::new("injected interrupt")));
        assert!(index_repo_with(&spec, &o, boom).is_err());

        // The partial state must still be on disk for the resume to find.
        let db = o.cache_dir.clone().unwrap().join("local");
        let f = std::fs::read_dir(&db)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.extension().map(|x| x == "db").unwrap_or(false))
            .expect("cache db exists");
        let st = SqliteStore::open(&f.to_string_lossy()).unwrap();
        assert_eq!(st.get_meta("state").unwrap().as_deref(), Some("partial"));

        let r = index_repo(&spec, &o).unwrap();
        assert_eq!(
            r.action,
            Action::Resume,
            "an interrupted full index must resume"
        );
        assert_eq!(
            r.store.commit_count().unwrap(),
            3,
            "resumed index must be complete"
        );

        let _ = std::fs::remove_dir_all(o.cache_dir.clone().unwrap());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_nonexistent_ref_is_an_error_not_an_empty_report() {
        let d = fixture("badref");
        let mut o = opts_for();
        o.git_ref = "no-such-branch".into();
        o.cache_dir =
            Some(std::env::temp_dir().join(format!("ra-cache-bad-{}", std::process::id())));
        let _ = std::fs::remove_dir_all(o.cache_dir.clone().unwrap());
        let spec = d.to_string_lossy().into_owned();
        let err = index_repo(&spec, &o).err().expect("a bad ref must fail");
        assert!(
            err.message.contains("Could not resolve ref"),
            "{}",
            err.message
        );
        let _ = std::fs::remove_dir_all(o.cache_dir.clone().unwrap());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn reindex_forces_a_rescan_even_with_an_unchanged_head() {
        let d = fixture("reidx");
        let mut o = opts_for();
        o.cache_dir =
            Some(std::env::temp_dir().join(format!("ra-cache-rx-{}", std::process::id())));
        let _ = std::fs::remove_dir_all(o.cache_dir.clone().unwrap());
        let spec = d.to_string_lossy().into_owned();
        index_repo(&spec, &o).unwrap();
        o.reindex = true;
        let r = index_repo(&spec, &o).unwrap();
        assert_eq!(r.action, Action::Full);
        assert_eq!(r.store.commit_count().unwrap(), 3);
        let _ = std::fs::remove_dir_all(o.cache_dir.clone().unwrap());
        let _ = std::fs::remove_dir_all(&d);
    }
}
