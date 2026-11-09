//! Clone / fetch with an atomic rename.
//!
//! Cloning into a staging directory and renaming on success means an
//! interrupted run never leaves a half-populated cache that the next run
//! would treat as a valid bare repo.
//!
//! The staging dir is **user-supplied** (`--tmp-dir`, via `IndexOptions`).
//! There is no fallback next to the cache: an unwritable location, or one on
//! a different volume than the cache (cross-volume `rename` fails with
//! EXDEV), must fail before the download, not after it.

use super::git::Git;
use crate::error::{AuditError, Result};
use crate::util::text::safe;
use std::ffi::OsStr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug)]
pub enum Action {
    Cloned,
    Updated,
}

impl Action {
    pub fn as_str(&self) -> &'static str {
        match self {
            Action::Cloned => "cloned",
            Action::Updated => "updated",
        }
    }
}

fn tty() -> bool {
    unsafe { libc::isatty(2) == 1 }
}

/// `<stage>/<repo>.git.tmp-<pid>` — the same name the temp dir used to have
/// next to the cache, just pinned to the user-supplied staging directory.
fn staging_tmp(stage: &Path, dest: &Path) -> PathBuf {
    stage.join(format!(
        "{}.tmp-{}",
        dest.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ))
}

/// Returns whether the cache was created or refreshed.
pub fn clone_or_update(
    clone_url: &str,
    dest: &Path,
    staging: Option<&Path>,
    timeout: Duration,
    allow_file: bool,
    verbose: bool,
) -> Result<Action> {
    let inherit = tty();
    let mk = |dir: Option<&Path>| {
        let g = Git::new(dir.map(|d| d.to_path_buf()));
        Git { allow_file, ..g }
    };

    if dest.exists() {
        let g = mk(Some(dest));
        if !g.run_ok(&["rev-parse", "--git-dir"])? {
            return Err(AuditError::with_hint(
                format!("Corrupt cache directory: {}", dest.display()),
                "Delete the directory and try again",
            ));
        }
        if verbose {
            eprintln!("  fetch …");
        }
        let args: [&OsStr; 5] = [
            OsStr::new("fetch"),
            OsStr::new("--prune"),
            OsStr::new("--no-tags"),
            OsStr::new("origin"),
            OsStr::new("+refs/heads/*:refs/heads/*"),
        ];
        g.run_os(&args, &[], Some(timeout), true, inherit, None)?;
        sync_remote_head(&g, timeout);
        return Ok(Action::Updated);
    }

    if let Some(p) = dest.parent() {
        std::fs::create_dir_all(p).map_err(|e| {
            AuditError::new(format!(
                "Failed to create cache directory {}: {e}",
                p.display()
            ))
        })?;
    }

    // A fresh clone stages where the user says, precisely because the
    // alternative — inventing a location — is how a run ends up trusting a
    // directory race the user never agreed to. Missing, unwritable, or
    // cross-volume all fail here, before the download starts.
    let stage = staging.ok_or_else(|| {
        AuditError::with_hint(
            "Remote repositories require --tmp-dir <DIR>: specify a staging directory for clone --bare".to_string(),
            "Clone into the staging directory first, then atomically rename it into the cache; local repositories do not need this option",
        )
    })?;
    std::fs::create_dir_all(stage).map_err(|e| {
        AuditError::with_hint(
            format!("Failed to create staging directory {}: {e}", stage.display()),
            "Ensure --tmp-dir points to a writable path (choose another directory if the binary is installed in a read-only location)",
        )
    })?;
    // Same-volume check up front: `rename` cannot cross devices, and noticing
    // that only after a full download is the worst possible time to learn it.
    let dev_of = |p: &Path| std::fs::metadata(p).map(|m| m.dev());
    let dest_parent = dest
        .parent()
        .ok_or_else(|| AuditError::new("Cache directory has no parent"))?;
    if !matches!(
        (dev_of(stage), dev_of(dest_parent)),
        (Ok(a), Ok(b)) if a == b
    ) {
        return Err(AuditError::with_hint(
            format!(
                "--tmp-dir {} and cache directory {} are on different volumes",
                stage.display(),
                dest_parent.display()
            ),
            "Atomic rename cannot cross volumes; place --tmp-dir on the same volume as the cache, or use --cache-dir to change the cache location",
        ));
    }

    let tmp: PathBuf = staging_tmp(stage, dest);
    let _ = std::fs::remove_dir_all(&tmp);
    let res = (|| -> Result<()> {
        if verbose {
            eprintln!("  clone --bare {} …", safe(clone_url));
        }
        let progress: &OsStr = if inherit {
            OsStr::new("--progress")
        } else {
            OsStr::new("--quiet")
        };
        let url: &OsStr = OsStr::new(clone_url);
        let t: &OsStr = tmp.as_os_str();
        mk(None).run_os(
            &[
                OsStr::new("clone"),
                OsStr::new("--bare"),
                OsStr::new("--no-tags"),
                progress,
                OsStr::new("--"),
                url,
                t,
            ],
            &[],
            Some(timeout),
            true,
            inherit,
            None,
        )?;
        // A bare clone ships with no fetch refspec, so every later `fetch` would
        // silently update nothing.
        mk(Some(&tmp)).run(&[
            "config",
            "remote.origin.fetch",
            "+refs/heads/*:refs/heads/*",
        ])?;
        std::fs::rename(&tmp, dest).map_err(|e| {
            AuditError::new(format!(
                "Failed to write cache directory {}: {e}",
                dest.display()
            ))
        })
    })();
    // Either way the temp dir goes: on success it no longer exists.
    let _ = std::fs::remove_dir_all(&tmp);
    res?;
    Ok(Action::Cloned)
}

