use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid hash: expected 64 hex characters")]
    InvalidHash,

    #[error("object not found: {0}")]
    ObjectNotFound(String),

    /// A referenced object could not be read from the **local**
    /// `.omemfs/objects/` cache while `push`'s upload step was transferring
    /// it to the remote (design/07_stat_cache.md, "Upload-failure recovery
    /// (final backstop)"). This is the only site that produces this variant:
    /// `upload_missing_with_progress` (`src/commands/push.rs`) maps an
    /// `Error::ObjectNotFound` from `codec::store_read(local, hash, None)`
    /// into this variant so the L3 seal-clearing backstop can be triggered on
    /// exactly this failure shape and nothing broader (in particular, never
    /// on a *remote*-side `ObjectNotFound`, e.g. from `remote.exists()`).
    #[error("local object missing: {0}")]
    LocalObjectMissing(String),

    #[error("ambiguous hash prefix '{0}' — matches multiple objects")]
    AmbiguousHash(String),

    #[error("remote has been updated since last sync\nRun 'omemfs pull' and retry 'omemfs push'.")]
    CasFailed,

    #[error("{0}")]
    LockFailed(String),

    #[error("pull would overwrite local uncommitted changes")]
    Conflict,

    #[error("GCM authentication tag mismatch — object may be corrupted or tampered")]
    AuthTagMismatch,

    #[error("invalid object: {0}")]
    InvalidObject(String),

    /// A working-tree write target has a symlink somewhere between the
    /// repository root and the target's parent directory. Materialisation
    /// refuses to follow it (design/03_sync_model.md, "Write safety:
    /// symlinked working-tree directories").
    #[error("refusing to write through a symlinked directory: {0}")]
    SymlinkEscape(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The path was readable, but did not remain stable long enough to take a
    /// self-consistent snapshot. Push treats this as a best-effort skip.
    #[error("source file remained active while being read: {0}")]
    SourceChanged(String),

    /// The scan ran out of file descriptors (`EMFILE` / `ENFILE`).
    #[error("{0}")]
    TooManyOpenFiles(FdExhaustion),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Races expected while scanning a live working tree. They are distinct
    /// from permission, device, and storage failures, which remain fatal.
    pub fn is_live_path_race(&self) -> bool {
        match self {
            Error::SourceChanged(_) => true,
            Error::Io(error) => matches!(
                error.kind(),
                std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::NotADirectory
                    | std::io::ErrorKind::IsADirectory
            ),
            _ => false,
        }
    }

    /// A failure encountered while `restore` (or anything else that heals a
    /// local cache miss from the remote) tries to fetch an object is
    /// "recoverable" when it is exactly the case `04_cli_spec.md`'s "Missing
    /// objects" section describes: every object reachable from `clone_root`
    /// is guaranteed to exist on the remote, so a cache miss during the plan
    /// phase means "not fetched into this clone yet", never "gone" -- safe to
    /// silently downgrade to an `unavailable:` report.
    ///
    /// Everything else -- a GCM auth-tag mismatch (DEK mismatch or
    /// tampering), a corrupt/invalid object, a misconfigured or mistyped
    /// `--remote` (surfaces as `Error::Other("remote '<name>' not
    /// configured")`), a CAS failure, a permission or disk-full error, or any
    /// other unclassified failure -- is NOT recoverable: it indicates
    /// something is genuinely wrong and must be surfaced to the user, never
    /// silently swallowed as a bland "unavailable".
    ///
    /// `Error::LocalObjectMissing` is deliberately NOT included: it means
    /// push's own upload step could not read an object it believed was
    /// staged locally -- a real, actionable failure of the *current*
    /// process's local cache, never a "not fetched into this clone yet"
    /// situation. Downgrading it to "unavailable" would hide a bug rather
    /// than report it.
    ///
    /// L3: the `Error::Io` arm below only reflects how the **local-directory**
    /// backend maps failures -- it is the backend `restore`'s own test suite
    /// exercises against an unreachable remote, and on it, "remote root moved
    /// or removed out from under a configured path" surfaces as exactly one
    /// of these `io::ErrorKind`s. Cloud backends map most transport failures
    /// (DNS failure, an expired credential, throttling, a network partition,
    /// ...) to `Error::Other`/a backend-specific variant instead, which this
    /// function correctly does NOT classify as recoverable -- so on a cloud
    /// backend, an unreachable remote produces `restore`'s "warning: fetch
    /// from ... failed" line (see `design/04_cli_spec.md`, "Missing
    /// objects"), while on the local-directory backend the identical
    /// high-level situation (remote unreachable) stays silent. This is a
    /// real, intentional asymmetry, not a bug: a classification broad enough
    /// to mark every cloud SDK's transient/network error "recoverable" the
    /// way the local backend's I/O errors already are would risk also
    /// swallowing genuinely actionable cloud errors (bad credentials, for
    /// example) that must NOT be silently downgraded to a bland
    /// `unavailable:`. The `unavailable:` lines, the summary counts, and the
    /// exit code are identical either way -- only this one extra warning
    /// line differs by backend.
    pub fn is_recoverable_fetch_miss(&self) -> bool {
        match self {
            Error::ObjectNotFound(_) => true,
            // Mirrors `is_live_path_race`'s I/O kinds: on the local-directory
            // backend restore's own tests exercise, an unreachable remote
            // surfaces as exactly one of these (e.g. the remote root having
            // been moved or removed out from under a configured path).
            Error::Io(error) => matches!(
                error.kind(),
                std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::NotADirectory
                    | std::io::ErrorKind::IsADirectory
            ),
            _ => false,
        }
    }
}

