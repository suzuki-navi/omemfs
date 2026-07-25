use std::fs;
use std::io;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use hmac::{Hmac, Mac};
use libc;
use sha2::Sha256;

use crate::codec::encrypt::EncryptKey;
use crate::dtimer_l7;
use crate::error::Error;
use crate::object::Hash;
use crate::store::ObjectStore;
use crate::store::cloud::CloudObjects;
use crate::store::objects_dir::ObjectsDir;
use crate::store::remote_objects_dir::RemoteObjectsDir;

/// Backend selector: local cache uses adaptive depth; remote uses fixed 3-level sharding.
///
/// The dispatch is intentionally per-method (exists / size / list_with_sizes /
/// open_read / write_stream) rather than a single `find() -> Option<PathBuf>`
/// funnel. A future cloud variant (S3 / Azure / GCS) cannot resolve an object to
/// a local filesystem path, so the operations a cloud backend must implement are
/// expressed here WITHOUT returning a `PathBuf`. `find()` is retained only for
/// the local-only callers that genuinely need a filesystem path
/// (`open_read_by_storage_key`, `objects_path`); it is not used by the storage
/// operations that clouds will implement.
#[derive(Clone)]
enum ObjectsBackend {
    Local(ObjectsDir),
    Remote(RemoteObjectsDir),
    /// A cloud object store (S3 / Azure / GCS). It cannot resolve an object to a
    /// local filesystem path, so it implements the per-method storage ops
    /// (exists / size / open_read / write_stream / list_with_sizes) directly via
    /// `CloudObjectIo` and is never reached through `find()`.
    Cloud(CloudObjects),
}

impl ObjectsBackend {
    /// Resolve `hex` to a filesystem path, or `None` if absent.
    ///
    /// LOCAL-ONLY: this returns a filesystem `PathBuf` and therefore cannot be
    /// implemented by a cloud backend. It is used only by `open_read_by_storage_key`
    /// and `objects_path`, never by the per-method storage operations below. For
    /// the cloud variant it always returns `None` (those local-only callers never
    /// operate on a cloud-backed `LocalStore`).
    fn find(&self, hex: &str) -> Option<PathBuf> {
        match self {
            ObjectsBackend::Local(d) => d.find(hex),
            ObjectsBackend::Remote(d) => d.find(hex),
            ObjectsBackend::Cloud(_) => None,
        }
    }

    fn write_stream(&self, hex: &str, reader: &mut dyn io::Read) -> Result<(), Error> {
        match self {
            ObjectsBackend::Local(d) => d.write_stream(hex, reader),
            ObjectsBackend::Remote(d) => d.write_stream(hex, reader),
            ObjectsBackend::Cloud(c) => c.write_stream(hex, reader),
        }
    }

    /// Local and remote-directory backends never fail an existence check (a
    /// filesystem stat either finds the entry or doesn't); only the cloud
    /// backend can genuinely error (network/auth failure, distinct from a 404).
    fn exists(&self, hex: &str) -> Result<bool, Error> {
        match self {
            ObjectsBackend::Local(d) => Ok(d.exists(hex)),
            ObjectsBackend::Remote(d) => Ok(d.exists(hex)),
            ObjectsBackend::Cloud(c) => c.exists(hex),
        }
    }

    /// Stored byte length of the object addressed by `hex`. Errors if absent.
    /// Per-method dispatch (no PathBuf in the public surface): the local backends
    /// stat the filesystem path; the cloud backend issues a HEAD.
    fn size(&self, hex: &str) -> Result<u64, Error> {
        match self {
            ObjectsBackend::Cloud(c) => c.size(hex),
            _ => {
                let path = self
                    .find(hex)
                    .ok_or_else(|| Error::ObjectNotFound(hex.to_string()))?;
                let meta = fs::metadata(&path).map_err(Error::Io)?;
                Ok(meta.len())
            }
        }
    }

