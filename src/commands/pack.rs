/// `omemfs pack` — compaction and maintenance of the remote pack layer.
///
/// Operations performed in order:
///   1. Merge delta indexes into the hot index.
///   2. Move unreachable entries from hot to cold shards.
///   3. Consolidate small pack files referenced by the hot index.
///   4. Split cold shards that exceed 4 MiB.
///   5. Regenerate the Bloom filter from all entries.
///   6. CAS-update INDEX_ROOT.
///
/// Unreferenced objects produced by consolidation, cold-shard splitting, and
/// Bloom-filter regeneration are not deleted here. Storage is reclaimed via the
/// backup-reclone cycle (see design/04_cli_spec.md).
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use crate::codec::encrypt::EncryptKey;
use crate::codec::pack::bloom::{BloomFilter, DEFAULT_NUM_HASH_FUNCTIONS};
use crate::codec::pack::index::{IndexEntry, IndexFile, PackEntry};
use crate::codec::pack::index_root::IndexRoot;
use crate::codec::pack::root_pointer::RootToken;
use crate::dtimer_l1;
use crate::error::Error;
use crate::io_stats::{self, PackDetail};
use crate::object::{Hash, TreeEntry};
use crate::repo::Repo;
use crate::store::ObjectStore;
use crate::store::local::LocalStore;
use crate::store::stats::{IoRecord, StatsStore};

pub struct PackOptions {
    pub work_dir: PathBuf,
}

