use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::OnceLock;
use std::time::SystemTime;

use chrono::{DateTime, TimeZone, Utc};
use rayon::prelude::*;

use crate::codec;
use crate::dlog_l1;
use crate::dtimer_l1;
use crate::error::Error;
use crate::filter::FilterSet;
use crate::object::{Hash, TreeEntry};
use crate::stat_cache::{RACY_THRESHOLD_SECS, StatCache};
use crate::store::ObjectStore;
use crate::stub;

/// Per-file record collected during a scan. Used by callers to update STAT_CACHE.
pub struct ScannedFile {
    pub fs_mtime: SystemTime,
    pub fs_size: u64,
    pub hash: Hash,
    /// True when the hash was taken directly from STAT_CACHE (file was not read
    /// and the entry already exists in the cache). Such files do not need a
    /// STAT_CACHE update.
    pub cache_hit: bool,
    /// True when the hash was obtained without reading the file but the file is
    /// NOT yet recorded in STAT_CACHE — i.e. it was accepted via the clone-root
    /// `(mtime, size)` fallback. These hits must be inserted into STAT_CACHE so
    /// the next scan hits the cache directly instead of repeating the fallback
    /// (design/07 "Clone-root fallback").
    pub fallback_hit: bool,
    /// True when this scan wrote or verified the local object referenced by
    /// `hash` — i.e. `write_blobs` was `true` for the scan that produced this
    /// record, regardless of which of the three branches in `process_file_job`
    /// resolved it (cache hit, clone-root fallback, or full read+hash all only
    /// reach a `ScannedFile` when the local blob is confirmed present under
    /// `write_blobs = true`). Drives the STAT_CACHE writeback in
    /// `apply_scanned`: a cache miss is `update`d with `sealed = blob_sealed`,
    /// and a cache hit with `blob_sealed = true` gets `mark_sealed` (design/07
    /// "Usage in scan_and_store").
    pub blob_sealed: bool,
}

/// Side data collected while walking the working tree during a scan.
/// Returned to callers (e.g. `ls`) so they do not need additional full
/// filesystem walks for conflict, stub, and ignore detection.
///
/// Note: paths inside ignored directories and inside fully dir-stubbed
/// subtrees are not visited by the scan, so stubs/conflicts located there
/// do not appear here.
#[derive(Default)]
pub struct ScanSideData {
    /// Base relative paths that have conflict helper files
    /// (`*.omemfs-conflict-{base,local,remote}`).
    pub conflict_paths: HashSet<String>,
    /// All stub records found: `(rel_path, record)`. File stubs use the
    /// logical file path; directory stubs use the directory path (no trailing
    /// slash). Stale file stubs (real file also present) are included.
    pub stubs: Vec<(String, stub::StubRecord)>,
    /// Working-tree paths matched by `[ignore]` patterns: `(rel_path, is_dir)`.
    /// Only the topmost ignored path is recorded (contents are not visited).
    pub ignored: Vec<(String, bool)>,
    /// Relative paths of files in the reserved `.omemfs-` namespace whose kind
    /// this version does not recognise (forward compatibility). These are
    /// skipped by the scan and never uploaded.
    pub unknown_reserved: Vec<String>,
    /// Paths that could not be captured because they disappeared or remained
    /// active during this scan. Push preserves their previous tree entries.
    pub unstable_paths: HashSet<String>,
}

/// Result of `scan_and_store_with_cache`. Returns the root tree hash together
/// with per-file details needed to refresh STAT_CACHE.
pub struct ScanResult {
    pub root_hash: Hash,
    /// Only regular (non-stub, non-symlink) files appear here.
    pub files: HashMap<String, ScannedFile>,
    /// Filter rules loaded during the scan; returned so callers can reuse them
    /// without loading the `.omemfs-filter` files a second time.
    pub filters: FilterSet,
    /// Conflict / stub / ignore information collected during the walk.
    pub side: ScanSideData,
}

/// Scan `dir` recursively, build all blob and tree objects, write them to
/// `store`, and return the root tree hash plus per-file scan details for
/// STAT_CACHE refresh.
///
/// `work_dir` is the repository root. Stubs and filter rules are always loaded
/// relative to `work_dir`, even when `dir` is a subdirectory (scoped push).
/// `dir` is the subtree to scan; it must be equal to or a descendant of `work_dir`.
///
/// `clone_root_entries` maps flat relative paths to the last-synced clone root
/// entries. Used for mtime stability (reuse clone root mtime when hash matches).
///
/// `stat_cache` provides mtime-keyed hash acceleration: files whose `(mtime,
/// size)` match a cache entry skip file I/O entirely.
///
/// Paths matched by the `[ignore]` section of any `.omemfs-filter` file are
/// silently excluded from the scan.
///
/// `write_blobs` selects the blob-write mode (design/03 "Scan blob-write mode"):
/// when `true` (`push`, `stub`) a cache-miss file is hashed and its blob object
/// is written to `store`; when `false` (`ls`, `pull`) the file is only hashed
/// (no chunk / compress / encrypt / write). Tree objects are written in both
/// modes. Either way the file's `(mtime, size, hash)` is recorded in
/// `ScanResult.files` so the caller can refresh the STAT_CACHE.
pub fn scan_and_store_with_cache(
    work_dir: &Path,
    dir: &Path,
    store: &dyn ObjectStore,
    clone_root_entries: Option<&HashMap<String, TreeEntry>>,
    stat_cache: &StatCache,
    write_blobs: bool,
) -> Result<ScanResult, Error> {
    let _t = dtimer_l1!("scan");
    let now = SystemTime::now();
    let rel_prefix = dir
        .strip_prefix(work_dir)
        .unwrap_or(std::path::Path::new(""))
        .to_string_lossy()
        .into_owned();
    let rel_prefix = rel_prefix.trim_matches('/').to_string();
    // Scope-limited filter load (design/05): when scanning a subtree (`dir` is a
    // descendant of `work_dir`), load only the filters that can affect in-scope
    // paths instead of walking the whole tree. An empty prefix (whole-tree scan)
    // delegates to the full load.
    let filters = FilterSet::load_scoped(work_dir, &rel_prefix);
    dlog_l1!("scan dir: {:?} (with stat cache)", dir);
    let cfg = FileJobConfig {
        store,
        clone_root_entries,
        stat_cache: Some(stat_cache),
        now,
        write_blobs,
    };
    // Run the whole recursive walk inside the persistent scan pool so that the
    // top-level `rayon::scope` / `par_iter` and every nested one share the same
    // bounded set of worker threads (work-stealing across all depth levels),
    // rather than oversubscribing.
    let DirScan {
        hash: disk_hash,
        files,
        mut side,
        vanished,
        ..
    } = scan_pool().install(|| scan_dir(dir, &rel_prefix, &cfg, &filters))?;
    if vanished {
        return Err(Error::SourceChanged(dir.display().to_string()));
    }
    // The scan root has no parent to detect its own `.omemfs-stub` marker, so the
    // directory-stub classification a parent performs for a child is applied here
    // instead (design/03 "Partial-expansion merge is independent of scan root").
    // Without this, a scan rooted on a stubbed directory — every `ls <path>` /
    // `push <path>` landing exactly on one — would return a tree holding only the
    // on-disk entries and drop the stub-only ones.
    let root_hash = match classify_dir_stub(dir, &rel_prefix, &mut side) {
        DirStubKind::Full(record) => record.hash,
        DirStubKind::Partial(record) => {
            merge_partial_expansion(&record.hash, &disk_hash, store)?.0
        }
        DirStubKind::None => disk_hash,
    };
    Ok(ScanResult {
        root_hash,
        files,
        filters,
        side,
    })
}

/// Persistent global rayon pool used by every working-tree scan in this process.
///
/// Both axes of the scan are parallelised on this single pool: the directory
/// recursion (`rayon::scope`, one task per child directory) and the per-file
/// hashing (`par_iter`). Because the pool is created once and reused, the scan no
/// longer spawns/joins OS threads per directory (the previous design did, which
/// dominated wall-clock time on large trees — see design/03 "Parallel walk and
/// hash computation"). Work-stealing keeps the live thread count bounded by the
/// pool size regardless of tree depth or fan-out.
fn scan_pool() -> &'static rayon::ThreadPool {
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(scan_thread_count())
            .thread_name(|i| format!("omemfs-scan-{}", i))
            .build()
            // A pool build only fails on a fundamental OS resource problem; in
            // that case fall back to a single-thread pool so the scan still runs.
            .unwrap_or_else(|_| {
                rayon::ThreadPoolBuilder::new()
                    .num_threads(1)
                    .build()
                    .expect("single-thread rayon pool")
            })
    })
}

/// Resolve the scan worker count: `OMEMFS_SCAN_THREADS` when set to a positive
/// integer, otherwise `min(available_parallelism, 4)`.
///
/// The cap of 4 is deliberately conservative. The scan's cost on a cold tree is
/// per-file read + SHA-256 (and, for `push`, blob compression); a small amount of
/// parallelism relieves that without oversubscribing disk I/O, and the local
/// object store's transfer concurrency also defaults to 1
/// (`OMEMFS_TRANSFER_CONCURRENCY`). On a 2-core host the effective default is 2.
fn scan_thread_count() -> usize {
    if let Ok(v) = std::env::var("OMEMFS_SCAN_THREADS")
        && let Ok(n) = v.trim().parse::<usize>()
        && n >= 1
    {
        return n;
    }
    let nproc = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    nproc.clamp(1, 4)
}

/// Apply one scan's per-file results onto `cache`: a cache miss (including a
/// clone-root fallback hit) is recorded with a fresh `update`, and a cache hit
/// whose local blob was written or verified this scan (`blob_sealed`) gets
/// `mark_sealed`. Shared by `refresh_stat_cache`, `refresh_stat_cache_scoped`,
/// and `push`'s multi-path scoped loop so the writeback logic cannot drift
/// across call sites (design/07 "Usage in scan_and_store").
pub fn apply_scanned(cache: &mut StatCache, files: &HashMap<String, ScannedFile>) {
    for (path, scanned) in files {
        if !scanned.cache_hit {
            // Update entries that were re-hashed (full read) and entries
            // accepted via the clone-root fallback (`fallback_hit`): both have
            // `cache_hit == false`. Recording fallback hits means the next
            // scan gets a direct cache hit instead of repeating the
            // clone-root lookup (design/07 "Clone-root fallback").
            if scanned.fallback_hit {
                dlog_l1!("stat cache: record clone-root fallback hit {}", path);
            }
            cache.update(
                path.clone(),
                scanned.fs_mtime,
                scanned.fs_size,
                scanned.hash.clone(),
                scanned.blob_sealed,
            );
        } else if scanned.blob_sealed {
            // A direct STAT_CACHE hit whose local blob this scan wrote or
            // verified: seal it so a later blob-writing scan can skip its own
            // local-existence check (design/07 "Seal invalidation"). A no-op
            // when the entry was already sealed (keeps the cache clean).
            cache.mark_sealed(path);
        }
    }
}

