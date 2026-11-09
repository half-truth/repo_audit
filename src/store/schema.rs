//! The SQLite schema, kept byte-identical to Python's `_SCHEMA`.
//!
//! The file *is* a cache, not a data store: a version mismatch means deleting
//! and rebuilding, never migrating.

pub const SCHEMA: &str = r#"
CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE commits(
  sha TEXT PRIMARY KEY, seq INTEGER NOT NULL, parents TEXT NOT NULL, is_merge INTEGER NOT NULL,
  author_name TEXT NOT NULL, author_email TEXT NOT NULL, author_id INTEGER,
  author_ts INTEGER, commit_ts INTEGER, subject TEXT, body TEXT,
  files_changed INTEGER, insertions INTEGER, deletions INTEGER);
CREATE INDEX commits_seq ON commits(seq);
CREATE INDEX commits_author ON commits(author_id);
CREATE INDEX commits_ts ON commits(author_ts);
CREATE TABLE file_changes(
  id INTEGER PRIMARY KEY, sha TEXT NOT NULL, status TEXT NOT NULL, path TEXT NOT NULL, old_path TEXT,
  added INTEGER, deleted INTEGER, is_binary INTEGER, path_current TEXT);
CREATE INDEX fc_sha ON file_changes(sha);
CREATE INDEX fc_path ON file_changes(path);
CREATE INDEX fc_cur ON file_changes(path_current);
CREATE TABLE authors(
  author_id INTEGER PRIMARY KEY, canonical_name TEXT, canonical_email TEXT, is_bot INTEGER,
  commit_count INTEGER, first_ts INTEGER, last_ts INTEGER, aliases_json TEXT, reasons_json TEXT);
CREATE TABLE identities(name TEXT NOT NULL, email TEXT NOT NULL, author_id INTEGER NOT NULL, PRIMARY KEY(name, email));
CREATE TABLE files_at_head(path TEXT PRIMARY KEY, loc INTEGER, size INTEGER, is_binary INTEGER);
"#;

/// Cleared by `reset()`; `meta` is handled separately because it holds the
/// fingerprints, which must survive a re-index of the same ref.
pub const DATA_TABLES: &[&str] = &[
    "file_changes",
    "commits",
    "authors",
    "identities",
    "files_at_head",
];
