use std::path::PathBuf;
use std::sync::Arc;

use crate::codec;
use crate::codec::pack::writer::PackWriter;
use crate::commands::transfer::{deduplicate_paths, post_clone_sync_guard};
use crate::error::Error;
use crate::io_stats;
use crate::object::{Hash, TreeEntry};
use crate::repo::Repo;
use crate::scan::{refresh_stat_cache, scan_and_store_with_stub_store};
use crate::stat_cache::StatCache;
use crate::store::ObjectStore;
use crate::store::stats::IoRecord;
use crate::term::{Output, Styles, paint};
use crate::tree_ops;
use crate::{dlog_l1, dtimer_l1};

pub struct PushOptions {
    pub work_dir: PathBuf,
    pub current_dir: PathBuf,
    pub paths: Vec<PathBuf>,
    pub dry_run: bool,
    pub with_backup: bool,
}

pub fn run(opts: PushOptions) -> Result<(), Error> {
    let started = std::time::Instant::now();
    let repo = Repo::open(&opts.work_dir)?;
    crate::progress::notify_repo_dir(&repo.work_dir);
    let _lock = repo.acquire_lock()?;
    let local = repo.local_store();

    let remote_name = "origin";
    let _t = dtimer_l1!("push");
    // When no paths given, default to the current directory.
    // If cwd == repo root, this normalizes to "" and push_full is used.
    let paths = if opts.paths.is_empty() {
        vec![opts.current_dir.clone()]
    } else {
        opts.paths
    };
    let io_record = Arc::new(IoRecord::default());
    let result = push_scoped(
        &repo,
        &local,
        remote_name,
        &paths,
        &opts.current_dir,
        opts.dry_run,
        opts.with_backup,
        Arc::clone(&io_record),
    );
    if result.is_ok() && !opts.dry_run {
        let omemfs_dir = repo.omemfs_dir();
        let duration_ms = started.elapsed().as_millis() as u64;
        io_stats::append_record(&omemfs_dir, "push", remote_name, &io_record, duration_ms);
    }
    result
}

// ---------------------------------------------------------------------------
// Full push
// ---------------------------------------------------------------------------

