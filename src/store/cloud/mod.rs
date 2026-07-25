//! Shared infrastructure for the cloud object-storage backends (S3 / Azure /
//! GCS).
//!
//! The `ObjectStore` and `RootPointer` traits are synchronous, but all three
//! cloud SDKs are async. The bridge between the two lives here: a shared
//! multi-thread tokio runtime, the async-body -> `Box<dyn io::Read>` collect
//! bridge, the sync `write_from` buffering helper, and the hex <-> cloud-key
//! layout helpers. The three Phase-3 adapters (`s3.rs`, `azure.rs`, `gcs.rs`)
//! reuse everything here so each adapter is thin and differs only in the three
//! per-backend points documented in `design/13_cloud_backends.md`: client
//! construction/auth, the conditional-precondition API, and the 412 ->
//! `Error::CasFailed` mapping.
//!
//! Async MUST NOT leak past a backend's `.rs` file: no `ObjectStore` /
//! `RootPointer` method returns a future, and a backend method must never be
//! called from inside a runtime worker thread (that would deadlock `block_on`).
//!
//! Phase 3 status: `s3.rs` / `azure.rs` / `gcs.rs` now implement
//! [`CloudObjectIo`] and are wired into `Repo::remote_store` /
//! `Repo::remote_root_pointer` via the [`CloudObjects`] adapter and the cloud
//! `RootPointer` impls. The shared helpers below (runtime, collect/buffer
//! bridges, key-layout helpers, [`CloudObjects`]) are all referenced from the
//! non-test build now.

use std::io;
use std::sync::Arc;

use bytes::Bytes;

use crate::codec::encrypt::EncryptKey;
use crate::error::Error;

pub mod azure;
pub mod gcs;
pub mod s3;

#[cfg(test)]
mod mem;

// Re-exported for Phase 3/4 adapter tests; the fakes are referenced via
// `super::` inside `mem`'s own test battery today.
#[cfg(test)]
#[allow(unused_imports)]
pub use mem::{MemCloud, MemCloudRootPointer};

/// Multipart / staged-upload threshold. For Phase 2 every write is a single
/// buffered PUT regardless of size; the cloud adapters in Phase 3 must switch
/// to a multipart / staged upload above this threshold so a single object never
/// holds more than ~16 MiB resident per worker. The async->sync collect bridge
/// is likewise bounded at this size for v1 (collect-to-memory).
pub const MULTIPART_THRESHOLD: usize = 16 * 1024 * 1024;

/// A shared, multi-thread tokio runtime that the cloud backends use to drive
/// their async SDK calls to completion via [`CloudRuntime::block_on`].
///
/// **Why multi_thread.** The `ObjectStore` trait is synchronous and the
/// transfer loops (clone / push / pull) run N worker threads that each call
/// `ObjectStore` methods directly. Each such call does a `block_on` on this one
/// shared runtime. A multi-thread runtime lets those concurrent `block_on`
/// calls make progress in parallel (a current-thread runtime would serialise
/// them on the single calling thread and a single internal driver). `enable_all`
/// turns on the IO and time drivers the SDK HTTP clients need.
///
/// **Hard rule.** Callers must never call `block_on` from inside a runtime
/// worker thread (i.e. from within a future already running on this runtime):
/// that re-entrant block would deadlock. In practice every `ObjectStore` method
/// is invoked from an ordinary OS thread (the CLI thread or a transfer worker),
/// never from inside a spawned task, so this holds by construction.
pub struct CloudRuntime {
    rt: Arc<tokio::runtime::Runtime>,
}

impl CloudRuntime {
    /// Build a new shared multi-thread runtime with all drivers enabled.
    pub fn new() -> Result<Self, Error> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| Error::Other(format!("failed to build tokio runtime: {e}")))?;
        Ok(CloudRuntime { rt: Arc::new(rt) })
    }

    /// Drive `future` to completion on the shared runtime, blocking the calling
    /// (non-worker) thread until it resolves. See the type-level docs for the
    /// "never from a worker thread" rule.
    ///
    /// Debug builds assert that rule instead of leaving it convention-only: a
    /// call from inside one of this runtime's own worker threads would
    /// re-entrantly block that worker forever (a deadlock, not a panic) since
    /// Tokio version 1's `block_on` refuses to run inside another runtime's
    /// context. `Handle::try_current()` succeeds only when called from a task
    /// already running on *some* runtime, which -- given the hard rule that no
    /// `ObjectStore` / `RootPointer` method ever spawns a task on this runtime
    /// -- can only mean the caller is itself running on this runtime's pool
    /// (design/13 "One runtime per process, not one per call").
    pub fn block_on<F: std::future::Future>(&self, future: F) -> F::Output {
        debug_assert!(
            tokio::runtime::Handle::try_current().is_err(),
            "CloudRuntime::block_on called from inside a runtime worker thread -- this would deadlock in release builds"
        );
        self.rt.block_on(future)
    }
}

