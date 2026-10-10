use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid hash: expected 64 hex characters")]
    InvalidHash,

    #[error("object not found: {0}")]
    ObjectNotFound(String),

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