fn push_full(
    repo: &Repo,
    local: &dyn ObjectStore,
    remote_name: &str,
    dry_run: bool,
    with_backup: bool,
    io_record: Arc<IoRecord>,
) -> Result<(), Error> {
    let clone_root = repo.read_clone_root()?;
    // Read-through store used ONLY to read a partial-expansion directory's
    // `.omemfs-stub` record during the scan below (design/08_stub_system.md,
    // "Partial expansion", D15): that record names a clone-root-rooted tree
    // object a lazy, stub-aware clone may never have downloaded (the whole
    // directory was stubbed as a unit). Deliberately NOT used for
    // `flatten_tree_entries_for_push` (below) or anything else in this scan:
    // both that flatten's stub-boundary `.exists()` check and the scan's own
    // sealed-blob verification (`blob_fully_cached`, since this is a
    // `write_blobs = true` scan) must stay strictly local-only -- see
    // `scan_and_store_with_stub_store`'s doc comment for why routing those
    // through a remote-fetching store is a correctness bug, not just an
    // unnecessary fetch. Unbounded, like every other push/pull use of
    // `LazyTreeStore`: push already needs a working remote connection to
    // complete at all.
    let local_concrete = repo.local_store();
    let (pack_reader, _remote_raw, remote_key_reader) =
        repo.pack_reader(remote_name, Some(&io_record))?;
    let lazy = tree_ops::LazyTreeStore::new(&local_concrete, &pack_reader, remote_key_reader.as_ref());
    let clone_root_flat = match clone_root.as_ref() {
        Some(hash) => Some(tree_ops::flatten_tree_entries_for_push(hash, local)?),
        None => None,
    };
    let omemfs_dir = repo.omemfs_dir();
    let mut stat_cache = StatCache::read(&omemfs_dir);
    // push is always a blob-writing scan: compute seal trust once, right after
    // loading, before any file is scanned (design/07 "Implementation notes").
    stat_cache.compute_seals_trusted(&omemfs_dir, local);
    let (new_root_hash, conflict_paths, unstable_paths) = {
        let phase = crate::progress::begin_phase("Scan working tree");
        let _t = dtimer_l1!("scan working tree");
        let scan_result = scan_and_store_with_stub_store(
            &repo.work_dir,
            &repo.work_dir,
            local,
            &lazy,
            clone_root_flat.as_ref(),
            &stat_cache,
            true, // push needs blob objects in the local store for upload
        )?;
        let h = scan_result.root_hash.clone();
        let file_count = scan_result.files.len();
        let conflict_paths = scan_result.side.conflict_paths;
        let unstable_paths = scan_result.side.unstable_paths;
        refresh_stat_cache(stat_cache, &scan_result.files, &omemfs_dir, true);
        dlog_l1!("working tree root: {}", &h.as_str()[..8]);
        phase.complete(format!("{} files", file_count));
        (h, conflict_paths, unstable_paths)
    };

    // Conflict helper files found during scan → block push.
    if !conflict_paths.is_empty() {
        let mut sorted: Vec<_> = conflict_paths.iter().collect();
        sorted.sort();
        return Err(report_unresolved_conflicts(sorted));
    }

    if clone_root.as_ref() == Some(&new_root_hash) {
        colored_println("Nothing to push.", None);
        report_unstable_paths(&unstable_paths);
        maybe_backup_push_on_no_op(repo, local, clone_root.as_ref(), with_backup, dry_run);
        return Ok(());
    }

    if dry_run {
        let mut out = Output::for_stdout();
        let colored = out.colored();
        let styles = out.styles;
        let hash_str = paint(colored, styles.hash, &new_root_hash.as_str()[..8]);
        out.writeln(&format!("Would push root: {}", hash_str))?;
        out.finish()?;
        report_unstable_paths(&unstable_paths);
        return Ok(());
    }

    // Constructing the writer captures the INDEX_ROOT snapshot at push start;
    // finish() uses it as the CAS expected value, guarding the whole
    // read → upload → write window against concurrent pushes. push_full reads
    // the snapshot at start (here, via construction) even though it does not
    // splice onto it.
    let (mut writer, remote_key) = repo.pack_writer(remote_name, &io_record)?;
    let remote_root_snapshot = writer.remote_root_snapshot()?;
    // Post-clone sync guard (origin only): an absent index root with a synced
    // clone_root is a hard error (design/03).
    post_clone_sync_guard(clone_root.as_ref(), writer.index_root_present())?;
    // Lost-update precheck (design/03 "Push → Steps" step 4b): reject up
    // front if the remote has already advanced past clone_root, before
    // uploading anything. The CAS at finish() only guards divergence during
    // this push's own window, not divergence that already happened before
    // this push started.
    require_remote_not_advanced(
        clone_root.as_ref(),
        remote_root_snapshot.as_ref(),
        &new_root_hash,
    )?;
    upload_and_finalize(
        local,
        &mut writer,
        &new_root_hash,
        remote_key.as_ref(),
        &io_record,
    )
    .map_err(|e| recover_from_missing_object_if_applicable(&omemfs_dir, e))?;
    repo.write_clone_root(&new_root_hash)?;

    // Reconcile every stub in the working tree against the just-advanced
    // clone_root (design/08_stub_system.md, "Partial expansion": "After
    // push, the stub's hash field is updated to reflect the merged tree.").
    // Reuses pull's own reconcile step -- push does not duplicate the
    // decision logic, only triggers it at a different point. Placed strictly
    // after `write_clone_root` succeeds so a push that fails earlier (CAS
    // rejection, upload failure) never touches a stub record.
    crate::commands::pull::reconcile_stubs(
        &new_root_hash,
        &repo.work_dir,
        &repo.work_dir,
        &local_concrete,
    )?;

    {
        let mut out = Output::for_stdout();
        let colored = out.colored();
        let styles = out.styles;
        let hash_str = paint(colored, styles.hash, &new_root_hash.as_str()[..8]);
        out.writeln(&format!("Pushed to {}.", remote_name))?;
        out.writeln(&format!("Remote root: {}", hash_str))?;
        out.finish()?;
    }
    report_unstable_paths(&unstable_paths);

    if with_backup {
        push_to_backup(repo, local, &new_root_hash);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Multi-path scoped push
// ---------------------------------------------------------------------------

/// Push multiple scoped paths in a single remote operation.
///
/// Paths are deduplicated: if one path is an ancestor of another, the
/// descendant is dropped before processing.
/// Handles both a single scoped path and multiple scoped paths (the CLI
/// always calls this via `run`; there is no separate single-path
/// implementation -- refactor-instructions.md Phase 8 E7). A one-path call
/// takes exactly the same per-path loop as a multi-path call; the printed
/// summary is the only place the two are still told apart (see below), to
/// keep a single-path push's output byte-identical to before this merge.
fn push_scoped(
    repo: &Repo,
    local: &dyn ObjectStore,
    remote_name: &str,
    raw_paths: &[PathBuf],
    current_dir: &std::path::Path,
    dry_run: bool,
    with_backup: bool,
    io_record: Arc<IoRecord>,
) -> Result<(), Error> {
    // Normalise all paths against the cwd, re-expressed relative to the repo root.
    let mut rels: Vec<String> = raw_paths
        .iter()
        .map(|p| crate::repo::normalize_path(p, &repo.work_dir, current_dir))
        .collect::<Result<Vec<_>, _>>()?;

    // If any normalised path is empty, it means the root was specified → full push.
    if rels.iter().any(|r| r.is_empty()) {
        return push_full(repo, local, remote_name, dry_run, with_backup, io_record);
    }

    // Deduplicate: remove paths that are descendants of another path in the list.
    rels = deduplicate_paths(rels);

    // Validate each path before touching anything.
    let filters = crate::filter::FilterSet::load(&repo.work_dir);
    for rel in &rels {
        if is_system_path(rel) {
            return Err(Error::Other(format!("cannot push system path: {}", rel)));
        }
        if is_scoped_path_ignored(&filters, rel) {
            return Err(Error::Other(format!("cannot push ignored path: {}", rel)));
        }
    }

    let clone_root = repo.read_clone_root()?;
    let omemfs_dir = repo.omemfs_dir();

    // Construct the writer first so the INDEX_ROOT snapshot is captured at push
    // start; the remote root used for all splices below comes from that same
    // snapshot so the final CAS guards the whole read → upload → write window.
    let (mut writer, remote_key) = repo.pack_writer(remote_name, &io_record)?;
    post_clone_sync_guard(clone_root.as_ref(), writer.index_root_present())?;
    let remote_root = writer.remote_root_snapshot()?;

    // Pack-aware read-through store over the remote root: the remote's tree
    // objects (including the root itself) may live inline in another push's
    // delta index rather than as loose objects, so a raw, non-pack-aware
    // store cannot resolve them -- see design/03_sync_model.md, "Path-scoped
    // push". This mirrors how `pull` reads the remote root (`PackReader` /
    // `LazyTreeStore`). Constructed here, before the per-path scan loop
    // below, so the loop's own stub-merge reads (a partial-expansion
    // directory's recorded tree object may not be in the local cache after a
    // lazy clone -- design/08_stub_system.md, "Partial expansion") can fetch
    // on demand too, not just the later per-path divergence check and splice
    // loop that already used this store. Uses the writer's own `remote_key`
    // (rather than the reader's own copy of the same value) so both handles
    // are keyed by the same value.
    let (pack_reader, _remote_raw, _remote_key_reader) =
        repo.pack_reader(remote_name, Some(&io_record))?;
    let local_concrete = repo.local_store();
    let lazy = tree_ops::LazyTreeStore::new(&local_concrete, &pack_reader, remote_key.as_ref());

    // STAT_CACHE scope-limited load (design/07 "Read optimisation: scope-limited
    // load"): a single scoped path parses only that path's slice of STAT_CACHE
    // rather than the whole file. Multiple paths fall back to a full read.
    let mut stat_cache = if rels.len() == 1 {
        StatCache::read_scoped(&omemfs_dir, &rels[0])
    } else {
        StatCache::read(&omemfs_dir)
    };
    // push is always a blob-writing scan: compute seal trust once, right after
    // loading, before any path is scanned (design/07 "Implementation notes").
    stat_cache.compute_seals_trusted(&omemfs_dir, local);

    // Scan each path and compute its new hash.
    struct ScopedResult {
        rel: String,
        components: Vec<String>,
        scoped_hash: Hash,
        abs_path_is_dir: bool,
        /// True means this entry should be deleted.
        is_delete: bool,
    }

    // Precomputed stub entries for scoped paths that are themselves stubs.
    let mut stub_entries: std::collections::HashMap<String, TreeEntry> =
        std::collections::HashMap::new();

    // Conflict helper base paths found while scanning the in-scope subtrees.
    // Detection is a side effect of each path's own scan (same as push_full),
    // so it honours the scope and `.omemfs-filter` and never walks the whole
    // `work_dir` — see design/04_cli_spec.md "path-scoped push".
    let mut conflict_paths: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut unstable_paths: std::collections::HashSet<String> = std::collections::HashSet::new();

    // File entries captured from an open handle. Keeping their metadata here
    // avoids re-stat-ing a pathname that may be replaced before the splice.
    let mut captured_entries: std::collections::HashMap<String, TreeEntry> =
        std::collections::HashMap::new();

    let mut scoped_results: Vec<ScopedResult> = Vec::new();
    for rel in &rels {
        let components: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
        let abs_path = repo.work_dir.join(rel);

        // design/03_sync_model.md, "Path-scoped push" step 1: before scanning
        // or reading any content for this path, verify every component from
        // work_dir down to (and including) the scoped path itself is
        // symlink-free. `is_real_dir` returns `Err(SymlinkEscape)` for a
        // symlinked ancestor OR a symlink at the leaf itself, `Ok(_)`
        // (true/false, neither an error) for a real directory, a plain
        // file, or a missing path -- exactly the set of outcomes that are
        // safe to keep processing below (a plain file goes on to the file
        // scan branch, a missing path to the delete branch, a real
        // directory to the directory scan branch). This closes the escape a
        // symlinked ancestor of `<path>` would otherwise allow: reading (and
        // uploading) content that actually lives outside `work_dir`.
        crate::fsguard::is_real_dir(&repo.work_dir, &abs_path)?;

        // If the path is itself a stub, splice its recorded entry — never treat
        // the missing materialised content as a deletion (design/08).
        if let Some(stub_entry) = resolve_scoped_stub(&repo.work_dir, rel) {
            let scoped_hash = stub_entry
                .hash()
                .cloned()
                .ok_or_else(|| Error::Other("stub entry has no hash".to_string()))?;
            // Skip a stub that already matches clone_root at this path: nothing
            // to push for it (matches push_scoped's single-path short-circuit;
            // refactor-instructions.md Phase 8 E7 -- without this check a
            // multi-path push always re-spliced and re-CAS-wrote an unchanged
            // stub instead of leaving it out of scoped_results). Compared by
            // full entry (hash + mode), not bare hash, so a stub recording a
            // mode-only change is never mistaken for "unchanged" (design/03
            // "Path-scoped push" step 1c; D14).
            let existing_entry = navigate_entry_opt(clone_root.as_ref(), &components, &lazy)?;
            if existing_entry
                .as_ref()
                .is_some_and(|e| tree_entries_equal(e, &stub_entry))
            {
                continue;
            }
            let is_dir = matches!(stub_entry, TreeEntry::Tree { .. });
            stub_entries.insert(rel.clone(), stub_entry);
            scoped_results.push(ScopedResult {
                rel: rel.clone(),
                components: components.iter().map(|s| s.to_string()).collect(),
                scoped_hash,
                abs_path_is_dir: is_dir,
                is_delete: false,
            });
            continue;
        }

        // `rel` itself is not a stub (checked above). Before treating an
        // unreadable `abs_path` as "deleted", rule out the case where `rel`
        // is merely unreachable because one of its ANCESTORS is
        // dir-stubbed: the ancestor's real content was deferred, not
        // removed, so `rel` may or may not still exist inside it. Guessing
        // "deleted" here would silently drop that content from the remote
        // (design/03_sync_model.md, "Path-scoped push", "Deletion
        // detection"). This is deliberately the minimal safe response
        // (abort) rather than resolving the path through the stub's
        // recorded subtree.
        if crate::stub::deepest_ancestor_stub(&repo.work_dir, rel)?.is_some() {
            return Err(Error::Other(format!(
                "cannot push '{}': an ancestor directory is stubbed (its content is deferred, not confirmed absent)\n\
                 Run 'omemfs expand' on the stubbed ancestor first, or push the stubbed directory itself.",
                rel
            )));
        }

        // Deletion intent requires a direct NotFound on `abs_path` itself --
        // nothing there at all, not even a symlink (design/03_sync_model.md,
        // "Path-scoped push", "Deletion detection"). `symlink_metadata` (not
        // `Path::exists`, which silently maps every stat failure, including
        // a permission error, to `false`) is used so any other I/O error
        // (most importantly `PermissionDenied`) is distinguished from
        // "confirmed absent" and aborts the whole push instead of being
        // mistaken for a deletion.
        match std::fs::symlink_metadata(&abs_path) {
            Ok(_) => {
                // Path exists (as of this check) -- fall through to the scan
                // branch below.
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Deletion intent. Whether this is a real removal or a no-op
                // (the path is already absent on the remote) is decided
                // later, in the splice loop, against the remote-root
                // snapshot — see the is_delete branch there. Record the
                // intent here regardless.
                scoped_results.push(ScopedResult {
                    rel: rel.clone(),
                    components: components.iter().map(|s| s.to_string()).collect(),
                    scoped_hash: Hash::compute(b""),
                    abs_path_is_dir: false,
                    is_delete: true,
                });
                continue;
            }
            Err(e) => {
                return Err(Error::Other(format!(
                    "cannot stat '{}': {} — aborting push rather than risk treating it as deleted",
                    rel, e
                )));
            }
        }

        let phase = crate::progress::begin_phase(format!("Scan {}", rel));
        let scoped_hash = if abs_path.is_dir() {
            // Scoped flatten (design/03 "Path-scoped push"): build the mtime
            // pre-filter map from only `rel`'s subtree of the clone root,
            // instead of flattening the whole clone root for every path.
            // Local-only (`local`, not `lazy`): this flatten's stub-boundary
            // `.exists()` check (never descend into a stubbed subtree) must
            // stay local-only for the same reason `push_full`'s equivalent
            // flatten does -- see `scan_and_store_with_stub_store`'s doc
            // comment.
            let scoped_flat = match clone_root.as_ref() {
                Some(hash) => Some(tree_ops::flatten_tree_entries_scoped_for_push(
                    hash, rel, local,
                )?),
                None => None,
            };
            // `stub_store: &lazy` for the same reason as `push_full`'s scan
            // (D15): a partial-expansion directory's stub record may name a
            // tree object not yet in the local cache after a lazy clone.
            // `store: local` keeps the sealed-blob check and tree/blob writes
            // strictly local-only.
            let scan_result = match scan_and_store_with_stub_store(
                &repo.work_dir,
                &abs_path,
                local,
                &lazy,
                scoped_flat.as_ref(),
                &stat_cache,
                true, // push needs blob objects in the local store for upload
            ) {
                Ok(result) => result,
                Err(e) if e.is_live_path_race() => {
                    unstable_paths.insert(rel.clone());
                    phase.complete("skipped active path");
                    continue;
                }
                Err(e) => return Err(e),
            };
            let count = scan_result.files.len();
            // Shared with `refresh_stat_cache`/`refresh_stat_cache_scoped` so
            // this third writeback call site cannot drift from the other two
            // (design/07 "Usage in scan_and_store"): a cache miss gets a
            // fresh `update`, and a cache hit whose blob this scan verified
            // gets `mark_sealed`.
            crate::scan::apply_scanned(&mut stat_cache, &scan_result.files);
            conflict_paths.extend(scan_result.side.conflict_paths);
            unstable_paths.extend(scan_result.side.unstable_paths);
            phase.complete(format!("{} files", count));
            scan_result.root_hash
        } else {
            let stored = match crate::codec::chunk::store_file_snapshot(local, &abs_path, None) {
                Ok(stored) => stored,
                Err(e) if e.is_live_path_race() => {
                    unstable_paths.insert(rel.clone());
                    phase.complete("skipped active path");
                    continue;
                }
                Err(e) => return Err(e),
            };
            let hash = stored.hash.clone();
            captured_entries.insert(
                rel.clone(),
                TreeEntry::Blob {
                    name: components.last().unwrap().to_string(),
                    hash: hash.clone(),
                    mtime: stored.fs_mtime.map(chrono::DateTime::<chrono::Utc>::from),
                    size: stored.size,
                    mode: stored.mode,
                },
            );
            phase.complete("1 file");
            hash
        };

        // Skip a path whose freshly scanned entry already matches clone_root:
        // nothing to push for it (matches push_scoped's single-path
        // short-circuit; refactor-instructions.md Phase 8 E7 -- without this
        // check a multi-path push of N unchanged paths always performed a
        // full write cycle instead of reporting "Nothing to push."). Compared
        // by full entry (hash + mode for a blob leaf), not bare hash, so a
        // mode-only change (e.g. `chmod +x`) is never mistaken for
        // "unchanged" (design/03 "Path-scoped push" step 1c; D14). For a
        // directory subtree the freshly scanned root hash already reflects
        // every descendant's mode, so a plain `Tree` entry (mode is not one
        // of its fields) is sufficient to compare by.
        let existing_entry = navigate_entry_opt(clone_root.as_ref(), &components, &lazy)?;
        let is_unchanged = if let Some(fresh_blob) = captured_entries.get(rel) {
            // A scanned file: compare the full entry (hash + mode).
            existing_entry
                .as_ref()
                .is_some_and(|e| tree_entries_equal(e, fresh_blob))
        } else {
            // A scanned directory subtree: `mode` is not a `Tree` field, so
            // the subtree's own root hash already reflects every
            // descendant's mode -- a hash comparison is sufficient here.
            existing_entry.as_ref().and_then(|e| e.hash()) == Some(&scoped_hash)
        };
        if is_unchanged {
            continue;
        }

        scoped_results.push(ScopedResult {
            rel: rel.clone(),
            components: components.iter().map(|s| s.to_string()).collect(),
            scoped_hash,
            abs_path_is_dir: abs_path.is_dir(),
            is_delete: false,
        });
    }

    // Conflict helper files found while scanning the in-scope subtrees →
    // block push (design/04). Mirrors push_full's post-scan check.
    if !conflict_paths.is_empty() {
        let mut sorted: Vec<_> = conflict_paths.iter().collect();
        sorted.sort();
        return Err(report_unresolved_conflicts(sorted));
    }

    // push is always a blob-writing scan: if compute_seals_trusted found a
    // whole-epoch mismatch earlier, rotate and persist a fresh epoch now
    // (design/07 "Seal epoch"). A no-op when trust already held.
    stat_cache.finalize_seal_epoch(&omemfs_dir);

    // STAT_CACHE is a pure acceleration cache (design/07): a write failure must
    // not abort the command. Emit a warning and continue. A single-path push
    // merges only the in-scope slice back into the on-disk file (out-of-scope
    // entries survive untouched); a multi-path push falls back to a full
    // write. Either way, the write is skipped when nothing changed since load.
    let stat_cache_write_result = if rels.len() == 1 {
        stat_cache.write_scoped_merge(&omemfs_dir, &rels[0])
    } else {
        stat_cache.write_if_dirty(&omemfs_dir)
    };
    if let Err(e) = stat_cache_write_result {
        eprintln!(
            "warning: failed to write STAT_CACHE (acceleration cache): {}",
            e
        );
    }

    if scoped_results.is_empty() {
        colored_println("Nothing to push.", None);
        report_unstable_paths(&unstable_paths);
        maybe_backup_push_on_no_op(repo, local, clone_root.as_ref(), with_backup, dry_run);
        return Ok(());
    }

    if dry_run {
        let mut out = Output::for_stdout();
        let colored = out.colored();
        let styles = out.styles;
        for sr in &scoped_results {
            if sr.is_delete {
                let path_str = paint(colored, styles.deleted, &crate::term::escape_display(&sr.rel));
                out.writeln(&format!("Would delete {}", path_str))?;
            } else {
                let path_str = paint(colored, styles.added, &crate::term::escape_display(&sr.rel));
                let hash_str = paint(colored, styles.hash, &sr.scoped_hash.as_str()[..8]);
                out.writeln(&format!("Would push {} → {}", path_str, hash_str))?;
            }
        }
        out.finish()?;
        report_unstable_paths(&unstable_paths);
        return Ok(());
    }

    // `lazy`/`local_concrete` were already constructed above, before the
    // per-path scan loop -- reused here for the per-path divergence check
    // and the splice loop below (reads and writes through `lazy` fetch
    // missing objects from the remote on demand and cache them into `local`,
    // the same underlying `.omemfs/objects/` directory `local_concrete`
    // addresses).

    // Per-path divergence check (design/03 "Path-scoped push" step 2b): a
    // scoped push must reject only when one of ITS OWN pushed paths has
    // diverged remotely since this clone's last sync. Remote changes outside
    // every pushed path are preserved by the splice loop below and must not
    // block this push. Checked against `remote_root`, the snapshot captured
    // at push start (not a fresh read), so it observes exactly what the
    // final CAS is conditioned on.
    for sr in &scoped_results {
        let comps: Vec<&str> = sr.components.iter().map(|s| s.as_str()).collect();
        let remote_entry_at_path = navigate_entry_opt(remote_root.as_ref(), &comps, &lazy)?;

        if sr.is_delete {
            // Deleting a path already absent on the remote can never lose an
            // update -- it is the documented no-op case handled later in the
            // splice loop ("already absent on remote"), regardless of how
            // stale clone_root is for this path.
            if remote_entry_at_path.is_none() {
                continue;
            }
        } else {
            // This path's own new content already matches what's on the
            // remote: pushing it changes nothing there, so it cannot lose
            // anyone's update no matter how stale clone_root is for this
            // path. This is what makes a scoped push right after `omemfs
            // conflict accept-remote <path>` a safe no-op even though a
            // conflicting pull never advanced clone_root for that path.
            // Compared by full entry (hash + mode for a blob leaf), not bare
            // hash -- see design/03 "Path-scoped push" step 2b (D14): a
            // hash-only comparison would treat a mode-only remote change to
            // this exact leaf as "already matches" and let it be silently
            // overwritten by this push's own (different-mode) splice below.
            let already_matches = if let Some(fresh_blob) = captured_entries.get(&sr.rel) {
                remote_entry_at_path
                    .as_ref()
                    .is_some_and(|e| tree_entries_equal(e, fresh_blob))
            } else {
                remote_entry_at_path.as_ref().and_then(|e| e.hash()) == Some(&sr.scoped_hash)
            };
            if already_matches {
                continue;
            }
        }

        let clone_entry_at_path = navigate_entry_opt(clone_root.as_ref(), &comps, local)?;
        let entries_match = match (&remote_entry_at_path, &clone_entry_at_path) {
            (None, None) => true,
            (Some(r), Some(c)) => tree_entries_equal(r, c),
            _ => false,
        };
        if !entries_match {
            return Err(Error::CasFailed);
        }
    }

    // Apply all splices/removals onto the remote root sequentially. Every
    // non-delete scoped_result already represents a real change (the
    // "already matches clone_root" case was filtered out above), so only a
    // delete can turn out to be a no-op; `any_change` tracks whether at least
    // one splice/removal actually happened, so an all-no-op-deletes push
    // (e.g. deleting a single already-absent path, possibly with no remote
    // root at all) can skip the write/upload/print steps below exactly like
    // the pre-merge single-path no-op delete did (refactor-instructions.md
    // Phase 8 E7).
    let mut current_remote = remote_root.clone();
    let mut current_clone = clone_root.clone();
    let mut any_change = false;
    for sr in &scoped_results {
        let comps: Vec<&str> = sr.components.iter().map(|s| s.as_str()).collect();
        let leaf_name = sr.components.last().unwrap().clone();
        if sr.is_delete {
            // Remove from the remote tree. remove_entry returns:
            //   Ok(None)        → the entry was NOT present (no change). We must
            //                     leave current_remote unchanged; assigning None
            //                     here would later splice onto an EMPTY tree and
            //                     wipe every other remote file.
            //   Ok(Some(hash))  → entry removed (the resulting tree may legitimately
            //                     be empty, but it is still a real tree hash).
            let removed_from_remote = match current_remote {
                Some(ref rr) => match tree_ops::remove_entry(rr, &comps, &lazy)? {
                    Some(new_hash) => {
                        current_remote = Some(new_hash);
                        true
                    }
                    None => false,
                },
                None => false,
            };
            if !removed_from_remote {
                // Already absent on remote: no-op for this path with a note.
                colored_println(
                    &format!("note: '{}' is already absent on remote", sr.rel),
                    None,
                );
                // Do not touch current_clone for a path that did not exist on
                // remote; keep clone_root consistent with what we actually wrote.
                continue;
            }
            any_change = true;
            // Mirror the deletion into clone_root only when it was present there.
            if let Some(ref cr) = current_clone
                && let Some(new_hash) = tree_ops::remove_entry(cr, &comps, local)?
            {
                current_clone = Some(new_hash);
            }
        } else if let Some(stub_entry) = stub_entries.get(&sr.rel) {
            // Scoped path is a stub: splice its recorded entry directly. The
            // entry already carries the correct leaf name.
            any_change = true;
            let new_entry = stub_entry.clone();
            current_remote = Some(tree_ops::splice_entry(
                current_remote.as_ref(),
                &comps,
                new_entry.clone(),
                &lazy,
            )?);
            current_clone = Some(tree_ops::splice_entry(
                current_clone.as_ref(),
                &comps,
                new_entry,
                local,
            )?);
            continue;
        } else {
            any_change = true;
            let new_entry = if let Some(entry) = captured_entries.get(&sr.rel) {
                entry.clone()
            } else if sr.abs_path_is_dir {
                let (mtime, size, blob_count) = tree_ops::tree_meta(&sr.scoped_hash, local)?;
                TreeEntry::Tree {
                    name: leaf_name.clone(),
                    hash: sr.scoped_hash.clone(),
                    mtime,
                    size,
                    blob_count,
                }
            } else {
                return Err(Error::Other(format!(
                    "captured metadata missing for scoped file: {}",
                    sr.rel
                )));
            };

            current_remote = Some(tree_ops::splice_entry(
                current_remote.as_ref(),
                &comps,
                new_entry.clone(),
                &lazy,
            )?);
            current_clone = Some(tree_ops::splice_entry(
                current_clone.as_ref(),
                &comps,
                new_entry,
                local,
            )?);
        }
    }

    if !any_change {
        // Every scoped_result was a no-op deletion (already absent on
        // remote), possibly with no remote root at all -- nothing to
        // upload/write. Each no-op already printed its own "already absent"
        // note above; matches the pre-merge single-path push_scoped_delete's
        // no-op return. dry_run is always false here (the dry_run branch
        // above already returned), but the guard is harmless.
        maybe_backup_push_on_no_op(repo, local, current_clone.as_ref(), with_backup, dry_run);
        return Ok(());
    }

    let new_root_hash = match current_remote {
        Some(h) => h,
        None => {
            return Err(Error::Other(
                "push resulted in empty remote tree".to_string(),
            ));
        }
    };
    let new_clone_root = match current_clone {
        Some(h) => h,
        None => {
            return Err(Error::Other(
                "push resulted in empty clone root".to_string(),
            ));
        }
    };

    // Upload all new objects and perform single CAS write. The writer was
    // constructed earlier (snapshot captured at push start).
    upload_and_finalize(
        local,
        &mut writer,
        &new_root_hash,
        remote_key.as_ref(),
        &io_record,
    )
    .map_err(|e| recover_from_missing_object_if_applicable(&omemfs_dir, e))?;
    repo.write_clone_root(&new_clone_root)?;

    // Reconcile each pushed path's stub (if any) against the just-advanced
    // clone_root (design/08_stub_system.md, "Partial expansion": "After
    // push, the stub's hash field is updated to reflect the merged tree.").
    // Placed strictly after `write_clone_root` succeeds, matching
    // `push_full`. A deleted path never has a stub reconciled: nothing in
    // this function's own delete branch touches a stub marker at a deleted
    // path, and `reconcile_leaf_stub`/`reconcile_stubs` on an absent path is
    // a safe no-op in any case -- skipped explicitly for clarity.
    for sr in &scoped_results {
        if sr.is_delete {
            continue;
        }
        if sr.abs_path_is_dir {
            crate::commands::pull::reconcile_stubs(
                &new_clone_root,
                &repo.work_dir.join(&sr.rel),
                &repo.work_dir,
                &local_concrete,
            )?;
        } else {
            crate::commands::pull::reconcile_leaf_stub(
                &new_clone_root,
                &sr.rel,
                &repo.work_dir,
                &local_concrete,
            )?;
        }
    }

    {
        let mut out = Output::for_stdout();
        let colored = out.colored();
        let styles = out.styles;
        let hash_str = paint(colored, styles.hash, &new_root_hash.as_str()[..8]);
        // A single scoped path keeps its pre-merge, path-specific summary
        // ("Pushed <rel> to <remote>." / "Deleted <rel> from <remote>.");
        // multiple paths keep the plain multi-path summary. No bats test pins
        // either string exactly, but "push: path-scoped delete removes file
        // from remote" does assert the output contains "Deleted", so the
        // single-delete case must keep saying "Deleted" (refactor-instructions.md
        // Phase 8 E7 merge).
        match scoped_results.as_slice() {
            [only] if only.is_delete => {
                let path_str = paint(colored, styles.deleted, &crate::term::escape_display(&only.rel));
                out.writeln(&format!("Deleted {} from {}.", path_str, remote_name))?;
            }
            [only] => {
                let path_str = paint(colored, styles.added, &crate::term::escape_display(&only.rel));
                out.writeln(&format!("Pushed {} to {}.", path_str, remote_name))?;
            }
            _ => out.writeln(&format!("Pushed to {}.", remote_name))?,
        }
        out.writeln(&format!("Remote root: {}", hash_str))?;
        out.finish()?;
    }
    report_unstable_paths(&unstable_paths);

    if with_backup {
        push_to_backup(repo, local, &new_clone_root);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Print the styled "unresolved conflicts" error followed by the list of
/// conflict helper paths, and return the corresponding `Error`.
///
/// Both callers (push_full and push_scoped) collect `conflict_paths` as a
/// side effect of their own working-tree scan -- never via a separate
/// filesystem walk -- so detection always honours the pushed scope and
/// `.omemfs-filter` and never follows symlinks outside it (design/04
/// "path-scoped push"). Only the reporting tail is shared here.
fn report_unresolved_conflicts<'a>(paths: impl IntoIterator<Item = &'a String>) -> Error {
    let is_tty = atty::is(atty::Stream::Stderr);
    let colored = crate::term::color_enabled(crate::term::ColorChoice::Auto, is_tty);
    let styles = Styles::new();
    let err_msg = paint(
        colored,
        styles.deleted,
        "error: unresolved conflicts — resolve or restore before pushing",
    );
    eprintln!("{}", err_msg);
    eprintln!("The following conflict helper files were found:");
    for p in paths {
        eprintln!("  {}", crate::term::escape_display(&p.to_string()));
    }
    Error::Other("unresolved conflicts".to_string())
}

/// L3 upload-failure recovery, the final backstop of design/07 "Seal
/// invalidation": if an upload step still finds a referenced local blob
/// unreadable when it tries to transfer it, push must not fail silently or
/// upload a tree that references a missing object. Instead:
///
/// 1. Clear the `SEALED` bit on every STAT_CACHE entry and write the cache
///    back (hashes are untouched — nothing needs to be re-hashed).
/// 2. Return an error directing the user to run `push` again; the retry then
///    behaves exactly like today's push (every entry re-verified via
///    `store.exists()`, missing blobs recaptured from the working tree).
///
/// This backstop is gated on `Error::LocalObjectMissing` specifically (design/07
/// "The governing invariant", point 4) -- the variant `upload_missing_with_progress`
/// produces exactly when a *local* read out of `.omemfs/objects/` fails while
/// transferring an object. Any other error, including a plain
/// `Error::ObjectNotFound` surfaced by some other step of the upload walk
/// (e.g. a remote-side `remote.exists()` failure), is passed through
/// unchanged: it says nothing about whether the local SEALED bits are wrong,
/// so clearing them would not fix it and would only cost every future push a
/// wasted full `store.exists()` pass.
fn recover_from_missing_object_if_applicable(omemfs_dir: &std::path::Path, err: Error) -> Error {
    let hash = match &err {
        Error::LocalObjectMissing(h) => h.clone(),
        _ => return err,
    };
    let mut cache = StatCache::read(omemfs_dir);
    cache.clear_all_sealed();
    // STAT_CACHE is a pure acceleration cache (design/07): a write failure
    // here must not mask the original error or escalate; warn and continue --
    // the retry's own scan will simply re-verify every entry via
    // store.exists() regardless of whether this clear was persisted.
    if let Err(e) = cache.write_if_dirty(omemfs_dir) {
        eprintln!(
            "warning: failed to write STAT_CACHE while recovering from a missing object: {}",
            e
        );
    }
    Error::Other(format!(
        "upload failed: a referenced local object is missing ({}).\n\
         STAT_CACHE seal state has been cleared; run 'omemfs push' again to recapture and retry.\n\
         Note: some objects (e.g. tree objects) cannot be regenerated from the working\n\
         tree alone; if the retry still fails, the local object cache may need to be\n\
         rebuilt from a fresh scan or a clone.",
        hash
    ))
}

fn report_unstable_paths(paths: &std::collections::HashSet<String>) {
    if paths.is_empty() {
        return;
    }
    let mut sorted: Vec<_> = paths.iter().collect();
    sorted.sort();
    eprintln!(
        "warning: {} actively changing path{} not updated; previous remote contents were preserved when available:",
        sorted.len(),
        if sorted.len() == 1 { " was" } else { "s were" }
    );
    for path in sorted {
        eprintln!("  {}", crate::term::escape_display(&path.to_string()));
    }
}

/// Attempt a backup push from one of the "nothing changed on origin" early
/// returns in `push_full` / `push_scoped` (design/07 "Upload-failure
/// recovery" backup paragraph; design/04 "Backup push behaviour", defect
/// 7ii). Origin having nothing new to send does not mean backup has caught
/// up -- in particular, this is the only way a backup push that previously
/// failed and cleared STAT_CACHE's seals (see `push_to_backup`'s L3 recovery)
/// ever gets retried while the working tree itself stays unchanged. A
/// `--dry-run` must never perform any remote write, backup included, so it
/// is excluded here regardless of `with_backup`. A `None` `clone_root` means
/// nothing has ever been synced, so there is nothing to back up.
fn maybe_backup_push_on_no_op(
    repo: &Repo,
    local: &dyn ObjectStore,
    clone_root: Option<&Hash>,
    with_backup: bool,
    dry_run: bool,
) {
    if !with_backup || dry_run {
        return;
    }
    if let Some(clone_root) = clone_root {
        push_to_backup(repo, local, clone_root);
    }
}

/// Push the current clone_root state to the backup remote.
///
/// Uses the same pack layer as origin (PackWriter): the backup remote gets the
/// identical on-disk format (objects/ pack layer + INDEX_ROOT). clone_root is
/// not updated. Failure is non-fatal (warning only).
fn push_to_backup(repo: &Repo, local: &dyn ObjectStore, clone_root_hash: &Hash) {
    let config = match repo.read_config() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("warning: could not read config for backup push: {e}");
            return;
        }
    };
    if !config.remotes.contains_key("backup") {
        eprintln!("warning: --with-backup specified but no 'backup' remote configured");
        eprintln!("  Run 'omemfs config add-backup' to set one up.");
        return;
    }
    let backup = match repo.remote_store("backup") {
        Ok(s) => s,
        Err(e) => {
            eprintln!("warning: could not open backup remote: {e}");
            return;
        }
    };
    let backup_root_pointer = match repo.remote_root_pointer("backup") {
        Ok(p) => p,
        Err(e) => {
            eprintln!("warning: could not resolve backup remote root pointer: {e}");
            return;
        }
    };
    let backup_key = backup.encrypt_key.clone();

    // PackWriter captures the backup INDEX_ROOT snapshot at construction; finish
    // CAS-writes the backup INDEX_ROOT in the same format as origin.
    // Backup push is not tracked in io_stats (separate remote, non-fatal path).
    let mut writer = match PackWriter::new(
        Box::new(backup),
        backup_root_pointer,
        repo.objcache_store(),
        backup_key.clone(),
    ) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("warning: could not open backup pack writer: {e}");
            return;
        }
    };

    // Early "already up to date" check (design/04 "Backup push behaviour",
    // step 0; design/07 "Upload-failure recovery" backup paragraph): avoid a
    // redundant remote read/write once backup has caught up. This matters
    // now that a backup push is attempted even when origin had nothing to
    // send (defect 7ii's convergence fix below), which would otherwise
    // re-probe an already-current backup on every single push.
    match writer.remote_root_snapshot() {
        Ok(Some(ref backup_root)) if backup_root == clone_root_hash => {
            colored_println("Backup already up to date.", None);
            return;
        }
        Ok(_) => {}
        Err(e) => {
            eprintln!("warning: could not read backup remote root: {e}");
            return;
        }
    }

    if let Err(e) = upload_missing(local, &writer, clone_root_hash, backup_key.as_ref()) {
        // Reuse the exact same L3 recovery function origin's upload path
        // uses (design/07 "Upload-failure recovery", backup paragraph): on
        // `Error::LocalObjectMissing` this clears every SEALED bit and
        // returns a message describing the recovery; any other error passes
        // through unchanged. Either way this is a warning only -- backup
        // failure never affects the exit code (design/04).
        let omemfs_dir = repo.omemfs_dir();
        let reported = recover_from_missing_object_if_applicable(&omemfs_dir, e);
        eprintln!("warning: backup upload failed: {reported}");
        return;
    }
    if let Err(e) = writer.finish(clone_root_hash) {
        eprintln!("warning: backup INDEX_ROOT update failed: {e}");
        return;
    }
    let mut out = Output::for_stdout();
    let colored = out.colored();
    let styles = out.styles;
    let hash_str = paint(colored, styles.hash, &clone_root_hash.as_str()[..8]);
    let _ = out.writeln("Pushed to backup.");
    let _ = out.writeln(&format!("Backup remote root: {}", hash_str));
    let _ = out.finish();
}