/// Which descriptor limit a scan hit, and where (design/03 "File descriptor
/// budget", "Exhaustion is reported with its cause").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FdExhaustion {
    /// Repository-relative path being scanned (`.` for the scan root).
    pub path: String,
    /// `ENFILE` (system-wide table full) rather than `EMFILE` (per-process).
    pub system_wide: bool,
    /// `RLIMIT_NOFILE` soft limit; `None` if it could not be read.
    pub soft_limit: Option<libc::rlim_t>,
}

impl std::fmt::Display for FdExhaustion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.system_wide {
            return write!(
                f,
                "too many open files in the system while scanning '{}'",
                self.path
            );
        }
        let limit = match self.soft_limit {
            Some(n) if n == libc::RLIM_INFINITY => "unlimited".to_string(),
            Some(n) => n.to_string(),
            None => "unknown".to_string(),
        };
        write!(
            f,
            "too many open files while scanning '{}' (RLIMIT_NOFILE soft limit: {}); raise it with `ulimit -n`",
            self.path, limit
        )
    }
}

fn current_soft_nofile_limit() -> Option<libc::rlim_t> {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `lim` is valid writable storage for `getrlimit` to fill in.
    let ret = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) };
    (ret == 0).then_some(lim.rlim_cur)
}

/// Turn an `EMFILE` / `ENFILE` I/O error met while scanning `path` into
/// [`Error::TooManyOpenFiles`]; any other error is returned unchanged.
pub fn map_fd_exhaustion(err: Error, path: &str) -> Error {
    map_fd_exhaustion_with(err, path, current_soft_nofile_limit())
}

/// [`map_fd_exhaustion`] with the soft limit supplied by the caller.
pub fn map_fd_exhaustion_with(
    err: Error,
    path: &str,
    soft_limit: Option<libc::rlim_t>,
) -> Error {
    let system_wide = match &err {
        Error::Io(e) => match e.raw_os_error() {
            Some(libc::EMFILE) => false,
            Some(libc::ENFILE) => true,
            _ => return err,
        },
        _ => return err,
    };
    Error::TooManyOpenFiles(FdExhaustion {
        path: if path.is_empty() { ".".to_string() } else { path.to_string() },
        system_wide,
        soft_limit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn io(code: i32) -> Error {
        Error::Io(std::io::Error::from_raw_os_error(code))
    }

    #[test]
    fn emfile_maps_to_a_message_naming_the_path_and_the_soft_limit() {
        let e = map_fd_exhaustion_with(io(libc::EMFILE), "a/b", Some(1024));
        assert_eq!(
            e.to_string(),
            "too many open files while scanning 'a/b' (RLIMIT_NOFILE soft limit: 1024); raise it with `ulimit -n`"
        );
    }

    #[test]
    fn enfile_maps_to_the_system_wide_message() {
        let e = map_fd_exhaustion_with(io(libc::ENFILE), "a/b", Some(1024));
        assert_eq!(
            e.to_string(),
            "too many open files in the system while scanning 'a/b'"
        );
    }

    #[test]
    fn the_scan_root_is_named_with_a_dot() {
        let e = map_fd_exhaustion_with(io(libc::EMFILE), "", Some(256));
        assert!(e.to_string().contains("scanning '.' (RLIMIT_NOFILE soft limit: 256)"));
    }

    #[test]
    fn other_errors_pass_through_unchanged() {
        assert!(matches!(
            map_fd_exhaustion_with(io(libc::EACCES), "a", Some(1)),
            Error::Io(_)
        ));
        assert!(matches!(
            map_fd_exhaustion_with(Error::SymlinkEscape("x".into()), "a", Some(1)),
            Error::SymlinkEscape(_)
        ));
    }

    #[test]
    fn fd_exhaustion_is_neither_a_live_path_race_nor_a_recoverable_miss() {
        let e = map_fd_exhaustion_with(io(libc::EMFILE), "a", Some(1));
        assert!(!e.is_live_path_race());
        assert!(!e.is_recoverable_fetch_miss());
    }
}