pub fn run(opts: PackOptions) -> Result<(), Error> {
    let started = std::time::Instant::now();
    let repo = Repo::open(&opts.work_dir)?;
    crate::progress::notify_repo_dir(&repo.work_dir);
    // Hold the repo lock for the whole run: pack mutates the pack index and must
    // not race with push/pull. See design/12_locking.md.
    let _lock = repo.acquire_lock()?;
    let phase = crate::progress::begin_phase("Pack");
    let _t = dtimer_l1!("pack");
    let remote_name = "origin";
    let raw_remote = repo.remote_store(remote_name)?;
    let remote_key = raw_remote.encrypt_key.clone();

    // Wrap the remote in a StatsStore so every GET/PUT/HEAD/byte through the
    // ObjectStore trait is counted for the io_stats record. The index-root read
    // and CAS-write below go through the backend-pluggable root pointer (NOT
    // through ObjectStore), so those operations are not counted — an accepted
    // minor undercount (see design/04 stats Notes).
    let io_record = Arc::new(IoRecord::default());
    let remote = StatsStore::new(Box::new(raw_remote), Arc::clone(&io_record));

    // Accumulator for pack-layer tuning metrics, written to the io_stats record.
    let mut detail = PackDetail::default();

    // Single backend-pluggable index-root pointer: read at start, CAS-write at
    // the end (step 6), against the token observed here.
    let root_pointer = repo.remote_root_pointer(remote_name)?;
    let (old_raw, old_token): (Option<Vec<u8>>, RootToken) = root_pointer.read()?;
    let mut index_root = match old_raw.as_ref() {
        None => {
            eprintln!("omemfs pack: no index root found; nothing to do.");
            return Ok(());
        }
        Some(raw) => {
            let pt = crate::codec::pack::decrypt_index_root_bytes(raw, remote_key.as_ref())?;
            IndexRoot::deserialise(&pt)?
        }
    };

    // Number of delta indexes that will be merged into the hot index this run.
    // Captured BEFORE merge_deltas_into_hot clears them below.
    detail.deltas_merged = index_root.delta_hashes.len() as u64;

    // Capture the sizes of objects this run will unconditionally supersede
    // (new hot index, new Bloom filter are always written; the merged delta
    // indexes are always cleared from INDEX_ROOT), BEFORE they are read/
    // overwritten below. These sizes are a lower-bound estimate of
    // `detail.orphaned_bytes` (see its doc comment in io_stats.rs) -- used to
    // reconsider the pack cadence later from real data (design/04 "omemfs
    // pack" -> "When to run").
    let old_hot_bytes: u64 = index_root
        .hot_hash_opt()
        .and_then(|h| remote.size(&h).ok())
        .unwrap_or(0);
    let old_delta_bytes: u64 = index_root
        .delta_hashes_as_hashes()
        .iter()
        .filter_map(|h| remote.size(h).ok())
        .sum();
    let old_bloom_bytes: u64 = index_root
        .bloom_hash_opt()
        .and_then(|h| remote.size(&h).ok())
        .unwrap_or(0);

    // -----------------------------------------------------------------------
    // 1. Merge delta indexes into hot index
    // -----------------------------------------------------------------------
    let hot_entries = merge_deltas_into_hot(&remote, remote_key.as_ref(), &mut index_root)?;

    // -----------------------------------------------------------------------
    // 2. Move unreachable entries from hot to cold
    // -----------------------------------------------------------------------
    let local = repo.local_store();
    let (hot_entries, cold_entries_extra) = split_hot_cold(hot_entries, &repo, &local, &io_record)?;

    // -----------------------------------------------------------------------
    // 3. Consolidate small pack files in hot index
    // -----------------------------------------------------------------------
    let hot_entries = consolidate_pack_files(hot_entries, &remote, &mut detail)?;

    // Final hot index entry count after merge + split + consolidate.
    detail.hot_index_entries = hot_entries.len() as u64;
    detail.orphaned_bytes =
        old_hot_bytes + old_delta_bytes + old_bloom_bytes + detail.consolidated_bytes_in;

    // -----------------------------------------------------------------------
    // 4. Write hot index
    // -----------------------------------------------------------------------
    let new_hot_hash = write_index_file(&hot_entries, &remote, remote_key.as_ref())?;
    index_root.hot_hash = *new_hot_hash.as_bytes_array();
    index_root.delta_hashes.clear();

    // -----------------------------------------------------------------------
    // Merge cold_entries_extra into existing cold shards
    // -----------------------------------------------------------------------
    apply_cold_entries(
        cold_entries_extra,
        &mut index_root,
        &remote,
        remote_key.as_ref(),
    )?;

    // -----------------------------------------------------------------------
    // 4. Split oversized cold shards
    // -----------------------------------------------------------------------
    let did_split = split_cold_shards(&mut index_root, &remote, remote_key.as_ref())?;
    detail.cold_splits = if did_split { 1 } else { 0 };

    // -----------------------------------------------------------------------
    // Load every distinct cold shard once. cold_shards is not mutated again
    // after split_cold_shards, so bloom regeneration can share these index
    // files instead of re-reading them from the remote.
    // -----------------------------------------------------------------------
    let cold_shard_files = load_cold_shard_files(&remote, remote_key.as_ref(), &index_root)?;

    // -----------------------------------------------------------------------
    // 5. Regenerate Bloom filter
    // -----------------------------------------------------------------------
    let bloom = regenerate_bloom(&hot_entries, &cold_shard_files);
    detail.bloom_elements = bloom.element_count;
    let bloom_bytes = bloom.serialise();
    let bloom_hash = Hash::compute(&bloom_bytes);
    let bloom_stored = encrypt_object(&bloom_bytes, remote_key.as_ref(), &bloom_hash);
    let mut cursor = io::Cursor::new(&bloom_stored);
    remote.write_from(&bloom_hash, &mut cursor)?;
    index_root.bloom_hash = *bloom_hash.as_bytes_array();

    // -----------------------------------------------------------------------
    // 6. CAS-update INDEX_ROOT
    // -----------------------------------------------------------------------
    let new_index_root_bytes = encrypt_index_root(&index_root, remote_key.as_ref())?;
    // A concurrent push or pack updated the index root between our snapshot read
    // and this CAS write. Remap the shared CasFailed (whose default message tells
    // the user to pull and retry a *push*) to pack-specific guidance via
    // `remap_cas_failure`. See design/04 "omemfs pack" → Errors.
    root_pointer
        .cas_write(&old_token, &new_index_root_bytes)
        .map_err(remap_cas_failure)?;

    eprintln!("omemfs pack: done.");
    phase.complete("done");

    // Record real I/O counts (GET/PUT/HEAD/bytes via the StatsStore wrapper)
    // together with the pack-layer tuning metrics. The index-root CAS file ops
    // are not counted (see comment above).
    let omemfs_dir = repo.work_dir.join(".omemfs");
    let duration_ms = started.elapsed().as_millis() as u64;
    io_stats::append_record_with_detail(
        &omemfs_dir,
        "pack",
        remote_name,
        &io_record,
        duration_ms,
        Some(detail),
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Step 1: merge delta indexes
// ---------------------------------------------------------------------------

fn merge_deltas_into_hot(
    remote: &dyn ObjectStore,
    key: Option<&EncryptKey>,
    index_root: &mut IndexRoot,
) -> Result<Vec<IndexEntry>, Error> {
    let mut merged: HashMap<String, IndexEntry> = HashMap::new();

    // Load hot index first (lowest priority — overwritten by deltas).
    if let Some(hot_hash_bytes) = index_root.hot_hash_opt() {
        let idx = load_index_file(remote, key, &hot_hash_bytes)?;
        for entry in idx.entries() {
            merged.insert(entry.hash().as_str().to_string(), entry.clone());
        }
    }

    // Apply delta indexes newest-first (newest overrides older).
    for delta_hash in index_root.delta_hashes_as_hashes().into_iter().rev() {
        let idx = load_index_file(remote, key, &delta_hash)?;
        for entry in idx.entries() {
            merged.insert(entry.hash().as_str().to_string(), entry.clone());
        }
    }

    Ok(merged.into_values().collect())
}

// ---------------------------------------------------------------------------
// Step 2: split hot vs cold
// ---------------------------------------------------------------------------

fn split_hot_cold(
    entries: Vec<IndexEntry>,
    repo: &Repo,
    local: &LocalStore,
    io_record: &Arc<IoRecord>,
) -> Result<(Vec<IndexEntry>, Vec<IndexEntry>), Error> {
    // Collect the set of object hashes that are "hot" (reachable from this
    // clone's working tree, clone_root, and remote_root, excluding subtrees
    // that are stubbed on this clone).
    let hot_hashes = collect_hot_hashes(repo, local, io_record)?;

    let mut hot: Vec<IndexEntry> = Vec::new();
    let mut cold: Vec<IndexEntry> = Vec::new();

    for entry in entries {
        match &entry {
            // Standalone entries never go to cold (they are always reachable
            // via objects/<hash> directly).
            IndexEntry::Standalone(_) => {
                hot.push(entry);
            }
            IndexEntry::Inline(e) => {
                if hot_hashes.contains(e.hash.as_str()) {
                    hot.push(entry);
                } else {
                    cold.push(entry);
                }
            }
            IndexEntry::Pack(e) => {
                if hot_hashes.contains(e.hash.as_str()) {
                    hot.push(entry);
                } else {
                    cold.push(entry);
                }
            }
        }
    }

    Ok((hot, cold))
}

/// Collect all object hashes reachable from working tree, clone_root, and
/// remote_root on this clone. Stub subtrees are excluded: only the stub
/// record's blob hash itself is included, not any objects under the stub path.
fn collect_hot_hashes(
    repo: &Repo,
    local: &LocalStore,
    io_record: &Arc<IoRecord>,
) -> Result<HashSet<String>, Error> {
    let mut hot: HashSet<String> = HashSet::new();

    // Identify stubbed paths so we can skip their subtrees.
    let stubbed_paths = collect_stubbed_paths(&repo.work_dir);

    // Collect from clone_root.
    if let Some(clone_root) = repo.read_clone_root()? {
        collect_tree_hashes(&clone_root, local, &stubbed_paths, &mut hot)?;
    }

    // Collect from remote_root (read from INDEX_ROOT). Wrap this remote in the
    // SAME StatsStore io_record so the remote_root read for hot/cold
    // classification is counted alongside the rest of the pack run.
    let remote_name = "origin";
    if let (Ok(remote), Ok(root_pointer)) = (
        repo.remote_store(remote_name),
        repo.remote_root_pointer(remote_name),
    ) {
        let remote_key = remote.encrypt_key.clone();
        let stats_remote = StatsStore::new(Box::new(remote), Arc::clone(io_record));
        let pack_reader = crate::codec::pack::reader::PackReader::new(
            Box::new(stats_remote),
            repo.local_store(),
            repo.packcache_store(),
            repo.objcache_store(),
            root_pointer,
            remote_key,
        );
        if let Ok(Some(remote_root)) = pack_reader.read_root() {
            // Remote tree objects may be in the local cache from previous pulls.
            collect_tree_hashes(&remote_root, local, &stubbed_paths, &mut hot)?;
        }
    }

    // Also collect from working tree scan (via local cache written by push).
    // Working tree hashes are already included transitively through clone_root
    // and remote_root in normal operation.

    Ok(hot)
}

/// Walk the tree rooted at `root_hash` and add all reachable object hashes to
/// `out`. Stub paths are skipped (their blob hash is still included, but the
/// subtree under a stub directory is not walked).
fn collect_tree_hashes(
    root_hash: &Hash,
    local: &LocalStore,
    stubbed_paths: &HashSet<String>,
    out: &mut HashSet<String>,
) -> Result<(), Error> {
    collect_tree_hashes_inner(root_hash, local, stubbed_paths, "", out)
}

fn collect_tree_hashes_inner(
    hash: &Hash,
    local: &LocalStore,
    stubbed_paths: &HashSet<String>,
    prefix: &str,
    out: &mut HashSet<String>,
) -> Result<(), Error> {
    out.insert(hash.as_str().to_string());

    // Try to read the tree; if the object is not in local cache, skip it.
    let data = match crate::codec::store_read(local, hash, None) {
        Ok(d) => d,
        Err(_) => return Ok(()), // not in local cache; can't walk
    };

    let entries = match crate::object::Tree::deserialise(&data) {
        Ok(crate::object::Tree::Normal { entries }) => entries,
        Err(_) => return Ok(()), // blob or chunk — no children
    };

    for entry in &entries {
        let rel_path = if prefix.is_empty() {
            entry.name().to_string()
        } else {
            format!("{}/{}", prefix, entry.name())
        };

        match entry {
            TreeEntry::Blob {
                hash: blob_hash, ..
            } => {
                out.insert(blob_hash.as_str().to_string());
                // Also include chunk objects if the blob is chunked.
                if let Ok(raw) = crate::codec::store_read(local, blob_hash, None)
                    && let Some(chunk_hashes) = crate::object::deserialise_manifest(&raw)
                {
                    for ch in &chunk_hashes {
                        out.insert(ch.as_str().to_string());
                    }
                }
            }
            TreeEntry::Tree {
                hash: child_hash, ..
            } => {
                // Skip subtrees that are entirely stubbed on this clone.
                if stubbed_paths.contains(&rel_path) {
                    out.insert(child_hash.as_str().to_string());
                } else {
                    collect_tree_hashes_inner(child_hash, local, stubbed_paths, &rel_path, out)?;
                }
            }
            TreeEntry::Symlink { .. } => {}
        }
    }

    Ok(())
}

/// List the logical relative paths of all stub entries on this clone.
fn collect_stubbed_paths(work_dir: &std::path::Path) -> HashSet<String> {
    let mut paths: HashSet<String> = HashSet::new();
    collect_stub_paths_recursive(work_dir, work_dir, &mut paths);
    paths
}

fn collect_stub_paths_recursive(
    dir: &std::path::Path,
    work_dir: &std::path::Path,
    out: &mut HashSet<String>,
) {
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".omemfs" {
            continue;
        }
        if path.is_dir() {
            collect_stub_paths_recursive(&path, work_dir, out);
        } else if path.is_file()
            && let Some(logical_name) = crate::stub::logical_name(&name)
        {
            // Derive the logical path (without the stub suffix).
            let logical_path = path.with_file_name(logical_name);
            if let Ok(rel) = logical_path.strip_prefix(work_dir) {
                let s = rel.to_string_lossy().replace('\\', "/");
                out.insert(s);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Step 3: consolidate small pack files
// ---------------------------------------------------------------------------

const CONSOLIDATION_THRESHOLD: u64 = 2 * 1024 * 1024; // 2 MiB
// Reuse the canonical pack magic (ED E1) from the writer. ED E0 is the
// standalone-escape prefix, not the pack magic, so it must not be used here.
// PACK_TARGET/PACK_MAX are shared too (refactor-instructions.md D2): this
// consolidation pass must produce packs shaped like the ones PackWriter
// itself writes, so both must flush at the same sizes.
use crate::codec::pack::writer::{PACK_MAGIC, PACK_MAX, PACK_TARGET};

fn consolidate_pack_files(
    entries: Vec<IndexEntry>,
    remote: &dyn ObjectStore,
    detail: &mut PackDetail,
) -> Result<Vec<IndexEntry>, Error> {
    // Collect pack file sizes via the ObjectStore::size trait method.
    let mut pack_sizes: HashMap<String, u64> = HashMap::new();
    for entry in &entries {
        if let IndexEntry::Pack(pe) = entry {
            let ph = pe.pack_hash.as_str().to_string();
            if let std::collections::hash_map::Entry::Vacant(e) = pack_sizes.entry(ph) {
                let size = remote.size(&pe.pack_hash)?;
                e.insert(size);
            }
        }
    }

    // Identify candidate pack files below the consolidation threshold.
    let candidates: HashSet<String> = pack_sizes
        .iter()
        .filter(|(_, size)| **size < CONSOLIDATION_THRESHOLD)
        .map(|(hash, _)| hash.clone())
        .collect();

    // Number of consolidation candidate pack files found.
    detail.packs_before = candidates.len() as u64;

    if candidates.is_empty() {
        return Ok(entries);
    }

    // Read each candidate pack file ONCE into a buffer keyed by pack_hash.
    // Candidates are bounded below 2 MiB so reading them whole is safe.
    // The stored layout is: 2-byte PACK_MAGIC followed by packed encrypted
    // object bytes. We cache the full file bytes (including the magic) so
    // slicing at [2 + offset .. 2 + offset + length] is byte-identical to the
    // old per-slice seek+read approach.
    let mut pack_bufs: HashMap<String, Vec<u8>> = HashMap::new();
    for pack_hash_str in &candidates {
        let pack_hash = Hash::from_hex(pack_hash_str)
            .map_err(|_| Error::InvalidObject(format!("bad pack_hash hex: {}", pack_hash_str)))?;
        let mut r = remote.open_read(&pack_hash)?;
        let mut buf = Vec::new();
        use std::io::Read;
        r.read_to_end(&mut buf).map_err(Error::Io)?;
        pack_bufs.insert(pack_hash_str.clone(), buf);
    }

    // Accumulate data for new pack files.
    let mut new_buf: Vec<u8> = Vec::new();
    let mut new_pack_entries: Vec<(Hash, u32, u32)> = Vec::new(); // (hash, offset, length)
    let mut completed_packs: Vec<(Vec<u8>, Vec<(Hash, u32, u32)>)> = Vec::new();

    let mut updated: HashMap<String, IndexEntry> = HashMap::new();

    for entry in &entries {
        if let IndexEntry::Pack(pe) = entry
            && candidates.contains(pe.pack_hash.as_str())
        {
            // Slice encrypted bytes from the cached full pack file.
            // Layout: buf[0..2] = magic, buf[2 + offset .. 2 + offset + length] = payload.
            let buf = pack_bufs
                .get(pe.pack_hash.as_str())
                .ok_or_else(|| Error::ObjectNotFound(pe.pack_hash.as_str().to_string()))?;
            let start = 2 + pe.offset as usize;
            let end = start + pe.length as usize;
            // Guard against a malformed offset/length slicing out of bounds.
            // The old seek-based read_pack_slice used read_exact, which
            // errored on a short read rather than panicking; preserve that
            // by validating the range before slicing.
            if end > buf.len() {
                return Err(Error::InvalidObject(format!(
                    "pack {} entry {} out of range: {}..{} exceeds {} bytes",
                    pe.pack_hash.as_str(),
                    pe.hash.as_str(),
                    start,
                    end,
                    buf.len()
                )));
            }
            let encrypted = &buf[start..end];

            let offset = new_buf.len() as u32;
            let length = encrypted.len() as u32;
            // Bytes read from candidate packs and re-packed (slice length).
            detail.consolidated_bytes_in += length as u64;
            new_buf.extend_from_slice(encrypted);
            new_pack_entries.push((pe.hash.clone(), offset, length));

            if new_buf.len() >= PACK_TARGET || new_buf.len() >= PACK_MAX {
                completed_packs.push((new_buf.clone(), new_pack_entries.clone()));
                new_buf.clear();
                new_pack_entries.clear();
            }
            continue;
        }
        updated.insert(entry.hash().as_str().to_string(), entry.clone());
    }

    if !new_pack_entries.is_empty() {
        completed_packs.push((new_buf, new_pack_entries));
    }

    // Write new pack files.
    for (buf, pack_entries) in completed_packs {
        let mut pack_bytes: Vec<u8> = Vec::with_capacity(2 + buf.len());
        pack_bytes.extend_from_slice(&PACK_MAGIC);
        pack_bytes.extend_from_slice(&buf);
        let pack_hash = Hash::compute(&pack_bytes);
        // Record the newly-written consolidated pack: count, output bytes, size.
        detail.packs_after += 1;
        detail.consolidated_bytes_out += pack_bytes.len() as u64;
        detail.pack_sizes_after.push(pack_bytes.len() as u64);
        let mut cursor = io::Cursor::new(&pack_bytes);
        remote.write_from(&pack_hash, &mut cursor)?;

        for (hash, offset, length) in pack_entries {
            updated.insert(
                hash.as_str().to_string(),
                IndexEntry::Pack(PackEntry {
                    hash: hash.clone(),
                    pack_hash: pack_hash.clone(),
                    offset,
                    length,
                }),
            );
        }
    }

    Ok(updated.into_values().collect())
}

// ---------------------------------------------------------------------------
// Write index file helper
// ---------------------------------------------------------------------------

fn write_index_file(
    entries: &[IndexEntry],
    remote: &dyn ObjectStore,
    key: Option<&EncryptKey>,
) -> Result<Hash, Error> {
    let mut idx = IndexFile::new();
    for entry in entries {
        idx.push(entry.clone());
    }
    let bytes = idx.serialise()?;
    let hash = Hash::compute(&bytes);
    let stored = encrypt_object(&bytes, key, &hash);
    let mut cursor = io::Cursor::new(&stored);
    remote.write_from(&hash, &mut cursor)?;
    Ok(hash)
}

// ---------------------------------------------------------------------------
// Cold shard helpers
// ---------------------------------------------------------------------------

fn apply_cold_entries(
    cold_entries: Vec<IndexEntry>,
    index_root: &mut IndexRoot,
    remote: &dyn ObjectStore,
    key: Option<&EncryptKey>,
) -> Result<(), Error> {
    if cold_entries.is_empty() {
        // Ensure at least one cold shard slot exists.
        if index_root.cold_shards.is_empty() {
            index_root.cold_shards = vec![[0u8; 32]; 1];
        }
        return Ok(());
    }

    // Load existing shared shard (slot 0) and append.
    let mut shard_entries: Vec<IndexEntry> = Vec::new();
    if !index_root.cold_shards.is_empty() && index_root.cold_shards[0] != [0u8; 32] {
        let shard_hash = Hash::from_bytes(index_root.cold_shards[0]);
        let idx = load_index_file(remote, key, &shard_hash)?;
        shard_entries.extend_from_slice(idx.entries());
    }
    shard_entries.extend(cold_entries);

    let new_shard_hash = write_index_file(&shard_entries, remote, key)?;
    let new_bytes = *new_shard_hash.as_bytes_array();
    // Update all slots pointing to the old shared shard.
    for slot in index_root.cold_shards.iter_mut() {
        *slot = new_bytes;
    }
    Ok(())
}

const COLD_SHARD_SPLIT_THRESHOLD: u64 = 4 * 1024 * 1024; // 4 MiB

/// Split the largest oversized cold shard (at most one split per run). Returns
/// `true` if a split was performed, `false` otherwise.
fn split_cold_shards(
    index_root: &mut IndexRoot,
    remote: &dyn ObjectStore,
    key: Option<&EncryptKey>,
) -> Result<bool, Error> {
    // Find the largest cold shard that exceeds the threshold.
    let mut largest_idx: Option<usize> = None;
    let mut largest_size: u64 = 0;

    // Collect distinct shard hashes to avoid re-checking duplicates.
    let mut seen: HashSet<String> = HashSet::new();
    for (i, shard_bytes) in index_root.cold_shards.iter().enumerate() {
        if *shard_bytes == [0u8; 32] {
            continue;
        }
        let sh = Hash::from_bytes(*shard_bytes);
        let key_str = sh.as_str().to_string();
        if seen.contains(&key_str) {
            continue;
        }
        seen.insert(key_str);

        let size = remote.size(&sh).unwrap_or(0);
        if size > COLD_SHARD_SPLIT_THRESHOLD && size > largest_size {
            largest_size = size;
            largest_idx = Some(i);
        }
    }

    let Some(split_idx) = largest_idx else {
        return Ok(false);
    };

    let shard_hash = Hash::from_bytes(index_root.cold_shards[split_idx]);
    let idx = load_index_file(remote, key, &shard_hash)?;

    // Find the most populous hash prefix among the entries.
    let mut prefix_counts: HashMap<u8, usize> = HashMap::new();
    for entry in idx.entries() {
        let prefix_byte = entry.hash().as_bytes_array()[0];
        *prefix_counts.entry(prefix_byte).or_insert(0) += 1;
    }
    let Some((&best_prefix, _)) = prefix_counts.iter().max_by_key(|(_, c)| *c) else {
        return Ok(false);
    };

    // Split into dedicated shard (best_prefix) and shared shard (remainder).
    let mut dedicated: Vec<IndexEntry> = Vec::new();
    let mut shared: Vec<IndexEntry> = Vec::new();
    for entry in idx.entries() {
        if entry.hash().as_bytes_array()[0] == best_prefix {
            dedicated.push(entry.clone());
        } else {
            shared.push(entry.clone());
        }
    }

    let dedicated_hash = write_index_file(&dedicated, remote, key)?;
    let shared_hash = write_index_file(&shared, remote, key)?;

    let old_shard_bytes = index_root.cold_shards[split_idx];

    // Update INDEX_ROOT.cold_shards:
    // Ensure cold_prefix_bits is at least 8 for byte-granularity addressing.
    if index_root.cold_prefix_bits < 8 {
        let new_bits = 8u8;
        let new_slots = 1usize << new_bits;
        let old_slots = index_root.cold_shard_count();
        let scale = new_slots / old_slots;

        let mut new_shards = vec![[0u8; 32]; new_slots];
        for (i, &old_bytes) in index_root.cold_shards.iter().enumerate() {
            for j in 0..scale {
                new_shards[i * scale + j] = old_bytes;
            }
        }
        index_root.cold_shards = new_shards;
        index_root.cold_prefix_bits = new_bits;
    }

    // Point dedicated prefix to dedicated shard, remainder to shared shard.
    for (i, slot) in index_root.cold_shards.iter_mut().enumerate() {
        if *slot == old_shard_bytes {
            if i == best_prefix as usize {
                *slot = *dedicated_hash.as_bytes_array();
            } else {
                *slot = *shared_hash.as_bytes_array();
            }
        }
    }

    Ok(true)
}

// ---------------------------------------------------------------------------
// Step 5: Bloom filter regeneration
// ---------------------------------------------------------------------------

fn regenerate_bloom(
    hot_entries: &[IndexEntry],
    cold_shard_files: &HashMap<String, IndexFile>,
) -> BloomFilter {
    let mut bf = BloomFilter::new(100_000, 0.01, DEFAULT_NUM_HASH_FUNCTIONS);

    for entry in hot_entries {
        bf.insert(entry.hash());
    }

    // Also insert entries from cold shards (preloaded once by the caller).
    for idx in cold_shard_files.values() {
        for entry in idx.entries() {
            bf.insert(entry.hash());
        }
    }

    bf
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Load every distinct non-empty cold shard index file referenced by
/// `index_root.cold_shards`, keyed by shard hash hex. Many slots may point to
/// the same shard, so duplicates are loaded only once.
fn load_cold_shard_files(
    remote: &dyn ObjectStore,
    key: Option<&EncryptKey>,
    index_root: &IndexRoot,
) -> Result<HashMap<String, IndexFile>, Error> {
    let mut map: HashMap<String, IndexFile> = HashMap::new();
    for shard_bytes in &index_root.cold_shards {
        if *shard_bytes == [0u8; 32] {
            continue;
        }
        let sh = Hash::from_bytes(*shard_bytes);
        let key_str = sh.as_str().to_string();
        if map.contains_key(&key_str) {
            continue;
        }
        let idx = load_index_file(remote, key, &sh)?;
        map.insert(key_str, idx);
    }
    Ok(map)
}

fn load_index_file(
    remote: &dyn ObjectStore,
    key: Option<&EncryptKey>,
    hash: &Hash,
) -> Result<IndexFile, Error> {
    let mut r = remote.open_read(hash)?;
    let mut stored = Vec::new();
    use std::io::Read;
    r.read_to_end(&mut stored).map_err(Error::Io)?;
    let plaintext = crate::codec::encrypt::decrypt(stored, key, hash.as_bytes_array())?;
    IndexFile::deserialise(&plaintext)
}

fn encrypt_object(data: &[u8], key: Option<&EncryptKey>, hash: &Hash) -> Vec<u8> {
    crate::codec::encrypt::encrypt(data.to_vec(), key, hash.as_bytes_array())
}

fn encrypt_index_root(root: &IndexRoot, key: Option<&EncryptKey>) -> Result<Vec<u8>, Error> {
    let plaintext = root.serialise()?;
    let Some(key) = key else {
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

/// Translate a storage-layer `Error::CasFailed` into pack-specific guidance.
///
/// `Error::CasFailed`'s default message tells the user to "Run 'omemfs pull'
/// and retry 'omemfs push'", which is correct for `push` but misleading for
/// `pack`: pack only rewrites the pack layer and index root and never touches
/// the working tree, so a `pull` is meaningless. Re-running `omemfs pack`
/// re-reads the current index root and retries the consolidation. Any other
/// error is passed through unchanged. See design/04 "omemfs pack" → Errors.
fn remap_cas_failure(e: Error) -> Error {
    match e {
        Error::CasFailed => Error::Other(
            "remote has been updated since last sync\nRe-run 'omemfs pack'.".to_string(),
        ),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remap_cas_failure_gives_pack_specific_guidance() {
        // A CAS failure during pack must be remapped to pack-oriented guidance:
        // keep the shared first line, but tell the user to re-run pack — never
        // to retry a push (pack does not touch the working tree, so no pull).
        let msg = remap_cas_failure(Error::CasFailed).to_string();
        assert!(
            msg.contains("remote has been updated since last sync"),
            "first line must be preserved, got: {msg:?}"
        );
        assert!(
            msg.contains("Re-run 'omemfs pack'."),
            "pack guidance must instruct re-running pack, got: {msg:?}"
        );
        assert!(
            !msg.contains("omemfs push"),
            "pack guidance must not mention push, got: {msg:?}"
        );
        assert!(
            !msg.contains("omemfs pull"),
            "pack guidance must not instruct a pull, got: {msg:?}"
        );
    }

    #[test]
    fn remap_cas_failure_passes_through_other_errors() {
        // Non-CAS errors must be returned unchanged.
        let msg = remap_cas_failure(Error::Other("disk on fire".to_string())).to_string();
        assert_eq!(msg, "disk on fire");
    }
}