/// Upload all objects reachable from `root_hash` that are absent from `remote`.
///
/// When `phase` is provided, sends a detail line for each uploaded blob.
pub fn upload_missing(
    local: &dyn ObjectStore,
    remote: &dyn ObjectStore,
    root_hash: &Hash,
    remote_key: Option<&crate::codec::encrypt::EncryptKey>,
) -> Result<(), Error> {
    upload_missing_with_progress(local, remote, root_hash, remote_key, None)
}

/// Run the "Upload objects" and "Finalize remote" phases: upload every
/// object reachable from `new_root_hash` that `writer`'s remote is missing,
/// then finish the pack (flush pack buffer, write delta index + Bloom,
/// CAS-update INDEX_ROOT) and record pack I/O stats onto `io_record`.
///
/// Consolidates the identical upload/finalize block across push's call sites:
/// push_full and push_scoped
/// (which itself now handles both single-path and multi-path scoped pushes,
/// refactor-instructions.md Phase 8 E7; originally 5 call sites at the time
/// of the E5 consolidation: push_full, push_scoped_multi, push_scoped,
/// push_scoped_entry, push_scoped_delete). The surrounding clone_root update
/// and user-facing "Pushed ..." report differ per call site (different
/// messages, different clone_root splicing) and are NOT consolidated here.
///
/// The two `dtimer_l1!` spans below previously existed only at push_full's
/// copy of this block; the other 4 sites had no L1 timing for these phases.
/// Adding them uniformly gives every push variant the same diagnostic
/// granularity in `omemfs log timers` output -- purely internal timing data
/// (not user-facing stdout, no doc/test contract on which commands emit
/// which timer labels), so this is a diagnostics improvement, not an
/// observable behavior change.
fn upload_and_finalize(
    local: &dyn ObjectStore,
    writer: &mut PackWriter,
    new_root_hash: &Hash,
    remote_key: Option<&crate::codec::encrypt::EncryptKey>,
    io_record: &IoRecord,
) -> Result<(), Error> {
    {
        let phase = crate::progress::begin_phase("Upload objects");
        let _t = dtimer_l1!("upload missing objects");
        upload_missing_with_progress(local, writer, new_root_hash, remote_key, Some(&phase))?;
        phase.complete("done");
    }
    {
        let phase = crate::progress::begin_phase("Finalize remote");
        let _t = dtimer_l1!("finalize pack + update INDEX_ROOT");
        let deltas_after = writer.finish(new_root_hash)?;
        io_record.set_delta_count_after(deltas_after);
        let (pack_count, pack_sizes) = writer.io_pack_stats();
        io_record.set_pack_stats(pack_count, pack_sizes);
        phase.complete("ok");
    }
    // design/02 "Fill-rate reporting": push never resizes the Bloom filter
    // itself (only `omemfs pack` does), so once this push's insertions push
    // it past the capacity its current size was designed for, warn so the
    // user knows to run `omemfs pack` rather than silently accumulating
    // false positives until the next scheduled pack.
    if let Some((elements, capacity)) = writer.bloom_over_capacity() {
        eprintln!(
            "warning: Bloom filter has {} elements but was designed for ~{} \
             -- run `omemfs pack` to resize it",
            elements, capacity
        );
    }
    Ok(())
}

