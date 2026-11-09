# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

One Rust crate (edition 2021, `rust-version` 1.98, crate version 0.2.0): library `repo_audit` plus
binary `repo-audit`. No workspace, no CI test job, no remote configured, no deploy. ~8.4k lines of
Rust under `src/` and `tests/`.

The tool takes a repository (URL / `git@host:owner/repo` / `owner/repo` / local path), bare-clones
it, scans the history once into a local SQLite index, and renders five findings: churn hotspots,
bus factor, bug clustering (fix density), team trend, and firefight frequency.

User-facing CLI documentation lives in `README.md`. There is currently **no** design-rationale
document in the workspace: `README.md` and the `CLAUDE.md` that preceded this file both pointed at
`agent-chat-history.md`, which is no longer present (recover it from the earlier repository's git
history if you need the rationale behind an audit metric). The per-module `//!` doc comments are
the most reliable record of intent that ships with the code.

## Verification (run all four, in order; all currently pass)

```bash
cargo fmt --check
```

```bash
cargo clippy --all-targets -- -D warnings
```

```bash
cargo test
```

```bash
cargo build --release && ./target/release/repo-audit selftest
```

- `cargo test` = 107 library tests + 10 integration tests (`tests/git_ground_truth.rs`) + one empty
  doc-test suite, ~1s. The release binary is the artifact under test — the gate and the CLI both use
  `target/release/repo-audit`.
- Single test: `cargo test identity -- --nocapture`. Useful filters: `identity`, `tokens`,
  `parser`, `store`, `density`.
- `selftest` is a **user-facing command**, not a test harness: it builds real git fixtures in a temp
  dir (rename chains, spaces/newlines/non-ASCII filenames, path reuse, binary blobs, bot commits,
  a `feat` branch merged `--no-ff`) and prints `selftest: all 28 assertions passed`.
- `tests/git_ground_truth.rs` compares the index against git's own output. It **deletes
  `target/git-gate/` at the start of every run** so a warm cache cannot make the gate agree with
  itself. A run finishing in ~0s that prints `skip: no regression targets checked out` is **not a
  pass** — it is the gate silently skipping itself because no local fixture repos are checked out.
  This repo currently has **no** `proxyClient/` / `proxyServer/` fixtures, so all 10 integration
  tests skip locally and in CI.

### Regression gate layer 1 (golden baseline) — needs fixtures

Layer 1 compared each of a set of local fixture repositories in four output modes (report, `--json`,
`identities`, `index`) byte-for-byte against `.baseline/`. The fixture targets lived in
`proxyClient/` and `proxyServer/` and were deleted; neither they nor `.baseline/` are present here,
so **this gate cannot run**. (They were kept out of git on purpose — committed, they would be
recorded as submodule gitlinks.) Restoring it means re-cloning a fixture set into that layout and
regenerating baselines from scratch.

Do not "regenerate" baselines to make a gate green — a baseline is the code's own past output, so
overwriting one freezes whatever the code does now as the standard. Rebuild only for a deliberate
output change, in the same session as the change, and report which outputs moved and why.

## Architecture

Pipeline: `acquire → ingest → normalize → store → analyzers → report`, orchestrated by `index.rs`.
Layers depend inward; `analyzers` and `report` read only through `SqliteStore` methods — do not
scatter SQL into the render layer.

- **`main.rs` is a thin wrapper.** All logic is in the library so `cargo test` and the CLI exercise
  the same code. `normalize_argv` runs *before* clap because clap cannot express a defaulted
  subcommand (`audit` is the default). `catch_unwind` turns a panic into exit code 3 — a panic is a
  defect to report, not user-input handling.
- **`acquire/`**: `spec.rs` classifies the input (local-path branch first and deliberately
  permissive; only bare `owner/repo` defaults to github.com); `git.rs` is the single wrapper every
  git subprocess goes through; `clone.rs` does bare clone only; `lock.rs` is a `flock` on
  `.audit.db.lock` held for the whole indexing pass.
- **`ingest/`**: `tokens.rs` is **position-driven** parsing of `git log -z --raw --numstat`
  (`\x1e` record start, `\x1f` field separator, `\x1d` header end). Never pattern-match a path or
  use a filename as a regex: paths may contain spaces, quotes, CJK, newlines, any byte at all. A
  read error is not EOF — it is surfaced by `take_err`, because silently truncating a history is
  the one failure an audit tool may not have. `record.rs` holds `BODY_LIMIT = 2000`.
- **`normalize/`**: `identity.rs` merges authors with a union-find; `patterns.rs` holds every
  classification regex, passed *into* SQLite as bind parameters, so they must stay byte-identical
  to what the Python implementation compiled.
- **`store/`**: `sqlite.rs` owns the schema, fingerprints and every read-only query;
  `fingerprint.rs` re-implements CPython's `json.dumps` string encoding by hand (fingerprints must
  be byte-identical to Python's or every cached `.audit.db` is silently invalidated).
  `udf.rs` registers `rx` / `isrevert` as SQLite scalar functions.