    /// Open the stored bytes for `hex` as a stream. Errors if absent.
    /// Per-method dispatch: the local backends open the file; the cloud backend
    /// issues a GetObject and wraps the collected bytes.
    fn open_read(&self, hex: &str) -> Result<Box<dyn io::Read>, Error> {
        match self {
            ObjectsBackend::Cloud(c) => c.open_read(hex),
            _ => {
                let path = self
                    .find(hex)
                    .ok_or_else(|| Error::ObjectNotFound(hex.to_string()))?;
                let file = fs::File::open(&path).map_err(Error::Io)?;
                Ok(Box::new(file))
            }
        }
    }

    /// List every stored object as (storage_key_hex, byte_size). Per-method
    /// dispatch: the local backends walk the tree and stat each file; the cloud
    /// backend does a single paginated LIST (size from the LIST response).
    fn list_with_sizes(&self) -> Result<Vec<(String, u64)>, Error> {
        if let ObjectsBackend::Cloud(c) = self {
            return c.list_with_sizes();
        }
        let hexes = self.iter_hashes();
        let mut result = Vec::with_capacity(hexes.len());
        for hex in hexes {
            let path = match self.find(&hex) {
                Some(p) => p,
                None => continue, // disappeared between iter and stat; skip
            };
            let len = match fs::metadata(&path) {
                Ok(m) => m.len(),
                Err(_) => continue, // unreadable; skip without failing the whole listing
            };
            result.push((hex, len));
        }
        Ok(result)
    }

    fn iter_hashes(&self) -> Vec<String> {
        match self {
            ObjectsBackend::Local(d) => d.iter_hashes(),
            ObjectsBackend::Remote(d) => d.iter_hashes(),
            ObjectsBackend::Cloud(c) => c.iter_hashes(),
        }
    }
}

#[derive(Clone)]
pub struct LocalStore {
    objects: ObjectsBackend,
    /// Present only on remote stores when encryption is configured.
    pub(crate) encrypt_key: Option<EncryptKey>,
}

impl LocalStore {
    pub fn for_cache(objects_root: impl Into<PathBuf>) -> Self {
        LocalStore {
            objects: ObjectsBackend::Local(ObjectsDir::new(objects_root)),
            encrypt_key: None,
        }
    }

    pub fn for_remote(base: impl Into<PathBuf>) -> Self {
        let base = base.into();
        LocalStore {
            objects: ObjectsBackend::Remote(RemoteObjectsDir::new(base.join("objects"))),
            encrypt_key: None,
        }
    }

    pub fn for_remote_encrypted(base: impl Into<PathBuf>, key: EncryptKey) -> Self {
        let base = base.into();
        LocalStore {
            objects: ObjectsBackend::Remote(RemoteObjectsDir::new(base.join("objects"))),
            encrypt_key: Some(key),
        }
    }

    /// Wrap a cloud object store (S3 / Azure / GCS) as a remote `LocalStore`.
    ///
    /// The storage-key HMAC stays in this wrapper (see the encryption-layering
    /// decision in `private/s3-backend-plan.md` section B item 7), so the cloud
    /// adapter receives already-derived storage-key hex and applies only the
    /// shared hex->cloud-key layout. `objects` already carries the configured
    /// prefix.
    pub fn for_cloud(objects: CloudObjects, key: Option<EncryptKey>) -> Self {
        LocalStore {
            objects: ObjectsBackend::Cloud(objects),
            encrypt_key: key,
        }
    }

    /// Iterate all stored hashes. Used for prefix-search in `cat`.
    pub fn iter_hashes(&self) -> Vec<String> {
        self.objects.iter_hashes()
    }

    /// Open the stored bytes for an object identified by its raw storage-key hex.
    /// Unlike `open_read`, this bypasses the logical→storage-key derivation and
    /// reads the file directly. Used by `stats` to read objects from encrypted
    /// remote stores where only the storage key is known.
    pub fn open_read_by_storage_key(
        &self,
        storage_key_hex: &str,
    ) -> Result<Box<dyn io::Read>, Error> {
        let path = match self.objects.find(storage_key_hex) {
            Some(p) => p,
            None => return Err(Error::ObjectNotFound(storage_key_hex.to_string())),
        };
        let file = fs::File::open(&path).map_err(Error::Io)?;
        Ok(Box::new(file))
    }