pub fn upload_missing_with_progress(
    local: &dyn ObjectStore,
    remote: &dyn ObjectStore,
    root_hash: &Hash,
    remote_key: Option<&crate::codec::encrypt::EncryptKey>,
    phase: Option<&crate::progress::phase_view::PhaseHandle>,
) -> Result<(), Error> {
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

    // Shared counters so the serial and parallel paths report identically.
    // Counting is approximate for the "cached" case (it skips a subtree whose
    // size is unknown), unchanged from the pre-Phase-5 behaviour.
    let uploaded = AtomicUsize::new(0);
    let cached = AtomicUsize::new(0);

    // Bound the total size of upload buffers held resident across all workers,
    // independently of the worker count (design/02 "Two independent knobs").
    let budget = crate::commands::transfer::ByteBudget::new(
        crate::commands::transfer::resolve_memory_budget(),
    );

    // Process one node: upload it if absent from `remote` (and return its bytes
    // so its children are walked), or count it cached and skip its subtree.
    let process = |hash: &Hash| -> Result<Option<Vec<u8>>, Error> {
        if remote.exists(hash)? {
            // Skip this object AND its entire subtree (same as transfer_objects
            // with traverse_existing=false), avoiding the O(n) traversal.
            let c = cached.fetch_add(1, Relaxed) + 1;
            if let Some(ph) = phase {
                let u = uploaded.load(Relaxed);
                ph.detail(format!(
                    "cached {} ({} uploaded, {} cached)",
                    &hash.as_str()[..8],
                    u,
                    c
                ));
            }
            return Ok(None);
        }
        // Reserve budget for the object buffer before reading it, so peak memory
        // stays bounded regardless of concurrency. The hint is the local stored
        // size (a cheap stat); the guard releases after the upload completes.
        // A missing/unknown size reserves nothing — no deadlock.
        let hint = local.size(hash).unwrap_or(0);
        let _permit = budget.acquire(hint);
        // Push scanning seals every referenced blob in the local store. Never
        // re-read the live working tree after the root hash has been built.
        //
        // Map a local-read `ObjectNotFound` into the dedicated
        // `LocalObjectMissing` variant (design/07 "The governing invariant",
        // point 4; "Upload-failure recovery (final backstop)"): this is the
        // ONLY site that produces it, so the L3 backstop below can trigger on
        // exactly this failure shape -- a blob this process itself believed
        // was staged locally turning out not to be readable -- and never on
        // a remote-side `ObjectNotFound` (e.g. from `remote.exists()` just
        // above), which means something unrelated to STAT_CACHE's SEALED
        // bits.
        let serialised = codec::store_read(local, hash, None).map_err(|e| match e {
            Error::ObjectNotFound(h) => Error::LocalObjectMissing(h),
            other => other,
        })?;
        codec::store_write(remote, hash, &serialised, remote_key)?;
        let u = uploaded.fetch_add(1, Relaxed) + 1;
        if let Some(ph) = phase {
            let c = cached.load(Relaxed);
            ph.detail(format!(
                "upload {} ({} uploaded, {} cached)",
                &hash.as_str()[..8],
                u,
                c
            ));
        }
        Ok(Some(serialised))
    };

    // Uploads land on `remote` (the cloud side); `local` is the local cache.
    // Resolve from the max so the cloud side drives parallelism.
    let workers = crate::commands::transfer::resolve_concurrency(local)
        .max(crate::commands::transfer::resolve_concurrency(remote));
    if workers >= 2 {
        return crate::commands::transfer::parallel_bfs(
            std::slice::from_ref(root_hash),
            workers,
            &process,
        );
    }

    // Serial path (default for local backends) — byte-identical to the
    // pre-Phase-5 loop.
    use std::collections::{HashSet, VecDeque};
    let mut queue: VecDeque<Hash> = VecDeque::new();
    let mut visited: HashSet<Hash> = HashSet::new();
    queue.push_back(root_hash.clone());
    while let Some(hash) = queue.pop_front() {
        // Skip already-processed hashes (shared subtrees / duplicate blobs).
        if !visited.insert(hash.clone()) {
            continue;
        }
        if let Some(data) = process(&hash)? {
            for child in crate::commands::transfer::child_hashes(&data) {
                queue.push_back(child);
            }
        }
    }
    Ok(())
}

