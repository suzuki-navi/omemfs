/// Stage 5: pack reader.
///
/// PackReader wraps a raw remote ObjectStore and implements `open_read` using
/// the three-tier index (delta / hot / cold) instead of direct object lookup.
///
/// Search order for `open_read(target_hash)` (see design/02_storage_format.md
/// "Read path" for the full rationale behind steps 3a/4a below, Improvements
/// A and C):
///   1. Local objects/ cache — return immediately on hit.
///   2. Fetch INDEX_ROOT (local cache or remote file).
///   3. Search delta files newest-first (binary search within each).
///   4. Binary search the hot index.
///   3a. [Improvement A] If the covering cold shard is already cached
///       locally, search it now (a local disk read, no network call).
///   5. Check if objects/<hash> exists directly (standalone). Unconditional —
///      always runs, since a standalone object can be written to the remote
///      at any time, after the Bloom filter's snapshot was taken.
///   4a. [Improvement C] If a Bloom filter is recorded, consult it; a
///       "definitely absent" answer short-circuits straight to NotFound,
///       skipping ONLY the cold-shard fetch (step 6) — never step 5, which
///       already ran above.
///   6. Compute prefix → load cold shard → binary search (skipped if 3a
///      already searched this exact shard, or if 4a ruled it out).
///   7. Not found → error.
///
/// On index hit:
///   inline  → return data bytes (already encrypted; caller decrypts via codec).
///   pack    → fetch pack file → slice at [offset, offset+length) → return bytes.
///   standalone → fetch objects/<hash> directly from remote.
///
/// After fetching from remote, the encrypted bytes are stored in the local
/// objects/ cache (decrypt happens at the codec layer, not here).
use std::collections::HashMap;
use std::io::{self, Cursor, Read};
use std::sync::{Arc, Condvar, Mutex};

use crate::codec::encrypt::EncryptKey;
use crate::codec::pack::bloom::BloomFilter;
use crate::codec::pack::index::{IndexEntry, IndexFile};
use crate::codec::pack::index_root::IndexRoot;
use crate::codec::pack::root_pointer::RootPointer;
use crate::error::Error;
use crate::object::Hash;
use crate::store::ObjectStore;
use crate::store::local::LocalStore;

// Offset of pack body relative to the start of the pack file
// (i.e. skip the 2-byte ED E1 magic header).
const PACK_BODY_OFFSET: u64 = 2;

/// Strip the standalone escape prefix (ED E0) if present.
/// Standalone objects whose encrypted bytes began with ED E0..EF are stored
/// wrapped with this prefix by PackWriter to avoid ambiguity with L6 magics.
fn unescape_standalone(bytes: Vec<u8>) -> Vec<u8> {
    if bytes.len() >= 2 && bytes[0] == 0xED && bytes[1] == 0xE0 {
        bytes[2..].to_vec()
    } else {
        bytes
    }
}

/// Outcome of `PackReader::locate` (index-only lookup, see its doc comment).
#[derive(Debug)]
enum Located {
    /// Found in a delta/hot/cold index file. Caller still needs
    /// `resolve_entry` to fetch the actual bytes (inline data is already in
    /// hand; pack/standalone entries need a further fetch).
    Entry(IndexEntry),
    /// An INDEX_ROOT exists but `hash` is not referenced anywhere in it.
    NotFound,
    /// The remote has no INDEX_ROOT at all (never pushed, or push-in-progress).
    NoIndexRoot,
}

// ---------------------------------------------------------------------------
// PackReader
// ---------------------------------------------------------------------------

pub struct PackReader {
    remote: Box<dyn ObjectStore>,
    local_cache: LocalStore,
    /// Cache of whole remote pack files (raw, still-encrypted bytes).
    /// On the first slice read of a pack the entire pack is streamed from the
    /// remote into this store; all subsequent slices are served locally.
    packcache: LocalStore,
    /// Plaintext cache for decrypted remote index files (delta / hot / cold shards).
    /// Decrypted once on first fetch, stored as plaintext, never stale (content-addressed).
    objcache: LocalStore,
    /// Backend-pluggable index-root pointer. The pack layer no longer needs the
    /// remote base path: index-root reads route through this injected pointer,
    /// so a cloud backend can be slotted in without filesystem assumptions.
    root_pointer: Box<dyn RootPointer>,
    remote_key: Option<EncryptKey>,
    /// The published index-root snapshot retained for this reader's lifetime.
    /// `None` means it has not been loaded yet; the condition variable makes
    /// the first load single-flight when transfer workers start concurrently.
    index_root: Mutex<SnapshotState>,
    index_root_ready: Condvar,
    /// Parsed immutable indexes avoid an objcache disk read and deserialisation
    /// on every object lookup. The mutex intentionally serialises first loads;
    /// subsequent lookups only clone an Arc.
    indexes: Mutex<HashMap<Hash, Arc<IndexFile>>>,
    index_load: Mutex<()>,
    bloom: Mutex<HashMap<Hash, Arc<BloomFilter>>>,
    bloom_load: Mutex<()>,
    /// Serialises cache-miss pack installation so concurrent slices of one pack
    /// cannot download the same body repeatedly.
    pack_fetch: Mutex<()>,
}

enum SnapshotState {
    Empty,
    Loading,
    Ready(Option<IndexRoot>),
}

impl PackReader {
    pub fn new(
        remote: Box<dyn ObjectStore>,
        local_cache: LocalStore,
        packcache: LocalStore,
        objcache: LocalStore,
        root_pointer: Box<dyn RootPointer>,
        remote_key: Option<EncryptKey>,
    ) -> Self {
        PackReader {
            remote,
            local_cache,
            packcache,
            objcache,
            root_pointer,
            remote_key,
            index_root: Mutex::new(SnapshotState::Empty),
            index_root_ready: Condvar::new(),
            indexes: Mutex::new(HashMap::new()),
            index_load: Mutex::new(()),
            bloom: Mutex::new(HashMap::new()),
            bloom_load: Mutex::new(()),
            pack_fetch: Mutex::new(()),
        }
    }

    // -----------------------------------------------------------------------
    // INDEX_ROOT helpers
    // -----------------------------------------------------------------------

    fn decrypt_index_root(&self, raw: &[u8]) -> Result<Vec<u8>, Error> {
        crate::codec::pack::decrypt_index_root_bytes(raw, self.remote_key.as_ref())
    }

    /// Read the remote root tree hash from INDEX_ROOT.
    /// Returns `None` if the remote has no INDEX_ROOT yet (never pushed) or the
    /// recorded remote root is all-zero.
    pub fn read_root(&self) -> Result<Option<Hash>, Error> {
        match self.read_index_root()? {
            Some(ir) => Ok(ir.remote_root_hash()),
            None => Ok(None),
        }
    }

    pub fn read_index_root(&self) -> Result<Option<IndexRoot>, Error> {
        let mut state = self.index_root.lock().unwrap();
        loop {
            match &*state {
                SnapshotState::Ready(root) => return Ok(root.clone()),
                SnapshotState::Loading => state = self.index_root_ready.wait(state).unwrap(),
                SnapshotState::Empty => {
                    *state = SnapshotState::Loading;
                    break;
                }
            }
        }
        drop(state);
        // Route the index-root read through the same backend-pluggable root
        // pointer abstraction used by push (PackWriter) and pack, so all
        // index-root reads resolve the storage path identically.
        // This is a plain read (no CAS), so the version token is discarded.
        let loaded = (|| {
            let raw = match self.root_pointer.read()?.0 {
                Some(b) => b,
                None => return Ok(None),
            };
            let plaintext = self.decrypt_index_root(&raw)?;
            Ok(Some(IndexRoot::deserialise(&plaintext)?))
        })();
        let mut state = self.index_root.lock().unwrap();
        match loaded {
            Ok(root) => {
                *state = SnapshotState::Ready(root.clone());
                self.index_root_ready.notify_all();
                Ok(root)
            }
            Err(err) => {
                *state = SnapshotState::Empty;
                self.index_root_ready.notify_all();
                Err(err)
            }
        }
    }

