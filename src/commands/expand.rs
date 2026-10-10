use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use filetime::FileTime;

use crate::codec;
use crate::dtimer_l1;
use crate::error::Error;
use crate::io_stats;
use crate::object::{Hash, Tree, TreeEntry};
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
    let mut all_stubs = stub::list(&repo.work_dir)?;
    let scoped_paths: Vec<String> = if opts.paths.is_empty() {
        Vec::new()
    } else {
        opts.paths
            .iter()
            .map(|path| crate::repo::normalize_path(path, &repo.work_dir, &opts.current_dir))
            .collect::<Result<_, _>>()?
    };

    // Scopes that don't match any on-disk stub record directly, but may lie
    // nested inside an unexpanded directory stub (design/04_cli_spec.md,
    // "Paths below the stub frontier"). Computed with pure string comparisons
    // -- no I/O -- so the offline-safe "Nothing to expand." fast path below
    // is unaffected when this is empty.
    let unmatched: Vec<String> = if opts.paths.is_empty() || scoped_paths.iter().any(|s| s.is_empty()) {
        Vec::new()
    } else {
        scoped_paths
            .iter()
            .filter(|scope| {
                !all_stubs
                    .iter()
                    .any(|(rel, _)| rel == *scope || rel.starts_with(&format!("{}/", scope)))
            })
            .cloned()
            .collect()
    };

    // Apply the scope filter, then stub_threshold: partition into
    // (expand, keep-stubbed). threshold == 0 means expand everything. Stubs
    // that exactly match a user-specified path bypass the threshold.
    // Re-run after phase 0 below once lowering may have added new on-disk
    // stub records for previously-unmatched scopes.
    let partition_stubs = |stubs: &[(String, stub::StubRecord)]| {
        let scoped: Vec<(String, stub::StubRecord)> =
            if opts.paths.is_empty() || scoped_paths.iter().any(|scope| scope.is_empty()) {
                stubs.to_vec()
            } else {
                stubs
                    .iter()
                    .cloned()
                    .filter(|(rel, _)| {
                        scoped_paths
                            .iter()
                            .any(|scope| rel == scope || rel.starts_with(&format!("{}/", scope)))
                    })
                    .collect()
            };
        scoped.into_iter().partition::<Vec<_>, _>(|(rel, record)| {
            opts.stub_threshold == 0
                || record.size < opts.stub_threshold
                || scoped_paths.iter().any(|scope| rel == scope)
        })
    };

    let (mut to_expand, mut kept_stubbed) = partition_stubs(&all_stubs);

    if to_expand.is_empty() && kept_stubbed.is_empty() && unmatched.is_empty() {
        crate::progress::emit_output_line("Nothing to expand.");
        return Ok(());
    }

    let io_record = Arc::new(IoRecord::default());
    let (pack_reader, _remote, remote_key) =
        repo.pack_reader(&opts.remote_name, Some(&io_record))?;
    let remote_key = remote_key.as_ref();

    // Registered before phase 0 runs, not after: resolving a path below the
    // stub frontier can require several sequential remote reads (one tree
    // object per level -- each level's bytes are what reveal the next hash,
    // so this cannot be batched like the ordinary blob fetch phase below). On
    // a slow or high-latency remote this can take a visible amount of time;
    // registering the phase first means the phase view's "-> Expand stubs"
    // row (and any detail lines from resolve_and_lower) appear immediately,
    // instead of the terminal staying blank until phase 0 finishes.
    let phase = crate::progress::begin_phase("Expand stubs");

    // Phase 0: resolve any scope that lies below the stub frontier by
    // lowering the frontier down to it one tree level at a time. Every
    // resolved target becomes an ordinary on-disk stub record (non-dry-run),
    // which the re-computed `to_expand`/`kept_stubbed` below then picks up
    // through the unchanged existing pipeline.
    let mut phase_zero_materialised = 0usize;
    let mut dry_run_virtual_targets: Vec<String> = Vec::new();
    if !unmatched.is_empty() {
        let report = resolve_and_lower(
            &repo.work_dir,
            &unmatched,
            &local,
            &pack_reader,
            remote_key,
            opts.dry_run,
            &phase,
        )?;
        phase_zero_materialised = report.materialised_count;
        dry_run_virtual_targets = report.dry_run_targets;
        if !opts.dry_run {
            all_stubs = stub::list(&repo.work_dir)?;
            let (te, ks) = partition_stubs(&all_stubs);
            to_expand = te;
            kept_stubbed = ks;
        }
    }

    if to_expand.is_empty()
        && kept_stubbed.is_empty()
        && dry_run_virtual_targets.is_empty()
        && phase_zero_materialised == 0
    {
        phase.complete("nothing to expand");
        crate::progress::emit_output_line("Nothing to expand.");
        return Ok(());
    }

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
                // `blob_fully_cached`, not a bare `local.exists`, because an
                // interrupted transfer can leave a cached manifest whose
                // chunks were never fetched -- see `restore`'s "Missing
                // objects" (design/04_cli_spec.md) for the failure this
                // guards against.
                if !codec::chunk::blob_fully_cached(&local, &record.hash)? {
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
        // Targets resolved below the stub frontier (phase 0) never became
        // on-disk stub records under --dry-run, so they are not in
        // `to_expand` -- report them here from the phase-0 result directly.
        for rel_path in &dry_run_virtual_targets {
            crate::progress::emit_output_line(&format!("  would expand: {}", rel_path));
        }
        let total_would_expand = to_expand.len() + dry_run_virtual_targets.len();
        let summary = if !kept_stubbed.is_empty() {
            format!(
                "{} would expand, {} kept",
                total_would_expand,
                kept_stubbed.len()
            )
        } else {
            format!("{} would expand", total_would_expand)
        };
        phase.complete(summary);
        if !kept_stubbed.is_empty() {
            crate::progress::emit_output_line(&format!(
                "{} stub(s) would be expanded, {} stub(s) at or above threshold kept.",
                total_would_expand,
                kept_stubbed.len()
            ));
        } else {
            crate::progress::emit_output_line(&format!(
                "{} stub(s) would be expanded.",
                total_would_expand
            ));
        }
    } else {
        // Files materialised during phase-0 lowering (siblings forced to
        // materialise via the Git-visibility fallback while descending to a
        // requested target) count toward the reported total just like any
        // other materialised file.
        let count = count + phase_zero_materialised;
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
                    // `blob_fully_cached`, not a bare `local.exists`: see the
                    // comment at this function's other `local.exists` fix
                    // below (`expand_tree_materialise`) for why a manifest
                    // alone is not sufficient evidence a blob is cached.
                    if (stub_threshold == 0 || size < stub_threshold)
                        && !codec::chunk::blob_fully_cached(local, &hash)?
                    {
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
                // `blob_fully_cached`, not a bare `local.exists`: the object
                // named by `hash` may be a chunk manifest whose bytes are
                // present locally but whose chunks were never fetched (an
                // interrupted `transfer_objects`/`transfer_objects_many`
                // leaves exactly this state, since the manifest and its
                // chunks are written as separate objects). `local.exists`
                // alone would see the manifest, skip the fetch below, and
                // then fail inside `materialise_blob_at` with
                // `Error::ObjectNotFound` for a *chunk* hash -- which never
                // appears in any tree entry, so the error names no path a
                // user could act on. See `restore`'s "Missing objects"
                // (design/04_cli_spec.md) for the full incident this bug
                // caused in practice.
                if !codec::chunk::blob_fully_cached(local, &hash)? {
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
// Phase 0: resolving a path below the stub frontier (design/04_cli_spec.md,
// "Paths below the stub frontier"; design/08_stub_system.md, "Lowering the
// stub frontier past a `.git` entry").
// ---------------------------------------------------------------------------

/// Result of resolving a batch of below-frontier `<path>` arguments.
#[derive(Debug)]
struct PhaseZeroReport {
    /// Repo-relative paths phase 0 confirmed reachable under `--dry-run`.
    /// Never written to disk under `--dry-run`, so `stub::list()` cannot see
    /// them -- the caller reports these directly.
    dry_run_targets: Vec<String>,
    /// Files materialised as a side effect of lowering (a sibling, or the
    /// target itself, whose stub would be visible to Git). Counted toward
    /// the caller's total the same as any other materialised file.
    materialised_count: usize,
}

/// Read one tree level's entries for stub-frontier lowering, memoized per
/// `resolve_and_lower` call so multiple `<path>` targets sharing an ancestor
/// read that ancestor's tree object only once.
///
/// `cache_writes` selects the read strategy: `true` (normal run) reads
/// through the local cache, fetching a miss from `remote` and caching the
/// plaintext locally. `false` (`--dry-run`) reads from the local cache first
/// and, on a miss, reads from `remote` and discards the bytes afterward --
/// `.omemfs/objects/` is never written to under `--dry-run`.
fn read_level_entries(
    tree_hash: &Hash,
    local: &dyn ObjectStore,
    remote: &dyn ObjectStore,
    remote_key: Option<&crate::codec::encrypt::EncryptKey>,
    cache_writes: bool,
    memo: &RefCell<HashMap<Hash, Rc<Vec<TreeEntry>>>>,
) -> Result<Rc<Vec<TreeEntry>>, Error> {
    if let Some(cached) = memo.borrow().get(tree_hash) {
        return Ok(cached.clone());
    }
    let data = if cache_writes {
        codec::ensure_local_then_read(remote, local, tree_hash, remote_key)?
    } else if local.exists(tree_hash)? {
        codec::store_read(local, tree_hash, None)?
    } else {
        codec::store_read(remote, tree_hash, remote_key)?
    };
    let Tree::Normal { entries } = Tree::deserialise(&data)?;
    let entries = Rc::new(entries);
    memo.borrow_mut().insert(tree_hash.clone(), entries.clone());
    Ok(entries)
}

/// Write or materialise a single child encountered while lowering one level
/// of the stub frontier. Mirrors `expand_tree_materialise`'s per-entry
/// decision table exactly, so the two never drift: a symlink is always
/// materialised (never stubbable); a Blob/Tree whose stub would be visible
/// to Git is materialised instead of stubbed; everything else is written as
/// its own individual stub record, purely from the tree entry's fields (no
/// fetch). A child already present on disk is left untouched -- on-disk
/// state always wins over the recorded tree entry.
///
/// Returns the number of files (blobs and symlinks) actually materialised,
/// so the caller can fold it into the overall "N file(s) expanded" count.
fn lower_one_child(
    work_dir: &std::path::Path,
    dir_rel: &str,
    dir_abs: &std::path::Path,
    local: &dyn ObjectStore,
    remote: &dyn ObjectStore,
    remote_key: Option<&crate::codec::encrypt::EncryptKey>,
    entry: &TreeEntry,
) -> Result<usize, Error> {
    let rel_path = format!("{}/{}", dir_rel, entry.name());
    let abs_path = dir_abs.join(entry.name());
    if abs_path.exists() {
        return Ok(0);
    }
    match entry {
        TreeEntry::Symlink { target, mtime, .. } => {
            #[cfg(unix)]
            {
                crate::fsmeta::write_symlink_atomic(&abs_path, target)?;
                crate::fsmeta::restore_symlink_mtime(&abs_path, mtime);
            }
            #[cfg(not(unix))]
            {
                let _ = (target, mtime);
            }
            Ok(1)
        }
        TreeEntry::Blob {
            hash,
            size,
            mtime,
            mode,
            ..
        } => {
            let stub_path = stub::file_stub_path_for(&abs_path);
            if stub::stub_would_be_visible_to_git(&stub_path, work_dir) {
                if !local.exists(hash)? {
                    crate::commands::push::transfer_objects(remote, local, hash, remote_key, true)?;
                }
                crate::fsmeta::materialise_blob_at(local, hash, &abs_path, mtime, mode)?;
                Ok(1)
            } else {
                stub::write(
                    work_dir,
                    &rel_path,
                    &stub::StubRecord {
                        target_type: stub::StubTargetType::Blob,
                        hash: hash.clone(),
                        size: *size,
                        mtime: *mtime,
                        mode: mode.clone(),
                        blob_count: 0,
                    },
                )?;
                Ok(0)
            }
        }
        TreeEntry::Tree {
            hash,
            size,
            mtime,
            blob_count,
            ..
        } => {
            let dir_stub_path = abs_path.join(stub::DIR_STUB_NAME);
            if stub::stub_would_be_visible_to_git(&dir_stub_path, work_dir) {
                expand_tree(
                    hash, &abs_path, work_dir, &rel_path, local, remote, remote_key, 0,
                )
            } else {
                fs::create_dir_all(&abs_path)?;
                stub::write_dir_stub(
                    work_dir,
                    &rel_path,
                    &stub::StubRecord {
                        target_type: stub::StubTargetType::Tree,
                        hash: hash.clone(),
                        size: *size,
                        mtime: *mtime,
                        mode: None,
                        blob_count: *blob_count,
                    },
                )?;
                Ok(0)
            }
        }
    }
}

/// Lower one directory-stub level: replace the marker at `dir_rel` (tree
/// hash `dir_record.hash`) with a real directory whose every child is
/// written as its own stub record or materialised (`lower_one_child`), then
/// remove the marker.
///
/// The `.git` child, if present, is written **last**, after every other
/// sibling's decision has already been made and written. `stub`'s
/// enclosing-worktree lookup detects a Git working tree by the physical
/// existence of a `.git` entry alone, and writing a directory stub for
/// `.git` makes it physically exist. Writing it first would make every
/// remaining sibling's Git-visibility check run `git check-ignore` inside
/// that non-functional repository, fail, and hit the fail-safe "visible"
/// answer -- silently forcing every remaining sibling to be fully
/// materialised instead of stubbed. Writing `.git` last avoids this
/// deterministically. See design/08_stub_system.md, "Lowering the stub
/// frontier past a `.git` entry".
///
/// A no-op besides the read if `dir_rel`'s marker is already gone -- a
/// previous target sharing this ancestor already lowered this level, or a
/// previous run was interrupted after finishing this level.
///
/// Under `dry_run`, makes no writes at all; it exists purely to fetch the
/// level's entries so descent can continue.
///
/// Returns the level's entries (so the caller can look up the next path
/// component without a second read) and the number of files materialised.
fn lower_one_level(
    work_dir: &std::path::Path,
    dir_rel: &str,
    dir_record: &stub::StubRecord,
    local: &dyn ObjectStore,
    remote: &dyn ObjectStore,
    remote_key: Option<&crate::codec::encrypt::EncryptKey>,
    dry_run: bool,
    memo: &RefCell<HashMap<Hash, Rc<Vec<TreeEntry>>>>,
    phase: &crate::progress::phase_view::PhaseHandle,
) -> Result<(Rc<Vec<TreeEntry>>, usize), Error> {
    // Emitted before the read (which may be a remote round-trip) so the
    // phase view shows this level is being worked on even while the read is
    // still in flight, not only after it returns.
    phase.detail(format!("resolving: {}", dir_rel));
    let entries = read_level_entries(&dir_record.hash, local, remote, remote_key, !dry_run, memo)?;
    if dry_run || stub::read_dir_stub(work_dir, dir_rel)?.is_none() {
        return Ok((entries, 0));
    }
    let dir_abs = work_dir.join(dir_rel);
    let mut materialised = 0usize;
    let mut git_child = None;
    for entry in entries.iter() {
        if entry.name() == ".git" {
            git_child = Some(entry);
            continue;
        }
        materialised += lower_one_child(work_dir, dir_rel, &dir_abs, local, remote, remote_key, entry)?;
    }
    if let Some(entry) = git_child {
        materialised += lower_one_child(work_dir, dir_rel, &dir_abs, local, remote, remote_key, entry)?;
    }
    stub::remove_dir_stub(work_dir, dir_rel)?;
    Ok((entries, materialised))
}

/// Phase 0 of `expand`: resolve every scope in `unmatched` (already confirmed
/// to have no direct on-disk stub match) against its deepest on-disk
/// ancestor stub, lowering the frontier one tree level at a time until the
/// scope itself is an on-disk stub record (non-dry-run) or is confirmed
/// reachable (`--dry-run`).
///
/// Multiple scopes sharing a common ancestor share the same `memo`, so that
/// ancestor's tree object is read only once regardless of how many scopes
/// descend through it.
///
/// A scope with no on-disk stub ancestor at all is left for the caller's
/// existing pipeline to treat as a plain no-op, matching pre-existing
/// behaviour for a path unrelated to any stub.
fn resolve_and_lower(
    work_dir: &std::path::Path,
    unmatched: &[String],
    local: &dyn ObjectStore,
    remote: &dyn ObjectStore,
    remote_key: Option<&crate::codec::encrypt::EncryptKey>,
    dry_run: bool,
    phase: &crate::progress::phase_view::PhaseHandle,
) -> Result<PhaseZeroReport, Error> {
    let memo: RefCell<HashMap<Hash, Rc<Vec<TreeEntry>>>> = RefCell::new(HashMap::new());
    let mut report = PhaseZeroReport {
        dry_run_targets: Vec::new(),
        materialised_count: 0,
    };

    for scope in unmatched {
        let Some((ancestor_rel, ancestor_record)) = stub::deepest_ancestor_stub(work_dir, scope)?
        else {
            continue;
        };

        let remaining: Vec<&str> = scope[ancestor_rel.len()..]
            .trim_start_matches('/')
            .split('/')
            .collect();

        let mut current_rel = ancestor_rel;
        let mut current_record = ancestor_record;
        for (i, component) in remaining.iter().enumerate() {
            let (entries, materialised) = lower_one_level(
                work_dir,
                &current_rel,
                &current_record,
                local,
                remote,
                remote_key,
                dry_run,
                &memo,
                phase,
            )?;
            report.materialised_count += materialised;

            let Some(child) = entries.iter().find(|e| e.name() == *component) else {
                return Err(Error::Other(format!(
                    "cannot expand '{}': '{}/{}' does not exist",
                    scope, current_rel, component
                )));
            };
            if matches!(child, TreeEntry::Tree { name, .. } if name == ".git") {
                return Err(Error::Other(format!(
                    "cannot expand '{}': '{}/.git' is a stubbed Git directory and can only be \
                     expanded together with its working tree — expand '{}' as a whole \
                     ('omemfs expand {}')",
                    scope, current_rel, current_rel, current_rel
                )));
            }

            let is_last = i + 1 == remaining.len();
            let next_rel = format!("{}/{}", current_rel, component);
            match child {
                TreeEntry::Tree {
                    hash,
                    size,
                    mtime,
                    blob_count,
                    ..
                } => {
                    current_rel = next_rel.clone();
                    current_record = stub::StubRecord {
                        target_type: stub::StubTargetType::Tree,
                        hash: hash.clone(),
                        size: *size,
                        mtime: *mtime,
                        mode: None,
                        blob_count: *blob_count,
                    };
                    if is_last && dry_run {
                        report.dry_run_targets.push(next_rel);
                    }
                }
                TreeEntry::Blob { .. } if !is_last => {
                    return Err(Error::Other(format!(
                        "cannot expand '{}': '{}' is a file, not a directory",
                        scope, next_rel
                    )));
                }
                TreeEntry::Symlink { .. } if !is_last => {
                    return Err(Error::Other(format!(
                        "cannot expand '{}': '{}' is a symlink, not a directory",
                        scope, next_rel
                    )));
                }
                TreeEntry::Blob { .. } | TreeEntry::Symlink { .. } => {
                    if dry_run {
                        report.dry_run_targets.push(next_rel);
                    }
                }
            }
        }
    }

    Ok(report)
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

    /// Read-side `ObjectStore` wrapper that counts total `open_read` calls,
    /// used to verify that a tree object shared by multiple phase-0 targets
    /// is read only once (`resolve_and_lower`'s memoization).
    struct ReadCountingStore {
        inner: LocalStore,
        reads: AtomicUsize,
    }

    impl ObjectStore for ReadCountingStore {
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
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.inner.open_read(hash)
        }
        fn write_from(&self, hash: &Hash, reader: &mut dyn std::io::Read) -> Result<(), Error> {
            self.inner.write_from(hash, reader)
        }
    }

    #[test]
    fn resolve_and_lower_reads_a_shared_ancestor_tree_only_once() {
        let remote_dir = TempDir::new().unwrap();
        let inner_remote = LocalStore::for_remote(remote_dir.path());
        // Root tree with two blob children, "file0.txt" and "file1.txt" --
        // both targets below descend through this one tree object.
        let (tree_hash, _files) = populate_tree_with_blobs(&inner_remote, 2);
        let remote = ReadCountingStore {
            inner: inner_remote,
            reads: AtomicUsize::new(0),
        };

        let local_dir = TempDir::new().unwrap();
        let local = LocalStore::for_cache(local_dir.path());
        let work_dir = TempDir::new().unwrap();

        stub::write_dir_stub(
            work_dir.path(),
            "a",
            &stub::StubRecord {
                target_type: stub::StubTargetType::Tree,
                hash: tree_hash,
                size: 1000,
                mtime: None,
                mode: None,
                blob_count: 2,
            },
        )
        .unwrap();

        let unmatched = vec!["a/file0.txt".to_string(), "a/file1.txt".to_string()];
        let report =
            resolve_and_lower(work_dir.path(), &unmatched, &local, &remote, None, false, &crate::progress::phase_view::PhaseHandle::noop()).unwrap();

        assert_eq!(report.materialised_count, 0);
        assert!(work_dir.path().join("a/file0.txt.omemfs-stub").is_file());
        assert!(work_dir.path().join("a/file1.txt.omemfs-stub").is_file());
        assert_eq!(
            remote.reads.load(Ordering::SeqCst),
            1,
            "the shared ancestor tree object must be read only once across both targets"
        );
    }

    #[test]
    fn resolve_and_lower_dry_run_makes_no_local_cache_writes() {
        let remote_dir = TempDir::new().unwrap();
        let remote = LocalStore::for_remote(remote_dir.path());
        let (tree_hash, _files) = populate_tree_with_blobs(&remote, 2);

        let local_dir = TempDir::new().unwrap();
        let local = LocalStore::for_cache(local_dir.path());
        let work_dir = TempDir::new().unwrap();

        stub::write_dir_stub(
            work_dir.path(),
            "a",
            &stub::StubRecord {
                target_type: stub::StubTargetType::Tree,
                hash: tree_hash,
                size: 1000,
                mtime: None,
                mode: None,
                blob_count: 2,
            },
        )
        .unwrap();

        let objects_before = local.list_with_sizes().unwrap().len();
        let unmatched = vec!["a/file0.txt".to_string()];
        let report =
            resolve_and_lower(work_dir.path(), &unmatched, &local, &remote, None, true, &crate::progress::phase_view::PhaseHandle::noop()).unwrap();

        assert_eq!(report.dry_run_targets, vec!["a/file0.txt".to_string()]);
        // Nothing written: the directory stub marker is untouched and no
        // stub record or working-tree file was created.
        assert!(work_dir.path().join("a/.omemfs-stub").is_file());
        assert!(!work_dir.path().join("a/file0.txt.omemfs-stub").exists());
        let objects_after = local.list_with_sizes().unwrap().len();
        assert_eq!(objects_before, objects_after);
    }

    #[test]
    fn resolve_and_lower_errors_on_git_target() {
        let remote_dir = TempDir::new().unwrap();
        let remote = LocalStore::for_remote(remote_dir.path());

        // Root tree "a" containing a ".git" Tree child and an unrelated blob.
        let git_tree_bytes = Tree::Normal { entries: vec![] }.serialise();
        let git_tree_hash = Hash::compute(&git_tree_bytes);
        codec::store_write(&remote, &git_tree_hash, &git_tree_bytes, None).unwrap();

        let content = b"hello".to_vec();
        let serialised = crate::object::serialise_blob(&content);
        let blob_hash = crate::object::blob_hash(&content);
        codec::chunk::store_chunked(&remote, &blob_hash, &serialised, None).unwrap();

        let root_bytes = Tree::Normal {
            entries: vec![
                TreeEntry::Tree {
                    name: ".git".to_string(),
                    hash: git_tree_hash,
                    mtime: None,
                    size: 0,
                    blob_count: 0,
                },
                TreeEntry::Blob {
                    name: "readme.txt".to_string(),
                    hash: blob_hash,
                    mtime: None,
                    size: content.len() as u64,
                    mode: None,
                },
            ],
        }
        .serialise();
        let root_hash = Hash::compute(&root_bytes);
        codec::store_write(&remote, &root_hash, &root_bytes, None).unwrap();

        let local_dir = TempDir::new().unwrap();
        let local = LocalStore::for_cache(local_dir.path());
        let work_dir = TempDir::new().unwrap();

        stub::write_dir_stub(
            work_dir.path(),
            "a",
            &stub::StubRecord {
                target_type: stub::StubTargetType::Tree,
                hash: root_hash,
                size: 1000,
                mtime: None,
                mode: None,
                blob_count: 1,
            },
        )
        .unwrap();

        let unmatched = vec!["a/.git/config".to_string()];
        let err = resolve_and_lower(work_dir.path(), &unmatched, &local, &remote, None, false, &crate::progress::phase_view::PhaseHandle::noop())
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("stubbed Git directory"),
            "unexpected error message: {msg}"
        );
        // The level was still safely lowered: .git became its own atomic
        // stub, and the refusal only blocks descending further into it.
        assert!(work_dir.path().join("a/.git/.omemfs-stub").is_file());
        assert!(work_dir.path().join("a/readme.txt.omemfs-stub").is_file());
    }
}