/// Update `cache` with any files that were not a cache hit, then persist it.
/// `write_blobs` must match the scan that produced `files`: only a
/// blob-writing scan (`push`, `stub`) ever needs to rotate the seal epoch
/// (design/07 "Seal epoch"); a read-only scan (`ls`, `pull`) always passes
/// `false` and simply carries its loaded `seal_epoch` forward unchanged.
pub fn refresh_stat_cache(
    mut cache: StatCache,
    files: &HashMap<String, ScannedFile>,
    omemfs_dir: &std::path::Path,
    write_blobs: bool,
) {
    apply_scanned(&mut cache, files);
    if write_blobs {
        cache.finalize_seal_epoch(omemfs_dir);
    }
    // STAT_CACHE is a pure acceleration cache (design/07): a write failure must
    // not abort the command. Emit a warning and continue. The write is skipped
    // entirely when nothing changed since load (no-change scans do not rewrite
    // the cache file).
    if let Err(e) = cache.write_if_dirty(omemfs_dir) {
        eprintln!(
            "warning: failed to write STAT_CACHE (acceleration cache): {}",
            e
        );
    }
}

/// Like [`refresh_stat_cache`], but for a scope-limited scan: `cache` was loaded
/// via [`StatCache::read_scoped`] with `scope_prefix` and therefore holds only
/// the in-scope entries. The writeback re-reads the full file fresh and overlays
/// the in-scope updates so out-of-scope entries survive byte-for-byte
/// (design/07 "Read optimisation: scope-limited load").
///
/// Only used by `ls` (always a read-only, `write_blobs = false` scan), so
/// there is no `write_blobs` parameter and no seal-epoch rotation here; the
/// loaded `seal_epoch` is carried forward unchanged by `write_scoped_merge`.
pub fn refresh_stat_cache_scoped(
    mut cache: StatCache,
    files: &HashMap<String, ScannedFile>,
    omemfs_dir: &std::path::Path,
    scope_prefix: &str,
) {
    apply_scanned(&mut cache, files);
    // STAT_CACHE is a pure acceleration cache (design/07): a write failure must
    // not abort the command. The merge re-reads the full file and overlays the
    // in-scope updates; a no-change scoped scan skips the write entirely.
    if let Err(e) = cache.write_scoped_merge(omemfs_dir, scope_prefix) {
        eprintln!(
            "warning: failed to write STAT_CACHE (acceleration cache): {}",
            e
        );
    }
}

/// Local result of scanning one directory level. `scan_dir` is a pure function:
/// it returns this struct instead of pushing into a shared `&mut`, so sibling
/// subdirectories can be scanned concurrently with no locking. The caller merges
/// a child's `files` / `side` into its own (design/03 "Parallel walk and hash
/// computation").
struct DirScan {
    hash: Hash,
    mtime: Option<chrono::DateTime<chrono::Utc>>,
    size: u64,
    blob_count: u64,
    /// Per-file `ScannedFile` records for every regular file in this subtree.
    files: HashMap<String, ScannedFile>,
    /// Conflict / stub / ignore information collected over this subtree.
    side: ScanSideData,
    /// The directory was listed by its parent but vanished before its own
    /// `read_dir`. Its parent must preserve the previous entry, not encode an
    /// empty directory (which would look like a destructive edit).
    vanished: bool,
}

/// A subdirectory to recurse into, classified during the (sequential) name-walk
/// of the parent level. The recursion itself runs in parallel across siblings.
struct SubdirJob {
    name: String,
    rel_path: String,
    child: std::path::PathBuf,
    /// `Some(stub_hash)` for a partially-expanded directory stub: after the disk
    /// subtree is scanned, its result is merged with the stub's recorded tree
    /// (design/08 "Partial expansion"). `None` for an ordinary directory.
    partial_stub_hash: Option<Hash>,
}

/// Merge a child's scan result into the parent's accumulators (files map and
/// side data). Paths are globally unique, so the maps/vecs simply absorb the
/// child's contents; order does not matter (the tree hash is name-sorted).
fn merge_child(files: &mut HashMap<String, ScannedFile>, side: &mut ScanSideData, child: DirScan) {
    files.extend(child.files);
    side.conflict_paths.extend(child.side.conflict_paths);
    side.stubs.extend(child.side.stubs);
    side.ignored.extend(child.side.ignored);
    side.unknown_reserved.extend(child.side.unknown_reserved);
    side.unstable_paths.extend(child.side.unstable_paths);
}

