use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::dtimer_l1;
use crate::error::Error;
use crate::object::Hash;
use crate::store::ObjectStore;
use crate::store::local::{atomic_write_no_fsync, sync_local_objects_fs};

// ---------------------------------------------------------------------------
// Entry
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct StatCacheEntry {
    pub mtime_secs: i64,
    pub mtime_nanos: u32,
    pub fs_size: u64,
    pub hash: Hash,
    pub is_unsafe: bool,
    /// Set once a blob-writing scan has written or verified the local object
    /// for `hash` (design/07 "Seal invalidation"). Lets a later blob-writing
    /// scan skip a redundant `store.exists()` check when seal trust holds.
    pub sealed: bool,
}

// ---------------------------------------------------------------------------
// v1 binary format constants
// ---------------------------------------------------------------------------

const MAGIC: &[u8; 4] = b"OSTC";
const VERSION: u32 = 1;
const HEADER_LEN: usize = 32;
const INDEX_ENTRY_LEN: usize = 8;
const DATA_ENTRY_LEN: usize = 56; // i64 + u32 + u64 + [u8;32] + u32
const DIGEST_LEN: usize = 32; // also the TRAILER size (design/07 "TRAILER")
const PATHS_ALIGN: usize = 8;
const UNSAFE_FLAG: u32 = 0x0001;
const SEALED_FLAG: u32 = 0x0002;

/// Length of the `seal_epoch` sub-field within the 32-byte TRAILER; the
/// remaining `DIGEST_LEN - SEAL_EPOCH_LEN` bytes stay reserved/zero
/// (design/07 "TRAILER").
const SEAL_EPOCH_LEN: usize = 16;
/// All-zero epoch: "no epoch recorded" (design/07 "TRAILER").
const NO_SEAL_EPOCH: [u8; SEAL_EPOCH_LEN] = [0u8; SEAL_EPOCH_LEN];
/// Default sample size for `OMEMFS_SEAL_SAMPLE` (design/07 "Sampling verification").
const DEFAULT_SEAL_SAMPLE: usize = 256;
/// Dotfile marker inside `.omemfs/objects/`, at the same level as the existing
/// `.depth` / `.migrating` markers (design/07 "Seal epoch").
const SEAL_EPOCH_MARKER_FILE: &str = ".seal_epoch";

/// Racy-window threshold in seconds. A file whose mtime is within this many
/// seconds of "now" is treated as racily clean (its hash cannot be trusted as
/// a cache hit) and the clone-root fallback in `scan_dir` likewise refuses to
/// skip hashing it. Covers FAT32's 2-second mtime granularity plus margin.
/// This is the single source of truth shared by both the STAT_CACHE racy-clean
/// detection (`is_racily_clean`) and `scan::can_skip_hash` (design/07).
pub const RACY_THRESHOLD_SECS: u64 = 3;

// ---------------------------------------------------------------------------
// StatCache
// ---------------------------------------------------------------------------

/// Persistent mtime-keyed cache mapping repo-relative paths to blob hashes.
/// Used to skip re-reading and re-hashing unchanged files during working-tree scans.
#[derive(Debug, Default)]
pub struct StatCache {
    entries: HashMap<String, StatCacheEntry>,
    /// True when `entries` differs from the on-disk state loaded by `read`
    /// (or from the empty default). Cleared after a successful `write`. Used to
    /// skip rewriting the cache file when nothing changed (design/07
    /// "Cache hits do not trigger a writeback unless something changed").
    dirty: bool,
    /// Opaque epoch ID from the TRAILER, copied from `.omemfs/objects/.seal_epoch`
    /// at the time this cache was last written by a blob-writing scan.
    /// All-zero means "no epoch recorded" (design/07 "TRAILER").
    seal_epoch: [u8; SEAL_EPOCH_LEN],
    /// Computed once by [`compute_seals_trusted`](Self::compute_seals_trusted)
    /// for a blob-writing scan; `false` until computed (safe default: every
    /// `sealed` entry falls back to a real `store.exists()` check, exactly as
    /// before this feature existed). Read-only scans never compute this and
    /// never consult it (design/07 "Implementation notes").
    seals_trusted: bool,
    /// Set by `compute_seals_trusted` when the loaded `seal_epoch` did not
    /// match the on-disk marker (or no epoch was recorded at all). Consumed by
    /// [`finalize_seal_epoch`](Self::finalize_seal_epoch), which then
    /// generates and persists a fresh marker ID (design/07 "Seal epoch").
    needs_new_epoch: bool,
}

