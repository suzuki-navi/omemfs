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

/// Build a unique `.tmp`-prefixed sibling filename. `.tmp` is the prefix
/// `store::local::sweep_stale_temp_files` matches, so a crash leftover here is
/// still swept the same way local cache writes already are.
fn temp_name() -> String {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(".tmp{:x}-{:x}-{:x}", pid, nanos, n)
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

    /// openat(O_CREAT|O_EXCL|O_WRONLY|O_NOFOLLOW) a ".tmp"-prefixed sibling
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
}