/// Scan one directory level, building and storing its tree object and returning
/// a [`DirScan`]. Aggregates are computed from the in-memory entries to avoid a
/// read-back from the object store (which would incur decompress + decrypt per
/// directory).
///
/// Both axes of work are parallelised on the shared scan pool: subdirectories
/// are recursed via `rayon::scope` and regular files are hashed via `par_iter`.
/// The `rayon::scope` join point waits for every child before this level builds
/// its own tree, preserving the parent-after-child ordering the tree hash needs.
fn scan_dir(
    dir: &Path,
    rel_prefix: &str,
    cfg: &FileJobConfig<'_>,
    filters: &FilterSet,
) -> Result<DirScan, Error> {
    let mut entries: Vec<TreeEntry> = Vec::new();
    let mut files: HashMap<String, ScannedFile> = HashMap::new();
    let mut side = ScanSideData::default();
    // Deferred work for this level, classified sequentially then run in parallel.
    let mut file_jobs: Vec<FileJob> = Vec::new();
    let mut subdir_jobs: Vec<SubdirJob> = Vec::new();

    // Single read_dir for this level. We keep the DirEntry so we can use its
    // file_type() (satisfied from getdents64 on Linux in the common case, i.e.
    // no extra statx). raw_name_set is the set of all in-scope names at this
    // level, used to decide stub/real-file presence without an extra statx.
    let mut dir_entries: Vec<(String, fs::DirEntry)> = Vec::new();
    let mut raw_name_set: HashSet<String> = HashSet::new();
    match fs::read_dir(dir) {
        Ok(rd) => {
            for item in rd {
                let e = match item {
                    Ok(e) => e,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(Error::Io(e)),
                };
                let n = e.file_name().to_string_lossy().into_owned();
                if n == ".omemfs" {
                    continue;
                }
                // Conflict helpers and the metadata sidecar (L6:
                // `crate::commands::conflict::is_conflict_artifact`, the
                // single shared predicate -- avoids this check and the
                // per-path cleanup in `restore.rs` drifting out of sync, as
                // they once did when the sidecar was missing from an older,
                // separate copy of the suffix list) are excluded from
                // scan/push: they are internal state consumed by `omemfs
                // conflict accept`, not working-tree content. Only the three
                // helper suffixes are ever tracked in `conflict_paths`
                // (`conflict_base_name` recognises those and no others); the
                // sidecar contributes nothing there and is simply skipped.
                if crate::commands::conflict::is_conflict_artifact(&n) {
                    if let Some(base) = conflict_base_name(&n) {
                        let rel = join_rel(rel_prefix, base);
                        side.conflict_paths.insert(rel);
                    }
                    continue;
                }
                // Unknown reserved name (produced by a newer omemfs version): warn
                // once, skip, and never treat as regular file content
                // (design/09_reserved_names.md "Forward compatibility").
                if is_unknown_reserved_name(&n) {
                    let rel = join_rel(rel_prefix, &n);
                    warn_unknown_reserved(&rel);
                    side.unknown_reserved.push(rel);
                    continue;
                }
                raw_name_set.insert(n.clone());
                dir_entries.push((n, e));
            }
        }
        // Vanished between being listed by the parent and being scanned here.
        // Return a distinct outcome: treating this as an empty directory would
        // silently replace its previous subtree with an empty one.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DirScan {
                hash: Hash::compute(b"vanished-directory"),
                mtime: None,
                size: 0,
                blob_count: 0,
                files,
                side,
                vanished: true,
            });
        }
        Err(e) => return Err(Error::Io(e)),
    }

    // Build the set of logical names. Stub files contribute their logical name
    // (without the .omemfs-stub suffix); regular files contribute their own name.
    // If both a real file and a stub file exist for the same logical name, the
    // real file takes priority (stub is treated as stale and skipped).
    // The directory stub marker (.omemfs-stub inside a dir) is excluded here —
    // it is handled in the is_dir branch below.
    //
    // We map logical name -> the DirEntry that materialises it (the real file's
    // entry; for a stub-only name there is no real-file entry so it is None and
    // resolved purely from the stub file).
    let mut logical: Vec<(String, Option<fs::DirEntry>)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (raw, entry) in dir_entries {
        if raw == stub::DIR_STUB_NAME {
            // Skip: the marker itself is never an entry of its own directory. The
            // stub for the *current* dir is classified by whoever owns this level
            // (the enumerating parent, or `scan_and_store_with_cache` when this
            // dir is the scan root); the is_dir branch below handles the marker
            // inside a *child* dir.
            continue;
        }
        if let Some(logical_name) = stub::logical_name(&raw) {
            // A stub file. Only contribute its logical name when the real file
            // is absent at this level — decided from raw_name_set (no statx).
            if !raw_name_set.contains(logical_name) && seen.insert(logical_name.to_string()) {
                logical.push((logical_name.to_string(), None));
            }
            // If the real file exists, the stub is ignored; the real file's own
            // DirEntry is added when we encounter it below.
        } else if seen.insert(raw.clone()) {
            logical.push((raw, Some(entry)));
        }
    }
    // Sort by name for a deterministic classification order (the final tree hash
    // is name-sorted anyway, but a stable order keeps side-data vectors stable).
    logical.sort_by(|a, b| a.0.cmp(&b.0));

    // Deterministic integration-test hook for list→open races. It is compiled
    // only in debug builds and has no effect unless explicitly requested.
    #[cfg(debug_assertions)]
    if let Ok(ms) = std::env::var("OMEMFS_TEST_SCAN_AFTER_LIST_DELAY_MS")
        && let Ok(ms) = ms.parse::<u64>()
    {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }

    for (name, real_entry) in logical {
        let rel_path = join_rel(rel_prefix, &name);

        // Skip paths matched by [ignore] patterns; record them as side data
        // (with directory-ness) so ls can display them without another walk.
        // The directory-ness comes from the already-listed DirEntry's file_type
        // when available (no extra statx); only a stub-only name needs a stat.
        if filters.is_ignored(&rel_path) {
            let is_dir = match &real_entry {
                Some(e) => e.file_type().map(|t| t.is_dir()).unwrap_or(false),
                None => dir.join(&name).is_dir(),
            };
            side.ignored.push((rel_path, is_dir));
            continue;
        }

        let child = dir.join(&name);

        // Check for a file-adjacent stub (<name>.omemfs-stub). Its presence is a
        // set lookup against the names already listed at this level (no statx).
        let stub_name = format!("{}{}", name, stub::STUB_SUFFIX);
        if raw_name_set.contains(&stub_name) {
            let stub_file = dir.join(&stub_name);
            if let Ok(s) = fs::read_to_string(&stub_file)
                && let Ok(record) = serde_json::from_str::<stub::StubRecord>(&s)
            {
                // Record the stub as side data even when stale (real file
                // present); display logic decides how to treat it.
                side.stubs.push((rel_path.clone(), record.clone()));
                // The real file is present iff this logical name carried a
                // real DirEntry (set membership, no statx).
                if real_entry.is_none() {
                    entries.push(TreeEntry::Blob {
                        name,
                        hash: record.hash,
                        mtime: record.mtime,
                        size: record.size,
                        mode: record.mode,
                    });
                    continue;
                }
                // Stale stub: the real file takes priority below.
            }
        }

        // Resolve the entry's type from the listed DirEntry's file_type() when
        // possible (no extra statx). Only fetch full metadata once, where mode /
        // size / mtime are genuinely needed (regular files and symlinks).
        let ftype = match &real_entry {
            Some(e) => match e.file_type() {
                Ok(t) => t,
                // file_type() can fail (e.g. the entry vanished); fall back to a
                // single symlink_metadata. NotFound there confirms a vanish race
                // (skip); any other error (e.g. permission denied) fails the scan
                // rather than silently treating the entry as absent.
                Err(_) => match fs::symlink_metadata(&child) {
                    Ok(m) => m.file_type(),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        preserve_unstable_entry(&mut entries, &mut side, cfg, &rel_path);
                        continue;
                    }
                    Err(e) => return Err(Error::Io(e)),
                },
            },
            // Stub-only name with no real file: nothing on disk to classify.
            None => continue,
        };

        if ftype.is_symlink() {
            let metadata = match fs::symlink_metadata(&child) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    preserve_unstable_entry(&mut entries, &mut side, cfg, &rel_path);
                    continue;
                }
                Err(e) => return Err(Error::Io(e)),
            };
            let target = match fs::read_link(&child) {
                Ok(target) => target.to_string_lossy().into_owned(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    preserve_unstable_entry(&mut entries, &mut side, cfg, &rel_path);
                    continue;
                }
                Err(e) => return Err(Error::Io(e)),
            };
            let mtime = system_time_to_datetime(metadata.modified().ok());
            entries.push(TreeEntry::Symlink {
                name,
                target,
                mtime,
            });
        } else if ftype.is_dir() {
            match classify_dir_stub(&child, &rel_path, &mut side) {
                // Fully stubbed: use the recorded tree hash directly without
                // scanning the subtree.
                DirStubKind::Full(record) => {
                    entries.push(TreeEntry::Tree {
                        name,
                        hash: record.hash,
                        mtime: record.mtime,
                        size: record.size,
                        blob_count: record.blob_count,
                    });
                    continue;
                }
                // Partial expansion: scan the disk subtree, then merge with the
                // stub's recorded tree after recursion.
                DirStubKind::Partial(record) => {
                    subdir_jobs.push(SubdirJob {
                        name,
                        rel_path,
                        child,
                        partial_stub_hash: Some(record.hash),
                    });
                    continue;
                }
                DirStubKind::None => {}
            }
            subdir_jobs.push(SubdirJob {
                name,
                rel_path,
                child,
                partial_stub_hash: None,
            });
        } else if ftype.is_file() {
            // A regular file: fetch metadata once for mode / size / mtime, then
            // defer the (optionally parallel) hash. The body is read later, in
            // process_file_job, only on a STAT_CACHE / clone-root miss.
            let metadata = match fs::symlink_metadata(&child) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    preserve_unstable_entry(&mut entries, &mut side, cfg, &rel_path);
                    continue;
                }
                Err(e) => return Err(Error::Io(e)),
            };
            file_jobs.push(FileJob {
                name,
                rel_path,
                file_size: metadata.len(),
                fs_mtime_st: metadata.modified().ok(),
                mode: crate::fsmeta::mode_from_metadata(&metadata),
                child,
            });
        }
    }

    // --- Parallel pass 1: recurse into subdirectories ---------------------
    // Each child scan is independent and returns its own DirScan. rayon::scope
    // runs them on the shared pool and the scope's close is a join barrier: every
    // child finishes before this level builds its tree (parent-after-child).
    // Results are collected into a pre-sized slot vector so a panic-free scope
    // body needs no locking; errors are surfaced first-error-wins after join.
    if !subdir_jobs.is_empty() {
        let mut slots: Vec<Option<Result<(SubdirEntry, DirScan), Error>>> =
            (0..subdir_jobs.len()).map(|_| None).collect();
        rayon::scope(|scope| {
            for (job, slot) in subdir_jobs.iter().zip(slots.iter_mut()) {
                scope.spawn(move |_| {
                    *slot = Some(scan_subdir(job, cfg, filters));
                });
            }
        });
        for slot in slots {
            let (subentry, child_scan) = slot.expect("rayon::scope filled every slot")?;
            // Merge the child's files/side first (moves child_scan.side out
            // before we read the entry built from it).
            let entry = subentry.entry;
            merge_child(&mut files, &mut side, child_scan);
            if let Some(entry) = entry {
                entries.push(entry);
            }
        }
    }

    // --- Parallel pass 2: hash this level's regular files -----------------
    // Each job is independent (STAT_CACHE / clone-root lookups are read-only;
    // content-addressed blob/tree writes are idempotent). par_iter collects into
    // a Result, which short-circuits on the first Err (first-error-wins). Entry
    // order does not matter — build_and_store sorts by name.
    let job_results: Vec<FileJobResult> = file_jobs
        .par_iter()
        .map(|job| process_file_job(job, cfg))
        .collect::<Result<Vec<_>, _>>()?;
    for (entry, scanned, unstable) in job_results {
        if let Some((rel_path, sf)) = scanned {
            files.insert(rel_path, sf);
        }
        if let Some(path) = unstable {
            side.unstable_paths.insert(path);
        }
        if let Some(entry) = entry {
            entries.push(entry);
        }
    }

    // An editor may atomically replace a file while this directory is being
    // scanned. If a previous entry was absent from the initial listing but has
    // reappeared by the end, this was not a stable deletion. Preserve the old
    // entry for this push; a later push will capture the replacement.
    if cfg.write_blobs
        && let Some(previous) = cfg.clone_root_entries
    {
        for (path, old_entry) in previous {
            if parent_rel(path) != rel_prefix
                || raw_name_set.contains(old_entry.name())
                || entries.iter().any(|entry| entry.name() == old_entry.name())
            {
                continue;
            }
            match fs::symlink_metadata(dir.join(old_entry.name())) {
                Ok(_) => {
                    side.unstable_paths.insert(path.clone());
                    entries.push(old_entry.clone());
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(Error::Io(e)),
            }
        }
    }

    // The directory may have been removed after `read_dir` succeeded (an open
    // directory iterator can otherwise look like a legitimately emptied
    // directory). Let the parent preserve the whole previous subtree.
    if !rel_prefix.is_empty() {
        match fs::symlink_metadata(dir) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                side.unstable_paths.insert(rel_prefix.to_string());
                return Ok(DirScan {
                    hash: Hash::compute(b"replaced-directory"),
                    mtime: None,
                    size: 0,
                    blob_count: 0,
                    files: HashMap::new(),
                    side,
                    vanished: true,
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                side.unstable_paths.insert(rel_prefix.to_string());
                return Ok(DirScan {
                    hash: Hash::compute(b"vanished-directory"),
                    mtime: None,
                    size: 0,
                    blob_count: 0,
                    files: HashMap::new(),
                    side,
                    vanished: true,
                });
            }
            Err(e) => return Err(Error::Io(e)),
        }
    }

    // Compute aggregate metadata from the in-memory entries instead of reading
    // the stored tree object back (avoids decompress + decrypt per directory).
    let mtime = crate::object::Tree::aggregate_mtime(&entries);
    let size = crate::object::Tree::aggregate_size(&entries);
    let blob_count = crate::object::Tree::aggregate_blob_count(&entries);
    let hash = crate::tree_ops::build_and_store(entries, cfg.store)?;
    Ok(DirScan {
        hash,
        mtime,
        size,
        blob_count,
        files,
        side,
        vanished: false,
    })
}

/// The parent-level `TreeEntry` produced for one scanned subdirectory.
struct SubdirEntry {
    entry: Option<TreeEntry>,
}

/// Scan one subdirectory job and produce both the parent's `TreeEntry` and the
/// child's full `DirScan` (so the parent can merge files/side). For a partial
/// directory-stub expansion, the disk subtree is merged with the stub's recorded
/// tree (design/08 "Partial expansion") before the entry is built.
fn scan_subdir(
    job: &SubdirJob,
    cfg: &FileJobConfig<'_>,
    filters: &FilterSet,
) -> Result<(SubdirEntry, DirScan), Error> {
    let mut child_scan = scan_dir(&job.child, &job.rel_path, cfg, filters)?;
    if child_scan.vanished {
        child_scan.side.unstable_paths.insert(job.rel_path.clone());
        return Ok((
            SubdirEntry {
                entry: previous_entry(cfg, &job.rel_path),
            },
            child_scan,
        ));
    }
    let entry = match &job.partial_stub_hash {
        Some(stub_hash) => {
            let (merged_hash, mtime, size, blob_count) =
                merge_partial_expansion(stub_hash, &child_scan.hash, cfg.store)?;
            TreeEntry::Tree {
                name: job.name.clone(),
                hash: merged_hash,
                mtime,
                size,
                blob_count,
            }
        }
        None => TreeEntry::Tree {
            name: job.name.clone(),
            hash: child_scan.hash.clone(),
            mtime: child_scan.mtime,
            size: child_scan.size,
            blob_count: child_scan.blob_count,
        },
    };
    Ok((SubdirEntry { entry: Some(entry) }, child_scan))
}

/// Join a relative-path prefix with a name (`""` prefix → bare name).
fn join_rel(rel_prefix: &str, name: &str) -> String {
    if rel_prefix.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", rel_prefix, name)
    }
}

fn parent_rel(path: &str) -> &str {
    path.rsplit_once('/')
        .map(|(parent, _)| parent)
        .unwrap_or("")
}

fn previous_entry(cfg: &FileJobConfig<'_>, rel_path: &str) -> Option<TreeEntry> {
    cfg.clone_root_entries
        .and_then(|entries| entries.get(rel_path))
        .cloned()
}