/// Follow the remote's default branch when it has been renamed (master→main):
/// a cache fetched before the rename would keep auditing the old branch forever.
fn sync_remote_head(g: &Git, timeout: Duration) {
    let Ok(out) = g.text(&["ls-remote", "--symref", "origin", "HEAD"], Some(timeout)) else {
        return; // offline: the cached refs are still good enough
    };
    let Some(rest) = out.strip_prefix("ref:") else {
        return;
    };
    let Some(refname) = rest.split_whitespace().next() else {
        return;
    };
    if !refname.starts_with("refs/heads/") || refname.is_empty() {
        return;
    }
    if g.run_ok(&["show-ref", "--verify", "--quiet", refname])
        .unwrap_or(false)
    {
        let _ = g.run(&["symbolic-ref", "HEAD", refname]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn init_repo(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ra-clone-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "a@b.c"],
            vec!["config", "user.name", "A"],
        ] {
            assert!(Command::new("git")
                .args(&args)
                .current_dir(&d)
                .status()
                .unwrap()
                .success());
        }
        std::fs::write(d.join("f.txt"), "hello\n").unwrap();
        let st = Command::new("git")
            .args(["add", "-A"])
            .current_dir(&d)
            .status()
            .unwrap();
        assert!(st.success());
        let st = Command::new("git")
            .args(["commit", "-q", "-m", "c1"])
            .current_dir(&d)
            .status()
            .unwrap();
        assert!(st.success());
        d
    }

    #[test]
    fn clone_then_update_against_a_local_source() {
        let src = init_repo("clone");
        let dest = std::env::temp_dir().join(format!("ra-clone-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dest);
        let stage = std::env::temp_dir().join(format!("ra-clone-stage-{}", std::process::id()));
        let url = format!("file://{}", src.display());
        assert_eq!(
            clone_or_update(
                &url,
                &dest,
                Some(&stage),
                Duration::from_secs(60),
                true,
                false
            )
            .unwrap()
            .as_str(),
            "cloned"
        );
        let g = Git::with_file(Some(dest.clone()));
        assert_eq!(
            g.text(&["rev-parse", "--verify", "main"], None)
                .unwrap()
                .len(),
            40
        );

        // A second call reuses the cache rather than re-cloning.
        assert_eq!(
            clone_or_update(
                &url,
                &dest,
                Some(&stage),
                Duration::from_secs(60),
                true,
                false
            )
            .unwrap()
            .as_str(),
            "updated"
        );
        let _ = std::fs::remove_dir_all(&dest);
        let _ = std::fs::remove_dir_all(&stage);
        let _ = std::fs::remove_dir_all(&src);
    }

    #[test]
    fn no_temp_directory_survives_a_failed_clone() {
        let dest = std::env::temp_dir().join(format!("ra-clone-bad-{}.git", std::process::id()));
        let _ = std::fs::remove_dir_all(&dest);
        let stage = std::env::temp_dir().join(format!("ra-clone-stage2-{}", std::process::id()));
        let err = clone_or_update(
            "/nonexistent/path/repo.git",
            &dest,
            Some(&stage),
            Duration::from_secs(30),
            true,
            false,
        )
        .expect_err("clone of a nonexistent path must fail");
        assert!(!err.message.is_empty());
        let leftover = staging_tmp(&stage, &dest);
        assert!(
            !leftover.exists(),
            "interrupted clone left {leftover:?} behind"
        );
        assert!(!dest.exists());
        let _ = std::fs::remove_dir_all(&stage);
    }

    /// The staging dir is mandatory for a fresh clone: no env fallback, no
    /// silent default next to the cache. A missing flag fails with a hint.
    #[test]
    fn a_fresh_clone_without_staging_is_an_error() {
        let src = init_repo("nostage");
        let dest = std::env::temp_dir().join(format!("ra-clone-nostage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dest);
        let url = format!("file://{}", src.display());
        let err =
            clone_or_update(&url, &dest, None, Duration::from_secs(60), true, false).unwrap_err();
        assert!(err.message.contains("--tmp-dir"), "{}", err.message);
        assert!(!dest.exists(), "no cache may materialise on a failed spec");
        let _ = std::fs::remove_dir_all(&src);
    }

    /// An unwritable staging location fails before any download, and never
    /// falls back to staging beside the cache.
    #[test]
    fn an_unwritable_staging_dir_errors_instead_of_falling_back() {
        let src = init_repo("badstage");
        let dest = std::env::temp_dir().join(format!("ra-clone-badstage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dest);
        let url = format!("file://{}", src.display());
        let err = clone_or_update(
            &url,
            &dest,
            Some(Path::new("/dev/null/repo-audit-stage")),
            Duration::from_secs(30),
            true,
            false,
        )
        .unwrap_err();
        assert!(err.message.contains("staging directory"), "{}", err.message);
        assert!(!dest.exists());
        let _ = std::fs::remove_dir_all(&src);
    }
}
