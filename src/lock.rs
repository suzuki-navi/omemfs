use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;
use std::path::Path;

use crate::error::Error;

/// An exclusive lock on the repository's `clone_root` state file.
///
/// Acquired by opening the **persistent** lock file `.omemfs/clone_root.lock`
/// (created if absent, never deleted) and taking a non-blocking exclusive
/// `flock(2)` (`LOCK_EX | LOCK_NB`) on it. The lock is released automatically by
/// the kernel when the owning file descriptor is closed (on `Drop`) or when the
/// process exits — there is no stale-lock condition and no PID-based cleanup.
///
/// The PID and command name are written into the file purely for diagnostics in
/// the contention error message; they are never used for liveness checks.
///
/// See `design/12_locking.md` for the full specification.
pub struct RepoLock {
    // Holding the file open keeps the flock alive. Dropping it releases the
    // lock (the kernel closes the descriptor). The lock file itself is never
    // removed.
    _file: std::fs::File,
}

impl RepoLock {
    pub fn acquire(omemfs_dir: &Path) -> Result<Self, Error> {
        let lock_path = omemfs_dir.join("clone_root.lock");

        // Open (creating if necessary) the persistent lock file.
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(Error::Io)?;

        // Try to take a non-blocking exclusive lock.
        let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if ret != 0 {
            // Acquisition failed: another process holds the lock. Read the
            // diagnostic content the holder wrote, for the error message only.
            let holder = read_holder_info(&lock_path);
            return Err(lock_contention_error(&lock_path, holder));
        }

        // We hold the lock. Record diagnostic info (PID and command name).
        // Truncate and rewrite from the start so a previous holder's longer
        // line cannot leave trailing bytes.
        let _ = file.set_len(0);
        let _ = file.seek(SeekFrom::Start(0));
        let cmd = current_command_name();
        let _ = write!(file, "{}\n{}\n", std::process::id(), cmd);
        let _ = file.flush();

        Ok(RepoLock { _file: file })
    }
}

// No explicit `Drop`: closing `_file` releases the flock, and the lock file is
// intentionally left in place (persistent lock file).

/// Read the diagnostic holder info (PID and optional command name) previously
/// written into the lock file. Best-effort; returns `None` on any error.
fn read_holder_info(lock_path: &Path) -> Option<(u32, Option<String>)> {
    let mut f = std::fs::File::open(lock_path).ok()?;
    let mut s = String::new();
    f.read_to_string(&mut s).ok()?;
    let mut lines = s.lines();
    let pid: u32 = lines.next()?.trim().parse().ok()?;
    let cmd = lines
        .next()
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty());
    Some((pid, cmd))
}

/// Best-effort retrieval of the current process's command name (argv[0] base).
fn current_command_name() -> String {
    std::env::args()
        .next()
        .and_then(|a| {
            Path::new(&a)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "omemfs".to_string())
}

/// Build the contention error message.
///
/// Deliberately does NOT suggest deleting the lock file. Locking is
/// `flock(2)`-based (see the struct doc above): there is no stale-lock
/// condition, so "just delete it" is never the right advice. If the holder
/// process is still alive, deleting `.omemfs/clone_root.lock` does not free
/// anything -- it makes the next `omemfs` invocation create a brand-new inode
/// and take its own, independent flock on it, so the original holder and the
/// new process would then mutate `clone_root` concurrently with no lock
/// between them. The correct remedy for a genuinely stuck holder is to
/// terminate that process (e.g. `kill <PID>`); the kernel releases the flock
/// the moment its file descriptor closes.
fn lock_contention_error(lock_path: &Path, holder: Option<(u32, Option<String>)>) -> Error {
    let detail = match holder {
        Some((pid, Some(cmd))) => format!(" (PID: {}, command: {})", pid, cmd),
        Some((pid, None)) => format!(" (PID: {})", pid),
        None => String::new(),
    };
    Error::LockFailed(format!(
        "Unable to acquire lock '{lock}': file exists.\n\nAnother omemfs process{detail} holds this lock. Wait for it to finish, or terminate that process if it is stuck.",
        lock = lock_path.display(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn second_acquire_in_same_process_fails() {
        let dir = TempDir::new().unwrap();
        let _lock = RepoLock::acquire(dir.path()).unwrap();
        // A second attempt on the same lock file fails cleanly (LOCK_NB).
        match RepoLock::acquire(dir.path()) {
            Err(Error::LockFailed(_)) => {}
            Ok(_) => panic!("expected LockFailed, got Ok"),
            Err(other) => panic!("expected LockFailed, got {:?}", other),
        }
    }

    #[test]
    fn release_on_drop_allows_reacquire() {
        let dir = TempDir::new().unwrap();
        {
            let _lock = RepoLock::acquire(dir.path()).unwrap();
        } // dropped here -> flock released
        // Re-acquisition after release must succeed.
        let _lock2 = RepoLock::acquire(dir.path()).unwrap();
    }

    #[test]
    fn lock_file_persists_after_release() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join("clone_root.lock");
        {
            let _lock = RepoLock::acquire(dir.path()).unwrap();
            assert!(lock_path.exists());
        }
        // The persistent lock file is never deleted on release.
        assert!(lock_path.exists(), "lock file must persist after release");
    }

    #[test]
    fn holder_diagnostics_written() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join("clone_root.lock");
        let _lock = RepoLock::acquire(dir.path()).unwrap();
        let content = std::fs::read_to_string(&lock_path).unwrap();
        let first = content.lines().next().unwrap();
        assert_eq!(first.trim().parse::<u32>().unwrap(), std::process::id());
    }

    #[test]
    fn cross_process_lock_is_exclusive() {
        // A child process attempting to acquire the lock while the parent holds
        // it must fail. flock is per-open-file-description, so a forked/exec'd
        // process is a genuine second holder.
        let dir = TempDir::new().unwrap();
        let _lock = RepoLock::acquire(dir.path()).unwrap();

        // Spawn a short Rust program is overkill; instead use `flock` semantics
        // via a separate file descriptor opened by a child shell. We rely on the
        // OS `flock(1)` utility being present; skip if it is not.
        let flock_bin = which_flock();
        let Some(flock_bin) = flock_bin else { return };
        let lock_path = dir.path().join("clone_root.lock");
        let status = std::process::Command::new(flock_bin)
            .arg("-n")
            .arg(&lock_path)
            .arg("true")
            .status();
        if let Ok(status) = status {
            assert!(
                !status.success(),
                "child flock should fail while parent holds the lock"
            );
        }
    }

    fn which_flock() -> Option<std::path::PathBuf> {
        for dir in ["/usr/bin", "/bin", "/usr/local/bin"] {
            let p = Path::new(dir).join("flock");
            if p.exists() {
                return Some(p);
            }
        }
        None
    }
}