fn preserve_unstable_entry(
    entries: &mut Vec<TreeEntry>,
    side: &mut ScanSideData,
    cfg: &FileJobConfig<'_>,
    rel_path: &str,
) {
    side.unstable_paths.insert(rel_path.to_string());
    if let Some(entry) = previous_entry(cfg, rel_path) {
        entries.push(entry);
    }
}

// ---------------------------------------------------------------------------
// Per-directory regular-file hashing (parallel)
// ---------------------------------------------------------------------------

/// A deferred regular-file entry collected during the directory walk. Holds
/// everything needed to resolve the file's tree entry; the file body is read
/// only if the STAT_CACHE and clone-root fallback both miss.
struct FileJob {
    name: String,
    rel_path: String,
    file_size: u64,
    fs_mtime_st: Option<SystemTime>,
    mode: Option<String>,
    child: std::path::PathBuf,
}

/// Read-only inputs shared by every file job in one `scan_dir` call. All fields
/// are `Send + Sync` (the object store is `Send + Sync` and the lookups are
/// read-only), so a borrow may be shared across the worker threads.
struct FileJobConfig<'a> {
    store: &'a dyn ObjectStore,
    clone_root_entries: Option<&'a HashMap<String, TreeEntry>>,
    stat_cache: Option<&'a StatCache>,
    now: SystemTime,
    write_blobs: bool,
}

/// Result of one file job: optional entry (new transient files may disappear),
/// optional cache record, and an unstable path to report.
type FileJobResult = (
    Option<TreeEntry>,
    Option<(String, ScannedFile)>,
    Option<String>,
);

/// Resolve a single regular file to its tree entry, applying the same
/// three-stage check as the serial scan: STAT_CACHE hit, clone-root
/// `(mtime, size)` fallback, then full read + hash.
fn process_file_job(job: &FileJob, cfg: &FileJobConfig<'_>) -> Result<FileJobResult, Error> {
    let name = job.name.clone();
    let rel_path = &job.rel_path;
    let file_size = job.file_size;
    let fs_mtime_st = job.fs_mtime_st;
    let fs_mtime = system_time_to_datetime(fs_mtime_st);
    let mode = job.mode.clone();

    // STAT_CACHE lookup: skip hash computation when (mtime, size) match.
    //
    // Skip condition on a hit (design/07 "process_file_job's exact skip
    // condition"):
    //   write_blobs == false                     -- read-only scan never
    //                                                needed the blob
    //     OR (entry.sealed AND seals_trusted)     -- verified-present, and
    //                                                that verification is
    //                                                still trustworthy
    //     OR blob_fully_cached(store, entry.hash) -- fall back to the check
    //                                                this replaces
    // Only the middle branch is new; it never changes what a scan accepts, it
    // only skips a local-existence check whose answer is already known.
    // The third branch must be `blob_fully_cached`, not a bare
    // `store.exists()`: for a chunked blob, `exists()` on the manifest hash
    // alone says nothing about whether every chunk it lists is still present
    // (design/07 "Depth of the local-existence check").
    if let Some(cache) = cfg.stat_cache
        && let Some(fs_mtime_sys) = fs_mtime_st
        && let Some(cached_entry) = cache.lookup_current_entry(rel_path, fs_mtime_sys, file_size)
        && (!cfg.write_blobs
            || (cached_entry.sealed && cache.seals_trusted())
            || codec::chunk::blob_fully_cached(cfg.store, &cached_entry.hash)?)
    {
        let cached_hash = &cached_entry.hash;
        let mtime = mtime_stable(
            mtime_for_entry(cfg.clone_root_entries, rel_path, cached_hash),
            fs_mtime,
        );
        let scanned = ScannedFile {
            fs_mtime: fs_mtime_sys,
            fs_size: file_size,
            hash: cached_hash.clone(),
            cache_hit: true,
            fallback_hit: false,
            // The blob is confirmed present exactly when write_blobs is true:
            // either it was already sealed-and-trusted, or blob_fully_cached()
            // was just called and returned true (the && chain above
            // short-circuits otherwise). A read-only scan never checked, so
            // it is not sealed.
            blob_sealed: cfg.write_blobs,
        };
        let entry = TreeEntry::Blob {
            name,
            hash: cached_hash.clone(),
            mtime,
            size: file_size,
            mode,
        };
        return Ok((Some(entry), Some((rel_path.clone(), scanned)), None));
    }

    // Fallback: clone_root mtime+size match outside the racy window. The
    // local-existence check here must be the same `blob_fully_cached` depth
    // as the STAT_CACHE-hit branch above -- accepting a fallback hit on a
    // manifest-only `store.exists()` while a chunk is missing would
    // re-establish `sealed = true` on a blob that is not actually
    // retrievable (design/07 "Depth of the local-existence check").
    if let Some(cached_entry) = cfg.clone_root_entries.and_then(|m| m.get(rel_path))
        && can_skip_hash(cached_entry, file_size, fs_mtime_st, cfg.now)
        && let Some(cached_hash) = cached_entry.hash()
        && (!cfg.write_blobs || codec::chunk::blob_fully_cached(cfg.store, cached_hash)?)
    {
        let (hash, cr_mtime) = match cached_entry {
            TreeEntry::Blob { hash, mtime, .. } => (hash.clone(), *mtime),
            _ => unreachable!(),
        };
        let scanned = fs_mtime_st.map(|fs_mtime_sys| {
            (
                rel_path.clone(),
                ScannedFile {
                    fs_mtime: fs_mtime_sys,
                    fs_size: file_size,
                    hash: hash.clone(),
                    // Not read, but not yet in STAT_CACHE: record it so the
                    // next scan hits the cache directly.
                    cache_hit: false,
                    fallback_hit: true,
                    // Present exactly when write_blobs is true (the && chain
                    // above required blob_fully_cached() == true in that case).
                    blob_sealed: cfg.write_blobs,
                },
            )
        });
        let entry = TreeEntry::Blob {
            name,
            hash,
            mtime: cr_mtime,
            size: file_size,
            mode,
        };
        return Ok((Some(entry), scanned, None));
    }

    // Full read + hash computation required.
    // - write_blobs: store_file switches between the in-memory and one-pass
    //   streaming write paths by file size; both yield the identical logical
    //   hash and stored objects.
    // - !write_blobs (ls/pull): hash only — no chunk/compress/encrypt/write. The
    //   logical hash is identical to store_file's (design/02 "Hash-only
    //   variant"). Tree objects are still written by the caller.
    dlog_l1!(
        "hash file: {} ({}B, write_blobs={})",
        rel_path,
        file_size,
        cfg.write_blobs
    );
    let captured = if cfg.write_blobs {
        match codec::chunk::store_file_snapshot(cfg.store, &job.child, None) {
            Ok(stored) => Some(stored),
            Err(Error::SourceChanged(_)) => {
                return Ok((previous_entry(cfg, rel_path), None, Some(rel_path.clone())));
            }
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok((previous_entry(cfg, rel_path), None, Some(rel_path.clone())));
            }
            Err(e) => return Err(e),
        }
    } else {
        None
    };
    let hash = match &captured {
        Some(stored) => stored.hash.clone(),
        None => codec::chunk::hash_file(&job.child)?,
    };
    let effective_mtime_st = captured
        .as_ref()
        .and_then(|stored| stored.fs_mtime)
        .or(fs_mtime_st);
    let effective_size = captured
        .as_ref()
        .map(|stored| stored.size)
        .unwrap_or(file_size);
    let effective_mode = captured
        .as_ref()
        .map(|stored| stored.mode.clone())
        .unwrap_or(mode);
    // Apply mtime stability: if hash matches clone root, reuse its mtime.
    let mtime = mtime_stable(
        mtime_for_entry(cfg.clone_root_entries, rel_path, &hash),
        system_time_to_datetime(effective_mtime_st),
    );
    let scanned = effective_mtime_st.map(|fs_mtime_sys| {
        (
            rel_path.clone(),
            ScannedFile {
                fs_mtime: fs_mtime_sys,
                fs_size: effective_size,
                hash: hash.clone(),
                cache_hit: false,
                fallback_hit: false,
                // The blob was just written to `cfg.store` above when
                // write_blobs is true (store_file_snapshot); a read-only scan
                // never writes it.
                blob_sealed: cfg.write_blobs,
            },
        )
    });
    let entry = TreeEntry::Blob {
        name,
        hash,
        mtime,
        size: effective_size,
        mode: effective_mode,
    };
    Ok((Some(entry), scanned, None))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// How a directory's own stub marker (`<dir>/.omemfs-stub`) affects the tree
/// built for that directory.
enum DirStubKind {
    /// No directory stub, or one whose target is not a tree: scan disk only.
    None,
    /// Fully stubbed (the marker is the directory's only content): the recorded
    /// tree is the directory's content.
    Full(stub::StubRecord),
    /// Partially expanded (the marker coexists with real files /
    /// subdirectories): the disk tree must be merged with the recorded one.
    Partial(stub::StubRecord),
}

/// Classify `dir`'s own directory stub marker and record it in `side`.
///
/// Shared by both callers so the classification cannot drift between them: a
/// parent enumerating `dir` as a child, and the entry point handling `dir` as the
/// scan root (design/03 "Partial-expansion merge is independent of scan root").
/// The stub is recorded as side data regardless of target type (matches stub
/// enumeration semantics).
fn classify_dir_stub(dir: &Path, rel_path: &str, side: &mut ScanSideData) -> DirStubKind {
    // The marker is not part of the parent level's name set, so its presence
    // requires one statx (kept to a single call).
    let dir_stub_path = dir.join(stub::DIR_STUB_NAME);
    if !dir_stub_path.is_file() {
        return DirStubKind::None;
    }
    let Ok(text) = fs::read_to_string(&dir_stub_path) else {
        return DirStubKind::None;
    };
    let Ok(record) = serde_json::from_str::<stub::StubRecord>(&text) else {
        return DirStubKind::None;
    };
    side.stubs.push((rel_path.to_string(), record.clone()));
    if record.target_type != stub::StubTargetType::Tree {
        return DirStubKind::None;
    }
    if dir_has_real_children(dir) {
        DirStubKind::Partial(record)
    } else {
        DirStubKind::Full(record)
    }
}

/// Returns true if `dir` contains any entry other than the directory stub
/// marker (`.omemfs-stub`). Used to distinguish a fully-stubbed directory from
/// a partially expanded one.
fn dir_has_real_children(dir: &Path) -> bool {
    match fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .any(|e| e.file_name().to_string_lossy() != stub::DIR_STUB_NAME),
        Err(_) => false,
    }
}

