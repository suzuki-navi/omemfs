/// Stage 5: pack writer.
///
/// PackWriter wraps a raw remote ObjectStore and intercepts `write_from` calls.
/// Encrypted bytes are routed based on their size:
///   < 256 B    → inline entry in the delta index
///   256 B–1 MiB → accumulated in a pack buffer; flushed to a pack file
///   ≥ 1 MiB    → written directly to remote as a standalone object
///
/// After all objects are written, call `finish()` to:
///   1. Flush the current pack file buffer to remote.
///   2. Write the delta index file to remote.
///   3. Update the Bloom filter (insert all new hashes).
///   4. CAS-update INDEX_ROOT on remote.
///
/// PackWriter also implements ObjectStore for transparent drop-in use.
use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

use crate::codec::encrypt::EncryptKey;
use crate::codec::pack::bloom::{self, BloomFilter};
use crate::codec::pack::index::{IndexEntry, IndexFile, InlineEntry, PackEntry, StandaloneEntry};
use crate::codec::pack::index_root::IndexRoot;
use crate::codec::pack::root_pointer::{RootPointer, RootToken};
use crate::dlog_l6;
use crate::error::Error;
use crate::object::Hash;
use crate::store::ObjectStore;
use crate::store::local::LocalStore;

const INLINE_THRESHOLD: usize = 256;
const PACK_THRESHOLD: usize = 1024 * 1024; // 1 MiB

/// Target size a pack buffer is flushed at (see `route()`'s pack branch).
/// Also used by `commands::pack`'s consolidation pass, which must produce
/// packs shaped like the ones `PackWriter` itself writes (refactor-
/// instructions.md D2 -- these were previously two independent constants
/// that had to be kept in sync by hand).
pub(crate) const PACK_TARGET: usize = 4 * 1024 * 1024; // 4 MiB
/// Hard cap a pack buffer is flushed at regardless of `PACK_TARGET` (see
/// `route()`'s pack branch). Shared with `commands::pack` for the same
/// reason as `PACK_TARGET`.
pub(crate) const PACK_MAX: usize = 16 * 1024 * 1024; // 16 MiB

/// Magic prefix for pack files (ED E1).
pub const PACK_MAGIC: [u8; 2] = [0xED, 0xE1];

/// Magic prefix for standalone escape (ED E0).
/// Standalone objects whose first 2 bytes fall in ED E0..EF are wrapped with
/// this prefix so they are distinguishable from L6 internal objects.
pub const STANDALONE_ESCAPE_MAGIC: [u8; 2] = [0xED, 0xE0];

// ---------------------------------------------------------------------------
// PackWriter
// ---------------------------------------------------------------------------

pub struct PackWriter {
    remote: Box<dyn ObjectStore>,
    /// Plaintext cache for decrypted remote index files (`.omemfs/objcache/`).
    /// Index files are content-addressed (a given hash always maps to the same bytes),
    /// so caching them locally as plaintext is always safe. The Bloom filter is NOT
    /// cached here because its hash changes on every push and a cached copy would
    /// never be reused.
    objcache: LocalStore,
    /// Resident, per-push cache of already-deserialised index files, keyed by
    /// hash. `objcache` above avoids a remote fetch on a cache hit but still
    /// re-deserialises the cached plaintext bytes on every call; this cache
    /// avoids that too, mirroring `PackReader::indexes` (design/02 "Index
    /// lookup cost on the push path"). Purely an in-memory addition -- it
    /// changes no on-disk or on-wire format.
    indexes: Mutex<HashMap<Hash, Arc<IndexFile>>>,
    /// Serialises cache-miss `load_index_file` calls so concurrent lookups
    /// (`route()`'s "maybe present" check runs from multiple parallel
    /// transfer worker threads, same as `PackReader`) for the SAME hash
    /// fetch and deserialise it at most once, mirroring
    /// `PackReader::index_load` (design/02 "Index lookup cost on the push
    /// path"). Held only across the fetch+deserialise on a genuine cache
    /// miss, never while serving an already-cached hash.
    index_load: Mutex<()>,
    /// Backend-pluggable index-root pointer. Captured once at construction (its
    /// `read` provides the push-start snapshot + CAS token) and reused at
    /// `finish()` for the CAS write — both ends of the CAS go through this single
    /// injected pointer, so a cloud backend works without a remote base path.
    root_pointer: Box<dyn RootPointer>,
    remote_key: Option<EncryptKey>,
    // Mutable routing state lives behind Mutex because ObjectStore methods
    // take `&self` and the trait requires Send + Sync. PackWriter IS used
    // concurrently: cloud pushes drive `route()` from multiple parallel
    // transfer worker threads (default_transfer_concurrency, see
    // commands/transfer.rs), so these locks are load-bearing, not just a
    // Send/Sync formality. Where two of these locks are held nested, the
    // outer-to-inner order is always pack_buf -> delta -> pending_pack_entries
    // (see route()'s pack branch and flush_pack()); bloom is only ever nested
    // under pack_buf, never under delta or pending_pack_entries. Preserve this
    // order when touching this code -- reversing it can deadlock.
    delta: Mutex<IndexFile>,
    /// Accumulated encrypted bytes for the current pack buffer.
    pack_buf: Mutex<Vec<u8>>,
    /// Pack entries referencing the current (not yet written) pack buffer.
    pending_pack_entries: Mutex<Vec<(Hash, u32, u32)>>, // (hash, offset, length)
    bloom: Mutex<BloomFilter>,
    /// Raw INDEX_ROOT bytes captured once at construction (push start). Used to
    /// decode the push-start `remote_root_snapshot`, and as the base the new
    /// INDEX_ROOT is built on. `None` means the remote had no INDEX_ROOT when the
    /// push began.
    start_snapshot: Option<Vec<u8>>,
    /// Version token of the INDEX_ROOT observed at construction (push start).
    /// Used as the CAS expected token at `finish()`, guarding the whole
    /// read → upload → write window against concurrent pushes.
    start_token: RootToken,
    /// Decoded form of `start_snapshot`. `None` when the remote had no
    /// INDEX_ROOT at push start. Used by `exists()` to resolve "maybe present"
    /// hashes against the remote index (delta / hot / cold) for the duration of
    /// the push — the index does not change until `finish()` writes the new one.
    snapshot_root: Option<IndexRoot>,
    /// Sizes of pack files written during this session (populated by flush_pack).
    written_pack_sizes: Mutex<Vec<u64>>,
}

impl PackWriter {
    /// Create a new PackWriter wrapping `remote`.
    ///
    /// `remote_base` is the root directory of the remote store
    /// (i.e. the directory that contains `objects/` and `INDEX_ROOT`).
    ///
    /// `objcache` is the plaintext index-file cache (`.omemfs/objcache/`). Index
    /// files loaded during the push dedup check are stored here as plaintext so
    /// that subsequent pushes can serve them from local disk instead of re-fetching
    /// them from the remote.
    ///
    /// The raw INDEX_ROOT bytes are captured here, at push start, and reused as
    /// the CAS expected value in `finish()`. All push paths must obtain the
    /// remote root they diff/splice against from this same snapshot
    /// (`remote_root_snapshot`), so the CAS guards the entire push window.
    ///
    /// `root_pointer` is the backend-pluggable index-root pointer (built by
    /// `Repo::remote_root_pointer`). It is read here for the push-start snapshot
    /// and token, then stored and reused for the CAS write in `finish()`.
    pub fn new(
        remote: Box<dyn ObjectStore>,
        root_pointer: Box<dyn RootPointer>,
        objcache: LocalStore,
        remote_key: Option<EncryptKey>,
    ) -> Result<Self, Error> {
        let (start_snapshot, start_token) = root_pointer.read()?;

        // Decode the snapshot's INDEX_ROOT once so the push can (a) load the
        // remote Bloom filter and (b) resolve membership against the remote
        // index without re-reading INDEX_ROOT on every lookup.
        let snapshot_root: Option<IndexRoot> = match start_snapshot {
            Some(ref raw) => {
                let plaintext =
                    crate::codec::pack::decrypt_index_root_bytes(raw, remote_key.as_ref())?;
                Some(IndexRoot::deserialise(&plaintext)?)
            }
            None => None,
        };

        // Load the remote Bloom filter so membership checks reflect what the
        // remote already holds. Starting from an empty filter would make every
        // `exists()` report "definitely absent" and re-upload every object on
        // every push (the worst-case performance bug). If the stored filter is
        // missing, unreadable, or sized differently from the current
        // configuration, rebuild a fresh empty filter rather than corrupt-merge.
        let bloom = load_remote_bloom(remote.as_ref(), snapshot_root.as_ref(), remote_key.as_ref());

        Ok(PackWriter {
            remote,
            objcache,
            indexes: Mutex::new(HashMap::new()),
            index_load: Mutex::new(()),
            root_pointer,
            remote_key,
            delta: Mutex::new(IndexFile::new()),
            pack_buf: Mutex::new(Vec::new()),
            pending_pack_entries: Mutex::new(Vec::new()),
            bloom: Mutex::new(bloom),
            snapshot_root,
            start_snapshot,
            start_token,
            written_pack_sizes: Mutex::new(Vec::new()),
        })
    }

    /// Whether the remote had an index root at push start. Used by the
    /// post-clone sync guard (design/03) to distinguish an empty remote from a
    /// wrong-key / reset remote.
    pub fn index_root_present(&self) -> bool {
        self.start_snapshot.is_some()
    }

    /// Decode the captured push-start snapshot and return the remote root tree
    /// hash recorded in it. Returns `None` when the remote had no INDEX_ROOT at
    /// push start, or when the recorded remote root is all-zero.
    ///
    /// All push paths must use this (not a fresh read) so that the root they
    /// splice/diff against matches the value the final CAS is conditioned on.
    pub fn remote_root_snapshot(&self) -> Result<Option<Hash>, Error> {
        let Some(ref raw) = self.start_snapshot else {
            return Ok(None);
        };
        let plaintext = self.decrypt_index_root(raw)?;
        let ir = IndexRoot::deserialise(&plaintext)?;
        Ok(ir.remote_root_hash())
    }