/// Wrap owned bytes (already collected by the backend via `block_on`) as a
/// synchronous `Box<dyn io::Read>`.
///
/// v1 strategy: the backend collects the whole object body into memory (a
/// `Bytes`) and hands it here, where it becomes a `Cursor`. This is bounded by
/// the `MULTIPART_THRESHOLD` chunking for writes; for reads of larger objects a
/// follow-up streaming adapter (a `Read` that pulls the next chunk on demand by
/// re-entering `block_on`) should replace this collect-to-memory path so peak
/// memory stays at roughly one chunk rather than a whole pack.
pub fn bytes_reader(body: Bytes) -> Box<dyn io::Read> {
    Box::new(io::Cursor::new(body))
}

/// Buffer an entire `reader` into a `Vec<u8>` for a single PUT / upload.
///
/// The cloud `write_from` path is synchronous and the SDKs want an owned body,
/// so the bytes are read fully into memory first. Phase 3 must consult
/// [`MULTIPART_THRESHOLD`] and switch to a multipart / staged upload when the
/// buffered length exceeds it, so a single object never holds more than
/// ~16 MiB resident per worker.
pub fn buffer_for_put(reader: &mut dyn io::Read) -> Result<Vec<u8>, Error> {
    let mut buf = Vec::new();
    io::copy(reader, &mut buf).map_err(Error::Io)?;
    Ok(buf)
}

// ---------------------------------------------------------------------------
// hex <-> cloud-key layout helpers
// ---------------------------------------------------------------------------
//
// All cloud backends use the identical key layout, mirroring the local
// `RemoteObjectsDir` sharding (`src/store/remote_objects_dir.rs`) and the
// `index_root_path` derivation (`src/codec/pack/mod.rs`):
//
//   <prefix>/objects/<hex[0..2]>/<hex[2..4]>/<hex[4..6]>/<hex[6..64]>
//
// On a cloud store `/` is just an ordinary character in a flat key/blob/object
// name; there are no real directories.

/// Path segment under which content objects live, shared by all backends.
const OBJECTS_SEGMENT: &str = "objects";

/// Normalize a configured prefix: strip any leading/trailing `/` so the joined
/// key never contains a doubled or leading slash. An empty prefix is allowed
/// (keys then start at `objects/...`).
fn normalize_prefix(prefix: &str) -> &str {
    prefix.trim_matches('/')
}

/// Join a normalized prefix with the rest of a key, handling the empty-prefix
/// case (no leading slash).
fn join_prefix(prefix: &str, rest: &str) -> String {
    let p = normalize_prefix(prefix);
    if p.is_empty() {
        rest.to_string()
    } else {
        format!("{p}/{rest}")
    }
}

/// Map a 64-hex storage key to the cloud object key
/// `<prefix>/objects/<2>/<2>/<2>/<58>`. Returns `None` if `hex` is not exactly
/// 64 characters (mirroring `RemoteObjectsDir::expected_path`).
pub fn storage_key_to_cloud_key(prefix: &str, hex: &str) -> Option<String> {
    if hex.len() != 64 {
        return None;
    }
    let sharded = format!(
        "{}/{}/{}/{}/{}",
        OBJECTS_SEGMENT,
        &hex[0..2],
        &hex[2..4],
        &hex[4..6],
        &hex[6..],
    );
    Some(join_prefix(prefix, &sharded))
}

