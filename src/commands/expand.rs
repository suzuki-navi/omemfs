use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use filetime::FileTime;

use crate::codec;
use crate::dtimer_l1;
use crate::error::Error;
use crate::io_stats;
use crate::object::{Tree, TreeEntry};
use crate::repo::Repo;
use crate::store::ObjectStore;
use crate::store::stats::IoRecord;
use crate::stub::{self, StubTargetType};

pub struct ExpandOptions {
    pub work_dir: PathBuf,
    /// Directory the command was invoked from; relative paths resolve against it.
    pub current_dir: PathBuf,
    /// Paths to expand (relative to the cwd). If empty, expand all stubs.
    pub paths: Vec<PathBuf>,
    pub remote_name: String,
    pub dry_run: bool,
    /// Only expand stubs whose size is strictly below this threshold. 0 means expand all.
    pub stub_threshold: u64,
}

pub fn run(opts: ExpandOptions) -> Result<(), Error> {
    let started = std::time::Instant::now();
    let repo = Repo::open(&opts.work_dir)?;
    crate::progress::notify_repo_dir(&repo.work_dir);
    // Hold the repo lock: expand writes working-tree files and must not race
    // with a concurrent push scanning a partially-written file. See
    // design/12_locking.md.
    let _lock = repo.acquire_lock()?;
    let _t = dtimer_l1!("expand");
    let local = repo.local_store();

    // Collect all stubs, then apply path scope filter.
    // When no paths given, default to the current working directory.
    let all_stubs = stub::list(&repo.work_dir)?;
    let scoped_paths: Vec<String> = if opts.paths.is_empty() {
        Vec::new()
    } else {
        opts.paths
            .iter()
            .map(|path| crate::repo::normalize_path(path, &repo.work_dir, &opts.current_dir))
            .collect::<Result<_, _>>()?
    };
    let scoped: Vec<(String, crate::stub::StubRecord)> =
        if opts.paths.is_empty() || scoped_paths.iter().any(|scope| scope.is_empty()) {
            all_stubs
        } else {
            all_stubs
                .into_iter()
                .filter(|(rel, _)| {
                    scoped_paths
                        .iter()
                        .any(|scope| rel == scope || rel.starts_with(&format!("{}/", scope)))
                })
                .collect()
        };

    // Apply stub_threshold: partition into (expand, keep-stubbed).
    // threshold == 0 means expand everything.
    // Stubs that exactly match a user-specified path bypass the threshold.
    let (to_expand, kept_stubbed): (Vec<_>, Vec<_>) =
        scoped.into_iter().partition(|(rel, record)| {
            opts.stub_threshold == 0
                || record.size < opts.stub_threshold
                || scoped_paths.iter().any(|scope| rel == scope)
        });

    if to_expand.is_empty() {
        if !kept_stubbed.is_empty() {
            crate::progress::emit_output_line(&format!(
                "Nothing to expand ({} stub(s) at or above threshold kept).",
                kept_stubbed.len()
            ));
        } else {
            crate::progress::emit_output_line("Nothing to expand.");
        }
        return Ok(());
    }

    let io_record = Arc::new(IoRecord::default());
    let (pack_reader, _remote, remote_key) =
        repo.pack_reader(&opts.remote_name, Some(&io_record))?;
    let remote_key = remote_key.as_ref();

    let phase = crate::progress::begin_phase("Expand stubs");

    // Phase 1 (Improvement B, design/02_storage_format.md "Multi-root
    // batching"): walk the whole target scope and collect every blob hash that
    // is missing from the local cache, WITHOUT fetching or writing anything.
    // Then fetch the entire batch with a single multi-root transfer, so the
    // worker pool has all of the (typically childless) leaf blobs to divide
    // between its threads at once. Phase 2 below is the unchanged
    // materialisation walk; it finds every batched blob already cached.
    //
    // --dry-run must not fetch any object or touch the local cache, so the
    // collection walk is skipped entirely in that mode.
    if !opts.dry_run {
        let mut pending: Vec<crate::object::Hash> = Vec::new();
        for (_rel_path, record) in &to_expand {
            match &record.target_type {
                StubTargetType::Tree => {
                    collect_tree_fetches(
                        &record.hash,
                        &local,
                        &pack_reader,
                        remote_key,
                        opts.stub_threshold,
                        &mut pending,
                    )?;
                }
                StubTargetType::Blob => {
                    if !local.exists(&record.hash)? {
                        pending.push(record.hash.clone());
                    }
                }
            }
        }
        crate::commands::push::transfer_objects_many(
            &pack_reader,
            &local,
            &pending,
            remote_key,
            true,
        )?;
    }

    let mut count = 0usize;
    for (rel_path, record) in &to_expand {
        match &record.target_type {
            StubTargetType::Tree => {
                if opts.dry_run {
                    crate::progress::emit_output_line(&format!("  would expand: {}", rel_path));
                    continue;
                }
                // The tree object is fetched on demand by expand_tree below.

                let abs_dir = opts.work_dir.join(rel_path);
                // Recursively materialise the tree into the directory, re-stubbing children
                // that are at or above the threshold.
                // The directory stub marker is removed AFTER successful expansion so that
                // a mid-expansion failure leaves the stub in place for retry.
                // expand_tree returns the number of files (blobs + symlinks)
                // actually materialised, so the reported count reflects files
                // written rather than the number of top-level stub records
                // processed.
                let materialised = expand_tree(
                    &record.hash,
                    &abs_dir,
                    &opts.work_dir,
                    rel_path,
                    &local,
                    &pack_reader,
                    remote_key,
                    opts.stub_threshold,
                )?;
                // Remove the directory stub marker only on success.
                stub::remove_dir_stub(&repo.work_dir, rel_path)?;

                if let Some(mt) = record.mtime {
                    let ft = FileTime::from_unix_time(mt.timestamp(), mt.timestamp_subsec_nanos());
                    filetime::set_file_mtime(&abs_dir, ft).ok();
                }
                count += materialised;
            }
            StubTargetType::Blob => {
                // --dry-run is a pure report: it must not fetch any object from
                // the remote or write to the local cache (design/04 expand
                // --dry-run: "show what would be expanded without writing files
                // or removing stub records").
                if opts.dry_run {
                    crate::progress::emit_output_line(&format!("  would expand: {}", rel_path));
                    continue;
                }

                // Ensure the blob (and all its chunks) are in the local cache.
                // transfer_objects walks per object, so peak memory stays
                // bounded by one chunk (≤ CDC_MAX) regardless of file size.
                if !local.exists(&record.hash)? {
                    crate::commands::push::transfer_objects(
                        &pack_reader,
                        &local,
                        &record.hash,
                        remote_key,
                        true,
                    )?;
                }

                let abs_path = opts.work_dir.join(rel_path);
                if let Some(parent) = abs_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                // Streaming materialisation from the local cache (bounded memory).
                codec::chunk::materialise_to_file(&local, &record.hash, None, &abs_path)?;

                if let Some(mt) = record.mtime {
                    let ft = FileTime::from_unix_time(mt.timestamp(), mt.timestamp_subsec_nanos());
                    filetime::set_file_mtime(&abs_path, ft).ok();
                }
                // The temp + rename in materialise_to_file resets permissions —
                // the mode must be re-applied after it.
                crate::fsmeta::apply_mode(&abs_path, &record.mode);

                stub::remove(&repo.work_dir, rel_path)?;
                count += 1;
            }
        }
    }

    if opts.dry_run {
        let summary = if !kept_stubbed.is_empty() {
            format!(
                "{} would expand, {} kept",
                to_expand.len(),
                kept_stubbed.len()
            )
        } else {
            format!("{} would expand", to_expand.len())
        };
        phase.complete(summary);
        if !kept_stubbed.is_empty() {
            crate::progress::emit_output_line(&format!(
                "{} stub(s) would be expanded, {} stub(s) at or above threshold kept.",
                to_expand.len(),
                kept_stubbed.len()
            ));
        } else {
            crate::progress::emit_output_line(&format!(
                "{} stub(s) would be expanded.",
                to_expand.len()
            ));
        }
    } else {
        let summary = if !kept_stubbed.is_empty() {
            format!("{} expanded, {} kept", count, kept_stubbed.len())
        } else {
            format!("{} expanded", count)
        };
        phase.complete(summary);
        if !kept_stubbed.is_empty() {
            crate::progress::emit_output_line(&format!(
                "{} file(s) expanded, {} stub(s) at or above threshold kept.",
                count,
                kept_stubbed.len()
            ));
        } else {
            crate::progress::emit_output_line(&format!("{} file(s) expanded.", count));
        }
    }

    if !opts.dry_run {
        let omemfs_dir = repo.work_dir.join(".omemfs");
        let duration_ms = started.elapsed().as_millis() as u64;
        io_stats::append_record(
            &omemfs_dir,
            "expand",
            &opts.remote_name,
            &io_record,
            duration_ms,
        );
    }
    Ok(())
}