/// Merge the directory stub's recorded tree (`stub_hash`) with the tree built
/// from the on-disk children (`disk_hash`) of a partially expanded directory.
///
/// On-disk entries take priority over same-name entries recorded in the stub;
/// stub entries are kept for names not present on disk. The merged tree is
/// stored as a new object. Returns `(merged_hash, mtime, size, blob_count)`.
fn merge_partial_expansion(
    stub_hash: &Hash,
    disk_hash: &Hash,
    store: &dyn ObjectStore,
) -> Result<(Hash, Option<chrono::DateTime<chrono::Utc>>, u64, u64), Error> {
    let stub_entries = crate::tree_ops::load_all_entries(stub_hash, store)?;
    let disk_entries = crate::tree_ops::load_all_entries(disk_hash, store)?;
    let disk_names: HashSet<String> = disk_entries.iter().map(|e| e.name().to_string()).collect();
    let mut merged: Vec<TreeEntry> = disk_entries;
    for entry in stub_entries {
        if !disk_names.contains(entry.name()) {
            merged.push(entry);
        }
    }
    let mtime = crate::object::Tree::aggregate_mtime(&merged);
    let size = crate::object::Tree::aggregate_size(&merged);
    let blob_count = crate::object::Tree::aggregate_blob_count(&merged);
    let hash = crate::tree_ops::build_and_store(merged, store)?;
    Ok((hash, mtime, size, blob_count))
}

/// Returns true if the file can skip hash computation:
/// - mtime and size match the clone root entry, AND
/// - the file's mtime is outside the racy window (not too recent).
fn can_skip_hash(
    cached: &TreeEntry,
    fs_size: u64,
    fs_mtime_st: Option<SystemTime>,
    now: SystemTime,
) -> bool {
    // Only applicable to blob entries.
    let (cr_size, cr_mtime) = match cached {
        TreeEntry::Blob { size, mtime, .. } => (*size, *mtime),
        _ => return false,
    };

    // Size mismatch → definitely changed.
    if fs_size != cr_size {
        return false;
    }

    let fs_st = match fs_mtime_st {
        Some(t) => t,
        None => return false,
    };

    // mtime mismatch → likely changed.
    if system_time_to_datetime(Some(fs_st)) != cr_mtime {
        return false;
    }

    // Racy check: if the file was modified very recently, force hash computation
    // regardless of mtime match (covers FAT32's 2-second granularity rounding).
    let age = now.duration_since(fs_st).unwrap_or_default();
    age.as_secs() >= RACY_THRESHOLD_SECS
}

/// If the clone root has an entry at `rel_path` whose hash equals `computed_hash`,
/// return its mtime (for mtime stability). Otherwise return None.
fn mtime_for_entry(
    clone_root_entries: Option<&HashMap<String, TreeEntry>>,
    rel_path: &str,
    computed_hash: &Hash,
) -> Option<DateTime<Utc>> {
    let entry = clone_root_entries?.get(rel_path)?;
    let (cr_hash, cr_mtime) = match entry {
        TreeEntry::Blob { hash, mtime, .. } => (hash, mtime),
        _ => return None,
    };
    if cr_hash == computed_hash {
        *cr_mtime
    } else {
        None
    }
}

/// Return `clone_mtime` if it is `Some`, otherwise fall back to `fs_mtime`.
fn mtime_stable(
    clone_mtime: Option<DateTime<Utc>>,
    fs_mtime: Option<DateTime<Utc>>,
) -> Option<DateTime<Utc>> {
    clone_mtime.or(fs_mtime)
}

/// Returns true if `name` is a conflict helper file produced by pull.
pub fn is_conflict_helper(name: &str) -> bool {
    conflict_base_name(name).is_some()
}

/// Returns true if `name` belongs to the reserved `.omemfs-` namespace, in
/// either the standalone/dir-interior form (`.omemfs-<kind>`) or the
/// file-adjacent form (`<name>.omemfs-<kind>`). See design/09_reserved_names.md.
pub fn is_reserved_name(name: &str) -> bool {
    name.contains(".omemfs-")
}

/// Returns true if `name` is a reserved name that this version recognises:
/// `.omemfs-stub`, `.omemfs-conflict-{base,local,remote,meta}`, or
/// `.omemfs-filter` (in standalone or file-adjacent form). See
/// design/09_reserved_names.md.
pub fn is_known_reserved_name(name: &str) -> bool {
    stub::is_stub_filename(name)
        || crate::commands::conflict::is_conflict_artifact(name)
        || name == ".omemfs-filter"
}

/// Returns true if `name` matches the reserved `.omemfs-` namespace but is NOT
/// a kind this version recognises. Such files are produced by a newer version
/// of omemfs and must be left untouched (forward compatibility,
/// design/09_reserved_names.md "Forward compatibility").
pub fn is_unknown_reserved_name(name: &str) -> bool {
    is_reserved_name(name) && !is_known_reserved_name(name)
}

/// Strip a conflict-helper suffix from a filename, returning the base name.
/// Returns `None` if the name is not a conflict helper file.
pub fn conflict_base_name(name: &str) -> Option<&str> {
    crate::commands::conflict::CONFLICT_SUFFIXES
        .iter()
        .find_map(|suffix| name.strip_suffix(suffix))
}

/// Emit a one-time warning (per process, per path) about an unknown reserved
/// file. Deduplicated so a repeated scan in the same process is quiet.
fn warn_unknown_reserved(rel: &str) {
    use std::collections::HashSet;
    use std::sync::Mutex;
    use std::sync::OnceLock;
    static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let set = WARNED.get_or_init(|| Mutex::new(HashSet::new()));
    if let Ok(mut guard) = set.lock()
        && guard.insert(rel.to_string())
    {
        eprintln!(
            "warning: unknown reserved file '{}' produced by a newer omemfs version; skipping",
            rel
        );
    }
}

