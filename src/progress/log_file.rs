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
        Self::start(&std::env::temp_dir(), build_stem(command))
    }

    /// Create the temporary log in `tmp_dir` and start the writer thread.
    ///
    /// The file gets a random name and is created `O_EXCL` with mode `0600`
    /// (design/06 "Before the repository is located"), so concurrent runs
    /// cannot collide and no one can pre-create the name to block logging.
    fn start(tmp_dir: &Path, stem: String) -> std::io::Result<Self> {
        let (file, tmp_path) = tempfile::Builder::new()
            .prefix("omemfs-")
            .suffix(".log")
            .tempfile_in(tmp_dir)?
            .keep()
            .map_err(|e| e.error)?;

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
        let omemfs_dir = repo_dir.join(".omemfs");
        let logs_dir = omemfs_dir.join("logs");
        // `.omemfs/` normally exists already; create it and `logs/` private
        // (0700) in case a fresh directory is what we make here.
        if let Err(e) = create_private_dir_all(&logs_dir) {
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
    let _ = writer.flush();

    // Claim a free final name by creating the file (O_EXCL), so a same-second
    // run of the same command never overwrites an earlier log.
    let final_path = match claim_final_path(logs_dir, stem) {
        Ok(p) => p,
        Err(e) => {
            let _ = writeln!(
                std::io::stderr(),
                "omemfs: could not create log file: {}",
                e
            );
            return;
        }
    };

    // `rename` replaces the placeholder we just created. It fails across
    // filesystems (e.g. WSL /tmp -> NTFS /mnt/c); fall back to copying contents
    // only to avoid fchmod on NTFS.
    if let Err(e) = fs::rename(&*current_path, &final_path) {
        if let Err(e2) = copy_file_contents(&*current_path, &final_path) {
            let _ = writeln!(
                std::io::stderr(),
                "omemfs: could not move log file: {} / {}",
                e,
                e2
            );
            let _ = fs::remove_file(&final_path);
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

/// Create `<stem>.log` in `logs_dir` (mode `0600`, `O_EXCL`), or the first free
/// `<stem>-2.log`, `<stem>-3.log`, ... if the name is taken (design/06 "Final
/// name collisions"). Returns the path of the file it created.
fn claim_final_path(logs_dir: &Path, stem: &str) -> std::io::Result<PathBuf> {
    for n in 1u32..=10_000 {
        let name = if n == 1 {
            format!("{stem}.log")
        } else {
            format!("{stem}-{n}.log")
        };
        let path = logs_dir.join(name);
        match private_file_options().create_new(true).open(&path) {
            Ok(_) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("no free log file name"))
}

fn private_file_options() -> fs::OpenOptions {
    let mut o = fs::OpenOptions::new();
    o.write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o
}

/// `create_dir_all` that creates any missing directory with mode `0700`.
fn create_private_dir_all(path: &Path) -> std::io::Result<()> {
    let mut b = fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(path)
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

    fn names_in(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn temp_log_gets_a_random_name_not_derived_from_the_stem() {
        let tmp = tempfile::TempDir::new().unwrap();
        let a = LogFile::start(tmp.path(), "20260930-120000-push".to_string()).unwrap();
        let b = LogFile::start(tmp.path(), "20260930-120000-push".to_string()).unwrap();
        let names = names_in(tmp.path());
        assert_eq!(names.len(), 2, "same stem must not collide: {names:?}");
        for n in &names {
            assert!(n.starts_with("omemfs-") && n.ends_with(".log"), "{n}");
            assert!(!n.contains("20260930"), "name must not be predictable: {n}");
        }
        a.finish();
        b.finish();
        assert!(names_in(tmp.path()).is_empty(), "unlocated logs are discarded");
    }

    #[test]
    fn a_pre_created_predictable_name_cannot_block_logging() {
        // The old fixed name `omemfs-<stem>.log` could be pre-created (as a
        // directory, say) by another user to make every command fail.
        let tmp = tempfile::TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("omemfs-20260930-120000-push.log")).unwrap();
        let lf = LogFile::start(tmp.path(), "20260930-120000-push".to_string());
        assert!(lf.is_ok(), "{:?}", lf.err());
        lf.unwrap().finish();
    }

    #[cfg(unix)]
    #[test]
    fn temp_log_is_mode_0600_regardless_of_umask() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let lf = crate::test_umask::with_umask(0o000, || {
            LogFile::start(tmp.path(), "20260930-120000-push".to_string()).unwrap()
        });
        let name = names_in(tmp.path()).remove(0);
        let mode = fs::metadata(tmp.path().join(name)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        lf.finish();
    }

    #[test]
    fn relocation_moves_the_log_into_the_repository() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tempfile::TempDir::new().unwrap();
        let lf = LogFile::start(tmp.path(), "20260930-120000-push".to_string()).unwrap();
        lf.write_line("hello");
        lf.set_repo_dir(repo.path());
        lf.write_line("after");
        lf.finish();
        let logs = repo.path().join(".omemfs/logs");
        let body = fs::read_to_string(logs.join("20260930-120000-push.log")).unwrap();
        assert_eq!(body, "hello\nafter\n");
        assert!(names_in(tmp.path()).is_empty(), "temp file must be gone");
    }

    #[test]
    fn relocation_never_overwrites_an_existing_log_of_the_same_name() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tempfile::TempDir::new().unwrap();
        let logs = repo.path().join(".omemfs/logs");
        fs::create_dir_all(&logs).unwrap();
        fs::write(logs.join("20260930-120000-ls.log"), "first\n").unwrap();

        let lf = LogFile::start(tmp.path(), "20260930-120000-ls".to_string()).unwrap();
        lf.write_line("second");
        lf.set_repo_dir(repo.path());
        lf.finish();

        assert_eq!(
            fs::read_to_string(logs.join("20260930-120000-ls.log")).unwrap(),
            "first\n",
            "the earlier log must be untouched"
        );
        assert_eq!(
            fs::read_to_string(logs.join("20260930-120000-ls-2.log")).unwrap(),
            "second\n"
        );
    }

    #[test]
    fn repeated_collisions_get_increasing_suffixes() {
        let logs = tempfile::TempDir::new().unwrap();
        touch(logs.path(), "s-cmd.log");
        touch(logs.path(), "s-cmd-2.log");
        let p = claim_final_path(logs.path(), "s-cmd").unwrap();
        assert_eq!(p.file_name().unwrap(), "s-cmd-3.log");
        assert!(p.exists(), "the name is claimed by creating the file");
        let q = claim_final_path(logs.path(), "s-cmd").unwrap();
        assert_eq!(q.file_name().unwrap(), "s-cmd-4.log");
    }

    #[test]
    fn a_free_name_is_used_as_is() {
        let logs = tempfile::TempDir::new().unwrap();
        let p = claim_final_path(logs.path(), "s-cmd").unwrap();
        assert_eq!(p.file_name().unwrap(), "s-cmd.log");
    }

    #[cfg(unix)]
    #[test]
    fn logs_directory_is_0700_and_log_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tempfile::TempDir::new().unwrap();
        crate::test_umask::with_umask(0o000, || {
            let lf = LogFile::start(tmp.path(), "20260930-120000-push".to_string()).unwrap();
            lf.write_line("x");
            lf.set_repo_dir(repo.path());
            lf.finish();
        });
        let mode_of = |p: std::path::PathBuf| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode_of(repo.path().join(".omemfs")), 0o700);
        assert_eq!(mode_of(repo.path().join(".omemfs/logs")), 0o700);
        assert_eq!(
            mode_of(repo.path().join(".omemfs/logs/20260930-120000-push.log")),
            0o600
        );
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