/// Plan the blob fetches a whole `expand_tree` run will need, without fetching
/// any blob or touching the working tree (phase 1 of the two-phase expansion —
/// design/02_storage_format.md, "Multi-root batching (Improvement B)").
///
/// Appends to `out` every blob hash, at any depth below `tree_hash`, that is
/// missing from `local` and will therefore have to come from `remote`. The
/// caller fetches the whole batch with one `transfer_objects_many` call, so the
/// worker pool can pull independent (typically childless) leaf blobs
/// concurrently instead of one per sequential `transfer_objects` call.
///
/// Tree objects are still read one at a time here: their bytes are what reveals
/// their children, so they cannot be part of the batch (design/04_cli_spec.md,
/// expand step 3). Reading them through `ensure_local_then_read` caches them
/// locally, so the materialisation walk re-reads them from the local cache
/// rather than the remote.
///
/// Entries at or above `stub_threshold` are deliberately **not** planned. Whether
/// such an entry is expanded or left stubbed depends on
/// `stub::stub_would_be_visible_to_git`, a `git check-ignore` subprocess whose
/// answer can change as the materialisation walk writes files; evaluating it here
/// would both duplicate the subprocess and move the decision earlier than it
/// happens today. Those entries therefore keep their existing per-entry
/// behaviour (the materialisation walk's own `local.exists` fallback fetches one
/// if it does turn out to need expanding), and batching covers exactly the
/// unconditionally-expanded blobs below the threshold — the many-small-files case
/// Improvement B targets.
fn collect_tree_fetches(
    tree_hash: &crate::object::Hash,
    local: &dyn ObjectStore,
    remote: &dyn ObjectStore,
    remote_key: Option<&crate::codec::encrypt::EncryptKey>,
    stub_threshold: u64,
    out: &mut Vec<crate::object::Hash>,
) -> Result<(), Error> {
    // Tree discovery used to be a recursive DFS, so one remote tree lookup
    // had to finish before even a sibling tree could start. Walk tree nodes
    // through the shared worker engine instead; blobs are collected as roots
    // for the following content transfer, not traversed here.
    let pending = Mutex::new(HashSet::<crate::object::Hash>::new());
    let discover = |hash: &crate::object::Hash| -> Result<Vec<crate::object::Hash>, Error> {
        let data = codec::ensure_local_then_read(remote, local, hash, remote_key)?;
        let Tree::Normal { entries } = Tree::deserialise(&data)?;
        let mut child_trees = Vec::new();
        for entry in entries {
            match entry {
                TreeEntry::Blob { hash, size, .. } => {
                    if (stub_threshold == 0 || size < stub_threshold) && !local.exists(&hash)? {
                        pending.lock().unwrap().insert(hash);
                    }
                }
                TreeEntry::Tree { hash, size, .. } => {
                    if stub_threshold == 0 || size < stub_threshold {
                        child_trees.push(hash);
                    }
                }
                TreeEntry::Symlink { .. } => {}
            }
        }
        Ok(child_trees)
    };
    crate::commands::transfer::parallel_walk(
        std::slice::from_ref(tree_hash),
        crate::commands::transfer::resolve_concurrency(remote),
        &discover,
    )?;
    out.extend(pending.into_inner().unwrap());
    Ok(())
}