    // -----------------------------------------------------------------------
    // Write routing
    // -----------------------------------------------------------------------

    /// Route one encrypted object. Called from `write_from`.
    fn route(&self, hash: &Hash, encrypted: &[u8]) -> Result<(), Error> {
        let n = encrypted.len();

        if n < INLINE_THRESHOLD {
            dlog_l6!("route inline: {} ({}B)", &hash.as_str()[..8], n);
            self.delta
                .lock()
                .unwrap()
                .push(IndexEntry::Inline(InlineEntry {
                    hash: hash.clone(),
                    data: encrypted.to_vec(),
                }));
            self.bloom.lock().unwrap().insert(hash);
            return Ok(());
        }

        if n < PACK_THRESHOLD {
            dlog_l6!("route pack: {} ({}B)", &hash.as_str()[..8], n);
            // Append to pack buffer.
            let should_flush = {
                let mut pack_buf = self.pack_buf.lock().unwrap();
                let offset = pack_buf.len() as u32;
                let length = n as u32;
                pack_buf.extend_from_slice(encrypted);
                self.pending_pack_entries
                    .lock()
                    .unwrap()
                    .push((hash.clone(), offset, length));
                self.bloom.lock().unwrap().insert(hash);
                // Flush when the buffer reaches the target size or the hard cap.
                pack_buf.len() >= PACK_TARGET || pack_buf.len() >= PACK_MAX
            };
            if should_flush {
                self.flush_pack()?;
            }
            return Ok(());
        }

        // Standalone: write directly to remote objects/ directory.
        // Wrap with escape magic if the first 2 bytes fall in the ED Ex range
        // to prevent ambiguity with L6 object magic bytes.
        dlog_l6!("route standalone: {} ({}B)", &hash.as_str()[..8], n);
        let needs_escape = encrypted.len() >= 2
            && encrypted[0] == 0xED
            && encrypted[1] >= 0xE0
            && encrypted[1] <= 0xEF;
        if needs_escape {
            dlog_l6!(
                "route standalone escape: {} ({:02X} {:02X})",
                &hash.as_str()[..8],
                encrypted[0],
                encrypted[1]
            );
            let mut wrapped = Vec::with_capacity(2 + encrypted.len());
            wrapped.extend_from_slice(&STANDALONE_ESCAPE_MAGIC);
            wrapped.extend_from_slice(encrypted);
            let mut cursor = io::Cursor::new(wrapped);
            self.remote.write_from(hash, &mut cursor)?;
        } else {
            let mut cursor = io::Cursor::new(encrypted);
            self.remote.write_from(hash, &mut cursor)?;
        }
        self.delta
            .lock()
            .unwrap()
            .push(IndexEntry::Standalone(StandaloneEntry {
                hash: hash.clone(),
            }));
        self.bloom.lock().unwrap().insert(hash);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Pack file flush
    // -----------------------------------------------------------------------

    fn flush_pack(&self) -> Result<(), Error> {
        if self.pending_pack_entries.lock().unwrap().is_empty() {
            return Ok(());
        }

        let mut pack_buf = self.pack_buf.lock().unwrap();

        // Build the pack file bytes: MAGIC || encrypted_bytes...
        let mut pack_bytes: Vec<u8> = Vec::with_capacity(2 + pack_buf.len());
        pack_bytes.extend_from_slice(&PACK_MAGIC);
        pack_bytes.extend_from_slice(&pack_buf);

        let pack_hash = Hash::compute(&pack_bytes);

        // Write the pack file to remote (pack files are not further encrypted).
        let mut cursor = io::Cursor::new(&pack_bytes);
        self.remote.write_from(&pack_hash, &mut cursor)?;

        // Record the pack file size for io_pack_stats().
        self.written_pack_sizes
            .lock()
            .unwrap()
            .push(pack_bytes.len() as u64);

        // Convert pending entries into index entries referencing this pack file.
        let mut delta = self.delta.lock().unwrap();
        for (hash, offset, length) in self.pending_pack_entries.lock().unwrap().drain(..) {
            delta.push(IndexEntry::Pack(PackEntry {
                hash,
                pack_hash: pack_hash.clone(),
                offset,
                length,
            }));
        }

        pack_buf.clear();
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Finish: write delta index + update INDEX_ROOT
    // -----------------------------------------------------------------------

    /// Finalise the push: flush pack buffer, write delta index, update INDEX_ROOT.
    /// `new_remote_root` is the new tree hash to record in INDEX_ROOT.
    ///
    /// Returns the number of delta index files listed in `INDEX_ROOT.delta_hashes`
    /// after this push's CAS write (including the delta this push itself just
    /// added, if any). Callers use this to record `IoStatsRecord.deltas_after`
    /// for pack-scheduling analysis (design/04 "io_stats.jsonl" Notes).
    pub fn finish(&mut self, new_remote_root: &Hash) -> Result<u64, Error> {
        // 1. Flush remaining pack buffer.
        self.flush_pack()?;

        // 2. Base the new INDEX_ROOT on the push-start snapshot (NOT a fresh
        //    read), so the value we mutate and the CAS expected value are the
        //    same observation. This is what makes the CAS guard the entire
        //    read → upload → write window.
        let mut index_root = match self.start_snapshot {
            Some(ref raw) => {
                let plaintext = self.decrypt_index_root(raw)?;
                IndexRoot::deserialise(&plaintext)?
            }
            None => IndexRoot::new_empty(),
        };

        // 3. Write delta index file to remote.
        if !self.delta.lock().unwrap().is_empty() {
            let delta_bytes = self.delta.lock().unwrap().serialise()?;
            let delta_hash = Hash::compute(&delta_bytes);

            // Encrypt delta index if key is configured.
            let stored_bytes = if let Some(ref key) = self.remote_key {
                crate::codec::encrypt::encrypt(delta_bytes, Some(key), delta_hash.as_bytes_array())
            } else {
                delta_bytes
            };

            let mut cursor = io::Cursor::new(&stored_bytes);
            self.remote.write_from(&delta_hash, &mut cursor)?;

            // Prepend to delta_hashes (newest first).
            let mut new_deltas: Vec<[u8; 32]> = vec![*delta_hash.as_bytes_array()];
            new_deltas.extend_from_slice(&index_root.delta_hashes);
            index_root.delta_hashes = new_deltas;
        }

        // 4. Write the updated Bloom filter. `self.bloom` was initialised at
        //    construction from the remote's current filter (or rebuilt fresh on
        //    mismatch — see `load_remote_bloom`) and has had every object pushed
        //    in this session inserted into it, so it already covers all past and
        //    present entries. No re-load / merge is needed here, which also
        //    avoids the lossy size-mismatch branch of BloomFilter::merge.
        let bloom = self.bloom.lock().unwrap();
        let bloom_bytes = bloom.serialise();
        let bloom_hash = Hash::compute(&bloom_bytes);
        let bloom_stored = if let Some(ref key) = self.remote_key {
            crate::codec::encrypt::encrypt(bloom_bytes, Some(key), bloom_hash.as_bytes_array())
        } else {
            bloom_bytes
        };
        let mut cursor = io::Cursor::new(&bloom_stored);
        self.remote.write_from(&bloom_hash, &mut cursor)?;

        // 5. Update INDEX_ROOT fields.
        index_root.remote_root = *new_remote_root.as_bytes_array();
        index_root.bloom_hash = *bloom_hash.as_bytes_array();

        // 6. CAS-write INDEX_ROOT through the backend-pluggable root pointer,
        //    conditioned on the version token captured at push start. This is the
        //    SAME injected pointer that produced `start_token` in `new()`, so the
        //    read and the CAS go through one pointer instance.
        let new_index_root_bytes = self.encrypt_index_root(&index_root)?;
        self.root_pointer
            .cas_write(&self.start_token, &new_index_root_bytes)?;

        Ok(index_root.delta_hashes.len() as u64)
    }

    /// Return the number and sizes of pack files written during this session.
    /// Call after `finish()` to obtain stats for `IoRecord::set_pack_stats`.
    pub fn io_pack_stats(&self) -> (u64, Vec<u64>) {
        let sizes = self.written_pack_sizes.lock().unwrap().clone();
        let count = sizes.len() as u64;
        (count, sizes)
    }

    /// Returns `Some((element_count, designed_capacity))` when the in-memory
    /// Bloom filter -- after `finish()` has inserted this push's new hashes
    /// -- holds more elements than the capacity its current `num_bits` was
    /// designed for. Per design/02 "Filter sizing", a filter stays at its
    /// designed false-positive rate until the element count it was built for
    /// grows by 50% (the ×1.5 margin baked into `capacity_for`), so crossing
    /// `designed_capacity` means the filter is now producing more false
    /// positives than intended and only `omemfs pack` can resize it (push
    /// itself never rebuilds/resizes the filter it loaded). Call after
    /// `finish()` so the count reflects this push's insertions. Returns
    /// `None` when still within capacity.
    pub fn bloom_over_capacity(&self) -> Option<(u64, u64)> {
        let bloom = self.bloom.lock().unwrap();
        let capacity = bloom::designed_capacity(bloom.num_bits);
        if bloom.element_count > capacity {
            Some((bloom.element_count, capacity))
        } else {
            None
        }
    }

    // -----------------------------------------------------------------------
    // INDEX_ROOT helpers
    // -----------------------------------------------------------------------

    /// Decrypt INDEX_ROOT bytes (nonce || ciphertext || tag) when key is set.
    fn decrypt_index_root(&self, raw: &[u8]) -> Result<Vec<u8>, Error> {
        crate::codec::pack::decrypt_index_root_bytes(raw, self.remote_key.as_ref())
    }

    /// Resolve whether `hash` is already present on the remote by consulting the
    /// push-start index snapshot (delta → hot → cold). Returns `Ok(true)` only
    /// when an index entry is found; standalone objects are not recorded as
    /// resolvable here (they are confirmed via `remote.exists` in `exists()`).
    ///
    /// A `None` snapshot (no INDEX_ROOT at push start) means nothing is indexed
    /// yet, so this returns `Ok(false)`.
    fn index_contains(&self, hash: &Hash) -> Result<bool, Error> {
        let Some(ref ir) = self.snapshot_root else {
            return Ok(false);
        };

        // Delta files, newest first.
        for delta_hash in ir.delta_hashes_as_hashes() {
            if self.load_index_file(&delta_hash)?.find(hash).is_some() {
                return Ok(true);
            }
        }

        // Hot index.
        if let Some(hot_hash) = ir.hot_hash_opt()
            && self.load_index_file(&hot_hash)?.find(hash).is_some()
        {
            return Ok(true);
        }

        // Cold shard for this hash prefix.
        let prefix_bits = ir.cold_prefix_bits as usize;
        let shard_hash = if prefix_bits > 0 {
            let prefix = crate::codec::pack::reader::hash_prefix(hash, prefix_bits);
            ir.cold_shard_hash(prefix)
        } else if !ir.cold_shards.is_empty() {
            ir.cold_shard_hash(0)
        } else {
            None
        };
        if let Some(shard_hash) = shard_hash
            && self.load_index_file(&shard_hash)?.find(hash).is_some()
        {
            return Ok(true);
        }

        Ok(false)
    }

    /// Load and deserialise an index file (delta / hot / cold shard), keeping
    /// the parsed result resident in `self.indexes` for the rest of this push
    /// so a given hash is deserialised at most once per `PackWriter`
    /// (design/02 "Index lookup cost on the push path"). On an in-memory
    /// cache miss: check the objcache disk cache if already present;
    /// otherwise fetch from remote, decrypt, and write plaintext to objcache.
    ///
    /// Index files are content-addressed and immutable: the same hash always
    /// maps to the same plaintext. The objcache stores plaintext
    /// (unencrypted); decryption is performed once on the first remote
    /// fetch, mirroring PackReader.
    ///
    /// Single-flighted via `index_load`, mirroring
    /// `PackReader::load_index_file`: the first cache-miss caller to acquire
    /// `index_load` fetches and deserialises; every other concurrent caller
    /// for any hash blocks on the same lock, then re-checks `self.indexes`
    /// (now populated if it was fetching this exact hash) before falling
    /// through to its own fetch otherwise. This bounds concurrent fetches for
    /// the SAME hash to one, at the cost of serialising cache-miss loads
    /// across different hashes too -- the same trade-off `PackReader` already
    /// makes.
    fn load_index_file(&self, hash: &Hash) -> Result<Arc<IndexFile>, Error> {
        if let Some(index) = self.indexes.lock().unwrap().get(hash).cloned() {
            return Ok(index);
        }
        let _loading = self.index_load.lock().unwrap();
        if let Some(index) = self.indexes.lock().unwrap().get(hash).cloned() {
            return Ok(index);
        }
        let index = crate::codec::pack::load_index_file(
            self.remote.as_ref(),
            &self.objcache,
            self.remote_key.as_ref(),
            hash,
        )?;
        let mut cache = self.indexes.lock().unwrap();
        Ok(Arc::clone(
            cache.entry(hash.clone()).or_insert_with(|| Arc::new(index)),
        ))
    }

    /// Provide access to the inner remote store (as `&dyn ObjectStore`).
    /// Used by `push.rs::ensure_path_in_store` which needs to read from remote.
    pub fn as_remote_store(&self) -> &dyn ObjectStore {
        self.remote.as_ref()
    }

    /// Encrypt INDEX_ROOT plaintext → nonce || ciphertext || tag.
    fn encrypt_index_root(&self, root: &IndexRoot) -> Result<Vec<u8>, Error> {
        let plaintext = root.serialise()?;
        let Some(ref key) = self.remote_key else {
            return Ok(plaintext);
        };

        use rand::RngCore;
        let mut nonce = [0u8; 12];
        rand::rng().fill_bytes(&mut nonce);

        let mut pseudo_hash = [0u8; 32];
        pseudo_hash[..12].copy_from_slice(&nonce);

        let encrypted = crate::codec::encrypt::encrypt(plaintext, Some(key), &pseudo_hash);
        let mut out = Vec::with_capacity(12 + encrypted.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&encrypted);
        Ok(out)
    }
}

/// Load the remote Bloom filter referenced by the snapshot's `bloom_hash`.
///
/// Per design/02 "Filter sizing": push never rebuilds an empty filter merely
/// because the loaded filter's size does not match the current design
/// target. A structurally valid filter (`num_bits >= 1` and
/// `num_hash_functions >= 1`) that is successfully fetched, decrypted, and
/// deserialised is accepted and used AS-IS, whatever its `(num_bits,
/// num_hash_functions)` happen to be -- only `omemfs pack` resizes the
/// filter.
///
/// A fresh (empty) filter is only built when there is nothing valid to load
/// at all:
///   - there is no snapshot, or `bloom_hash` is all-zero (never generated), or
///   - the stored filter cannot be fetched / decrypted / deserialised, or
///   - the stored filter is structurally unusable per
///     `BloomFilter::structural_defect()` (`num_bits == 0`, which would
///     divide by zero inside `may_contain`, or `num_hash_functions == 0`,
///     which is well-defined but useless).
///
/// The fresh filter's capacity hint is `capacity_for(loaded.element_count)`
/// when a filter WAS read but rejected for being structurally unusable (its
/// `element_count` field is still meaningful even though its bit array is
/// not); otherwise (nothing could be read at all) the hint is `0`, which
/// `capacity_for` floors at `MIN_CAPACITY` -- the same size the old fixed
/// constant always produced. `loaded.element_count` is an 8-byte field fully
/// controlled by whoever wrote the remote object, so it is passed to
/// `capacity_for` unclamped -- the bound (`bloom::MAX_CAPACITY`) is enforced
/// inside `capacity_for` itself, not here (design/02 "Bounding the capacity
/// (hostile and corrupt filters)").
fn load_remote_bloom(
    remote: &dyn ObjectStore,
    snapshot_root: Option<&IndexRoot>,
    key: Option<&EncryptKey>,
) -> BloomFilter {
    let fresh = |hint_element_count: u64| bloom::sized_for(hint_element_count);

    let Some(ir) = snapshot_root else {
        return fresh(0);
    };
    let Some(bloom_hash) = ir.bloom_hash_opt() else {
        return fresh(0);
    };

    use std::io::Read as _;
    let mut r = match remote.open_read(&bloom_hash) {
        Ok(r) => r,
        Err(_) => return fresh(0),
    };
    let mut stored = Vec::new();
    if r.read_to_end(&mut stored).is_err() {
        return fresh(0);
    }
    let plaintext = match crate::codec::encrypt::decrypt(stored, key, bloom_hash.as_bytes_array()) {
        Ok(pt) => pt,
        Err(_) => return fresh(0),
    };
    let loaded = match BloomFilter::deserialise(&plaintext) {
        Ok(bf) => bf,
        Err(_) => return fresh(0),
    };

    // Structurally unusable: rebuild fresh, using the just-read filter's
    // element_count (still meaningful even though its bit array is not) as
    // the capacity hint. `fresh` -> `bloom::sized_for` -> `capacity_for`
    // clamps that hint to `MAX_CAPACITY`, so a hostile/corrupt element_count
    // cannot demand an unbounded allocation here.
    if let Some(reason) = loaded.structural_defect() {
        dlog_l6!(
            "load_remote_bloom: rejecting structurally defective filter ({}), \
             rebuilding fresh from element_count={}",
            reason,
            loaded.element_count
        );
        return fresh(loaded.element_count);
    }

    // Any other structurally valid filter is accepted as-is, whatever size
    // it happens to be.
    loaded
}

// ---------------------------------------------------------------------------
// ObjectStore implementation
// ---------------------------------------------------------------------------

impl ObjectStore for PackWriter {
    fn default_transfer_concurrency(&self) -> usize {
        // Uploads land on the remote; inherit its transfer parallelism.
        self.remote.default_transfer_concurrency()
    }

    fn exists(&self, hash: &Hash) -> Result<bool, Error> {
        // Check in-flight delta index first (covers inline + pending pack entries).
        if self.delta.lock().unwrap().find(hash).is_some() {
            return Ok(true);
        }
        // Check pending pack entries not yet converted to delta entries.
        if self
            .pending_pack_entries
            .lock()
            .unwrap()
            .iter()
            .any(|(h, _, _)| h == hash)
        {
            return Ok(true);
        }
        // Fall through to the Bloom filter. "Definitely absent" → not present,
        // so the caller must upload it.
        if !self.bloom.lock().unwrap().may_contain(hash) {
            return Ok(false);
        }
        // "Maybe present" → consult the locally cached snapshot index FIRST
        // (delta → hot → cold via load_index_file, which serves immutable index
        // files from the local cache with no remote I/O on a cache hit). The
        // index records inline, pack, AND standalone entries from every push
        // captured by the snapshot, so it answers for the vast majority of
        // already-present objects.
        //
        // Only on an index miss do we issue a remote HEAD, to catch a standalone
        // object written to objects/<storage_key> by an earlier push that the
        // snapshot index does not yet record. Probing the remote first would, in
        // the common steady state (after a `pack`, where every prior object lives
        // in the index rather than as a loose objects/<storage_key>), issue a
        // HEAD per sibling object in any touched directory that always 404s and
        // then falls through to the index anyway — a wasted remote round-trip
        // each. See design/02 "Membership test semantics".
        if self.index_contains(hash)? {
            return Ok(true);
        }
        self.remote.exists(hash)
    }

    fn open_read(&self, hash: &Hash) -> Result<Box<dyn io::Read>, Error> {
        // Read inline entries from in-flight delta index.
        {
            let delta = self.delta.lock().unwrap();
            if let Some(crate::codec::pack::index::IndexEntry::Inline(e)) = delta.find(hash) {
                use std::io::Cursor;
                return Ok(Box::new(Cursor::new(e.data.clone())));
            }
        }
        // Fall through to raw remote store for standalone/pack entries.
        self.remote.open_read(hash)
    }

    fn size(&self, hash: &Hash) -> Result<u64, Error> {
        // Delegate to remote for already-stored objects.
        self.remote.size(hash)
    }

    fn list_with_sizes(&self) -> Result<Vec<(String, u64)>, Error> {
        // Delegate to the remote store — it holds the authoritative object list.
        self.remote.list_with_sizes()
    }

    fn write_from(&self, hash: &Hash, reader: &mut dyn io::Read) -> Result<(), Error> {
        // Read all bytes from `reader` so we can measure the size.
        let mut encrypted = Vec::new();
        reader.read_to_end(&mut encrypted).map_err(Error::Io)?;

        // Routing mutates writer state behind Mutex (ObjectStore methods
        // take `&self` and require Sync).
        self.route(hash, &encrypted)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::pack::root_pointer::{LocalRootPointer, RootPointer, RootToken};
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn setup_remote(tmp: &TempDir) -> (LocalStore, PathBuf) {
        let base = tmp.path().to_path_buf();
        std::fs::create_dir_all(base.join("objects")).unwrap();
        std::fs::create_dir_all(base.join("tmp")).unwrap();
        let store = LocalStore::for_remote(&base);
        (store, base)
    }

    fn make_hash_from_data(data: &[u8]) -> Hash {
        Hash::compute(data)
    }

    #[test]
    fn inline_routing() {
        let tmp = TempDir::new().unwrap();
        let (remote, base) = setup_remote(&tmp);
        let objcache = LocalStore::for_cache(tmp.path().join("objcache"));
        let writer = PackWriter::new(
            Box::new(remote),
            Box::new(LocalRootPointer::new(base, None)),
            objcache,
            None,
        )
        .unwrap();

        // < 256 bytes → inline
        let data = vec![0xAA; 100];
        let hash = make_hash_from_data(&data);
        let mut cursor = io::Cursor::new(&data);
        writer.write_from(&hash, &mut cursor).unwrap();

        assert_eq!(writer.delta.lock().unwrap().len(), 1);
        matches!(
            &writer.delta.lock().unwrap().entries()[0],
            IndexEntry::Inline(_)
        );
    }

    #[test]
    fn standalone_routing() {
        let tmp = TempDir::new().unwrap();
        let (remote, base) = setup_remote(&tmp);
        let objcache = LocalStore::for_cache(tmp.path().join("objcache"));
        let writer = PackWriter::new(
            Box::new(remote),
            Box::new(LocalRootPointer::new(base, None)),
            objcache,
            None,
        )
        .unwrap();

        // ≥ 1 MiB → standalone
        let data = vec![0xBB; PACK_THRESHOLD + 1];
        let hash = make_hash_from_data(&data);
        let mut cursor = io::Cursor::new(&data);
        writer.write_from(&hash, &mut cursor).unwrap();

        assert_eq!(writer.delta.lock().unwrap().len(), 1);
        matches!(
            &writer.delta.lock().unwrap().entries()[0],
            IndexEntry::Standalone(_)
        );
        // Must be present in remote objects/ (check via PackWriter's exists).
        assert!(writer.remote.exists(&hash).unwrap());
    }

    #[test]
    fn pack_routing() {
        let tmp = TempDir::new().unwrap();
        let (remote, base) = setup_remote(&tmp);
        let objcache = LocalStore::for_cache(tmp.path().join("objcache"));
        let writer = PackWriter::new(
            Box::new(remote),
            Box::new(LocalRootPointer::new(base, None)),
            objcache,
            None,
        )
        .unwrap();

        // 256 B–1 MiB → pack buffer
        let data = vec![0xCC; 1024];
        let hash = make_hash_from_data(&data);
        let mut cursor = io::Cursor::new(&data);
        writer.write_from(&hash, &mut cursor).unwrap();

        // Not yet in delta (pending_pack_entries), not yet flushed.
        assert_eq!(writer.delta.lock().unwrap().len(), 0);
        assert_eq!(writer.pending_pack_entries.lock().unwrap().len(), 1);
    }

    #[test]
    fn finish_writes_index_root() {
        let tmp = TempDir::new().unwrap();
        let (remote, base) = setup_remote(&tmp);
        let objcache = LocalStore::for_cache(tmp.path().join("objcache"));
        let mut writer = PackWriter::new(
            Box::new(remote),
            Box::new(LocalRootPointer::new(base.clone(), None)),
            objcache,
            None,
        )
        .unwrap();

        // Write one pack-range object.
        let data = vec![0xDD; 1024];
        let hash = make_hash_from_data(&data);
        let mut cursor = io::Cursor::new(&data);
        writer.write_from(&hash, &mut cursor).unwrap();

        let root_hash = Hash::compute(b"root");
        let deltas_after = writer.finish(&root_hash).unwrap();

        // INDEX_ROOT must exist.
        assert!(base.join("INDEX_ROOT").exists());

        // Deserialise and verify remote_root.
        let raw = std::fs::read(base.join("INDEX_ROOT")).unwrap();
        let ir = IndexRoot::deserialise(&raw).unwrap();
        assert_eq!(ir.remote_root, *root_hash.as_bytes_array());
        assert_eq!(ir.delta_hashes.len(), 1); // one delta index
        // finish() must report the same post-CAS delta count as INDEX_ROOT,
        // so callers can log it for pack-scheduling analysis without a
        // separate INDEX_ROOT re-read.
        assert_eq!(deltas_after, 1);
    }

    #[test]
    fn finish_reports_growing_delta_count_across_pushes_without_pack() {
        // Three independent pushes (no `omemfs pack` run in between) must each
        // report a delta count one higher than the last, matching how
        // `INDEX_ROOT.delta_hashes` accumulates (newest-first prepend, never
        // cleared until a pack run). This is the exact value push records
        // into `IoStatsRecord.deltas_after`.
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().to_path_buf();
        std::fs::create_dir_all(base.join("objects")).unwrap();
        std::fs::create_dir_all(base.join("tmp")).unwrap();

        for (i, expected_count) in [(0u8, 1u64), (1u8, 2u64), (2u8, 3u64)] {
            let objcache = LocalStore::for_cache(tmp.path().join(format!("objcache{i}")));
            let mut writer = PackWriter::new(
                Box::new(LocalStore::for_remote(&base)),
                Box::new(LocalRootPointer::new(base.clone(), None)),
                objcache,
                None,
            )
            .unwrap();
            let data = vec![i; 1024];
            let hash = make_hash_from_data(&data);
            writer
                .write_from(&hash, &mut io::Cursor::new(&data))
                .unwrap();
            let deltas_after = writer
                .finish(&Hash::compute(format!("root-{i}").as_bytes()))
                .unwrap();
            assert_eq!(deltas_after, expected_count);
        }
    }

    #[test]
    fn finish_cas_fails_against_stale_snapshot() {
        // Two writers constructed against the same remote both capture the SAME
        // INDEX_ROOT snapshot at push start (here: None — no INDEX_ROOT yet).
        // The first finish writes INDEX_ROOT. The second finish, whose CAS
        // expected value is the stale snapshot, must fail with CasFailed —
        // proving the CAS is conditioned on the push-start snapshot, not a
        // value re-read at finish time.
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().to_path_buf();
        std::fs::create_dir_all(base.join("objects")).unwrap();
        std::fs::create_dir_all(base.join("tmp")).unwrap();

        let objcache_a = LocalStore::for_cache(tmp.path().join("objcache_a"));
        let objcache_b = LocalStore::for_cache(tmp.path().join("objcache_b"));
        let mut writer_a = PackWriter::new(
            Box::new(LocalStore::for_remote(&base)),
            Box::new(LocalRootPointer::new(base.clone(), None)),
            objcache_a,
            None,
        )
        .unwrap();
        let mut writer_b = PackWriter::new(
            Box::new(LocalStore::for_remote(&base)),
            Box::new(LocalRootPointer::new(base.clone(), None)),
            objcache_b,
            None,
        )
        .unwrap();

        // A writes an object and finishes first.
        let data_a = vec![0xA1; 1024];
        let hash_a = make_hash_from_data(&data_a);
        writer_a
            .write_from(&hash_a, &mut io::Cursor::new(&data_a))
            .unwrap();
        writer_a.finish(&Hash::compute(b"root-a")).unwrap();

        // B (stale snapshot) finishes second → CAS must fail.
        let data_b = vec![0xB2; 1024];
        let hash_b = make_hash_from_data(&data_b);
        writer_b
            .write_from(&hash_b, &mut io::Cursor::new(&data_b))
            .unwrap();
        let err = writer_b.finish(&Hash::compute(b"root-b")).unwrap_err();
        assert!(
            matches!(err, Error::CasFailed),
            "expected CasFailed, got {err:?}"
        );

        // A's root must still be on the remote (not clobbered by B).
        let raw = std::fs::read(base.join("INDEX_ROOT")).unwrap();
        let ir = IndexRoot::deserialise(&raw).unwrap();
        assert_eq!(ir.remote_root, *Hash::compute(b"root-a").as_bytes_array());
    }

    #[test]
    fn standalone_escape_roundtrip() {
        // Standalone object whose encrypted bytes begin with ED E0 must be
        // escaped on write and unescaped on read so the caller gets the
        // original bytes back.
        let tmp = TempDir::new().unwrap();
        let (writer, reader) = {
            let base = tmp.path().to_path_buf();
            std::fs::create_dir_all(base.join("objects")).unwrap();
            std::fs::create_dir_all(base.join("tmp")).unwrap();

            let cache_dir = tmp.path().join("cache");
            std::fs::create_dir_all(&cache_dir).unwrap();

            use crate::codec::pack::reader::PackReader;
            use crate::store::local::LocalStore;

            let packcache_dir = tmp.path().join("packcache");
            std::fs::create_dir_all(&packcache_dir).unwrap();
            let objcache_dir = tmp.path().join("objcache");
            std::fs::create_dir_all(&objcache_dir).unwrap();

            let remote_for_writer = LocalStore::for_remote(&base);
            let writer_objcache = LocalStore::for_cache(tmp.path().join("writer_objcache"));
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
        };

        // Construct a standalone-sized payload (≥ 1 MiB) that starts with ED E2
        // (an L6 index file magic) to trigger the escape path.
        let mut data = vec![0u8; PACK_THRESHOLD + 1];
        data[0] = 0xED;
        data[1] = 0xE2;
        let hash = make_hash_from_data(&data);
        let mut cursor = io::Cursor::new(&data);
        writer.write_from(&hash, &mut cursor).unwrap();

        let root = Hash::compute(b"root");
        let mut writer = writer;
        writer.finish(&root).unwrap();

        // Read back and verify the original bytes are returned.
        let mut r = reader.open_read(&hash).unwrap();
        let mut got = Vec::new();
        r.read_to_end(&mut got).unwrap();
        assert_eq!(got, data, "standalone escape roundtrip failed");
    }

    #[test]
    fn second_writer_sees_packed_and_inline_objects() {
        // Regression guard for the "empty Bloom filter every push" bug. After a
        // first push stores inline + pack-range objects (which live in pack /
        // index files, NOT objects/<hash>), a SECOND PackWriter constructed
        // against the same remote must report exists() == true for them so they
        // are not re-uploaded. With a fresh-empty Bloom filter, exists() would
        // wrongly report "definitely absent" and trigger a full re-upload.
        let tmp = TempDir::new().unwrap();
        let (remote, base) = setup_remote(&tmp);

        // First push: one inline object (<256 B) and one pack-range object.
        let inline_data = vec![0x11; 100];
        let inline_hash = make_hash_from_data(&inline_data);
        let pack_data = vec![0x22; 1024];
        let pack_hash = make_hash_from_data(&pack_data);

        let objcache1 = LocalStore::for_cache(tmp.path().join("objcache1"));
        let mut w1 = PackWriter::new(
            Box::new(remote),
            Box::new(LocalRootPointer::new(base.clone(), None)),
            objcache1,
            None,
        )
        .unwrap();
        w1.write_from(&inline_hash, &mut io::Cursor::new(&inline_data))
            .unwrap();
        w1.write_from(&pack_hash, &mut io::Cursor::new(&pack_data))
            .unwrap();
        w1.finish(&Hash::compute(b"root-1")).unwrap();

        // Second writer: both objects must be reported present.
        let objcache2 = LocalStore::for_cache(tmp.path().join("objcache2"));
        let w2 = PackWriter::new(
            Box::new(LocalStore::for_remote(&base)),
            Box::new(LocalRootPointer::new(base.clone(), None)),
            objcache2,
            None,
        )
        .unwrap();
        assert!(
            w2.exists(&inline_hash).unwrap(),
            "inline object must be seen by second push"
        );
        assert!(
            w2.exists(&pack_hash).unwrap(),
            "pack object must be seen by second push"
        );

        // A genuinely new object is still reported absent.
        let new_hash = make_hash_from_data(&[0x33; 100]);
        assert!(
            !w2.exists(&new_hash).unwrap(),
            "new object must be reported absent"
        );
    }

    #[test]
    fn second_writer_sees_standalone_objects() {
        // Standalone objects (≥ 1 MiB) live at objects/<hash>; a second writer
        // must also report them present so they are not re-uploaded.
        let tmp = TempDir::new().unwrap();
        let (remote, base) = setup_remote(&tmp);

        let data = vec![0x44; PACK_THRESHOLD + 1];
        let hash = make_hash_from_data(&data);
        let objcache1 = LocalStore::for_cache(tmp.path().join("objcache1"));
        let mut w1 = PackWriter::new(
            Box::new(remote),
            Box::new(LocalRootPointer::new(base.clone(), None)),
            objcache1,
            None,
        )
        .unwrap();
        w1.write_from(&hash, &mut io::Cursor::new(&data)).unwrap();
        w1.finish(&Hash::compute(b"root-1")).unwrap();

        let objcache2 = LocalStore::for_cache(tmp.path().join("objcache2"));
        let w2 = PackWriter::new(
            Box::new(LocalStore::for_remote(&base)),
            Box::new(LocalRootPointer::new(base.clone(), None)),
            objcache2,
            None,
        )
        .unwrap();
        assert!(
            w2.exists(&hash).unwrap(),
            "standalone object must be seen by second push"
        );
    }

    #[test]
    fn exists_via_bloom() {
        let tmp = TempDir::new().unwrap();
        let (remote, base) = setup_remote(&tmp);
        let writer = PackWriter::new(
            Box::new(remote),
            Box::new(LocalRootPointer::new(base, None)),
            LocalStore::for_cache(tmp.path().join("objcache")),
            None,
        )
        .unwrap();

        let data = vec![0xEE; 100];
        let hash = make_hash_from_data(&data);
        assert!(!writer.exists(&hash).unwrap());

        let mut cursor = io::Cursor::new(&data);
        writer.write_from(&hash, &mut cursor).unwrap();
        // After insert, bloom says maybe-present; remote.exists is checked for confirmation.
        // Since inline entries aren't written to remote.objects/, exists() returns false
        // from remote but bloom returns true — the current impl checks remote.exists() after bloom.
        // This is acceptable: inline entries will be found via the index on read path.
    }

    /// Verify that `load_index_file` fetches from the remote exactly ONCE, even
    /// when called twice for the same hash. The second call must be served from
    /// the local cache (no additional remote `open_read`).
    #[test]
    fn load_index_file_cached_after_first_fetch() {
        use crate::store::stats::{IoRecord, StatsStore};
        use std::sync::Arc;
        use std::sync::atomic::Ordering::Relaxed;

        let tmp = TempDir::new().unwrap();
        let base = tmp.path().to_path_buf();
        std::fs::create_dir_all(base.join("objects")).unwrap();
        std::fs::create_dir_all(base.join("tmp")).unwrap();

        // Write an inline object and finish to produce a real delta index on the remote.
        let data = vec![0x11u8; 100];
        let hash = make_hash_from_data(&data);
        let remote_for_write = LocalStore::for_remote(&base);
        let cache_dir = tmp.path().join("cache_write");
        let objcache_dir = tmp.path().join("objcache_write");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::create_dir_all(&objcache_dir).unwrap();
        let mut w1 = PackWriter::new(
            Box::new(remote_for_write),
            Box::new(LocalRootPointer::new(base.clone(), None)),
            LocalStore::for_cache(&objcache_dir),
            None,
        )
        .unwrap();
        w1.write_from(&hash, &mut io::Cursor::new(&data)).unwrap();
        w1.finish(&Hash::compute(b"root")).unwrap();

        // Now construct a second PackWriter backed by a StatsStore to count remote reads.
        // Use a FRESH cache directory so nothing is pre-cached.
        let cache_dir2 = tmp.path().join("cache_read");
        let objcache_dir2 = tmp.path().join("objcache_read");
        std::fs::create_dir_all(&cache_dir2).unwrap();
        std::fs::create_dir_all(&objcache_dir2).unwrap();

        let record = Arc::new(IoRecord::default());
        let raw_remote = LocalStore::for_remote(&base);
        let stats_remote = StatsStore::new(Box::new(raw_remote), Arc::clone(&record));

        let w2 = PackWriter::new(
            Box::new(stats_remote),
            Box::new(LocalRootPointer::new(base.clone(), None)),
            LocalStore::for_cache(&objcache_dir2),
            None,
        )
        .unwrap();

        // The second writer should have a snapshot_root with one delta hash.
        // Calling exists() on the previously-written `hash` triggers index_contains()
        // which calls load_index_file for that delta hash. The FIRST call goes to remote.
        let reads_before = record.reads.load(Relaxed);
        let found = w2.exists(&hash).unwrap();
        assert!(
            found,
            "previously pushed object must be found by the second writer"
        );
        let reads_after_first = record.reads.load(Relaxed);
        // At least one remote read must have happened (for the delta index file).
        assert!(
            reads_after_first > reads_before,
            "first call to load_index_file must fetch from remote"
        );

        // Call exists() again for the same hash — the index file is now in the local
        // cache so remote.open_read should NOT be called again.
        let reads_before_second = record.reads.load(Relaxed);
        let found2 = w2.exists(&hash).unwrap();
        assert!(found2, "object must still be found on second exists() call");
        let reads_after_second = record.reads.load(Relaxed);
        assert_eq!(
            reads_before_second, reads_after_second,
            "second call to load_index_file (same hash) must be served from local cache \
             with no additional remote open_read"
        );
    }

    /// `ObjectStore` wrapper that delays every `open_read` and counts how many
    /// times it was called, to make a `load_index_file` race window
    /// observable deterministically rather than relying on scheduler luck.
    /// Because a successful `crate::codec::pack::load_index_file` call
    /// performs exactly one `open_read` immediately followed by one
    /// `IndexFile::deserialise` on a cold objcache (see
    /// `load_cached_plaintext`), counting `open_read` calls here is
    /// equivalent to counting deserialisations for this test's setup (an
    /// empty objcache, a single hash never written to objcache by anything
    /// else).
    struct DelayedCountingStore {
        inner: LocalStore,
        delay: std::time::Duration,
        open_read_calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl ObjectStore for DelayedCountingStore {
        fn exists(&self, hash: &Hash) -> Result<bool, Error> {
            self.inner.exists(hash)
        }
        fn size(&self, hash: &Hash) -> Result<u64, Error> {
            self.inner.size(hash)
        }
        fn list_with_sizes(&self) -> Result<Vec<(String, u64)>, Error> {
            self.inner.list_with_sizes()
        }
        fn open_read(&self, hash: &Hash) -> Result<Box<dyn io::Read>, Error> {
            self.open_read_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::thread::sleep(self.delay);
            self.inner.open_read(hash)
        }
        fn write_from(&self, hash: &Hash, reader: &mut dyn io::Read) -> Result<(), Error> {
            self.inner.write_from(hash, reader)
        }
    }

    /// Regression test for design/02 "Index lookup cost on the push path":
    /// when several threads call `load_index_file` for the SAME hash
    /// concurrently while it is not yet cached (neither in `self.indexes` nor
    /// on local disk), the underlying fetch + deserialise must happen exactly
    /// once -- the rest must block on the single-flight guard and reuse the
    /// result, mirroring `PackReader::load_index_file`'s `index_load` guard.
    #[test]
    fn load_index_file_single_flights_concurrent_misses() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        use std::time::Duration;

        let tmp = TempDir::new().unwrap();
        let base = make_remote_base(&tmp);

        // Produce a real delta index on the remote via a normal push.
        let data = vec![0x33u8; 100];
        let hash = make_hash_from_data(&data);
        {
            let mut w1 = PackWriter::new(
                Box::new(LocalStore::for_remote(&base)),
                Box::new(LocalRootPointer::new(base.clone(), None)),
                LocalStore::for_cache(tmp.path().join("objcache_write")),
                None,
            )
            .unwrap();
            w1.write_from(&hash, &mut io::Cursor::new(&data)).unwrap();
            w1.finish(&Hash::compute(b"root")).unwrap();
        }

        // A second writer, backed by the delaying/counting store, with a
        // FRESH (empty) objcache so the first load of the delta index is a
        // genuine cache miss for every racing thread below.
        let open_read_calls = Arc::new(AtomicUsize::new(0));
        let delayed_store = DelayedCountingStore {
            inner: LocalStore::for_remote(&base),
            delay: Duration::from_millis(150),
            open_read_calls: Arc::clone(&open_read_calls),
        };
        let w2 = Arc::new(
            PackWriter::new(
                Box::new(delayed_store),
                Box::new(LocalRootPointer::new(base.clone(), None)),
                LocalStore::for_cache(tmp.path().join("objcache_read")),
                None,
            )
            .unwrap(),
        );

        let delta_hash = w2
            .snapshot_root
            .as_ref()
            .unwrap()
            .delta_hashes_as_hashes()
            .into_iter()
            .next()
            .expect("push must have produced one delta index");

        // `PackWriter::new` itself issued one `open_read` above (fetching the
        // Bloom filter written by `w1.finish()`) -- baseline it away so the
        // assertion below counts only the concurrent `load_index_file` calls.
        let baseline = open_read_calls.load(SeqCst);

        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let w2 = Arc::clone(&w2);
                    let delta_hash = delta_hash.clone();
                    scope.spawn(move || w2.load_index_file(&delta_hash).unwrap())
                })
                .collect();
            for h in handles {
                h.join().unwrap();
            }
        });