    /// Return the filesystem path where `hash` is stored, or `None` if absent.
    /// Uses the storage key derived from `hash` for the lookup.
    #[allow(dead_code)]
    pub fn objects_path(&self, hash: &Hash) -> Option<PathBuf> {
        self.objects.find(self.storage_key(hash).as_str())
    }

    /// Return the storage key for `logical`: HMAC-SHA256(DEK, logical) when encryption
    /// is configured, or `logical` itself when encryption is not configured.
    pub fn storage_key_of(&self, logical: &Hash) -> Hash {
        self.storage_key(logical)
    }

    fn storage_key(&self, logical: &Hash) -> Hash {
        match &self.encrypt_key {
            None => logical.clone(),
            Some(k) => hmac_sha256_msg(&k.bytes, logical.as_bytes_array()),
        }
    }
}

impl ObjectStore for LocalStore {
    fn store_name(&self) -> &str {
        match &self.objects {
            ObjectsBackend::Local(_) => "local",
            ObjectsBackend::Remote(_) => "remote",
            ObjectsBackend::Cloud(_) => "remote",
        }
    }

    fn default_transfer_concurrency(&self) -> usize {
        match &self.objects {
            // Cloud transfers are network-bound: overlap round-trips. 8 is a
            // moderate default; peak memory is bounded separately by the byte
            // budget (OMEMFS_TRANSFER_MEMORY_BUDGET), not by this worker count
            // (design/02 "Two independent knobs"). Override via
            // OMEMFS_TRANSFER_CONCURRENCY.
            ObjectsBackend::Cloud(_) => 8,
            // Local-filesystem backends stay serial (byte-identical path).
            ObjectsBackend::Local(_) | ObjectsBackend::Remote(_) => 1,
        }
    }

    fn exists(&self, hash: &Hash) -> Result<bool, Error> {
        self.objects.exists(self.storage_key(hash).as_str())
    }

    fn size(&self, hash: &Hash) -> Result<u64, Error> {
        let key = self.storage_key(hash);
        // The per-method dispatch reports absence keyed on the storage-key hex;
        // rewrite that to the logical hash for the caller-facing message
        // (byte-identical to the previous behavior). Other errors (e.g. Io) pass
        // through unchanged.
        self.objects
            .size(key.as_str())
            .map_err(|e| relabel_not_found(e, hash))
    }

    fn list_with_sizes(&self) -> Result<Vec<(String, u64)>, Error> {
        self.objects.list_with_sizes()
    }

    fn open_read(&self, hash: &Hash) -> Result<Box<dyn io::Read>, Error> {
        let key = self.storage_key(hash);
        self.objects
            .open_read(key.as_str())
            .map_err(|e| relabel_not_found(e, hash))
    }

    fn write_from(&self, hash: &Hash, reader: &mut dyn io::Read) -> Result<(), Error> {
        let key = self.storage_key(hash);
        let _t = dtimer_l7!("write_from ({})", self.store_name());
        // No existence pre-check here: every ObjectsBackend variant's own
        // write_stream already skips the write when the key is present
        // (ObjectsDir / RemoteObjectsDir stat the local path directly; for
        // Cloud, CloudObjects::write_stream performs the one HEAD that
        // decides whether to call put at all). A pre-check here was
        // previously duplicating that same check -- for a cloud remote,
        // tripling the HEAD requests per new object (refactor-instructions.md
        // F4): once here, once in CloudObjects::write_stream, once more
        // inside the backend's own put().
        self.objects.write_stream(key.as_str(), reader)
    }
}

/// Flush all dirty pages in the filesystem that contains `objects_dir` to durable
/// storage. Called once, immediately before writing `clone_root` or `STAT_CACHE`,
/// to ensure that all local cache objects written since the last barrier are on
/// stable media before any pointer that references them is persisted.
///
/// On Linux: `syncfs(2)` on the objects directory file descriptor.
/// On other platforms: `sync(2)` (global sync, conservative fallback).
/// If `objects_dir` does not exist, the call is a no-op (nothing to flush).
pub fn sync_local_objects_fs(objects_dir: &Path) -> Result<(), Error> {
    // Fall back to the parent directory when objects_dir is not yet created.
    let path = if objects_dir.exists() {
        objects_dir
    } else if let Some(p) = objects_dir.parent() {
        if p.exists() {
            p
        } else {
            return Ok(());
        }
    } else {
        return Ok(());
    };

    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        let f = fs::File::open(path).map_err(Error::Io)?;
        let ret = unsafe { libc::syncfs(f.as_raw_fd()) };
        if ret != 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        unsafe {
            libc::sync();
        }
    }
    Ok(())
}