/// Expand the tree at `tree_hash` into `base_dir`: plan every blob fetch it
/// needs, pull them all in one batched multi-root transfer, then materialise.
///
/// Returns the number of files (blobs and symlinks) actually materialised into
/// the working tree. Children left stubbed do not count; nested directories
/// contribute the files materialised within them.
///
/// This is the self-contained entry point. `run` above widens the batch further
/// by planning across *every* stub it is expanding (top-level blob stubs
/// included) and issuing a single transfer before this point, which leaves this
/// function's own planning pass finding everything already cached — a local
/// re-walk with no remote I/O.
fn expand_tree(
    tree_hash: &crate::object::Hash,
    base_dir: &std::path::Path,
    work_dir: &std::path::Path,
    rel_base: &str,
    local: &dyn ObjectStore,
    remote: &dyn ObjectStore,
    remote_key: Option<&crate::codec::encrypt::EncryptKey>,
    stub_threshold: u64,
) -> Result<usize, Error> {
    let mut pending: Vec<crate::object::Hash> = Vec::new();
    collect_tree_fetches(
        tree_hash,
        local,
        remote,
        remote_key,
        stub_threshold,
        &mut pending,
    )?;
    crate::commands::push::transfer_objects_many(remote, local, &pending, remote_key, true)?;

    expand_tree_materialise(
        tree_hash,
        base_dir,
        work_dir,
        rel_base,
        local,
        remote,
        remote_key,
        stub_threshold,
    )
}

