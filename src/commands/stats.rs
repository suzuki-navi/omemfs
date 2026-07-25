use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::PathBuf;

use crate::codec::compress;
use crate::codec::pack::index::{IndexEntry, IndexFile};
use crate::codec::pack::index_root::IndexRoot;
use crate::codec::pack::reader::PackReader;
use crate::dtimer_l1;
use crate::error::Error;
use crate::io_stats::{self, IoStatsRecord, PackDetail};
use crate::object::{Hash, TYPE_TAG_CHUNK, TYPE_TAG_MANIFEST, TYPE_TAG_TREE};
use crate::repo::Repo;
use crate::store::ObjectStore;
use crate::store::local::LocalStore;
use crate::term::Output;

// ---------------------------------------------------------------------------
// Remote storage classification (cheap LIST + index metadata)
// ---------------------------------------------------------------------------

/// One class in the remote-storage capacity breakdown: count + total bytes.
#[derive(Debug, Default, Clone, Copy)]
struct ClassStat {
    count: u64,
    bytes: u64,
}

impl ClassStat {
    fn add(&mut self, bytes: u64) {
        self.count += 1;
        self.bytes += bytes;
    }
}

/// Remote storage capacity breakdown, classified from a single cheap LIST plus
/// the index-derived known storage-key sets. No data-object contents are read.
#[derive(Debug, Default)]
struct RemoteStorage {
    pack_files: ClassStat,
    index_files: ClassStat,
    bloom: ClassStat,
    standalone_objects: ClassStat,
    index_root: ClassStat,
    orphans: ClassStat,
}

impl RemoteStorage {
    fn total(&self) -> ClassStat {
        let mut t = ClassStat::default();
        for c in [
            self.pack_files,
            self.index_files,
            self.bloom,
            self.standalone_objects,
            self.index_root,
            self.orphans,
        ] {
            t.count += c.count;
            t.bytes += c.bytes;
        }
        t
    }
}

/// Known storage-key sets, derived from the index root and its index files.
#[derive(Default)]
struct KnownKeys {
    pack: HashSet<String>,
    index: HashSet<String>,
    bloom: HashSet<String>,
    standalone: HashSet<String>,
    index_root: HashSet<String>,
}

/// Walk all index files referenced by `index_root` (hot, deltas, cold shards)
/// and collect the storage keys of: index files themselves, the bloom filter,
/// the index root, every PackEntry.pack_hash, and every StandaloneEntry.hash.
///
/// `load` reads each small index file (hot/delta/cold) — this is the only remote
/// read this section performs; data-object contents are never read.
fn collect_known_keys(remote: &LocalStore, index_root: &IndexRoot) -> KnownKeys {
    let mut k = KnownKeys::default();

    // Index-root storage key. On encrypted remotes the object lives under
    // objects/ at the derived name, so its LIST key is the FULL 64-hex
    // `index_root_name` (the same string `list_with_sizes` reconstructs from the
    // sharded path) — NOT the path leaf. On unencrypted remotes it is the fixed
    // INDEX_ROOT file at the prefix root (NOT under objects/), so it will not
    // appear in list_with_sizes at all — it is stat-ed separately by the caller.
    match remote.encrypt_key.as_ref() {
        Some(key) => {
            let name = crate::codec::pack::index_root_name(key);
            k.index_root.insert(name.as_str().to_string());
        }
        None => {
            k.index_root
                .insert(crate::codec::pack::INDEX_ROOT_FILE.to_string());
        }
    }

    // Loader for a single index file by logical hash.
    let load_index = |hash: &Hash| -> Option<IndexFile> {
        let mut r = remote.open_read(hash).ok()?;
        let mut stored = Vec::new();
        r.read_to_end(&mut stored).ok()?;
        let plaintext = crate::codec::encrypt::decrypt(
            stored,
            remote.encrypt_key.as_ref(),
            hash.as_bytes_array(),
        )
        .ok()?;
        IndexFile::deserialise(&plaintext).ok()
    };

    let absorb_index = |k: &mut KnownKeys, hash: &Hash, idx: &IndexFile| {
        k.index
            .insert(remote.storage_key_of(hash).as_str().to_string());
        for entry in idx.entries() {
            match entry {
                IndexEntry::Pack(pe) => {
                    k.pack
                        .insert(remote.storage_key_of(&pe.pack_hash).as_str().to_string());
                }
                IndexEntry::Standalone(se) => {
                    k.standalone
                        .insert(remote.storage_key_of(&se.hash).as_str().to_string());
                }
                IndexEntry::Inline(_) => {} // inline data lives inside the index file
            }
        }
    };

    // Hot index.
    if let Some(hot_hash) = index_root.hot_hash_opt() {
        if let Some(idx) = load_index(&hot_hash) {
            absorb_index(&mut k, &hot_hash, &idx);
        } else {
            k.index
                .insert(remote.storage_key_of(&hot_hash).as_str().to_string());
        }
    }

    // Delta indexes.
    for dh in index_root.delta_hashes_as_hashes() {
        if let Some(idx) = load_index(&dh) {
            absorb_index(&mut k, &dh, &idx);
        } else {
            k.index
                .insert(remote.storage_key_of(&dh).as_str().to_string());
        }
    }

    // Cold shards (distinct hashes only).
    let mut seen_cold: HashSet<String> = HashSet::new();
    for shard_bytes in &index_root.cold_shards {
        if *shard_bytes == [0u8; 32] {
            continue;
        }
        let sh = Hash::from_bytes(*shard_bytes);
        if !seen_cold.insert(sh.as_str().to_string()) {
            continue;
        }
        if let Some(idx) = load_index(&sh) {
            absorb_index(&mut k, &sh, &idx);
        } else {
            k.index
                .insert(remote.storage_key_of(&sh).as_str().to_string());
        }
    }

    // Bloom filter.
    if let Some(bh) = index_root.bloom_hash_opt() {
        k.bloom
            .insert(remote.storage_key_of(&bh).as_str().to_string());
    }

    k
}

/// Classify the remote via a single cheap LIST plus the known storage-key sets.
/// Returns `None` if the remote has no index root (never pushed).
///
/// The second element of the returned tuple is the list of all object sizes
/// (including the unencrypted index root when stat-ed separately) to be used
/// for the remote-object size histogram — no additional remote I/O is needed.
fn classify_remote(
    remote: &LocalStore,
    root_pointer: Box<dyn crate::codec::pack::root_pointer::RootPointer>,
    packcache: LocalStore,
) -> Option<(RemoteStorage, Vec<u64>)> {
    // Read the raw index-root bytes once through the backend-pluggable root
    // pointer. Its length is the index-root object size used below (backend-
    // neutral: no filesystem path assumption). `None` => the remote has never
    // been pushed.
    let ir_raw = root_pointer.read().ok()?.0;
    let index_root_size = ir_raw.as_ref().map(|b| b.len() as u64);

    // Read the index root via the pack reader (cheap: one small object).
    let pack_reader = PackReader::new(
        Box::new(remote.clone()),
        // A throwaway local cache is fine: read_index_root resolves the root
        // pointer directly, it does not consult this cache.
        remote.clone(),
        packcache.clone(),
        packcache,
        root_pointer,
        remote.encrypt_key.clone(),
    );
    let index_root = pack_reader.read_index_root().ok().flatten()?;

    let known = collect_known_keys(remote, &index_root);

    let mut rs = RemoteStorage::default();
    let mut all_sizes: Vec<u64> = Vec::new();

    // Single cheap enumeration of objects/ (sizes only — no GET).
    let listing = remote.list_with_sizes().unwrap_or_default();
    for (key, size) in listing {
        all_sizes.push(size);
        if known.pack.contains(&key) {
            rs.pack_files.add(size);
        } else if known.index.contains(&key) {
            rs.index_files.add(size);
        } else if known.bloom.contains(&key) {
            rs.bloom.add(size);
        } else if known.standalone.contains(&key) {
            rs.standalone_objects.add(size);
        } else if known.index_root.contains(&key) {
            rs.index_root.add(size);
        } else {
            // Anything not referenced by the index is an orphan, reclaimable
            // via the backup-reclone cycle.
            rs.orphans.add(size);
        }
    }

    // On unencrypted remotes the index root lives at the prefix root (not under
    // objects/), so it is absent from the LIST above; account for it using the
    // size of the bytes read from the root pointer earlier (backend-neutral).
    if rs.index_root.count == 0
        && let Some(ir_size) = index_root_size
    {
        all_sizes.push(ir_size);
        rs.index_root.add(ir_size);
    }

    Some((rs, all_sizes))
}

// ---------------------------------------------------------------------------
// Size-distribution histogram
// ---------------------------------------------------------------------------

/// Fixed histogram bucket definitions.
/// Each entry: (lower bound in bytes inclusive, display label).
/// The last entry covers everything at or above 16 MiB.
static HISTOGRAM_BUCKETS: &[(u64, &str)] = &[
    (0, "<256B"),
    (256, "256B-1KB"),
    (1_024, "1-4KB"),
    (4_096, "4-16KB"),
    (16_384, "16-64KB"),
    (65_536, "64-256KB"),
    (262_144, "256KB-1MB"),
    (1_048_576, "1-4MB"),
    (4_194_304, "4-16MB"),
    (16_777_216, ">16MB"),
];

