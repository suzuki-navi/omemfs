//! Log file writer for progress/debug output.
//!
//! Writes structured log lines to `.omemfs/logs/YYYYMMDD-HHMMSS-{cmd}.log`.
//! While the repository directory is not yet known, a temporary file is used
//! and moved once the repository is located.
//!
//! All public methods are non-blocking: log commands are queued on an `mpsc`
//! channel and processed by a dedicated background writer thread.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;

enum LogCommand {
    Line(String),
    SetRepoDir(PathBuf),
    Shutdown,
}

/// Shared handle to the current log file writer.
///
/// All write operations are non-blocking: commands are queued on an `mpsc`
/// channel and processed by a dedicated background thread.
/// `Clone` is cheap (`Sender<T>` clone + `Arc` clone).
#[derive(Clone)]
pub struct LogFile {
    sender: mpsc::Sender<LogCommand>,
    thread: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl LogFile {
    /// Open a new log file, initially in a temporary directory.
    ///
    /// `command` is the subcommand name (e.g. `"push"`, `"pull"`).
    pub fn new(command: &str) -> std::io::Result<Self> {
        let tmp_dir = std::env::temp_dir();
        let stem = build_stem(command);
        let tmp_path = tmp_dir.join(format!("omemfs-{}.log", stem));
        let file = File::create(&tmp_path)?;

        let (tx, rx) = mpsc::channel::<LogCommand>();
        let handle = std::thread::spawn(move || {
            run_writer(file, tmp_path, stem, rx);
        });

        Ok(Self {
            sender: tx,
            thread: Arc::new(Mutex::new(Some(handle))),
        })
    }

    /// Inform the log file that the repository root is now known.
    ///
    /// The directory is created synchronously (to surface errors on the
    /// calling thread); the actual file move is enqueued and executed by the
    /// background thread, preserving ordering with respect to pending log lines.
    pub fn set_repo_dir(&self, repo_dir: &Path) {
        let logs_dir = repo_dir.join(".omemfs").join("logs");
        if let Err(e) = fs::create_dir_all(&logs_dir) {
            let _ = writeln!(
                std::io::stderr(),
                "omemfs: could not create logs directory: {}",
                e
            );
            return;
        }
        let _ = self.sender.send(LogCommand::SetRepoDir(logs_dir));
    }

    /// Write a single line to the log file (non-blocking).
    pub fn write_line(&self, line: &str) {
        let _ = self.sender.send(LogCommand::Line(line.to_owned()));
    }

    /// Send a shutdown sentinel and wait for the background thread to flush
    /// and exit, guaranteeing all previously enqueued lines are written.
    pub fn finish(&self) {
        let _ = self.sender.send(LogCommand::Shutdown);
        if let Ok(mut guard) = self.thread.lock()
            && let Some(handle) = guard.take()
        {
            let _ = handle.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Background writer thread
// ---------------------------------------------------------------------------

fn run_writer(file: File, initial_path: PathBuf, stem: String, rx: mpsc::Receiver<LogCommand>) {
    let mut writer = BufWriter::new(file);
    let mut current_path = initial_path;
    let mut relocated = false;

    for cmd in rx {
        match cmd {
            LogCommand::Line(line) => {
                let _ = writeln!(writer, "{}", line);
            }
            LogCommand::SetRepoDir(logs_dir) => {
                if !relocated {
                    relocated = true;
                    relocate_log(&mut writer, &mut current_path, &stem, &logs_dir);
                }
            }
            LogCommand::Shutdown => break,
        }
    }
    let _ = writer.flush();

    // If the repository was never located, the log file still lives in the
    // temp directory and would otherwise accumulate as garbage. Discard it
    // (design/06 "Before the repository is located"). When relocated, the temp
    // file was already moved into `.omemfs/logs/` and must be kept.
    if !relocated {
        // Drop the writer (and its file handle) before removing the file so the
        // descriptor is closed first.
        drop(writer);
        let _ = fs::remove_file(&current_path);
    }
}

fn relocate_log(
    writer: &mut BufWriter<File>,
    current_path: &mut PathBuf,
    stem: &str,
    logs_dir: &Path,
) {
    let final_path = logs_dir.join(format!("{}.log", stem));
    let _ = writer.flush();

    if let Err(e) = fs::rename(&*current_path, &final_path) {
        // rename across filesystems fails (e.g. WSL /tmp -> NTFS /mnt/c);
        // fall back to copying contents only to avoid fchmod on NTFS.
        if let Err(e2) = copy_file_contents(&*current_path, &final_path) {
            let _ = writeln!(
                std::io::stderr(),
                "omemfs: could not move log file: {} / {}",
                e,
                e2
            );
            return;
        }
        let _ = fs::remove_file(&*current_path);
    }

    match File::options().append(true).open(&final_path) {
        Ok(f) => {
            *writer = BufWriter::new(f);
            *current_path = final_path.clone();
        }
        Err(e) => {
            let _ = writeln!(
                std::io::stderr(),
                "omemfs: could not reopen log file: {}",
                e
            );
            return;
        }
    }

    update_latest(logs_dir, &final_path);
    sweep_old_logs(logs_dir);
}

/// Maximum number of `.log` files retained in `.omemfs/logs/` (`latest.log`
/// excluded from the count). Without this, one log file per invocation would
/// accumulate forever (refactor-instructions.md G4a; design/06 "Retention").
const MAX_LOG_FILES: usize = 200;

/// Delete the oldest `.log` files in `logs_dir` beyond [`MAX_LOG_FILES`],
/// keeping the newest ones. Filenames sort chronologically by construction
/// (`YYYYMMDD-HHMMSS-{cmd}.log`), so a plain lexicographic sort suffices --
/// no need to stat mtimes. `latest.log` is excluded from the count and never
/// deleted by this sweep (it is a symlink/copy maintained by `update_latest`,
/// not one of the invocation-per-file entries).
///
/// Best-effort: any I/O error here is silently ignored -- this is pure
/// housekeeping and must never fail the command that triggered it.
fn sweep_old_logs(logs_dir: &Path) {
    let Ok(rd) = fs::read_dir(logs_dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.ends_with(".log") && n != "latest.log")
                .unwrap_or(false)
        })
        .collect();
    if entries.len() <= MAX_LOG_FILES {
        return;
    }
    entries.sort();
    let excess = entries.len() - MAX_LOG_FILES;
    for path in &entries[..excess] {
        let _ = fs::remove_file(path);
    }
}

fn copy_file_contents(src: &Path, dst: &Path) -> std::io::Result<()> {
    let mut src_file = fs::File::open(src)?;
    let mut dst_file = fs::File::create(dst)?;
    std::io::copy(&mut src_file, &mut dst_file)?;
    Ok(())
}

fn build_stem(command: &str) -> String {
    let now = chrono::Local::now();
    format!("{}-{}", now.format("%Y%m%d-%H%M%S"), command)
}

fn update_latest(logs_dir: &Path, target: &Path) {
    let latest = logs_dir.join("latest.log");
    let _ = fs::remove_file(&latest);

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let rel = target
            .file_name()
            .map(PathBuf::from)
            .unwrap_or_else(|| target.to_path_buf());
        if symlink(rel, &latest).is_ok() {
            return;
        }
    }
    let _ = fs::copy(target, &latest);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(dir: &Path, name: &str) {
        fs::write(dir.join(name), b"x").unwrap();
    }

    #[test]
    fn sweep_old_logs_is_a_noop_under_the_limit() {
        let tmp = tempfile::TempDir::new().unwrap();
        for i in 0..5 {
            touch(tmp.path(), &format!("2026070{}-000000-push.log", i));
        }
        sweep_old_logs(tmp.path());
        let remaining: Vec<_> = fs::read_dir(tmp.path()).unwrap().collect();
        assert_eq!(
            remaining.len(),
            5,
            "must not delete anything under MAX_LOG_FILES"
        );
    }

    #[test]
    fn sweep_old_logs_keeps_only_the_newest_max_log_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        let total = MAX_LOG_FILES + 10;
        for i in 0..total {
            // Zero-padded so lexicographic order matches numeric/chronological order.
            touch(tmp.path(), &format!("{:06}-push.log", i));
        }
        touch(tmp.path(), "latest.log");

        sweep_old_logs(tmp.path());

        let mut remaining: Vec<String> = fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        remaining.sort();

        // latest.log must survive (excluded from the count and never deleted).
        assert!(remaining.contains(&"latest.log".to_string()));
        // Exactly MAX_LOG_FILES real log files plus latest.log must remain.
        assert_eq!(remaining.len(), MAX_LOG_FILES + 1);
        // The oldest 10 must be gone; the newest MAX_LOG_FILES must survive.
        assert!(
            !remaining.contains(&"000000-push.log".to_string()),
            "oldest file must be deleted"
        );
        assert!(
            remaining.contains(&format!("{:06}-push.log", total - 1)),
            "newest file must survive"
        );
    }
}