/// If the scoped path `rel` is itself a stub (a file stub `<rel>.omemfs-stub`
/// with no materialised file, or a fully-stubbed directory whose only content is
/// `.omemfs-stub`), return the `TreeEntry` it represents. A stub means "content
/// exists, not materialised" — pushing it must keep the recorded content, never
/// treat the missing materialised file as a deletion (design/08 "Interaction
/// with push").
///
/// Returns `None` when `rel` is not a (pure) stub: a real file/dir present, a
/// partially-expanded directory (handled by the scan/merge path), or absent.
pub(crate) fn resolve_scoped_stub(work_dir: &std::path::Path, rel: &str) -> Option<TreeEntry> {
    let leaf = rel.rsplit('/').next().unwrap_or(rel).to_string();
    let abs = work_dir.join(rel);

    // File stub: `<rel>.omemfs-stub` exists and the real file is absent.
    if !abs.exists()
        && crate::stub::exists(work_dir, rel)
        && let Ok(Some(record)) = crate::stub::read(work_dir, rel)
        && record.target_type == crate::stub::StubTargetType::Blob
    {
        return Some(TreeEntry::Blob {
            name: leaf,
            hash: record.hash,
            mtime: record.mtime,
            size: record.size,
            mode: record.mode,
        });
    }

    // Directory stub: `<rel>/.omemfs-stub` exists. Only treat as a pure stub
    // when the directory has no real children (fully stubbed). A partially
    // expanded directory is handled by the working-tree scan/merge path.
    if abs.is_dir() && crate::stub::dir_exists(work_dir, rel) {
        let fully_stubbed = std::fs::read_dir(&abs)
            .map(|rd| {
                !rd.flatten()
                    .any(|e| e.file_name().to_string_lossy() != crate::stub::DIR_STUB_NAME)
            })
            .unwrap_or(false);
        if fully_stubbed
            && let Ok(Some(record)) = crate::stub::read_dir_stub(work_dir, rel)
            && record.target_type == crate::stub::StubTargetType::Tree
        {
            return Some(TreeEntry::Tree {
                name: leaf,
                hash: record.hash,
                mtime: record.mtime,
                size: record.size,
                blob_count: record.blob_count,
            });
        }
    }

    None
}