/// Assign `size` to its bucket index (index into `HISTOGRAM_BUCKETS`).
fn bucket_index(size: u64) -> usize {
    // Walk buckets in reverse; first one whose lower bound ≤ size wins.
    for (i, &(lower, _)) in HISTOGRAM_BUCKETS.iter().enumerate().rev() {
        if size >= lower {
            return i;
        }
    }
    0
}

/// Build a histogram from a slice of sizes.
/// Returns a `Vec` of `(label, count, total_bytes)` for every non-empty bucket,
/// in ascending bucket order (smallest first).
pub fn size_histogram(sizes: &[u64]) -> Vec<(&'static str, u64, u64)> {
    let n = HISTOGRAM_BUCKETS.len();
    let mut counts = vec![0u64; n];
    let mut bytes = vec![0u64; n];

    for &s in sizes {
        let idx = bucket_index(s);
        counts[idx] += 1;
        bytes[idx] += s;
    }

    HISTOGRAM_BUCKETS
        .iter()
        .enumerate()
        .filter(|&(i, _)| counts[i] > 0)
        .map(|(i, &(_, label))| (label, counts[i], bytes[i]))
        .collect()
}

/// Perform a read-only, filter-aware walk of `work_dir` and collect the sizes
/// of all regular files that omemfs would track.
///
/// This mirrors the ignore/exclusion semantics of `scan.rs` (`scan_dir`):
/// - `.omemfs/` directories at any depth are skipped entirely.
/// - Conflict helpers (`*.omemfs-conflict-{base,local,remote}`) are skipped.
/// - Stub files (`*.omemfs-stub`, `.omemfs-stub`) are skipped.
/// - Unknown reserved `.omemfs-*` names are skipped.
/// - `.omemfs-filter` itself IS included (scan.rs includes it).
/// - Symlinks are excluded (they have no associated blob size).
/// - Directories matching `[ignore]` patterns are pruned immediately (not
///   descended into), mirroring scan.rs ~line 279 and the early-return in
///   `scan_dir`. This is essential for performance with large ignored trees
///   like `node_modules/`.
///
/// The function only calls `fs::symlink_metadata` and reads directory entries;
/// it does NOT hash or store anything.
fn collect_worktree_sizes(work_dir: &std::path::Path) -> Vec<u64> {
    let filters = crate::filter::FilterSet::load(work_dir);
    let mut sizes = Vec::new();
    walk_dir_sizes(work_dir, "", &filters, &mut sizes);
    sizes
}

/// Recursive helper for `collect_worktree_sizes`.
/// `rel_prefix` is the path relative to `work_dir` of `dir` (empty string for
/// the root).
fn walk_dir_sizes(
    dir: &std::path::Path,
    rel_prefix: &str,
    filters: &crate::filter::FilterSet,
    out: &mut Vec<u64>,
) {
    let rd = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };

    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();

        // Always skip .omemfs/ at every directory level (mirrors scan_dir).
        if name == ".omemfs" {
            continue;
        }

        // Skip conflict helpers and stub files (scan.rs excluded names).
        if crate::scan::is_conflict_helper(&name) {
            continue;
        }
        if crate::stub::is_stub_filename(&name) {
            continue;
        }
        // Skip unknown reserved names (forward-compat, same as scan.rs).
        if crate::scan::is_unknown_reserved_name(&name) {
            continue;
        }

        let rel_path = if rel_prefix.is_empty() {
            name.clone()
        } else {
            format!("{}/{}", rel_prefix, name)
        };

        // Prune ignored paths immediately (do NOT descend into ignored dirs).
        // This mirrors scan_dir's `filters.is_ignored(&rel_path)` guard at ~line 279.
        if filters.is_ignored(&rel_path) {
            continue;
        }

        let child = dir.join(&name);
        let meta = match fs::symlink_metadata(&child) {
            Ok(m) => m,
            Err(_) => continue,
        };

        let ftype = meta.file_type();
        if ftype.is_symlink() {
            // Exclude symlinks — they have no associated blob size in the
            // object model, matching scan.rs which treats them separately.
            continue;
        } else if ftype.is_dir() {
            walk_dir_sizes(&child, &rel_path, filters, out);
        } else if ftype.is_file() {
            out.push(meta.len());
        }
    }
}

pub struct StatsOptions {
    pub work_dir: PathBuf,
    /// When true, also compute the remote-backed sections (Remote storage and
    /// Remote object sizes). When false, NO remote I/O is performed.
    pub remote: bool,
    pub json: bool,
}

#[derive(Debug, Default)]
struct SizeStats {
    count: u64,
    total_stored: u64,
    total_logical: u64,
    min_stored: Option<u64>,
    max_stored: Option<u64>,
}

impl SizeStats {
    fn add(&mut self, stored: u64, logical: u64) {
        self.count += 1;
        self.total_stored += stored;
        self.total_logical += logical;
        self.min_stored = Some(self.min_stored.map_or(stored, |m: u64| m.min(stored)));
        self.max_stored = Some(self.max_stored.map_or(stored, |m: u64| m.max(stored)));
    }

    fn avg_stored(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.total_stored as f64 / self.count as f64
        }
    }

    fn compression_ratio(&self) -> Option<f64> {
        if self.total_logical == 0 {
            None
        } else {
            Some(self.total_stored as f64 / self.total_logical as f64 * 100.0)
        }
    }
}

#[derive(Debug, Default, Clone)]
struct CompressCount {
    dict_zstd: u64,
    plain_zstd: u64,
    escaped_raw: u64,
    raw: u64,
    // Stored bytes attributable to each compression method. Populated only via
    // `add_sized` (the count-only `add` leaves these at zero); used by the
    // Compression (L4) table's `stored` column.
    dict_zstd_bytes: u64,
    plain_zstd_bytes: u64,
    escaped_raw_bytes: u64,
    raw_bytes: u64,
}

impl CompressCount {
    fn add(&mut self, magic: [u8; 2]) {
        match magic {
            [0xED, 0xDE] => self.dict_zstd += 1,
            [0xED, 0xDF] => self.plain_zstd += 1,
            [0xED, 0xD0] => self.escaped_raw += 1,
            _ => self.raw += 1,
        }
    }

    /// Like `add` but also accumulates the stored byte length for the matching
    /// method, so the per-method size impact can be reported.
    fn add_sized(&mut self, magic: [u8; 2], len: u64) {
        match magic {
            [0xED, 0xDE] => {
                self.dict_zstd += 1;
                self.dict_zstd_bytes += len;
            }
            [0xED, 0xDF] => {
                self.plain_zstd += 1;
                self.plain_zstd_bytes += len;
            }
            [0xED, 0xD0] => {
                self.escaped_raw += 1;
                self.escaped_raw_bytes += len;
            }
            _ => {
                self.raw += 1;
                self.raw_bytes += len;
            }
        }
    }

    fn total(&self) -> u64 {
        self.dict_zstd + self.plain_zstd + self.escaped_raw + self.raw
    }
}

#[derive(Debug, Default)]
struct BlobTypeEntry {
    size: SizeStats,
    compress: CompressCount,
}

/// Local cache composition (deep scan of `.omemfs/objects/`).
#[derive(Debug, Default)]
struct Stats {
    total: u64,
    tree_stats: SizeStats,
    blob_stats: SizeStats,
    chunk_manifest_count: u64,
    chunk_body_count: u64,
    unknown_count: u64,
    compress_total: CompressCount,
    compress_tree: CompressCount,
    compress_blob: CompressCount,
    total_stored_bytes: u64,
    blob_type_stats: HashMap<&'static str, BlobTypeEntry>,
}

/// Detect the content type of a blob from its magic bytes.
fn detect_blob_type(data: &[u8]) -> &'static str {
    if data.len() >= 3 && data[0] == 0xFF && data[1] == 0xD8 && data[2] == 0xFF {
        return "jpeg";
    }
    if data.len() >= 8 && &data[..8] == b"\x89PNG\r\n\x1a\n" {
        return "png";
    }
    if data.len() >= 4 && &data[..4] == b"GIF8" {
        return "gif";
    }
    if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        return "webp";
    }
    if data.len() >= 4 && &data[..4] == b"%PDF" {
        return "pdf";
    }
    if data.len() >= 4 && data[0] == 0x50 && data[1] == 0x4B && data[2] == 0x03 && data[3] == 0x04 {
        return "zip";
    }
    if data.len() >= 2 && data[0] == 0x1F && data[1] == 0x8B {
        return "gzip";
    }
    if data.len() >= 4 && data[0] == 0x28 && data[1] == 0xB5 && data[2] == 0x2F && data[3] == 0xFD {
        return "zstd";
    }
    if data.len() >= 2 {
        let b = (data[0], data[1]);
        if b == (0x78, 0x01) || b == (0x78, 0x5E) || b == (0x78, 0x9C) || b == (0x78, 0xDA) {
            return "zlib";
        }
    }
    let sample = &data[..data.len().min(1024)];
    if std::str::from_utf8(sample).is_ok()
        && !sample.iter().any(|&b| b < 0x09 || (b > 0x0D && b < 0x20))
    {
        return "text";
    }
    "binary"
}

