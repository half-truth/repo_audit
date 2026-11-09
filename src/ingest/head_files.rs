//! Snapshot of the files present at HEAD: `(path, loc, size, is_binary)`.
//!
//! Two purposes: files deleted before HEAD must not distort churn totals, and
//! LOC normalises "this file is big because it is generated" into something a
//! churn ranking can reason about.

use super::record::MAX_BLOB_FOR_LOC;
use crate::acquire::git::Git;
use crate::error::Result;
use crate::ingest::record::dec;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};

pub type HeadFile = (String, Option<i64>, i64, i64);

pub fn snapshot_head_files(git: &Git, head_sha: &str) -> Result<Vec<HeadFile>> {
    let raw = git.run(&["ls-tree", "-r", "-z", "-l", "--full-tree", head_sha])?;
    let mut entries: Vec<(String, String, i64, bool)> = Vec::new();
    for ent in raw.split(|&b| b == 0) {
        if ent.is_empty() {
            continue;
        }
        let Some(tab) = ent.iter().position(|&b| b == b'\t') else {
            continue;
        };
        let meta = &ent[..tab];
        let path = dec(&ent[tab + 1..]);
        let fields: Vec<&[u8]> = meta
            .split(|&b| b.is_ascii_whitespace())
            .filter(|f| !f.is_empty())
            .collect();
        // 4 fields and a blob: a submodule entry ("commit") is skipped.
        if fields.len() != 4 || fields[1] != b"blob" {
            continue;
        }
        let Ok(size) = std::str::from_utf8(fields[3]).unwrap_or("0").parse::<i64>() else {
            continue;
        };
        let sha = dec(fields[2]);
        let is_link = fields[0] == b"120000";
        entries.push((path, sha, size, is_link));
    }

    let mut todo: Vec<String> = entries
        .iter()
        .filter(|(_, _, size, link)| !*link && *size <= MAX_BLOB_FOR_LOC as i64)
        .map(|(_, sha, _, _)| sha.clone())
        .collect();
    todo.sort();
    todo.dedup();

    let loc_by_sha = if todo.is_empty() {
        HashMap::new()
    } else {
        batch_cat_file(git, &todo)?
    };

    Ok(entries
        .into_iter()
        .map(|(path, sha, size, _link)| {
            let (loc, binary) = loc_by_sha.get(&sha).copied().unwrap_or((None, 0));
            (path, loc, size, binary)
        })
        .collect())
}

/// One long-lived `git cat-file --batch` process for every blob that needs a
/// line count. Spawning per file would dominate the runtime of a big repo.
fn batch_cat_file(git: &Git, todo: &[String]) -> Result<HashMap<String, (Option<i64>, i64)>> {
    // Reuse the same argv builder and environment as every other call — a
    // second code path here could quietly miss a safety switch.
    let argv = git.argv(
        &[
            std::ffi::OsStr::new("cat-file"),
            std::ffi::OsStr::new("--batch"),
        ],
        &[],
    );
    let mut cmd = Command::new("git");
    cmd.env_clear();
    for (k, v) in crate::acquire::git::git_env() {
        cmd.env(k, v);
    }
    cmd.args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().map_err(|_| {
        crate::error::AuditError::with_hint(
            "git not found",
            "On macOS, run: xcode-select --install or brew install git",
        )
    })?;
    let mut si = child.stdin.take().unwrap();
    let mut so = BufReader::new(child.stdout.take().unwrap());
    let mut out: HashMap<String, (Option<i64>, i64)> = HashMap::new();

    // Batches of 200 keep each write well under a pipe buffer, so git cannot
    // block writing its reply while we are still writing the next request.
    for chunk in todo.chunks(200) {
        si.write_all(chunk.join("\n").as_bytes())?;
        si.write_all(b"\n")?;
        si.flush()?;
        for sha in chunk {
            let mut hdr = String::new();
            if so.read_line(&mut hdr)? == 0 {
                out.insert(sha.clone(), (None, 0));
                continue;
            }
            let f: Vec<&str> = hdr.split_whitespace().collect();
            if f.len() != 3 {
                out.insert(sha.clone(), (None, 0));
                continue;
            }
            let n: usize = f[2].parse().unwrap_or(0);
            let mut data = vec![0u8; n];
            so.read_exact(&mut data)?;
            let mut nl = [0u8; 1];
            let _ = so.read_exact(&mut nl); // trailing newline git appends
            if data.iter().take(8000).any(|&b| b == 0) {
                out.insert(sha.clone(), (None, 1));
            } else {
                let lf = data.iter().filter(|&&b| b == b'\n').count() as i64;
                // A file not ending in a newline still has a last line.
                let last = if !data.is_empty() && *data.last().unwrap() != b'\n' {
                    1
                } else {
                    0
                };
                out.insert(sha.clone(), (Some(lf + last), 0));
            }
        }
    }
    drop(si);
    let _ = child.kill();
    let _ = child.wait();
    Ok(out)
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
    fn loc_counts_lines_and_flags_binary() {
        let dir = std::env::temp_dir().join(format!("ra-head-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        sh(&dir, &["init", "-q", "-b", "main", "."]);
        sh(&dir, &["config", "user.name", "T"]);
        sh(&dir, &["config", "user.email", "t@t"]);
        std::fs::write(dir.join("three.txt"), "a\nb\nc\n").unwrap();
        // No trailing newline: still one line.
        std::fs::write(dir.join("unterminated.txt"), "a\nb").unwrap();
        std::fs::write(dir.join("binary.bin"), [0u8, 1, 2, 3, 0, 9]).unwrap();
        sh(&dir, &["add", "-A"]);
        sh(&dir, &["commit", "-q", "-m", "x"]);
        let gd = dir.join(".git");
        let head = Git::new(Some(gd.clone()))
            .text(&["rev-parse", "HEAD"], None)
            .unwrap();

        let g = Git::new(Some(gd.clone()));
        let files = snapshot_head_files(&g, &head).unwrap();
        let get = |n: &str| files.iter().find(|f| f.0 == n).unwrap().clone();
        assert_eq!(get("three.txt"), ("three.txt".into(), Some(3), 6, 0));
        assert_eq!(
            get("unterminated.txt"),
            ("unterminated.txt".into(), Some(2), 3, 0)
        );
        let bin = get("binary.bin");
        assert_eq!((bin.1, bin.3), (None, 1), "binary files get loc=NULL");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_symlink_is_recorded_but_not_read() {
        let dir = std::env::temp_dir().join(format!("ra-head2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        sh(&dir, &["init", "-q", "-b", "main", "."]);
        sh(&dir, &["config", "user.name", "T"]);
        sh(&dir, &["config", "user.email", "t@t"]);
        std::fs::write(dir.join("real.txt"), "x\n").unwrap();
        std::os::unix::fs::symlink("real.txt", dir.join("link")).unwrap();
        sh(&dir, &["add", "-A"]);
        sh(&dir, &["commit", "-q", "-m", "x"]);
        let gd = dir.join(".git");
        let head = Git::new(Some(gd.clone()))
            .text(&["rev-parse", "HEAD"], None)
            .unwrap();
        let files = snapshot_head_files(&Git::new(Some(gd.clone())), &head).unwrap();
        let link = files.iter().find(|f| f.0 == "link").unwrap();
        assert_eq!(
            link.1, None,
            "a symlink's target must not be read as content"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