/// Inverse of [`storage_key_to_cloud_key`]: parse a cloud object key back to its
/// 64-hex storage key. Returns `None` for any key that is not a content object
/// under `<prefix>/objects/` with the exact 2/2/2/58 sharding — in particular
/// the index-root keys (`<prefix>/INDEX_ROOT` or the derived sharded encrypted
/// name, which has no `objects/` segment) are rejected. Used by
/// `list_with_sizes` to skip non-object keys returned by a LIST.
pub fn cloud_key_to_storage_key(prefix: &str, key: &str) -> Option<String> {
    let p = normalize_prefix(prefix);
    // Strip the prefix (and the single separating slash) if present.
    let rest = if p.is_empty() {
        key
    } else {
        key.strip_prefix(p)?.strip_prefix('/')?
    };
    let rest = rest.strip_prefix(OBJECTS_SEGMENT)?.strip_prefix('/')?;
    let parts: Vec<&str> = rest.split('/').collect();
    if parts.len() != 4 {
        return None;
    }
    if parts[0].len() != 2 || parts[1].len() != 2 || parts[2].len() != 2 || parts[3].len() != 58 {
        return None;
    }
    let joined = format!("{}{}{}{}", parts[0], parts[1], parts[2], parts[3]);
    if joined.len() != 64 || !joined.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(joined)
}

/// Derive the cloud key of the index root for a backend.
///
/// - Unencrypted (`key = None`): the flat key `<prefix>/INDEX_ROOT`, mirroring
///   the local `INDEX_ROOT_FILE`.
/// - Encrypted (`key = Some`): the derived sharded name
///   `HMAC-SHA256(DEK, "omemfs:index-root:v1")` placed under `objects/` exactly
///   like a content object (reusing `crate::codec::pack::index_root_name`), so
///   it is byte-identical to the local encrypted index-root path.
pub fn index_root_cloud_key(prefix: &str, key: Option<&EncryptKey>) -> String {
    match key {
        None => join_prefix(prefix, crate::codec::pack::INDEX_ROOT_FILE),
        Some(k) => {
            let name = crate::codec::pack::index_root_name(k);
            // The derived name is itself a 64-hex storage key; it shards under
            // objects/ identically to a content object.
            storage_key_to_cloud_key(prefix, name.as_str())
                .expect("index_root_name is always 64 hex chars")
        }
    }
}

// ---------------------------------------------------------------------------
// Per-backend IO boundary
// ---------------------------------------------------------------------------

/// The thin per-backend object-IO boundary that the three Phase-3 cloud
/// adapters (S3 / Azure / GCS) implement.
///
/// **Why a trait (not free functions).** The plan recommends the small-trait
/// approach so the Phase-3 adapters stay thin: each backend implements only
/// these five object ops plus the conditional-put for CAS, and all the shared
/// glue (runtime, collect bridge, key layout, the `ObjectStore` /
/// `RootPointer` impls that call into this trait) lives once in
/// `src/store/cloud/`. The alternative — shared free functions the three
/// structs call — would force each struct to re-wire the same dispatch and
/// would not give the in-memory `MemCloud` fake a single seam to stand in for a
/// real backend in tests. A trait gives exactly that seam.
///
/// All methods are **synchronous**: an implementation built on an async SDK
/// does its `block_on` internally (using a [`CloudRuntime`]) and never exposes a
/// future. Keys passed here are already the fully-formed cloud keys produced by
/// the layout helpers above.
///
/// Phase 3 fills these in per backend; for Phase 2 only the in-memory
/// [`MemCloud`] fake implements it (used by the CAS battery and future
/// adapter tests).
pub trait CloudObjectIo: Send + Sync {
    /// `true` if an object exists at `key` (404 -> `false`).
    fn head_exists(&self, key: &str) -> Result<bool, Error>;

    /// Stored byte length of the object at `key`. Errors if absent.
    fn head_size(&self, key: &str) -> Result<u64, Error>;

    /// List `(key, size)` for every object under the `objects/` prefix in one
    /// paginated LIST. The caller maps each key back through
    /// [`cloud_key_to_storage_key`] and skips non-object keys.
    fn list(&self, objects_prefix: &str) -> Result<Vec<(String, u64)>, Error>;

    /// Download the whole object body at `key`, collected to memory.
    fn get(&self, key: &str) -> Result<Bytes, Error>;