/// Classify and accumulate one stored object into `stats`.
/// `stored` is the raw bytes as read from the object store.
/// `skip_hashes` is used to avoid double-counting objects present in both stores.
fn classify_object(stored: &[u8], stats: &mut Stats) {
    let stored_len = stored.len() as u64;
    stats.total_stored_bytes += stored_len;
    stats.total += 1;

    // Check for standalone escape: strip ED E0 prefix and re-examine inner bytes.
    let effective = if stored.len() >= 2 && stored[0] == 0xED && stored[1] == 0xE0 {
        &stored[2..]
    } else {
        stored
    };

    // L6 objects (pack/index/bloom/index-root, ED E1..EF) are only written to
    // the remote, not the local cache. If one is somehow present in the local
    // cache, classify it as unknown — the remote pack layer is reported
    // separately via the cheap LIST-based "Remote storage" section.
    if effective.len() >= 2 && effective[0] == 0xED && (0xE1..=0xEF).contains(&effective[1]) {
        stats.unknown_count += 1;
        return;
    }

    let compress_magic = if effective.len() >= 2 {
        [effective[0], effective[1]]
    } else {
        [0, 0]
    };
    stats.compress_total.add_sized(compress_magic, stored_len);

    match compress::decompress(effective) {
        Ok(logical) => {
            let logical_len = logical.len() as u64;
            if logical.starts_with(&TYPE_TAG_MANIFEST) {
                stats.chunk_manifest_count += 1;
            } else if logical.starts_with(&TYPE_TAG_CHUNK) {
                stats.chunk_body_count += 1;
            } else if logical.starts_with(&TYPE_TAG_TREE) {
                use crate::object::Tree;
                if Tree::deserialise(&logical).is_ok() {
                    stats.tree_stats.add(stored_len, logical_len);
                    stats.compress_tree.add(compress_magic);
                } else {
                    stats.unknown_count += 1;
                }
            } else {
                let blob_type = detect_blob_type(&logical);
                stats.blob_stats.add(stored_len, logical_len);
                stats.compress_blob.add(compress_magic);
                let entry = stats.blob_type_stats.entry(blob_type).or_default();
                entry.size.add(stored_len, logical_len);
                entry.compress.add(compress_magic);
            }
        }
        Err(_) => {
            stats.unknown_count += 1;
        }
    }
}

pub fn run(opts: StatsOptions) -> Result<(), Error> {
    let repo = Repo::open(&opts.work_dir)?;
    crate::progress::notify_repo_dir(&repo.work_dir);
    let _t = dtimer_l1!("stats");

    let local = repo.local_store();
    let local_hashes: Vec<String> = local.iter_hashes();

    // -----------------------------------------------------------------------
    // Remote storage breakdown — cheap LIST + index-metadata classification.
    // No data-object contents are read (only small index files + bloom).
    // Also collect the per-object sizes for the remote-object histogram.
    //
    // Gated behind --remote: without the flag, NO remote I/O is performed and
    // both remote-backed sections are omitted. The default invocation is
    // local-only and offline-safe.
    // -----------------------------------------------------------------------
    let (remote_storage, remote_object_sizes): (Option<RemoteStorage>, Vec<u64>) = if opts.remote {
        // Build the origin remote store and root pointer for ANY backend
        // (local or cloud). store_for_config / root_pointer_for_config wire up
        // all four backends, so classification works uniformly through the
        // ObjectStore / RootPointer abstractions; the cloud arms perform a
        // single paginated LIST plus small index/bloom GETs, mirroring the
        // local directory walk.
        //
        // Failures here (no remote configured, unreachable backend, missing
        // credentials) are swallowed to None so the default invocation stays
        // offline-safe and the local-only sections still print.
        match (
            repo.remote_store("origin"),
            repo.remote_root_pointer("origin"),
        ) {
            (Ok(remote), Ok(root_pointer)) => {
                match classify_remote(&remote, root_pointer, repo.packcache_store()) {
                    Some((rs, sizes)) => (Some(rs), sizes),
                    None => (None, Vec::new()),
                }
            }
            _ => (None, Vec::new()),
        }
    } else {
        (None, Vec::new())
    };

    // -----------------------------------------------------------------------
    // Local cache composition — deep scan of .omemfs/objects/.
    // Local objects are not encrypted, so storage_key == logical_hash and
    // open_read_by_storage_key is equivalent to open_read here.
    // -----------------------------------------------------------------------
    let mut stats = Stats::default();
    let phase = crate::progress::begin_phase("Scan local cache");
    for hex in &local_hashes {
        let mut reader = match local.open_read_by_storage_key(hex) {
            Ok(r) => r,
            Err(_) => {
                stats.unknown_count += 1;
                stats.total += 1;
                continue;
            }
        };
        let mut stored = Vec::new();
        if reader.read_to_end(&mut stored).is_err() {
            stats.unknown_count += 1;
            stats.total += 1;
            continue;
        }
        classify_object(&stored, &mut stats);
    }
    phase.complete(format!("{} objects", stats.total));

    let omemfs_dir = repo.work_dir.join(".omemfs");
    let io_recent = io_stats::read_recent(&omemfs_dir, 20);
    let io_all = if io_recent.is_empty() {
        Vec::new()
    } else {
        io_stats::read_all(&omemfs_dir)
    };

    // Most recent record carrying pack_detail, for the Pack effectiveness section.
    let pack_eff: Option<(String, PackDetail)> = io_all
        .iter()
        .rev()
        .find_map(|r| r.pack_detail.clone().map(|d| (r.ts.clone(), d)));

    // -----------------------------------------------------------------------
    // Working-tree file sizes — read-only filtered walk of the working tree.
    // Mirrors scan.rs ignore/exclusion semantics (see collect_worktree_sizes).
    // -----------------------------------------------------------------------
    let wt_sizes = collect_worktree_sizes(&repo.work_dir);

    let mut out = Output::for_stdout();
    if opts.json {
        print_json(
            &stats,
            &remote_storage,
            &remote_object_sizes,
            pack_eff.as_ref(),
            &io_recent,
            &io_all,
            &wt_sizes,
            &mut out,
        )?;
    } else {
        print_text(
            &stats,
            &remote_storage,
            &remote_object_sizes,
            pack_eff.as_ref(),
            &io_recent,
            &io_all,
            &wt_sizes,
            &mut out,
        )?;
    }
    out.finish()?;

    Ok(())
}

fn fmt_size(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GiB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MiB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1_024 {
        format!("{:.1} KiB", bytes as f64 / 1_024.0)
    } else {
        format!("{} B", bytes)
    }
}

/// Format an integer count with thousands separators (e.g. 1773 → "1,773").
fn fmt_int(n: u64) -> String {
    let digits = n.to_string();
    let bytes = digits.as_bytes();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    let len = bytes.len();
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(b as char);
    }
    out
}

fn compression_bar(ratio_pct: f64, width: usize) -> String {
    let filled = ((ratio_pct / 100.0) * width as f64).round() as usize;
    let filled = filled.min(width);
    let empty = width - filled;
    format!("{}{}", "█".repeat(filled), "░".repeat(empty))
}

/// Compact size format (no space): "15.0MB", "12.0KB", "512B", "1.4GB".
/// Uses the same thresholds and precision as `fmt_size`.
fn fmt_size_compact(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1}GiB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1}MiB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1_024 {
        format!("{:.1}KiB", bytes as f64 / 1_024.0)
    } else {
        format!("{}B", bytes)
    }
}

const SECTION_RULE: &str = "──────────────────────────────────────────────────────────────────────";