    /// Resolve a hex `prefix` (4..=64 chars) to the full hashes of all stored
    /// objects whose storage key begins with it, by enumerating every index
    /// file (delta + hot + all cold shards). Intended for the `cat` diagnostic
    /// command, which lets the user paste the short hash shown by `ls`; it is
    /// not on any hot path. A full 64-char prefix yields at most one match.
    ///
    /// Returns the matches sorted and de-duplicated. An empty vector means no
    /// stored object matches the prefix.
    pub fn resolve_prefix(&self, prefix: &str) -> Result<Vec<Hash>, Error> {
        let index_root = match self.read_index_root()? {
            None => return Ok(Vec::new()),
            Some(ir) => ir,
        };

        let mut matches: Vec<String> = Vec::new();
        let mut scan_index = |hash: &Hash| -> Result<(), Error> {
            let idx = self.load_index_file(hash)?;
            for entry in idx.entries() {
                let h = entry.hash().as_str();
                if h.starts_with(prefix) {
                    matches.push(h.to_string());
                }
            }
            Ok(())
        };

        for delta_hash in index_root.delta_hashes_as_hashes() {
            scan_index(&delta_hash)?;
        }
        if let Some(hot_hash) = index_root.hot_hash_opt() {
            scan_index(&hot_hash)?;
        }
        for prefix_slot in 0..index_root.cold_shard_count() {
            if let Some(shard_hash) = index_root.cold_shard_hash(prefix_slot) {
                scan_index(&shard_hash)?;
            }
        }

        matches.sort();
        matches.dedup();
        // Index entries always store full 64-char hex, so from_hex succeeds;
        // filter_map keeps the signature total rather than panicking.
        Ok(matches
            .iter()
            .filter_map(|h| Hash::from_hex(h).ok())
            .collect())
    }

    // -----------------------------------------------------------------------
    // Index file loading
    // -----------------------------------------------------------------------

    fn load_index_file(&self, hash: &Hash) -> Result<IndexFile, Error> {
        if let Some(index) = self.indexes.lock().unwrap().get(hash).cloned() {
            return Ok((*index).clone());
        }
        let _loading = self.index_load.lock().unwrap();
        if let Some(index) = self.indexes.lock().unwrap().get(hash).cloned() {
            return Ok((*index).clone());
        }
        let index = crate::codec::pack::load_index_file(
            self.remote.as_ref(),
            &self.objcache,
            self.remote_key.as_ref(),
            hash,
        )?;
        let mut cache = self.indexes.lock().unwrap();
        Ok((**cache.entry(hash.clone()).or_insert_with(|| Arc::new(index))).clone())
    }

    // -----------------------------------------------------------------------
    // Pack file slicing
    // -----------------------------------------------------------------------

    fn fetch_pack_slice(
        &self,
        pack_hash: &Hash,
        offset: u32,
        length: u32,
    ) -> Result<Vec<u8>, Error> {
        // Ensure the whole pack is cached locally (one remote fetch per pack).
        // Pack files hold per-object-encrypted bytes; cache them raw (no decryption).
        // The packcache directory is created lazily by ObjectsDir::write_stream.
        if !self.packcache.exists(pack_hash)? {
            // Recheck under the installation lock. The lock is held only for
            // the cache miss, not while serving already-cached slices.
            let _fetch_guard = self.pack_fetch.lock().unwrap();
            if !self.packcache.exists(pack_hash)? {
                let mut r = self.remote.open_read(pack_hash)?;
                self.packcache.write_from(pack_hash, &mut r)?;
            }
        }
        // Serve the slice from the local cache.
        let mut reader = self.packcache.open_read(pack_hash)?;
        let skip = PACK_BODY_OFFSET + offset as u64;
        io::copy(&mut reader.by_ref().take(skip), &mut io::sink()).map_err(Error::Io)?;
        let mut buf = vec![0u8; length as usize];
        reader.read_exact(&mut buf).map_err(Error::Io)?;
        Ok(buf)
    }

    // -----------------------------------------------------------------------
    // Core lookup
    // -----------------------------------------------------------------------

    /// Where `hash` was found while searching the pack index (delta -> hot ->
    /// standalone-probe -> cold, the same order `resolve` uses). Carries only
    /// index metadata (an owned `IndexEntry`, cloned out of the index file
    /// before it is dropped) or the fact that a direct remote HEAD confirmed
    /// a standalone object -- never an object body or pack slice.
    ///
    /// Split out of `resolve` so `exists()` can answer its boolean from index
    /// lookups alone, instead of the old "just try resolve", which downloaded
    /// the full object (or a whole pack file, for a Pack-routed entry) purely
    /// to answer a boolean (refactor-instructions.md F5). `NoIndexRoot` is a
    /// distinct case (not folded into `NotFound`) so `resolve` can still fall
    /// through to `fetch_from_remote_direct` exactly as before, and so this
    /// function reads INDEX_ROOT at most once -- `resolve` no longer does its
    /// own separate `read_index_root()` check before calling `locate`.
    fn locate(&self, hash: &Hash) -> Result<Located, Error> {
        let index_root = match self.read_index_root()? {
            None => return Ok(Located::NoIndexRoot),
            Some(ir) => ir,
        };

        // 3. Search delta files newest-first.
        for delta_hash in index_root.delta_hashes_as_hashes() {
            let idx = self.load_index_file(&delta_hash)?;
            if let Some(entry) = idx.find(hash) {
                return Ok(Located::Entry(entry.clone()));
            }
        }

        // 4. Search hot index.
        if let Some(hot_hash) = index_root.hot_hash_opt() {
            let idx = self.load_index_file(&hot_hash)?;
            if let Some(entry) = idx.find(hash) {
                return Ok(Located::Entry(entry.clone()));
            }
        }

        // The cold shard covering `hash`, computed once and shared by step
        // [A] below (cache-only warm search) and the unchanged step 6
        // fallback (cold-start fetch), so the prefix-bit logic that used to
        // live only in step 6 is not duplicated.
        let shard_hash = cold_shard_hash_for(hash, &index_root);

        // [A] If the covering shard is already present in the local objcache,
        // search it now -- this is a local disk read, not a network round
        // trip, because index files are content-addressed and immutable (see
        // design/02 "Why the cold shard is checked before the remote probe
        // when already cached"). A shard that has never been fetched is left
        // alone here (no network call) and still defers to the probe below,
        // exactly as before this change.
        let mut shard_already_searched = false;
        if let Some(sh) = &shard_hash {
            if self.objcache.exists(sh)? {
                shard_already_searched = true;
                let idx = self.load_index_file(sh)?;
                if let Some(entry) = idx.find(hash) {
                    return Ok(Located::Entry(entry.clone()));
                }
                // Miss in the now-searched shard: remember this so step 6
                // below does not redo the same search after the probe.
            }
        }

        // Consult the Bloom filter, if the index root has one recorded,
        // to decide whether the cold-shard fetch (step 6) below is worth
        // paying for. A "definitely absent" answer is exact (no false
        // negatives are possible by construction) and is safe to substitute
        // for "not present in any cold shard" -- the Bloom filter and the
        // cold shards are built from the same `omemfs pack` snapshot with no
        // staleness gap between them -- so it short-circuits straight to
        // NotFound, skipping the cold-shard fetch. Snapshot readers never
        // probe unindexed standalone keys: a published snapshot records every
        // object reachable from its root, including standalone entries.
        if !shard_already_searched {
            if let Some(bloom_hash) = index_root.bloom_hash_opt() {
                let bloom = self.load_bloom_filter(&bloom_hash)?;
                if !bloom.may_contain(hash) {
                    return Ok(Located::NotFound);
                }
            }
        }

        // 6. Cold shard lookup, unless step [A] already searched this exact
        // shard above (cache hit, not found there) -- in that case the shard
        // is already known to miss and there is nothing left to fetch.
        if shard_already_searched {
            return Ok(Located::NotFound);
        }
        if let Some(shard_hash) = shard_hash {
            let idx = self.load_index_file(&shard_hash)?;
            if let Some(entry) = idx.find(hash) {
                return Ok(Located::Entry(entry.clone()));
            }
        }

        Ok(Located::NotFound)
    }