    /// Upload `body` to `key`, UNCONDITIONALLY (no existence check).
    ///
    /// Implementations must not HEAD before uploading: `CloudObjects::write_stream`
    /// (the sole caller in production) already performs the one existence
    /// check that decides whether to call `put` at all. Content-addressed
    /// storage makes an unconditional put safe -- the same key always maps
    /// to the same bytes, so overwriting an existing object is a no-op in
    /// effect. Previously every implementation duplicated the same
    /// `head_exists` check `write_stream` had just performed, tripling the
    /// HEAD requests (and their cost/latency) per new object upload
    /// (refactor-instructions.md F4).
    fn put(&self, key: &str, body: Vec<u8>) -> Result<(), Error>;

    /// Conditional put for the index-root CAS. `expected_token` is `None` for
    /// "create only if absent" and `Some(token)` for "update only if the
    /// current version still equals `token`". On success returns the NEW
    /// version token of the written object. A precondition failure (HTTP 412 /
    /// ETag or generation mismatch) maps to `Error::CasFailed`.
    fn conditional_put(
        &self,
        key: &str,
        body: &[u8],
        expected_token: Option<&[u8]>,
    ) -> Result<Vec<u8>, Error>;

    /// Read the object at `key`, returning `(bytes, token)` where `token` is the
    /// current version identity (ETag / generation), or `None` if the object is
    /// absent. Used by the cloud `RootPointer::read`.
    fn get_with_token(&self, key: &str) -> Result<Option<(Vec<u8>, Vec<u8>)>, Error>;
}

/// Blanket `CloudObjectIo` impl for `Arc<T>`, so an `Arc`-shared backend (used
/// by the `MemCloud` test fake for concurrent access) can be plugged into
/// [`CloudRootPointer`] the same way an owned backend (`S3Backend` /
/// `AzureBackend` / `GcsBackend`) is.
impl<T: CloudObjectIo + ?Sized> CloudObjectIo for Arc<T> {
    fn head_exists(&self, key: &str) -> Result<bool, Error> {
        (**self).head_exists(key)
    }
    fn head_size(&self, key: &str) -> Result<u64, Error> {
        (**self).head_size(key)
    }
    fn list(&self, objects_prefix: &str) -> Result<Vec<(String, u64)>, Error> {
        (**self).list(objects_prefix)
    }
    fn get(&self, key: &str) -> Result<Bytes, Error> {
        (**self).get(key)
    }
    fn put(&self, key: &str, body: Vec<u8>) -> Result<(), Error> {
        (**self).put(key, body)
    }
    fn conditional_put(
        &self,
        key: &str,
        body: &[u8],
        expected_token: Option<&[u8]>,
    ) -> Result<Vec<u8>, Error> {
        (**self).conditional_put(key, body, expected_token)
    }
    fn get_with_token(&self, key: &str) -> Result<Option<(Vec<u8>, Vec<u8>)>, Error> {
        (**self).get_with_token(key)
    }
}

/// Generic [`RootPointer`](crate::codec::pack::root_pointer::RootPointer)
/// implementation over any [`CloudObjectIo`] backend, using the cloud CAS
/// model: `read` fetches the current bytes + opaque version token via
/// `get_with_token`; `cas_write` performs a `conditional_put` keyed on that
/// same token (`None` for create-only, `Some(token)` for update-if-current).
///
/// This is the single implementation shared by every cloud backend (S3 /
/// Azure / GCS) and the `MemCloud` test fake (via `CloudRootPointer<Arc<MemCloud>>`,
/// using the blanket `Arc<T>: CloudObjectIo` impl above) -- previously four
/// byte-identical copies named `S3RootPointer` / `AzureRootPointer` /
/// `GcsRootPointer` / `MemCloudRootPointer` (refactor-instructions.md E10).
pub struct CloudRootPointer<T: CloudObjectIo> {
    backend: T,
    key: String,
}

impl<T: CloudObjectIo> CloudRootPointer<T> {
    /// Build the root pointer for `prefix` (+ optional encryption key),
    /// targeting the index-root key directly (NOT through the storage-key
    /// HMAC). Pass a raw key (e.g. `"INDEX_ROOT"` in tests) via `for_key`
    /// instead when the caller already has the exact cloud key.
    pub fn new(backend: T, prefix: &str, enc: Option<&EncryptKey>) -> Self {
        let key = index_root_cloud_key(prefix, enc);
        CloudRootPointer { backend, key }
    }

