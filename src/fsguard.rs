//! Race-free working-tree path resolution and writes.
//!
//! Every write into the working tree resolves its parent directory
//! component-by-component from a `work_dir` directory fd with
//! openat(O_DIRECTORY|O_NOFOLLOW), then performs the write *at()-relative to
//! that fd. A symlinked component strictly below `work_dir` is therefore
//! impossible to traverse, and there is no check-then-use window: the fd the
//! check produced *is* the fd the write uses.
//! See design/03_sync_model.md, "Write safety: the working-tree write boundary".
//!
//! Error policy: every case fails closed. A symlinked component strictly
//! below `work_dir` (`openat` fails with `ELOOP`/`ENOTDIR`) is
//! `Error::SymlinkEscape`. A `strip_prefix(work_dir)` failure in one of the
//! module-level convenience wrappers is a hard `Error::Other`, never `Ok(())`.
//! A path component that is `.`, `..`, empty, or contains a NUL byte is a
//! validation error. A missing component with `create == false` surfaces as
//! the underlying `Error::Io(NotFound)`. `work_dir` itself failing to open is
//! a hard error -- there is deliberately no "boundary unknown -> permissive"
//! branch anywhere in this module.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use filetime::FileTime;

use crate::error::Error;

/// An owned directory file descriptor whose path was verified
/// component-by-component from `work_dir`, plus the absolute path it
/// corresponds to. `abs` is kept for error messages and best-effort
/// operations only -- it must never be used to re-open the directory by
/// path, which would reintroduce the TOCTOU window this module exists to
/// close.
#[derive(Debug)]
pub struct GuardedDir {
    fd: OwnedFd,
    abs: PathBuf,
}

// ---------------------------------------------------------------------------
// Component / path validation
// ---------------------------------------------------------------------------

/// Reject `.`, `..`, an empty component, or one containing a NUL byte or a
/// `/` separator. Belt-and-braces: `resolve_dir`'s caller already splits on
/// `/`, so this also closes a theoretical lexical `..` escape.
fn validate_component(name: &str) -> Result<(), Error> {
    if name.is_empty() || name == "." || name == ".." || name.contains('\0') || name.contains('/')
    {
        return Err(Error::Other(format!(
            "invalid path component: {:?}",
            name
        )));
    }
    Ok(())
}

fn to_cstring(s: &str) -> Result<CString, Error> {
    CString::new(s)
        .map_err(|_| Error::Other(format!("path component contains a NUL byte: {:?}", s)))
}

fn is_symlink_like(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR))
}

/// Monotonic counter used, together with the pid and a timestamp, to build a
/// unique `.tmp`-prefixed temp filename per write (no external dependency on
/// `rand` needed for this).
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Build a unique staging sibling filename in the reserved `.omemfs-` namespace
/// (design/09 "`.omemfs-tmp-*`"). The name lives in the working tree, so a
/// leftover from an interrupted write must be excluded from every scan rather
/// than pushed as content; a generic `.tmp` prefix would not be.
fn temp_name() -> String {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{}{:x}-{:x}-{:x}",
        crate::object::WORKING_TREE_TMP_PREFIX,
        pid,
        nanos,
        n
    )
}

/// How old a staging file must be before the sweep removes it. Younger ones may
/// belong to a concurrent writer.
const STALE_TMP_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Directories (by device and inode) already swept by this process, so a run
/// that writes many files into one directory sweeps it once, not once per write.
/// Keyed by identity, not by path: the same directory reached through a symlinked
/// `work_dir` is still one directory.
fn sweep_gate() -> &'static std::sync::Mutex<std::collections::HashSet<(u64, u64)>> {
    static GATE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<(u64, u64)>>> =
        std::sync::OnceLock::new();
    GATE.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

// ---------------------------------------------------------------------------
// Raw openat/mkdirat helpers
// ---------------------------------------------------------------------------

