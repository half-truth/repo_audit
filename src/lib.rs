//! repo-audit — Git repository health auditor.
//!
//! Acquire (bare clone + cache) → Ingest (streaming `git log -z` parse) →
//! SQLite cache → five read-only analyzers → terminal report.
//!
//! History is scanned once and stored; the analyzers only query the database, so
//! memory use is independent of repository size.

pub mod acquire;
pub mod analyzers;
pub mod cli;
pub mod error;
pub mod index;
pub mod ingest;
pub mod normalize;
pub mod report;
pub mod selftest;
pub mod store;
pub mod util;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub const SCHEMA_VERSION: i64 = 1;
pub const PARSER_VERSION: i64 = 1;
/// v3: noreply identities merge on the numeric UID (the login changes on rename,
/// the UID does not).
/// v2: tightened generic-email detection (`git@real.domain` is no longer
/// anonymous) + "a group with no email anchor at all" is unattributable.
pub const IDENTITY_VERSION: i64 = 3;