/// Print one size-distribution histogram block.
///
/// `title` is the section header (e.g. `"Remote object sizes  (origin)"`).
/// `sizes` is the slice of byte counts to bucket.
fn print_histogram(out: &mut Output, title: &str, sizes: &[u64]) -> Result<(), Error> {
    let hist = size_histogram(sizes);
    if hist.is_empty() {
        return Ok(());
    }
    let total_n: u64 = hist.iter().map(|&(_, c, _)| c).sum();
    let total_bytes: u64 = hist.iter().map(|&(_, _, b)| b).sum();
    out.writeln(&format!(
        "{}   (n={}, total={})",
        title,
        fmt_int(total_n),
        fmt_size(total_bytes)
    ))?;
    out.writeln(SECTION_RULE)?;

    // Pre-compute formatted byte and count strings to find column widths.
    let bytes_strs: Vec<String> = hist.iter().map(|&(_, _, b)| fmt_size(b)).collect();
    let bytes_col_w = bytes_strs
        .iter()
        .map(|s| s.len())
        .max()
        .unwrap_or(5)
        .max("bytes".len());
    let count_strs: Vec<String> = hist.iter().map(|&(_, c, _)| fmt_int(c)).collect();
    let count_col_w = count_strs
        .iter()
        .map(|s| s.len())
        .max()
        .unwrap_or(5)
        .max("count".len());

    // Width of the `distribution` bar (block characters); the header label is
    // wider than the bar, so the column width is the header label length.
    const BAR_W: usize = 18;
    let dist_col_w = "distribution".len().max(BAR_W);

    out.writeln(&format!(
        "  {:<10}  {:>cw$}   {:>5}   {:<dw$}   {:>bw$}   {:>5}",
        "bucket",
        "count",
        "%cnt",
        "distribution",
        "bytes",
        "%byt",
        cw = count_col_w,
        dw = dist_col_w,
        bw = bytes_col_w,
    ))?;

    for (i, &(label, count, bytes)) in hist.iter().enumerate() {
        let pct_cnt = count as f64 / total_n as f64 * 100.0;
        let pct_byt = if total_bytes == 0 {
            0.0
        } else {
            bytes as f64 / total_bytes as f64 * 100.0
        };
        out.writeln(&format!(
            "  {:<10}  {:>cw$}   {:>4.1}%   {:<dw$}   {:>bw$}   {:>4.1}%",
            label,
            count_strs[i],
            pct_cnt,
            compression_bar(pct_cnt, BAR_W),
            bytes_strs[i],
            pct_byt,
            cw = count_col_w,
            dw = dist_col_w,
            bw = bytes_col_w,
        ))?;
    }
    out.writeln("")?;
    Ok(())
}

/// Print a group banner line: `━━━ LABEL ━━━…` padded with a heavy rule to a
/// fixed visible width. The heavy rule visually distinguishes a group banner
/// from the lighter per-section rule (`SECTION_RULE`).
fn print_banner(out: &mut Output, label: &str) -> Result<(), Error> {
    const WIDTH: usize = 70;
    let prefix = format!("━━━ {} ", label);
    let fill = WIDTH.saturating_sub(prefix.chars().count());
    out.writeln(&format!("{}{}", prefix, "━".repeat(fill)))?;
    Ok(())
}