/// openat(dirfd, name, O_DIRECTORY|O_NOFOLLOW): open a subdirectory of
/// `dirfd` without following a symlink at `name`. Fails with `ELOOP` (name is
/// a symlink) or `ENOTDIR` (name exists but is not a directory at all).
fn open_subdir_raw(dirfd: RawFd, name: &CString) -> io::Result<OwnedFd> {
    // SAFETY: `dirfd` is a valid, open directory fd borrowed from the
    // caller's `GuardedDir` for the duration of this call. `name` is a
    // NUL-terminated component containing no `/` (validated by the caller),
    // so this cannot resolve outside `dirfd`. O_NOFOLLOW makes the call fail
    // rather than dereference a symlink at `name`.
    let fd = unsafe {
        libc::openat(
            dirfd,
            name.as_ptr(),
            libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_RDONLY | libc::O_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` was just returned by a successful `openat()` call above
    // and is not owned by anything else yet.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// mkdirat(dirfd, name, mode), tolerating `EEXIST`.
fn mkdirat_raw(dirfd: RawFd, name: &CString, mode: libc::mode_t) -> io::Result<()> {
    // SAFETY: `dirfd` is a valid open directory fd for the duration of this
    // call; `name` is a NUL-terminated, `/`-free component.
    let ret = unsafe { libc::mkdirat(dirfd, name.as_ptr(), mode) };
    if ret != 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EEXIST) {
            return Ok(());
        }
        return Err(e);
    }
    Ok(())
}

/// Open `work_dir` itself. `work_dir` and everything ABOVE it may
/// legitimately be a symlink or a mount point, so this deliberately follows
/// symlinks when opening `work_dir`; only components strictly below it are
/// checked by `descend`.
fn open_root(work_dir: &Path) -> Result<GuardedDir, Error> {
    let c_path = CString::new(work_dir.as_os_str().as_bytes()).map_err(|_| {
        Error::Other(format!(
            "work_dir path contains a NUL byte: {}",
            work_dir.display()
        ))
    })?;
    // SAFETY: opening `work_dir` itself deliberately follows symlinks (see
    // this function's doc comment); `c_path` is a valid NUL-terminated C
    // string for the duration of this call.
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_DIRECTORY | libc::O_RDONLY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(Error::Io(io::Error::last_os_error()));
    }
    // SAFETY: `fd` was just returned by a successful `open()` call above and
    // is not owned by anything else yet.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    Ok(GuardedDir {
        fd: owned,
        abs: work_dir.to_path_buf(),
    })
}

/// Descend one component below `parent`. `create` mkdirat()s a missing
/// component (tolerating a concurrent creator's `EEXIST`) before retrying the
/// open -- this is the guarded replacement for `fs::create_dir_all` one level
/// at a time. A symlink (or any non-directory) at `name` is refused with
/// `Error::SymlinkEscape` in every case, including the just-created-then-
/// raced-with-a-symlink-swap retry path, so nothing is ever created *inside*
/// a symlinked target: `open_subdir_raw`'s `O_NOFOLLOW` fails before ever
/// entering it.
fn descend(parent: &GuardedDir, name: &str, create: bool) -> Result<GuardedDir, Error> {
    validate_component(name)?;
    let c_name = to_cstring(name)?;
    let dirfd = parent.fd.as_raw_fd();
    match open_subdir_raw(dirfd, &c_name) {
        Ok(fd) => Ok(GuardedDir {
            fd,
            abs: parent.abs.join(name),
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound && create => {
            mkdirat_raw(dirfd, &c_name, 0o755).map_err(Error::Io)?;
            match open_subdir_raw(dirfd, &c_name) {
                Ok(fd) => Ok(GuardedDir {
                    fd,
                    abs: parent.abs.join(name),
                }),
                Err(e2) if is_symlink_like(&e2) => Err(Error::SymlinkEscape(
                    parent.abs.join(name).display().to_string(),
                )),
                Err(e2) => Err(Error::Io(e2)),
            }
        }
        Err(e) if is_symlink_like(&e) => Err(Error::SymlinkEscape(
            parent.abs.join(name).display().to_string(),
        )),
        Err(e) => Err(Error::Io(e)),
    }
}

/// Resolve `rel` (work_dir-relative, '/'-separated; "" == work_dir itself).
/// `create` mkdirat()s a missing component (tolerating EEXIST) -- this is the
/// guarded replacement for `fs::create_dir_all` on a working-tree path.
fn resolve_dir(work_dir: &Path, rel: &str, create: bool) -> Result<GuardedDir, Error> {
    let mut current = open_root(work_dir)?;
    if rel.is_empty() {
        return Ok(current);
    }
    for component in rel.split('/') {
        current = descend(&current, component, create)?;
    }
    Ok(current)
}

/// Split `rel` into (verified parent dir, leaf name).
pub fn open_leaf(
    work_dir: &Path,
    rel: &str,
    create_parents: bool,
) -> Result<(GuardedDir, String), Error> {
    let rel = rel.trim_end_matches('/');
    let (dir_rel, leaf) = match rel.rfind('/') {
        Some(idx) => (&rel[..idx], &rel[idx + 1..]),
        None => ("", rel),
    };
    validate_component(leaf)?;
    let dir = resolve_dir(work_dir, dir_rel, create_parents)?;
    Ok((dir, leaf.to_string()))
}

// ---------------------------------------------------------------------------
// GuardedDir operations
// ---------------------------------------------------------------------------

impl GuardedDir {
    /// The absolute path of `name` inside this directory, for error messages
    /// and best-effort operations only -- never for re-opening.
    pub fn abs(&self, name: &str) -> PathBuf {
        self.abs.join(name)
    }

    fn unlinkat(&self, name: &str) -> Result<(), Error> {
        let c_name = to_cstring(name)?;
        // SAFETY: `self.fd` is a valid open directory fd owned by this
        // `GuardedDir`; `c_name` is a validated, `/`-free component.
        let ret = unsafe { libc::unlinkat(self.fd.as_raw_fd(), c_name.as_ptr(), 0) };
        if ret != 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        Ok(())
    }

    fn rmdir(&self, name: &str) -> Result<(), Error> {
        let c_name = to_cstring(name)?;
        // SAFETY: as above; AT_REMOVEDIR requests directory-only removal.
        let ret =
            unsafe { libc::unlinkat(self.fd.as_raw_fd(), c_name.as_ptr(), libc::AT_REMOVEDIR) };
        if ret != 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        Ok(())
    }

    fn renameat_within(&self, old_name: &str, new_name: &str) -> Result<(), Error> {
        let c_old = to_cstring(old_name)?;
        let c_new = to_cstring(new_name)?;
        let dirfd = self.fd.as_raw_fd();
        // SAFETY: `dirfd` is valid for the duration of this call; both names
        // are validated, `/`-free components resolved relative to it, so the
        // rename cannot escape this directory.
        let ret = unsafe { libc::renameat(dirfd, c_old.as_ptr(), dirfd, c_new.as_ptr()) };
        if ret != 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        Ok(())
    }

    fn symlinkat(&self, target: &str, name: &str) -> Result<(), Error> {
        // `target` is the symlink's textual target, not a component of this
        // directory's own path -- it may contain `/` and is not validated as
        // a component, only checked for an embedded NUL.
        let c_target = to_cstring(target)?;
        let c_name = to_cstring(name)?;
        // SAFETY: `self.fd` is a valid open directory fd; `c_name` is a
        // validated, `/`-free component resolved relative to it.
        let ret = unsafe {
            libc::symlinkat(c_target.as_ptr(), self.fd.as_raw_fd(), c_name.as_ptr())
        };
        if ret != 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        Ok(())
    }

    fn sync_dir(&self) -> Result<(), Error> {
        // SAFETY: `self.fd` is a valid open directory fd owned by this
        // `GuardedDir`.
        let ret = unsafe { libc::fsync(self.fd.as_raw_fd()) };
        if ret != 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        Ok(())
    }

    fn open_tmp_file(&self, name: &str) -> Result<std::fs::File, Error> {
        let c_name = to_cstring(name)?;
        // SAFETY: `self.fd` is a valid open directory fd; `c_name` is a
        // NUL-terminated, `/`-free temp filename generated by this module
        // (never user-controlled). O_CREAT|O_EXCL|O_NOFOLLOW guarantees this
        // creates a brand-new regular file rather than following or reusing
        // anything that might already exist at that name.
        let fd = unsafe {
            libc::openat(
                self.fd.as_raw_fd(),
                c_name.as_ptr(),
                libc::O_CREAT
                    | libc::O_EXCL
                    | libc::O_WRONLY
                    | libc::O_NOFOLLOW
                    | libc::O_CLOEXEC,
                0o600_u32,
            )
        };
        if fd < 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        // SAFETY: `fd` was just returned by a successful `openat()` call
        // above and is not owned by anything else yet.
        Ok(unsafe { std::fs::File::from_raw_fd(fd) })
    }

    /// (device, inode) of this directory itself, via fstat on the verified fd.
    fn identity(&self) -> Result<(u64, u64), Error> {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `st` is zero-initialised stack storage for `fstat` to fill in
        // and `self.fd` is a valid open directory fd.
        let ret = unsafe { libc::fstat(self.fd.as_raw_fd(), &mut st) };
        if ret != 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        Ok((st.st_dev as u64, st.st_ino as u64))
    }

    /// Remove `.omemfs-tmp-*` staging leftovers in this directory that are at
    /// least `max_age` old (design/09 "`.omemfs-tmp-*`"). Only regular files and
    /// symlinks are removed, and a symlink is unlinked, never followed;
    /// directories and every other name are left alone. Best-effort per entry:
    /// one that cannot be inspected or removed is skipped.
    fn sweep_stale_tmp(&self, max_age: std::time::Duration) -> Result<(), Error> {
        let now = std::time::SystemTime::now();
        for (name, _d_type) in self.list_entries()? {
            if !crate::object::is_working_tree_tmp(&name) {
                continue;
            }
            let c_name = to_cstring(&name)?;
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            // SAFETY: as `lstat_mode`: zeroed storage, valid dir fd, validated
            // `/`-free component. AT_SYMLINK_NOFOLLOW inspects a symlink itself.
            let ret = unsafe {
                libc::fstatat(
                    self.fd.as_raw_fd(),
                    c_name.as_ptr(),
                    &mut st,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if ret != 0 {
                continue;
            }
            let fmt = st.st_mode as u32 & libc::S_IFMT as u32;
            if fmt != libc::S_IFREG as u32 && fmt != libc::S_IFLNK as u32 {
                continue;
            }
            let mtime = std::time::UNIX_EPOCH
                + std::time::Duration::new(st.st_mtime.max(0) as u64, st.st_mtime_nsec.max(0) as u32);
            match now.duration_since(mtime) {
                Ok(age) if age >= max_age => {
                    let _ = self.unlinkat(&name);
                }
                _ => {} // fresh, or mtime in the future: treat as live
            }
        }
        Ok(())
    }

    /// Sweep this directory once per process (see [`sweep_gate`]). Never fails
    /// the write that triggered it: housekeeping must not break a real write.
    fn sweep_stale_tmp_once(&self) {
        let Ok(id) = self.identity() else { return };
        let first = sweep_gate()
            .lock()
            .map(|mut g| g.insert(id))
            .unwrap_or(false);
        if first {
            let _ = self.sweep_stale_tmp(STALE_TMP_MAX_AGE);
        }
    }

    /// openat(O_CREAT|O_EXCL|O_WRONLY|O_NOFOLLOW) a reserved-name staging sibling
    /// of `name` in this directory, run `write_fn` against it, then
    /// renameat() it onto `name`. No fsync -- mirrors
    /// `store::local::atomic_write_with_no_fsync`. On any error from
    /// `write_fn` (or the rename), the temp file is removed and no partial
    /// content is ever visible at `name`.
    pub fn atomic_write_with<F>(&self, name: &str, write_fn: F) -> Result<(), Error>
    where
        F: FnOnce(&mut dyn std::io::Write) -> Result<(), Error>,
    {
        validate_component(name)?;
        self.sweep_stale_tmp_once();
        let tmp = temp_name();
        let file = self.open_tmp_file(&tmp)?;
        let result: Result<(), Error> = (|| {
            let mut writer = std::io::BufWriter::new(&file);
            write_fn(&mut writer)?;
            std::io::Write::flush(&mut writer).map_err(Error::Io)?;
            Ok(())
        })();
        drop(file);
        if let Err(e) = result {
            let _ = self.unlinkat(&tmp);
            return Err(e);
        }
        if let Err(e) = self.renameat_within(&tmp, name) {
            let _ = self.unlinkat(&tmp);
            return Err(e);
        }
        Ok(())
    }

    /// As above, plus fsync(file) + fsync(dir) -- mirrors
    /// `store::local::atomic_write`.
    pub fn atomic_write_durable(&self, name: &str, data: &[u8]) -> Result<(), Error> {
        validate_component(name)?;
        self.sweep_stale_tmp_once();
        let tmp = temp_name();
        let file = self.open_tmp_file(&tmp)?;
        let result: Result<(), Error> = (|| {
            use std::io::Write as _;
            (&file).write_all(data).map_err(Error::Io)?;
            file.sync_all().map_err(Error::Io)?;
            Ok(())
        })();
        drop(file);
        if let Err(e) = result {
            let _ = self.unlinkat(&tmp);
            return Err(e);
        }
        if let Err(e) = self.renameat_within(&tmp, name) {
            let _ = self.unlinkat(&tmp);
            return Err(e);
        }
        // Flush the directory entry so the rename survives a power failure.
        self.sync_dir()
    }

    /// symlinkat to a ".tmp"-prefixed sibling + renameat (replaces
    /// `write_symlink_atomic`'s body). If `name` currently exists as a
    /// (assumed-empty) directory, it is removed first with a best-effort
    /// `rmdir` -- a plain `renameat` cannot replace an existing directory
    /// with a symlink. Per-entry deletions for the directory's own contents
    /// are the caller's responsibility, exactly as before.
    pub fn replace_symlink(&self, name: &str, target: &str) -> Result<(), Error> {
        validate_component(name)?;
        if let Ok(mode) = self.lstat_mode(name)
            && (mode & libc::S_IFMT as u32) == libc::S_IFDIR as u32
        {
            let _ = self.rmdir(name);
        }
        self.sweep_stale_tmp_once();
        let tmp = temp_name();
        self.symlinkat(target, &tmp)?;
        if let Err(e) = self.renameat_within(&tmp, name) {
            let _ = self.unlinkat(&tmp);
            return Err(e);
        }
        Ok(())
    }

    /// unlinkat, tolerating ENOENT (`NotFound`) only. Deliberately does
    /// *not* also tolerate `EISDIR`: at every current call site, `name` is
    /// expected to be a genuine file, so a directory turning up there
    /// instead is a real-bug signal that must propagate, not be papered
    /// over. A caller that expects "delete a file, or possibly a stale
    /// directory left by a type change" (L7's `restore::DeleteAbsent`
    /// handler is the one such case in this codebase) must check the
    /// on-disk type itself *before* calling this, and handle a directory
    /// there explicitly -- never by loosening this helper's tolerance.
    pub fn remove_file_tolerant(&self, name: &str) -> Result<(), Error> {
        validate_component(name)?;
        match self.unlinkat(name) {
            Ok(()) => Ok(()),
            Err(Error::Io(e)) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// mkdirat, tolerating EEXIST.
    pub fn create_dir(&self, name: &str) -> Result<(), Error> {
        validate_component(name)?;
        let c_name = to_cstring(name)?;
        mkdirat_raw(self.fd.as_raw_fd(), &c_name, 0o755).map_err(Error::Io)
    }

    /// fstatat(AT_SYMLINK_NOFOLLOW): the mode bits of `name` itself, without
    /// following a symlink there.
    fn lstat_mode(&self, name: &str) -> Result<u32, Error> {
        let c_name = to_cstring(name)?;
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `st` is zero-initialized stack storage for `fstatat` to
        // fill in; `self.fd` is a valid open directory fd and `c_name` is a
        // validated, `/`-free component.
        let ret = unsafe {
            libc::fstatat(
                self.fd.as_raw_fd(),
                c_name.as_ptr(),
                &mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if ret != 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        Ok(st.st_mode as u32)
    }

    /// fstatat: the mode bits of whatever `name` resolves to (following a
    /// symlink there, matching `fchmodat`'s own follow semantics below --
    /// Linux has no way to `chmod` a symlink itself).
    fn stat_mode(&self, name: &str) -> Result<u32, Error> {
        let c_name = to_cstring(name)?;
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: as `lstat_mode` above, but following a symlink at `name`.
        let ret = unsafe { libc::fstatat(self.fd.as_raw_fd(), c_name.as_ptr(), &mut st, 0) };
        if ret != 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        Ok(st.st_mode as u32 & 0o7777)
    }

    /// Apply the executable-bit `mode` from a tree entry / stub record to
    /// `name`, relative to this already-verified directory -- the guarded
    /// counterpart of `fsmeta::apply_mode` for the "mode-only, no content
    /// write" case (N2, design/03_sync_model.md). Same semantics: `Some(_)`
    /// sets the execute bits (`perms | 0o111`), `None` clears them
    /// (`perms & !0o111`); a no-op chmod (bits already correct) is skipped.
    pub fn set_mode(&self, name: &str, mode: &Option<String>) -> Result<(), Error> {
        validate_component(name)?;
        let cur = self.stat_mode(name)?;
        let new = if mode.is_some() {
            cur | 0o111
        } else {
            cur & !0o111
        };
        if new != cur {
            let c_name = to_cstring(name)?;
            // SAFETY: `self.fd` is a valid open directory fd; `c_name` is a
            // validated, `/`-free component; `new` is a valid mode value.
            let ret = unsafe {
                libc::fchmodat(self.fd.as_raw_fd(), c_name.as_ptr(), new as libc::mode_t, 0)
            };
            if ret != 0 {
                return Err(Error::Io(io::Error::last_os_error()));
            }
        }
        Ok(())
    }

    /// utimensat: set `name`'s mtime (following a symlink there, matching
    /// `filetime::set_file_mtime`'s own behaviour), leaving atime untouched.
    pub fn set_mtime(&self, name: &str, mtime: FileTime) -> Result<(), Error> {
        validate_component(name)?;
        let c_name = to_cstring(name)?;
        let times = [
            libc::timespec {
                tv_sec: 0,
                tv_nsec: libc::UTIME_OMIT,
            },
            libc::timespec {
                tv_sec: mtime.unix_seconds() as libc::time_t,
                tv_nsec: mtime.nanoseconds() as i64,
            },
        ];
        // SAFETY: `self.fd` is a valid open directory fd; `c_name` is a
        // validated, `/`-free component; `times` is a valid 2-element array
        // as `utimensat(2)` expects.
        let ret =
            unsafe { libc::utimensat(self.fd.as_raw_fd(), c_name.as_ptr(), times.as_ptr(), 0) };
        if ret != 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        Ok(())
    }

    /// Duplicate this directory's fd for handoff to `fdopendir` -- `fdopendir`
    /// takes ownership of (and eventually `closedir`s) the fd it is given, but
    /// `self.fd` must remain usable by this `GuardedDir` afterwards, so a
    /// dup'd fd is handed over instead of the original.
    fn dup_fd(&self) -> io::Result<OwnedFd> {
        // SAFETY: `self.fd` is a valid open fd for the duration of this call.
        // F_DUPFD_CLOEXEC duplicates it as a new, independent, close-on-exec
        // fd; the two fds share the same underlying open file description
        // (and thus directory position), but that is fine here because the
        // dup is immediately handed to `fdopendir`, whose own read position
        // is used exclusively by the walk below and never interleaved with
        // reads via `self.fd`.
        let new_fd = unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if new_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `new_fd` was just returned by a successful `fcntl` call
        // above and is not owned by anything else yet.
        Ok(unsafe { OwnedFd::from_raw_fd(new_fd) })
    }

    /// List this directory's entries (name + `d_type`), skipping `.`/`..`.
    /// `d_type` may be `DT_UNKNOWN` on some filesystems; callers that need a
    /// reliable type must fall back to `lstat_mode`.
    fn list_entries(&self) -> Result<Vec<(String, u8)>, Error> {
        let dup = self.dup_fd().map_err(Error::Io)?;
        // SAFETY: `fdopendir` takes ownership of `dup`'s raw fd on success;
        // `dup` is a valid, freshly-duplicated directory fd. `into_raw_fd`
        // hands off ownership so it is not also closed by `OwnedFd`'s drop.
        let dirp = unsafe { libc::fdopendir(std::os::fd::IntoRawFd::into_raw_fd(dup)) };
        if dirp.is_null() {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        let mut out = Vec::new();
        let result: Result<(), Error> = (|| {
            loop {
                // readdir() does not clear errno on end-of-directory, so
                // errno must be reset first to distinguish "no more
                // entries" (NULL, errno unchanged) from a genuine error
                // (NULL, errno set) per readdir(3).
                // SAFETY: `__errno_location` returns a valid pointer to the
                // calling thread's errno.
                unsafe {
                    *libc::__errno_location() = 0;
                }
                // SAFETY: `dirp` was successfully opened above and is not
                // used concurrently from any other thread.
                let entry = unsafe { libc::readdir(dirp) };
                if entry.is_null() {
                    let e = io::Error::last_os_error();
                    if e.raw_os_error() == Some(0) {
                        break; // end of directory
                    }
                    return Err(Error::Io(e));
                }
                // SAFETY: `entry` is a valid, non-null pointer returned by
                // `readdir` above, whose lifetime is at least until the next
                // `readdir`/`closedir` call on `dirp`.
                let d_type = unsafe { (*entry).d_type };
                // SAFETY: `d_name` is a NUL-terminated array within `*entry`,
                // valid for the duration of this call.
                let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }
                    .to_string_lossy()
                    .into_owned();
                if name == "." || name == ".." {
                    continue;
                }
                out.push((name, d_type));
            }
            Ok(())
        })();
        // SAFETY: `dirp` was successfully opened above and is closed exactly
        // once here, regardless of whether the loop above succeeded.
        unsafe {
            libc::closedir(dirp);
        }
        result?;
        Ok(out)
    }

    /// Recursively remove every entry in this directory (files, symlinks,
    /// and subdirectories with their own contents) EXCEPT those named in
    /// `keep`, leaving this directory itself in place. An entry whose name
    /// is in `keep` is left completely untouched -- neither removed nor
    /// descended into, regardless of whether it is a file, directory, or
    /// symlink -- so a caller can protect a marker file it just wrote (e.g.
    /// `omemfs stub`'s `.omemfs-stub`) from being swept up by this same
    /// call. Any symlink encountered -- at the top level or at any depth of
    /// the recursion -- aborts with `Error::SymlinkEscape` before it is ever
    /// removed or traversed; entries already processed before the symlink
    /// was found remain removed (this is a best-effort cleanup, not a
    /// transaction), but nothing beyond the symlink is touched, and the
    /// symlink entry itself is left in place.
    fn remove_contents_all(&self, keep: &[&str]) -> Result<(), Error> {
        for (name, d_type) in self.list_entries()? {
            if keep.contains(&name.as_str()) {
                continue;
            }

            // DT_UNKNOWN can occur on some filesystems (notably some FUSE
            // and network filesystems); fall back to an authoritative
            // lstat in that case rather than guessing.
            let is_symlink = if d_type == libc::DT_UNKNOWN {
                (self.lstat_mode(&name)? & libc::S_IFMT as u32) == libc::S_IFLNK as u32
            } else {
                d_type == libc::DT_LNK
            };
            if is_symlink {
                return Err(Error::SymlinkEscape(self.abs(&name).display().to_string()));
            }

            let is_dir = if d_type == libc::DT_UNKNOWN {
                (self.lstat_mode(&name)? & libc::S_IFMT as u32) == libc::S_IFDIR as u32
            } else {
                d_type == libc::DT_DIR
            };

            if is_dir {
                // descend() with create=false opens the child with
                // O_NOFOLLOW, giving a second, race-free confirmation that
                // it is still a real directory (not a symlink swapped in
                // between list_entries() and here) before recursing. A
                // `keep`-listed entry never reaches this point (skipped
                // above), so nothing in `keep` is ever descended into
                // either, even if it happens to be a directory.
                let child = descend(self, &name, false)?;
                child.remove_contents_all(&[])?;
                self.rmdir(&name)?;
            } else {
                self.unlinkat(&name)?;
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Convenience wrappers -- most call sites become one-liners that only gain a
// `work_dir` argument. `abs_path` is converted to `rel` via
// strip_prefix(work_dir); a strip_prefix failure is a HARD ERROR (never a
// pass -- this closes the third fail-open path in the old guard).
// ---------------------------------------------------------------------------

fn to_rel_string(work_dir: &Path, abs: &Path) -> Result<String, Error> {
    let rel = abs.strip_prefix(work_dir).map_err(|_| {
        Error::Other(format!(
            "{} is not inside the working tree {}",
            abs.display(),
            work_dir.display()
        ))
    })?;
    // `\` is an ordinary Linux filename byte (design/01_object_model.md
    // invariant #5), not a separator -- it must not be rewritten to `/`,
    // which would turn one path component into two.
    Ok(rel.to_string_lossy().into_owned())
}

pub fn create_dir_all(work_dir: &Path, abs_dir: &Path) -> Result<(), Error> {
    let rel = to_rel_string(work_dir, abs_dir)?;
    resolve_dir(work_dir, &rel, true)?;
    Ok(())
}

pub fn atomic_write_with<F>(work_dir: &Path, abs_path: &Path, f: F) -> Result<(), Error>
where
    F: FnOnce(&mut dyn std::io::Write) -> Result<(), Error>,
{
    let rel = to_rel_string(work_dir, abs_path)?;
    let (dir, leaf) = open_leaf(work_dir, &rel, true)?;
    dir.atomic_write_with(&leaf, f)
}

pub fn atomic_write_durable(work_dir: &Path, abs_path: &Path, data: &[u8]) -> Result<(), Error> {
    let rel = to_rel_string(work_dir, abs_path)?;
    let (dir, leaf) = open_leaf(work_dir, &rel, true)?;
    dir.atomic_write_durable(&leaf, data)
}

pub fn write_symlink(work_dir: &Path, abs_path: &Path, target: &str) -> Result<(), Error> {
    let rel = to_rel_string(work_dir, abs_path)?;
    let (dir, leaf) = open_leaf(work_dir, &rel, true)?;
    dir.replace_symlink(&leaf, target)
}

pub fn remove_file_tolerant(work_dir: &Path, abs_path: &Path) -> Result<(), Error> {
    let rel = to_rel_string(work_dir, abs_path)?;
    match open_leaf(work_dir, &rel, false) {
        Ok((dir, leaf)) => dir.remove_file_tolerant(&leaf),
        Err(Error::Io(e)) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Answers "is `abs_path` a real, existing, symlink-free directory below
/// `work_dir`?" -- the guarded replacement for `Path::is_dir()` wherever a
/// caller uses the answer to decide whether to recurse into and clear out a
/// directory (design/03_sync_model.md, "Write safety: the working-tree write
/// boundary": "The boundary covers *deciding* whether to delete, not just
/// the delete call itself").
///
/// Every component from `work_dir` down to (and including) `abs_path` itself
/// is verified to be symlink-free, exactly like `resolve_dir`/`descend`. The
/// three possible outcomes are:
/// - `Ok(true)`: `abs_path` exists and is a real directory.
/// - `Ok(false)`: `abs_path` (or an ancestor strictly below `work_dir`) does
///   not exist, or `abs_path` exists but is not a directory (e.g. a plain
///   file). This is deliberately not an error: a caller such as `omemfs
///   stub` needs to tell "nothing to stub-as-a-directory here" apart from
///   "something is trying to escape the boundary", and only the latter is a
///   `SymlinkEscape`.
/// - `Err(Error::SymlinkEscape(_))`: some component strictly below `work_dir`
///   -- an intermediate directory, or `abs_path` itself -- is a symlink.
///
/// `work_dir` itself (`abs_path == work_dir`) is exempt from the symlink
/// check, same as everywhere else in this module, and is always reported as
/// a real directory.
pub fn is_real_dir(work_dir: &Path, abs_path: &Path) -> Result<bool, Error> {
    let rel = to_rel_string(work_dir, abs_path)?;
    if rel.is_empty() {
        // abs_path == work_dir: exempt from the symlink check (work_dir and
        // its ancestors may legitimately be a symlink/mount point), and
        // open_root already requires it to be a directory to succeed.
        return Ok(true);
    }
    let (dir_rel, leaf) = match rel.rfind('/') {
        Some(idx) => (&rel[..idx], &rel[idx + 1..]),
        None => ("", rel.as_str()),
    };
    validate_component(leaf)?;
    let parent = match resolve_dir(work_dir, dir_rel, false) {
        Ok(d) => d,
        // A missing ancestor directory means abs_path cannot exist either --
        // "nothing to delete here", not an error.
        Err(Error::Io(e)) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    // Deliberately use `lstat_mode` (AT_SYMLINK_NOFOLLOW) rather than
    // `descend`: `descend`'s ENOTDIR-is-symlink-like conflation is correct
    // for a write that needs *some* directory to continue into, but here a
    // plain file at `leaf` is a legitimate `Ok(false)`, not a `SymlinkEscape`
    // -- only an actual symlink at `leaf` is.
    match parent.lstat_mode(leaf) {
        Ok(mode) => {
            let file_type = mode & libc::S_IFMT as u32;
            if file_type == libc::S_IFLNK as u32 {
                Err(Error::SymlinkEscape(parent.abs(leaf).display().to_string()))
            } else {
                Ok(file_type == libc::S_IFDIR as u32)
            }
        }
        Err(Error::Io(e)) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Recursively delete every entry inside `abs_dir` EXCEPT those named in
/// `keep`, leaving `abs_dir` itself in place (the `omemfs stub`
/// directory-stub use case only ever wants the directory emptied, never
/// removed -- the `.omemfs-stub` marker is written into it *before* this is
/// called, and must be named in `keep` so this call does not delete the very
/// marker it is meant to leave behind). An entry named in `keep` is left
/// completely untouched at the top level of `abs_dir` -- neither removed nor
/// descended into. Every component from `work_dir` down to (and including)
/// `abs_dir`, and every symlink discovered while recursing through its
/// contents, is refused with `Error::SymlinkEscape`: unlike
/// `remove_file_tolerant`, this is not "tolerant" of anything it finds --
/// finding a symlink at all, anywhere under `abs_dir`, aborts the whole
/// operation and leaves that symlink (and everything else) untouched.
///
/// A missing `abs_dir` is an `Error::Io` (`NotFound`), matching
/// `std::fs::remove_dir_all`'s own behaviour: every current call site only
/// calls this right after `is_real_dir` has confirmed the directory exists,
/// so a NotFound here would signal a genuine race worth surfacing rather
/// than a "nothing to do" case to swallow silently.
pub fn remove_dir_all(work_dir: &Path, abs_dir: &Path, keep: &[&str]) -> Result<(), Error> {
    let rel = to_rel_string(work_dir, abs_dir)?;
    let dir = resolve_dir(work_dir, &rel, false)?;
    dir.remove_contents_all(keep)
}

/// Remove `abs_dir` if it exists, is a real (symlink-free) directory, and is
/// empty. Every component from `work_dir` down to (and including) `abs_dir`
/// is verified symlink-free by `resolve_dir`/`rmdir`'s underlying `openat`
/// calls, so this never follows a symlinked ancestor to remove a directory
/// outside `work_dir`.
///
/// Tolerant by design (mirroring `remove_file_tolerant`'s "nothing to do is
/// not an error" policy): a missing `abs_dir`, `abs_dir == work_dir`, or a
/// non-empty directory all return `Ok(false)` rather than an error -- the
/// caller (conflict-resolution directory pruning, design/03_sync_model.md
/// "Pruning directories emptied by conflict resolution") walks an ancestor
/// chain and simply stops once a directory turns out to still have content,
/// which is the expected, non-exceptional way that walk ends. A `abs_dir`
/// that is not a directory at all (e.g. a plain file) is likewise `Ok(false)`
/// -- there is nothing this function is asked to remove. Returns `Ok(true)`
/// only when `abs_dir` itself was actually removed.
pub fn remove_dir_if_empty(work_dir: &Path, abs_dir: &Path) -> Result<bool, Error> {
    let rel = to_rel_string(work_dir, abs_dir)?;
    if rel.is_empty() {
        // Never remove work_dir itself.
        return Ok(false);
    }
    let (dir_rel, leaf) = match rel.rfind('/') {
        Some(idx) => (&rel[..idx], &rel[idx + 1..]),
        None => ("", rel.as_str()),
    };
    validate_component(leaf)?;
    let parent = match resolve_dir(work_dir, dir_rel, false) {
        Ok(d) => d,
        // A missing ancestor directory means abs_dir cannot exist either.
        Err(Error::Io(e)) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    // Confirm `leaf` is still a real, symlink-free directory (not a file, and
    // not a symlink swapped in since the caller last looked) before removing
    // it -- the same lstat-based check `is_real_dir` uses, so a symlink at
    // `leaf` is refused rather than silently skipped or followed.
    match parent.lstat_mode(leaf) {
        Ok(mode) => {
            let file_type = mode & libc::S_IFMT as u32;
            if file_type == libc::S_IFLNK as u32 {
                return Err(Error::SymlinkEscape(parent.abs(leaf).display().to_string()));
            }
            if file_type != libc::S_IFDIR as u32 {
                return Ok(false);
            }
        }
        Err(Error::Io(e)) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    }
    match parent.rmdir(leaf) {
        Ok(()) => Ok(true),
        // ENOTEMPTY (via io::ErrorKind::DirectoryNotEmpty, or its raw errno
        // on platforms/Rust versions where that ErrorKind variant is not yet
        // classified) means there is still content in it -- expected, not an
        // error. A concurrent removal (NotFound) is equally a non-error: the
        // directory is gone either way.
        Err(Error::Io(e))
            if e.kind() == io::ErrorKind::NotFound
                || e.kind() == io::ErrorKind::DirectoryNotEmpty
                || e.raw_os_error() == Some(libc::ENOTEMPTY) =>
        {
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    #[cfg(unix)]
    fn resolve_dir_rejects_symlinked_intermediate_create_true() {
        let work_dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), work_dir.path().join("sub")).unwrap();

        let err = resolve_dir(work_dir.path(), "sub/child", true).unwrap_err();
        assert!(matches!(err, Error::SymlinkEscape(_)), "got {err:?}");
        assert!(
            fs::read_dir(outside.path()).unwrap().next().is_none(),
            "nothing must be created inside the symlink target"
        );
    }

    #[test]
    #[cfg(unix)]
    fn resolve_dir_rejects_symlinked_intermediate_create_false() {
        let work_dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), work_dir.path().join("sub")).unwrap();

        let err = resolve_dir(work_dir.path(), "sub/child", false).unwrap_err();
        assert!(matches!(err, Error::SymlinkEscape(_)), "got {err:?}");
        assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
    }

    #[test]
    #[cfg(unix)]
    fn resolve_dir_rejects_symlink_two_levels_deep() {
        // The symlink sits below a real directory ("a"); the guard must catch
        // it at "a/b" even though "a" itself is fine, and must not create
        // anything inside the symlink target ("c").
        let work_dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::create_dir_all(work_dir.path().join("a")).unwrap();
        std::os::unix::fs::symlink(outside.path(), work_dir.path().join("a/b")).unwrap();

        let err = resolve_dir(work_dir.path(), "a/b/c", true).unwrap_err();
        assert!(matches!(err, Error::SymlinkEscape(_)), "got {err:?}");
        assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
    }

    #[test]
    fn resolve_dir_creates_real_nested_dirs() {
        let work_dir = TempDir::new().unwrap();
        let dir = resolve_dir(work_dir.path(), "a/b/c", true).unwrap();
        assert!(work_dir.path().join("a/b/c").is_dir());
        assert_eq!(dir.abs(""), work_dir.path().join("a/b/c"));
    }

    #[test]
    fn resolve_dir_finds_existing_real_nested_dirs_without_create() {
        let work_dir = TempDir::new().unwrap();
        fs::create_dir_all(work_dir.path().join("a/b/c")).unwrap();
        let dir = resolve_dir(work_dir.path(), "a/b/c", false).unwrap();
        assert_eq!(dir.abs("leaf"), work_dir.path().join("a/b/c/leaf"));
    }

    #[test]
    #[cfg(unix)]
    fn resolve_dir_accepts_a_symlinked_work_dir_root() {
        // Only components strictly BELOW work_dir are checked; work_dir
        // itself being a symlink must keep working.
        let real = TempDir::new().unwrap();
        let parent = TempDir::new().unwrap();
        let link = parent.path().join("link_root");
        std::os::unix::fs::symlink(real.path(), &link).unwrap();

        let dir = resolve_dir(&link, "a/b", true).unwrap();
        assert!(real.path().join("a/b").is_dir());
        assert_eq!(dir.abs("f"), link.join("a/b/f"));
    }

    #[test]
    fn validate_component_rejects_dot_dotdot_empty_and_nul() {
        assert!(validate_component(".").is_err());
        assert!(validate_component("..").is_err());
        assert!(validate_component("").is_err());
        assert!(validate_component("has\0nul").is_err());
        assert!(validate_component("has/slash").is_err());
        assert!(validate_component("ordinary").is_ok());
    }

    #[test]
    fn resolve_dir_propagates_component_validation() {
        let work_dir = TempDir::new().unwrap();
        assert!(resolve_dir(work_dir.path(), "a/../b", true).is_err());
        assert!(resolve_dir(work_dir.path(), "a/./b", true).is_err());
    }

    #[test]
    fn to_rel_string_failure_is_a_hard_error_never_ok() {
        let work_dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let err = to_rel_string(work_dir.path(), outside.path()).unwrap_err();
        assert!(matches!(err, Error::Other(_)), "got {err:?}");
    }

    #[test]
    fn create_dir_all_wrapper_creates_the_verified_directory() {
        let work_dir = TempDir::new().unwrap();
        let abs = work_dir.path().join("x/y/z");
        create_dir_all(work_dir.path(), &abs).unwrap();
        assert!(abs.is_dir());
    }

    #[test]
    #[cfg(unix)]
    fn create_dir_all_wrapper_rejects_symlinked_intermediate() {
        let work_dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), work_dir.path().join("sub")).unwrap();

        let abs = work_dir.path().join("sub/child");
        let err = create_dir_all(work_dir.path(), &abs).unwrap_err();
        assert!(matches!(err, Error::SymlinkEscape(_)));
        assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
    }

    #[test]
    fn atomic_write_with_lands_in_the_verified_directory() {
        // Confirm via the resulting file's real (canonicalized) path that the
        // write landed in the directory that was actually verified, not
        // merely at a lexically-matching path.
        let work_dir = TempDir::new().unwrap();
        let abs = work_dir.path().join("a/b/file.txt");
        atomic_write_with(work_dir.path(), &abs, |w| {
            w.write_all(b"hello").map_err(Error::Io)
        })
        .unwrap();

        assert_eq!(fs::read(&abs).unwrap(), b"hello");
        let real = fs::canonicalize(&abs).unwrap();
        let expected = fs::canonicalize(work_dir.path())
            .unwrap()
            .join("a/b/file.txt");
        assert_eq!(real, expected);

        // No stray `.tmp*` sibling left behind.
        let leftovers: Vec<_> = fs::read_dir(abs.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "unexpected temp leftovers: {leftovers:?}");
    }

    #[test]
    fn atomic_write_with_failing_closure_leaves_no_partial_file() {
        let work_dir = TempDir::new().unwrap();
        let abs = work_dir.path().join("a/file.txt");
        let result = atomic_write_with(work_dir.path(), &abs, |w| {
            w.write_all(b"partial").map_err(Error::Io)?;
            Err(Error::Other("boom".to_string()))
        });
        assert!(result.is_err());
        assert!(!abs.exists(), "no partial file must be visible at dest");

        let dir = abs.parent().unwrap();
        if dir.exists() {
            let leftovers: Vec<_> = fs::read_dir(dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().starts_with(".tmp"))
                .collect();
            assert!(leftovers.is_empty(), "unexpected temp leftovers: {leftovers:?}");
        }
    }

    // ---- Working-tree staging names and stale sweep (design/09) -------------

    fn age(path: &Path, secs: u64) {
        let t = std::time::SystemTime::now() - std::time::Duration::from_secs(secs);
        filetime::set_symlink_file_times(
            path,
            FileTime::from_system_time(t),
            FileTime::from_system_time(t),
        )
        .unwrap();
    }

    #[test]
    fn temp_names_are_reserved_and_unique() {
        let a = temp_name();
        let b = temp_name();
        assert!(a.starts_with(".omemfs-tmp-"), "{a}");
        assert!(crate::object::is_working_tree_tmp(&a));
        assert!(crate::object::is_reserved_name(&a));
        assert_ne!(a, b);
    }

    #[test]
    fn a_write_stages_through_a_reserved_name() {
        // While write_fn runs, the only extra entry in the directory is the
        // staging file, and its name must be in the reserved namespace.
        let work_dir = TempDir::new().unwrap();
        let abs = work_dir.path().join("f.txt");
        let dir_path = work_dir.path().to_path_buf();
        let seen: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        atomic_write_with(work_dir.path(), &abs, |w| {
            w.write_all(b"x").map_err(Error::Io)?;
            w.flush().map_err(Error::Io)?;
            for e in fs::read_dir(&dir_path).unwrap().flatten() {
                seen.lock().unwrap().push(e.file_name().to_string_lossy().into_owned());
            }
            Ok(())
        })
        .unwrap();
        let seen = seen.into_inner().unwrap();
        assert!(
            seen.iter().any(|n| n.starts_with(".omemfs-tmp-")),
            "staging file must be reserved-named: {seen:?}"
        );
        assert!(!seen.iter().any(|n| n.starts_with(".tmp")), "{seen:?}");
    }

    #[test]
    fn sweep_removes_only_old_staging_files() {
        let work_dir = TempDir::new().unwrap();
        let d = work_dir.path();
        fs::write(d.join(".omemfs-tmp-old"), b"x").unwrap();
        fs::write(d.join(".omemfs-tmp-fresh"), b"x").unwrap();
        fs::write(d.join("user-old.txt"), b"x").unwrap();
        fs::write(d.join(".tmp-user-file"), b"x").unwrap();
        fs::create_dir(d.join(".omemfs-tmp-dir")).unwrap();
        age(&d.join(".omemfs-tmp-old"), 25 * 3600);
        age(&d.join("user-old.txt"), 25 * 3600);
        age(&d.join(".tmp-user-file"), 25 * 3600);
        age(&d.join(".omemfs-tmp-dir"), 25 * 3600);

        let dir = resolve_dir(d, "", false).unwrap();
        dir.sweep_stale_tmp(std::time::Duration::from_secs(24 * 3600)).unwrap();

        assert!(!d.join(".omemfs-tmp-old").exists(), "old staging file is swept");
        assert!(d.join(".omemfs-tmp-fresh").exists(), "fresh one may belong to a live writer");
        assert!(d.join("user-old.txt").exists(), "user files are never touched");
        assert!(d.join(".tmp-user-file").exists(), "a user's .tmp* file is not ours");
        assert!(d.join(".omemfs-tmp-dir").is_dir(), "a directory is never removed");
    }

    #[test]
    #[cfg(unix)]
    fn sweep_removes_an_old_staging_symlink_without_following_it() {
        let work_dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("target"), b"keep").unwrap();
        let d = work_dir.path();
        std::os::unix::fs::symlink(outside.path().join("target"), d.join(".omemfs-tmp-link")).unwrap();
        age(&d.join(".omemfs-tmp-link"), 25 * 3600);

        let dir = resolve_dir(d, "", false).unwrap();
        dir.sweep_stale_tmp(std::time::Duration::from_secs(24 * 3600)).unwrap();

        assert!(d.join(".omemfs-tmp-link").symlink_metadata().is_err());
        assert_eq!(fs::read(outside.path().join("target")).unwrap(), b"keep");
    }

    #[test]
    fn writes_sweep_a_directory_once_per_process_not_once_per_write() {
        let work_dir = TempDir::new().unwrap();
        let d = work_dir.path();
        // Unique directory name so no other test has swept it.
        let sub = d.join("sweep-once-dir");
        fs::create_dir(&sub).unwrap();

        // First write into the directory: sweeps, so the old file goes.
        fs::write(sub.join(".omemfs-tmp-first"), b"x").unwrap();
        age(&sub.join(".omemfs-tmp-first"), 25 * 3600);
        atomic_write_with(d, &sub.join("a.txt"), |w| w.write_all(b"1").map_err(Error::Io)).unwrap();
        assert!(!sub.join(".omemfs-tmp-first").exists());

        // Second write: no second sweep, so a newly planted old file stays.
        fs::write(sub.join(".omemfs-tmp-second"), b"x").unwrap();
        age(&sub.join(".omemfs-tmp-second"), 25 * 3600);
        atomic_write_with(d, &sub.join("b.txt"), |w| w.write_all(b"2").map_err(Error::Io)).unwrap();
        assert!(
            sub.join(".omemfs-tmp-second").exists(),
            "the sweep must not run again for the same directory"
        );
    }

    #[test]
    fn the_sweep_gate_is_keyed_by_directory_identity_not_a_lexical_path() {
        // The same directory reached through a symlinked work_dir is one
        // directory, so it is swept once, not once per spelling.
        let real = TempDir::new().unwrap();
        let link_parent = TempDir::new().unwrap();
        let link = link_parent.path().join("via-link");
        std::os::unix::fs::symlink(real.path(), &link).unwrap();

        fs::write(real.path().join(".omemfs-tmp-a"), b"x").unwrap();
        age(&real.path().join(".omemfs-tmp-a"), 25 * 3600);
        atomic_write_with(real.path(), &real.path().join("f1"), |w| w.write_all(b"1").map_err(Error::Io)).unwrap();
        assert!(!real.path().join(".omemfs-tmp-a").exists());

        fs::write(real.path().join(".omemfs-tmp-b"), b"x").unwrap();
        age(&real.path().join(".omemfs-tmp-b"), 25 * 3600);
        atomic_write_with(&link, &link.join("f2"), |w| w.write_all(b"2").map_err(Error::Io)).unwrap();
        assert!(
            real.path().join(".omemfs-tmp-b").exists(),
            "same directory via another path must not be swept again"
        );
    }

    #[test]
    #[cfg(unix)]
    fn atomic_write_with_wrapper_rejects_symlinked_intermediate_and_writes_nothing() {
        let work_dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), work_dir.path().join("sub")).unwrap();

        let abs = work_dir.path().join("sub/file.txt");
        let err = atomic_write_with(work_dir.path(), &abs, |w| {
            w.write_all(b"x").map_err(Error::Io)
        })
        .unwrap_err();
        assert!(matches!(err, Error::SymlinkEscape(_)));
        assert!(!outside.path().join("file.txt").exists());
        assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
    }

    #[test]
    fn atomic_write_durable_round_trips_and_fsyncs() {
        let work_dir = TempDir::new().unwrap();
        let abs = work_dir.path().join("d/file.bin");
        atomic_write_durable(work_dir.path(), &abs, b"durable-content").unwrap();
        assert_eq!(fs::read(&abs).unwrap(), b"durable-content");
    }

    #[test]
    fn write_symlink_wrapper_creates_and_replaces() {
        let work_dir = TempDir::new().unwrap();
        let abs = work_dir.path().join("link");
        write_symlink(work_dir.path(), &abs, "target-a").unwrap();
        assert_eq!(fs::read_link(&abs).unwrap(), Path::new("target-a"));

        // Replacing an existing symlink is atomic and idempotent.
        write_symlink(work_dir.path(), &abs, "target-b").unwrap();
        assert_eq!(fs::read_link(&abs).unwrap(), Path::new("target-b"));
    }

    #[test]
    fn write_symlink_replaces_an_existing_empty_directory() {
        let work_dir = TempDir::new().unwrap();
        let abs = work_dir.path().join("was_a_dir");
        fs::create_dir_all(&abs).unwrap();
        write_symlink(work_dir.path(), &abs, "elsewhere").unwrap();
        assert_eq!(fs::read_link(&abs).unwrap(), Path::new("elsewhere"));
    }

    #[test]
    #[cfg(unix)]
    fn write_symlink_wrapper_rejects_symlinked_intermediate() {
        let work_dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), work_dir.path().join("sub")).unwrap();

        let abs = work_dir.path().join("sub/link");
        let err = write_symlink(work_dir.path(), &abs, "target").unwrap_err();
        assert!(matches!(err, Error::SymlinkEscape(_)));
        assert!(!outside.path().join("link").exists());
    }

    #[test]
    fn remove_file_tolerant_wrapper_removes_and_tolerates_missing() {
        let work_dir = TempDir::new().unwrap();
        let abs = work_dir.path().join("a/file.txt");
        fs::create_dir_all(abs.parent().unwrap()).unwrap();
        fs::write(&abs, b"x").unwrap();

        remove_file_tolerant(work_dir.path(), &abs).unwrap();
        assert!(!abs.exists());
        // A second removal of the same (now-absent) path is a no-op, not an
        // error, and a missing PARENT directory is tolerated too.
        remove_file_tolerant(work_dir.path(), &abs).unwrap();
        remove_file_tolerant(work_dir.path(), &work_dir.path().join("never/there.txt")).unwrap();
    }

    /// L7: `remove_file_tolerant` only tolerates `NotFound` -- a directory
    /// found where a file was expected must still propagate as `EISDIR`
    /// rather than being swallowed here. This helper is used at several
    /// call sites for genuine file deletes where a directory appearing
    /// unexpectedly is a real-bug signal that must surface, not be masked;
    /// `restore`'s `DeleteAbsent` handler is the one call site that needs
    /// different (still not silent, but not `EISDIR`-shaped) handling for a
    /// stale stub record whose path turned out to be a directory, and it
    /// gets that by checking the on-disk type itself *before* ever calling
    /// this helper, not by changing this helper's tolerance.
    #[test]
    fn remove_file_tolerant_wrapper_propagates_eisdir() {
        let work_dir = TempDir::new().unwrap();
        let abs = work_dir.path().join("a_dir");
        fs::create_dir(&abs).unwrap();

        let err = remove_file_tolerant(work_dir.path(), &abs).unwrap_err();
        match err {
            Error::Io(e) => assert_eq!(e.kind(), io::ErrorKind::IsADirectory),
            other => panic!("expected Error::Io(IsADirectory), got {other:?}"),
        }
        assert!(abs.is_dir(), "the directory must be left untouched");
    }

    #[test]
    #[cfg(unix)]
    fn remove_file_tolerant_wrapper_rejects_symlinked_intermediate() {
        let work_dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("victim.txt"), b"secret").unwrap();
        std::os::unix::fs::symlink(outside.path(), work_dir.path().join("sub")).unwrap();

        let abs = work_dir.path().join("sub/victim.txt");
        let err = remove_file_tolerant(work_dir.path(), &abs).unwrap_err();
        assert!(matches!(err, Error::SymlinkEscape(_)));
        assert!(
            outside.path().join("victim.txt").exists(),
            "the file outside work_dir must never be removed"
        );
    }

    #[test]
    fn open_leaf_splits_parent_and_name() {
        let work_dir = TempDir::new().unwrap();
        let (dir, leaf) = open_leaf(work_dir.path(), "a/b/file.txt", true).unwrap();
        assert_eq!(leaf, "file.txt");
        assert_eq!(dir.abs("file.txt"), work_dir.path().join("a/b/file.txt"));
    }

    #[test]
    fn open_leaf_at_work_dir_root() {
        let work_dir = TempDir::new().unwrap();
        let (dir, leaf) = open_leaf(work_dir.path(), "file.txt", true).unwrap();
        assert_eq!(leaf, "file.txt");
        assert_eq!(dir.abs("file.txt"), work_dir.path().join("file.txt"));
    }

    #[test]
    fn set_mode_toggles_exec_bits_relative_to_the_verified_directory() {
        use std::os::unix::fs::PermissionsExt;
        let work_dir = TempDir::new().unwrap();
        let abs = work_dir.path().join("script.sh");
        fs::write(&abs, b"x").unwrap();
        fs::set_permissions(&abs, fs::Permissions::from_mode(0o644)).unwrap();

        let (dir, leaf) = open_leaf(work_dir.path(), "script.sh", false).unwrap();
        dir.set_mode(&leaf, &Some("755".to_string())).unwrap();
        assert_eq!(fs::metadata(&abs).unwrap().permissions().mode() & 0o777, 0o755);

        dir.set_mode(&leaf, &None).unwrap();
        assert_eq!(fs::metadata(&abs).unwrap().permissions().mode() & 0o777, 0o644);
    }

    #[test]
    fn create_dir_tolerates_existing() {
        let work_dir = TempDir::new().unwrap();
        let dir = open_root(work_dir.path()).unwrap();
        dir.create_dir("sub").unwrap();
        dir.create_dir("sub").unwrap(); // EEXIST tolerated
        assert!(work_dir.path().join("sub").is_dir());
    }

    // -----------------------------------------------------------------------
    // is_real_dir
    // -----------------------------------------------------------------------

    #[test]
    fn is_real_dir_true_for_existing_real_directory() {
        let work_dir = TempDir::new().unwrap();
        fs::create_dir_all(work_dir.path().join("a/b")).unwrap();
        assert!(is_real_dir(work_dir.path(), &work_dir.path().join("a/b")).unwrap());
    }

    #[test]
    fn is_real_dir_true_for_work_dir_itself() {
        // work_dir itself is exempt from the symlink check by design (see
        // the module doc comment) -- it is always reported as a real dir.
        let work_dir = TempDir::new().unwrap();
        assert!(is_real_dir(work_dir.path(), work_dir.path()).unwrap());
    }

    #[test]
    fn is_real_dir_false_for_missing_path() {
        let work_dir = TempDir::new().unwrap();
        assert!(!is_real_dir(work_dir.path(), &work_dir.path().join("nope")).unwrap());
    }

    #[test]
    fn is_real_dir_false_for_missing_path_with_missing_ancestor() {
        // Not just the leaf missing -- an ancestor directory missing too.
        let work_dir = TempDir::new().unwrap();
        assert!(!is_real_dir(work_dir.path(), &work_dir.path().join("a/b/c")).unwrap());
    }

    #[test]
    fn is_real_dir_false_for_a_plain_file() {
        // A regular file (not a symlink) at the target path is "not a real
        // dir" but is deliberately NOT an error -- callers such as `omemfs
        // stub` need to distinguish "there is nothing to stub-as-a-directory
        // here" from "something is actively trying to escape the boundary".
        let work_dir = TempDir::new().unwrap();
        let abs = work_dir.path().join("file.txt");
        fs::write(&abs, b"x").unwrap();
        assert!(!is_real_dir(work_dir.path(), &abs).unwrap());
    }

    #[test]
    #[cfg(unix)]
    fn is_real_dir_rejects_a_symlink_leaf_even_if_it_points_to_a_real_directory() {
        let work_dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let abs = work_dir.path().join("link");
        std::os::unix::fs::symlink(outside.path(), &abs).unwrap();

        let err = is_real_dir(work_dir.path(), &abs).unwrap_err();
        assert!(matches!(err, Error::SymlinkEscape(_)), "got {err:?}");
    }

    #[test]
    #[cfg(unix)]
    fn is_real_dir_rejects_symlinked_intermediate_component() {
        let work_dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), work_dir.path().join("sub")).unwrap();

        let err = is_real_dir(work_dir.path(), &work_dir.path().join("sub/child")).unwrap_err();
        assert!(matches!(err, Error::SymlinkEscape(_)), "got {err:?}");
    }

    // -----------------------------------------------------------------------
    // remove_dir_all
    // -----------------------------------------------------------------------

    #[test]
    fn remove_dir_all_removes_nested_contents_leaving_dir_itself() {
        let work_dir = TempDir::new().unwrap();
        let target = work_dir.path().join("target");
        fs::create_dir_all(target.join("sub/deeper")).unwrap();
        fs::write(target.join("top.txt"), b"a").unwrap();
        fs::write(target.join("sub/mid.txt"), b"b").unwrap();
        fs::write(target.join("sub/deeper/leaf.txt"), b"c").unwrap();

        remove_dir_all(work_dir.path(), &target, &[]).unwrap();

        assert!(target.is_dir(), "the target directory itself must remain");
        assert_eq!(
            fs::read_dir(&target).unwrap().count(),
            0,
            "the target directory must be empty"
        );
    }

    #[test]
    fn remove_dir_all_on_an_already_empty_directory_is_a_noop() {
        let work_dir = TempDir::new().unwrap();
        let target = work_dir.path().join("empty");
        fs::create_dir_all(&target).unwrap();
        remove_dir_all(work_dir.path(), &target, &[]).unwrap();
        assert!(target.is_dir());
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
    }

    /// This is the regression this whole `keep` parameter exists to guard
    /// against: a caller that writes a marker file into a directory and then
    /// wants to clear out everything else must be able to name that marker
    /// in `keep` and have it survive -- the marker must neither be unlinked
    /// nor descended into (were it to be, say, a directory).
    #[test]
    fn remove_dir_all_leaves_kept_entries_untouched() {
        let work_dir = TempDir::new().unwrap();
        let target = work_dir.path().join("target");
        fs::create_dir_all(target.join("sub")).unwrap();
        fs::write(target.join("marker.txt"), b"keep-me").unwrap();
        fs::write(target.join("top.txt"), b"a").unwrap();
        fs::write(target.join("sub/mid.txt"), b"b").unwrap();

        remove_dir_all(work_dir.path(), &target, &["marker.txt"]).unwrap();

        assert_eq!(
            fs::read(target.join("marker.txt")).unwrap(),
            b"keep-me",
            "the kept entry's content must be untouched"
        );
        assert!(!target.join("top.txt").exists());
        assert!(!target.join("sub").exists());
        assert_eq!(
            fs::read_dir(&target).unwrap().count(),
            1,
            "only the kept entry must remain"
        );
    }

    /// A `keep`-listed name that happens to be a directory must not be
    /// descended into either -- it is skipped outright, exactly like a
    /// kept plain file.
    #[test]
    fn remove_dir_all_does_not_descend_into_a_kept_directory() {
        let work_dir = TempDir::new().unwrap();
        let target = work_dir.path().join("target");
        fs::create_dir_all(target.join("keep_dir")).unwrap();
        fs::write(target.join("keep_dir/inner.txt"), b"still here").unwrap();
        fs::write(target.join("drop.txt"), b"gone").unwrap();

        remove_dir_all(work_dir.path(), &target, &["keep_dir"]).unwrap();

        assert!(!target.join("drop.txt").exists());
        assert_eq!(
            fs::read(target.join("keep_dir/inner.txt")).unwrap(),
            b"still here",
            "a kept directory's contents must survive untouched"
        );
    }

    #[test]
    #[cfg(unix)]
    fn remove_dir_all_rejects_when_target_itself_is_a_symlink() {
        let work_dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("victim.txt"), b"secret").unwrap();
        let abs = work_dir.path().join("link");
        std::os::unix::fs::symlink(outside.path(), &abs).unwrap();

        let err = remove_dir_all(work_dir.path(), &abs, &[]).unwrap_err();
        assert!(matches!(err, Error::SymlinkEscape(_)), "got {err:?}");
        assert!(
            outside.path().join("victim.txt").exists(),
            "content behind the symlink must never be touched"
        );
    }

    #[test]
    #[cfg(unix)]
    fn remove_dir_all_rejects_a_symlink_found_while_recursing() {
        let work_dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("victim.txt"), b"secret").unwrap();

        let target = work_dir.path().join("target");
        fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(outside.path(), target.join("escape")).unwrap();

        let err = remove_dir_all(work_dir.path(), &target, &[]).unwrap_err();
        assert!(matches!(err, Error::SymlinkEscape(_)), "got {err:?}");
        assert!(
            outside.path().join("victim.txt").exists(),
            "content behind the symlink must never be touched"
        );
        // The symlink entry itself must also survive -- remove_dir_all must
        // refuse to unlink it, not just refuse to follow it.
        assert!(
            target.join("escape").symlink_metadata().is_ok(),
            "the symlink entry itself must not be removed either"
        );
    }

    #[test]
    #[cfg(unix)]
    fn remove_dir_all_rejects_symlinked_intermediate_component() {
        let work_dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), work_dir.path().join("sub")).unwrap();

        let err = remove_dir_all(work_dir.path(), &work_dir.path().join("sub/child"), &[])
            .unwrap_err();
        assert!(matches!(err, Error::SymlinkEscape(_)), "got {err:?}");
        assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
    }

    #[test]
    fn remove_dir_all_errors_on_a_missing_directory() {
        // Design decision: unlike `remove_file_tolerant`, `remove_dir_all` is
        // NOT tolerant of a missing target -- it matches `std::fs::
        // remove_dir_all`'s own behaviour (a NotFound `io::Error`), because
        // every current call site (`omemfs stub`) only calls this
        // immediately after confirming via `is_real_dir` that the directory
        // exists, so a NotFound here signals a genuine race/bug worth
        // surfacing rather than a "nothing to do" case to swallow silently.
        let work_dir = TempDir::new().unwrap();
        let err =
            remove_dir_all(work_dir.path(), &work_dir.path().join("nope"), &[]).unwrap_err();
        assert!(matches!(err, Error::Io(_)), "got {err:?}");
    }

    // -----------------------------------------------------------------------
    // remove_dir_if_empty
    // -----------------------------------------------------------------------

    #[test]
    fn remove_dir_if_empty_removes_an_empty_directory() {
        let work_dir = TempDir::new().unwrap();
        let target = work_dir.path().join("target");
        fs::create_dir_all(&target).unwrap();

        let removed = remove_dir_if_empty(work_dir.path(), &target).unwrap();
        assert!(removed, "an empty directory must be reported as removed");
        assert!(!target.exists(), "the directory must actually be gone");
    }

    #[test]
    fn remove_dir_if_empty_leaves_a_non_empty_directory_untouched() {
        let work_dir = TempDir::new().unwrap();
        let target = work_dir.path().join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("still-here.txt"), b"content").unwrap();

        let removed = remove_dir_if_empty(work_dir.path(), &target).unwrap();
        assert!(!removed, "a non-empty directory must not be reported as removed");
        assert!(target.is_dir(), "the directory must still exist");
        assert!(target.join("still-here.txt").exists());
    }

    #[test]
    fn remove_dir_if_empty_on_a_missing_directory_is_a_tolerant_noop() {
        let work_dir = TempDir::new().unwrap();
        let removed =
            remove_dir_if_empty(work_dir.path(), &work_dir.path().join("nope")).unwrap();
        assert!(!removed);
    }

    #[test]
    fn remove_dir_if_empty_on_a_plain_file_is_a_tolerant_noop() {
        let work_dir = TempDir::new().unwrap();
        let target = work_dir.path().join("target.txt");
        fs::write(&target, b"a plain file, not a directory").unwrap();

        let removed = remove_dir_if_empty(work_dir.path(), &target).unwrap();
        assert!(!removed);
        assert!(target.is_file(), "the file must be untouched");
    }

    #[test]
    fn remove_dir_if_empty_refuses_work_dir_itself() {
        let work_dir = TempDir::new().unwrap();
        let removed = remove_dir_if_empty(work_dir.path(), work_dir.path()).unwrap();
        assert!(!removed);
        assert!(work_dir.path().is_dir());
    }

    #[test]
    #[cfg(unix)]
    fn remove_dir_if_empty_rejects_a_symlink_at_the_target() {
        let work_dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let target = work_dir.path().join("link");
        std::os::unix::fs::symlink(outside.path(), &target).unwrap();

        let err = remove_dir_if_empty(work_dir.path(), &target).unwrap_err();
        assert!(matches!(err, Error::SymlinkEscape(_)), "got {err:?}");
        assert!(
            outside.path().is_dir(),
            "the symlink target must survive untouched"
        );
    }

    #[test]
    #[cfg(unix)]
    fn remove_dir_if_empty_rejects_symlinked_intermediate_component() {
        let work_dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), work_dir.path().join("sub")).unwrap();

        let err = remove_dir_if_empty(work_dir.path(), &work_dir.path().join("sub/child"))
            .unwrap_err();
        assert!(matches!(err, Error::SymlinkEscape(_)), "got {err:?}");
    }
}
