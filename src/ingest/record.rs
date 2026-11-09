//! Records produced by the `git log -z` ingest pass.

/// One file's change within a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// `A M D R T` — `C` (copy) is not enabled.
    pub status: String,
    pub path: String,
    pub old_path: Option<String>,
    pub added: i64,
    pub deleted: i64,
    pub is_binary: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRecord {
    pub sha: String,
    pub parents: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    pub author_ts: i64,
    pub commit_ts: i64,
    pub subject: String,
    pub body: String,
    pub files: Vec<FileChange>,
}

impl CommitRecord {
    pub fn is_merge(&self) -> bool {
        self.parents.len() > 1
    }
}

/// `%x1e` starts a commit header, `%x1f` separates fields, `%x1d` ends the
/// header. None of these appear in normal text, which is what makes
/// position-driven token consumption safe.
pub const LOG_FORMAT: &str = "%x1e%H%x1f%P%x1f%aN%x1f%aE%x1f%at%x1f%ct%x1f%s%x1f%b%x1d";

/// The complete, fixed argument list for the history scan.
///
/// `--format` lives here rather than being a caller parameter: the parser's
/// whole contract is position-driven token consumption of *this* format, so a
/// caller able to swap it could break parsing silently.
pub const LOG_ARGS: &str = concat!(
    "log --topo-order -M --raw --numstat -z --no-ext-diff --no-textconv ",
    "--no-color --no-show-signature --abbrev=7 --format=",
    "%x1e%H%x1f%P%x1f%aN%x1f%aE%x1f%at%x1f%ct%x1f%s%x1f%b%x1d"
);

/// Split for argv. Kept next to [`LOG_ARGS`] so the two cannot drift.
pub fn log_argv() -> Vec<&'static str> {
    LOG_ARGS.split(' ').collect()
}

pub const BODY_LIMIT: usize = 2000;

/// Blobs above this size are not read for LOC; the file is recorded with
/// `loc = NULL` rather than pulling megabytes through a pipe.
pub const MAX_BLOB_FOR_LOC: usize = 1 << 20;

/// Python's `bytes.decode("utf-8", "replace")`, which is exactly what
/// `String::from_utf8_lossy` does: invalid bytes become U+FFFD.
pub fn dec(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}