fn print_text(
    s: &Stats,
    remote_storage: &Option<RemoteStorage>,
    remote_object_sizes: &[u64],
    pack_eff: Option<&(String, PackDetail)>,
    io_recent: &[IoStatsRecord],
    io_all: &[IoStatsRecord],
    wt_sizes: &[u64],
    out: &mut Output,
) -> Result<(), Error> {
    // -----------------------------------------------------------------------
    // Section 0: Summary panel — a one-glance overview printed before any group.
    // -----------------------------------------------------------------------
    let title = if remote_storage.is_some() {
        "omemfs stats — origin"
    } else {
        "omemfs stats"
    };
    out.writeln(title)?;
    out.writeln(&"═".repeat(70))?;
    if let Some(rs) = remote_storage {
        let total = rs.total();
        let orphan_bytes = rs.orphans.bytes;
        let live_bytes = total.bytes.saturating_sub(orphan_bytes);
        if rs.orphans.count > 0 {
            out.writeln(&format!(
                "  Remote   {} objects   {}   (live {} + reclaimable {})",
                fmt_int(total.count),
                fmt_size(total.bytes),
                fmt_size(live_bytes),
                fmt_size(orphan_bytes),
            ))?;
        } else {
            out.writeln(&format!(
                "  Remote   {} objects   {}",
                fmt_int(total.count),
                fmt_size(total.bytes),
            ))?;
        }
    }
    out.writeln(&format!(
        "  Local    {} objects   {} stored",
        fmt_int(s.total),
        fmt_size(s.total_stored_bytes),
    ))?;
    // `io_recent` is in file order (oldest first); the most recent record is
    // the last element, not the first.
    if let Some(rec) = io_recent.last() {
        out.writeln(&format!(
            "  Last I/O   {}  {}   ({} writes {}, {} reads {})",
            format_time(&rec.ts),
            rec.cmd,
            rec.writes,
            fmt_size(rec.write_bytes),
            rec.reads,
            fmt_size(rec.read_bytes),
        ))?;
    }
    out.writeln("")?;

    // REMOTE group banner — only when there is remote content to show.
    if remote_storage.is_some() || !io_recent.is_empty() {
        print_banner(out, "REMOTE")?;
    }

    // -----------------------------------------------------------------------
    // Section 1: Remote storage (origin) — cheap LIST classification.
    // -----------------------------------------------------------------------
    if let Some(rs) = remote_storage {
        out.writeln("Remote storage  (origin)")?;
        out.writeln(SECTION_RULE)?;
        let line =
            |out: &mut Output, label: &str, c: ClassStat, suffix: &str| -> Result<(), Error> {
                out.writeln(&format!(
                    "  {:<20} {:>6}   {}{}",
                    label,
                    fmt_int(c.count),
                    fmt_size(c.bytes),
                    suffix
                ))?;
                Ok(())
            };
        line(out, "pack-files", rs.pack_files, "")?;
        line(out, "index-files", rs.index_files, "")?;
        line(out, "bloom", rs.bloom, "")?;
        line(out, "standalone-objects", rs.standalone_objects, "")?;
        line(out, "index-root", rs.index_root, "")?;
        line(
            out,
            "orphans",
            rs.orphans,
            "   (reclaimable via backup-reclone)",
        )?;
        out.writeln("  ────────────────────────────────────")?;
        line(out, "total", rs.total(), "")?;
        out.writeln("")?;
    }

    // -----------------------------------------------------------------------
    // Section 2: Remote object sizes histogram (only when remote is local-type).
    // -----------------------------------------------------------------------
    if remote_storage.is_some() && !remote_object_sizes.is_empty() {
        print_histogram(out, "Remote object sizes  (origin)", remote_object_sizes)?;
    }

    // -----------------------------------------------------------------------
    // Section 3: Recent I/O (printed below by print_recent_io).
    // -----------------------------------------------------------------------
    print_recent_io(io_recent, io_all, out)?;

    // -----------------------------------------------------------------------
    // Section 3: Pack effectiveness.
    // -----------------------------------------------------------------------
    if let Some((ts, d)) = pack_eff {
        let ts_disp = ts.replace('T', " ").trim_end_matches('Z').to_string();
        out.writeln("")?;
        out.writeln(&format!("Pack effectiveness  ({})", ts_disp))?;
        out.writeln(SECTION_RULE)?;
        out.writeln(&format!("  delta indexes merged  {}", d.deltas_merged))?;
        out.writeln(&format!(
            "  pack files  {} → {}  (consolidated {} → {})",
            d.packs_before,
            d.packs_after,
            fmt_size(d.consolidated_bytes_in),
            fmt_size(d.consolidated_bytes_out)
        ))?;
        let sizes_str = if d.pack_sizes_after.is_empty() {
            "—".to_string()
        } else {
            d.pack_sizes_after
                .iter()
                .map(|&b| fmt_size(b))
                .collect::<Vec<_>>()
                .join("  ")
        };
        out.writeln(&format!("  pack sizes  {}", sizes_str))?;
        out.writeln(&format!("  cold splits  {}", d.cold_splits))?;
        out.writeln(&format!(
            "  hot index  {} entries     bloom  {} elements",
            d.hot_index_entries, d.bloom_elements
        ))?;
    } else if !io_recent.is_empty() {
        // There is I/O history but no pack run carries pack_detail: show the
        // header with a placeholder so "not run recently" is distinguishable
        // from "section missing".
        out.writeln("")?;
        out.writeln("Pack effectiveness")?;
        out.writeln(SECTION_RULE)?;
        out.writeln("  (no recent consolidation recorded)")?;
    }

    // -----------------------------------------------------------------------
    // Section 5: Local cache composition (deep scan of .omemfs/objects/).
    // -----------------------------------------------------------------------
    out.writeln("")?;
    print_banner(out, "LOCAL")?;
    out.writeln("Local cache composition")?;
    out.writeln(SECTION_RULE)?;

    // Object types
    let total_l2_l3 = s.tree_stats.count
        + s.blob_stats.count
        + s.chunk_manifest_count
        + s.chunk_body_count
        + s.unknown_count;
    out.writeln(&format!("Objects:  {} total", fmt_int(total_l2_l3)))?;
    let pct = |n: u64| -> String {
        if total_l2_l3 == 0 {
            "  0.0%".to_string()
        } else {
            format!("{:5.1}%", n as f64 / total_l2_l3 as f64 * 100.0)
        }
    };
    if s.blob_stats.count > 0 {
        out.writeln(&format!(
            "  blob              {:>7}  ({})",
            fmt_int(s.blob_stats.count),
            pct(s.blob_stats.count)
        ))?;
    }
    if s.tree_stats.count > 0 {
        out.writeln(&format!(
            "  tree              {:>7}  ({})",
            fmt_int(s.tree_stats.count),
            pct(s.tree_stats.count)
        ))?;
    }
    if s.chunk_manifest_count > 0 || s.chunk_body_count > 0 {
        out.writeln(&format!(
            "  chunk-manifest    {:>7}",
            fmt_int(s.chunk_manifest_count)
        ))?;
        out.writeln(&format!(
            "  chunk-body        {:>7}",
            fmt_int(s.chunk_body_count)
        ))?;
    }
    if s.unknown_count > 0 {
        out.writeln(&format!(
            "  unknown           {:>7}",
            fmt_int(s.unknown_count)
        ))?;
    }

    // Compression methods table. The `stored` column reports the total stored
    // bytes attributable to each method (from compress_total's byte fields);
    // total/tree/blob remain object counts.
    let t = &s.compress_total;
    let tr = &s.compress_tree;
    let bl = &s.compress_blob;
    if t.total() > 0 {
        out.writeln("")?;
        // (method, total_count, tree_count, blob_count, stored_bytes); keep only
        // methods that occur at least once in any column.
        let rows: Vec<(&str, u64, u64, u64, u64)> = [
            (
                "dict_zstd",
                t.dict_zstd,
                tr.dict_zstd,
                bl.dict_zstd,
                t.dict_zstd_bytes,
            ),
            (
                "plain_zstd",
                t.plain_zstd,
                tr.plain_zstd,
                bl.plain_zstd,
                t.plain_zstd_bytes,
            ),
            (
                "escaped_raw",
                t.escaped_raw,
                tr.escaped_raw,
                bl.escaped_raw,
                t.escaped_raw_bytes,
            ),
            ("raw", t.raw, tr.raw, bl.raw, t.raw_bytes),
        ]
        .into_iter()
        .filter(|r| r.1 > 0 || r.2 > 0 || r.3 > 0)
        .collect();

        let total_strs: Vec<String> = rows.iter().map(|r| fmt_int(r.1)).collect();
        let tree_strs: Vec<String> = rows.iter().map(|r| fmt_int(r.2)).collect();
        let blob_strs: Vec<String> = rows.iter().map(|r| fmt_int(r.3)).collect();
        let stored_strs: Vec<String> = rows.iter().map(|r| fmt_size(r.4)).collect();
        let cw = total_strs
            .iter()
            .chain(tree_strs.iter())
            .chain(blob_strs.iter())
            .map(|x| x.len())
            .max()
            .unwrap_or(5)
            .max("total".len());
        let sw = stored_strs
            .iter()
            .map(|x| x.len())
            .max()
            .unwrap_or(6)
            .max("stored".len());

        out.writeln(&format!(
            "Compression (L4):     {:>cw$}  {:>cw$}  {:>cw$}  {:>sw$}",
            "total",
            "tree",
            "blob",
            "stored",
            cw = cw,
            sw = sw,
        ))?;
        for (i, r) in rows.iter().enumerate() {
            out.writeln(&format!(
                "  {:<18}  {:>cw$}  {:>cw$}  {:>cw$}  {:>sw$}",
                r.0,
                &total_strs[i],
                &tree_strs[i],
                &blob_strs[i],
                &stored_strs[i],
                cw = cw,
                sw = sw,
            ))?;
        }
    }

    // Storage sizes
    out.writeln("")?;
    out.writeln("Storage:")?;
    out.writeln(&format!("  total   {}", fmt_size(s.total_stored_bytes)))?;
    if s.tree_stats.count > 0 {
        out.writeln(&format!(
            "  tree    avg  {}, min {}, max {}",
            fmt_size(s.tree_stats.avg_stored() as u64),
            fmt_size(s.tree_stats.min_stored.unwrap_or(0)),
            fmt_size(s.tree_stats.max_stored.unwrap_or(0)),
        ))?;
    }
    if s.blob_stats.count > 0 {
        out.writeln(&format!(
            "  blob    avg  {}, min {}, max {}",
            fmt_size(s.blob_stats.avg_stored() as u64),
            fmt_size(s.blob_stats.min_stored.unwrap_or(0)),
            fmt_size(s.blob_stats.max_stored.unwrap_or(0)),
        ))?;
    }

    // Space saved with progress bars. Framed as saved space (1 − stored/logical)
    // so a longer bar means better compression. The bar's filled fraction equals
    // the saved percentage.
    let has_ratio =
        s.tree_stats.compression_ratio().is_some() || s.blob_stats.compression_ratio().is_some();
    if has_ratio {
        out.writeln("")?;
        out.writeln("Space saved (1 − stored / logical):")?;
        if let Some(r) = s.tree_stats.compression_ratio() {
            let saved = 100.0 - r;
            out.writeln(&format!(
                "  tree  {}  {:5.1}%",
                compression_bar(saved, 20),
                saved
            ))?;
        }
        if let Some(r) = s.blob_stats.compression_ratio() {
            let saved = 100.0 - r;
            out.writeln(&format!(
                "  blob  {}  {:5.1}%",
                compression_bar(saved, 20),
                saved
            ))?;
        }
    }

    // Blob content types (aligned table with header)
    if !s.blob_type_stats.is_empty() {
        out.writeln("")?;
        out.writeln("Blob content types:")?;

        let mut entries: Vec<(&'static str, &BlobTypeEntry)> =
            s.blob_type_stats.iter().map(|(&k, v)| (k, v)).collect();
        entries.sort_by(|a, b| b.1.size.count.cmp(&a.1.size.count).then(a.0.cmp(b.0)));

        // Pre-format variable-width columns to compute column widths.
        let stored_strs: Vec<String> = entries
            .iter()
            .map(|(_, v)| fmt_size(v.size.total_stored))
            .collect();
        let logical_strs: Vec<String> = entries
            .iter()
            .map(|(_, v)| fmt_size(v.size.total_logical))
            .collect();
        let ratio_strs: Vec<String> = entries
            .iter()
            .map(|(_, v)| {
                v.size
                    .compression_ratio()
                    .map(|r| format!("{:.1}%", r))
                    .unwrap_or_else(|| "n/a".to_string())
            })
            .collect();

        let label_w = entries
            .iter()
            .map(|(k, _)| k.len())
            .max()
            .unwrap_or(4)
            .max("type".len());
        let count_w = entries
            .iter()
            .map(|(_, v)| format!("{}", v.size.count).len())
            .max()
            .unwrap_or(5)
            .max("count".len());
        let stored_w = stored_strs
            .iter()
            .map(|s| s.len())
            .max()
            .unwrap_or(6)
            .max("stored".len());
        let logical_w = logical_strs
            .iter()
            .map(|s| s.len())
            .max()
            .unwrap_or(7)
            .max("logical".len());
        let ratio_w = ratio_strs
            .iter()
            .map(|s| s.len())
            .max()
            .unwrap_or(5)
            .max("ratio".len());

        // Header row.
        out.writeln(&format!(
            "  {:<lw$}  {:>cw$}  {:>sw$}  {:>lw2$}  {:>rw$}  compression",
            "type",
            "count",
            "stored",
            "logical",
            "ratio",
            lw = label_w,
            cw = count_w,
            sw = stored_w,
            lw2 = logical_w,
            rw = ratio_w,
        ))?;
        out.writeln(&format!(
            "  {}  {}  {}  {}  {}",
            "─".repeat(label_w),
            "─".repeat(count_w),
            "─".repeat(stored_w),
            "─".repeat(logical_w),
            "─".repeat(ratio_w),
        ))?;

        for (i, (label, entry)) in entries.iter().enumerate() {
            let c = &entry.compress;

            // Build compact compression breakdown.
            let methods_used = [c.dict_zstd, c.plain_zstd, c.escaped_raw, c.raw]
                .iter()
                .filter(|&&n| n > 0)
                .count();
            let compress_str = if methods_used <= 1 {
                if c.dict_zstd > 0 {
                    "dict".to_string()
                } else if c.plain_zstd > 0 {
                    "zstd".to_string()
                } else if c.escaped_raw > 0 {
                    "esc".to_string()
                } else {
                    "raw".to_string()
                }
            } else {
                let mut parts = Vec::new();
                if c.dict_zstd > 0 {
                    parts.push(format!("dict×{}", c.dict_zstd));
                }
                if c.plain_zstd > 0 {
                    parts.push(format!("zstd×{}", c.plain_zstd));
                }
                if c.escaped_raw > 0 {
                    parts.push(format!("esc×{}", c.escaped_raw));
                }
                if c.raw > 0 {
                    parts.push(format!("raw×{}", c.raw));
                }
                parts.join("  ")
            };

            out.writeln(&format!(
                "  {:<lw$}  {:>cw$}  {:>sw$}  {:>lw2$}  {:>rw$}  {}",
                label,
                entry.size.count,
                &stored_strs[i],
                &logical_strs[i],
                &ratio_strs[i],
                compress_str,
                lw = label_w,
                cw = count_w,
                sw = stored_w,
                lw2 = logical_w,
                rw = ratio_w,
            ))?;
        }
    }

    // -----------------------------------------------------------------------
    // Section 6: Working-tree file sizes histogram (always shown).
    // -----------------------------------------------------------------------
    out.writeln("")?;
    print_histogram(out, "Working-tree file sizes", wt_sizes)?;
    Ok(())
}

/// Section 3: Recent I/O (last 20 commands). Only printed when io_stats.jsonl
/// has records. Emitted before the Pack effectiveness and Local cache sections.
/// Uses a compact table layout: one header row + one row per record.
fn print_recent_io(
    io_recent: &[IoStatsRecord],
    io_all: &[IoStatsRecord],
    out: &mut Output,
) -> Result<(), Error> {
    if io_recent.is_empty() {
        return Ok(());
    }
    out.writeln("Recent I/O  (last 20 commands)")?;
    out.writeln(SECTION_RULE)?;

    // Determine if the remote column should be shown (only when >1 distinct remote).
    let distinct_remotes: std::collections::HashSet<&str> =
        io_recent.iter().map(|r| r.remote.as_str()).collect();
    let show_remote = distinct_remotes.len() > 1;

    // Pre-format all cells to compute column widths.
    let time_strs: Vec<String> = io_recent.iter().map(|r| format_time(&r.ts)).collect();
    let cmd_strs: Vec<String> = io_recent.iter().map(|r| r.cmd.clone()).collect();
    let remote_strs: Vec<String> = if show_remote {
        io_recent.iter().map(|r| r.remote.clone()).collect()
    } else {
        Vec::new()
    };
    let writes_strs: Vec<String> = io_recent
        .iter()
        .map(|r| format_io_cell(r.writes, r.write_bytes))
        .collect();
    let reads_strs: Vec<String> = io_recent
        .iter()
        .map(|r| format_io_cell(r.reads, r.read_bytes))
        .collect();
    let head_strs: Vec<String> = io_recent
        .iter()
        .map(|r| format_head_cell(r.exists_found, r.exists_miss))
        .collect();
    let pack_strs: Vec<String> = io_recent
        .iter()
        .map(|r| format_pack_cell(r.pack_files_written, &r.pack_sizes_bytes))
        .collect();

    // Compute column widths (max of data and header).
    let time_w = time_strs
        .iter()
        .map(|s| s.len())
        .max()
        .unwrap_or(0)
        .max("time".len());
    let cmd_w = cmd_strs
        .iter()
        .map(|s| s.len())
        .max()
        .unwrap_or(0)
        .max("cmd".len());
    let remote_w = if show_remote {
        remote_strs
            .iter()
            .map(|s| s.len())
            .max()
            .unwrap_or(0)
            .max("remote".len())
    } else {
        0
    };
    let writes_w = writes_strs
        .iter()
        .map(|s| s.len())
        .max()
        .unwrap_or(0)
        .max("writes".len());
    let reads_w = reads_strs
        .iter()
        .map(|s| s.len())
        .max()
        .unwrap_or(0)
        .max("reads".len());
    let head_w = head_strs
        .iter()
        .map(|s| s.len())
        .max()
        .unwrap_or(0)
        .max("HEAD".len());
    let pack_w = pack_strs
        .iter()
        .map(|s| s.len())
        .max()
        .unwrap_or(0)
        .max("pack".len());

    // Print header row.
    if show_remote {
        out.writeln(&format!(
            "{:<tw$}  {:<cw$}  {:<rw$}  {:<ww$}  {:<rdw$}  {:<hw$}  {:<pw$}",
            "time",
            "cmd",
            "remote",
            "writes",
            "reads",
            "HEAD",
            "pack",
            tw = time_w,
            cw = cmd_w,
            rw = remote_w,
            ww = writes_w,
            rdw = reads_w,
            hw = head_w,
            pw = pack_w,
        ))?;
    } else {
        out.writeln(&format!(
            "{:<tw$}  {:<cw$}  {:<ww$}  {:<rdw$}  {:<hw$}  {:<pw$}",
            "time",
            "cmd",
            "writes",
            "reads",
            "HEAD",
            "pack",
            tw = time_w,
            cw = cmd_w,
            ww = writes_w,
            rdw = reads_w,
            hw = head_w,
            pw = pack_w,
        ))?;
    }

    // Legend for the terse compound cells, printed right under the header row.
    out.writeln("legend: HEAD = found/miss (bloom)   pack = files × total")?;

    // Print data rows.
    for i in 0..io_recent.len() {
        if show_remote {
            out.writeln(&format!(
                "{:<tw$}  {:<cw$}  {:<rw$}  {:<ww$}  {:<rdw$}  {:<hw$}  {:<pw$}",
                &time_strs[i],
                &cmd_strs[i],
                &remote_strs[i],
                &writes_strs[i],
                &reads_strs[i],
                &head_strs[i],
                &pack_strs[i],
                tw = time_w,
                cw = cmd_w,
                rw = remote_w,
                ww = writes_w,
                rdw = reads_w,
                hw = head_w,
                pw = pack_w,
            ))?;
        } else {
            out.writeln(&format!(
                "{:<tw$}  {:<cw$}  {:<ww$}  {:<rdw$}  {:<hw$}  {:<pw$}",
                &time_strs[i],
                &cmd_strs[i],
                &writes_strs[i],
                &reads_strs[i],
                &head_strs[i],
                &pack_strs[i],
                tw = time_w,
                cw = cmd_w,
                ww = writes_w,
                rdw = reads_w,
                hw = head_w,
                pw = pack_w,
            ))?;
        }
    }
    out.writeln("")?;

    // Totals across all recorded commands (unchanged).
    if !io_all.is_empty() {
        let total_writes: u64 = io_all.iter().map(|r| r.writes).sum();
        let total_write_bytes: u64 = io_all.iter().map(|r| r.write_bytes).sum();
        let total_reads: u64 = io_all.iter().map(|r| r.reads).sum();
        let total_read_bytes: u64 = io_all.iter().map(|r| r.read_bytes).sum();
        let total_found: u64 = io_all.iter().map(|r| r.exists_found).sum();
        let total_miss: u64 = io_all.iter().map(|r| r.exists_miss).sum();

        out.writeln("Totals (all recorded commands)")?;
        out.writeln(&format!(
            "  writes {:>5} ops  {}",
            fmt_int(total_writes),
            fmt_size(total_write_bytes)
        ))?;
        out.writeln(&format!(
            "  reads  {:>5} ops  {}",
            fmt_int(total_reads),
            fmt_size(total_read_bytes)
        ))?;
        let head_total = total_found + total_miss;
        if head_total > 0 {
            let miss_rate = total_miss as f64 / head_total as f64 * 100.0;
            out.writeln(&format!(
                "  HEAD   {:>5} ops  bloom miss rate: {:.1}%  (misses fall through to a remote HEAD)",
                fmt_int(head_total), miss_rate
            ))?;
        }
    }
    Ok(())
}

/// Format timestamp from "2026-06-13T02:05:11Z" to "06-13 02:05:11".
fn format_time(ts: &str) -> String {
    // Strip year, replace 'T' with space, strip 'Z'.
    // Expected format: YYYY-MM-DDTHH:MM:SSZ
    if ts.len() >= 19 && ts.chars().nth(4) == Some('-') && ts.chars().nth(10) == Some('T') {
        let month_day = &ts[5..10]; // "MM-DD"
        let hms = &ts[11..19]; // "HH:MM:SS"
        format!("{} {}", month_day, hms)
    } else {
        // Fallback: just strip T and Z.
        ts.replace('T', " ").trim_end_matches('Z').to_string()
    }
}

/// Format writes or reads cell: "<ops> <compact_size>", or "<ops> —" when bytes == 0.
fn format_io_cell(ops: u64, bytes: u64) -> String {
    if bytes == 0 {
        format!("{} —", ops)
    } else {
        format!("{} {}", ops, fmt_size_compact(bytes))
    }
}

/// Format HEAD cell: "<found>/<miss>", or "—" when both are 0.
fn format_head_cell(found: u64, miss: u64) -> String {
    if found == 0 && miss == 0 {
        "—".to_string()
    } else {
        format!("{}/{}", found, miss)
    }
}

/// Format pack cell: "<files>×<compact_total_size>", or "—" when files == 0.
fn format_pack_cell(files: u64, sizes: &[u64]) -> String {
    if files == 0 {
        "—".to_string()
    } else {
        let total: u64 = sizes.iter().sum();
        format!("{}×{}", files, fmt_size_compact(total))
    }
}

/// Emit a histogram as a JSON array of `{"bucket":"...","count":N,"bytes":N}` entries.
fn histogram_json_array(sizes: &[u64]) -> String {
    let hist = size_histogram(sizes);
    if hist.is_empty() {
        return "[]".to_string();
    }
    let mut parts = Vec::with_capacity(hist.len());
    for (label, count, bytes) in hist {
        parts.push(format!(
            "{{\"bucket\": \"{}\", \"count\": {}, \"bytes\": {}}}",
            label, count, bytes
        ));
    }
    format!("[{}]", parts.join(", "))
}

fn print_json(
    s: &Stats,
    remote_storage: &Option<RemoteStorage>,
    remote_object_sizes: &[u64],
    pack_eff: Option<&(String, PackDetail)>,
    io_recent: &[IoStatsRecord],
    io_all: &[IoStatsRecord],
    wt_sizes: &[u64],
    out: &mut Output,
) -> Result<(), Error> {
    out.writeln("{")?;
    out.writeln(&format!("  \"total\": {},", s.total))?;
    out.writeln("  \"by_type\": {")?;
    out.writeln(&format!("    \"tree\": {},", s.tree_stats.count))?;
    out.writeln(&format!("    \"blob\": {},", s.blob_stats.count))?;
    out.writeln(&format!(
        "    \"chunk_manifest\": {},",
        s.chunk_manifest_count
    ))?;
    out.writeln(&format!("    \"chunk_body\": {},", s.chunk_body_count))?;
    out.writeln(&format!("    \"unknown\": {}", s.unknown_count))?;
    out.writeln("  },")?;
    out.writeln("  \"by_compression\": {")?;
    out.writeln("    \"total\": {")?;
    out.writeln(&format!(
        "      \"dict_zstd\": {},",
        s.compress_total.dict_zstd
    ))?;
    out.writeln(&format!(
        "      \"plain_zstd\": {},",
        s.compress_total.plain_zstd
    ))?;
    out.writeln(&format!(
        "      \"escaped_raw\": {},",
        s.compress_total.escaped_raw
    ))?;
    out.writeln(&format!("      \"raw\": {}", s.compress_total.raw))?;
    out.writeln("    },")?;
    out.writeln("    \"tree\": {")?;
    out.writeln(&format!(
        "      \"dict_zstd\": {},",
        s.compress_tree.dict_zstd
    ))?;
    out.writeln(&format!(
        "      \"plain_zstd\": {},",
        s.compress_tree.plain_zstd
    ))?;
    out.writeln(&format!(
        "      \"escaped_raw\": {},",
        s.compress_tree.escaped_raw
    ))?;
    out.writeln(&format!("      \"raw\": {}", s.compress_tree.raw))?;
    out.writeln("    },")?;
    out.writeln("    \"blob\": {")?;
    out.writeln(&format!(
        "      \"dict_zstd\": {},",
        s.compress_blob.dict_zstd
    ))?;
    out.writeln(&format!(
        "      \"plain_zstd\": {},",
        s.compress_blob.plain_zstd
    ))?;
    out.writeln(&format!(
        "      \"escaped_raw\": {},",
        s.compress_blob.escaped_raw
    ))?;
    out.writeln(&format!("      \"raw\": {}", s.compress_blob.raw))?;
    out.writeln("    }")?;
    out.writeln("  },")?;
    out.writeln("  \"storage_bytes\": {")?;
    out.writeln(&format!("    \"total\": {},", s.total_stored_bytes))?;
    out.writeln(&format!(
        "    \"tree_total\": {},",
        s.tree_stats.total_stored
    ))?;
    out.writeln(&format!(
        "    \"tree_min\": {},",
        s.tree_stats.min_stored.unwrap_or(0)
    ))?;
    out.writeln(&format!(
        "    \"tree_max\": {},",
        s.tree_stats.max_stored.unwrap_or(0)
    ))?;
    out.writeln(&format!(
        "    \"blob_total\": {},",
        s.blob_stats.total_stored
    ))?;
    out.writeln(&format!(
        "    \"blob_min\": {},",
        s.blob_stats.min_stored.unwrap_or(0)
    ))?;
    out.writeln(&format!(
        "    \"blob_max\": {}",
        s.blob_stats.max_stored.unwrap_or(0)
    ))?;
    out.writeln("  },")?;
    out.writeln("  \"compression_ratio\": {")?;
    let tree_ratio = s
        .tree_stats
        .compression_ratio()
        .map(|r| format!("{:.3}", r / 100.0))
        .unwrap_or_else(|| "null".to_string());
    let blob_ratio = s
        .blob_stats
        .compression_ratio()
        .map(|r| format!("{:.3}", r / 100.0))
        .unwrap_or_else(|| "null".to_string());
    out.writeln(&format!("    \"tree\": {},", tree_ratio))?;
    out.writeln(&format!("    \"blob\": {}", blob_ratio))?;
    out.writeln("  },")?;

    // Remote storage breakdown — present only for local-type remotes.
    if let Some(rs) = remote_storage {
        out.writeln("  \"remote_storage\": {")?;
        let cls = |out: &mut Output, name: &str, c: ClassStat, comma: &str| -> Result<(), Error> {
            out.writeln(&format!(
                "    \"{}\": {{\"count\": {}, \"bytes\": {}}}{}",
                name, c.count, c.bytes, comma
            ))?;
            Ok(())
        };
        cls(out, "pack_files", rs.pack_files, ",")?;
        cls(out, "index_files", rs.index_files, ",")?;
        cls(out, "bloom", rs.bloom, ",")?;
        cls(out, "standalone_objects", rs.standalone_objects, ",")?;
        cls(out, "index_root", rs.index_root, ",")?;
        cls(out, "orphans", rs.orphans, ",")?;
        cls(out, "total", rs.total(), "")?;
        out.writeln("  },")?;
        // Remote object histogram — reuses the same sizes, no additional I/O.
        out.writeln(&format!(
            "  \"remote_object_histogram\": {},",
            histogram_json_array(remote_object_sizes)
        ))?;
    }

    // Pack effectiveness — present only when a pack_detail record exists.
    if let Some((ts, d)) = pack_eff {
        let sizes_json =
            serde_json::to_string(&d.pack_sizes_after).unwrap_or_else(|_| "[]".to_string());
        out.writeln("  \"pack_effectiveness\": {")?;
        out.writeln(&format!("    \"ts\": \"{}\",", ts))?;
        out.writeln(&format!("    \"deltas_merged\": {},", d.deltas_merged))?;
        out.writeln(&format!("    \"packs_before\": {},", d.packs_before))?;
        out.writeln(&format!("    \"packs_after\": {},", d.packs_after))?;
        out.writeln(&format!(
            "    \"consolidated_bytes_in\": {},",
            d.consolidated_bytes_in
        ))?;
        out.writeln(&format!(
            "    \"consolidated_bytes_out\": {},",
            d.consolidated_bytes_out
        ))?;
        out.writeln(&format!("    \"cold_splits\": {},", d.cold_splits))?;
        out.writeln(&format!(
            "    \"hot_index_entries\": {},",
            d.hot_index_entries
        ))?;
        out.writeln(&format!("    \"bloom_elements\": {},", d.bloom_elements))?;
        out.writeln(&format!("    \"pack_sizes_after\": {}", sizes_json))?;
        out.writeln("  },")?;
    }

    // Blob content types sorted by count descending.
    out.writeln("  \"blob_content_types\": {")?;
    let mut entries: Vec<(&'static str, &BlobTypeEntry)> =
        s.blob_type_stats.iter().map(|(&k, v)| (k, v)).collect();
    entries.sort_by(|a, b| b.1.size.count.cmp(&a.1.size.count).then(a.0.cmp(b.0)));
    let last = entries.len().saturating_sub(1);
    for (i, (label, entry)) in entries.iter().enumerate() {
        let st = &entry.size;
        let c = &entry.compress;
        let comma = if i < last { "," } else { "" };
        let ratio = st
            .compression_ratio()
            .map(|r| format!("{:.3}", r / 100.0))
            .unwrap_or_else(|| "null".to_string());
        out.writeln(&format!(
            "    \"{}\": {{\"count\": {}, \"stored_bytes\": {}, \"logical_bytes\": {}, \
             \"compression_ratio\": {}, \
             \"dict_zstd\": {}, \"plain_zstd\": {}, \"escaped_raw\": {}, \"raw\": {}}}{}",
            label,
            st.count,
            st.total_stored,
            st.total_logical,
            ratio,
            c.dict_zstd,
            c.plain_zstd,
            c.escaped_raw,
            c.raw,
            comma
        ))?;
    }
    // Close blob_content_types — with comma only when io_history will follow.
    if io_recent.is_empty() {
        out.writeln("  },")?;
        // worktree_file_histogram is always present (no trailing comma — last key).
        out.writeln(&format!(
            "  \"worktree_file_histogram\": {}",
            histogram_json_array(wt_sizes)
        ))?;
        out.writeln("}")?;
        return Ok(());
    }
    out.writeln("  },")?;

    // io_history and io_totals — only present when io_stats.jsonl has records.
    {
        out.writeln("  \"io_history\": [")?;
        let last_idx = io_recent.len().saturating_sub(1);
        for (i, rec) in io_recent.iter().enumerate() {
            let pack_sizes_json =
                serde_json::to_string(&rec.pack_sizes_bytes).unwrap_or_else(|_| "[]".to_string());
            let comma = if i < last_idx { "," } else { "" };
            out.writeln(&format!(
                "    {{\"ts\": \"{}\", \"cmd\": \"{}\", \"remote\": \"{}\", \
                 \"exists_found\": {}, \"exists_miss\": {}, \
                 \"writes\": {}, \"write_bytes\": {}, \
                 \"reads\": {}, \"read_bytes\": {}, \
                 \"pack_files_written\": {}, \"pack_sizes_bytes\": {}}}{}",
                rec.ts,
                rec.cmd,
                rec.remote,
                rec.exists_found,
                rec.exists_miss,
                rec.writes,
                rec.write_bytes,
                rec.reads,
                rec.read_bytes,
                rec.pack_files_written,
                pack_sizes_json,
                comma
            ))?;
        }
        out.writeln("  ],")?;

        let total_writes: u64 = io_all.iter().map(|r| r.writes).sum();
        let total_write_bytes: u64 = io_all.iter().map(|r| r.write_bytes).sum();
        let total_reads: u64 = io_all.iter().map(|r| r.reads).sum();
        let total_read_bytes: u64 = io_all.iter().map(|r| r.read_bytes).sum();
        let total_found: u64 = io_all.iter().map(|r| r.exists_found).sum();
        let total_miss: u64 = io_all.iter().map(|r| r.exists_miss).sum();

        out.writeln("  \"io_totals\": {")?;
        out.writeln(&format!("    \"writes\": {},", total_writes))?;
        out.writeln(&format!("    \"write_bytes\": {},", total_write_bytes))?;
        out.writeln(&format!("    \"reads\": {},", total_reads))?;
        out.writeln(&format!("    \"read_bytes\": {},", total_read_bytes))?;
        out.writeln(&format!("    \"exists_found\": {},", total_found))?;
        out.writeln(&format!("    \"exists_miss\": {}", total_miss))?;
        out.writeln("  },")?;

        // worktree_file_histogram is always the last key.
        out.writeln(&format!(
            "  \"worktree_file_histogram\": {}",
            histogram_json_array(wt_sizes)
        ))?;

        out.writeln("}")?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify bucket_index assignments for boundary values.
    #[test]
    fn bucket_index_boundaries() {
        assert_eq!(bucket_index(0), 0, "<256B lower");
        assert_eq!(bucket_index(255), 0, "<256B upper");
        assert_eq!(bucket_index(256), 1, "256B-1KB lower");
        assert_eq!(bucket_index(1_023), 1, "256B-1KB upper");
        assert_eq!(bucket_index(1_024), 2, "1-4KB lower");
        assert_eq!(bucket_index(4_095), 2, "1-4KB upper");
        assert_eq!(bucket_index(4_096), 3, "4-16KB lower");
        assert_eq!(bucket_index(16_383), 3, "4-16KB upper");
        assert_eq!(bucket_index(16_384), 4, "16-64KB lower");
        assert_eq!(bucket_index(65_535), 4, "16-64KB upper");
        assert_eq!(bucket_index(65_536), 5, "64-256KB lower");
        assert_eq!(bucket_index(262_143), 5, "64-256KB upper");
        assert_eq!(bucket_index(262_144), 6, "256KB-1MB lower");
        assert_eq!(bucket_index(1_048_575), 6, "256KB-1MB upper");
        assert_eq!(bucket_index(1_048_576), 7, "1-4MB lower");
        assert_eq!(bucket_index(4_194_303), 7, "1-4MB upper");
        assert_eq!(bucket_index(4_194_304), 8, "4-16MB lower");
        assert_eq!(bucket_index(16_777_215), 8, "4-16MB upper");
        assert_eq!(bucket_index(16_777_216), 9, ">16MB lower");
        assert_eq!(bucket_index(u64::MAX), 9, ">16MB max");
    }

    /// Feed known sizes spanning several buckets and verify count/bytes/labels.
    #[test]
    fn size_histogram_basic() {
        // 2 files in <256B, 1 in 1-4MB, 1 in >16MB; bucket 4-16MB stays empty.
        let sizes = vec![
            100,        // <256B
            200,        // <256B
            2_000_000,  // 1-4MB
            20_000_000, // >16MB
        ];
        let hist = size_histogram(&sizes);
        assert_eq!(hist.len(), 3, "three non-empty buckets");

        // Ascending order: <256B, 1-4MB, >16MB.
        assert_eq!(hist[0].0, "<256B");
        assert_eq!(hist[0].1, 2);
        assert_eq!(hist[0].2, 300);

        assert_eq!(hist[1].0, "1-4MB");
        assert_eq!(hist[1].1, 1);
        assert_eq!(hist[1].2, 2_000_000);

        assert_eq!(hist[2].0, ">16MB");
        assert_eq!(hist[2].1, 1);
        assert_eq!(hist[2].2, 20_000_000);
    }

    /// Empty input produces empty histogram.
    #[test]
    fn size_histogram_empty_input() {
        let hist = size_histogram(&[]);
        assert!(hist.is_empty(), "empty input → empty histogram");
    }

    /// All sizes in one bucket produces exactly one row.
    #[test]
    fn size_histogram_single_bucket() {
        let sizes = vec![1_048_576, 2_000_000, 3_000_000]; // all 1-4MB
        let hist = size_histogram(&sizes);
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].0, "1-4MB");
        assert_eq!(hist[0].1, 3);
        assert_eq!(hist[0].2, 1_048_576 + 2_000_000 + 3_000_000);
    }

    /// Histogram covers all nine buckets when fed one representative value each.
    #[test]
    fn size_histogram_all_buckets() {
        let sizes = vec![
            0,          // <256B
            256,        // 256B-1KB
            1_024,      // 1-4KB
            4_096,      // 4-16KB
            16_384,     // 16-64KB
            65_536,     // 64-256KB
            262_144,    // 256KB-1MB
            1_048_576,  // 1-4MB
            4_194_304,  // 4-16MB
            16_777_216, // >16MB
        ];
        let hist = size_histogram(&sizes);
        assert_eq!(hist.len(), 10, "all ten buckets non-empty");
        // Labels must be in ascending order matching HISTOGRAM_BUCKETS.
        let expected_labels = [
            "<256B",
            "256B-1KB",
            "1-4KB",
            "4-16KB",
            "16-64KB",
            "64-256KB",
            "256KB-1MB",
            "1-4MB",
            "4-16MB",
            ">16MB",
        ];
        for (i, &(label, count, _)) in hist.iter().enumerate() {
            assert_eq!(label, expected_labels[i]);
            assert_eq!(count, 1);
        }
    }

    /// Verify `fmt_size_compact` produces the same numeric values as `fmt_size`
    /// but without spaces.
    #[test]
    fn fmt_size_compact_basic() {
        assert_eq!(fmt_size_compact(0), "0B");
        assert_eq!(fmt_size_compact(512), "512B");
        assert_eq!(fmt_size_compact(1_024), "1.0KiB");
        assert_eq!(fmt_size_compact(12_288), "12.0KiB");
        assert_eq!(fmt_size_compact(1_048_576), "1.0MiB");
        assert_eq!(fmt_size_compact(15_728_640), "15.0MiB");
        assert_eq!(fmt_size_compact(1_073_741_824), "1.0GiB");
        assert_eq!(fmt_size_compact(1_503_238_553), "1.4GiB");
    }

    /// `fmt_int` groups digits with thousands separators.
    #[test]
    fn fmt_int_basic() {
        assert_eq!(fmt_int(0), "0");
        assert_eq!(fmt_int(999), "999");
        assert_eq!(fmt_int(1_000), "1,000");
        assert_eq!(fmt_int(1_773), "1,773");
        assert_eq!(fmt_int(1_234_567), "1,234,567");
    }

    /// Round-trip: verify that fmt_size_compact(x) == fmt_size(x).replace(" ", "")
    #[test]
    fn fmt_size_compact_matches_fmt_size() {
        let test_cases = vec![
            0,
            1,
            127,
            255,
            256,
            512,
            1023,
            1024,
            2_048,
            4_096,
            16_384,
            65_536,
            262_144,
            1_048_576,
            2_097_152,
            4_194_304,
            16_777_216,
            1_073_741_824,
            u64::MAX,
        ];
        for bytes in test_cases {
            let compact = fmt_size_compact(bytes);
            let normal = fmt_size(bytes).replace(" ", "");
            assert_eq!(compact, normal, "fmt_size_compact({}) mismatch", bytes);
        }
    }

    #[test]
    fn format_time_strips_year_and_separators() {
        assert_eq!(format_time("2026-06-13T02:05:11Z"), "06-13 02:05:11");
        assert_eq!(format_time("2025-12-31T23:59:59Z"), "12-31 23:59:59");
        assert_eq!(format_time("2026-01-01T00:00:00Z"), "01-01 00:00:00");
    }

    #[test]
    fn format_io_cell_basic() {
        assert_eq!(format_io_cell(251, 15_728_640), "251 15.0MiB");
        assert_eq!(format_io_cell(3, 12_288), "3 12.0KiB");
        assert_eq!(format_io_cell(0, 0), "0 —");
        assert_eq!(format_io_cell(12, 2_097_152), "12 2.0MiB");
    }

    #[test]
    fn format_head_cell_basic() {
        assert_eq!(format_head_cell(2, 248), "2/248");
        assert_eq!(format_head_cell(0, 0), "—");
        assert_eq!(format_head_cell(1, 40), "1/40");
    }

    #[test]
    fn format_pack_cell_basic() {
        // Pack cell: files × sum of sizes.
        assert_eq!(
            format_pack_cell(3, &[5_242_880, 5_242_880, 3_145_728]),
            "3×13.0MiB"
        );
        assert_eq!(format_pack_cell(0, &[]), "—");
        assert_eq!(format_pack_cell(2, &[1_048_576, 1_048_576]), "2×2.0MiB");
    }
}