    // -----------------------------------------------------------------------
    // Bloom filter loading (read path, Improvement C)
    // -----------------------------------------------------------------------

    /// Load the Bloom filter at `hash`, consulting the local objcache first
    /// and falling back to fetch + decrypt + cache on first use -- identical
    /// caching pattern to `load_index_file` / `codec::pack::load_index_file`
    /// (see design/02 "Index file local caching", Improvement C paragraph).
    /// Reuses the shared fetch-decrypt-cache primitive
    /// (`codec::pack::load_cached_plaintext`) so the decrypt logic is not
    /// duplicated between index files and the Bloom filter.
    fn load_bloom_filter(&self, hash: &Hash) -> Result<BloomFilter, Error> {
        if let Some(bloom) = self.bloom.lock().unwrap().get(hash).cloned() {
            return Ok((*bloom).clone());
        }
        let _loading = self.bloom_load.lock().unwrap();
        if let Some(bloom) = self.bloom.lock().unwrap().get(hash).cloned() {
            return Ok((*bloom).clone());
        }
        let plaintext = crate::codec::pack::load_cached_plaintext(
            self.remote.as_ref(),
            &self.objcache,
            self.remote_key.as_ref(),
            hash,
        )?;
        let bloom = BloomFilter::deserialise(&plaintext)?;
        let mut cache = self.bloom.lock().unwrap();
        Ok((**cache.entry(hash.clone()).or_insert_with(|| Arc::new(bloom))).clone())
    }

    /// Resolve `hash` to its encrypted bytes, searching the pack index.
    /// Returns the raw encrypted bytes; decryption is handled by the codec layer.
    fn resolve(&self, hash: &Hash) -> Result<Vec<u8>, Error> {
        match self.locate(hash)? {
            Located::Entry(entry) => self.resolve_entry(&entry, hash),
            Located::NotFound => Err(Error::ObjectNotFound(hash.as_str().to_string())),
            // No INDEX_ROOT — fall through to direct remote lookup.
            Located::NoIndexRoot => self.fetch_from_remote_direct(hash),
        }
    }

    fn resolve_entry(&self, entry: &IndexEntry, hash: &Hash) -> Result<Vec<u8>, Error> {
        match entry {
            IndexEntry::Inline(e) => Ok(e.data.clone()),
            IndexEntry::Pack(e) => self.fetch_pack_slice(&e.pack_hash, e.offset, e.length),
            IndexEntry::Standalone(_) => {
                let mut r = self.remote.open_read(hash)?;
                let mut buf = Vec::new();
                r.read_to_end(&mut buf).map_err(Error::Io)?;
                Ok(unescape_standalone(buf))
            }
        }
    }

    fn fetch_from_remote_direct(&self, hash: &Hash) -> Result<Vec<u8>, Error> {
        let mut r = self.remote.open_read(hash)?;
        let mut buf = Vec::new();
        r.read_to_end(&mut buf).map_err(Error::Io)?;
        Ok(unescape_standalone(buf))
    }
}

// ---------------------------------------------------------------------------
// Cold shard addressing
// ---------------------------------------------------------------------------