impl StatCache {
    /// Load from `.omemfs/STAT_CACHE`. Returns an empty cache on any error
    /// (absent file, corrupt data, version mismatch) — the cache is purely
    /// an acceleration layer, so a cold cache is always safe.
    pub fn read(omemfs_dir: &Path) -> Self {
        let _t = dtimer_l1!("stat_cache read");
        let path = omemfs_dir.join("STAT_CACHE");
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(_) => return Self::default(),
        };
        parse_v1(&bytes).unwrap_or_default()
    }

    /// Load only the entries within `scope_prefix` from `.omemfs/STAT_CACHE`.
    ///
    /// `scope_prefix` is a repo-relative forward-slash path with no leading or
    /// trailing slash (e.g. `src` or `src/codec`). An empty prefix means the
    /// repository root and delegates to the full [`read`](Self::read).
    ///
    /// The INDEX section is sorted by raw path bytes, so all in-scope entries
    /// (the prefix itself plus its `prefix + "/"` descendants) form a single
    /// contiguous run. This uses a lower-bound binary search to locate that run
    /// and decodes only that slice, leaving the rest of the file unread.
    /// Siblings such as `foo.txt` are *not* included for scope `foo` — only
    /// `foo` and `foo/...` match (design/07 "Read optimisation").
    ///
    /// Any error (absent file, corruption) yields an empty cache, exactly like
    /// [`read`](Self::read): the cache is purely an acceleration layer.
    pub fn read_scoped(omemfs_dir: &Path, scope_prefix: &str) -> Self {
        if scope_prefix.is_empty() {
            return Self::read(omemfs_dir);
        }
        // Timer placed after the empty-prefix delegation so the root scope is
        // not double-counted: `read` carries its own "stat_cache read" span.
        let _t = dtimer_l1!("stat_cache read");
        let path = omemfs_dir.join("STAT_CACHE");
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(_) => return Self::default(),
        };
        parse_v1_scoped(&bytes, scope_prefix).unwrap_or_default()
    }

    /// Write to `.omemfs/STAT_CACHE` atomically (temp file + rename, no fsync).
    /// Issues a durability barrier first so that STAT_CACHE never references
    /// local cache objects that have not yet reached stable storage.
    ///
    /// Always writes, regardless of the dirty flag. On success the dirty flag
    /// is cleared. Callers that want to skip a clean write should use
    /// [`StatCache::write_if_dirty`].
    pub fn write(&mut self, omemfs_dir: &Path) -> Result<(), Error> {
        let _t = dtimer_l1!("stat_cache write");
        sync_local_objects_fs(&omemfs_dir.join("objects"))?;
        let path = omemfs_dir.join("STAT_CACHE");
        let bytes = encode_v1(&self.entries, &self.seal_epoch)?;
        atomic_write_no_fsync(&path, &bytes)?;
        self.dirty = false;
        Ok(())
    }

    /// Write the cache only if it has been modified since it was loaded.
    /// Returns `Ok(true)` if a write was performed, `Ok(false)` if the cache
    /// was clean and the write was skipped (design/07: a no-change scan must
    /// not rewrite STAT_CACHE).
    pub fn write_if_dirty(&mut self, omemfs_dir: &Path) -> Result<bool, Error> {
        if !self.dirty {
            return Ok(false);
        }
        self.write(omemfs_dir)?;
        Ok(true)
    }

    /// Returns true if the cache has unsaved modifications.
    #[allow(dead_code)]
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Merge the in-scope entries held by `self` (loaded via
    /// [`read_scoped`](Self::read_scoped) and possibly updated during a scoped
    /// scan) back into the full on-disk cache, then write the merged result.
    ///
    /// The full file is re-read fresh so that out-of-scope entries survive
    /// byte-for-byte. The in-scope entries from `self` overlay the freshly read
    /// ones. The write is skipped entirely when `self` is clean (no in-scope
    /// change since the scoped load), preserving the no-change-scan optimisation
    /// (design/07).
    ///
    /// `scope_prefix` must be the same prefix passed to `read_scoped`.
    pub fn write_scoped_merge(
        &mut self,
        omemfs_dir: &Path,
        scope_prefix: &str,
    ) -> Result<bool, Error> {
        if scope_prefix.is_empty() {
            // Root scope: `self` already holds the whole cache.
            return self.write_if_dirty(omemfs_dir);
        }
        if !self.dirty {
            // Nothing changed in scope: leave the on-disk file untouched.
            return Ok(false);
        }
        // Reload the full file fresh and overlay the in-scope entries. Any
        // out-of-scope entry is left exactly as it was on disk.
        let mut full = Self::read(omemfs_dir);
        for (path, entry) in &self.entries {
            full.entries.insert(path.clone(), entry.clone());
        }
        // Carry `self`'s seal_epoch forward, not the freshly re-read one: a
        // blob-writing scoped scan may have just rotated it via
        // `finalize_seal_epoch`, and that rotation must win over the (now
        // stale) on-disk value (design/07 "Seal epoch"). For every other
        // caller (no rotation happened) the two values are already identical,
        // so this is a no-op overwrite.
        full.seal_epoch = self.seal_epoch;
        full.dirty = true;
        full.write(omemfs_dir)?;
        self.dirty = false;
        Ok(true)
    }

    /// Return the cached hash for `path` if `(mtime, fs_size)` match and the
    /// entry is not unsafe. The caller determines whether the returned hash
    /// matches the clone root to decide modified/unchanged status.
    pub fn lookup_current(&self, path: &str, mtime: SystemTime, fs_size: u64) -> Option<&Hash> {
        self.lookup_current_entry(path, mtime, fs_size)
            .map(|entry| &entry.hash)
    }

    /// Like [`lookup_current`](Self::lookup_current), but returns the whole
    /// entry (including `sealed`) instead of just the hash. `SEALED` and
    /// `UNSAFE` are independent bits (design/07 "flags bit assignments"): an
    /// unsafe entry is filtered out here exactly like `lookup_current`, so the
    /// `sealed` bit of an unsafe entry is never observed by a caller.
    pub fn lookup_current_entry(
        &self,
        path: &str,
        mtime: SystemTime,
        fs_size: u64,
    ) -> Option<&StatCacheEntry> {
        let entry = self.entries.get(path)?;
        if entry.is_unsafe {
            return None;
        }
        if entry.fs_size != fs_size {
            return None;
        }
        let (secs, nanos) = systemtime_to_parts(mtime);
        if entry.mtime_secs != secs || entry.mtime_nanos != nanos {
            return None;
        }
        Some(entry)
    }

    /// Insert or replace an entry. Sets `is_unsafe = true` when the file's
    /// mtime falls within the current second (racily clean). `sealed` is set
    /// directly from the caller (design/07 "Update": `sealed = write_blobs`
    /// for the scan that produced this entry).
    pub fn update(&mut self, path: String, mtime: SystemTime, fs_size: u64, hash: Hash, sealed: bool) {
        let (mtime_secs, mtime_nanos) = systemtime_to_parts(mtime);
        let is_unsafe = is_racily_clean(mtime_secs, SystemTime::now());
        let new_entry = StatCacheEntry {
            mtime_secs,
            mtime_nanos,
            fs_size,
            hash,
            is_unsafe,
            sealed,
        };
        // Only mark the cache dirty when the entry is genuinely new or changed,
        // so a no-change scan leaves the cache clean and skips the writeback.
        let changed = match self.entries.get(&path) {
            Some(existing) => !entry_eq(existing, &new_entry),
            None => true,
        };
        if changed {
            self.dirty = true;
        }
        self.entries.insert(path, new_entry);
    }

    /// Set the `SEALED` bit on an existing entry, leaving every other field
    /// untouched (design/07 "Update"). Returns `true` (and marks the cache
    /// dirty) only when the bit was not already set; a no-op on an
    /// already-sealed entry or a missing path returns `false` and leaves the
    /// cache clean.
    pub fn mark_sealed(&mut self, path: &str) -> bool {
        match self.entries.get_mut(path) {
            Some(entry) if !entry.sealed => {
                entry.sealed = true;
                self.dirty = true;
                true
            }
            _ => false,
        }
    }

    /// Clear the `SEALED` bit on every entry (design/07 "Upload-failure
    /// recovery"): hashes and every other field are left untouched. Returns
    /// `true` (and marks the cache dirty) if any bit was actually cleared.
    pub fn clear_all_sealed(&mut self) -> bool {
        let mut changed = false;
        for entry in self.entries.values_mut() {
            if entry.sealed {
                entry.sealed = false;
                changed = true;
            }
        }
        if changed {
            self.dirty = true;
        }
        changed
    }

    #[allow(dead_code)]
    pub fn remove(&mut self, path: &str) {
        if self.entries.remove(path).is_some() {
            self.dirty = true;
        }
    }

    #[allow(dead_code)]
    pub fn contains(&self, path: &str) -> bool {
        self.entries.contains_key(path)
    }

    /// Whether this entry (if present, not-unsafe, and matching `sealed`) is
    /// currently sealed. Test/diagnostic helper.
    #[allow(dead_code)]
    pub fn is_sealed(&self, path: &str) -> bool {
        self.entries.get(path).map(|e| e.sealed).unwrap_or(false)
    }

    // -----------------------------------------------------------------------
    // Seal invalidation (design/07 "Seal invalidation")
    // -----------------------------------------------------------------------

    /// Compute whether `sealed` entries in this cache can be trusted without a
    /// fresh `store.exists()` check, for the blob-writing scan that just
    /// loaded this cache. Must be called once, right after loading (before any
    /// scanning), and only for a `write_blobs = true` scan — `seals_trusted()`
    /// defaults to `false` (safe) until this is called.
    ///
    /// Two conditions must both hold:
    /// 1. **Epoch match**: `seal_epoch` is non-zero and equals the current
    ///    `.omemfs/objects/.seal_epoch` marker. On a mismatch (including a
    ///    missing marker file, or a zero/never-recorded epoch), trust is
    ///    withheld for this whole scan and [`finalize_seal_epoch`] will
    ///    generate and persist a fresh marker at the end of the scan.
    /// 2. **Sampling**: when the epoch matches, a fixed-stride sample of up to
    ///    `OMEMFS_SEAL_SAMPLE` (default 256; `0` disables sampling) `sealed`
    ///    entries is checked against `store.exists()`. Any single miss
    ///    downgrades trust to `false` for this scan (but does not trigger an
    ///    epoch rotation — see design/07 "Sampling verification").
    pub fn compute_seals_trusted(&mut self, omemfs_dir: &Path, store: &dyn ObjectStore) {
        let marker = read_seal_epoch_marker(omemfs_dir);
        let epoch_matches = self.seal_epoch != NO_SEAL_EPOCH && marker == Some(self.seal_epoch);
        if !epoch_matches {
            self.seals_trusted = false;
            self.needs_new_epoch = true;
            return;
        }
        self.needs_new_epoch = false;
        self.seals_trusted = self.sample_verify(store);
    }

    /// Result of the most recent [`compute_seals_trusted`] call. Defaults to
    /// `false` (never computed / read-only scan), which is always safe: every
    /// `sealed` entry then falls back to a real `store.exists()` check.
    pub fn seals_trusted(&self) -> bool {
        self.seals_trusted
    }

    /// Deterministic fixed-stride sample of up to `OMEMFS_SEAL_SAMPLE` sealed
    /// entries (sorted by path, so the check is reproducible in tests — no
    /// RNG). Returns `false` on the first sampled object found missing;
    /// `true` if every sampled object exists (including the vacuous case of
    /// no sealed entries, or sampling disabled via `OMEMFS_SEAL_SAMPLE=0`).
    fn sample_verify(&self, store: &dyn ObjectStore) -> bool {
        let sample_max = seal_sample_size();
        if sample_max == 0 {
            return true;
        }
        let mut sealed_paths: Vec<&String> = self
            .entries
            .iter()
            .filter(|(_, e)| e.sealed)
            .map(|(p, _)| p)
            .collect();
        if sealed_paths.is_empty() {
            return true;
        }
        sealed_paths.sort();

        let n = sealed_paths.len();
        let sample_n = sample_max.min(n);
        let stride = n / sample_n; // >= 1, since sample_n <= n
        for k in 0..sample_n {
            let path = sealed_paths[k * stride];
            let entry = &self.entries[path];
            match store.exists(&entry.hash) {
                Ok(true) => {}
                _ => return false,
            }
        }
        true
    }

    /// Generate and persist a fresh seal epoch when [`compute_seals_trusted`]
    /// found a whole-epoch mismatch, and record it in this cache's TRAILER.
    /// Forces a write even when no entry changed (design/07 "Seal epoch": the
    /// new epoch must be persisted so future scans can trust it).
    ///
    /// Only meaningful for a `write_blobs = true` scan; a read-only scan must
    /// never call this — it carries the loaded `seal_epoch` forward unchanged
    /// instead (design/07 "Implementation notes").
    pub fn finalize_seal_epoch(&mut self, omemfs_dir: &Path) {
        if !self.needs_new_epoch {
            return;
        }
        let new_epoch = random_seal_epoch();
        // Best-effort: a failure to persist the marker is not fatal (STAT_CACHE
        // is a pure acceleration cache); the next scan will simply observe a
        // mismatch again and retry.
        let _ = write_seal_epoch_marker(omemfs_dir, &new_epoch);
        self.seal_epoch = new_epoch;
        self.dirty = true;
        self.needs_new_epoch = false;
    }
}

