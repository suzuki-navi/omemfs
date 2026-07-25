/// Backend-pluggable abstraction for the remote index-root pointer.
///
/// The index root is the single root pointer for a remote. Reading it and
/// updating it via compare-and-swap (CAS) is the only place the sync model
/// depends on backend-specific atomicity primitives. Both `push`
/// (`PackWriter`) and `pack` (`commands::pack`) route the index-root read and
/// CAS-write through this one trait, so the semantics are guaranteed identical
/// between the two commands and a future S3 / Azure / GCS backend can be slotted
/// in without touching either call site.
///
/// The contract is built around an opaque [`RootToken`] threaded from read to
/// write. `read` returns both the raw bytes AND the version token observed at
/// read time; `cas_write` is conditioned on that token, never on a byte
/// comparison. This is what makes the abstraction implementable on cloud object
/// stores: none of S3 / Azure / GCS offer a server-side byte-compare-and-swap,
/// but all offer a conditional write keyed on a version identifier observed at
/// read time (an ETag or a generation number). Keeping the token opaque lets
/// each backend supply its own version identity.
///
/// Backend mappings (see design/03_sync_model.md "CAS safety for push"). The
/// cloud API details below are to be confirmed against current provider docs at
/// implementation time:
///
/// - Local directory (`LocalRootPointer`): `read` is a plain file read
///   (NotFound → `(None, RootToken::Absent)`), with the token being the stored
///   bytes themselves (`RootToken::Present(bytes)`) so the comparison is
///   byte-identical to the previous byte-compare contract. `cas_write` takes an
///   exclusive `flock(2)` on the fixed-name `INDEX_ROOT.lock`, re-reads the
///   current bytes under the lock, recomputes the token the SAME way as `read`,
///   compares it to `expected`, and on a match writes the new bytes via atomic
///   rename. The lock closes the TOCTOU window between compare and write.
///
/// - S3 / Azure (`S3RootPointer` / `AzureRootPointer`, see
///   `src/store/cloud/{s3,azure}.rs`): `read` is a `GetObject` / `GetBlob`
///   returning `(None, RootToken::Absent)` on a 404, and captures the object
///   ETag into `RootToken::Present(etag)`. `cas_write` is a conditional
///   `PutObject` / `PutBlob` using `If-Match: <etag>` when
///   `expected = Present(etag)`, or `If-None-Match: *` when `expected = Absent`
///   (the root must not yet exist). A `412 Precondition Failed` response maps
///   to `Error::CasFailed`. No lock file is needed because the conditional
///   write is atomic on the server side.
///
/// - GCS (`GcsRootPointer`, see `src/store/cloud/gcs.rs`): `read` captures the
///   object generation number into `RootToken::Present(generation)`.
///   `cas_write` is a conditional upload using `ifGenerationMatch=<generation>`
///   when `expected = Present(generation)`, or `ifGenerationMatch=0` when
///   `expected = Absent`. A `412 Precondition Failed` maps to
///   `Error::CasFailed`. No lock file is needed.
use std::path::PathBuf;

use libc;

use crate::codec::encrypt::EncryptKey;
use crate::error::Error;
use crate::store::local::atomic_write;

/// Opaque version token identifying a specific state of the root pointer.
///
/// `LocalRootPointer` uses the stored bytes as the token; an S3 / Azure backend
/// would use the object ETag, GCS the generation number. `Absent` means the
/// root pointer did not exist at read time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RootToken {
    /// The root pointer did not exist at read time. As a `cas_write` expectation
    /// this means "must still not exist".
    Absent,
    /// The root pointer existed; the bytes identify its version. The contents
    /// are backend-defined and opaque to call sites (local: the stored bytes;
    /// S3 / Azure: the ETag; GCS: the generation number).
    Present(Vec<u8>),
}

/// Read and compare-and-swap the remote index-root pointer.
///
/// `Send + Sync` is required because the `ls` command moves a boxed root pointer
/// onto a detached worker thread to bound the INDEX_ROOT lookup with a timeout
/// (`src/commands/ls.rs`). `LocalRootPointer` holds only a `PathBuf` and an
/// `Option<EncryptKey>`, both `Send + Sync`, so it satisfies this bound; future
/// cloud pointers must likewise be `Send + Sync`.
pub trait RootPointer: Send + Sync {
    /// Read the index root, returning its raw bytes (`None` if absent) AND the
    /// version token observed at read time.
    fn read(&self) -> Result<(Option<Vec<u8>>, RootToken), Error>;