/// Write `data` to `path` atomically and durably (fsync before and after rename).
///
/// Use for `clone_root`, `config`, remote `objects/**`, and stub files — any state
/// that must survive a power failure independently. Local cache `objects/**` uses
/// `atomic_write_no_fsync` instead; a durability barrier covers them in bulk.
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<(), Error> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    sweep_stale_temp_files(dir);
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(data)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| Error::Io(e.error))?;
    // Flush the directory entry so the rename survives a power failure.
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}

/// Write `data` to `path` atomically but without fsync.
///
/// The rename is OS-atomic so the file is never partially written, but power
/// loss before the page cache flushes may revert it to the previous value.
/// Use only for pure acceleration caches (e.g. `STAT_CACHE`).
pub fn atomic_write_no_fsync(path: &Path, data: &[u8]) -> Result<(), Error> {
    atomic_write_with_no_fsync(path, |w| w.write_all(data).map_err(Error::Io))
}

/// Atomically write to `path` using a caller-supplied writer callback, without
/// fsync. Same temp+rename+stale-sweep contract as `atomic_write_no_fsync`, but
/// the content is produced incrementally by `write_fn` instead of held whole in
/// memory. Used by the streaming read/materialisation path so peak memory stays
/// bounded at roughly one chunk. On any error from `write_fn` the temp file is
/// dropped (deleted) and the destination path is left untouched (no partial
/// file is ever visible at `path`).
pub fn atomic_write_with_no_fsync<F>(path: &Path, write_fn: F) -> Result<(), Error>
where
    F: FnOnce(&mut dyn io::Write) -> Result<(), Error>,
{
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    sweep_stale_temp_files(dir);
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    {
        let file = tmp.as_file_mut();
        let mut writer = io::BufWriter::new(file);
        write_fn(&mut writer)?;
        writer.flush().map_err(Error::Io)?;
    }
    tmp.persist(path).map_err(|e| Error::Io(e.error))?;
    Ok(())
}