- **`analyzers.rs`**: the five analyses, each a read-only query. Thresholds are module-level
  constants.
- **`report/`**: `terminal.rs`, `json.rs`, `identities.rs`, plus `layout.rs` for CJK-aware column
  layout.
- **`selftest.rs`**: the fixture builder behind the `selftest` subcommand.

### Three version constants (`src/lib.rs`)

`PARSER_VERSION` (log parsing), `IDENTITY_VERSION` (author merge rules), `SCHEMA_VERSION` (tables).
Bump the one matching the change — they are the cache's invalidation mechanism. On a
`PRAGMA user_version` mismatch the DB is **deleted and rebuilt**: SQLite here is a local index cache,
not a database, so do not add migrations.

Cache layout: remote repos `~/Library/Caches/repo-audit/<host>/<owner>/<repo>.git` + sibling
`.audit.db`; local repos `local/<sha1-first-10>-<repo>.audit.db` (no copy of the repo). Override with
`--cache-dir` or `REPO_AUDIT_CACHE`.

`--tmp-dir` is **mandatory whenever a fresh remote clone is needed** — a staging dir on the same
volume as the cache, checked for writability and same-volume-ness up front, with no fallback (the
final step is an atomic rename into the cache). Local paths never need it.

### Two behaviours that look like bugs and are not

- **Tie-breaking is load-bearing.** `PyCounter` replicates Python's `Counter.most_common()`
  insertion order, which equals SQLite `GROUP BY` return order. Files with identical churn are
  common, so adding an `ORDER BY` to "stabilise" a ranking silently rewrites the report.
- **Item 3 has no 🟢/🟡/🔴 on purpose.** It ranks by fix density (`fixes / touches`,
  floor `DENSITY_MIN_TOUCH = 10`) because the old `max(churn percentile × bug percentile)` scored
  🔴 on 15/15 repositories — the statistic, not the threshold, was wrong. Do not restore a light
  there, and do not add a significance threshold: fix commits span 1.3–4.8 files on average, so
  the usual per-file null hypothesis is invalid.

### Untrusted input

The audited repository is untrusted. Every git call keeps `core.hooksPath=/dev/null`,
`core.fsmonitor=false`, `core.quotepath=false`, `protocol.allow=never` with https/ssh explicitly
re-allowed, `GIT_TERMINAL_PROMPT=0`, `LC_ALL=C`, `GIT_SSH_COMMAND=ssh -o BatchMode=yes`,
`env_clear()` (with `GIT_DIR` / `GIT_WORK_TREE` / `GIT_INDEX_FILE` / object-dir vars stripped), and
`--no-ext-diff` / `--no-textconv`. Bare clone only: never checkout, never fetch submodules, never run
anything from an audited repo. Local paths are read through `rev-parse --absolute-git-dir`.

## Streams and output

**stdout = report, stderr = progress.** `-q` suppresses progress and is forbidden in byte-level
comparisons because it hides stdout/stderr routing regressions. The report has **no ANSI colour,
deliberately** — colour would make output depend on whether stdout is a TTY and the byte gate
compares piped output. Emphasis comes from structure (`report/layout.rs`); do not add
TTY-conditional escapes. Column width is measured in terminal columns (`display_width` /
`pad_display` / `truncate_display` in `util/text.rs` treat CJK as two columns); padding with
`char::count()` misaligns every row with a Chinese author name.

## Constraints

- **Dependency allowlist is closed**: `rusqlite` (bundled + functions), `regex`, `clap` (derive),
  `serde_json` (preserve_order), `sha1`, `libc`; dev-dep `tempfile`. Adding or bumping a crate needs
  explicit approval.
- No credentials in the program: system git credential helper / `gh auth` only. URL userinfo is
  stripped before it reaches git.
- 🟢/🟡/🔴 thresholds are empirical and the report discloses this; changing judgement logic means
  updating that disclosure.
- Do not audit repositories outside a known set (network + disk writes). `~/linux` (1.48M commits)
  is forbidden outright — indexing it heats the machine for a long time.
- `git commit` / `git push` need explicit approval. Commit messages in English, verb-first, one
  atomic change.

## Cloud audit workflow

`.github/workflows/audit.yml` is a manual (`workflow_dispatch`) workflow: it takes a repo URL and
an optional ref, builds the release binary, and runs `repo-audit audit` with `--cache-dir` and
`--tmp-dir` both inside `$GITHUB_WORKSPACE`, uploads `report.txt` as an artifact, then deletes the
cache regardless of outcome. There is no separate clone step — the tool's remote mode already
bare-clones into the staging dir. The `audit` subcommand is the only thing the workflow runs; it is
not a CI test job for this crate.

## History worth knowing

The tool was originally a single-file Python script and was rewritten in Rust on 2026-10-05. The
Python source is gone from the workspace but recoverable from the earlier repository's git history
(`git show dc79b69^:repo_audit_mvp.py` there) for one-off cross-checks — never put it back.
`PYTHON`-era comments still in the tree (positions in `sqlite.rs`, Python-compatible regex
semantics, `PyCounter`) are deliberate compatibility requirements, not leftovers.