    /// Build the root pointer for an already-resolved cloud `key`, bypassing
    /// `index_root_cloud_key` derivation. Used by tests that address a fixed
    /// key directly (e.g. `"INDEX_ROOT"`) rather than a prefix + encryption
    /// mode -- so unused in a plain (non-test) `cargo build`.
    #[allow(dead_code)]
    pub fn for_key(backend: T, key: impl Into<String>) -> Self {
        CloudRootPointer {
            backend,
            key: key.into(),
        }
    }
}

impl<T: CloudObjectIo> crate::codec::pack::root_pointer::RootPointer for CloudRootPointer<T> {
    fn read(
        &self,
    ) -> Result<(Option<Vec<u8>>, crate::codec::pack::root_pointer::RootToken), Error> {
        use crate::codec::pack::root_pointer::RootToken;
        match self.backend.get_with_token(&self.key)? {
            None => Ok((None, RootToken::Absent)),
            Some((bytes, token)) => Ok((Some(bytes), RootToken::Present(token))),
        }
    }

    fn cas_write(
        &self,
        expected: &crate::codec::pack::root_pointer::RootToken,
        new: &[u8],
    ) -> Result<(), Error> {
        use crate::codec::pack::root_pointer::RootToken;
        let expected_token = match expected {
            RootToken::Absent => None,
            RootToken::Present(t) => Some(t.as_slice()),
        };
        self.backend
            .conditional_put(&self.key, new, expected_token)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// CloudObjects: the storage-operations adapter over a CloudObjectIo backend
// ---------------------------------------------------------------------------

/// Adapts a per-backend [`CloudObjectIo`] implementation to the storage
/// operations the `ObjectStore` wrapper (`LocalStore`) needs, applying the
/// shared hex <-> cloud-key layout. It receives ALREADY-DERIVED storage-key hex
/// (the HMAC stays in `LocalStore`, exactly like `RemoteObjectsDir`) and only
/// maps that hex to a cloud key via [`storage_key_to_cloud_key`]; it never does
/// any HMAC itself.
///
/// This is the cloud analogue of `RemoteObjectsDir`: it exposes
/// `exists` / `size` / `open_read` / `write_stream` / `list_with_sizes`
/// / `iter_hashes` keyed on the 64-hex storage key, and the configured prefix
/// is applied here once.
#[derive(Clone)]
pub struct CloudObjects {
    io: Arc<dyn CloudObjectIo>,
    prefix: String,
}

impl CloudObjects {
    pub fn new(io: Arc<dyn CloudObjectIo>, prefix: impl Into<String>) -> Self {
        CloudObjects {
            io,
            prefix: prefix.into(),
        }
    }

    /// Map a 64-hex storage key to its cloud key under the configured prefix.
    fn cloud_key(&self, hex: &str) -> Result<String, Error> {
        storage_key_to_cloud_key(&self.prefix, hex)
            .ok_or_else(|| Error::Other(format!("invalid storage key length: {}", hex.len())))
    }

    /// The cloud key prefix under which content objects live
    /// (`<prefix>/objects/`) — the argument to a paginated LIST.
    fn objects_prefix(&self) -> String {
        join_prefix(&self.prefix, OBJECTS_SEGMENT)
    }

    /// `Ok(true)`/`Ok(false)` is a genuine existence answer (e.g. cloud HEAD
    /// 404 -> `Ok(false)`). A transient backend failure (network/auth error)
    /// is returned as `Err`, NOT coerced to `Ok(false)` -- callers must not
    /// treat "the backend broke" the same as "the object is absent"
    /// (refactor-instructions.md C2). Doing so previously caused unnecessary
    /// re-uploads on push and could surface as a spurious `ObjectNotFound`
    /// downstream (`PackReader::resolve` falls through to a real lookup when
    /// `exists` says "false").
    pub fn exists(&self, hex: &str) -> Result<bool, Error> {
        let key = self.cloud_key(hex)?;
        self.io.head_exists(&key)
    }

    pub fn size(&self, hex: &str) -> Result<u64, Error> {
        let key = self.cloud_key(hex)?;
        self.io.head_size(&key)
    }

    pub fn open_read(&self, hex: &str) -> Result<Box<dyn io::Read>, Error> {
        let key = self.cloud_key(hex)?;
        let body = self.io.get(&key)?;
        Ok(bytes_reader(body))
    }

    pub fn write_stream(&self, hex: &str, reader: &mut dyn io::Read) -> Result<(), Error> {
        let key = self.cloud_key(hex)?;
        // Idempotent (content-addressed): skip the upload if already present.
        if self.io.head_exists(&key)? {
            return Ok(());
        }
        let body = buffer_for_put(reader)?;
        self.io.put(&key, body)
    }

    /// One paginated LIST over `<prefix>/objects/`, mapping each returned cloud
    /// key back to its 64-hex storage key and skipping any non-object key.
    pub fn list_with_sizes(&self) -> Result<Vec<(String, u64)>, Error> {
        let listed = self.io.list(&self.objects_prefix())?;
        let mut out = Vec::with_capacity(listed.len());
        for (key, size) in listed {
            if let Some(hex) = cloud_key_to_storage_key(&self.prefix, &key) {
                out.push((hex, size));
            }
        }
        Ok(out)
    }

    /// Derive the stored storage-key hexes from a single LIST. Mirrors
    /// `RemoteObjectsDir::iter_hashes`, but built from the LIST result rather
    /// than a directory walk (there are no real directories on a cloud store).
    pub fn iter_hashes(&self) -> Vec<String> {
        self.list_with_sizes()
            .map(|v| v.into_iter().map(|(hex, _)| hex).collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::encrypt::EncryptKey;

    #[test]
    fn storage_key_round_trips_through_cloud_key() {
        let hex = "ab".repeat(32); // 64 hex chars
        for prefix in ["myrepo", "", "/leading/", "trailing/", "a/b/c"] {
            let key = storage_key_to_cloud_key(prefix, &hex).unwrap();
            assert!(key.ends_with(&format!(
                "objects/{}/{}/{}/{}",
                &hex[0..2],
                &hex[2..4],
                &hex[4..6],
                &hex[6..]
            )));
            let back = cloud_key_to_storage_key(prefix, &key).unwrap();
            assert_eq!(
                back, hex,
                "round-trip must recover the original hex (prefix {prefix:?})"
            );
        }
    }

    #[test]
    fn cloud_key_sharding_matches_local_layout() {
        // Mirror src/store/remote_objects_dir.rs: <2>/<2>/<2>/<58>.
        let hex = format!("{:064x}", 0x1234u32);
        let key = storage_key_to_cloud_key("p", &hex).unwrap();
        assert_eq!(
            key,
            format!(
                "p/objects/{}/{}/{}/{}",
                &hex[0..2],
                &hex[2..4],
                &hex[4..6],
                &hex[6..]
            )
        );
        let rest = key.strip_prefix("p/objects/").unwrap();
        let parts: Vec<&str> = rest.split('/').collect();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[3].len(), 58);
    }

    #[test]
    fn short_hex_is_rejected_by_forward_map() {
        assert!(storage_key_to_cloud_key("p", "tooshort").is_none());
    }

    #[test]
    fn index_root_keys_are_rejected_by_inverse() {
        // Unencrypted INDEX_ROOT must not parse back to a storage key.
        let unenc = index_root_cloud_key("p", None);
        assert_eq!(unenc, "p/INDEX_ROOT");
        assert!(cloud_key_to_storage_key("p", &unenc).is_none());

        // A bare INDEX_ROOT with empty prefix likewise.
        assert!(cloud_key_to_storage_key("", "INDEX_ROOT").is_none());
    }

    #[test]
    fn encrypted_index_root_key_is_sharded_under_objects() {
        let key = EncryptKey::new([3u8; 32]);
        let prefix = "repo";
        let cloud = index_root_cloud_key(prefix, Some(&key));
        // It is a sharded objects/ key derived from index_root_name.
        let name = crate::codec::pack::index_root_name(&key);
        let expected = storage_key_to_cloud_key(prefix, name.as_str()).unwrap();
        assert_eq!(cloud, expected);
        // And, being a real 64-hex object key, it DOES parse back (it is the
        // derived storage key, deliberately indistinguishable from a content
        // object — exactly the local encrypted-index-root behaviour).
        assert_eq!(
            cloud_key_to_storage_key(prefix, &cloud).unwrap(),
            name.as_str()
        );
    }

    #[test]
    fn inverse_rejects_wrong_prefix_and_malformed_keys() {
        let hex = "cd".repeat(32);
        let key = storage_key_to_cloud_key("right", &hex).unwrap();
        // Wrong prefix -> None.
        assert!(cloud_key_to_storage_key("wrong", &key).is_none());
        // Non-hex content in a well-shaped key -> None.
        let bad = "p/objects/zz/zz/zz/".to_string() + &"z".repeat(58);
        assert!(cloud_key_to_storage_key("p", &bad).is_none());
        // Wrong number of segments -> None.
        assert!(cloud_key_to_storage_key("p", "p/objects/ab/cd").is_none());
    }

    #[test]
    fn prefix_normalization_collapses_slashes() {
        let hex = "ef".repeat(32);
        let a = storage_key_to_cloud_key("p", &hex).unwrap();
        let b = storage_key_to_cloud_key("/p/", &hex).unwrap();
        let c = storage_key_to_cloud_key("p///", &hex).unwrap();
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert!(!a.contains("//"), "no doubled slashes in {a}");
    }

    // --- C2: exists() must propagate backend errors, not coerce to false ---

    #[test]
    fn exists_returns_false_for_genuinely_absent_object() {
        let cloud = std::sync::Arc::new(MemCloud::new());
        let objects = CloudObjects::new(cloud, "repo");
        let hex = "ab".repeat(32);
        assert!(!objects.exists(&hex).unwrap());
    }

    #[test]
    fn exists_returns_true_for_present_object() {
        let cloud = std::sync::Arc::new(MemCloud::new());
        let objects = CloudObjects::new(cloud, "repo");
        let hex = "cd".repeat(32);
        objects
            .write_stream(&hex, &mut io::Cursor::new(b"hello".to_vec()))
            .unwrap();
        assert!(objects.exists(&hex).unwrap());
    }

    #[test]
    fn exists_propagates_transient_backend_failure_as_err() {
        let cloud = std::sync::Arc::new(MemCloud::new());
        let objects = CloudObjects::new(cloud.clone(), "repo");
        let hex = "ef".repeat(32);
        cloud.set_fail_head_exists(true);
        // Must be Err, not Ok(false): a network/auth failure at exists() time
        // is not the same fact as "the object does not exist".
        assert!(
            objects.exists(&hex).is_err(),
            "backend failure must surface as Err"
        );
        cloud.set_fail_head_exists(false);
        assert!(!objects.exists(&hex).unwrap());
    }

    // --- F4: exactly one HEAD per new upload -------------------------------

    #[test]
    fn write_stream_of_a_new_object_does_exactly_one_head() {
        // refactor-instructions.md F4: write_stream must decide via a single
        // head_exists call whether to upload at all; CloudObjectIo::put must
        // not repeat the check (MemCloud's put has no such check to begin
        // with, so this specifically pins CloudObjects::write_stream's own
        // count, guarding against a future regression that reintroduces a
        // pre-check elsewhere in the write path).
        let cloud = std::sync::Arc::new(MemCloud::new());
        let objects = CloudObjects::new(cloud.clone(), "repo");
        let hex = "12".repeat(32);
        assert_eq!(cloud.head_exists_call_count(), 0);
        objects
            .write_stream(&hex, &mut io::Cursor::new(b"hello".to_vec()))
            .unwrap();
        assert_eq!(
            cloud.head_exists_call_count(),
            1,
            "exactly one HEAD for a new object upload"
        );
    }

    #[test]
    fn write_stream_of_an_existing_object_does_exactly_one_head_and_skips_the_put() {
        let cloud = std::sync::Arc::new(MemCloud::new());
        let objects = CloudObjects::new(cloud.clone(), "repo");
        let hex = "34".repeat(32);
        objects
            .write_stream(&hex, &mut io::Cursor::new(b"hello".to_vec()))
            .unwrap();
        let count_after_first = cloud.head_exists_call_count();

        // Second write_stream of the SAME key: still exactly one more HEAD,
        // and the body must not change (idempotent skip, not overwritten).
        objects
            .write_stream(&hex, &mut io::Cursor::new(b"different".to_vec()))
            .unwrap();
        assert_eq!(cloud.head_exists_call_count(), count_after_first + 1);
        use std::io::Read as _;
        let mut buf = Vec::new();
        objects
            .open_read(&hex)
            .unwrap()
            .read_to_end(&mut buf)
            .unwrap();
        assert_eq!(buf, b"hello");
    }
}