/// `None` (no root recorded) and `Some(empty_tree_hash)` both mean "nothing
/// has been pushed to this remote yet" — equivalent states for the lost-
/// update precheck below (design/03 "Push → Steps" step 4b).
fn is_empty_root(h: Option<&Hash>) -> bool {
    match h {
        None => true,
        Some(hash) => *hash == crate::object::Tree::empty_tree_hash(),
    }
}

/// Whole-root push precheck (design/03 "Push → Steps", step 4b): reject a
/// push whose remote root (as observed in THIS push's own snapshot, never a
/// fresh read) has advanced past `clone_root`, before any object is
/// built/uploaded. This is the primary defense against a lost update: the
/// CAS at finish time alone cannot catch divergence that already existed
/// before this push started, because in that case the CAS's own
/// precondition (the token read at push start still matching at finish) is
/// trivially satisfied -- nothing changed *during* this push's own window.
///
/// `new_root_hash` is the tree this push is actually about to write (already
/// scanned before this check runs). If it already equals `remote_root`, this
/// push cannot lose anyone's update no matter how stale `clone_root` is: the
/// remote's content would be unchanged by it. This is what makes a push
/// immediately after `omemfs conflict accept-remote` a safe no-op even though
/// pull's conflict-abort left `clone_root` pinned at the pre-conflict base
/// (design/03_sync_model.md, "Conflict handling": a conflicting pull never
/// updates `clone_root`) -- the resolved working tree already matches the
/// remote exactly (same content, and `accept-remote` restores the remote's
/// tracked mtime/mode too), so its scanned hash equals `remote_root`.
///
/// Both `clone_root`/`remote_root` sides are also normalized so a fresh
/// remote and a clone that has never synced are treated as equivalent,
/// allowing the very first push (and a push right after
/// `clone --new`/`clone --existing`) to proceed.
fn require_remote_not_advanced(
    clone_root: Option<&Hash>,
    remote_root: Option<&Hash>,
    new_root_hash: &Hash,
) -> Result<(), Error> {
    if remote_root == Some(new_root_hash) {
        return Ok(());
    }
    if is_empty_root(clone_root) && is_empty_root(remote_root) {
        return Ok(());
    }
    if clone_root == remote_root {
        return Ok(());
    }
    Err(Error::CasFailed)
}

