use std::path::PathBuf;
use std::sync::Arc;

use crate::codec;
use crate::codec::pack::writer::PackWriter;
use crate::error::Error;
use crate::io_stats;
use crate::object::{Hash, TreeEntry};
use crate::repo::Repo;
use crate::scan::{refresh_stat_cache, scan_and_store_with_cache};
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
        let omemfs_dir = repo.work_dir.join(".omemfs");
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
    let clone_root_flat = match clone_root.as_ref() {
        Some(hash) => Some(tree_ops::flatten_tree_entries_for_push(hash, local)?),
        None => None,
    };
    let omemfs_dir = repo.work_dir.join(".omemfs");
    let stat_cache = StatCache::read(&omemfs_dir);
    let (new_root_hash, conflict_paths, unstable_paths) = {
        let phase = crate::progress::begin_phase("Scan working tree");
        let _t = dtimer_l1!("scan working tree");
        let scan_result = scan_and_store_with_cache(
            &repo.work_dir,
            &repo.work_dir,
            local,
            clone_root_flat.as_ref(),
            &stat_cache,
            true, // push needs blob objects in the local store for upload
        )?;
        let h = scan_result.root_hash.clone();
        let file_count = scan_result.files.len();
        let conflict_paths = scan_result.side.conflict_paths;
        let unstable_paths = scan_result.side.unstable_paths;
        refresh_stat_cache(stat_cache, &scan_result.files, &omemfs_dir);
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
    let _ = writer.remote_root_snapshot()?; // touch the snapshot at push start
    // Post-clone sync guard (origin only): an absent index root with a synced
    // clone_root is a hard error (design/03).
    post_clone_sync_guard(clone_root.as_ref(), writer.index_root_present())?;
    upload_and_finalize(
        local,
        &mut writer,
        &new_root_hash,
        remote_key.as_ref(),
        &io_record,
    )?;
    repo.write_clone_root(&new_root_hash)?;

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
    let omemfs_dir = repo.work_dir.join(".omemfs");

    // Construct the writer first so the INDEX_ROOT snapshot is captured at push
    // start; the remote root used for all splices below comes from that same
    // snapshot so the final CAS guards the whole read → upload → write window.
    let (mut writer, remote_key) = repo.pack_writer(remote_name, &io_record)?;
    post_clone_sync_guard(clone_root.as_ref(), writer.index_root_present())?;
    let remote_root = writer.remote_root_snapshot()?;

    // STAT_CACHE scope-limited load (design/07 "Read optimisation: scope-limited
    // load"): a single scoped path parses only that path's slice of STAT_CACHE
    // rather than the whole file. Multiple paths fall back to a full read.
    let mut stat_cache = if rels.len() == 1 {
        StatCache::read_scoped(&omemfs_dir, &rels[0])
    } else {
        StatCache::read(&omemfs_dir)
    };

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
            // stub instead of leaving it out of scoped_results).
            let existing_hash = if let Some(ref cr) = clone_root {
                tree_ops::navigate(cr, &components, local)?
            } else {
                None
            };
            if existing_hash.as_ref() == Some(&scoped_hash) {
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

        if !abs_path.exists() {
            // Deletion intent. Whether this is a real removal or a no-op (the
            // path is already absent on the remote) is decided later, in the
            // splice loop, against the remote-root snapshot — see the is_delete
            // branch there. Record the intent here regardless.
            scoped_results.push(ScopedResult {
                rel: rel.clone(),
                components: components.iter().map(|s| s.to_string()).collect(),
                scoped_hash: Hash::compute(b""),
                abs_path_is_dir: false,
                is_delete: true,
            });
            continue;
        }

        let phase = crate::progress::begin_phase(format!("Scan {}", rel));
        let scoped_hash = if abs_path.is_dir() {
            // Scoped flatten (design/03 "Path-scoped push"): build the mtime
            // pre-filter map from only `rel`'s subtree of the clone root,
            // instead of flattening the whole clone root for every path.
            let scoped_flat = match clone_root.as_ref() {
                Some(hash) => Some(tree_ops::flatten_tree_entries_scoped_for_push(
                    hash, rel, local,
                )?),
                None => None,
            };
            let scan_result = match scan_and_store_with_cache(
                &repo.work_dir,
                &abs_path,
                local,
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
            for (path, scanned) in &scan_result.files {
                if !scanned.cache_hit {
                    stat_cache.update(
                        path.clone(),
                        scanned.fs_mtime,
                        scanned.fs_size,
                        scanned.hash.clone(),
                    );
                }
            }
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

        // Skip a path whose freshly scanned hash already matches clone_root:
        // nothing to push for it (matches push_scoped's single-path
        // short-circuit; refactor-instructions.md Phase 8 E7 -- without this
        // check a multi-path push of N unchanged paths always performed a
        // full write cycle instead of reporting "Nothing to push.").
        let existing_hash = if let Some(ref cr) = clone_root {
            tree_ops::navigate(cr, &components, local)?
        } else {
            None
        };
        if existing_hash.as_ref() == Some(&scoped_hash) {
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
        return Ok(());
    }

    if dry_run {
        let mut out = Output::for_stdout();
        let colored = out.colored();
        let styles = out.styles;
        for sr in &scoped_results {
            if sr.is_delete {
                let path_str = paint(colored, styles.deleted, &sr.rel);
                out.writeln(&format!("Would delete {}", path_str))?;
            } else {
                let path_str = paint(colored, styles.added, &sr.rel);
                let hash_str = paint(colored, styles.hash, &sr.scoped_hash.as_str()[..8]);
                out.writeln(&format!("Would push {} → {}", path_str, hash_str))?;
            }
        }
        out.finish()?;
        report_unstable_paths(&unstable_paths);
        return Ok(());
    }

    // Ensure intermediate tree objects from remote_root are in local cache for each path.
    if let Some(ref rr) = remote_root {
        for sr in &scoped_results {
            let comps: Vec<&str> = sr.components.iter().map(|s| s.as_str()).collect();
            tree_ops::ensure_path_in_store(
                writer.as_remote_store(),
                local,
                rr,
                &comps[..comps.len().saturating_sub(1)],
            )?;
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
                Some(ref rr) => match tree_ops::remove_entry(rr, &comps, local)? {
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
                local,
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
                local,
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
        // no-op return.
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
    )?;
    repo.write_clone_root(&new_clone_root)?;

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
                let path_str = paint(colored, styles.deleted, &only.rel);
                out.writeln(&format!("Deleted {} from {}.", path_str, remote_name))?;
            }
            [only] => {
                let path_str = paint(colored, styles.added, &only.rel);
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

/// Remove paths that are descendants of another path in the list.
/// Input paths are repo-relative forward-slash strings (no leading slash).
pub fn deduplicate_paths(mut paths: Vec<String>) -> Vec<String> {
    paths.sort();
    paths.dedup();
    let prefixes: Vec<String> = paths.iter().map(|p| format!("{}/", p)).collect();
    let mut result: Vec<String> = Vec::new();
    'outer: for (i, path) in paths.iter().enumerate() {
        for (j, prefix) in prefixes.iter().enumerate() {
            if i != j && path.starts_with(prefix.as_str()) {
                continue 'outer;
            }
        }
        result.push(path.clone());
    }
    result
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
        eprintln!("  {}", p);
    }
    Error::Other("unresolved conflicts".to_string())
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
        eprintln!("  {}", path);
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
    if let Err(e) = upload_missing(local, &writer, clone_root_hash, backup_key.as_ref()) {
        eprintln!("warning: backup upload failed: {e}");
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

/// Transfer all objects reachable from `root_hash` that are absent in `dst`.
/// Reads from `src` (using `src_key` for decryption) and writes to `dst`.
/// `src_key` is `None` when `src` is unencrypted.
///
/// `dst` is always the unencrypted local cache in every current caller (push
/// uploads local->remote through a different path; this function is only used
/// for the reverse, remote/cache->local direction), so writes to `dst` are
/// never encrypted here. If a future caller needs to transfer into an
/// encrypted `dst`, add the key parameter back rather than assuming `None`.
///
/// When `traverse_existing` is `false`, objects already present in `dst` are
/// skipped along with their entire subtree (fast path for upload/clone).
/// When `true`, existing objects in `dst` are still traversed so their
/// children can be checked — use this when `dst` may be partially populated
/// (e.g. after a partial pull).
pub fn transfer_objects(
    src: &dyn crate::store::ObjectStore,
    dst: &dyn crate::store::ObjectStore,
    root_hash: &Hash,
    src_key: Option<&crate::codec::encrypt::EncryptKey>,
    traverse_existing: bool,
) -> Result<(), Error> {
    // Bound the total size of object buffers held resident across all workers,
    // independently of the worker count (design/02 "Two independent knobs").
    let budget = crate::commands::transfer::ByteBudget::new(
        crate::commands::transfer::resolve_memory_budget(),
    );

    // Process one node: copy it to `dst` if missing (or read it back when
    // already present and we are traversing existing objects), and return the
    // bytes whose children should be walked next (`None` to walk nothing).
    let process = |hash: &Hash| -> Result<Option<Vec<u8>>, Error> {
        if dst.exists(hash)? {
            if !traverse_existing {
                return Ok(None);
            }
            // Already in dst; read locally to traverse children without
            // re-writing. A read miss here is tolerated (skip its subtree).
            // Reserve budget for the read-back buffer (size hint from dst).
            let hint = dst.size(hash).unwrap_or(0);
            let _permit = budget.acquire(hint);
            return Ok(codec::store_read(dst, hash, None).ok());
        }
        // Reserve budget for the object buffer before reading it; the guard
        // releases after the write completes. The hint is the source-side
        // stored size (a cheap stat). Unknown size reserves nothing — no
        // deadlock.
        let hint = src.size(hash).unwrap_or(0);
        let _permit = budget.acquire(hint);
        let serialised = codec::store_read(src, hash, src_key)?;
        codec::store_write(dst, hash, &serialised, None)?;
        Ok(Some(serialised))
    };

    // Resolve from whichever side is network-bound: push copies local->cloud
    // (dst is cloud), while clone/pull/expand copy cloud->local (src is cloud).
    // Taking the max lets the cloud side drive parallelism in either direction.
    let workers = crate::commands::transfer::resolve_concurrency(src)
        .max(crate::commands::transfer::resolve_concurrency(dst));
    if workers >= 2 {
        return crate::commands::transfer::parallel_bfs(root_hash, workers, &process);
    }

    // Serial path (default for local backends) — byte-identical to the
    // pre-Phase-5 loop.
    use std::collections::{HashSet, VecDeque};
    let mut queue: VecDeque<Hash> = VecDeque::new();
    let mut visited: HashSet<Hash> = HashSet::new();
    queue.push_back(root_hash.clone());
    while let Some(hash) = queue.pop_front() {
        // Skip hashes we have already processed; shared subtrees and duplicate
        // blobs (identical content under multiple paths) only need handling once.
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
        let serialised = codec::store_read(local, hash, None)?;
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
        return crate::commands::transfer::parallel_bfs(root_hash, workers, &process);
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

/// Post-clone sync guard (design/03). Returns an error when the local clone has
/// sync history (clone_root is present and not the empty-tree hash) but the
/// origin index root is absent — an absent index root would otherwise be
/// silently interpreted as "empty remote" and corrupt the clone.
///
/// An absent index root is allowed only when this clone has never synced any
/// content: clone_root is missing (None) or equals the empty-tree hash. The
/// guard applies to the origin remote only; backup push is exempt.
pub fn post_clone_sync_guard(
    clone_root: Option<&Hash>,
    index_root_present: bool,
) -> Result<(), Error> {
    if index_root_present {
        return Ok(());
    }
    let never_synced = match clone_root {
        None => true,
        Some(h) => *h == crate::object::Tree::empty_tree_hash(),
    };
    if never_synced {
        Ok(())
    } else {
        Err(Error::Other(
            "index root not found on remote\n\
             The remote appears empty, but this clone has sync history.\n\
             Possible causes: wrong encryption key, wrong URL/prefix, or the remote was reset."
                .to_string(),
        ))
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