/// Compute the cold shard hash covering `hash` under `index_root`: a
/// prefix-addressed shard when `cold_prefix_bits > 0`, or the single shared
/// shard (slot 0) when `cold_prefix_bits == 0` and at least one shard slot
/// exists. Returns `None` when no cold shard is recorded at all.
///
/// Shared by `locate`'s step [A] (cache-only warm search) and step 6 (the
/// unchanged cold-start fetch), so the two steps always address the same
/// shard for a given hash and the prefix-bit logic is not duplicated.
fn cold_shard_hash_for(hash: &Hash, index_root: &IndexRoot) -> Option<Hash> {
    let prefix_bits = index_root.cold_prefix_bits as usize;
    if prefix_bits > 0 {
        index_root.cold_shard_hash(hash_prefix(hash, prefix_bits))
    } else if !index_root.cold_shards.is_empty() {
        // cold_prefix_bits == 0: single shared shard.
        index_root.cold_shard_hash(0)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Bit prefix extraction
// ---------------------------------------------------------------------------

/// Extract the first `bits` bits of the hash as a usize index.
pub(crate) fn hash_prefix(hash: &Hash, bits: usize) -> usize {
    let bytes = hash.as_bytes_array();
    let byte_idx = bits / 8;
    let bit_offset = bits % 8;

    if bit_offset == 0 {
        // Exact byte boundary: use the first `byte_idx` bytes.
        let mut v = 0usize;
        for &byte in bytes.iter().take(byte_idx.min(bytes.len())) {
            v = (v << 8) | byte as usize;
        }
        v
    } else {
        // Partial byte.
        let mut v = 0usize;
        for &byte in bytes.iter().take(byte_idx.min(bytes.len())) {
            v = (v << 8) | byte as usize;
        }
        if byte_idx < bytes.len() {
            v = (v << bit_offset) | (bytes[byte_idx] >> (8 - bit_offset)) as usize;
        }
        v
    }
}

// ---------------------------------------------------------------------------
// ObjectStore implementation
// ---------------------------------------------------------------------------

impl ObjectStore for PackReader {
    fn default_transfer_concurrency(&self) -> usize {
        // Reads are served from the remote; inherit its transfer parallelism.
        self.remote.default_transfer_concurrency()
    }

    fn exists(&self, hash: &Hash) -> Result<bool, Error> {
        if self.local_cache.exists(hash)? {
            return Ok(true);
        }
        // Index-only lookup (delta/hot/cold entries only -- no pack slice or
        // object body is fetched) rather than the old "just try resolve",
        // which downloaded the full object (or a whole pack file, for a
        // Pack-routed entry) purely to answer a boolean (refactor-
        // instructions.md F5).
        Ok(!matches!(
            self.locate(hash)?,
            Located::NotFound | Located::NoIndexRoot
        ))
    }

    fn open_read(&self, hash: &Hash) -> Result<Box<dyn io::Read>, Error> {
        // 1. Local cache hit.
        if self.local_cache.exists(hash)? {
            return self.local_cache.open_read(hash);
        }

        // 2–6. Pack index lookup.
        let encrypted = self.resolve(hash)?;

        // We do NOT cache these bytes in the local cache here: local cache
        // stores compressed-but-not-encrypted bytes per design (see
        // codec/pack/mod.rs), but this method returns still-encrypted bytes.
        // The caller (codec::store_read) decrypts them before they reach
        // downstream code; only decrypted bytes are ever written back to the
        // local cache (e.g. by LazyTreeStore / ensure_blob_local in pull.rs).
        Ok(Box::new(Cursor::new(encrypted)))
    }

    fn size(&self, hash: &Hash) -> Result<u64, Error> {
        // Delegate to local cache if present, else remote.
        if self.local_cache.exists(hash)? {
            return self.local_cache.size(hash);
        }
        self.remote.size(hash)
    }

    fn list_with_sizes(&self) -> Result<Vec<(String, u64)>, Error> {
        // Delegate to the remote store — it holds the authoritative object list.
        self.remote.list_with_sizes()
    }

    fn write_from(&self, hash: &Hash, reader: &mut dyn io::Read) -> Result<(), Error> {
        // PackReader is read-only; delegate writes to local cache only.
        self.local_cache.write_from(hash, reader)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use crate::codec::pack::root_pointer::{LocalRootPointer, RootToken};
    use crate::codec::pack::writer::PackWriter;
    use tempfile::TempDir;

    fn setup(tmp: &TempDir) -> (PackWriter, PackReader) {
        let base = tmp.path().to_path_buf();
        std::fs::create_dir_all(base.join("objects")).unwrap();
        std::fs::create_dir_all(base.join("tmp")).unwrap();

        let cache_dir = tmp.path().join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();

        let packcache_dir = tmp.path().join("packcache");
        std::fs::create_dir_all(&packcache_dir).unwrap();

        let objcache_dir = tmp.path().join("objcache");
        std::fs::create_dir_all(&objcache_dir).unwrap();

        let writer_cache_dir = tmp.path().join("writer_cache");
        std::fs::create_dir_all(&writer_cache_dir).unwrap();

        let writer_objcache_dir = tmp.path().join("writer_objcache");
        std::fs::create_dir_all(&writer_objcache_dir).unwrap();

        let remote_for_writer = LocalStore::for_remote(&base);
        let writer_objcache = LocalStore::for_cache(&writer_objcache_dir);
        let writer = PackWriter::new(
            Box::new(remote_for_writer),
            Box::new(LocalRootPointer::new(base.clone(), None)),
            writer_objcache,
            None,
        )
        .unwrap();

        let remote_for_reader = LocalStore::for_remote(&base);
        let local_cache = LocalStore::for_cache(&cache_dir);
        let packcache = LocalStore::for_cache(&packcache_dir);
        let objcache = LocalStore::for_cache(&objcache_dir);
        let reader = PackReader::new(
            Box::new(remote_for_reader),
            local_cache,
            packcache,
            objcache,
            Box::new(LocalRootPointer::new(base.clone(), None)),
            None,
        );

        (writer, reader)
    }

    #[test]
    fn read_inline_entry() {
        let tmp = TempDir::new().unwrap();
        let (writer, reader) = setup(&tmp);

        // Write an inline-sized object via PackWriter.
        let data = vec![0xAA; 50];
        let hash = Hash::compute(&data);
        let mut cursor = io::Cursor::new(&data);
        writer.write_from(&hash, &mut cursor).unwrap();

        let root = Hash::compute(b"root");
        let mut writer = writer; // make mutable
        writer.finish(&root).unwrap();

        // Now read back via PackReader.
        let mut r = reader.open_read(&hash).unwrap();
        let mut got = Vec::new();
        r.read_to_end(&mut got).unwrap();
        assert_eq!(got, data);
    }

    #[test]
    fn read_pack_entry() {
        let tmp = TempDir::new().unwrap();
        let (writer, reader) = setup(&tmp);

        // Write a pack-range object.
        let data = vec![0xBB; 1024];
        let hash = Hash::compute(&data);
        let mut cursor = io::Cursor::new(&data);
        writer.write_from(&hash, &mut cursor).unwrap();

        let root = Hash::compute(b"root");
        let mut writer = writer;
        writer.finish(&root).unwrap();

        let mut r = reader.open_read(&hash).unwrap();
        let mut got = Vec::new();
        r.read_to_end(&mut got).unwrap();
        assert_eq!(got, data);
    }

    #[test]
    fn read_standalone_entry() {
        let tmp = TempDir::new().unwrap();
        let (writer, reader) = setup(&tmp);

        // Write a standalone-sized object (≥ 1 MiB).
        let data = vec![0xCC; 1024 * 1024 + 1];
        let hash = Hash::compute(&data);
        let mut cursor = io::Cursor::new(&data);
        writer.write_from(&hash, &mut cursor).unwrap();

        let root = Hash::compute(b"root");
        let mut writer = writer;
        writer.finish(&root).unwrap();

        let mut r = reader.open_read(&hash).unwrap();
        let mut got = Vec::new();
        r.read_to_end(&mut got).unwrap();
        assert_eq!(got, data);
    }

    #[test]
    fn snapshot_reader_rejects_unindexed_standalone() {
        // A raw standalone object with no entry in the published index is an
        // orphan, not part of this reader's snapshot. SnapshotOnly lookup must
        // reject it rather than paying a remote HEAD for every normal miss.
        use crate::codec::pack::writer::{PackWriter, STANDALONE_ESCAPE_MAGIC};

        let tmp = TempDir::new().unwrap();
        let base = tmp.path().to_path_buf();
        std::fs::create_dir_all(base.join("objects")).unwrap();
        std::fs::create_dir_all(base.join("tmp")).unwrap();
        let cache_dir = tmp.path().join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();

        // Original object bytes begin with ED E2 (an L6 index magic), so they
        // require the standalone escape on write.
        let mut data = vec![0u8; 64];
        data[0] = 0xED;
        data[1] = 0xE2;
        let hash = Hash::compute(&data);

        // Write the escaped bytes directly into the remote objects/ store, with
        // no index entry referencing the hash.
        let remote = LocalStore::for_remote(&base);
        let mut wrapped = Vec::with_capacity(2 + data.len());
        wrapped.extend_from_slice(&STANDALONE_ESCAPE_MAGIC);
        wrapped.extend_from_slice(&data);
        remote
            .write_from(&hash, &mut Cursor::new(&wrapped))
            .unwrap();

        // Produce an INDEX_ROOT (with no entry for `hash`) via a separate push,
        // so resolve() reaches step 5 instead of the no-INDEX_ROOT shortcut.
        let writer_remote = LocalStore::for_remote(&base);
        let writer_objcache = LocalStore::for_cache(tmp.path().join("writer_objcache"));
        let mut writer = PackWriter::new(
            Box::new(writer_remote),
            Box::new(LocalRootPointer::new(base.clone(), None)),
            writer_objcache,
            None,
        )
        .unwrap();
        let dummy = vec![0x11; 50];
        let dummy_hash = Hash::compute(&dummy);
        writer
            .write_from(&dummy_hash, &mut Cursor::new(&dummy))
            .unwrap();
        writer.finish(&Hash::compute(b"root")).unwrap();

        let packcache_dir = tmp.path().join("packcache2");
        std::fs::create_dir_all(&packcache_dir).unwrap();
        let objcache_dir = tmp.path().join("objcache2");
        std::fs::create_dir_all(&objcache_dir).unwrap();

        let reader = PackReader::new(
            Box::new(LocalStore::for_remote(&base)),
            LocalStore::for_cache(&cache_dir),
            LocalStore::for_cache(&packcache_dir),
            LocalStore::for_cache(&objcache_dir),
            Box::new(LocalRootPointer::new(base.clone(), None)),
            None,
        );
        assert!(reader.resolve(&hash).is_err());
    }

    #[test]
    fn not_found_returns_error() {
        let tmp = TempDir::new().unwrap();
        let (_writer, reader) = setup(&tmp);
        let fake_hash = Hash::from_bytes([0xFF; 32]);
        assert!(reader.open_read(&fake_hash).is_err());
    }

    #[test]
    fn read_root_from_index_root() {
        let tmp = TempDir::new().unwrap();
        let (writer, reader) = setup(&tmp);

        let root = Hash::compute(b"my-root");
        let mut writer = writer;
        writer.finish(&root).unwrap();

        let got = reader.read_root().unwrap();
        assert_eq!(got, Some(root));
    }

    #[test]
    fn hash_prefix_basic() {
        // hash starting with 0xAB: first 8 bits → 0xAB = 171
        let hash = Hash::from_bytes([0xAB; 32]);
        assert_eq!(hash_prefix(&hash, 8), 0xAB);

        // first 4 bits of 0xAB = 0b1010 = 10
        assert_eq!(hash_prefix(&hash, 4), 0x0A);
    }

    /// The remote pack is fetched exactly ONCE even when multiple objects that
    /// live in the same pack are read.  A StatsStore wrapper counts every call
    /// to `open_read` on the remote; after reading two pack-range objects the
    /// remote open_read count must not increase between the two reads (the
    /// second object is served from the local packcache).
    #[test]
    fn pack_fetched_once_for_multiple_objects() {
        use crate::store::stats::{IoRecord, StatsStore};
        use std::sync::Arc;
        use std::sync::atomic::Ordering::Relaxed;

        let tmp = TempDir::new().unwrap();
        let base = tmp.path().to_path_buf();
        std::fs::create_dir_all(base.join("objects")).unwrap();
        std::fs::create_dir_all(base.join("tmp")).unwrap();

        // Write two pack-range objects (1 KiB each, above the inline threshold).
        let data_a = vec![0xA1u8; 1024];
        let data_b = vec![0xB2u8; 1024];
        let hash_a = Hash::compute(&data_a);
        let hash_b = Hash::compute(&data_b);

        let writer_remote = LocalStore::for_remote(&base);
        let writer_objcache = LocalStore::for_cache(tmp.path().join("writer_objcache"));
        let mut writer = PackWriter::new(
            Box::new(writer_remote),
            Box::new(LocalRootPointer::new(base.clone(), None)),
            writer_objcache,
            None,
        )
        .unwrap();
        writer
            .write_from(&hash_a, &mut io::Cursor::new(&data_a))
            .unwrap();
        writer
            .write_from(&hash_b, &mut io::Cursor::new(&data_b))
            .unwrap();
        writer.finish(&Hash::compute(b"root")).unwrap();

        // Wrap the remote in a StatsStore so we can count open_read calls.
        let record = Arc::new(IoRecord::default());
        let raw_remote = LocalStore::for_remote(&base);
        let stats_remote = StatsStore::new(Box::new(raw_remote), Arc::clone(&record));

        let cache_dir = tmp.path().join("cache");
        let packcache_dir = tmp.path().join("packcache");
        let objcache_dir = tmp.path().join("objcache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::create_dir_all(&packcache_dir).unwrap();
        std::fs::create_dir_all(&objcache_dir).unwrap();

        let reader = PackReader::new(
            Box::new(stats_remote),
            LocalStore::for_cache(&cache_dir),
            LocalStore::for_cache(&packcache_dir),
            LocalStore::for_cache(&objcache_dir),
            Box::new(LocalRootPointer::new(base.clone(), None)),
            None,
        );

        // Read object A (fetches the pack from remote and caches it in packcache).
        let mut got_a = Vec::new();
        reader
            .open_read(&hash_a)
            .unwrap()
            .read_to_end(&mut got_a)
            .unwrap();
        assert_eq!(got_a, data_a, "object A bytes must round-trip correctly");

        // Snapshot the remote read count after reading object A.
        let reads_after_a = record.reads.load(Relaxed);

        // Read object B — same pack, so the pack must be served from packcache.
        // The remote open_read count must not increase at all.
        let mut got_b = Vec::new();
        reader
            .open_read(&hash_b)
            .unwrap()
            .read_to_end(&mut got_b)
            .unwrap();
        assert_eq!(got_b, data_b, "object B bytes must round-trip correctly");

        let reads_after_b = record.reads.load(Relaxed);
        assert_eq!(
            reads_after_a, reads_after_b,
            "reading object B (same pack) must not trigger another remote open_read; \
             expected packcache hit but remote was accessed again"
        );
    }

    /// F5: `exists()` on a Pack-routed (indexed, non-standalone) hash must
    /// answer from the index alone, without ever fetching the pack file body.
    /// Confirmed via a StatsStore wrapper: the remote's read count after
    /// `exists()` must equal the count after only the INDEX_ROOT + delta
    /// index fetches that `pack_fetched_once_for_multiple_objects` shows are
    /// needed to open the SAME object -- i.e. strictly fewer reads than a
    /// full `open_read`.
    #[test]
    fn exists_on_pack_routed_hash_does_not_fetch_the_pack_body() {
        use crate::store::stats::{IoRecord, StatsStore};
        use std::sync::Arc;
        use std::sync::atomic::Ordering::Relaxed;

        let tmp = TempDir::new().unwrap();
        let base = tmp.path().to_path_buf();
        std::fs::create_dir_all(base.join("objects")).unwrap();
        std::fs::create_dir_all(base.join("tmp")).unwrap();

        // A pack-range object (above the inline threshold).
        let data = vec![0xC3u8; 1024];
        let hash = Hash::compute(&data);

        let writer_remote = LocalStore::for_remote(&base);
        let writer_objcache = LocalStore::for_cache(tmp.path().join("writer_objcache"));
        let mut writer = PackWriter::new(
            Box::new(writer_remote),
            Box::new(LocalRootPointer::new(base.clone(), None)),
            writer_objcache,
            None,
        )
        .unwrap();
        writer
            .write_from(&hash, &mut io::Cursor::new(&data))
            .unwrap();
        writer.finish(&Hash::compute(b"root")).unwrap();

        let record = Arc::new(IoRecord::default());
        let raw_remote = LocalStore::for_remote(&base);
        let stats_remote = StatsStore::new(Box::new(raw_remote), Arc::clone(&record));

        let cache_dir = tmp.path().join("cache");
        let packcache_dir = tmp.path().join("packcache");
        let objcache_dir = tmp.path().join("objcache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::create_dir_all(&packcache_dir).unwrap();
        std::fs::create_dir_all(&objcache_dir).unwrap();

        let reader = PackReader::new(
            Box::new(stats_remote),
            LocalStore::for_cache(&cache_dir),
            LocalStore::for_cache(&packcache_dir),
            LocalStore::for_cache(&objcache_dir),
            Box::new(LocalRootPointer::new(base.clone(), None)),
            None,
        );

        assert!(
            reader.exists(&hash).unwrap(),
            "the pushed hash must be reported as existing"
        );
        let reads_for_exists = record.reads.load(Relaxed);

        // The packcache must still be empty: exists() never fetched the pack.
        assert!(
            std::fs::read_dir(&packcache_dir).unwrap().next().is_none(),
            "exists() must not populate the packcache -- it should never fetch the pack body"
        );

        // Actually opening the object DOES fetch the pack, so its read count
        // must be strictly greater than exists()'s (INDEX_ROOT + delta index
        // only).
        let mut got = Vec::new();
        reader
            .open_read(&hash)
            .unwrap()
            .read_to_end(&mut got)
            .unwrap();
        assert_eq!(got, data);
        let reads_for_open = record.reads.load(Relaxed);
        assert!(
            reads_for_open > reads_for_exists,
            "open_read (which fetches the pack body) must cost strictly more \
             remote reads than exists() (index-only): exists={reads_for_exists}, open={reads_for_open}"
        );
    }

    // -------------------------------------------------------------------------
    // Read-path performance tests (design/02_storage_format.md "Read path",
    // Improvements A and C).
    //
    // These build INDEX_ROOT / index-file / Bloom-filter fixtures directly
    // (bypassing PackWriter and `omemfs pack`) so cold-shard and Bloom-filter
    // behaviour can be exercised in isolation, without a full push + pack
    // cycle. `remote_key` is always `None` here, matching every other test in
    // this file, so `IndexRoot::serialise()` / `IndexFile::serialise()` /
    // `BloomFilter::serialise()` bytes can be written to the remote verbatim
    // (see `codec::encrypt::{encrypt,decrypt}`'s documented `key = None`
    // passthrough).
    // -------------------------------------------------------------------------

    /// Create a fresh remote-backend directory layout under `tmp` and return
    /// its base path.
    fn make_remote_base(tmp: &TempDir) -> std::path::PathBuf {
        let base = tmp.path().join("remote");
        std::fs::create_dir_all(base.join("objects")).unwrap();
        std::fs::create_dir_all(base.join("tmp")).unwrap();
        base
    }

    /// Write `entries` as a plaintext index file (delta / hot / cold shard --
    /// they all share the same on-disk format) directly to the remote and
    /// return its hash. Mirrors what `commands::pack::write_index_file` /
    /// `PackWriter::finish` produce, without requiring a full `omemfs pack`
    /// run to build a cold-shard or hot-index fixture.
    fn write_raw_index(remote: &LocalStore, entries: Vec<IndexEntry>) -> Hash {
        let mut idx = IndexFile::new();
        for e in entries {
            idx.push(e);
        }
        let bytes = idx.serialise().unwrap();
        let hash = Hash::compute(&bytes);
        remote
            .write_from(&hash, &mut io::Cursor::new(&bytes))
            .unwrap();
        hash
    }

    /// Write a Bloom filter covering exactly `present` to the remote and
    /// return its hash. Sized generously (1000 expected elements at a 1% false
    /// positive rate) relative to the handful of hashes these tests insert, so
    /// an unrelated "absent" hash used in the same test has a negligible
    /// chance of colliding into a false positive.
    fn write_raw_bloom(remote: &LocalStore, present: &[Hash]) -> Hash {
        use crate::codec::pack::bloom::{BloomFilter, DEFAULT_NUM_HASH_FUNCTIONS};
        let mut bf = BloomFilter::new(1000, 0.01, DEFAULT_NUM_HASH_FUNCTIONS);
        for h in present {
            bf.insert(h);
        }
        let bytes = bf.serialise();
        let hash = Hash::compute(&bytes);
        remote
            .write_from(&hash, &mut io::Cursor::new(&bytes))
            .unwrap();
        hash
    }

    /// Write `root` as the (unencrypted) INDEX_ROOT at `base`, directly via
    /// `LocalRootPointer`, bypassing `PackWriter::finish`'s CAS-from-snapshot
    /// flow entirely. Lets a test control every INDEX_ROOT field (delta/hot/
    /// cold/bloom) precisely, since `base` starts with no INDEX_ROOT at all.
    fn write_raw_index_root(base: &std::path::Path, root: &IndexRoot) {
        use crate::codec::pack::root_pointer::{LocalRootPointer, RootPointer, RootToken};
        let bytes = root.serialise().unwrap();
        let rp = LocalRootPointer::new(base.to_path_buf(), None);
        rp.cas_write(&RootToken::Absent, &bytes).unwrap();
    }

    /// Build a `PackReader` over `base` whose remote is wrapped in a
    /// `StatsStore`, so tests can assert exact `exists()` / `open_read()`
    /// counts. Returns the reader, the shared `IoRecord`, and the reader's
    /// `objcache` directory (so a test can independently probe, via a fresh
    /// `LocalStore::for_cache`, whether a specific index file -- e.g. a cold
    /// shard -- has been fetched and cached).
    fn make_reader_with_stats(
        base: &std::path::Path,
        tmp: &TempDir,
    ) -> (
        PackReader,
        std::sync::Arc<crate::store::stats::IoRecord>,
        std::path::PathBuf,
    ) {
        use crate::store::stats::{IoRecord, StatsStore};
        use std::sync::Arc;

        let record = Arc::new(IoRecord::default());
        let raw_remote = LocalStore::for_remote(base);
        let stats_remote = StatsStore::new(Box::new(raw_remote), Arc::clone(&record));

        let cache_dir = tmp.path().join("cache");
        let packcache_dir = tmp.path().join("packcache");
        let objcache_dir = tmp.path().join("objcache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::create_dir_all(&packcache_dir).unwrap();
        std::fs::create_dir_all(&objcache_dir).unwrap();

        let reader = PackReader::new(
            Box::new(stats_remote),
            LocalStore::for_cache(&cache_dir),
            LocalStore::for_cache(&packcache_dir),
            LocalStore::for_cache(&objcache_dir),
            Box::new(LocalRootPointer::new(base.to_path_buf(), None)),
            None,
        );
        (reader, record, objcache_dir)
    }

    struct CountingRootPointer {
        inner: LocalRootPointer,
        reads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl CountingRootPointer {
        fn new(base: std::path::PathBuf) -> (Self, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
            let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            (
                CountingRootPointer {
                    inner: LocalRootPointer::new(base, None),
                    reads: std::sync::Arc::clone(&reads),
                },
                reads,
            )
        }
    }

    impl RootPointer for CountingRootPointer {
        fn read(&self) -> Result<(Option<Vec<u8>>, RootToken), Error> {
            self.reads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner.read()
        }

        fn cas_write(&self, expected: &RootToken, new: &[u8]) -> Result<(), Error> {
            self.inner.cas_write(expected, new)
        }
    }

    #[test]
    fn reader_reads_index_root_once_per_snapshot() {
        use crate::codec::pack::index::InlineEntry;
        use std::sync::atomic::Ordering::Relaxed;

        let tmp = TempDir::new().unwrap();
        let base = make_remote_base(&tmp);
        let remote = LocalStore::for_remote(&base);
        let first = Hash::compute(b"snapshot-first");
        let second = Hash::compute(b"snapshot-second");
        let index_hash = write_raw_index(
            &remote,
            vec![
                IndexEntry::Inline(InlineEntry {
                    hash: first.clone(),
                    data: b"snapshot-first".to_vec(),
                }),
                IndexEntry::Inline(InlineEntry {
                    hash: second.clone(),
                    data: b"snapshot-second".to_vec(),
                }),
            ],
        );
        let mut root = IndexRoot::new_empty();
        root.delta_hashes = vec![*index_hash.as_bytes_array()];
        write_raw_index_root(&base, &root);
        let (pointer, reads) = CountingRootPointer::new(base.clone());
        let cache = tmp.path().join("snapshot-cache");
        let packcache = tmp.path().join("snapshot-packcache");
        let objcache = tmp.path().join("snapshot-objcache");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::create_dir_all(&packcache).unwrap();
        std::fs::create_dir_all(&objcache).unwrap();
        let reader = PackReader::new(
            Box::new(LocalStore::for_remote(&base)),
            LocalStore::for_cache(&cache),
            LocalStore::for_cache(&packcache),
            LocalStore::for_cache(&objcache),
            Box::new(pointer),
            None,
        );
        let _ = read_all(reader.open_read(&first).unwrap());
        let _ = read_all(reader.open_read(&second).unwrap());
        assert_eq!(
            reads.load(Relaxed),
            1,
            "one reader must fetch INDEX_ROOT once"
        );
    }

    /// Read a `Box<dyn Read>` to completion (test helper).
    fn read_all(mut r: Box<dyn io::Read>) -> Vec<u8> {
        let mut buf = Vec::new();
        r.read_to_end(&mut buf).unwrap();
        buf
    }

    /// [Improvement A] Once a cold shard has been fetched into the local
    /// objcache -- by resolving any hash that lives in it -- resolving a
    /// DIFFERENT hash from the SAME shard must be a pure local operation: no
    /// further remote `exists()` probe. The current implementation always
    /// issues one `exists()` HEAD per lookup regardless of shard warmth (see
    /// `locate`'s step 5, which runs unconditionally before step 6's cold
    /// shard search), so this test is expected to FAIL until Improvement A is
    /// implemented.
    #[test]
    fn cold_shard_search_is_free_once_warm() {
        use crate::codec::pack::index::InlineEntry;
        use std::sync::atomic::Ordering::Relaxed;

        let tmp = TempDir::new().unwrap();
        let base = make_remote_base(&tmp);
        let remote_for_setup = LocalStore::for_remote(&base);

        let data1 = vec![0xA1u8; 10];
        let hash1 = Hash::compute(&data1);
        let data2 = vec![0xA2u8; 10];
        let hash2 = Hash::compute(&data2);

        // Both hashes live in the SAME (single, shared) cold shard.
        let shard_hash = write_raw_index(
            &remote_for_setup,
            vec![
                IndexEntry::Inline(InlineEntry {
                    hash: hash1.clone(),
                    data: data1.clone(),
                }),
                IndexEntry::Inline(InlineEntry {
                    hash: hash2.clone(),
                    data: data2.clone(),
                }),
            ],
        );

        let mut root = IndexRoot::new_empty();
        root.cold_shards = vec![*shard_hash.as_bytes_array()];
        write_raw_index_root(&base, &root);

        let (reader, record, _objcache_dir) = make_reader_with_stats(&base, &tmp);

        // First resolution fetches the shard but SnapshotOnly lookup never
        // probes a loose standalone key.
        let got1 = read_all(reader.open_read(&hash1).unwrap());
        assert_eq!(
            got1, data1,
            "first object's bytes must round-trip correctly"
        );
        let exists_after_first =
            record.exists_found.load(Relaxed) + record.exists_miss.load(Relaxed);
        assert_eq!(
            exists_after_first, 0,
            "SnapshotOnly lookup must not issue an exists() probe"
        );

        // Second resolution: a DIFFERENT hash from the SAME now-cached shard.
        let got2 = read_all(reader.open_read(&hash2).unwrap());
        assert_eq!(
            got2, data2,
            "second object's bytes must round-trip correctly"
        );
        let exists_after_second =
            record.exists_found.load(Relaxed) + record.exists_miss.load(Relaxed);
        assert_eq!(
            exists_after_second, exists_after_first,
            "resolving a second hash from an already-cached cold shard must not \
             issue any additional remote exists() probe (Improvement A)"
        );
    }

    /// Regression guard: the FIRST lookup of a hash whose cold shard has never
    /// been fetched must still defer to the remote `exists()` probe before
    /// fetching the shard -- Improvement A only changes behaviour once a shard
    /// is already warm (design/02 "Why the cold shard is checked before the
    /// remote probe when already cached"). This must hold both before and
    /// after Improvement A is implemented.
    #[test]
    fn cold_shard_first_lookup_avoids_standalone_probe() {
        use crate::codec::pack::index::InlineEntry;
        use std::sync::atomic::Ordering::Relaxed;

        let tmp = TempDir::new().unwrap();
        let base = make_remote_base(&tmp);
        let remote_for_setup = LocalStore::for_remote(&base);

        let data = vec![0xB1u8; 10];
        let hash = Hash::compute(&data);
        let shard_hash = write_raw_index(
            &remote_for_setup,
            vec![IndexEntry::Inline(InlineEntry {
                hash: hash.clone(),
                data: data.clone(),
            })],
        );

        let mut root = IndexRoot::new_empty();
        root.cold_shards = vec![*shard_hash.as_bytes_array()];
        write_raw_index_root(&base, &root);

        let (reader, record, objcache_dir) = make_reader_with_stats(&base, &tmp);

        assert!(
            !LocalStore::for_cache(&objcache_dir)
                .exists(&shard_hash)
                .unwrap(),
            "shard must not be cached before the first lookup"
        );

        let got = read_all(reader.open_read(&hash).unwrap());
        assert_eq!(got, data);

        let exists_calls = record.exists_found.load(Relaxed) + record.exists_miss.load(Relaxed);
        assert_eq!(
            exists_calls, 0,
            "snapshot lookup must not issue a standalone exists() probe"
        );
        assert!(
            LocalStore::for_cache(&objcache_dir)
                .exists(&shard_hash)
                .unwrap(),
            "the shard must now be cached after its first fetch"
        );
    }

    /// [Improvement C, corrected] When a Bloom filter is recorded and
    /// reports a hash as "definitely absent", `locate` must still issue the
    /// remote `exists()` probe (step 5) -- exactly one, the same cold-start
    /// cost as if there were no Bloom filter at all -- because a standalone
    /// object can be written to the remote at any time, after the Bloom
    /// filter's snapshot was taken, and only the probe can observe it (see
    /// `read_standalone_step5_unescapes`, and design/02 "Why the Bloom
    /// filter is checked before the cold-shard fetch, but never before the
    /// remote probe"). What the Bloom filter DOES still skip is the
    /// cold-shard *fetch* (step 7): since the filter and the cold shards
    /// share the same `omemfs pack` snapshot with no staleness gap, a
    /// definite miss proves the hash cannot be in any cold shard, so the
    /// shard is never downloaded -- verified here by asserting the shard is
    /// NOT present in objcache afterward (a fetch would have cached it).
    ///
    /// This test previously asserted `exists_calls == 0`, which was the
    /// unsafe behaviour this fix removes: a Bloom-negative hash used to skip
    /// the probe entirely, which could wrongly report NotFound for a
    /// standalone object written after the last `omemfs pack`.
    #[test]
    fn definite_absence_short_circuits_via_bloom_filter() {
        use crate::codec::pack::index::InlineEntry;
        use std::sync::atomic::Ordering::Relaxed;

        let tmp = TempDir::new().unwrap();
        let base = make_remote_base(&tmp);
        let remote_for_setup = LocalStore::for_remote(&base);

        // A cold shard covering some present hash -- unrelated to the absent
        // hash below, but its non-fetch is what this test proves.
        let present_data = vec![0xC1u8; 10];
        let present_hash = Hash::compute(&present_data);
        let shard_hash = write_raw_index(
            &remote_for_setup,
            vec![IndexEntry::Inline(InlineEntry {
                hash: present_hash.clone(),
                data: present_data,
            })],
        );

        // The Bloom filter covers ONLY the present hash -- the hash below is
        // never inserted, so `may_contain` must report "definitely absent".
        let absent_hash = Hash::compute(b"genuinely-absent-object");
        let bloom_hash = write_raw_bloom(&remote_for_setup, &[present_hash]);

        let mut root = IndexRoot::new_empty();
        root.cold_shards = vec![*shard_hash.as_bytes_array()];
        root.bloom_hash = *bloom_hash.as_bytes_array();
        write_raw_index_root(&base, &root);

        let (reader, record, objcache_dir) = make_reader_with_stats(&base, &tmp);

        let result = reader.open_read(&absent_hash);
        assert!(
            result.is_err(),
            "a genuinely absent hash must resolve to an error"
        );

        let exists_calls = record.exists_found.load(Relaxed) + record.exists_miss.load(Relaxed);
        assert_eq!(
            exists_calls, 0,
            "a SnapshotOnly Bloom miss must not probe a standalone key"
        );
        assert!(
            !LocalStore::for_cache(&objcache_dir)
                .exists(&shard_hash)
                .unwrap(),
            "a Bloom-filter-confirmed absent hash must never trigger a \
             cold-shard fetch -- the filter and the cold shards share the \
             same pack snapshot, so a definite miss proves the hash cannot \
             be in that shard"
        );
    }

    /// Guard: when the index root has no Bloom filter recorded at all
    /// (`bloom_hash_opt()` is `None`), a genuinely absent hash must still fall
    /// through to the (slower) remote probe + cold-shard fetch, exactly as
    /// before Improvement C (design/02 "or no Bloom filter recorded at all").
    /// This must hold both now and after Improvement C is implemented.
    #[test]
    fn no_bloom_filter_recorded_falls_through_unchanged() {
        use crate::codec::pack::index::InlineEntry;
        use std::sync::atomic::Ordering::Relaxed;

        let tmp = TempDir::new().unwrap();
        let base = make_remote_base(&tmp);
        let remote_for_setup = LocalStore::for_remote(&base);

        let present_data = vec![0xD1u8; 10];
        let present_hash = Hash::compute(&present_data);
        let shard_hash = write_raw_index(
            &remote_for_setup,
            vec![IndexEntry::Inline(InlineEntry {
                hash: present_hash.clone(),
                data: present_data,
            })],
        );

        let absent_hash = Hash::compute(b"also-genuinely-absent");

        let mut root = IndexRoot::new_empty();
        root.cold_shards = vec![*shard_hash.as_bytes_array()];
        // bloom_hash intentionally left all-zero -> bloom_hash_opt() == None.
        write_raw_index_root(&base, &root);

        let (reader, record, objcache_dir) = make_reader_with_stats(&base, &tmp);

        let result = reader.open_read(&absent_hash);
        assert!(result.is_err());

        let exists_calls = record.exists_found.load(Relaxed) + record.exists_miss.load(Relaxed);
        assert_eq!(
            exists_calls, 0,
            "SnapshotOnly lookup must not probe a standalone key"
        );
        assert!(
            LocalStore::for_cache(&objcache_dir)
                .exists(&shard_hash)
                .unwrap(),
            "with no Bloom filter recorded, the cold shard must still be fetched on a miss"
        );
    }

    /// [design/02 "Cost and ordering only, never classification"] Improvements
    /// A and C change WHEN/WHETHER network calls happen, but must never change
    /// WHAT `locate` ultimately returns. Build one fixture covering every
    /// `Located` variant and resolve each hash TWICE with the SAME reader (the
    /// second call is served from the now-warm objcache); both calls must
    /// agree. This test must pass on both the current code and the improved
    /// code -- it locks in the invariant, not a specific implementation, and
    /// is the sanity check that the other tests above are testing behaviour
    /// rather than accidentally locking in a bug.
    #[test]
    fn classification_is_identical_cold_and_warm_for_every_variant() {
        use crate::codec::pack::index::InlineEntry;

        let tmp = TempDir::new().unwrap();
        let base = make_remote_base(&tmp);
        let remote_for_setup = LocalStore::for_remote(&base);

        let delta_data = vec![0xE1u8; 10];
        let delta_hash = Hash::compute(&delta_data);
        let hot_data = vec![0xE2u8; 10];
        let hot_hash_val = Hash::compute(&hot_data);
        let cold_data = vec![0xE3u8; 10];
        let cold_hash = Hash::compute(&cold_data);
        let notfound_hash = Hash::compute(b"nowhere-to-be-found");

        let delta_index_hash = write_raw_index(
            &remote_for_setup,
            vec![IndexEntry::Inline(InlineEntry {
                hash: delta_hash.clone(),
                data: delta_data.clone(),
            })],
        );
        let hot_index_hash = write_raw_index(
            &remote_for_setup,
            vec![IndexEntry::Inline(InlineEntry {
                hash: hot_hash_val.clone(),
                data: hot_data.clone(),
            })],
        );
        let cold_shard_hash = write_raw_index(
            &remote_for_setup,
            vec![IndexEntry::Inline(InlineEntry {
                hash: cold_hash.clone(),
                data: cold_data.clone(),
            })],
        );
        let bloom_hash = write_raw_bloom(
            &remote_for_setup,
            &[delta_hash.clone(), hot_hash_val.clone(), cold_hash.clone()],
        );

        let mut root = IndexRoot::new_empty();
        root.delta_hashes = vec![*delta_index_hash.as_bytes_array()];
        root.hot_hash = *hot_index_hash.as_bytes_array();
        root.cold_shards = vec![*cold_shard_hash.as_bytes_array()];
        root.bloom_hash = *bloom_hash.as_bytes_array();
        write_raw_index_root(&base, &root);

        let (reader, _record, _objcache_dir) = make_reader_with_stats(&base, &tmp);

        // Entry via delta.
        let d1 = read_all(reader.open_read(&delta_hash).unwrap());
        let d2 = read_all(reader.open_read(&delta_hash).unwrap());
        assert_eq!(d1, delta_data);
        assert_eq!(
            d1, d2,
            "delta-index classification must be identical cold and warm"
        );

        // Entry via hot.
        let h1 = read_all(reader.open_read(&hot_hash_val).unwrap());
        let h2 = read_all(reader.open_read(&hot_hash_val).unwrap());
        assert_eq!(h1, hot_data);
        assert_eq!(
            h1, h2,
            "hot-index classification must be identical cold and warm"
        );

        // Entry via cold shard.
        let c1 = read_all(reader.open_read(&cold_hash).unwrap());
        let c2 = read_all(reader.open_read(&cold_hash).unwrap());
        assert_eq!(c1, cold_data);
        assert_eq!(
            c1, c2,
            "cold-shard classification must be identical cold and warm"
        );

        // NotFound.
        assert!(
            reader.open_read(&notfound_hash).is_err(),
            "NotFound classification (first call) must be an error"
        );
        assert!(
            reader.open_read(&notfound_hash).is_err(),
            "NotFound classification (second, warm call) must also be an error"
        );
    }
}