/// `tree_ops::navigate_entry`, tolerating a missing root (`None` means
/// "nothing at this path" without even touching `store`). Used by the
/// per-path divergence precheck in `push_scoped`, where either side's root
/// may legitimately be absent (a fresh remote, or a clone that has never
/// synced).
///
/// Returns the full `TreeEntry` (not just its `Hash`) because a `Blob`
/// entry's `hash` field alone does not capture an executable-bit-only
/// (mode) change (design/03_sync_model.md, "Path-scoped push" step 2b) --
/// comparing bare hashes here would let a scoped push silently overwrite a
/// remote-side or clone-root-side mode-only change to the exact leaf being
/// pushed, or miss its own mode-only divergence.
fn navigate_entry_opt(
    root: Option<&Hash>,
    components: &[&str],
    store: &dyn ObjectStore,
) -> Result<Option<TreeEntry>, Error> {
    match root {
        None => Ok(None),
        Some(h) => tree_ops::navigate_entry(h, components, store),
    }
}

/// True when two `TreeEntry`s at the same path represent the same content
/// for the purpose of "has this path actually changed". For a `Blob`, both
/// the content hash and the executable-bit `mode` must agree -- a hash-only
/// comparison would treat a `chmod`-only change as "unchanged" (design/03
/// "Path-scoped push" step 1c / step 2b, "Diff output", `ls --dirty`'s own
/// dirty detection). For a `Tree`, the tree hash alone is sufficient: it
/// already covers every descendant's mode. `Symlink` entries have no
/// content hash to compare by, so they are compared by target instead --
/// this function is currently only reached for `Blob`/`Tree` scoped_hash
/// comparisons, but is written to handle every variant correctly.
fn tree_entries_equal(a: &TreeEntry, b: &TreeEntry) -> bool {
    match (a, b) {
        (
            TreeEntry::Blob {
                hash: h1, mode: m1, ..
            },
            TreeEntry::Blob {
                hash: h2, mode: m2, ..
            },
        ) => h1 == h2 && m1 == m2,
        (TreeEntry::Tree { hash: h1, .. }, TreeEntry::Tree { hash: h2, .. }) => h1 == h2,
        (TreeEntry::Symlink { target: t1, .. }, TreeEntry::Symlink { target: t2, .. }) => t1 == t2,
        _ => false,
    }
}

