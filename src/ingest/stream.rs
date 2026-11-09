//! Drive `git log` and hand the byte stream to the parser.
//!
//! Memory use is bounded by the batch size, not by repository size: git's
//! output is consumed incrementally and each parsed commit is handed on before
//! the next is read.

use super::record::{log_argv, CommitRecord};
use super::tokens::{parse_log_tokens, NulTokens};
use crate::acquire::git::Git;
use crate::error::{AuditError, Result};
use std::ffi::OsStr;

pub fn stream_commits(
    git: &Git,
    rev_args: &[&str],
    warnings: &mut Vec<String>,
) -> Result<std::vec::IntoIter<CommitRecord>> {
    let mut cfg: Vec<String> = Vec::new();
    if let Some(m) = &git.mailmap_file {
        cfg.push(format!("mailmap.file={m}"));
    }
    let base = log_argv();
    let mut args: Vec<&OsStr> = base.iter().map(OsStr::new).collect();
    args.extend(rev_args.iter().map(OsStr::new));
    let mut child = git.spawn_stream_cfg(&args, &cfg)?;
    // git does not read stdin here; leaving the pipe open would deadlock.
    drop(child.stdin.take());

    let stdout = child.stdout.take().expect("stdout was piped");
    let mut tokens = NulTokens::new(stdout);
    // `&mut NulTokens` keeps the error slot reachable after parsing; the
    // iterator item itself is a bare token with nowhere to report a failure.
    let recs = parse_log_tokens(&mut tokens, warnings)?;
    // A mid-stream read error must fail the run, not masquerade as a short
    // history — this is an audit tool, "fewer commits" is not a valid answer.
    if let Some(e) = tokens.take_err() {
        return Err(AuditError::new(format!(
            "Failed to read git log output: {e}"
        )));
    }

    let status = child
        .wait_with_output()
        .map_err(|e| AuditError::new(format!("git log subprocess error: {e}")))?;
    if !status.status.success() {
        let err = crate::util::text::safe(
            &String::from_utf8_lossy(&status.stderr)
                .chars()
                .rev()
                .take(300)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>(),
        );
        return Err(AuditError::new(format!(
            "git log failed (exit code {:?}): {err}",
            status.status.code().unwrap_or(-1)
        )));
    }
    Ok(recs.into_iter())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &std::path::Path, args: &[&str]) {
        assert!(std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success());
    }

    #[test]
    fn streams_a_small_repo_end_to_end() {
        let dir = std::env::temp_dir().join(format!("ra-stream-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        sh(&dir, &["init", "-q", "-b", "main", "."]);
        sh(&dir, &["config", "user.name", "Alice Smith"]);
        sh(&dir, &["config", "user.email", "alice@example.org"]);
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        sh(&dir, &["add", "-A"]);
        sh(&dir, &["commit", "-q", "-m", "first"]);
        std::fs::write(dir.join("b.txt"), "three\n").unwrap();
        sh(&dir, &["add", "-A"]);
        sh(&dir, &["commit", "-q", "-m", "second"]);

        let g = Git::new(Some(dir.join(".git")));
        let mut w = Vec::new();
        let recs: Vec<CommitRecord> = stream_commits(&g, &["--no-merges", "HEAD"], &mut w)
            .unwrap()
            .collect();
        assert_eq!(recs.len(), 2, "{w:?}");
        // --topo-order means newest first.
        assert_eq!(recs[0].subject, "second");
        assert_eq!(recs[1].subject, "first");
        assert_eq!(recs[0].author_name, "Alice Smith");
        assert_eq!(recs[0].files.len(), 1);
        assert_eq!(recs[0].files[0].path, "b.txt");
        assert!(!recs[0].files[0].is_binary);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bad_revision_is_reported_not_silently_empty() {
        let dir = std::env::temp_dir().join(format!("ra-stream2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        sh(&dir, &["init", "-q", "-b", "main", "."]);
        let g = Git::new(Some(dir.join(".git")));
        let mut w = Vec::new();
        let err = stream_commits(&g, &["no-such-ref"], &mut w).unwrap_err();
        assert!(err.message.contains("git log failed"), "{}", err.message);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A read error mid-stream is stored, not eaten: the run must fail rather
    /// than print a report over a silently truncated history.
    #[test]
    fn a_read_error_is_surfaced_not_treated_as_eof() {
        use std::io::Read;

        #[derive(Default)]
        struct CountingReader {
            n: usize,
        }

        impl Read for CountingReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                match self.n {
                    0 => {
                        self.n += 1;
                        buf[..6].copy_from_slice(b"aa\0bb\0");
                        Ok(6)
                    }
                    _ => Err(std::io::Error::other("pipe broken")),
                }
            }
        }

        let mut r = CountingReader::default();
        let mut t = NulTokens::new(&mut r);
        let toks: Vec<Vec<u8>> = t.by_ref().collect();
        assert_eq!(
            toks,
            vec![b"aa".to_vec(), b"bb".to_vec()],
            "tokens before the failure are still delivered"
        );
        let err = t
            .take_err()
            .expect("the read error must survive the stream");
        assert_eq!(err.to_string(), "pipe broken");
    }
}