    /// Conditional write: write `new` only if the pointer's CURRENT version
    /// token still equals `expected` (`RootToken::Absent` means "must still not
    /// exist"). Returns `Error::CasFailed` on mismatch. The compare-and-write
    /// must be atomic against concurrent writers to the same root pointer.
    fn cas_write(&self, expected: &RootToken, new: &[u8]) -> Result<(), Error>;
}

/// Local-directory implementation of [`RootPointer`].
///
/// The index-root path is derived from `remote_base` + `key` (the derived
/// sharded path when encrypted, the fixed `INDEX_ROOT` file otherwise) via the
/// shared `index_root_path` helper. The CAS lock is the fixed-name
/// `INDEX_ROOT.lock` next to the remote root.
pub struct LocalRootPointer {
    remote_base: PathBuf,
    key: Option<EncryptKey>,
}

impl LocalRootPointer {
    pub fn new(remote_base: PathBuf, key: Option<EncryptKey>) -> Self {
        LocalRootPointer { remote_base, key }
    }

    fn path(&self) -> PathBuf {
        crate::codec::pack::index_root_path(&self.remote_base, self.key.as_ref())
    }
}

/// Derive the version token for a given current-bytes observation. For the
/// local backend the token IS the stored bytes, so a token comparison is
/// byte-identical to the previous byte-compare contract.
fn token_for(current: &Option<Vec<u8>>) -> RootToken {
    match current {
        None => RootToken::Absent,
        Some(bytes) => RootToken::Present(bytes.clone()),
    }
}

impl RootPointer for LocalRootPointer {
    fn read(&self) -> Result<(Option<Vec<u8>>, RootToken), Error> {
        let bytes = match std::fs::read(self.path()) {
            Ok(b) => Some(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(Error::Io(e)),
        };
        let token = token_for(&bytes);
        Ok((bytes, token))
    }

    fn cas_write(&self, expected: &RootToken, new: &[u8]) -> Result<(), Error> {
        let path = self.path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Serialise the read-compare-write against concurrent writers.
        let _guard = IndexRootCasLock::acquire(&self.remote_base)?;

        // Re-read the current value under the lock and recompute its token the
        // same way `read` does, then compare to `expected`.
        let current = match std::fs::read(&path) {
            Ok(b) => Some(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(Error::Io(e)),
        };

        if token_for(&current) != *expected {
            return Err(Error::CasFailed);
        }

        atomic_write(&path, new)?;
        Ok(())
    }
}

/// Exclusive lock guarding the INDEX_ROOT read-compare-write on the
/// local-directory backend. Held only for the duration of the CAS. The lock is
/// released when the file descriptor is dropped (kernel releases the flock).
struct IndexRootCasLock {
    _file: std::fs::File,
}

impl IndexRootCasLock {
    fn acquire(remote_base: &std::path::Path) -> Result<Self, Error> {
        use std::os::unix::io::AsRawFd;
        // The lock file name is always fixed (INDEX_ROOT.lock) regardless of
        // encryption mode — it is a transient coordination artifact and reveals
        // only that the prefix is an omemfs repo (design/02, design/03).
        let lock_path = crate::codec::pack::index_root_lock_path(remote_base);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(Error::Io)?;
        // Blocking exclusive lock: serialise concurrent CAS attempts rather than
        // failing fast, since the critical section is very short.
        let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if ret != 0 {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
        Ok(IndexRootCasLock { _file: file })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn read_absent_returns_none_and_absent_token() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().to_path_buf();
        let rp = LocalRootPointer::new(base, None);

        let (bytes, token) = rp.read().unwrap();
        assert!(bytes.is_none(), "pointer must start absent");
        assert_eq!(token, RootToken::Absent);
    }

    #[test]
    fn read_present_returns_bytes_and_present_token() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().to_path_buf();
        let rp = LocalRootPointer::new(base, None);

        let v1 = b"root-v1".to_vec();
        rp.cas_write(&RootToken::Absent, &v1).unwrap();

        let (bytes, token) = rp.read().unwrap();
        assert_eq!(bytes, Some(v1.clone()));
        // Local backend: token IS the stored bytes.
        assert_eq!(token, RootToken::Present(v1));
    }

    #[test]
    fn cas_write_creates_absent_pointer_unencrypted() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().to_path_buf();
        let rp = LocalRootPointer::new(base.clone(), None);

        let (_, token) = rp.read().unwrap();
        assert_eq!(token, RootToken::Absent, "pointer must start absent");

        let bytes = b"root-v1".to_vec();
        rp.cas_write(&RootToken::Absent, &bytes).unwrap();

        // Unencrypted: lands at the fixed INDEX_ROOT file.
        assert!(base.join("INDEX_ROOT").exists());
        assert_eq!(rp.read().unwrap().0, Some(bytes));
    }

    #[test]
    fn cas_write_absent_fails_when_already_present() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().to_path_buf();
        let rp = LocalRootPointer::new(base, None);

        let v1 = b"root-v1".to_vec();
        rp.cas_write(&RootToken::Absent, &v1).unwrap();

        // cas_write(&Absent, ..) means "must not exist", but it does → CasFailed.
        let v2 = b"root-v2".to_vec();
        let err = rp.cas_write(&RootToken::Absent, &v2).unwrap_err();
        assert!(
            matches!(err, Error::CasFailed),
            "expected CasFailed, got {err:?}"
        );
        assert_eq!(rp.read().unwrap().0, Some(v1));
    }