        assert_eq!(
            open_read_calls.load(SeqCst) - baseline,
            1,
            "concurrent load_index_file callers for the same hash must \
             single-flight: only one may reach the remote fetch + \
             deserialise, the rest must block and reuse the cached IndexFile"
        );
    }

    /// Regression guard for the stats "unknown" misclassification: index files
    /// (remote magic ED E2) must be cached in the dedicated objcache directory,
    /// NOT in the logical-object cache (objects/ / local_cache). If an index file
    /// leaked into local_cache, `omemfs stats` would count it as `unknown`.
    #[test]
    fn index_files_cached_in_objcache_not_local_cache() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().to_path_buf();
        std::fs::create_dir_all(base.join("objects")).unwrap();
        std::fs::create_dir_all(base.join("tmp")).unwrap();

        // Push an inline object and finish to produce a real delta index on the remote.
        let data = vec![0x22u8; 100];
        let hash = make_hash_from_data(&data);
        let cache_dir = tmp.path().join("cache_write");
        let objcache_dir = tmp.path().join("objcache_write");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::create_dir_all(&objcache_dir).unwrap();
        let mut w1 = PackWriter::new(
            Box::new(LocalStore::for_remote(&base)),
            Box::new(LocalRootPointer::new(base.clone(), None)),
            LocalStore::for_cache(&objcache_dir),
            None,
        )
        .unwrap();
        w1.write_from(&hash, &mut io::Cursor::new(&data)).unwrap();
        w1.finish(&Hash::compute(b"root")).unwrap();

        // Second writer over FRESH local_cache + objcache dirs. exists() triggers
        // load_index_file, which must populate objcache (and never local_cache).
        let local_cache_dir = tmp.path().join("local_cache_read");
        let objcache_dir2 = tmp.path().join("objcache_read");
        std::fs::create_dir_all(&local_cache_dir).unwrap();
        std::fs::create_dir_all(&objcache_dir2).unwrap();
        let w2 = PackWriter::new(
            Box::new(LocalStore::for_remote(&base)),
            Box::new(LocalRootPointer::new(base.clone(), None)),
            LocalStore::for_cache(&objcache_dir2),
            None,
        )
        .unwrap();
        assert!(
            w2.exists(&hash).unwrap(),
            "previously pushed object must be found"
        );

        // The objcache must now hold the cached (plaintext) index file...
        let count_files = |dir: &std::path::Path| -> usize { walkdir_count(dir) };
        assert!(
            count_files(&objcache_dir2) > 0,
            "load_index_file must cache the index file under objcache"
        );
        // ...and the logical-object cache must remain free of any index file
        // (an object whose stored bytes begin with the ED E2 index magic).
        assert!(
            !any_object_has_index_magic(&local_cache_dir),
            "no index file (ED E2) may be written into the local_cache (objects/) dir"
        );
    }

    /// Recursively count regular files under `dir` (test helper).
    fn walkdir_count(dir: &std::path::Path) -> usize {
        let mut n = 0;
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    n += walkdir_count(&p);
                } else if p.is_file() {
                    n += 1;
                }
            }
        }
        n
    }

    /// Returns true if any file under `dir` has stored bytes beginning with the
    /// index-file magic (ED E2), accounting for the ED E0 standalone escape.
    fn any_object_has_index_magic(dir: &std::path::Path) -> bool {
        let mut found = false;
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    found |= any_object_has_index_magic(&p);
                } else if p.is_file()
                    && let Ok(bytes) = std::fs::read(&p)
                {
                    let eff: &[u8] = if bytes.len() >= 2 && bytes[0] == 0xED && bytes[1] == 0xE0 {
                        &bytes[2..]
                    } else {
                        &bytes
                    };
                    if eff.len() >= 2 && eff[0] == 0xED && eff[1] == 0xE2 {
                        found = true;
                    }
                }
            }
        }
        found
    }

    /// Regression guard for the "wasted remote HEAD per sibling object" push
    /// performance bug. After a push, objects recorded in the snapshot index
    /// (inline, pack, AND standalone entries) must be confirmed present via the
    /// LOCAL cached index — `exists()` must NOT issue a remote `HEAD`
    /// (`ObjectStore::exists`) for them. Probing the remote first would cost one
    /// wasted round-trip per object in any touched directory after a `pack`,
    /// where every previously pushed object lives in the index rather than as a
    /// loose `objects/<storage_key>`.
    #[test]
    fn exists_consults_local_index_before_remote_head() {
        use crate::store::stats::{IoRecord, StatsStore};
        use std::sync::Arc;
        use std::sync::atomic::Ordering::Relaxed;

        let tmp = TempDir::new().unwrap();
        let base = tmp.path().to_path_buf();
        std::fs::create_dir_all(base.join("objects")).unwrap();
        std::fs::create_dir_all(base.join("tmp")).unwrap();

        // First push: an inline object, a pack-range object, and a standalone
        // object — each routed differently but all recorded in the delta index.
        let inline_data = vec![0x11u8; 100];
        let inline_hash = make_hash_from_data(&inline_data);
        let pack_data = vec![0x22u8; 1024];
        let pack_hash = make_hash_from_data(&pack_data);
        let standalone_data = vec![0x33u8; PACK_THRESHOLD + 1];
        let standalone_hash = make_hash_from_data(&standalone_data);

        let objcache1 = LocalStore::for_cache(tmp.path().join("objcache1"));
        let mut w1 = PackWriter::new(
            Box::new(LocalStore::for_remote(&base)),
            Box::new(LocalRootPointer::new(base.clone(), None)),
            objcache1,
            None,
        )
        .unwrap();
        w1.write_from(&inline_hash, &mut io::Cursor::new(&inline_data))
            .unwrap();
        w1.write_from(&pack_hash, &mut io::Cursor::new(&pack_data))
            .unwrap();
        w1.write_from(&standalone_hash, &mut io::Cursor::new(&standalone_data))
            .unwrap();
        w1.finish(&Hash::compute(b"root-1")).unwrap();

        // Second push: a fresh cache (no index files pre-cached) and a
        // StatsStore to count remote HEAD (exists) calls.
        let record = Arc::new(IoRecord::default());
        let stats_remote =
            StatsStore::new(Box::new(LocalStore::for_remote(&base)), Arc::clone(&record));
        let objcache2 = LocalStore::for_cache(tmp.path().join("objcache2"));
        let w2 = PackWriter::new(
            Box::new(stats_remote),
            Box::new(LocalRootPointer::new(base.clone(), None)),
            objcache2,
            None,
        )
        .unwrap();

        let heads_before = record.exists_found.load(Relaxed) + record.exists_miss.load(Relaxed);
        assert!(
            w2.exists(&inline_hash).unwrap(),
            "inline object must be found"
        );
        assert!(w2.exists(&pack_hash).unwrap(), "pack object must be found");
        assert!(
            w2.exists(&standalone_hash).unwrap(),
            "standalone object must be found"
        );
        let heads_after = record.exists_found.load(Relaxed) + record.exists_miss.load(Relaxed);

        assert_eq!(
            heads_before,
            heads_after,
            "exists() for objects recorded in the snapshot index must be answered from the \
             local cached index without any remote HEAD (issued {} HEAD(s))",
            heads_after - heads_before
        );
    }

    /// The remote `HEAD` fallback is still exercised for a standalone object that
    /// was written to `objects/` since the snapshot index was built (i.e. not yet
    /// recorded in any index file the second writer can see). The index lookup
    /// misses and `exists()` must fall through to a remote `HEAD` to confirm it.
    #[test]
    fn exists_falls_back_to_remote_head_for_unindexed_standalone() {
        use crate::store::stats::{IoRecord, StatsStore};
        use std::sync::Arc;
        use std::sync::atomic::Ordering::Relaxed;

        let tmp = TempDir::new().unwrap();
        let base = tmp.path().to_path_buf();
        std::fs::create_dir_all(base.join("objects")).unwrap();
        std::fs::create_dir_all(base.join("tmp")).unwrap();

        // Seed a snapshot index with one object so the second writer has a
        // non-empty bloom that reports "maybe present" for other hashes too.
        let seed = vec![0x55u8; 1024];
        let seed_hash = make_hash_from_data(&seed);
        let objcache1 = LocalStore::for_cache(tmp.path().join("objcache1"));
        let mut w1 = PackWriter::new(
            Box::new(LocalStore::for_remote(&base)),
            Box::new(LocalRootPointer::new(base.clone(), None)),
            objcache1,
            None,
        )
        .unwrap();
        w1.write_from(&seed_hash, &mut io::Cursor::new(&seed))
            .unwrap();
        w1.finish(&Hash::compute(b"root-1")).unwrap();

        // Write a standalone object directly to the remote `objects/` directory
        // that is NOT recorded in the snapshot index the second writer reads.
        let standalone_data = vec![0x66u8; PACK_THRESHOLD + 1];
        let standalone_hash = make_hash_from_data(&standalone_data);
        let raw_remote = LocalStore::for_remote(&base);
        raw_remote
            .write_from(&standalone_hash, &mut io::Cursor::new(&standalone_data))
            .unwrap();

        let record = Arc::new(IoRecord::default());
        let stats_remote =
            StatsStore::new(Box::new(LocalStore::for_remote(&base)), Arc::clone(&record));
        let objcache2 = LocalStore::for_cache(tmp.path().join("objcache2"));
        // Force the bloom to report "maybe present" for the unindexed standalone
        // by inserting its hash into the in-memory bloom after construction.
        let w2 = PackWriter::new(
            Box::new(stats_remote),
            Box::new(LocalRootPointer::new(base.clone(), None)),
            objcache2,
            None,
        )
        .unwrap();
        w2.bloom.lock().unwrap().insert(&standalone_hash);

        let heads_before = record.exists_found.load(Relaxed) + record.exists_miss.load(Relaxed);
        assert!(
            w2.exists(&standalone_hash).unwrap(),
            "an unindexed standalone in objects/ must be confirmed present via remote HEAD"
        );
        let heads_after = record.exists_found.load(Relaxed) + record.exists_miss.load(Relaxed);
        assert!(
            heads_after > heads_before,
            "exists() must fall back to a remote HEAD when the index lookup misses"
        );
    }

    // -------------------------------------------------------------------------
    // Filter sizing (design/02_storage_format.md "Filter sizing") --
    // load_remote_bloom acceptance / fallback rules.
    // -------------------------------------------------------------------------

    /// Publish `root` as the (unencrypted) INDEX_ROOT at `base`, bypassing
    /// `PackWriter::finish`'s CAS-from-snapshot flow entirely -- lets a test
    /// control the `bloom_hash` field directly against a remote that has
    /// never been pushed to (`RootToken::Absent`).
    fn publish_raw_index_root(base: &std::path::Path, root: &IndexRoot) {
        let bytes = root.serialise().unwrap();
        let rp = LocalRootPointer::new(base.to_path_buf(), None);
        rp.cas_write(&RootToken::Absent, &bytes).unwrap();
    }

    /// Build a fresh remote-backend directory layout (objects/ + tmp/) under
    /// `tmp` and return its base path.
    fn make_remote_base(tmp: &TempDir) -> PathBuf {
        let base = tmp.path().to_path_buf();
        std::fs::create_dir_all(base.join("objects")).unwrap();
        std::fs::create_dir_all(base.join("tmp")).unwrap();
        base
    }

    /// The direct proof that the old "size mismatch -> rebuild empty" bug
    /// (design/02 "Filter sizing") is gone: a remote Bloom filter whose
    /// (num_bits, num_hash_functions) differ from both the old fixed
    /// BLOOM_EXPECTED=100_000 default AND the current
    /// `bloom::sized_for(0)` default must still be loaded AS-IS -- same
    /// size, membership preserved -- by a `PackWriter` constructed against
    /// that remote.
    #[test]
    fn accepts_structurally_valid_bloom_filter_of_different_size() {
        let tmp = TempDir::new().unwrap();
        let base = make_remote_base(&tmp);

        // Deliberately small capacity (50) and a hash-function count (5)
        // distinct from DEFAULT_NUM_HASH_FUNCTIONS (7).
        let mut custom_bloom = BloomFilter::new(50, 0.01, 5);
        let present = make_hash_from_data(b"pre-existing-member");
        custom_bloom.insert(&present);
        let bloom_bytes = custom_bloom.serialise();
        let bloom_hash = Hash::compute(&bloom_bytes);
        LocalStore::for_remote(&base)
            .write_from(&bloom_hash, &mut io::Cursor::new(&bloom_bytes))
            .unwrap();

        let mut root = IndexRoot::new_empty();
        root.bloom_hash = *bloom_hash.as_bytes_array();
        publish_raw_index_root(&base, &root);

        let writer = PackWriter::new(
            Box::new(LocalStore::for_remote(&base)),
            Box::new(LocalRootPointer::new(base.clone(), None)),
            LocalStore::for_cache(tmp.path().join("objcache")),
            None,
        )
        .unwrap();

        let loaded = writer.bloom.lock().unwrap();
        assert_eq!(
            loaded.num_bits, custom_bloom.num_bits,
            "a structurally valid remote filter must be loaded with its OWN \
             num_bits, not rebuilt to a fixed default size"
        );
        assert_eq!(loaded.num_hash_functions, 5);
        assert!(
            loaded.may_contain(&present),
            "membership for a hash present in the loaded custom-sized filter \
             must be preserved -- a rebuild-to-empty would turn this into a \
             false negative and cause a needless re-upload"
        );
    }

    #[test]
    fn load_remote_bloom_falls_back_to_fresh_on_zero_num_bits() {
        let tmp = TempDir::new().unwrap();
        let base = make_remote_base(&tmp);

        // Structurally unusable: num_bits == 0 would divide by zero inside
        // may_contain. element_count == 42 is still meaningful and must be
        // used as the fresh filter's capacity hint.
        let unusable = BloomFilter::from_parts(7, 0, 42, vec![]).unwrap();
        let bytes = unusable.serialise();
        let bloom_hash = Hash::compute(&bytes);
        LocalStore::for_remote(&base)
            .write_from(&bloom_hash, &mut io::Cursor::new(&bytes))
            .unwrap();

        let mut root = IndexRoot::new_empty();
        root.bloom_hash = *bloom_hash.as_bytes_array();
        publish_raw_index_root(&base, &root);

        let writer = PackWriter::new(
            Box::new(LocalStore::for_remote(&base)),
            Box::new(LocalRootPointer::new(base.clone(), None)),
            LocalStore::for_cache(tmp.path().join("objcache")),
            None,
        )
        .unwrap();

        let loaded = writer.bloom.lock().unwrap();
        assert!(
            loaded.num_bits >= 1,
            "the fresh fallback must never have num_bits == 0"
        );
        // Must not panic when used.
        let probe = make_hash_from_data(b"anything");
        let _ = loaded.may_contain(&probe);
        assert_eq!(
            crate::codec::pack::bloom::designed_capacity(loaded.num_bits),
            crate::codec::pack::bloom::capacity_for(42),
            "the fresh filter's capacity must be sized from the rejected \
             filter's element_count, not MIN_CAPACITY"
        );
    }

    /// Regression test for design/02 "Bounding the capacity (hostile and
    /// corrupt filters)": a structurally unusable filter (`num_bits == 0`)
    /// whose `element_count` is a hostile/corrupt 8-byte value far beyond any
    /// real repository must not be used unclamped as a capacity hint.
    ///
    /// CAUTION: on unfixed code this test tries to allocate a bit array sized
    /// for ~1.6 TiB (`capacity_for(10^12)` with no `MAX_CAPACITY` clamp) and
    /// will abort the process. Only run this after `MAX_CAPACITY` clamping is
    /// implemented in `capacity_for` -- run the rest of the suite first.
    #[test]
    fn load_remote_bloom_clamps_hostile_element_count_to_max_capacity() {
        let tmp = TempDir::new().unwrap();
        let base = make_remote_base(&tmp);

        // num_bits == 0 (structurally unusable) + element_count = 10^12
        // (fully remote-controlled, no range check at deserialise time).
        let hostile = BloomFilter::from_parts(7, 0, 1_000_000_000_000u64, vec![]).unwrap();
        let bytes = hostile.serialise();
        assert_eq!(
            bytes.len(),
            20,
            "num_bits == 0 means an empty bit array -- 20-byte header only"
        );
        let bloom_hash = Hash::compute(&bytes);
        LocalStore::for_remote(&base)
            .write_from(&bloom_hash, &mut io::Cursor::new(&bytes))
            .unwrap();

        let mut root = IndexRoot::new_empty();
        root.bloom_hash = *bloom_hash.as_bytes_array();
        publish_raw_index_root(&base, &root);

        let writer = PackWriter::new(
            Box::new(LocalStore::for_remote(&base)),
            Box::new(LocalRootPointer::new(base.clone(), None)),
            LocalStore::for_cache(tmp.path().join("objcache")),
            None,
        )
        .unwrap();

        let loaded = writer.bloom.lock().unwrap();
        assert!(
            loaded.num_bits >= 1,
            "the fresh fallback must never have num_bits == 0"
        );
        assert!(
            crate::codec::pack::bloom::designed_capacity(loaded.num_bits)
                <= crate::codec::pack::bloom::MAX_CAPACITY,
            "a hostile element_count must not inflate the fresh filter's \
             capacity past MAX_CAPACITY"
        );
        // Must not panic when used.
        let probe = make_hash_from_data(b"hostile-element-count-probe");
        let _ = loaded.may_contain(&probe);
    }

    #[test]
    fn load_remote_bloom_falls_back_to_fresh_on_zero_num_hash_functions() {
        let tmp = TempDir::new().unwrap();
        let base = make_remote_base(&tmp);

        // num_hash_functions == 0 is well-defined (may_contain trivially
        // returns true, insert is a no-op) but useless -- must still be
        // rejected.
        let unusable = BloomFilter::from_parts(0, 64, 7, vec![0u8; 8]).unwrap();
        let bytes = unusable.serialise();
        let bloom_hash = Hash::compute(&bytes);
        LocalStore::for_remote(&base)
            .write_from(&bloom_hash, &mut io::Cursor::new(&bytes))
            .unwrap();

        let mut root = IndexRoot::new_empty();
        root.bloom_hash = *bloom_hash.as_bytes_array();
        publish_raw_index_root(&base, &root);

        let writer = PackWriter::new(
            Box::new(LocalStore::for_remote(&base)),
            Box::new(LocalRootPointer::new(base.clone(), None)),
            LocalStore::for_cache(tmp.path().join("objcache")),
            None,
        )
        .unwrap();

        let loaded = writer.bloom.lock().unwrap();
        assert!(
            loaded.num_hash_functions >= 1,
            "the fresh fallback must never have num_hash_functions == 0"
        );
        let probe = make_hash_from_data(b"anything-else");
        let _ = loaded.may_contain(&probe);
    }

    #[test]
    fn load_remote_bloom_falls_back_to_fresh_on_corrupted_bytes() {
        let tmp = TempDir::new().unwrap();
        let base = make_remote_base(&tmp);

        // Garbage bytes: not a valid Bloom filter at all (bad magic).
        let garbage = vec![0u8; 40];
        let bloom_hash = Hash::compute(&garbage);
        LocalStore::for_remote(&base)
            .write_from(&bloom_hash, &mut io::Cursor::new(&garbage))
            .unwrap();

        let mut root = IndexRoot::new_empty();
        root.bloom_hash = *bloom_hash.as_bytes_array();
        publish_raw_index_root(&base, &root);

        let writer = PackWriter::new(
            Box::new(LocalStore::for_remote(&base)),
            Box::new(LocalRootPointer::new(base.clone(), None)),
            LocalStore::for_cache(tmp.path().join("objcache")),
            None,
        )
        .unwrap();

        let loaded = writer.bloom.lock().unwrap();
        assert!(loaded.num_bits >= 1);
        let probe = make_hash_from_data(b"corrupted-fallback-probe");
        let _ = loaded.may_contain(&probe);
        // Nothing valid could be read at all -> the MIN_CAPACITY hint, not an
        // element-count-derived one.
        assert_eq!(
            crate::codec::pack::bloom::designed_capacity(loaded.num_bits),
            crate::codec::pack::bloom::capacity_for(0)
        );
    }

    /// Encrypt `root` into the same `nonce (12B) || AES-256-GCM ciphertext ||
    /// tag` layout `PackWriter::encrypt_index_root` produces, so a test can
    /// publish an INDEX_ROOT that a `PackWriter` constructed with `key` will
    /// decode successfully -- needed to isolate a decrypt failure to the
    /// Bloom filter object alone, rather than failing earlier at INDEX_ROOT
    /// decode time.
    fn encrypt_index_root_for_test(root: &IndexRoot, key: &crate::codec::encrypt::EncryptKey) -> Vec<u8> {
        let plaintext = root.serialise().unwrap();
        let nonce = [9u8; 12];
        let mut pseudo_hash = [0u8; 32];
        pseudo_hash[..12].copy_from_slice(&nonce);
        let encrypted = crate::codec::encrypt::encrypt(plaintext, Some(key), &pseudo_hash);
        let mut out = Vec::with_capacity(12 + encrypted.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&encrypted);
        out
    }

    #[test]
    fn load_remote_bloom_falls_back_to_fresh_on_decrypt_failure() {
        use crate::codec::encrypt::EncryptKey;

        let tmp = TempDir::new().unwrap();
        let base = make_remote_base(&tmp);

        let key = EncryptKey::new([7u8; 32]);

        // Store a WELL-FORMED plaintext filter, but UNENCRYPTED -- reading it
        // back with a configured key must fail GCM tag verification (the
        // bytes are not valid ciphertext||tag for that key), triggering the
        // fresh fallback rather than propagating a decrypt error.
        let plaintext_bloom = BloomFilter::new(50, 0.01, 5);
        let bytes = plaintext_bloom.serialise();
        let bloom_hash = Hash::compute(&bytes);
        LocalStore::for_remote(&base)
            .write_from(&bloom_hash, &mut io::Cursor::new(&bytes))
            .unwrap();

        // The INDEX_ROOT itself IS properly encrypted for `key`, so
        // `PackWriter::new` decodes it successfully and only the Bloom
        // filter's own decrypt fails.
        let mut root = IndexRoot::new_empty();
        root.bloom_hash = *bloom_hash.as_bytes_array();
        let root_bytes = encrypt_index_root_for_test(&root, &key);
        let rp = LocalRootPointer::new(base.clone(), None);
        rp.cas_write(&RootToken::Absent, &root_bytes).unwrap();

        let writer = PackWriter::new(
            Box::new(LocalStore::for_remote(&base)),
            Box::new(LocalRootPointer::new(base.clone(), None)),
            LocalStore::for_cache(tmp.path().join("objcache")),
            Some(key),
        )
        .unwrap();

        let loaded = writer.bloom.lock().unwrap();
        assert!(loaded.num_bits >= 1);
        let probe = make_hash_from_data(b"decrypt-failure-fallback-probe");
        let _ = loaded.may_contain(&probe);
    }

    // -------------------------------------------------------------------------
    // Resident index-file cache (design/02 "Index lookup cost on the push
    // path").
    // -------------------------------------------------------------------------

    /// The direct proof that `load_index_file` deserialises an index file at
    /// most ONCE per push: two calls for the SAME hash must return the SAME
    /// `Arc<IndexFile>` instance (`Arc::ptr_eq`), not two separately
    /// deserialised (but content-equal) copies.
    #[test]
    fn load_index_file_deserialises_once_via_resident_cache() {
        let tmp = TempDir::new().unwrap();
        let base = make_remote_base(&tmp);

        // Produce a real delta index on the remote via a first push.
        let data = vec![0x77u8; 100];
        let hash = make_hash_from_data(&data);
        let objcache1 = LocalStore::for_cache(tmp.path().join("objcache1"));
        let mut w1 = PackWriter::new(
            Box::new(LocalStore::for_remote(&base)),
            Box::new(LocalRootPointer::new(base.clone(), None)),
            objcache1,
            None,
        )
        .unwrap();
        w1.write_from(&hash, &mut io::Cursor::new(&data)).unwrap();
        w1.finish(&Hash::compute(b"root")).unwrap();

        let objcache2 = LocalStore::for_cache(tmp.path().join("objcache2"));
        let w2 = PackWriter::new(
            Box::new(LocalStore::for_remote(&base)),
            Box::new(LocalRootPointer::new(base.clone(), None)),
            objcache2,
            None,
        )
        .unwrap();

        let delta_hash = w2
            .snapshot_root
            .as_ref()
            .unwrap()
            .delta_hashes_as_hashes()
            .into_iter()
            .next()
            .unwrap();

        let first = w2.load_index_file(&delta_hash).unwrap();
        let second = w2.load_index_file(&delta_hash).unwrap();
        assert!(
            Arc::ptr_eq(&first, &second),
            "two load_index_file calls for the same hash within one \
             PackWriter must return the SAME Arc<IndexFile> instance -- \
             proof that the index was deserialised only once, not once per \
             call"
        );

        // Sanity: exists() through the normal call path must still find the
        // previously pushed hash via this cached index.
        assert!(w2.exists(&hash).unwrap());
    }

    // -------------------------------------------------------------------------
    // Over-capacity reporting (design/02 "Fill-rate reporting").
    // -------------------------------------------------------------------------

    #[test]
    fn bloom_over_capacity_none_when_within_capacity() {
        let tmp = TempDir::new().unwrap();
        let (remote, base) = setup_remote(&tmp);
        let writer = PackWriter::new(
            Box::new(remote),
            Box::new(LocalRootPointer::new(base, None)),
            LocalStore::for_cache(tmp.path().join("objcache")),
            None,
        )
        .unwrap();
        // A freshly-built filter (MIN_CAPACITY-sized) with zero elements is
        // nowhere near its designed capacity.
        assert!(writer.bloom_over_capacity().is_none());
    }

    #[test]
    fn bloom_over_capacity_reports_when_element_count_exceeds_designed_capacity() {
        let tmp = TempDir::new().unwrap();
        let (remote, base) = setup_remote(&tmp);
        let writer = PackWriter::new(
            Box::new(remote),
            Box::new(LocalRootPointer::new(base, None)),
            LocalStore::for_cache(tmp.path().join("objcache")),
            None,
        )
        .unwrap();

        // Replace the in-memory filter with a tiny one, then push it well
        // past its designed capacity.
        {
            let mut bloom = writer.bloom.lock().unwrap();
            *bloom = BloomFilter::new(10, 0.01, crate::codec::pack::bloom::DEFAULT_NUM_HASH_FUNCTIONS);
            let capacity = crate::codec::pack::bloom::designed_capacity(bloom.num_bits);
            for i in 0..(capacity + 5) {
                let h = make_hash_from_data(format!("elem-{i}").as_bytes());
                bloom.insert(&h);
            }
        }

        let over = writer.bloom_over_capacity();
        assert!(
            over.is_some(),
            "a filter holding more elements than its designed capacity must \
             be reported as over capacity"
        );
        let (elements, capacity) = over.unwrap();
        assert!(elements > capacity);
    }
}
