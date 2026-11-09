//! Exclusive per-repository lock.
//!
//! Two `repo-audit` processes indexing the same repo would interleave writes
//! into the same `.audit.db`; the second one to finish would leave a database
//! that is neither the old state nor the new one.

use crate::error::{AuditError, Result};
use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

pub struct FileLock {
    _file: File,
    path: PathBuf,
}

impl FileLock {
    /// The lock file's path, for diagnostics while the lock is held.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // Closing the fd releases the flock, but be explicit so a future refactor
        // that keeps the File around cannot silently extend the lock's life.
        unsafe {
            libc::flock(self._file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// Hold an exclusive lock on `path` until dropped. Non-blocking: a busy lock is
/// reported, never waited on — waiting silently would look like a hang.
pub fn file_lock(path: &Path) -> Result<FileLock> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            AuditError::with_hint(
                format!("Failed to create cache directory: {}", parent.display()),
                format!("Check permissions: {e}"),
            )
        })?;
    }
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| {
            AuditError::new(format!("Failed to open lock file {}: {e}", path.display()))
        })?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        return Err(AuditError::with_hint(
            "Another repo-audit process is handling this repository",
            format!(
                "Wait for it to finish, or delete the lock file: {}",
                path.display()
            ),
        ));
    }
    Ok(FileLock {
        _file: file,
        path: path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_is_refused_until_dropped() {
        let dir = std::env::temp_dir().join(format!("ra-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("x.lock");
        let a = file_lock(&p).unwrap();
        let err = file_lock(&p).err().expect("second acquire must fail");
        assert!(err.to_string().contains("Another"), "{err}");
        drop(a);
        assert!(file_lock(&p).is_ok(), "lock must be released on drop");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lock_path_is_reported_in_the_hint() {
        let dir = std::env::temp_dir().join(format!("ra-lock2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("repo.audit.db.lock");
        let _a = file_lock(&p).unwrap();
        let err = file_lock(&p).err().expect("second acquire must fail");
        assert!(err.hint.contains("repo.audit.db.lock"), "{}", err.hint);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