/// Returns true if `rel` refers to an internal system path that must never be pushed.
fn is_system_path(rel: &str) -> bool {
    let first = rel.split('/').next().unwrap_or("");
    first == ".omemfs"
}

/// Returns true if `rel` or any ancestor component of `rel` is matched by
/// the ignore rules, so that `omemfs push rel` should be rejected.
pub(crate) fn is_scoped_path_ignored(filters: &crate::filter::FilterSet, rel: &str) -> bool {
    let parts: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
    for len in 1..=parts.len() {
        let prefix = parts[..len].join("/");
        if filters.is_ignored(&prefix) {
            return true;
        }
    }
    false
}

/// Print a plain message to stdout with optional color style applied.
/// `style` = None means no color is applied.
fn colored_println(msg: &str, style: Option<anstyle::Style>) {
    let mut out = Output::for_stdout();
    let text = match style {
        Some(s) => paint(out.colored(), s, msg),
        None => msg.to_string(),
    };
    let _ = out.writeln(&text);
    let _ = out.finish();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::local::LocalStore;
    use std::fs;
    use std::time::{Duration, SystemTime};
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // L3 trigger precision (design/07 "Upload-failure recovery", defect 5):
    // only a confirmed *local* read failure (`Error::LocalObjectMissing`) may
    // trigger the seal-clearing backstop; any other error -- in particular a
    // remote-side `Error::ObjectNotFound` -- must propagate unchanged and
    // must not rewrite STAT_CACHE.
    // -----------------------------------------------------------------------

    /// A store whose `exists()` (and every other call) always fails with a
    /// fixed `ObjectNotFound`, simulating a remote backend that cannot answer
    /// an existence probe -- unrelated to whether the *local* copy is
    /// present.
    struct AlwaysErrsOnExists {
        err_msg: String,
    }

    impl ObjectStore for AlwaysErrsOnExists {
        fn exists(&self, _hash: &Hash) -> Result<bool, Error> {
            Err(Error::ObjectNotFound(self.err_msg.clone()))
        }
        fn size(&self, _hash: &Hash) -> Result<u64, Error> {
            Err(Error::ObjectNotFound(self.err_msg.clone()))
        }
        fn list_with_sizes(&self) -> Result<Vec<(String, u64)>, Error> {
            Ok(Vec::new())
        }
        fn open_read(&self, _hash: &Hash) -> Result<Box<dyn std::io::Read>, Error> {
            Err(Error::ObjectNotFound(self.err_msg.clone()))
        }
        fn write_from(&self, _hash: &Hash, _reader: &mut dyn std::io::Read) -> Result<(), Error> {
            Ok(())
        }
    }

    #[test]
    fn upload_missing_local_read_failure_maps_to_local_object_missing() {
        // A hash that is referenced but never actually stored locally: the
        // process closure's `codec::store_read(local, hash, None)` call fails
        // with `ObjectNotFound`, which must be mapped to `LocalObjectMissing`.
        let local_dir = TempDir::new().unwrap();
        let local = LocalStore::for_cache(local_dir.path());
        let remote_dir = TempDir::new().unwrap();
        let remote = LocalStore::for_remote(remote_dir.path());
        let hash = crate::object::blob_hash(b"never stored locally");

        let err = upload_missing_with_progress(&local, &remote, &hash, None, None).unwrap_err();
        assert!(
            matches!(err, Error::LocalObjectMissing(_)),
            "a local read failure while uploading must map to Error::LocalObjectMissing, got: {:?}",
            err
        );
    }

    #[test]
    fn upload_missing_propagates_remote_side_object_not_found_unchanged() {
        // remote.exists() fails with ObjectNotFound (a remote-side probe
        // failure) BEFORE the local read is ever attempted: this must
        // propagate as a plain ObjectNotFound, never LocalObjectMissing.
        let local_dir = TempDir::new().unwrap();
        let local = LocalStore::for_cache(local_dir.path());
        let remote = AlwaysErrsOnExists {
            err_msg: "remote-side probe failure".to_string(),
        };
        let hash = crate::object::blob_hash(b"irrelevant content");

        let err = upload_missing_with_progress(&local, &remote, &hash, None, None).unwrap_err();
        match err {
            Error::ObjectNotFound(ref m) => assert_eq!(m, "remote-side probe failure"),
            other => panic!(
                "a remote-side ObjectNotFound must propagate unchanged, not become \
                 LocalObjectMissing, got: {:?}",
                other
            ),
        }
    }

    #[test]
    fn recover_from_missing_object_only_fires_on_local_object_missing() {
        let tmp = TempDir::new().unwrap();
        let omemfs_dir = tmp.path().join(".omemfs");
        fs::create_dir_all(omemfs_dir.join("objects")).unwrap();
        let mut cache = StatCache::default();
        let old_mtime = SystemTime::now() - Duration::from_secs(1000);
        cache.update(
            "a.txt".to_string(),
            old_mtime,
            10,
            crate::object::blob_hash(b"a"),
            true,
            SystemTime::now(),
        );
        cache.write(&omemfs_dir).unwrap();
        let before = fs::read(omemfs_dir.join("STAT_CACHE")).unwrap();

        // A remote-side ObjectNotFound must NOT clear seals or rewrite
        // STAT_CACHE at all.
        let err = recover_from_missing_object_if_applicable(
            &omemfs_dir,
            Error::ObjectNotFound("remote object".to_string()),
        );
        assert!(
            matches!(err, Error::ObjectNotFound(_)),
            "a non-LocalObjectMissing error must pass through unchanged"
        );
        let after = fs::read(omemfs_dir.join("STAT_CACHE")).unwrap();
        assert_eq!(
            before, after,
            "STAT_CACHE must not be rewritten for a non-L3-triggering error"
        );

        // A LocalObjectMissing error MUST clear seals and rewrite STAT_CACHE.
        let err = recover_from_missing_object_if_applicable(
            &omemfs_dir,
            Error::LocalObjectMissing("a".repeat(64)),
        );
        assert!(matches!(err, Error::Other(_)));
        let reloaded = StatCache::read(&omemfs_dir);
        assert!(
            !reloaded.is_sealed("a.txt"),
            "L3 must have cleared every SEALED bit"
        );
    }

}