/// Maximum age before a leftover temp file is considered stale and removed.
const STALE_TEMP_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Opportunistically remove stale temp files in `dir` before creating a new
/// temp file there. A temp file is one whose name begins with the
/// `tempfile::NamedTempFile` default prefix (`.tmp`); only such files older than
/// 24 hours are removed. Non-`.tmp*` files are never touched. All errors are
/// ignored — this is a best-effort cleanup with no correctness impact (a
/// surviving stale temp file is harmless; the final path is always either the
/// old or the new content).
///
/// See `design/11_crash_safety.md` (temp files / stale-temp cleanup).
pub fn sweep_stale_temp_files(dir: &Path) {
    let now = std::time::SystemTime::now();
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    for entry in rd.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Match the NamedTempFile default prefix `.tmp` and only that prefix.
        if !name.starts_with(".tmp") {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.is_file() {
            continue;
        }
        let modified = match meta.modified() {
            Ok(t) => t,
            Err(_) => continue,
        };
        let age = match now.duration_since(modified) {
            Ok(a) => a,
            Err(_) => continue, // mtime in the future: treat as fresh
        };
        if age >= STALE_TEMP_MAX_AGE {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Rewrite an `ObjectNotFound` error (keyed on the storage-key hex by the
/// per-method `ObjectsBackend` dispatch) so it names the logical `hash` instead,
/// matching the historical caller-facing message. All other errors pass through.
fn relabel_not_found(e: Error, hash: &Hash) -> Error {
    match e {
        Error::ObjectNotFound(_) => Error::ObjectNotFound(hash.as_str().to_string()),
        other => other,
    }
}

/// Compute `HMAC-SHA256(key, message)` over an arbitrary-length message and
/// return it as a `Hash`. Used both for content-object storage keys (message =
/// 32-byte logical hash) and for the encrypted-remote index root name (message =
/// the ASCII context string `"omemfs:index-root:v1"`).
pub fn hmac_sha256_msg(key: &[u8; 32], message: &[u8]) -> Hash {
    let mut mac = <Hmac<Sha256>>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(message);
    let result: [u8; 32] = mac.finalize().into_bytes().into();
    Hash::from_bytes(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::cloud::{CloudObjects, MemCloud};
    use std::sync::Arc;
    use tempfile::TempDir;

    #[test]
    fn cloud_store_roundtrip_unencrypted() {
        // A cloud-backed LocalStore over the in-memory MemCloud fake: the
        // storage key equals the logical hash (no encryption), and the object
        // lands at the sharded objects/ cloud key.
        let cloud = Arc::new(MemCloud::new());
        let objects = CloudObjects::new(cloud.clone(), "repo");
        let store = LocalStore::for_cloud(objects, None);

        let data = b"cloud payload";
        let hash = crate::object::Hash::compute(data);
        assert!(!store.exists(&hash).unwrap());
        store
            .write_from(&hash, &mut std::io::Cursor::new(data))
            .unwrap();
        assert!(store.exists(&hash).unwrap());
        assert_eq!(store.size(&hash).unwrap(), data.len() as u64);

        let mut out = Vec::new();
        store
            .open_read(&hash)
            .unwrap()
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out, data);

        // It appears in the single-LIST listing keyed on the storage-key hex.
        let listed = store.list_with_sizes().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].0, hash.as_str());
        assert_eq!(listed[0].1, data.len() as u64);
    }

    #[test]
    fn cloud_store_roundtrip_encrypted_uses_hmac_storage_key() {
        // With encryption, the cloud adapter must receive the HMAC-derived
        // storage key (NOT the logical hash) — the HMAC stays in LocalStore.
        let cloud = Arc::new(MemCloud::new());
        let objects = CloudObjects::new(cloud.clone(), "repo");
        let key = EncryptKey::new([7u8; 32]);
        let store = LocalStore::for_cloud(objects, Some(key.clone()));

        let data = b"encrypted cloud payload";
        let hash = crate::object::Hash::compute(data);
        store
            .write_from(&hash, &mut std::io::Cursor::new(data))
            .unwrap();

        // The listed storage key is the HMAC of the logical hash, not the hash.
        let storage_key = store.storage_key_of(&hash);
        assert_ne!(storage_key.as_str(), hash.as_str());
        let listed = store.list_with_sizes().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].0, storage_key.as_str());

        // Read-back goes through the same HMAC mapping.
        let mut out = Vec::new();
        store
            .open_read(&hash)
            .unwrap()
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn cloud_store_write_is_idempotent() {
        let cloud = Arc::new(MemCloud::new());
        let objects = CloudObjects::new(cloud.clone(), "repo");
        let store = LocalStore::for_cloud(objects, None);
        let data = b"idem";
        let hash = crate::object::Hash::compute(data);
        store
            .write_from(&hash, &mut std::io::Cursor::new(data))
            .unwrap();
        // A second write of different bytes is a no-op (content-addressed).
        store
            .write_from(&hash, &mut std::io::Cursor::new(b"other"))
            .unwrap();
        let mut out = Vec::new();
        store
            .open_read(&hash)
            .unwrap()
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn write_from_roundtrip_local() {
        let tmp = TempDir::new().unwrap();
        let store = LocalStore::for_cache(tmp.path());
        let data = b"test payload";
        let hash = crate::object::Hash::compute(data);
        store
            .write_from(&hash, &mut std::io::Cursor::new(data))
            .unwrap();
        assert!(store.exists(&hash).unwrap());
        let mut out = Vec::new();
        store
            .open_read(&hash)
            .unwrap()
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn write_from_idempotent() {
        let tmp = TempDir::new().unwrap();
        let store = LocalStore::for_cache(tmp.path());
        let data = b"idempotent";
        let hash = crate::object::Hash::compute(data);
        store
            .write_from(&hash, &mut std::io::Cursor::new(data))
            .unwrap();
        store
            .write_from(&hash, &mut std::io::Cursor::new(b"other"))
            .unwrap();
        let mut out = Vec::new();
        store
            .open_read(&hash)
            .unwrap()
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn sweep_removes_stale_temp_keeps_fresh_and_non_temp() {
        use filetime::{FileTime, set_file_mtime};
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        // A stale `.tmp*` file (mtime 25 hours ago) -> must be removed.
        let stale = dir.join(".tmpSTALE");
        fs::write(&stale, b"old").unwrap();
        let old = FileTime::from_unix_time(FileTime::now().unix_seconds() - 25 * 60 * 60, 0);
        set_file_mtime(&stale, old).unwrap();

        // A fresh `.tmp*` file -> must be kept.
        let fresh = dir.join(".tmpFRESH");
        fs::write(&fresh, b"new").unwrap();

        // A non-`.tmp*` file, even if old, must never be touched.
        let regular = dir.join("clone_root");
        fs::write(&regular, b"data").unwrap();
        set_file_mtime(&regular, old).unwrap();

        sweep_stale_temp_files(dir);

        assert!(!stale.exists(), "stale .tmp file should be removed");
        assert!(fresh.exists(), "fresh .tmp file should be kept");
        assert!(regular.exists(), "non-.tmp file must never be removed");
    }

    #[test]
    fn atomic_write_sweeps_stale_temp() {
        use filetime::{FileTime, set_file_mtime};
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let stale = dir.join(".tmpOLD");
        fs::write(&stale, b"old").unwrap();
        let old = FileTime::from_unix_time(FileTime::now().unix_seconds() - 25 * 60 * 60, 0);
        set_file_mtime(&stale, old).unwrap();

        // A write into the same directory triggers the sweep.
        atomic_write(&dir.join("dest"), b"payload").unwrap();
        assert!(
            !stale.exists(),
            "atomic_write should sweep stale temp files"
        );
    }

    #[test]
    fn sync_local_objects_fs_smoke() {
        let tmp = TempDir::new().unwrap();
        // Existing directory: should succeed.
        sync_local_objects_fs(tmp.path()).unwrap();
        // Non-existent subdirectory: should be a no-op.
        let absent = tmp.path().join("absent");
        sync_local_objects_fs(&absent).unwrap();
    }

    #[test]
    fn list_with_sizes_returns_all_objects() {
        let tmp = TempDir::new().unwrap();
        let store = LocalStore::for_cache(tmp.path());

        // Write three objects of distinct lengths.
        let payloads: &[&[u8]] = &[b"a", b"hello world", b"the quick brown fox"];
        let mut expected: Vec<(String, u64)> = Vec::new();
        for &data in payloads {
            let hash = crate::object::Hash::compute(data);
            store
                .write_from(&hash, &mut std::io::Cursor::new(data))
                .unwrap();
            // For an unencrypted local store the storage key equals the logical hash.
            expected.push((hash.as_str().to_string(), data.len() as u64));
        }

        let result = store.list_with_sizes().unwrap();

        // All three objects must appear with correct sizes.
        assert_eq!(
            result.len(),
            expected.len(),
            "count must match number of written objects"
        );
        for (hex, size) in &expected {
            assert!(
                result.iter().any(|(h, s)| h == hex && *s == *size),
                "missing or wrong size for object {}",
                hex,
            );
        }
    }

    #[test]
    fn size_returns_stored_byte_length() {
        let tmp = TempDir::new().unwrap();
        let store = LocalStore::for_cache(tmp.path());
        let data = b"hello size test";
        let hash = crate::object::Hash::compute(data);

        // Object absent before write.
        assert!(
            store.size(&hash).is_err(),
            "size of absent object must be Err"
        );

        // Write and verify size matches the stored length.
        store
            .write_from(&hash, &mut std::io::Cursor::new(data))
            .unwrap();
        let stored_size = store.size(&hash).unwrap();
        assert_eq!(
            stored_size,
            data.len() as u64,
            "size must equal the number of bytes written"
        );
    }
}
