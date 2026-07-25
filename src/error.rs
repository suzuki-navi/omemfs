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
}