// ---------------------------------------------------------------------------
// Seal epoch marker file (`.omemfs/objects/.seal_epoch`)
// ---------------------------------------------------------------------------

fn seal_epoch_marker_path(omemfs_dir: &Path) -> PathBuf {
    omemfs_dir.join("objects").join(SEAL_EPOCH_MARKER_FILE)
}

/// Read the current marker ID, or `None` if absent/corrupt (wrong length) —
/// both treated as "no marker" (design/07 "Seal epoch").
fn read_seal_epoch_marker(omemfs_dir: &Path) -> Option<[u8; SEAL_EPOCH_LEN]> {
    let bytes = fs::read(seal_epoch_marker_path(omemfs_dir)).ok()?;
    bytes.try_into().ok()
}

/// Overwrite (or create) the marker file with a fresh epoch ID. `objects/` is
/// created if it does not exist yet (mirrors the lazy creation of `.depth` /
/// `.migrating`).
fn write_seal_epoch_marker(omemfs_dir: &Path, epoch: &[u8; SEAL_EPOCH_LEN]) -> Result<(), Error> {
    let path = seal_epoch_marker_path(omemfs_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    atomic_write_no_fsync(&path, epoch)
}

/// Generate a fresh 16-byte random epoch ID.
fn random_seal_epoch() -> [u8; SEAL_EPOCH_LEN] {
    use rand::RngCore;
    let mut buf = [0u8; SEAL_EPOCH_LEN];
    rand::rng().fill_bytes(&mut buf);
    buf
}

/// Serialises every test (in this module and in `scan.rs`) that reads or
/// mutates `OMEMFS_SEAL_SAMPLE` -- a single process-global env var -- so
/// parallel test runs cannot race each other. Shared across modules (rather
/// than one static per module) because two independent `Mutex`es would not
/// actually synchronise access to the same underlying env var.
#[cfg(test)]
pub(crate) static SEAL_SAMPLE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Resolve `OMEMFS_SEAL_SAMPLE` (default `DEFAULT_SEAL_SAMPLE`; `0` disables
/// sampling). An unset or unparseable value falls back to the default.
fn seal_sample_size() -> usize {
    match std::env::var("OMEMFS_SEAL_SAMPLE") {
        Ok(v) => v.trim().parse::<usize>().unwrap_or(DEFAULT_SEAL_SAMPLE),
        Err(_) => DEFAULT_SEAL_SAMPLE,
    }
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

fn encode_v1(
    entries: &HashMap<String, StatCacheEntry>,
    seal_epoch: &[u8; SEAL_EPOCH_LEN],
) -> Result<Vec<u8>, Error> {
    let mut sorted: Vec<(&String, &StatCacheEntry)> = entries.iter().collect();
    sorted.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));

    let entry_count = sorted.len();
    let index_offset = HEADER_LEN;
    let index_size = entry_count * INDEX_ENTRY_LEN;
    let paths_offset = index_offset + index_size;

    let total_path_len: usize = sorted.iter().map(|(p, _)| p.len()).sum();
    let paths_padded = (total_path_len + PATHS_ALIGN - 1) & !(PATHS_ALIGN - 1);
    let data_offset = paths_offset + paths_padded;
    let data_size = entry_count * DATA_ENTRY_LEN;
    let body_end = data_offset + data_size;

    let mut body: Vec<u8> = Vec::with_capacity(body_end + DIGEST_LEN);

    // HEADER
    body.extend_from_slice(MAGIC);
    body.extend_from_slice(&VERSION.to_be_bytes());
    body.extend_from_slice(&(entry_count as u32).to_be_bytes());
    body.extend_from_slice(&0u32.to_be_bytes()); // flags
    body.extend_from_slice(&(index_offset as u32).to_be_bytes());
    body.extend_from_slice(&(paths_offset as u32).to_be_bytes());
    body.extend_from_slice(&(data_offset as u32).to_be_bytes());
    body.extend_from_slice(&0u32.to_be_bytes()); // reserved

    // INDEX
    let mut path_cursor: u32 = 0;
    for (path, _) in &sorted {
        let len = path.len() as u32;
        body.extend_from_slice(&path_cursor.to_be_bytes());
        body.extend_from_slice(&len.to_be_bytes());
        path_cursor = path_cursor
            .checked_add(len)
            .ok_or_else(|| Error::Other("STAT_CACHE: path section overflow".to_string()))?;
    }

    // PATHS
    for (path, _) in &sorted {
        body.extend_from_slice(path.as_bytes());
    }
    let pad = paths_padded - total_path_len;
    body.extend(std::iter::repeat_n(0u8, pad));

    // DATA
    for (_, entry) in &sorted {
        let mut flags: u32 = 0;
        if entry.is_unsafe {
            flags |= UNSAFE_FLAG;
        }
        if entry.sealed {
            flags |= SEALED_FLAG;
        }
        body.extend_from_slice(&entry.mtime_secs.to_be_bytes());
        body.extend_from_slice(&entry.mtime_nanos.to_be_bytes());
        body.extend_from_slice(&entry.fs_size.to_be_bytes());
        body.extend_from_slice(entry.hash.as_bytes_array());
        body.extend_from_slice(&flags.to_be_bytes());
    }

    // TRAILER: seal_epoch (16 bytes) + reserved (16 bytes, zero-filled).
    body.extend_from_slice(seal_epoch);
    body.extend(std::iter::repeat_n(0u8, DIGEST_LEN - SEAL_EPOCH_LEN));

    Ok(body)
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

fn parse_v1(bytes: &[u8]) -> Option<StatCache> {
    if bytes.len() < HEADER_LEN + DIGEST_LEN {
        return None;
    }
    if &bytes[0..4] != MAGIC {
        return None;
    }
    let version = u32::from_be_bytes(bytes[4..8].try_into().ok()?);
    if version != VERSION {
        return None;
    }
    let entry_count = u32::from_be_bytes(bytes[8..12].try_into().ok()?) as usize;
    let index_offset = u32::from_be_bytes(bytes[16..20].try_into().ok()?) as usize;
    let paths_offset = u32::from_be_bytes(bytes[20..24].try_into().ok()?) as usize;
    let data_offset = u32::from_be_bytes(bytes[24..28].try_into().ok()?) as usize;

    if index_offset != HEADER_LEN {
        return None;
    }
    let index_size = entry_count.checked_mul(INDEX_ENTRY_LEN)?;
    let index_end = index_offset.checked_add(index_size)?;
    if index_end > paths_offset || paths_offset > data_offset {
        return None;
    }
    let paths_size = data_offset - paths_offset;
    let data_size = entry_count.checked_mul(DATA_ENTRY_LEN)?;
    let body_end = data_offset.checked_add(data_size)?;
    if body_end + DIGEST_LEN > bytes.len() {
        return None;
    }

    let mut entries = HashMap::with_capacity(entry_count);
    for i in 0..entry_count {
        let idx = index_offset + i * INDEX_ENTRY_LEN;
        let path_offset = u32::from_be_bytes(bytes[idx..idx + 4].try_into().ok()?) as usize;
        let path_len = u32::from_be_bytes(bytes[idx + 4..idx + 8].try_into().ok()?) as usize;
        if path_offset + path_len > paths_size {
            continue;
        }
        let path_bytes = &bytes[paths_offset + path_offset..paths_offset + path_offset + path_len];
        let path = std::str::from_utf8(path_bytes).ok()?.to_string();

        let off = data_offset + i * DATA_ENTRY_LEN;
        let mtime_secs = i64::from_be_bytes(bytes[off..off + 8].try_into().ok()?);
        let mtime_nanos = u32::from_be_bytes(bytes[off + 8..off + 12].try_into().ok()?);
        let fs_size = u64::from_be_bytes(bytes[off + 12..off + 20].try_into().ok()?);
        let hash_bin: [u8; 32] = bytes[off + 20..off + 52].try_into().ok()?;
        let flags = u32::from_be_bytes(bytes[off + 52..off + 56].try_into().ok()?);
        let hash = Hash::from_bytes(hash_bin);
        let is_unsafe = (flags & UNSAFE_FLAG) != 0;
        let sealed = (flags & SEALED_FLAG) != 0;

        entries.insert(
            path,
            StatCacheEntry {
                mtime_secs,
                mtime_nanos,
                fs_size,
                hash,
                is_unsafe,
                sealed,
            },
        );
    }

    // TRAILER: seal_epoch is the first SEAL_EPOCH_LEN bytes; already validated
    // in range by the `body_end + DIGEST_LEN > bytes.len()` check above.
    let seal_epoch: [u8; SEAL_EPOCH_LEN] = bytes[body_end..body_end + SEAL_EPOCH_LEN]
        .try_into()
        .ok()?;

    // A freshly loaded cache reflects the on-disk state exactly: not dirty.
    Some(StatCache {
        entries,
        dirty: false,
        seal_epoch,
        seals_trusted: false,
        needs_new_epoch: false,
    })
}

/// Parse only the entries within `scope_prefix` using a binary search over the
/// sorted INDEX. Returns `None` (treated as an empty cache) on any structural
/// problem, exactly like `parse_v1`.
fn parse_v1_scoped(bytes: &[u8], scope_prefix: &str) -> Option<StatCache> {
    if bytes.len() < HEADER_LEN + DIGEST_LEN {
        return None;
    }
    if &bytes[0..4] != MAGIC {
        return None;
    }
    let version = u32::from_be_bytes(bytes[4..8].try_into().ok()?);
    if version != VERSION {
        return None;
    }
    let entry_count = u32::from_be_bytes(bytes[8..12].try_into().ok()?) as usize;
    let index_offset = u32::from_be_bytes(bytes[16..20].try_into().ok()?) as usize;
    let paths_offset = u32::from_be_bytes(bytes[20..24].try_into().ok()?) as usize;
    let data_offset = u32::from_be_bytes(bytes[24..28].try_into().ok()?) as usize;

    if index_offset != HEADER_LEN {
        return None;
    }
    let index_size = entry_count.checked_mul(INDEX_ENTRY_LEN)?;
    let index_end = index_offset.checked_add(index_size)?;
    if index_end > paths_offset || paths_offset > data_offset {
        return None;
    }
    let paths_size = data_offset - paths_offset;
    let data_size = entry_count.checked_mul(DATA_ENTRY_LEN)?;
    let body_end = data_offset.checked_add(data_size)?;
    if body_end + DIGEST_LEN > bytes.len() {
        return None;
    }

    // Helper: read the path bytes of INDEX entry `i`. Returns None on structural
    // corruption of that index entry.
    let path_bytes_at = |i: usize| -> Option<&[u8]> {
        let idx = index_offset + i * INDEX_ENTRY_LEN;
        let path_offset = u32::from_be_bytes(bytes[idx..idx + 4].try_into().ok()?) as usize;
        let path_len = u32::from_be_bytes(bytes[idx + 4..idx + 8].try_into().ok()?) as usize;
        if path_offset + path_len > paths_size {
            return None;
        }
        Some(&bytes[paths_offset + path_offset..paths_offset + path_offset + path_len])
    };

    // Lower bound: index of the first entry whose path bytes are >= `key`.
    // A corrupt index entry compares as "greater" (we cannot read it), which is
    // safe: the worst case is loading slightly too wide a slice, and the
    // per-entry decode below drops any entry it cannot read.
    let lower_bound = |key: &[u8]| -> usize {
        let mut lo = 0usize;
        let mut hi = entry_count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let cmp_less = match path_bytes_at(mid) {
                Some(p) => p < key,
                None => false,
            };
            if cmp_less {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    };

    // In-scope entries are the prefix itself plus its `prefix + "/"` children.
    // Since the INDEX is byte-sorted, those form the contiguous run
    // [lower_bound(prefix), lower_bound(prefix_upper)) where `prefix_upper` is
    // `prefix` with its last byte incremented. This upper bound excludes
    // siblings such as `foo.txt` for scope `foo` ('.' = 0x2E < '/' = 0x2F, and
    // any byte > '/' makes a non-descendant): only `foo` and `foo/...` match.
    let start = lower_bound(scope_prefix.as_bytes());
    let mut prefix_upper = scope_prefix.as_bytes().to_vec();
    // Increment the last byte. A path string is never empty here (empty prefix
    // is handled by the caller), and 0xFF would overflow; treat overflow as
    // "no upper bound" (scan to the end) for safety, which is still correct.
    let end = match prefix_upper.last_mut() {
        Some(b) if *b < 0xFF => {
            *b += 1;
            lower_bound(&prefix_upper)
        }
        _ => entry_count,
    };

    let mut entries = HashMap::new();
    for i in start..end {
        let path_bytes = match path_bytes_at(i) {
            Some(p) => p,
            None => continue,
        };
        let path = match std::str::from_utf8(path_bytes) {
            Ok(s) => s,
            Err(_) => continue,
        };
        // Defensive: only keep entries that are genuinely in scope. The binary
        // search bounds are exact, but this guards against any edge case.
        if !path_in_scope(path, scope_prefix) {
            continue;
        }
        let path = path.to_string();

        let off = data_offset + i * DATA_ENTRY_LEN;
        let mtime_secs = i64::from_be_bytes(bytes[off..off + 8].try_into().ok()?);
        let mtime_nanos = u32::from_be_bytes(bytes[off + 8..off + 12].try_into().ok()?);
        let fs_size = u64::from_be_bytes(bytes[off + 12..off + 20].try_into().ok()?);
        let hash_bin: [u8; 32] = bytes[off + 20..off + 52].try_into().ok()?;
        let flags = u32::from_be_bytes(bytes[off + 52..off + 56].try_into().ok()?);
        let hash = Hash::from_bytes(hash_bin);
        let is_unsafe = (flags & UNSAFE_FLAG) != 0;
        let sealed = (flags & SEALED_FLAG) != 0;

        entries.insert(
            path,
            StatCacheEntry {
                mtime_secs,
                mtime_nanos,
                fs_size,
                hash,
                is_unsafe,
                sealed,
            },
        );
    }

    // TRAILER: a scoped read must still carry `seal_epoch` forward correctly
    // (design/07 "read_scoped/write_scoped_merge でも seal_epoch を正しく
    // 引き継ぐこと") even though only a slice of entries was decoded above.
    let seal_epoch: [u8; SEAL_EPOCH_LEN] = bytes[body_end..body_end + SEAL_EPOCH_LEN]
        .try_into()
        .ok()?;

    Some(StatCache {
        entries,
        dirty: false,
        seal_epoch,
        seals_trusted: false,
        needs_new_epoch: false,
    })
}

/// True when `path` is within the scope `prefix`: either equal to it or a
/// `prefix + "/"` descendant. `prefix` has no trailing slash.
fn path_in_scope(path: &str, prefix: &str) -> bool {
    path == prefix
        || (path.len() > prefix.len()
            && path.as_bytes()[prefix.len()] == b'/'
            && path.starts_with(prefix))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Field-wise equality of two cache entries (used to detect no-op updates).
fn entry_eq(a: &StatCacheEntry, b: &StatCacheEntry) -> bool {
    a.mtime_secs == b.mtime_secs
        && a.mtime_nanos == b.mtime_nanos
        && a.fs_size == b.fs_size
        && a.hash == b.hash
        && a.is_unsafe == b.is_unsafe
        && a.sealed == b.sealed
}

fn systemtime_to_parts(t: SystemTime) -> (i64, u32) {
    match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
        Err(e) => {
            let d = e.duration();
            let n = d.subsec_nanos();
            if n == 0 {
                (-(d.as_secs() as i64), 0)
            } else {
                (-(d.as_secs() as i64) - 1, 1_000_000_000 - n)
            }
        }
    }
}

/// Returns true when `mtime_secs` falls within the racy window relative to
/// `now`, i.e. the file was modified less than `RACY_THRESHOLD_SECS` seconds
/// ago. Such an entry must not be trusted as a cache hit (design/07
/// "Racily clean detection"). This uses the same 3-second window as
/// `scan::can_skip_hash` so the two scan paths agree.
fn is_racily_clean(mtime_secs: i64, now: SystemTime) -> bool {
    let now_secs = match now.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(_) => return true,
    };
    // mtime in the future (clock skew / pre-write) is also treated as racy.
    now_secs - mtime_secs < RACY_THRESHOLD_SECS as i64
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    fn fake_hash(seed: u8) -> Hash {
        Hash::from_bytes([seed; 32])
    }

    fn mtime_at(secs: u64, nanos: u32) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::new(secs, nanos)
    }

    #[test]
    fn roundtrip_single_entry() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut cache = StatCache::default();
        let hash = fake_hash(0xAB);
        cache.entries.insert(
            "src/main.rs".to_string(),
            StatCacheEntry {
                mtime_secs: 1_700_000_000,
                mtime_nanos: 123_456_789,
                fs_size: 4096,
                hash: hash.clone(),
                is_unsafe: false,
                sealed: false,
            },
        );
        cache.write(tmp.path()).unwrap();
        let loaded = StatCache::read(tmp.path());
        let e = loaded.entries.get("src/main.rs").unwrap();
        assert_eq!(e.mtime_secs, 1_700_000_000);
        assert_eq!(e.mtime_nanos, 123_456_789);
        assert_eq!(e.fs_size, 4096);
        assert_eq!(e.hash, hash);
        assert!(!e.is_unsafe);
    }

    #[test]
    fn roundtrip_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut cache = StatCache::default();
        cache.write(tmp.path()).unwrap();
        let loaded = StatCache::read(tmp.path());
        assert!(loaded.entries.is_empty());
    }

    #[test]
    fn lookup_current_hit() {
        let mut cache = StatCache::default();
        let hash = fake_hash(0x01);
        let mtime = mtime_at(1_700_000_000, 0);
        cache.update("a.txt".to_string(), mtime, 100, hash.clone(), false);
        // Force is_unsafe=false (mtime is old enough).
        cache.entries.get_mut("a.txt").unwrap().is_unsafe = false;
        assert_eq!(cache.lookup_current("a.txt", mtime, 100), Some(&hash));
    }

    #[test]
    fn lookup_current_miss_on_size() {
        let mut cache = StatCache::default();
        let hash = fake_hash(0x01);
        let mtime = mtime_at(1_700_000_000, 0);
        cache.update("a.txt".to_string(), mtime, 100, hash.clone(), false);
        cache.entries.get_mut("a.txt").unwrap().is_unsafe = false;
        assert_eq!(cache.lookup_current("a.txt", mtime, 99), None);
    }

    #[test]
    fn lookup_current_miss_on_mtime() {
        let mut cache = StatCache::default();
        let hash = fake_hash(0x01);
        let mtime = mtime_at(1_700_000_000, 0);
        cache.update("a.txt".to_string(), mtime, 100, hash.clone(), false);
        cache.entries.get_mut("a.txt").unwrap().is_unsafe = false;
        assert_eq!(
            cache.lookup_current("a.txt", mtime_at(1_700_000_001, 0), 100),
            None
        );
    }

    #[test]
    fn lookup_current_miss_on_unsafe() {
        let mut cache = StatCache::default();
        let hash = fake_hash(0x01);
        let mtime = mtime_at(1_700_000_000, 0);
        cache.entries.insert(
            "a.txt".to_string(),
            StatCacheEntry {
                mtime_secs: 1_700_000_000,
                mtime_nanos: 0,
                fs_size: 100,
                hash,
                is_unsafe: true,
                sealed: false,
            },
        );
        assert_eq!(cache.lookup_current("a.txt", mtime, 100), None);
    }

    #[test]
    fn lookup_current_entry_ignores_sealed_bit_of_unsafe_entry() {
        // design/07 "flags bit assignments": SEALED and UNSAFE are independent
        // bits, but lookup_current_entry filters out is_unsafe entries before
        // sealed is ever observed by a caller -- an unsafe-and-sealed entry
        // must be treated identically to an unsafe-and-unsealed one (a miss).
        let mut cache = StatCache::default();
        let mtime = mtime_at(1_700_000_000, 0);
        cache.entries.insert(
            "racy.txt".to_string(),
            StatCacheEntry {
                mtime_secs: 1_700_000_000,
                mtime_nanos: 0,
                fs_size: 100,
                hash: fake_hash(0x01),
                is_unsafe: true,
                sealed: true,
            },
        );
        assert!(cache.lookup_current_entry("racy.txt", mtime, 100).is_none());
        assert!(cache.lookup_current("racy.txt", mtime, 100).is_none());
    }

    #[test]
    fn is_racily_clean_three_second_window() {
        // now = 1_700_000_000.5s. The racy window is RACY_THRESHOLD_SECS (3s):
        // a file is racy when (now_secs - mtime_secs) < 3.
        let now = SystemTime::UNIX_EPOCH + Duration::new(1_700_000_000, 500_000_000);
        assert_eq!(RACY_THRESHOLD_SECS, 3);
        // Same second and recent seconds are racy.
        assert!(is_racily_clean(1_700_000_000, now)); // diff 0
        assert!(is_racily_clean(1_699_999_999, now)); // diff 1
        assert!(is_racily_clean(1_699_999_998, now)); // diff 2
        // Boundary: diff == 3 is outside the window (not racy).
        assert!(!is_racily_clean(1_699_999_997, now)); // diff 3
        assert!(!is_racily_clean(1_699_999_996, now)); // diff 4
        // Future mtime (negative diff) is treated as racy.
        assert!(is_racily_clean(1_700_000_001, now));
    }

    #[test]
    fn unsafe_flag_preserved_in_roundtrip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut cache = StatCache::default();
        cache.entries.insert(
            "x.bin".to_string(),
            StatCacheEntry {
                mtime_secs: 1_700_000_000,
                mtime_nanos: 0,
                fs_size: 8,
                hash: fake_hash(0xFF),
                is_unsafe: true,
                sealed: false,
            },
        );
        cache.write(tmp.path()).unwrap();
        let loaded = StatCache::read(tmp.path());
        assert!(loaded.entries["x.bin"].is_unsafe);
    }

    #[test]
    fn fresh_cache_is_not_dirty() {
        let cache = StatCache::default();
        assert!(!cache.is_dirty());
    }

    #[test]
    fn update_with_new_entry_marks_dirty() {
        let mut cache = StatCache::default();
        let mtime = mtime_at(1_700_000_000, 0);
        cache.update("a.txt".to_string(), mtime, 100, fake_hash(0x01), false);
        assert!(
            cache.is_dirty(),
            "inserting a new entry must mark the cache dirty"
        );
    }

    #[test]
    fn update_with_identical_entry_stays_clean() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut cache = StatCache::default();
        // Use an old mtime so is_unsafe is stable (false) across both updates.
        let mtime = mtime_at(1_000_000_000, 0);
        cache.update("a.txt".to_string(), mtime, 100, fake_hash(0x01), false);
        cache.write(tmp.path()).unwrap();
        assert!(!cache.is_dirty(), "write must clear the dirty flag");
        // Re-apply the exact same (mtime, size, hash): no change → stays clean.
        cache.update("a.txt".to_string(), mtime, 100, fake_hash(0x01), false);
        assert!(
            !cache.is_dirty(),
            "a no-op update must not mark the cache dirty"
        );
    }

    #[test]
    fn update_with_changed_hash_marks_dirty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut cache = StatCache::default();
        let mtime = mtime_at(1_000_000_000, 0);
        cache.update("a.txt".to_string(), mtime, 100, fake_hash(0x01), false);
        cache.write(tmp.path()).unwrap();
        assert!(!cache.is_dirty());
        cache.update("a.txt".to_string(), mtime, 100, fake_hash(0x02), false);
        assert!(cache.is_dirty(), "changed hash must mark the cache dirty");
    }

    #[test]
    fn write_if_dirty_skips_clean_cache() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut cache = StatCache::default();
        let mtime = mtime_at(1_000_000_000, 0);
        cache.update("a.txt".to_string(), mtime, 100, fake_hash(0x01), false);
        // First write: cache is dirty, so it writes and the file appears.
        assert!(cache.write_if_dirty(tmp.path()).unwrap());
        let path = tmp.path().join("STAT_CACHE");
        let mtime1 = fs::metadata(&path).unwrap().modified().unwrap();
        // Re-apply the same entry (no change), then write_if_dirty must skip.
        cache.update("a.txt".to_string(), mtime, 100, fake_hash(0x01), false);
        assert!(
            !cache.write_if_dirty(tmp.path()).unwrap(),
            "clean cache must skip write"
        );
        let mtime2 = fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(mtime1, mtime2, "skipped write must not touch the file");
    }

    #[test]
    fn remove_existing_marks_dirty_missing_stays_clean() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut cache = StatCache::default();
        let mtime = mtime_at(1_000_000_000, 0);
        cache.update("a.txt".to_string(), mtime, 100, fake_hash(0x01), false);
        cache.write(tmp.path()).unwrap();
        assert!(!cache.is_dirty());
        // Removing a missing key is a no-op.
        cache.remove("missing.txt");
        assert!(!cache.is_dirty());
        // Removing an existing key marks dirty.
        cache.remove("a.txt");
        assert!(cache.is_dirty());
    }

    #[test]
    fn missing_file_returns_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let loaded = StatCache::read(tmp.path());
        assert!(loaded.entries.is_empty());
    }

    #[test]
    fn corrupt_magic_returns_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("STAT_CACHE"), b"XXXX garbage data").unwrap();
        let loaded = StatCache::read(tmp.path());
        assert!(loaded.entries.is_empty());
    }

    // -----------------------------------------------------------------------
    // Scope-limited load (design/07 "Read optimisation")
    // -----------------------------------------------------------------------

    /// Build and persist a cache from a list of paths (old mtime → safe entries).
    fn write_cache_with_paths(dir: &Path, paths: &[&str]) {
        let mut cache = StatCache::default();
        let mtime = mtime_at(1_000_000_000, 0);
        for (i, p) in paths.iter().enumerate() {
            cache.update(p.to_string(), mtime, 100 + i as u64, fake_hash(i as u8), false);
        }
        cache.write(dir).unwrap();
    }

    fn sorted_keys(cache: &StatCache) -> Vec<String> {
        let mut v: Vec<String> = cache.entries.keys().cloned().collect();
        v.sort();
        v
    }

    #[test]
    fn read_scoped_returns_only_in_scope_slice() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_cache_with_paths(
            tmp.path(),
            &[
                "foo.txt",
                "foo/a.rs",
                "foo/b/c.rs",
                "foobar.txt",
                "src/main.rs",
            ],
        );
        let scoped = StatCache::read_scoped(tmp.path(), "foo");
        // `foo.txt` and `foobar.txt` are siblings, not descendants of `foo`.
        assert_eq!(sorted_keys(&scoped), vec!["foo/a.rs", "foo/b/c.rs"]);
    }

    #[test]
    fn read_scoped_includes_self_entry() {
        let tmp = tempfile::TempDir::new().unwrap();
        // A blob named exactly `foo` (the scope itself) must be included.
        write_cache_with_paths(tmp.path(), &["foo", "foo/a.rs", "other"]);
        let scoped = StatCache::read_scoped(tmp.path(), "foo");
        assert_eq!(sorted_keys(&scoped), vec!["foo", "foo/a.rs"]);
    }

    #[test]
    fn read_scoped_nested_prefix() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_cache_with_paths(
            tmp.path(),
            &[
                "src/a.rs",
                "src/codec/x.rs",
                "src/codec/y.rs",
                "src/main.rs",
            ],
        );
        let scoped = StatCache::read_scoped(tmp.path(), "src/codec");
        assert_eq!(
            sorted_keys(&scoped),
            vec!["src/codec/x.rs", "src/codec/y.rs"]
        );
    }

    #[test]
    fn read_scoped_no_match_is_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_cache_with_paths(tmp.path(), &["a/x.rs", "b/y.rs"]);
        let scoped = StatCache::read_scoped(tmp.path(), "c");
        assert!(scoped.entries.is_empty());
    }

    #[test]
    fn read_scoped_deeper_than_any_entry_is_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_cache_with_paths(tmp.path(), &["src/main.rs"]);
        let scoped = StatCache::read_scoped(tmp.path(), "src/main.rs/deeper");
        assert!(scoped.entries.is_empty());
    }

    #[test]
    fn read_scoped_empty_prefix_reads_full() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_cache_with_paths(tmp.path(), &["a.txt", "b/c.txt"]);
        let scoped = StatCache::read_scoped(tmp.path(), "");
        assert_eq!(sorted_keys(&scoped), vec!["a.txt", "b/c.txt"]);
    }

    #[test]
    fn read_scoped_missing_file_is_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let scoped = StatCache::read_scoped(tmp.path(), "src");
        assert!(scoped.entries.is_empty());
    }

    #[test]
    fn read_scoped_sibling_boundary() {
        // Bytes around '/': '.' (0x2E) and '0' (0x30) bracket '/' (0x2F).
        // Only `foo/...` must match scope `foo`.
        let tmp = tempfile::TempDir::new().unwrap();
        write_cache_with_paths(tmp.path(), &["foo-x", "foo.x", "foo/x", "foo0x", "fooz"]);
        let scoped = StatCache::read_scoped(tmp.path(), "foo");
        assert_eq!(sorted_keys(&scoped), vec!["foo/x"]);
    }

    #[test]
    fn write_scoped_merge_preserves_out_of_scope() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_cache_with_paths(tmp.path(), &["dirA/a.rs", "dirB/b.rs"]);
        // Capture dirB/b.rs's original on-disk entry.
        let before = StatCache::read(tmp.path());
        let b_before = before.entries["dirB/b.rs"].clone();

        // Scoped-load dirA, change its entry, write back via merge.
        let mut scoped = StatCache::read_scoped(tmp.path(), "dirA");
        assert_eq!(sorted_keys(&scoped), vec!["dirA/a.rs"]);
        let mtime = mtime_at(1_000_000_000, 0);
        scoped.update("dirA/a.rs".to_string(), mtime, 999, fake_hash(0x77), false);
        assert!(scoped.write_scoped_merge(tmp.path(), "dirA").unwrap());

        // dirB/b.rs must survive untouched; dirA/a.rs must reflect the update.
        let after = StatCache::read(tmp.path());
        assert!(
            entry_eq(&after.entries["dirB/b.rs"], &b_before),
            "out-of-scope entry must survive byte-for-byte"
        );
        assert_eq!(after.entries["dirA/a.rs"].fs_size, 999);
        assert_eq!(after.entries["dirA/a.rs"].hash, fake_hash(0x77));
    }

    #[test]
    fn write_scoped_merge_no_change_skips_write() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_cache_with_paths(tmp.path(), &["dirA/a.rs", "dirB/b.rs"]);
        let path = tmp.path().join("STAT_CACHE");
        let mtime1 = fs::metadata(&path).unwrap().modified().unwrap();

        let mut scoped = StatCache::read_scoped(tmp.path(), "dirA");
        // Re-apply the identical entry: no in-scope change.
        let mtime = mtime_at(1_000_000_000, 0);
        scoped.update("dirA/a.rs".to_string(), mtime, 100, fake_hash(0x00), false);
        assert!(!scoped.is_dirty(), "no-op scoped update must stay clean");
        assert!(
            !scoped.write_scoped_merge(tmp.path(), "dirA").unwrap(),
            "clean scoped cache must skip the write"
        );

        let mtime2 = fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(
            mtime1, mtime2,
            "skipped scoped write must not touch the file"
        );
    }

    #[test]
    fn write_scoped_merge_adds_new_in_scope_entry() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_cache_with_paths(tmp.path(), &["dirA/a.rs", "dirB/b.rs"]);
        let mut scoped = StatCache::read_scoped(tmp.path(), "dirA");
        let mtime = mtime_at(1_000_000_000, 0);
        scoped.update("dirA/new.rs".to_string(), mtime, 42, fake_hash(0x55), false);
        assert!(scoped.write_scoped_merge(tmp.path(), "dirA").unwrap());

        let after = StatCache::read(tmp.path());
        assert!(after.entries.contains_key("dirA/new.rs"));
        assert!(after.entries.contains_key("dirA/a.rs"));
        assert!(after.entries.contains_key("dirB/b.rs"));
    }

    #[test]
    fn path_in_scope_cases() {
        assert!(path_in_scope("foo", "foo"));
        assert!(path_in_scope("foo/a", "foo"));
        assert!(path_in_scope("foo/a/b", "foo"));
        assert!(!path_in_scope("foo.txt", "foo"));
        assert!(!path_in_scope("foobar", "foo"));
        assert!(!path_in_scope("fo", "foo"));
        assert!(!path_in_scope("other", "foo"));
    }

    // -----------------------------------------------------------------------
    // SEALED flag (design/07 "Seal invalidation")
    // -----------------------------------------------------------------------

    use crate::store::local::LocalStore;

    #[test]
    fn sealed_flag_roundtrips() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut cache = StatCache::default();
        let mtime = mtime_at(1_000_000_000, 0);
        cache.update("a.txt".to_string(), mtime, 100, fake_hash(0x01), true);
        cache.update("b.txt".to_string(), mtime, 100, fake_hash(0x02), false);
        cache.write(tmp.path()).unwrap();

        let loaded = StatCache::read(tmp.path());
        assert!(loaded.entries["a.txt"].sealed, "sealed=true must roundtrip");
        assert!(
            !loaded.entries["b.txt"].sealed,
            "sealed=false must roundtrip"
        );
    }

    #[test]
    fn existing_v1_file_with_zero_flags_reads_sealed_false() {
        // A cache written before the SEALED bit existed has flags = 0 for every
        // entry (bit 1 among them). Reading it must decode sealed = false and
        // must not spuriously mark anything dirty.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut cache = StatCache::default();
        let mtime = mtime_at(1_000_000_000, 0);
        // sealed=false already produces flags bit 1 = 0; this simulates the
        // pre-existing v1 on-disk format exactly.
        cache.update("old.txt".to_string(), mtime, 100, fake_hash(0x09), false);
        cache.write(tmp.path()).unwrap();

        let loaded = StatCache::read(tmp.path());
        assert!(!loaded.entries["old.txt"].sealed);
        assert!(!loaded.is_dirty(), "a freshly loaded cache must be clean");
    }

    #[test]
    fn mark_sealed_on_unsealed_entry_sets_bit_and_dirties() {
        let mut cache = StatCache::default();
        let mtime = mtime_at(1_000_000_000, 0);
        cache.update("a.txt".to_string(), mtime, 100, fake_hash(0x01), false);
        cache.dirty = false; // isolate mark_sealed's own effect
        assert!(cache.mark_sealed("a.txt"), "unsealed -> sealed must return true");
        assert!(cache.entries["a.txt"].sealed);
        assert!(cache.is_dirty(), "setting the bit must dirty the cache");
    }

    #[test]
    fn mark_sealed_on_already_sealed_entry_is_a_noop() {
        let mut cache = StatCache::default();
        let mtime = mtime_at(1_000_000_000, 0);
        cache.update("a.txt".to_string(), mtime, 100, fake_hash(0x01), true);
        cache.dirty = false;
        assert!(
            !cache.mark_sealed("a.txt"),
            "already-sealed entry must return false"
        );
        assert!(!cache.is_dirty(), "no-op mark_sealed must leave cache clean");
    }

    #[test]
    fn mark_sealed_on_missing_path_is_a_noop() {
        let mut cache = StatCache::default();
        assert!(!cache.mark_sealed("missing.txt"));
        assert!(!cache.is_dirty());
    }

    #[test]
    fn update_sealed_change_alone_marks_dirty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut cache = StatCache::default();
        let mtime = mtime_at(1_000_000_000, 0);
        cache.update("a.txt".to_string(), mtime, 100, fake_hash(0x01), false);
        cache.write(tmp.path()).unwrap();
        assert!(!cache.is_dirty());
        // Same (mtime, size, hash), only `sealed` flips: entry_eq must catch
        // this so update()'s no-op detection includes `sealed`.
        cache.update("a.txt".to_string(), mtime, 100, fake_hash(0x01), true);
        assert!(
            cache.is_dirty(),
            "a sealed-only change must still mark the cache dirty"
        );
    }

    #[test]
    fn clear_all_sealed_clears_every_bit_and_dirties() {
        let mut cache = StatCache::default();
        let mtime = mtime_at(1_000_000_000, 0);
        cache.update("a.txt".to_string(), mtime, 100, fake_hash(0x01), true);
        cache.update("b.txt".to_string(), mtime, 100, fake_hash(0x02), true);
        cache.update("c.txt".to_string(), mtime, 100, fake_hash(0x03), false);
        cache.dirty = false;

        assert!(cache.clear_all_sealed());
        assert!(!cache.entries["a.txt"].sealed);
        assert!(!cache.entries["b.txt"].sealed);
        assert!(!cache.entries["c.txt"].sealed);
        assert!(cache.is_dirty());
    }

    #[test]
    fn clear_all_sealed_on_already_clear_cache_is_a_noop() {
        let mut cache = StatCache::default();
        let mtime = mtime_at(1_000_000_000, 0);
        cache.update("a.txt".to_string(), mtime, 100, fake_hash(0x01), false);
        cache.dirty = false;
        assert!(!cache.clear_all_sealed());
        assert!(!cache.is_dirty());
    }

    #[test]
    fn read_scoped_carries_seal_epoch_forward() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_cache_with_paths(tmp.path(), &["dirA/a.rs", "dirB/b.rs"]);
        let mut full = StatCache::read(tmp.path());
        full.seal_epoch = [0x42; SEAL_EPOCH_LEN];
        full.dirty = true;
        full.write(tmp.path()).unwrap();

        let scoped = StatCache::read_scoped(tmp.path(), "dirA");
        assert_eq!(
            scoped.seal_epoch, [0x42; SEAL_EPOCH_LEN],
            "a scoped read must still decode the TRAILER's seal_epoch"
        );
    }

    #[test]
    fn write_scoped_merge_carries_rotated_epoch_over_stale_on_disk_value() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_cache_with_paths(tmp.path(), &["dirA/a.rs", "dirB/b.rs"]);

        let mut scoped = StatCache::read_scoped(tmp.path(), "dirA");
        // Simulate finalize_seal_epoch having rotated the epoch during a
        // blob-writing scoped scan (self.seal_epoch now differs from the
        // stale on-disk value that write_scoped_merge will re-read into `full`).
        scoped.seal_epoch = [0x99; SEAL_EPOCH_LEN];
        scoped.dirty = true;
        assert!(scoped.write_scoped_merge(tmp.path(), "dirA").unwrap());

        let after = StatCache::read(tmp.path());
        assert_eq!(
            after.seal_epoch,
            [0x99; SEAL_EPOCH_LEN],
            "the rotated epoch must win over the stale re-read value"
        );
    }

    // -----------------------------------------------------------------------
    // seals_trusted: epoch + sampling (design/07 "Seal epoch",
    // "Sampling verification")
    // -----------------------------------------------------------------------

    #[test]
    fn compute_seals_trusted_false_when_marker_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut cache = StatCache::default();
        cache.seal_epoch = [0x11; SEAL_EPOCH_LEN];
        let store = LocalStore::for_cache(tmp.path().join("objects"));
        cache.compute_seals_trusted(tmp.path(), &store);
        assert!(!cache.seals_trusted());
        assert!(
            cache.needs_new_epoch,
            "a missing marker must request a fresh epoch"
        );
    }

    #[test]
    fn compute_seals_trusted_false_when_cache_epoch_is_zero() {
        // An old cache (written before seal_epoch existed) has seal_epoch = 0,
        // which must never be trusted even if a marker happens to be present.
        let tmp = tempfile::TempDir::new().unwrap();
        write_seal_epoch_marker(tmp.path(), &[0x22; SEAL_EPOCH_LEN]).unwrap();
        let mut cache = StatCache::default(); // seal_epoch defaults to all-zero
        let store = LocalStore::for_cache(tmp.path().join("objects"));
        cache.compute_seals_trusted(tmp.path(), &store);
        assert!(!cache.seals_trusted());
        assert!(cache.needs_new_epoch);
    }

    #[test]
    fn compute_seals_trusted_true_when_epoch_matches_and_no_sealed_entries() {
        let tmp = tempfile::TempDir::new().unwrap();
        let epoch = [0x33; SEAL_EPOCH_LEN];
        write_seal_epoch_marker(tmp.path(), &epoch).unwrap();
        let mut cache = StatCache::default();
        cache.seal_epoch = epoch;
        let store = LocalStore::for_cache(tmp.path().join("objects"));
        cache.compute_seals_trusted(tmp.path(), &store);
        assert!(cache.seals_trusted());
        assert!(!cache.needs_new_epoch, "a matching epoch needs no rotation");
    }

    #[test]
    fn compute_seals_trusted_true_when_sampling_confirms_presence() {
        // Relies on the ambient (default, non-zero) OMEMFS_SEAL_SAMPLE, so it
        // holds SEAL_SAMPLE_ENV_LOCK for mutual exclusion with the tests that
        // override it.
        let _guard = SEAL_SAMPLE_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        let epoch = [0x44; SEAL_EPOCH_LEN];
        write_seal_epoch_marker(tmp.path(), &epoch).unwrap();
        let store = LocalStore::for_cache(tmp.path().join("objects"));

        let mut cache = StatCache::default();
        cache.seal_epoch = epoch;
        let mtime = mtime_at(1_000_000_000, 0);
        // Two sealed entries whose blobs genuinely exist in the store.
        for i in 0..2u8 {
            let content = format!("content-{i}").into_bytes();
            let hash = crate::object::blob_hash(&content);
            let serialised = crate::object::serialise_blob(&content);
            crate::codec::chunk::store_chunked(&store, &hash, &serialised, None).unwrap();
            cache.update(format!("f{i}.txt"), mtime, 100, hash, true);
        }
        cache.compute_seals_trusted(tmp.path(), &store);
        assert!(cache.seals_trusted());
    }

    #[test]
    fn compute_seals_trusted_downgrades_to_false_when_a_sampled_object_is_missing() {
        // Relies on the ambient (default, non-zero) OMEMFS_SEAL_SAMPLE, so it
        // holds SEAL_SAMPLE_ENV_LOCK for mutual exclusion with the tests that
        // override it.
        let _guard = SEAL_SAMPLE_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        let epoch = [0x55; SEAL_EPOCH_LEN];
        write_seal_epoch_marker(tmp.path(), &epoch).unwrap();
        let store = LocalStore::for_cache(tmp.path().join("objects"));

        let mut cache = StatCache::default();
        cache.seal_epoch = epoch;
        let mtime = mtime_at(1_000_000_000, 0);
        // A sealed entry whose blob is NOT actually present (e.g. a shard
        // subdirectory was deleted while .seal_epoch was untouched).
        cache.update(
            "missing.txt".to_string(),
            mtime,
            100,
            fake_hash(0x01),
            true,
        );
        cache.compute_seals_trusted(tmp.path(), &store);
        assert!(
            !cache.seals_trusted(),
            "a missing sampled object must downgrade trust for this scan"
        );
        assert!(
            !cache.needs_new_epoch,
            "sampling failure must not trigger an epoch rotation (design/07: \
             sampling only narrows trust for the current scan)"
        );
    }

    #[test]
    fn seal_sample_zero_disables_sampling() {
        let tmp = tempfile::TempDir::new().unwrap();
        let epoch = [0x66; SEAL_EPOCH_LEN];
        write_seal_epoch_marker(tmp.path(), &epoch).unwrap();
        let store = LocalStore::for_cache(tmp.path().join("objects"));

        let mut cache = StatCache::default();
        cache.seal_epoch = epoch;
        let mtime = mtime_at(1_000_000_000, 0);
        // Sealed entry with NO backing object -- would normally fail sampling.
        cache.update(
            "missing.txt".to_string(),
            mtime,
            100,
            fake_hash(0x01),
            true,
        );

        // SAFETY (test-only): OMEMFS_SEAL_SAMPLE is read by seal_sample_size()
        // on every compute_seals_trusted call; SEAL_SAMPLE_ENV_LOCK is shared
        // with every other test (here and in scan.rs) that reads or mutates
        // it, so no other test can observe this override.
        let _guard = SEAL_SAMPLE_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::set_var("OMEMFS_SEAL_SAMPLE", "0");
        }
        cache.compute_seals_trusted(tmp.path(), &store);
        unsafe {
            std::env::remove_var("OMEMFS_SEAL_SAMPLE");
        }
        assert!(
            cache.seals_trusted(),
            "OMEMFS_SEAL_SAMPLE=0 must disable sampling entirely"
        );
    }

    #[test]
    fn finalize_seal_epoch_noop_when_not_needed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut cache = StatCache::default();
        cache.seal_epoch = [0x77; SEAL_EPOCH_LEN];
        cache.needs_new_epoch = false;
        cache.dirty = false;
        cache.finalize_seal_epoch(tmp.path());
        assert_eq!(cache.seal_epoch, [0x77; SEAL_EPOCH_LEN]);
        assert!(!cache.is_dirty(), "no rotation needed -> no forced write");
        assert!(
            read_seal_epoch_marker(tmp.path()).is_none(),
            "no marker should be written when no rotation is needed"
        );
    }

    #[test]
    fn finalize_seal_epoch_rotates_and_persists_marker_when_needed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut cache = StatCache::default();
        cache.needs_new_epoch = true;
        cache.dirty = false;
        cache.finalize_seal_epoch(tmp.path());

        assert_ne!(
            cache.seal_epoch, NO_SEAL_EPOCH,
            "a freshly generated epoch must not be all-zero"
        );
        assert!(cache.is_dirty(), "rotation must force a write even with no entry changes");
        assert!(!cache.needs_new_epoch, "the flag must be cleared after rotating");
        assert_eq!(
            read_seal_epoch_marker(tmp.path()),
            Some(cache.seal_epoch),
            "the new epoch must be persisted to the marker file"
        );
    }

    #[test]
    fn full_cycle_epoch_mismatch_then_finalize_then_next_scan_trusts() {
        // End-to-end of the state machine within stat_cache.rs itself (no
        // scan.rs / push.rs involved): a cache with no recorded epoch is
        // untrusted, gets a fresh epoch persisted by finalize_seal_epoch, and
        // a *subsequent* load with that persisted epoch is trusted again.
        let tmp = tempfile::TempDir::new().unwrap();
        let store = LocalStore::for_cache(tmp.path().join("objects"));

        let mut cache = StatCache::default(); // seal_epoch = 0: never recorded
        cache.compute_seals_trusted(tmp.path(), &store);
        assert!(!cache.seals_trusted());
        cache.finalize_seal_epoch(tmp.path());
        let rotated = cache.seal_epoch;
        assert_ne!(rotated, NO_SEAL_EPOCH);

        // A later scan loads a cache that already recorded `rotated` as its
        // seal_epoch; the marker on disk still matches, so trust holds.
        let mut next = StatCache::default();
        next.seal_epoch = rotated;
        next.compute_seals_trusted(tmp.path(), &store);
        assert!(next.seals_trusted());
    }
}