/// Recursively materialise the tree at `tree_hash` into `base_dir` (phase 2).
/// Downloads any objects still missing from `local` from `remote`.
/// Children whose size is >= `stub_threshold` (and threshold > 0) are left as stubs
/// rather than being fully expanded.
///
/// Returns the number of files (blobs and symlinks) actually materialised into
/// the working tree. Children left stubbed do not count; nested directories
/// contribute the files materialised within them.
///
/// The per-blob `transfer_objects` call below is now a fallback rather than the
/// normal path: `collect_tree_fetches` + `transfer_objects_many` has already
/// batched every blob it could plan for, so this only fires for a blob the
/// planner deliberately skipped (an at-or-above-threshold entry that turns out
/// to be expanded because its stub would be visible to Git).
fn expand_tree_materialise(
    tree_hash: &crate::object::Hash,
    base_dir: &std::path::Path,
    work_dir: &std::path::Path,
    rel_base: &str,
    local: &dyn ObjectStore,
    remote: &dyn ObjectStore,
    remote_key: Option<&crate::codec::encrypt::EncryptKey>,
    stub_threshold: u64,
) -> Result<usize, Error> {
    fs::create_dir_all(base_dir)?;
    let mut materialised = 0usize;

    let data = codec::ensure_local_then_read(remote, local, tree_hash, remote_key)?;
    let Tree::Normal { entries } = Tree::deserialise(&data)?;

    for entry in entries {
        match entry {
            TreeEntry::Blob {
                name,
                hash,
                size,
                mtime,
                mode,
            } => {
                let rel_path = format!("{}/{}", rel_base, name);
                let abs_for_stub = base_dir.join(&name);
                let stub_path = stub::file_stub_path_for(&abs_for_stub);
                let keep_stubbed = stub_threshold > 0
                    && size >= stub_threshold
                    && !stub::stub_would_be_visible_to_git(&stub_path, work_dir);
                if keep_stubbed {
                    // Leave as a stub.
                    stub::write(
                        work_dir,
                        &rel_path,
                        &stub::StubRecord {
                            target_type: stub::StubTargetType::Blob,
                            hash,
                            size,
                            mtime,
                            mode,
                            blob_count: 0,
                        },
                    )?;
                    continue;
                }
                let abs = base_dir.join(&name);
                // Ensure the blob (and all chunks) are cached locally, walking
                // per object so peak memory stays bounded by one chunk.
                if !local.exists(&hash)? {
                    crate::commands::push::transfer_objects(
                        remote, local, &hash, remote_key, true,
                    )?;
                }
                crate::fsmeta::materialise_blob_at(local, &hash, &abs, &mtime, &mode)?;
                materialised += 1;
            }
            TreeEntry::Tree {
                name,
                hash,
                mtime,
                size,
                blob_count,
            } => {
                let rel_path = format!("{}/{}", rel_base, name);
                let sub_dir = base_dir.join(&name);
                let dir_stub_path = sub_dir.join(stub::DIR_STUB_NAME);
                let keep_stubbed = stub_threshold > 0
                    && size >= stub_threshold
                    && !stub::stub_would_be_visible_to_git(&dir_stub_path, work_dir);
                if keep_stubbed {
                    // Leave as a directory stub.
                    fs::create_dir_all(&sub_dir)?;
                    stub::write_dir_stub(
                        work_dir,
                        &rel_path,
                        &stub::StubRecord {
                            target_type: stub::StubTargetType::Tree,
                            hash,
                            size,
                            mtime,
                            mode: None,
                            blob_count,
                        },
                    )?;
                    continue;
                }
                materialised += expand_tree_materialise(
                    &hash,
                    &sub_dir,
                    work_dir,
                    &rel_path,
                    local,
                    remote,
                    remote_key,
                    stub_threshold,
                )?;
                if let Some(mt) = mtime {
                    let ft = FileTime::from_unix_time(mt.timestamp(), mt.timestamp_subsec_nanos());
                    filetime::set_file_mtime(&sub_dir, ft).ok();
                }
            }
            TreeEntry::Symlink {
                name,
                target,
                mtime,
            } => {
                #[cfg(unix)]
                {
                    let link_path = base_dir.join(&name);
                    crate::fsmeta::write_symlink_atomic(&link_path, &target)?;
                    crate::fsmeta::restore_symlink_mtime(&link_path, &mtime);
                }
                let _ = target;
                materialised += 1;
            }
        }
    }
    Ok(materialised)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::Hash;
    use crate::store::local::LocalStore;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tempfile::TempDir;

    /// Read-side `ObjectStore` wrapper that tracks concurrently-in-flight
    /// `open_read` calls (with an artificial delay to make overlap
    /// observable) and forces a fixed `default_transfer_concurrency()`.
    /// Duplicated from the equivalent fixture in `push.rs`'s and `pull.rs`'s
    /// tests -- there is no shared test-utility module in this crate today
    /// (see e.g. `transfer.rs`'s own self-contained `GraphStore` fixture for
    /// the same per-file convention), and forcing the concurrency value
    /// directly avoids mutating the process-wide `OMEMFS_TRANSFER_CONCURRENCY`
    /// env var across Rust's parallel test runner.
    struct ConcurrencyTrackingStore {
        inner: LocalStore,
        forced_concurrency: usize,
        delay: Duration,
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
    }

    impl ConcurrencyTrackingStore {
        fn new(inner: LocalStore, forced_concurrency: usize, delay: Duration) -> Self {
            ConcurrencyTrackingStore {
                inner,
                forced_concurrency,
                delay,
                in_flight: AtomicUsize::new(0),
                max_in_flight: AtomicUsize::new(0),
            }
        }
    }

    impl ObjectStore for ConcurrencyTrackingStore {
        fn exists(&self, hash: &Hash) -> Result<bool, Error> {
            self.inner.exists(hash)
        }
        fn size(&self, hash: &Hash) -> Result<u64, Error> {
            self.inner.size(hash)
        }
        fn list_with_sizes(&self) -> Result<Vec<(String, u64)>, Error> {
            self.inner.list_with_sizes()
        }
        fn open_read(&self, hash: &Hash) -> Result<Box<dyn std::io::Read>, Error> {
            let cur = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(cur, Ordering::SeqCst);
            // Hold the "in-flight" window open long enough for concurrent
            // workers (if any) to overlap with this call.
            std::thread::sleep(self.delay);
            let result = self.inner.open_read(hash);
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            result
        }
        fn write_from(&self, hash: &Hash, reader: &mut dyn std::io::Read) -> Result<(), Error> {
            self.inner.write_from(hash, reader)
        }
        fn default_transfer_concurrency(&self) -> usize {
            self.forced_concurrency
        }
    }

    /// Populate `store` with a tree containing `n` independent single-chunk
    /// blob children (each well under the 1 MiB chunk threshold). Returns the
    /// tree hash and the (file name, content) pairs the tree references.
    fn populate_tree_with_blobs(
        store: &dyn ObjectStore,
        n: usize,
    ) -> (Hash, Vec<(String, Vec<u8>)>) {
        let mut files = Vec::with_capacity(n);
        let mut entries = Vec::with_capacity(n);
        for i in 0..n {
            let name = format!("file{i}.txt");
            let content = format!("expand-test-content-{i}").into_bytes();
            let serialised = crate::object::serialise_blob(&content);
            let hash = crate::object::blob_hash(&content);
            codec::chunk::store_chunked(store, &hash, &serialised, None).unwrap();
            entries.push(TreeEntry::Blob {
                name: name.clone(),
                hash,
                size: content.len() as u64,
                mtime: None,
                mode: None,
            });
            files.push((name, content));
        }
        let tree_bytes = Tree::Normal { entries }.serialise();
        let tree_hash = Hash::compute(&tree_bytes);
        codec::store_write(store, &tree_hash, &tree_bytes, None).unwrap();
        (tree_hash, files)
    }

    #[test]
    fn expand_tree_materialises_correct_content() {
        // Correctness only (design/04 expand step 4). This must already pass
        // today, before the batched-fetch change: expand_tree's end result is
        // unaffected by *how many* transfer calls it takes to get there.
        let remote_dir = TempDir::new().unwrap();
        let remote = LocalStore::for_remote(remote_dir.path());
        let (tree_hash, files) = populate_tree_with_blobs(&remote, 6);

        let local_dir = TempDir::new().unwrap();
        let local = LocalStore::for_cache(local_dir.path());
        let work_dir = TempDir::new().unwrap();
        let base_dir = work_dir.path().join("dest");

        let materialised = expand_tree(
            &tree_hash,
            &base_dir,
            work_dir.path(),
            "dest",
            &local,
            &remote,
            None,
            0,
        )
        .unwrap();

        assert_eq!(materialised, 6);
        for (name, content) in &files {
            let path = base_dir.join(name);
            assert_eq!(
                std::fs::read(&path).unwrap(),
                *content,
                "materialised content mismatch for {name}"
            );
        }
    }

    #[test]
    fn expand_tree_batches_blob_fetches_for_concurrency() {
        // Improvement B applied to expand (design/02_storage_format.md,
        // "Multi-root batching"; design/04_cli_spec.md expand step 3): the
        // design calls for collecting every blob hash an expansion needs and
        // fetching all of them via a single batched, concurrent transfer,
        // instead of the current per-blob sequential loop over tree entries
        // (`expand_tree`'s `for entry in entries` loop above calls
        // `transfer_objects` once per blob).
        //
        // A tree of N small files, each below the CDC min_size chunk
        // threshold, is exactly the case the design doc calls out: each
        // blob's own BFS is a single childless node with nothing to divide
        // across workers, so today's per-blob sequential loop can never
        // observe concurrency > 1 no matter how `OMEMFS_TRANSFER_CONCURRENCY`
        // is set. This is expected to FAIL at runtime today (not a compile
        // failure -- `expand_tree` already exists) until the batched-fetch
        // change lands.
        //
        // JUDGMENT CALL: design/04_cli_spec.md's updated expand step 3 does
        // not name a specific "collect hashes" helper function or signature,
        // so rather than invent one and risk testing an API shape the
        // implementer doesn't choose, this test exercises the existing
        // `expand_tree` function directly (shared by both the top-level
        // directory-stub branch in `run()` and its own recursion) and
        // observes concurrency through an instrumented `remote` store --
        // the same "achievable concurrency" proxy used for
        // `transfer_objects_many` in push.rs's tests. See this task's final
        // report for the full rationale, including the known gap this
        // leaves (run()'s top-level loop over sibling Blob-type stubs, lines
        // ~94-172, is not separately covered here).
        if std::env::var("OMEMFS_TRANSFER_CONCURRENCY").is_ok() {
            return;
        }

        let remote_dir = TempDir::new().unwrap();
        let inner_remote = LocalStore::for_remote(remote_dir.path());
        let (tree_hash, files) = populate_tree_with_blobs(&inner_remote, 6);
        let remote = ConcurrencyTrackingStore::new(inner_remote, 4, Duration::from_millis(20));

        let local_dir = TempDir::new().unwrap();
        let local = LocalStore::for_cache(local_dir.path());
        let work_dir = TempDir::new().unwrap();
        let base_dir = work_dir.path().join("dest");

        let materialised = expand_tree(
            &tree_hash,
            &base_dir,
            work_dir.path(),
            "dest",
            &local,
            &remote,
            None,
            0,
        )
        .unwrap();

        assert_eq!(materialised, files.len());
        assert!(
            remote.max_in_flight.load(Ordering::SeqCst) >= 2,
            "expanding {} independently-stubbed small files should let the \
             worker pool overlap their fetches (max observed: {})",
            files.len(),
            remote.max_in_flight.load(Ordering::SeqCst)
        );
    }
}