fn system_time_to_datetime(t: Option<SystemTime>) -> Option<DateTime<Utc>> {
    let t = t?;
    let dur = t.duration_since(std::time::UNIX_EPOCH).ok()?;
    Utc.timestamp_opt(dur.as_secs() as i64, dur.subsec_nanos())
        .single()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::local::LocalStore;
    use crate::tree_ops;
    use std::fs;

    /// Deterministic pseudo-random byte generator (xorshift64*), mirroring
    /// `codec::chunk`'s test helper of the same name. Only needs to be
    /// reproducible and "random enough" that FastCDC finds cut points, so a
    /// large-enough buffer reliably chunks into >= 2 pieces (CDC_MIN = 1 MiB).
    fn pseudo_random(seed: u64, len: usize) -> Vec<u8> {
        let mut state = seed | 1;
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            let v = state.wrapping_mul(0x2545F4914F6CDD1D);
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    /// With `write_blobs = false` (the ls/pull mode), the scan must:
    /// - return the SAME root hash as a `write_blobs = true` scan, and
    /// - write the tree objects (so diffing can read them back), but
    /// - NOT write any blob objects.
    ///   See design/03 "Scan blob-write mode".
    #[test]
    fn scan_write_blobs_false_skips_blobs_same_root() {
        let dir = tempfile::TempDir::new().unwrap();
        let work = dir.path();
        fs::write(work.join("a.txt"), b"hello world").unwrap();
        fs::write(work.join("b.txt"), b"foo bar baz quux").unwrap();
        let sub = work.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("c.txt"), b"nested content here").unwrap();

        // Reference scan with blobs written.
        let full_dir = tempfile::TempDir::new().unwrap();
        let full_store = LocalStore::for_cache(full_dir.path());
        let full =
            scan_and_store_with_cache(work, work, &full_store, None, &StatCache::default(), true)
                .unwrap();

        // Hash-only scan into a fresh store.
        let lite_dir = tempfile::TempDir::new().unwrap();
        let lite_store = LocalStore::for_cache(lite_dir.path());
        let lite =
            scan_and_store_with_cache(work, work, &lite_store, None, &StatCache::default(), false)
                .unwrap();

        // Same root hash regardless of blob writing.
        assert_eq!(
            full.root_hash, lite.root_hash,
            "root hash must not depend on write_blobs"
        );

        // Per-file hashes are recorded in both modes.
        assert_eq!(full.files.len(), lite.files.len());
        for (path, sf) in &full.files {
            assert_eq!(
                lite.files.get(path).map(|s| &s.hash),
                Some(&sf.hash),
                "hash mismatch for {}",
                path
            );
        }

        // The hash-only store holds the tree objects (root + sub) but no blobs.
        // Root tree entries must all resolve in the lite store (trees written).
        let root_entries = tree_ops::load_all_entries(&lite.root_hash, &lite_store).unwrap();
        assert_eq!(root_entries.len(), 3, "expected a.txt, b.txt, sub/");

        // Every blob hash from the full scan must be ABSENT from the lite store,
        // while every blob hash must be PRESENT in the full store.
        for sf in full.files.values() {
            assert!(
                full_store.exists(&sf.hash).unwrap(),
                "blob should exist in full store"
            );
            assert!(
                !lite_store.exists(&sf.hash).unwrap(),
                "blob must NOT exist in hash-only store"
            );
        }
    }

    /// Scan a temporary directory with two files, then verify that the aggregate
    /// metadata (size, blob_count) on the returned root tree entry matches what
    /// `tree_meta` would compute by reading the tree object back from the store.
    /// This guards against regressions where scan_dir calls tree_meta internally.
    #[test]
    fn scan_aggregate_matches_tree_meta() {
        let dir = tempfile::TempDir::new().unwrap();
        let work = dir.path();

        // Write two files so size and blob_count are non-trivial.
        fs::write(work.join("a.txt"), b"hello world").unwrap();
        fs::write(work.join("b.txt"), b"foo bar baz quux").unwrap();

        let store_dir = tempfile::TempDir::new().unwrap();
        let store = LocalStore::for_cache(store_dir.path());
        let stat_cache = StatCache::default();

        let result =
            scan_and_store_with_cache(work, work, &store, None, &stat_cache, true).unwrap();

        // Compute the expected aggregates via tree_meta (reads from store).
        let (meta_mtime, meta_size, meta_blob_count) =
            tree_ops::tree_meta(&result.root_hash, &store).unwrap();

        // Also compute via scan result: navigate root tree to get its entries.
        let entries = tree_ops::load_all_entries(&result.root_hash, &store).unwrap();
        let agg_size = crate::object::Tree::aggregate_size(&entries);
        let agg_blob_count = crate::object::Tree::aggregate_blob_count(&entries);
        let _agg_mtime = crate::object::Tree::aggregate_mtime(&entries);

        // The values computed during scan must equal tree_meta's values.
        assert_eq!(agg_size, meta_size, "aggregate size mismatch");
        assert_eq!(
            agg_blob_count, meta_blob_count,
            "aggregate blob_count mismatch"
        );
        assert_eq!(agg_size, 11 + 16, "expected total file size");
        assert_eq!(agg_blob_count, 2, "expected 2 blobs");
        // mtime is filesystem-dependent; just verify tree_meta agrees with agg.
        let _ = meta_mtime;
    }

    /// A push scan must not trust a clone-root metadata hit when its local blob
    /// is absent: upload must be sealed from later working-tree changes. It
    /// captures the file now, stores the blob, and records the result in cache.
    #[test]
    fn clone_root_fallback_missing_blob_is_staged_and_cached() {
        let dir = tempfile::TempDir::new().unwrap();
        let work = dir.path();

        let content = b"hello world";
        fs::write(work.join("a.txt"), content).unwrap();

        // Force the file's mtime to a fixed time well outside the racy window.
        let old_secs: i64 = 1_000_000_000;
        let ft = filetime::FileTime::from_unix_time(old_secs, 0);
        filetime::set_file_mtime(work.join("a.txt"), ft).unwrap();

        // Build a clone-root entry matching (mtime, size, hash) exactly.
        let hash = crate::object::blob_hash(content);
        let mtime = Utc.timestamp_opt(old_secs, 0).single();
        let mut clone_root: HashMap<String, TreeEntry> = HashMap::new();
        clone_root.insert(
            "a.txt".to_string(),
            TreeEntry::Blob {
                name: "a.txt".to_string(),
                hash: hash.clone(),
                mtime,
                size: content.len() as u64,
                mode: None,
            },
        );

        let store_dir = tempfile::TempDir::new().unwrap();
        let store = LocalStore::for_cache(store_dir.path());
        // Empty stat cache → forces the clone-root fallback path.
        let stat_cache = StatCache::default();

        let result =
            scan_and_store_with_cache(work, work, &store, Some(&clone_root), &stat_cache, true)
                .unwrap();

        let scanned = result.files.get("a.txt").expect("a.txt scanned");
        assert!(
            !scanned.cache_hit,
            "fallback hit must not be a direct cache hit"
        );
        assert!(
            !scanned.fallback_hit,
            "missing local blob must be captured, not metadata-only fallback"
        );
        assert_eq!(scanned.hash, hash);
        assert!(store.exists(&hash).unwrap(), "blob must be staged locally");

        // The newly captured value is eligible for STAT_CACHE refresh.
        let omemfs_dir = work.join(".omemfs");
        fs::create_dir_all(omemfs_dir.join("objects")).unwrap();
        let mut cache = StatCache::default();
        for (path, sf) in &result.files {
            if !sf.cache_hit {
                cache.update(
                    path.clone(),
                    sf.fs_mtime,
                    sf.fs_size,
                    sf.hash.clone(),
                    sf.blob_sealed,
                );
            }
        }
        assert!(cache.is_dirty(), "captured file must make the cache dirty");
        assert!(
            cache.contains("a.txt"),
            "captured file must be recorded in STAT_CACHE"
        );
    }

    /// Verify that a subdirectory's Tree entry carries the correct aggregate
    /// metadata without calling tree_meta (regression test for the read-back).
    #[test]
    fn scan_subdir_aggregate_correct() {
        let dir = tempfile::TempDir::new().unwrap();
        let work = dir.path();

        let sub = work.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("x.txt"), b"xxxxxxxxxx").unwrap(); // 10 bytes
        fs::write(sub.join("y.txt"), b"yyyyyy").unwrap(); // 6 bytes

        let store_dir = tempfile::TempDir::new().unwrap();
        let store = LocalStore::for_cache(store_dir.path());
        let stat_cache = StatCache::default();

        let result =
            scan_and_store_with_cache(work, work, &store, None, &stat_cache, true).unwrap();

        // The root tree should have one Tree entry ("sub") with size=16, blob_count=2.
        let root_entries = tree_ops::load_all_entries(&result.root_hash, &store).unwrap();
        assert_eq!(root_entries.len(), 1);
        match &root_entries[0] {
            crate::object::TreeEntry::Tree {
                size, blob_count, ..
            } => {
                assert_eq!(*size, 16, "subdir size");
                assert_eq!(*blob_count, 2, "subdir blob_count");
            }
            other => panic!("expected Tree entry, got {:?}", other.name()),
        }
    }

    /// design/03 "Parallel hash computation": hashing the files in a directory
    /// with a worker pool must not change the resulting tree hash, the per-file
    /// hashes, or the recorded side data. With many files in one directory the
    /// parallel path is exercised; the root hash must be deterministic across
    /// repeated runs (order-independent because entries are sorted by name).
    #[test]
    fn scan_parallel_hashing_is_deterministic_and_order_independent() {
        let dir = tempfile::TempDir::new().unwrap();
        let work = dir.path();

        // Enough files in a single directory to span the worker pool.
        let n = 50;
        for i in 0..n {
            fs::write(
                work.join(format!("f{:03}.txt", i)),
                format!("content number {}", i),
            )
            .unwrap();
        }

        // Scan twice into two independent stores; the root hash must match.
        let s1_dir = tempfile::TempDir::new().unwrap();
        let s1 = LocalStore::for_cache(s1_dir.path());
        let r1 =
            scan_and_store_with_cache(work, work, &s1, None, &StatCache::default(), false).unwrap();

        let s2_dir = tempfile::TempDir::new().unwrap();
        let s2 = LocalStore::for_cache(s2_dir.path());
        let r2 =
            scan_and_store_with_cache(work, work, &s2, None, &StatCache::default(), false).unwrap();

        assert_eq!(
            r1.root_hash, r2.root_hash,
            "root hash must be deterministic across runs"
        );
        assert_eq!(r1.files.len(), n, "all files recorded");
        assert_eq!(
            r1.files.len(),
            r2.files.len(),
            "same number of files both runs"
        );
        for (path, sf) in &r1.files {
            assert_eq!(
                r2.files.get(path).map(|s| &s.hash),
                Some(&sf.hash),
                "per-file hash mismatch for {} across runs",
                path
            );
        }

        // The tree object must list all n files (sorted by name).
        let entries = tree_ops::load_all_entries(&r1.root_hash, &s1).unwrap();
        assert_eq!(entries.len(), n, "tree lists every file");
        let mut names: Vec<&str> = entries.iter().map(|e| e.name()).collect();
        let sorted = {
            let mut c = names.clone();
            c.sort();
            c
        };
        names.dedup();
        assert_eq!(names.len(), n, "no duplicate entries from parallel merge");
        assert_eq!(names, sorted, "entries are sorted by name");
    }

    /// A content change must always be detected, even when the parallel walk
    /// could otherwise be tempted to skip a file: changing a file's bytes (while
    /// keeping the same size, and even forcing the SAME mtime) must change the
    /// per-file hash and therefore the root hash. This guards the invariant that
    /// the scan never skips a full read+hash based on mtime alone — without a
    /// STAT_CACHE or clone-root entry, every file is read and hashed
    /// (design/03 "Parallel walk and hash computation").
    #[test]
    fn scan_detects_content_change_without_mtime_skip() {
        let dir = tempfile::TempDir::new().unwrap();
        let work = dir.path();

        // A handful of files so more than one is hashed in parallel.
        for i in 0..8 {
            fs::write(work.join(format!("f{}.txt", i)), b"AAAAAAAA").unwrap();
        }
        // Pin every file to one fixed, old mtime (outside the racy window).
        let ft = filetime::FileTime::from_unix_time(1_000_000_000, 0);
        for i in 0..8 {
            filetime::set_file_mtime(work.join(format!("f{}.txt", i)), ft).unwrap();
        }

        let s1_dir = tempfile::TempDir::new().unwrap();
        let s1 = LocalStore::for_cache(s1_dir.path());
        let before =
            scan_and_store_with_cache(work, work, &s1, None, &StatCache::default(), false).unwrap();

        // Change the content of one file in place, keeping the SAME size and
        // resetting the SAME mtime. A naive mtime/size short-circuit would miss
        // this; a correct scan must re-read and produce a different hash.
        fs::write(work.join("f3.txt"), b"BBBBBBBB").unwrap();
        filetime::set_file_mtime(work.join("f3.txt"), ft).unwrap();

        let s2_dir = tempfile::TempDir::new().unwrap();
        let s2 = LocalStore::for_cache(s2_dir.path());
        let after =
            scan_and_store_with_cache(work, work, &s2, None, &StatCache::default(), false).unwrap();

        assert_ne!(
            before.root_hash, after.root_hash,
            "content change with unchanged size+mtime must change the root hash"
        );
        assert_ne!(
            before.files.get("f3.txt").map(|s| &s.hash),
            after.files.get("f3.txt").map(|s| &s.hash),
            "the changed file's hash must differ"
        );
        // Untouched files keep their hash.
        for i in [0, 1, 2, 4, 5, 6, 7] {
            let p = format!("f{}.txt", i);
            assert_eq!(
                before.files.get(&p).map(|s| &s.hash),
                after.files.get(&p).map(|s| &s.hash),
                "untouched file {} must keep its hash",
                p
            );
        }
    }

    /// A deep, wide tree must produce a stable root hash and visit every file.
    /// Exercises the parallel directory recursion (rayon::scope) across multiple
    /// nesting levels and confirms the parent-after-child ordering holds: the
    /// root hash is identical across repeated runs even though sibling
    /// subdirectories are scanned concurrently in nondeterministic order.
    ///
    /// The SAME directory is scanned twice so the test isolates ordering
    /// nondeterminism — filesystem mtimes are part of the serialised tree entries
    /// (and so part of the tree hash), so two independently created trees would
    /// legitimately differ; scanning one tree twice holds the mtimes fixed.
    #[test]
    fn scan_parallel_deep_tree_is_deterministic() {
        let work_dir = tempfile::TempDir::new().unwrap();
        let work = work_dir.path();
        // 6 top-level dirs, each with 3 subdirs, each holding 5 files: a fan-out
        // wide enough to populate the worker pool at several nesting levels.
        for a in 0..6 {
            for b in 0..3 {
                let d = work.join(format!("d{}/s{}", a, b));
                fs::create_dir_all(&d).unwrap();
                for c in 0..5 {
                    fs::write(d.join(format!("f{}.txt", c)), format!("{}-{}-{}", a, b, c)).unwrap();
                }
            }
        }

        let s1_dir = tempfile::TempDir::new().unwrap();
        let s1 = LocalStore::for_cache(s1_dir.path());
        let r1 =
            scan_and_store_with_cache(work, work, &s1, None, &StatCache::default(), false).unwrap();

        let s2_dir = tempfile::TempDir::new().unwrap();
        let s2 = LocalStore::for_cache(s2_dir.path());
        let r2 =
            scan_and_store_with_cache(work, work, &s2, None, &StatCache::default(), false).unwrap();

        assert_eq!(
            r1.root_hash, r2.root_hash,
            "deep-tree root hash must be deterministic"
        );
        assert_eq!(
            r1.files.len(),
            6 * 3 * 5,
            "every file across all levels is recorded"
        );
        assert_eq!(r1.files.len(), r2.files.len());
        for (path, sf) in &r1.files {
            assert_eq!(
                r2.files.get(path).map(|s| &s.hash),
                Some(&sf.hash),
                "per-file hash mismatch for {}",
                path
            );
        }
    }

    /// Parallel hashing must equal serial hashing: scanning the same tree with
    /// blob writing enabled must store every blob and yield a tree whose
    /// aggregate blob_count matches the file count, regardless of the order in
    /// which workers complete.
    #[test]
    fn scan_parallel_stores_all_blobs() {
        let dir = tempfile::TempDir::new().unwrap();
        let work = dir.path();

        let n = 40;
        for i in 0..n {
            fs::write(
                work.join(format!("g{:03}.bin", i)),
                format!("blob body {}", i),
            )
            .unwrap();
        }

        let store_dir = tempfile::TempDir::new().unwrap();
        let store = LocalStore::for_cache(store_dir.path());
        let result =
            scan_and_store_with_cache(work, work, &store, None, &StatCache::default(), true)
                .unwrap();

        // Every recorded blob must be present in the store.
        for (path, sf) in &result.files {
            assert!(store.exists(&sf.hash).unwrap(), "blob missing for {}", path);
        }

        let entries = tree_ops::load_all_entries(&result.root_hash, &store).unwrap();
        let blob_count = crate::object::Tree::aggregate_blob_count(&entries);
        assert_eq!(
            blob_count as usize, n,
            "aggregate blob_count matches file count"
        );
    }

    // -----------------------------------------------------------------------
    // SEALED skip (design/07 "Seal invalidation")
    // -----------------------------------------------------------------------

    /// A store wrapper that counts calls to `exists()`, so a test can assert
    /// exactly how many local-existence checks a scan performed -- the
    /// observable effect of the SEALED-skip optimisation.
    /// `exists_calls` counts every call (tree objects included: `build_and_store`
    /// calls `store.exists()` once per directory level for its own dedup check,
    /// entirely unrelated to STAT_CACHE sealing). `exists_hashes` additionally
    /// records which hashes were checked, so a test can assert something more
    /// precise than a raw count -- specifically, that a given *blob* hash was
    /// (or was not) passed to `exists()`, independent of how many tree-object
    /// checks happened alongside it.
    struct CountingExistsStore {
        inner: LocalStore,
        exists_calls: std::sync::atomic::AtomicUsize,
        exists_hashes: std::sync::Mutex<HashSet<Hash>>,
    }

    impl CountingExistsStore {
        fn new(inner: LocalStore) -> Self {
            CountingExistsStore {
                inner,
                exists_calls: std::sync::atomic::AtomicUsize::new(0),
                exists_hashes: std::sync::Mutex::new(HashSet::new()),
            }
        }

        fn reset(&self) {
            self.exists_calls
                .store(0, std::sync::atomic::Ordering::Relaxed);
            self.exists_hashes.lock().unwrap().clear();
        }

        fn was_checked(&self, hash: &Hash) -> bool {
            self.exists_hashes.lock().unwrap().contains(hash)
        }
    }

    impl crate::store::ObjectStore for CountingExistsStore {
        fn exists(&self, hash: &Hash) -> Result<bool, Error> {
            self.exists_calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.exists_hashes.lock().unwrap().insert(hash.clone());
            self.inner.exists(hash)
        }
        fn size(&self, hash: &Hash) -> Result<u64, Error> {
            self.inner.size(hash)
        }
        fn list_with_sizes(&self) -> Result<Vec<(String, u64)>, Error> {
            self.inner.list_with_sizes()
        }
        fn open_read(&self, hash: &Hash) -> Result<Box<dyn std::io::Read>, Error> {
            self.inner.open_read(hash)
        }
        fn write_from(&self, hash: &Hash, reader: &mut dyn std::io::Read) -> Result<(), Error> {
            self.inner.write_from(hash, reader)
        }
    }

    /// Backdate every regular file directly under `dir` outside the racy
    /// window, so a STAT_CACHE hit is deterministic in a test (no sleep).
    fn backdate_files(dir: &Path) {
        let ft = filetime::FileTime::from_unix_time(1_000_000_000, 0);
        for entry in fs::read_dir(dir).unwrap().flatten() {
            if entry.file_type().unwrap().is_file() {
                filetime::set_file_mtime(entry.path(), ft).unwrap();
            }
        }
    }

    /// Run one "push-shaped" scan-and-refresh cycle exactly the way
    /// `push_full` does: load happens by the caller (so it can inspect
    /// `seals_trusted`), this helper covers compute -> scan -> apply_scanned
    /// -> finalize -> write.
    fn run_push_like_scan(
        work: &Path,
        store: &dyn ObjectStore,
        omemfs_dir: &Path,
        write_blobs: bool,
    ) -> ScanResult {
        let mut cache = StatCache::read(omemfs_dir);
        if write_blobs {
            cache.compute_seals_trusted(omemfs_dir, store);
        }
        let result = scan_and_store_with_cache(work, work, store, None, &cache, write_blobs)
            .unwrap();
        apply_scanned(&mut cache, &result.files);
        if write_blobs {
            cache.finalize_seal_epoch(omemfs_dir);
        }
        cache.write_if_dirty(omemfs_dir).unwrap();
        result
    }

    /// Run `f` with `OMEMFS_SEAL_SAMPLE=0` (sampling disabled) for its
    /// duration, holding `stat_cache::SEAL_SAMPLE_ENV_LOCK` so concurrent
    /// tests -- in this module AND in `stat_cache.rs`, which reads/mutates the
    /// same process-global env var -- cannot observe a different value.
    /// Sampling itself calls `store.exists()` on up to `OMEMFS_SEAL_SAMPLE`
    /// sealed entries on every blob-writing scan where the epoch matches
    /// (design/07 "Sampling verification") -- setting it to `0` isolates the
    /// per-file skip condition these tests are about, so the count they
    /// assert reflects only `process_file_job`, not sampling.
    fn with_seal_sampling_disabled<R>(f: impl FnOnce() -> R) -> R {
        let _guard = crate::stat_cache::SEAL_SAMPLE_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::set_var("OMEMFS_SEAL_SAMPLE", "0");
        }
        let result = f();
        unsafe {
            std::env::remove_var("OMEMFS_SEAL_SAMPLE");
        }
        result
    }

    /// End-to-end regression for design/07 "Seal invalidation" and the
    /// "process_file_job's exact skip condition": pushing the same unchanged
    /// tree twice must not call `store.exists()` with either file's *blob*
    /// hash on the second pass. Mirrors `push`'s own sequence (StatCache::read
    /// -> compute_seals_trusted -> scan -> apply_scanned -> finalize_seal_epoch
    /// -> write) without going through the `push` command. Sampling is
    /// disabled (see `with_seal_sampling_disabled`) so the assertion isolates
    /// the per-file skip condition rather than sampling's own bounded
    /// exists() calls. `store.exists()` is still called once per directory
    /// level for the (unrelated) tree-object dedup check in
    /// `tree_ops::build_and_store` -- that is not what this test is about, so
    /// it asserts on specific blob hashes rather than a raw call count.
    #[test]
    fn seal_skips_store_exists_on_unchanged_second_scan() {
        let dir = tempfile::TempDir::new().unwrap();
        let work = dir.path();
        fs::write(work.join("a.txt"), b"hello world").unwrap();
        fs::write(work.join("b.txt"), b"foo bar baz quux").unwrap();
        backdate_files(work);

        let omemfs_dir = work.join(".omemfs");
        fs::create_dir_all(omemfs_dir.join("objects")).unwrap();
        let store = CountingExistsStore::new(LocalStore::for_cache(omemfs_dir.join("objects")));

        with_seal_sampling_disabled(|| {
            // First "push": no cache yet, everything is hashed and written fresh.
            let result1 = run_push_like_scan(work, &store, &omemfs_dir, true);
            assert_eq!(result1.files.len(), 2);
            let a_hash = result1.files["a.txt"].hash.clone();
            let b_hash = result1.files["b.txt"].hash.clone();

            // Second "push" over the identical, unchanged tree.
            store.reset();
            let result2 = run_push_like_scan(work, &store, &omemfs_dir, true);
            assert_eq!(result2.files.len(), 2);

            assert!(
                !store.was_checked(&a_hash),
                "an unchanged, fully-sealed second scan must never call \
                 store.exists() on a.txt's blob hash"
            );
            assert!(
                !store.was_checked(&b_hash),
                "an unchanged, fully-sealed second scan must never call \
                 store.exists() on b.txt's blob hash"
            );
        });
    }

    /// A file discovered by a read-only scan (`ls`, `write_blobs = false`) is
    /// cached but not sealed. The next blob-writing scan (`push`) must still
    /// verify it via `store.exists()` (or capture it, since the blob was never
    /// written) -- but the scan AFTER that, over the same unchanged tree, must
    /// skip the check entirely once the file is sealed. Sampling is disabled
    /// (see `with_seal_sampling_disabled`) for the final assertion so it
    /// isolates the per-file skip condition. `store.exists()` is still called
    /// once per directory level for the (unrelated) tree-object dedup check in
    /// `tree_ops::build_and_store`, so assertions target `c.txt`'s specific
    /// blob hash rather than a raw call count.
    #[test]
    fn ls_then_push_seals_and_a_third_push_skips_exists() {
        let dir = tempfile::TempDir::new().unwrap();
        let work = dir.path();
        fs::write(work.join("c.txt"), b"new content").unwrap();
        backdate_files(work);

        let omemfs_dir = work.join(".omemfs");
        fs::create_dir_all(omemfs_dir.join("objects")).unwrap();
        let store = CountingExistsStore::new(LocalStore::for_cache(omemfs_dir.join("objects")));

        with_seal_sampling_disabled(|| {
            // "ls": read-only scan, hashes the file but writes no blob and
            // never seals it.
            let ls_result = run_push_like_scan(work, &store, &omemfs_dir, false);
            let sf = ls_result.files.get("c.txt").expect("c.txt scanned by ls");
            let c_hash = sf.hash.clone();
            assert!(!sf.blob_sealed, "a read-only scan must never seal");
            assert!(
                !store.inner.exists(&c_hash).unwrap(),
                "ls must not have written the blob"
            );

            // "push": blob-writing scan over the STAT_CACHE entry ls just left
            // behind (cache_hit=true, but not sealed) -- must consult
            // store.exists() (branch 3 of the skip condition), find it
            // missing, and fall through to a full capture that writes the
            // blob and seals it.
            store.reset();
            let push_result = run_push_like_scan(work, &store, &omemfs_dir, true);
            assert!(
                store.was_checked(&c_hash),
                "the first blob-writing scan over an unsealed cache hit must call \
                 store.exists() on c.txt's blob hash"
            );
            let sf = push_result
                .files
                .get("c.txt")
                .expect("c.txt scanned by push");
            assert_eq!(sf.hash, c_hash, "content is unchanged, so the hash is stable");
            assert!(
                store.inner.exists(&c_hash).unwrap(),
                "push must capture the blob"
            );

            // A second "push" over the now-sealed, unchanged tree must skip
            // store.exists() for c.txt's blob hash entirely.
            store.reset();
            run_push_like_scan(work, &store, &omemfs_dir, true);
            assert!(
                !store.was_checked(&c_hash),
                "a scan following a sealed, trusted, unchanged tree must skip \
                 store.exists() on c.txt's blob hash"
            );
        });
    }

    /// Sampling verification (design/07 "Sampling verification"): when a
    /// sealed entry's local object goes missing but `.seal_epoch` is
    /// untouched (a partial loss, not a whole-directory reset), the next
    /// blob-writing scan must detect it via sampling, downgrade
    /// `seals_trusted` for that whole scan, and recapture the missing blob
    /// (never silently skip it). Relies on the ambient (default) sampling
    /// size, so it holds `stat_cache::SEAL_SAMPLE_ENV_LOCK` for mutual
    /// exclusion with the tests (here and in `stat_cache.rs`) that override
    /// `OMEMFS_SEAL_SAMPLE`.
    #[test]
    fn sampling_detects_partial_loss_and_falls_back_to_full_verification() {
        let _guard = crate::stat_cache::SEAL_SAMPLE_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert!(
            std::env::var("OMEMFS_SEAL_SAMPLE").is_err(),
            "ambient OMEMFS_SEAL_SAMPLE would invalidate this test's default-sampling assumption"
        );

        let dir = tempfile::TempDir::new().unwrap();
        let work = dir.path();
        fs::write(work.join("a.txt"), b"hello world").unwrap();
        fs::write(work.join("b.txt"), b"foo bar baz quux").unwrap();
        backdate_files(work);

        let omemfs_dir = work.join(".omemfs");
        fs::create_dir_all(omemfs_dir.join("objects")).unwrap();
        let store = CountingExistsStore::new(LocalStore::for_cache(omemfs_dir.join("objects")));
        let _ = run_push_like_scan(work, &store, &omemfs_dir, true);

        // Simulate a partial loss: delete one blob's local object file, but
        // leave `.seal_epoch` (and every other object) untouched.
        let cache = StatCache::read(&omemfs_dir);
        let a_hash = cache.lookup_current(
            "a.txt",
            fs::metadata(work.join("a.txt")).unwrap().modified().unwrap(),
            fs::metadata(work.join("a.txt")).unwrap().len(),
        );
        let a_hash = a_hash.expect("a.txt must be a direct cache hit").clone();
        assert!(store.inner.exists(&a_hash).unwrap());
        let object_path = omemfs_dir.join("objects").join(a_hash.as_str());
        fs::remove_file(&object_path).expect("remove a.txt's local object");

        // The default OMEMFS_SEAL_SAMPLE (256) covers every sealed entry in
        // this 2-file cache, so sampling must catch the loss deterministically
        // (no env var override needed).
        store.reset();
        let result = run_push_like_scan(work, &store, &omemfs_dir, true);

        assert!(
            store.was_checked(&a_hash),
            "sampling must have downgraded trust, forcing a real store.exists() \
             check on a.txt's blob hash"
        );
        assert!(
            store.inner.exists(&a_hash).unwrap(),
            "the missing blob must have been recaptured, not silently skipped"
        );
        let sf = result.files.get("a.txt").expect("a.txt rescanned");
        assert_eq!(sf.hash, a_hash, "content is unchanged, so the hash is stable");
    }

    // -----------------------------------------------------------------------
    // Depth of the local-existence check (design/07 "Depth of the
    // local-existence check", defect 1): a STAT_CACHE hit or clone-root
    // fallback hit must be verified with `blob_fully_cached` (manifest AND
    // every chunk), never a bare `store.exists()` on the manifest hash alone.
    // -----------------------------------------------------------------------

    /// (defect 1a) A STAT_CACHE hit whose entry is `sealed` but whose seal
    /// trust was never established this scan (`seals_trusted() == false`)
    /// falls through to the third branch of `process_file_job`'s skip
    /// condition. For a chunked blob whose manifest object is present but one
    /// chunk has been deleted, a shallow `store.exists()` on the manifest hash
    /// would wrongly say "present"; `blob_fully_cached` must catch the missing
    /// chunk and force a full recapture.
    #[test]
    fn stat_cache_hit_with_missing_chunk_forces_recapture_not_shallow_exists() {
        let dir = tempfile::TempDir::new().unwrap();
        let work = dir.path();
        let content = pseudo_random(0xABCD_EF01, 10 * 1024 * 1024);
        fs::write(work.join("big.bin"), &content).unwrap();
        let ft = filetime::FileTime::from_unix_time(1_000_000_000, 0);
        filetime::set_file_mtime(work.join("big.bin"), ft).unwrap();

        let store_dir = tempfile::TempDir::new().unwrap();
        let store = LocalStore::for_cache(store_dir.path());

        // First scan: writes the manifest + all chunks, records a sealed
        // entry (blob_sealed follows write_blobs=true).
        let mut cache = StatCache::default();
        let first =
            scan_and_store_with_cache(work, work, &store, None, &cache, true).unwrap();
        apply_scanned(&mut cache, &first.files);
        let sf = first.files.get("big.bin").expect("scanned");
        assert!(!sf.cache_hit, "first scan is always a miss");
        let hash = sf.hash.clone();
        assert!(
            cache.is_sealed("big.bin"),
            "a write_blobs scan must seal a freshly captured entry"
        );

        // Confirm chunking actually happened (a manifest object, not a flat
        // single-object blob).
        let manifest_raw = codec::store_read(&store, &hash, None).unwrap();
        let chunk_hashes = crate::object::deserialise_manifest(&manifest_raw)
            .expect("a 10 MiB pseudo-random file must be chunked");
        assert!(chunk_hashes.len() >= 2, "expected at least 2 chunks");

        // Delete exactly one chunk, keeping the manifest object itself intact.
        let victim = chunk_hashes[0].clone();
        let victim_path = store_dir.path().join(victim.as_str());
        assert!(victim_path.exists(), "victim chunk must exist before removal");
        fs::remove_file(&victim_path).unwrap();
        assert!(
            store.exists(&hash).unwrap(),
            "the manifest object itself must still be present (only a chunk was deleted)"
        );

        // Second scan reuses the same cache: `compute_seals_trusted` was
        // never called, so `seals_trusted()` stays false and the skip
        // condition's middle branch cannot short-circuit -- it must fall
        // through to the (now blob_fully_cached) third branch.
        let second = scan_and_store_with_cache(work, work, &store, None, &cache, true).unwrap();
        let sf2 = second.files.get("big.bin").expect("scanned again");
        assert!(
            !sf2.cache_hit,
            "a STAT_CACHE hit whose manifest exists but is missing a chunk must not \
             be accepted as present -- blob_fully_cached must catch it and force a \
             full recapture, not a shallow store.exists() on the manifest hash alone"
        );
        assert_eq!(sf2.hash, hash, "content is unchanged, so the hash is stable");
        assert!(
            store.exists(&victim).unwrap(),
            "the missing chunk must have been recaptured by the full re-read"
        );
    }

    /// (defect 1b) The clone-root `(mtime, size)` fallback (a STAT_CACHE
    /// *miss*) must apply the same `blob_fully_cached` depth as the
    /// STAT_CACHE-hit branch. Accepting a fallback hit on a manifest-only
    /// `store.exists()` while a chunk is missing would re-establish
    /// `sealed = true` on a blob that is not actually retrievable, which is
    /// exactly the gap that stops L3 recovery from converging (design/07
    /// "Depth of the local-existence check").
    #[test]
    fn clone_root_fallback_with_missing_chunk_forces_recapture() {
        let dir = tempfile::TempDir::new().unwrap();
        let work = dir.path();
        let content = pseudo_random(0x1357_9BDF, 10 * 1024 * 1024);
        fs::write(work.join("big.bin"), &content).unwrap();
        let old_secs: i64 = 1_000_000_000;
        let ft = filetime::FileTime::from_unix_time(old_secs, 0);
        filetime::set_file_mtime(work.join("big.bin"), ft).unwrap();

        let store_dir = tempfile::TempDir::new().unwrap();
        let store = LocalStore::for_cache(store_dir.path());

        // Populate the store with the blob (manifest + chunks) via a first
        // scan with no clone root and an empty STAT_CACHE.
        let first =
            scan_and_store_with_cache(work, work, &store, None, &StatCache::default(), true)
                .unwrap();
        let hash = first.files["big.bin"].hash.clone();
        let manifest_raw = codec::store_read(&store, &hash, None).unwrap();
        let chunk_hashes = crate::object::deserialise_manifest(&manifest_raw)
            .expect("a 10 MiB pseudo-random file must be chunked");
        let victim = chunk_hashes[0].clone();
        fs::remove_file(store_dir.path().join(victim.as_str())).unwrap();
        assert!(store.exists(&hash).unwrap(), "manifest still present");

        // Build a clone-root entry matching (mtime, size, hash) exactly, and
        // scan with an EMPTY STAT_CACHE so process_file_job takes the
        // clone-root fallback branch (a STAT_CACHE miss), not the cache-hit
        // branch.
        let mtime = Utc.timestamp_opt(old_secs, 0).single();
        let mut clone_root: HashMap<String, TreeEntry> = HashMap::new();
        clone_root.insert(
            "big.bin".to_string(),
            TreeEntry::Blob {
                name: "big.bin".to_string(),
                hash: hash.clone(),
                mtime,
                size: content.len() as u64,
                mode: None,
            },
        );

        let second = scan_and_store_with_cache(
            work,
            work,
            &store,
            Some(&clone_root),
            &StatCache::default(),
            true,
        )
        .unwrap();
        let sf = second
            .files
            .get("big.bin")
            .expect("scanned via clone-root fallback path");
        assert!(
            !sf.fallback_hit,
            "a clone-root fallback whose manifest exists but is missing a chunk must \
             not be accepted as present -- blob_fully_cached must catch it and force \
             a full recapture instead of a metadata-only fallback"
        );
        assert!(!sf.cache_hit);
        assert_eq!(sf.hash, hash, "content is unchanged, so the hash is stable");
        assert!(
            store.exists(&victim).unwrap(),
            "the missing chunk must have been recaptured by the full re-read"
        );
    }
}
