/// Stage 5: pack reader.
///
/// PackReader wraps a raw remote ObjectStore and implements `open_read` using
/// the three-tier index (delta / hot / cold) instead of direct object lookup.
///
/// Search order for `open_read(target_hash)`:
///   1. Local objects/ cache — return immediately on hit.
///   2. Fetch INDEX_ROOT (local cache or remote file).
///   3. Search delta files newest-first (binary search within each).
///   4. Binary search the hot index.
///   5. Check if objects/<hash> exists directly (standalone).
///   6. Compute prefix → load cold shard → binary search.
///   7. Not found → error.
///
/// On index hit:
///   inline  → return data bytes (already encrypted; caller decrypts via codec).
///   pack    → fetch pack file → slice at [offset, offset+length) → return bytes.
///   standalone → fetch objects/<hash> directly from remote.
///
/// After fetching from remote, the encrypted bytes are stored in the local
/// objects/ cache (decrypt happens at the codec layer, not here).
use std::io::{self, Cursor, Read};

use crate::codec::encrypt::EncryptKey;
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
    /// No index entry, but a direct remote HEAD/stat confirmed a standalone
    /// object at `objects/<hash>`.
    Standalone,
    /// An INDEX_ROOT exists but `hash` is not referenced anywhere in it, and
    /// no standalone object was found either.
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
        // Route the index-root read through the same backend-pluggable root
        // pointer abstraction used by push (PackWriter) and pack, so all
        // index-root reads resolve the storage path identically.
        // This is a plain read (no CAS), so the version token is discarded.
        let raw = match self.root_pointer.read()?.0 {
            Some(b) => b,
            None => return Ok(None),
        };
        let plaintext = self.decrypt_index_root(&raw)?;
        Ok(Some(IndexRoot::deserialise(&plaintext)?))
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
        crate::codec::pack::load_index_file(
            self.remote.as_ref(),
            &self.objcache,
            self.remote_key.as_ref(),
            hash,
        )
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
            let mut r = self.remote.open_read(pack_hash)?;
            self.packcache.write_from(pack_hash, &mut r)?;
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

        // 5. Try standalone: check if objects/<hash> exists directly in remote
        // (a HEAD/stat only -- the body is fetched later, only if the caller
        // actually wants bytes).
        if self.remote.exists(hash)? {
            return Ok(Located::Standalone);
        }

        // 6. Cold shard lookup.
        let prefix_bits = index_root.cold_prefix_bits as usize;
        let shard_hash = if prefix_bits > 0 {
            index_root.cold_shard_hash(hash_prefix(hash, prefix_bits))
        } else if !index_root.cold_shards.is_empty() {
            // cold_prefix_bits == 0: single shared shard.
            index_root.cold_shard_hash(0)
        } else {
            None
        };
        if let Some(shard_hash) = shard_hash {
            let idx = self.load_index_file(&shard_hash)?;
            if let Some(entry) = idx.find(hash) {
                return Ok(Located::Entry(entry.clone()));
            }
        }

        Ok(Located::NotFound)
    }

    /// Resolve `hash` to its encrypted bytes, searching the pack index.
    /// Returns the raw encrypted bytes; decryption is handled by the codec layer.
    fn resolve(&self, hash: &Hash) -> Result<Vec<u8>, Error> {
        match self.locate(hash)? {
            Located::Entry(entry) => self.resolve_entry(&entry, hash),
            Located::Standalone => {
                // Standalone objects may carry the ED E0 escape prefix; strip
                // it here so this path is consistent with resolve_entry /
                // fetch_from_remote_direct.
                let mut r = self.remote.open_read(hash)?;
                let mut buf = Vec::new();
                r.read_to_end(&mut buf).map_err(Error::Io)?;
                Ok(unescape_standalone(buf))
            }
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
        // Cheap fast path: a standalone object needs no index lookup at all.
        if self.remote.exists(hash)? {
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

    use crate::codec::pack::root_pointer::LocalRootPointer;
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
    fn read_standalone_step5_unescapes() {
        // Step 5 of resolve() reads a standalone object found directly in
        // objects/ that has no matching index entry. If the stored bytes carry
        // the ED E0 escape prefix, step 5 must strip it just like resolve_entry
        // and fetch_from_remote_direct do, otherwise the returned bytes are
        // corrupted by the leading ED E0.
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
        let got = reader.resolve(&hash).unwrap();
        assert_eq!(got, data, "step 5 standalone read must strip ED E0 escape");
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
}
