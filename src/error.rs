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