    #[test]
    fn cas_write_updates_with_token_from_read() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().to_path_buf();
        let rp = LocalRootPointer::new(base, None);

        let v1 = b"root-v1".to_vec();
        rp.cas_write(&RootToken::Absent, &v1).unwrap();

        // Capture the current token via read(), then CAS against it.
        let (_, token) = rp.read().unwrap();
        let v2 = b"root-v2".to_vec();
        rp.cas_write(&token, &v2).unwrap();
        assert_eq!(rp.read().unwrap().0, Some(v2));
    }

    #[test]
    fn cas_write_rejects_stale_token_and_preserves_bytes() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().to_path_buf();
        let rp = LocalRootPointer::new(base, None);

        let v1 = b"root-v1".to_vec();
        rp.cas_write(&RootToken::Absent, &v1).unwrap();

        // Capture a token, then perform an intervening write that changes the
        // current version, making the captured token stale.
        let (_, stale_token) = rp.read().unwrap();
        let v2 = b"root-v2".to_vec();
        rp.cas_write(&stale_token, &v2).unwrap();

        // The stale token no longer matches the current version → CasFailed,
        // and the stored bytes are left unchanged.
        let v3 = b"root-v3".to_vec();
        let err = rp.cas_write(&stale_token, &v3).unwrap_err();
        assert!(
            matches!(err, Error::CasFailed),
            "expected CasFailed, got {err:?}"
        );
        assert_eq!(rp.read().unwrap().0, Some(v2));
    }

    #[test]
    fn cas_write_encrypted_lands_at_derived_path() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().to_path_buf();
        let key = EncryptKey::new([5u8; 32]);
        let rp = LocalRootPointer::new(base.clone(), Some(key.clone()));

        let (_, token) = rp.read().unwrap();
        assert_eq!(token, RootToken::Absent);

        let bytes = b"encrypted-root".to_vec();
        rp.cas_write(&RootToken::Absent, &bytes).unwrap();

        // Encrypted: lands at the derived sharded path, NOT the fixed INDEX_ROOT.
        let derived = crate::codec::pack::index_root_path(&base, Some(&key));
        assert!(
            derived.exists(),
            "encrypted pointer must land at the derived path"
        );
        assert!(
            !base.join("INDEX_ROOT").exists(),
            "must not use the fixed name when encrypted"
        );

        let (read_bytes, token) = rp.read().unwrap();
        assert_eq!(read_bytes, Some(bytes.clone()));
        assert_eq!(token, RootToken::Present(bytes));

        // CAS update against the token captured by read() succeeds.
        let bytes2 = b"encrypted-root-2".to_vec();
        rp.cas_write(&token, &bytes2).unwrap();
        assert_eq!(rp.read().unwrap().0, Some(bytes2));
    }
}
