use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use crate::{dlog_l1, dtimer_l1};

use filetime::FileTime;

use crate::codec;
use crate::codec::pack::reader::PackReader;
use crate::error::Error;
use crate::filter::FilterSet;
use crate::io_stats;
use crate::line_merge::{self, MergeOutcome};
use crate::object::{Hash, Tree, TreeEntry};
use crate::repo::Repo;
use crate::scan::{refresh_stat_cache, scan_and_store_with_stub_store};
use crate::stat_cache::StatCache;
use crate::store::ObjectStore;
use crate::store::local::LocalStore;
use crate::store::stats::IoRecord;
use crate::stub::{self, StubRecord, StubTargetType};
use crate::term::{Output, Styles, paint};
use crate::tree_ops;
use crate::tree_ops::LazyTreeStore;

pub struct PullOptions {
    pub work_dir: PathBuf,
    pub current_dir: PathBuf,
    pub paths: Vec<PathBuf>,
    pub dry_run: bool,
    pub stub_threshold: u64,
}

pub fn run(opts: PullOptions) -> Result<(), Error> {
    let started = std::time::Instant::now();
    let repo = Repo::open(&opts.work_dir)?;
    crate::progress::notify_repo_dir(&repo.work_dir);
    let _lock = repo.acquire_lock()?;
    let remote_name = "origin";
    let _t = dtimer_l1!("pull");
    // When no paths given, default to the current directory.
    // If cwd == repo root, this normalizes to "" and pull_full is used.
    let paths = if opts.paths.is_empty() {
        vec![opts.current_dir.clone()]
    } else {
        opts.paths
    };
    let io_record = Arc::new(IoRecord::default());
    let result = pull_scoped(
        &repo,
        remote_name,
        &paths,
        &opts.current_dir,
        opts.dry_run,
        opts.stub_threshold,
        Arc::clone(&io_record),
    );
    if result.is_ok() && !opts.dry_run {
        let omemfs_dir = repo.omemfs_dir();
        let duration_ms = started.elapsed().as_millis() as u64;
        io_stats::append_record(&omemfs_dir, "pull", remote_name, &io_record, duration_ms);
    }
    result
}

// ---------------------------------------------------------------------------
// Full pull
// ---------------------------------------------------------------------------

fn pull_full(
    repo: &Repo,
    remote_name: &str,
    dry_run: bool,
    stub_threshold: u64,
    io_record: Arc<IoRecord>,
) -> Result<(), Error> {
    let local = repo.local_store();
    let (pack_reader, _remote, remote_key) = repo.pack_reader(remote_name, Some(&io_record))?;

    let clone_root = repo.read_clone_root()?;
    // Post-clone sync guard (origin only): an absent index root with a synced
    // clone_root is a hard error (design/03).
    let index_root = pack_reader.read_index_root()?;
    crate::commands::transfer::post_clone_sync_guard(clone_root.as_ref(), index_root.is_some())?;

    let remote_root = match index_root.and_then(|ir| ir.remote_root_hash()) {
        None => {
            plain_println("Already up to date.");
            return Ok(());
        }
        Some(h) => h,
    };

    if clone_root.as_ref() == Some(&remote_root) {
        plain_println("Already up to date.");
        return Ok(());
    }

    dlog_l1!("remote root: {}", &remote_root.as_str()[..8]);

    // Both clone_root AND remote_root tree objects are read through a
    // read-through store. After a lazy clone the local cache holds neither the
    // clone_root tree objects of stubbed subtrees nor the remote_root skeleton.
    // There is deliberately NO eager skeleton pre-download: `diff_recursive`
    // short-circuits on equal subtree hashes and descends only differing Tree
    // entries, so reading both sides through `lazy` fetches only the subtrees
    // that actually changed (design/03). Changed/added blob content is fetched
    // on demand in `apply_diff` via the pack reader.
    let lazy = LazyTreeStore::new(&local, &pack_reader, remote_key.as_ref());
    let remote_diff = diff_trees(
        clone_root.as_ref(),
        Some(&remote_root),
        &lazy,
        stub_threshold,
    )?;
    if remote_diff.is_empty() {
        plain_println("Already up to date.");
        repo.write_clone_root(&remote_root)?;
        return Ok(());
    }

    // Check working tree for local changes.
    // Pass the flattened clone_root so the scan can reuse stored tree-object
    // entries for unchanged paths (mtime stability — avoids tree-object churn).
    // Bounded to working-tree-present subtrees so a stubbed clone_root is not
    // fully fetched through the lazy store (design/03 scan optimization).
    let clone_root_flat = clone_root
        .as_ref()
        .and_then(|h| flatten_clone_root_present(h, &repo.work_dir, &lazy).ok());
    let omemfs_dir = repo.omemfs_dir();
    let stat_cache = StatCache::read(&omemfs_dir);
    let (working_hash, observed) = {
        let phase = crate::progress::begin_phase("Scan working tree");
        let _t = dtimer_l1!("scan working tree");
        // `stub_store: &lazy`: a partial-expansion directory's `.omemfs-stub`
        // record (design/08_stub_system.md, "Partial expansion") names a
        // clone-root-rooted tree object that may not be in the local cache
        // after a lazy clone (the whole directory was stubbed as a unit) --
        // the same gap `clone_root_flat` above is already routed through
        // `lazy` to cover. `store: &local` keeps every `.exists()`-based
        // decision in this scan strictly local-only (see
        // `scan_and_store_with_stub_store`'s doc comment for why the two
        // must never simply be the same value).
        let scan_result = scan_and_store_with_stub_store(
            &repo.work_dir,
            &repo.work_dir,
            &local,
            &lazy,
            clone_root_flat.as_ref(),
            &stat_cache,
            false,
        )?;
        let file_count = scan_result.files.len();
        refresh_stat_cache(stat_cache, &scan_result.files, &omemfs_dir, false);
        dlog_l1!(
            "working tree root: {}",
            &scan_result.root_hash.as_str()[..8]
        );
        phase.complete(format!("{} files", file_count));
        let root_hash = scan_result.root_hash;
        (
            root_hash,
            ScanObservation::with_stubs(
                scan_result.files,
                scan_result.side.seen_other,
                scan_result.side.seen_stub,
            ),
        )
    };
    let local_diff = diff_trees(clone_root.as_ref(), Some(&working_hash), &lazy, 0)?;

    // Classify conflicts and build a clean (non-conflicting) diff.
    let (clean_diff, mut conflicts) = classify_conflicts(&remote_diff, &local_diff);

    if dry_run {
        let mut out = Output::for_stdout();
        let colored = out.colored();
        let styles = out.styles;
        let mut paths: Vec<&String> = remote_diff.keys().collect();
        paths.sort();
        for path in paths {
            let change = &remote_diff[path];
            out.writeln(&format!(
                "  {}",
                paint_diff_entry(colored, &styles, path, change.label())
            ))?;
        }
        out.finish()?;
        return Ok(());
    }

    // [line_merge] auto-merge pass (design/15_line_history_merge.md, "`pull`
    // integration point"): resolve (but do not yet write) a merge for
    // remaining conflicts on paths matching `[line_merge]`, before the
    // conflict-abort check below. A resolved path is removed from
    // `conflicts` here, but its bytes are deliberately not written to the
    // working tree until after the abort check confirms the pull is going
    // to proceed -- the atomic-abort policy below requires that NOTHING is
    // applied to the working tree while any conflict (line-merge or not)
    // remains, and this path's own merge is not exempt from that.
    let filter_set = FilterSet::load(&repo.work_dir);
    let resolved_line_merges = resolve_line_merges(
        &mut conflicts,
        &remote_diff,
        &local_diff,
        &filter_set,
        &repo.work_dir,
        clone_root.as_ref(),
        &lazy,
        |p| p.to_string(),
    );

    // Atomic-abort policy (design/03, design/04): if ANY conflict is detected,
    // NOTHING is applied to the working tree. Write the conflict helper files,
    // leave the working tree otherwise untouched, do NOT update clone_root, and
    // exit non-zero. Conflicts must be detected BEFORE any working-tree mutation.
    if !conflicts.is_empty() {
        for path in &conflicts {
            let abs = repo.work_dir.join(path);
            write_conflict_for_path(
                &repo.work_dir,
                &abs,
                path,
                clone_root.as_ref(),
                &remote_diff,
                &lazy,
                &local,
                &pack_reader,
                remote_key.as_ref(),
            )?;
        }
        print_conflict_result(&conflicts);
        return Err(Error::Conflict);
    }

    let phase_apply = crate::progress::begin_phase("Apply changes");
    // apply_diff reads tree objects (the AddedTree -> materialise_tree fallback
    // inside a local git worktree) and blob content. After a lazy clone neither
    // is in the bare local cache, so it must read through `lazy`, which fetches
    // on demand and caches to local. Blobs are fetched via the pack reader.
    let changed = apply_diff(
        &clean_diff,
        &repo.work_dir,
        &local,
        &lazy,
        &repo.work_dir,
        stub_threshold,
        &pack_reader,
        remote_key.as_ref(),
        &observed,
    )?;
    // Every remaining conflict has already been ruled out above, so it is now
    // safe to write the resolved line-merges directly to the working tree.
    let merged_paths =
        write_resolved_line_merges(&resolved_line_merges, &repo.work_dir, &repo.work_dir)?;
    phase_apply.complete(format!("{} paths", changed));

    repo.write_clone_root(&remote_root)?;

    // Stub reconcile (design/08_stub_system.md, "Stub reconcile (pull and
    // restore)"): run only after clone_root has actually advanced above --
    // never while a conflict is outstanding (this point is unreached in that
    // case; see the `Err(Error::Conflict)` return earlier in this function).
    reconcile_stubs(&remote_root, &repo.work_dir, &repo.work_dir, &local)?;

    {
        let mut out = Output::for_stdout();
        let colored = out.colored();
        let styles = out.styles;
        out.writeln(&format!("Pulling from {} ...", remote_name))?;
        let mut paths: Vec<&String> = remote_diff.keys().collect();
        paths.sort();
        for path in paths {
            let change = &remote_diff[path];
            out.writeln(&format!(
                "  {}",
                paint_diff_entry(colored, &styles, path, change.label())
            ))?;
        }
        out.writeln(&format!("{} path(s) updated.", changed))?;
        print_merged_block(&mut out, colored, &styles, &merged_paths)?;

        let preserved: Vec<&String> = local_diff
            .keys()
            .filter(|p| !remote_diff.contains_key(*p))
            .collect();
        if !preserved.is_empty() {
            out.writeln("Your local modifications to the following paths were preserved:")?;
            for p in preserved {
                let p_str = paint(colored, styles.modified, &crate::term::escape_display(&p.to_string()));
                out.writeln(&format!("  modified: {}", p_str))?;
            }
        }
        out.finish()?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Path-scoped pull (single path or multiple paths)
// ---------------------------------------------------------------------------

/// Handles both a single scoped path and multiple scoped paths (the CLI
/// always calls this via `run`; there is no separate single-path
/// implementation -- refactor-instructions.md Phase 8 E7). A one-path call
/// takes exactly the same per-path loop as a multi-path call; the printed
/// header is the only place the two are still told apart (see below), to
/// keep a single-path pull's output byte-identical to before this merge.
fn pull_scoped(
    repo: &Repo,
    remote_name: &str,
    raw_paths: &[PathBuf],
    current_dir: &std::path::Path,
    dry_run: bool,
    stub_threshold: u64,
    io_record: Arc<IoRecord>,
) -> Result<(), Error> {
    // Normalise all paths against the cwd, re-expressed relative to the repo
    // root, then deduplicate ancestors.
    let mut rels: Vec<String> = raw_paths
        .iter()
        .map(|p| crate::repo::normalize_path(p, &repo.work_dir, current_dir))
        .collect::<Result<Vec<_>, _>>()?;

    if rels.iter().any(|r| r.is_empty()) {
        return pull_full(repo, remote_name, dry_run, stub_threshold, io_record);
    }

    rels = crate::commands::transfer::deduplicate_paths(rels);

    let local = repo.local_store();
    let (pack_reader, _remote, remote_key) = repo.pack_reader(remote_name, Some(&io_record))?;

    let clone_root = repo.read_clone_root()?;
    let index_root = pack_reader.read_index_root()?;
    crate::commands::transfer::post_clone_sync_guard(clone_root.as_ref(), index_root.is_some())?;

    let remote_root = match index_root.and_then(|ir| ir.remote_root_hash()) {
        None => {
            plain_println("Already up to date.");
            return Ok(());
        }
        Some(h) => h,
    };

    // Collect per-path diffs.
    struct PathDiff {
        rel: String,
        components: Vec<String>,
        remote_diff: HashMap<String, DiffEntry>,
        local_diff: HashMap<String, DiffEntry>,
        remote_scoped_hash: Option<Hash>,
        remote_scoped_entry: Option<TreeEntry>,
        is_leaf: bool,
        /// The clone-side hash at this scoped subtree root -- the base root
        /// a [line_merge] merge resolves each path's base blob against (see
        /// `resolve_line_merges`). Already computed above; stored here so
        /// the merge pass does not need to re-navigate `clone_root`. For a
        /// leaf (`is_leaf`) target this is the one-entry wrapper tree hash,
        /// matching `remote_scoped_hash` and the diff-map keys, which are the
        /// leaf's file name rather than a subtree-relative path.
        clone_scoped_hash: Option<Hash>,
    }

    // Clone_root tree objects of stubbed subtrees are not in the local cache
    // after a lazy clone; route clone_root reads through a read-through store.
    let lazy = LazyTreeStore::new(&local, &pack_reader, remote_key.as_ref());

    // Flatten clone_root so the scan can reuse unchanged tree-object entries
    // (mtime stability), bounded to working-tree-present subtrees so a stubbed
    // clone_root is not fully fetched through the lazy store.
    let clone_root_flat = clone_root
        .as_ref()
        .and_then(|h| flatten_clone_root_present(h, &repo.work_dir, &lazy).ok());
    let omemfs_dir = repo.omemfs_dir();
    // STAT_CACHE is read once in full (not per-path scoped, unlike push's
    // single-path `read_scoped`) and written back once at the end from the
    // union of every path's scan results (refactor-instructions.md Phase 8
    // step 4: preserve pull's existing "full read + per-path update +
    // write_if_dirty" semantics -- only the *scan* becomes per-path here).
    let stat_cache = StatCache::read(&omemfs_dir);
    let mut scanned_files: HashMap<String, crate::scan::ScannedFile> = HashMap::new();
    let mut seen_other: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut seen_stub: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut leaf_files: HashMap<String, (std::time::SystemTime, u64)> = HashMap::new();

    let mut path_diffs: Vec<PathDiff> = Vec::new();
    for rel in &rels {
        let components: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();

        // Both the remote_root and clone_root sides are read lazily (no skeleton
        // or path pre-download): navigate / diff / mark_deleted_tree go through
        // the read-through store, which fetches only the tree objects actually
        // traversed and only the subtrees that differ. Changed blob content is
        // fetched on demand in apply_diff.
        let remote_scoped_entry = tree_ops::navigate_entry(&remote_root, &components, &lazy)?;
        let clone_scoped_entry = if let Some(ref cr) = clone_root {
            tree_ops::navigate_entry(cr, &components, &lazy)?
        } else {
            None
        };
        // `navigate` returns a blob hash for a file leaf. Diffing that hash as
        // a tree attempts to parse file content as JSON, so compare leaf entries
        // through one-entry wrapper trees instead.
        //
        // A leaf scope is selected whenever at least one EXISTING side (remote
        // or clone-root) is not a Tree -- not only when BOTH existing sides
        // agree on being non-Tree (design/03_sync_model.md, "Leaf-scope type
        // changes"). This also covers a type change AT the scope root itself
        // (e.g. a pushed file replaced by a directory of the same name, or
        // vice versa): one side navigates to a Blob/Symlink entry, the other
        // to a Tree entry. Requiring agreement between the two sides would
        // send that Tree side down the plain `navigate` (hash-only) path
        // below, and diffing a blob hash as if it were a tree hash fails
        // (`tree object hash mismatch`, since `diff_recursive`/`load_all_entries`
        // read every `base`/`target` hash as a tree object unconditionally).
        // Wrapping both sides in a one-entry tree here, as an ordinary leaf
        // would be, hands the type change to the same tree-diff type-change
        // rule ("Diff algorithm") that already applies at any other depth.
        let is_leaf = matches!(
            remote_scoped_entry,
            Some(TreeEntry::Blob { .. }) | Some(TreeEntry::Symlink { .. })
        ) || matches!(
            clone_scoped_entry,
            Some(TreeEntry::Blob { .. }) | Some(TreeEntry::Symlink { .. })
        );
        let remote_scoped_hash = if is_leaf {
            remote_scoped_entry
                .as_ref()
                .map(|entry| tree_ops::build_and_store(vec![entry.clone()], &local))
                .transpose()?
        } else {
            tree_ops::navigate(&remote_root, &components, &lazy)?
        };
        let clone_scoped_hash = if is_leaf {
            clone_scoped_entry
                .as_ref()
                .map(|entry| tree_ops::build_and_store(vec![entry.clone()], &local))
                .transpose()?
        } else if let Some(ref cr) = clone_root {
            tree_ops::navigate(cr, &components, &lazy)?
        } else {
            None
        };

        if remote_scoped_hash == clone_scoped_hash {
            // No remote change for this path; skip.
            continue;
        }

        let remote_diff = match &remote_scoped_hash {
            Some(rh) => {
                let mut d =
                    diff_trees(clone_scoped_hash.as_ref(), Some(rh), &lazy, stub_threshold)?;
                // If the diff is empty but the path is newly added, it is an empty directory.
                if d.is_empty() && clone_scoped_hash.is_none() {
                    d.insert(String::new(), DiffEntry::AddedEmptyDir);
                }
                d
            }
            None => {
                let mut result = HashMap::new();
                if let Some(ref ch) = clone_scoped_hash {
                    // Keys must be relative to the scoped subtree root (matching
                    // the `Some(rh)` branch's diff_trees keys), not repo-root-
                    // relative -- apply_diff below joins each key onto
                    // `scoped_abs` (already `rel`-rooted), so a `rel`-prefixed
                    // key here would double the prefix and silently miss every
                    // real file (refactor-instructions.md C6).
                    mark_deleted_tree(ch, &lazy, "", &mut result)?;
                }
                result
            }
        };

        if remote_diff.is_empty() {
            continue;
        }

        // Scan only <work_dir>/<rel>, not the whole working tree: a multi-path
        // pull must never read files outside the paths it was asked to pull
        // (refactor-instructions.md Phase 8 E7 scan-scope fix). Matches
        // pull_scoped's per-path scan below.
        let scoped_abs = repo.work_dir.join(rel);
        // `is_leaf` reflects the remote/clone-root sides' kinds, not what is
        // actually on disk here -- a leaf-scope type change (design/03,
        // "Leaf-scope type changes") may still have the OLD kind on disk
        // locally (e.g. the working tree still has a real directory at
        // `<path>` when the remote replaced it with a file, and this pull
        // has not been run yet). `scan_file_as_tree` would try to open that
        // directory as a file and fail with EISDIR, so branch on the actual
        // on-disk entity kind here, independent of `is_leaf`, and wrap either
        // result in the same one-entry synthetic tree so it compares against
        // `clone_scoped_hash`/`remote_scoped_hash` (both wrapped the same way
        // whenever `is_leaf` is true).
        // `symlink_metadata` (not `is_dir`, which follows a symlink target)
        // so a symlink pointing at a directory is still routed through
        // `scan_file_as_tree` below, which itself branches on
        // `file_type().is_symlink()` -- matching the non-type-changed leaf
        // path's existing symlink handling exactly.
        let scoped_is_dir = fs::symlink_metadata(&scoped_abs)
            .map(|md| md.is_dir())
            .unwrap_or(false);
        let working_scoped_hash = if scoped_abs.exists() {
            if is_leaf && !scoped_is_dir {
                // This path bypasses `scan_dir`, so record the leaf itself in
                // the observation here (a regular file with its `(mtime, size)`,
                // anything else as "seen, present"), or the apply-time check
                // would read a scanned leaf as "nothing was here".
                match fs::symlink_metadata(&scoped_abs) {
                    Ok(md) if md.is_file() => {
                        if let Ok(mtime) = md.modified() {
                            leaf_files.insert(rel.clone(), (mtime, md.len()));
                        }
                    }
                    Ok(_) => {
                        seen_other.insert(rel.clone());
                    }
                    Err(_) => {}
                }
                Some(scan_file_as_tree(
                    &scoped_abs,
                    components.last().unwrap(),
                    &local,
                )?)
            } else if is_leaf {
                // On-disk directory, but the remote/clone-root sides make this
                // a leaf scope: scan the directory as an ordinary subtree, then
                // wrap its root in a one-entry tree named after the leaf so it
                // compares against the wrapped remote/clone-root hashes.
                // `stub_store: &lazy` (not `local`): a partial-expansion
                // directory's stub record may name a clone-root tree object
                // not yet in the local cache after a lazy clone. `store:
                // &local` keeps this read-only scan's own `.exists()`-based
                // decisions local-only (see
                // `scan_and_store_with_stub_store`'s doc comment).
                let phase = crate::progress::begin_phase("Scan working tree");
                let scan_result = scan_and_store_with_stub_store(
                    &repo.work_dir,
                    &scoped_abs,
                    &local,
                    &lazy,
                    clone_root_flat.as_ref(),
                    &stat_cache,
                    false,
                )?;
                let file_count = scan_result.files.len();
                scanned_files.extend(scan_result.files);
                seen_other.extend(scan_result.side.seen_other);
                seen_stub.extend(scan_result.side.seen_stub);
                // The scan walked this directory's contents, so it never
                // recorded the directory itself. It is the path the pull will
                // replace with a file, and the apply-time re-check must know
                // the scan saw a directory here.
                seen_other.insert(rel.clone());
                phase.complete(format!("{} files", file_count));
                let (mtime, size, blob_count) =
                    tree_ops::tree_meta(&scan_result.root_hash, &local)?;
                let entry = TreeEntry::Tree {
                    name: components.last().unwrap().to_string(),
                    hash: scan_result.root_hash,
                    mtime,
                    size,
                    blob_count,
                };
                Some(tree_ops::build_and_store(vec![entry], &local)?)
            } else {
                // Same `stub_store`/`store` split as the branch above.
                let phase = crate::progress::begin_phase("Scan working tree");
                let scan_result = scan_and_store_with_stub_store(
                    &repo.work_dir,
                    &scoped_abs,
                    &local,
                    &lazy,
                    clone_root_flat.as_ref(),
                    &stat_cache,
                    false,
                )?;
                let file_count = scan_result.files.len();
                scanned_files.extend(scan_result.files);
                seen_other.extend(scan_result.side.seen_other);
                seen_stub.extend(scan_result.side.seen_stub);
                phase.complete(format!("{} files", file_count));
                Some(scan_result.root_hash)
            }
        } else if let Some(entry) = crate::commands::push::resolve_scoped_stub(&repo.work_dir, rel)
        {
            // A file stub for this leaf path (design/08_stub_system.md, "Path-
            // scoped pull's local-change check must resolve a stubbed leaf"):
            // the real file is absent (the `scoped_abs.exists()` check above
            // is false), but a stub is "content exists, not materialised" --
            // never a local deletion. Resolve to the stub's own recorded
            // entry, the same way `push_scoped`'s `resolve_scoped_stub`
            // already splices it in directly, instead of falling through to
            // `None` (which the rest of this diff reads as "locally deleted"
            // and would misreport as a conflict against any remote change to
            // this same path). `resolve_scoped_stub`'s directory-stub branch
            // requires `abs.is_dir()`, which is never true here (this `else`
            // arm is only reached when `scoped_abs.exists()` is false), so in
            // practice this only ever resolves a file stub -- wrapped the
            // same way an ordinary is_leaf entry is, so it compares against
            // the already-wrapped `clone_scoped_hash`/`remote_scoped_hash`.
            //
            // This path also bypasses `scan_dir`, so record the stub for the
            // apply-time re-check (the real file is expected to stay absent).
            seen_stub.insert(rel.clone());
            Some(tree_ops::build_and_store(vec![entry], &local)?)
        } else {
            None
        };
        let local_diff = diff_trees(
            clone_scoped_hash.as_ref(),
            working_scoped_hash.as_ref(),
            &lazy,
            0,
        )?;

        path_diffs.push(PathDiff {
            rel: rel.clone(),
            components: components.iter().map(|s| s.to_string()).collect(),
            remote_diff,
            local_diff,
            remote_scoped_hash,
            remote_scoped_entry,
            is_leaf,
            clone_scoped_hash,
        });
    }
    refresh_stat_cache(stat_cache, &scanned_files, &omemfs_dir, false);
    let mut observed = ScanObservation::with_stubs(scanned_files, seen_other, seen_stub);
    observed.files.extend(leaf_files);

    if path_diffs.is_empty() {
        plain_println("Already up to date.");
        return Ok(());
    }

    if dry_run {
        let mut out = Output::for_stdout();
        let colored = out.colored();
        let styles = out.styles;
        for pd in &path_diffs {
            let mut paths: Vec<&String> = pd.remote_diff.keys().collect();
            paths.sort();
            for path in paths {
                let change = &pd.remote_diff[path];
                out.writeln(&format!(
                    "  {}",
                    paint_diff_entry(colored, &styles, path, change.label())
                ))?;
            }
        }
        out.finish()?;
        return Ok(());
    }

    // Collect all diffs and conflict-check across all paths before applying anything.
    // pd_clean[i] accumulates clean entries for path_diffs[i] so apply_diff can
    // iterate each pd's entries directly without re-scanning the combined map.
    // conflict_owner[repo_rel_path] maps each conflict's repo-root-relative
    // path to (its pd index, its key within that pd's own scoped-relative
    // remote_diff/local_diff maps) -- the two differ whenever the pull is
    // scoped, so both are needed: the repo-relative path is what identifies
    // the conflict uniquely across all specified paths (see the loop below
    // that populates this map), while the scoped-relative key is what
    // `pd.remote_diff`/`pd.local_diff` are actually keyed by.
    let mut pd_clean: Vec<HashMap<String, DiffEntry>> = vec![HashMap::new(); path_diffs.len()];
    let mut all_conflicts: Vec<String> = Vec::new();
    let mut conflict_owner: HashMap<String, (usize, String)> = HashMap::new();
    // resolved_by_pd[i] holds the [line_merge] merges resolved for path_diffs[i]
    // (not yet written to disk -- see resolve_line_merges's doc comment).
    let mut resolved_by_pd: Vec<Vec<ResolvedLineMerge>> =
        (0..path_diffs.len()).map(|_| Vec::new()).collect();
    let filter_set = FilterSet::load(&repo.work_dir);

    for (i, pd) in path_diffs.iter().enumerate() {
        let (clean, mut conflicts) = classify_conflicts(&pd.remote_diff, &pd.local_diff);
        pd_clean[i].extend(clean);

        // [line_merge] auto-merge pass (design/15_line_history_merge.md,
        // "`pull` integration point"), scoped to this pd's subtree. `path`
        // (a key in pd.remote_diff/pd.local_diff) is relative to the scoped
        // subtree root (pd.rel), not the repo root, so the repo-root-relative
        // path for the FilterSet lookup is `pd.rel` joined with `path`.
        //
        // For a leaf-entry scope target (`omemfs pull <file>`) the diff maps
        // compare one-entry wrapper trees, so their keys are the leaf's own
        // file name; both the working-tree join base and the reporting prefix
        // must therefore be the leaf's PARENT directory -- exactly the same
        // adjustment the apply phase below makes for its `scoped_abs`.
        let (scoped_abs, rel_prefix) = if pd.is_leaf {
            let parent_rel = match pd.rel.rsplit_once('/') {
                Some((parent, _)) => parent.to_string(),
                None => String::new(),
            };
            (repo.work_dir.join(&parent_rel), parent_rel)
        } else {
            (repo.work_dir.join(&pd.rel), pd.rel.clone())
        };
        resolved_by_pd[i] = resolve_line_merges(
            &mut conflicts,
            &pd.remote_diff,
            &pd.local_diff,
            &filter_set,
            &scoped_abs,
            pd.clone_scoped_hash.as_ref(),
            &lazy,
            {
                let rel_prefix = rel_prefix.clone();
                move |p| {
                    if rel_prefix.is_empty() {
                        p.to_string()
                    } else if p.is_empty() {
                        rel_prefix.clone()
                    } else {
                        format!("{}/{}", rel_prefix, p)
                    }
                }
            },
        );

        // Conflict tracking across all specified paths keys on the
        // repo-root-relative path, never the scoped-subtree-relative key each
        // pd's own remote_diff/local_diff use internally -- otherwise a
        // same-named conflict under two different scoped paths (e.g. `pull a
        // b` where both a/f and b/f conflict) would collide on the bare key
        // "f" in conflict_owner (a HashMap keyed by that string) and in
        // all_conflicts (design/04_cli_spec.md, "Behaviour (path-scoped
        // pull)"; design/03_sync_model.md, "Conflict helper files"). Reuses
        // the same scoped-to-repo-relative join `resolve_line_merges` above
        // is given as `to_repo_rel`.
        for path in conflicts {
            let repo_rel = if rel_prefix.is_empty() {
                path.clone()
            } else if path.is_empty() {
                rel_prefix.clone()
            } else {
                format!("{}/{}", rel_prefix, path)
            };
            conflict_owner.insert(repo_rel.clone(), (i, path));
            all_conflicts.push(repo_rel);
        }
    }
    all_conflicts.sort();

    if !all_conflicts.is_empty() {
        // Pre-compute clone_scoped_hash once per pd that owns at least one conflict.
        let mut clone_scoped_by_pd: Vec<Option<Hash>> = vec![None; path_diffs.len()];
        for &(i, _) in conflict_owner.values() {
            if clone_scoped_by_pd[i].is_none() {
                clone_scoped_by_pd[i] = if path_diffs[i].is_leaf {
                    // Leaf scopes use a one-entry wrapper tree for their diff keys.
                    path_diffs[i].clone_scoped_hash.clone()
                } else if let Some(ref cr) = clone_root {
                    let comps: Vec<&str> = path_diffs[i]
                        .components
                        .iter()
                        .map(|s| s.as_str())
                        .collect();
                    tree_ops::navigate(cr, &comps, &lazy)?
                } else {
                    None
                };
            }
        }
        // Write conflict helpers for each conflicting path. `repo_rel` (the
        // key in `all_conflicts`/`conflict_owner`) is already the
        // repo-root-relative path, so it joins directly onto `work_dir` --
        // unlike the scoped-relative `scoped_key` that `pd.remote_diff` /
        // `pd.local_diff` are actually keyed by, which still needs looking up
        // via `conflict_owner`.
        for repo_rel in &all_conflicts {
            let (i, scoped_key) = &conflict_owner[repo_rel];
            let pd = &path_diffs[*i];
            let abs = repo.work_dir.join(repo_rel);
            let clone_scoped_hash = clone_scoped_by_pd[*i].as_ref();
            write_conflict_for_path(
                &repo.work_dir,
                &abs,
                scoped_key,
                clone_scoped_hash,
                &pd.remote_diff,
                &lazy,
                &local,
                &pack_reader,
                remote_key.as_ref(),
            )?;
        }
        print_conflict_result(&all_conflicts);
        return Err(Error::Conflict);
    }

    // Apply all clean changes.
    let mut total_changed = 0;
    let mut merged_paths: Vec<String> = Vec::new();
    {
        let phase = crate::progress::begin_phase("Apply changes");
        for (i, pd) in path_diffs.iter().enumerate() {
            let scoped_abs = if pd.is_leaf {
                repo.work_dir.join(&pd.rel).parent().unwrap().to_path_buf()
            } else {
                repo.work_dir.join(&pd.rel)
            };
            total_changed += apply_diff(
                &pd_clean[i],
                &scoped_abs,
                &local,
                &lazy,
                &repo.work_dir,
                stub_threshold,
                &pack_reader,
                remote_key.as_ref(),
                &observed,
            )?;
            // Every remaining conflict has already been ruled out above (the
            // abort check returned early otherwise), so it is now safe to
            // write this pd's resolved line-merges directly to the working
            // tree.
            merged_paths.extend(write_resolved_line_merges(
                &resolved_by_pd[i],
                &repo.work_dir,
                &scoped_abs,
            )?);
        }
        phase.complete(format!("{} paths", total_changed));
    }

    // Update clone_root by splicing all remote subtrees.
    let mut new_clone_root = clone_root.clone();
    for pd in &path_diffs {
        let comps: Vec<&str> = pd.components.iter().map(|s| s.as_str()).collect();
        // Splice reads intermediate clone_root tree objects along the path,
        // which may be absent locally after a lazy clone; route through lazy.
        // Newly-built trees are written to the local cache by `build_and_store`.
        new_clone_root = Some(if pd.is_leaf {
            splice_entry_into_clone_root(
                new_clone_root.as_ref(),
                &comps,
                pd.remote_scoped_entry.clone(),
                &lazy,
            )?
        } else {
            splice_into_clone_root(
                new_clone_root.as_ref(),
                &comps,
                pd.remote_scoped_hash.as_ref(),
                &lazy,
            )?
        });
    }
    if let Some(ref cr) = new_clone_root {
        repo.write_clone_root(cr)?;

        // Stub reconcile (design/08_stub_system.md, "Stub reconcile (pull
        // and restore)"): run only after clone_root has actually advanced
        // above -- never while a conflict is outstanding (unreached in that
        // case; see the `Err(Error::Conflict)` return earlier). Each pd is
        // reconciled at its own scope: a leaf path (`omemfs pull <file>`)
        // via `reconcile_leaf_stub` (its own path may itself be a stub, but
        // `stub::list_under` cannot walk a file), a directory scope via
        // `reconcile_stubs` bounded to that pd's own subtree -- never wider
        // than what this pull actually touched.
        for pd in &path_diffs {
            if pd.is_leaf {
                reconcile_leaf_stub(cr, &pd.rel, &repo.work_dir, &local)?;
            } else {
                reconcile_stubs(cr, &repo.work_dir.join(&pd.rel), &repo.work_dir, &local)?;
            }
        }
    }

    {
        let mut out = Output::for_stdout();
        let colored = out.colored();
        let styles = out.styles;
        // A single scoped path keeps its pre-merge header ("Pulling <rel> from
        // <remote> ..."); multiple paths keep the plain multi-path header. No
        // bats test pins either string, but preserving both avoids silently
        // changing a single-path pull's console output (refactor-instructions.md
        // Phase 8 E7 merge).
        match path_diffs.as_slice() {
            [only] => out.writeln(&format!("Pulling {} from {} ...", only.rel, remote_name))?,
            _ => out.writeln(&format!("Pulling from {} ...", remote_name))?,
        }
        for pd in &path_diffs {
            let mut paths: Vec<&String> = pd.remote_diff.keys().collect();
            paths.sort();
            for path in paths {
                let change = &pd.remote_diff[path];
                out.writeln(&format!(
                    "  {}",
                    paint_diff_entry(colored, &styles, path, change.label())
                ))?;
            }
        }
        out.writeln(&format!("{} path(s) updated.", total_changed))?;
        print_merged_block(&mut out, colored, &styles, &merged_paths)?;
        out.finish()?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

use crate::commands::conflict::HelperSource;

/// Resolve the base side from a tree rooted at `root_hash` by relative `path`.
/// Returns `None` if the path does not exist in the tree.
fn base_source_from_tree(
    root_hash: &Hash,
    path: &str,
    store: &dyn ObjectStore,
) -> Result<Option<HelperSource>, Error> {
    let components: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let Some(blob_hash) = tree_ops::navigate(root_hash, &components, store)? else {
        return Ok(None);
    };
    Ok(Some(HelperSource::Blob(blob_hash)))
}

/// Resolve the conflict-helper "base" side from a base tree root (the
/// clone_root for a full pull, or the scoped subtree's clone-root hash for a
/// scoped pull). `None` when there is no base root to read from.
///
/// Named generically (not `clone_root_source`/`clone_scoped_source`) because
/// full and scoped pull pass the same shape of argument here -- previously
/// two byte-identical functions (refactor-instructions.md E6).
fn base_root_source(
    base_root: Option<&Hash>,
    path: &str,
    store: &dyn ObjectStore,
) -> Result<Option<HelperSource>, Error> {
    match base_root {
        None => Ok(None),
        Some(h) => base_source_from_tree(h, path, store),
    }
}

/// Resolve the local side to an opened working-tree `File`. Returns `None` when
/// the path is absent, unreadable, or is a directory (or any non-regular-file),
/// preserving the previous `fs::read(..).ok()` semantics.
///
/// Holding the open `File` in the variant eliminates a TOCTOU gap: if the file
/// is replaced or deleted between open and write time the fd remains valid on
/// Linux, matching the old in-memory-bytes outcome. Opening a directory
/// succeeds on Linux but `file.metadata()?.is_file()` returns `false`, so
/// directories are filtered out here rather than failing later at `io::copy`.
fn working_file_source(abs_path: &std::path::Path) -> Option<HelperSource> {
    let file = fs::File::open(abs_path).ok()?;
    // Reject directories (File::open succeeds for dirs on Linux; io::copy would
    // fail with EISDIR). Also reject symlinks and other non-regular entries.
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    Some(HelperSource::WorkingFile(file))
}

/// Build the conflict metadata sidecar contents for one conflicting path from
/// the base tree entry (clone-root side) and the remote diff entry. Either side
/// may be `None` (the path did not exist there). The local side is omitted on
/// purpose: `accept-local` keeps the working-tree file unchanged, so its
/// metadata never needs restoring. See `conflict::ConflictMeta`.
fn build_conflict_meta(
    base_entry: Option<&TreeEntry>,
    remote_entry: Option<&DiffEntry>,
) -> crate::commands::conflict::ConflictMeta {
    use crate::commands::conflict::{ConflictMeta, RemoteResolution, SideMeta};
    let base = base_entry.and_then(|e| match e {
        TreeEntry::Blob { mtime, mode, .. } => Some(SideMeta {
            mtime: *mtime,
            mode: mode.clone(),
        }),
        _ => None,
    });
    let remote = remote_entry.and_then(|e| match e {
        DiffEntry::Added { mtime, mode, .. } | DiffEntry::Modified { mtime, mode, .. } => {
            Some(SideMeta {
                mtime: *mtime,
                mode: mode.clone(),
            })
        }
        _ => None,
    });
    // The exact value the remote held for this path when the conflict was
    // detected (design/03_sync_model.md, "Advancing clone_root on
    // resolution"), for `accept-*` to splice into `clone_root`. `None` here
    // (path absent from remote_diff) cannot happen for a real conflict --
    // `classify_conflicts` only ever produces a conflict for a path present
    // in `remote_diff`. A remote side that turned the path into a symlink is
    // a real, reachable shape (design/03_sync_model.md, "Remote side is a
    // symlink"), recorded explicitly rather than falling back to
    // `Unrecorded`, which is a defensive-only fallback for any other shape
    // (`AddedEmptyDir`/`AddedTree`, or no entry at all), never exercised by
    // pull's own conflict-detection path.
    let remote_resolution = Some(match remote_entry {
        Some(DiffEntry::Added {
            hash,
            size,
            mtime,
            mode,
        })
        | Some(DiffEntry::Modified {
            hash,
            size,
            mtime,
            mode,
        }) => RemoteResolution::Blob {
            hash: hash.clone(),
            size: *size,
            mtime: *mtime,
            mode: mode.clone(),
        },
        Some(DiffEntry::Deleted) => RemoteResolution::Deleted,
        Some(DiffEntry::Symlink { target, mtime, .. }) => RemoteResolution::Symlink {
            target: target.clone(),
            mtime: *mtime,
        },
        Some(DiffEntry::AddedEmptyDir) | Some(DiffEntry::AddedTree { .. }) | None => {
            RemoteResolution::Unrecorded
        }
    });
    ConflictMeta {
        // Filled in by `conflict::write_conflict_artifacts`, which knows
        // which helpers it actually wrote.
        sides: None,
        base,
        remote,
        remote_resolution,
    }
}

/// Resolve the remote side from a diff entry (Added or Modified).
/// Returns None for Deleted/dir/symlink entries (no content to write as a helper).
///
/// Pull performs no eager bulk download, so the blob is generally absent from
/// the local store; it is fetched from `remote` on demand here.
fn entry_source(
    entry: &DiffEntry,
    store: &dyn ObjectStore,
    remote: &dyn ObjectStore,
    remote_key: Option<&crate::codec::encrypt::EncryptKey>,
) -> Result<Option<HelperSource>, Error> {
    match entry {
        DiffEntry::Added { hash, .. } | DiffEntry::Modified { hash, .. } => {
            ensure_blob_local(remote, store, hash, remote_key)?;
            Ok(Some(HelperSource::Blob(hash.clone())))
        }
        DiffEntry::Deleted
        | DiffEntry::AddedEmptyDir
        | DiffEntry::AddedTree { .. }
        | DiffEntry::Symlink { .. } => Ok(None),
    }
}

/// Write the conflict-helper files and metadata sidecar for one conflicting
/// path. `base_hash` is the base-side tree root to resolve `path` against
/// (clone_root for a full pull, the scoped subtree's clone-side hash for a
/// scoped pull); `remote_diff` is the diff map to look `path` up in for the
/// remote side.
///
/// Consolidates a ~28-line loop body that was duplicated identically (module
/// which base root / remote-diff map each call site reads from) across
/// pull's two paths: pull_full and pull_scoped (which itself now handles
/// both single-path and multi-path scoped pulls, refactor-instructions.md
/// Phase 8 E7). Streams both sides through `lazy`: the
/// base (clone_root) blob may live in a stubbed subtree never downloaded
/// after a lazy clone, so it is fetched on demand here; the remote side was
/// already fetched into `local` by `entry_source`, which `lazy` serves from
/// its local hit.
#[allow(clippy::too_many_arguments)]
fn write_conflict_for_path(
    work_dir: &std::path::Path,
    abs: &std::path::Path,
    path: &str,
    base_hash: Option<&Hash>,
    remote_diff: &HashMap<String, DiffEntry>,
    lazy: &LazyTreeStore,
    local: &dyn ObjectStore,
    pack_reader: &PackReader,
    remote_key: Option<&crate::codec::encrypt::EncryptKey>,
) -> Result<(), Error> {
    let base_src = base_root_source(base_hash, path, lazy)?;
    let local_src = working_file_source(abs);
    let remote_src = remote_diff
        .get(path)
        .map(|e| entry_source(e, local, pack_reader, remote_key))
        .transpose()?
        .flatten();
    // Record the base/remote tracked metadata so `accept` can restore it.
    let base_entry = match base_hash {
        Some(h) => {
            let comps: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
            tree_ops::navigate_entry(h, &comps, lazy)?
        }
        None => None,
    };
    let meta = build_conflict_meta(base_entry.as_ref(), remote_diff.get(path));
    // Clearing the path's earlier artifacts, writing the helpers and writing
    // the sidecar (with the recorded sides) all happen in conflict.rs.
    crate::commands::conflict::write_conflict_artifacts(
        work_dir,
        abs,
        base_src.as_ref(),
        local_src.as_ref(),
        remote_src.as_ref(),
        meta,
        lazy,
    )
}

/// Partition `remote_diff` entries into:
/// - clean:     not in `local_diff`, OR in `local_diff` with the same content hash
/// - conflicts: paths present in both diffs with different content
///
/// When both sides changed a path to the same content, it is not a real conflict.
/// The remote entry (including its mtime/mode) is applied to the working tree,
/// so remote metadata wins for same-content resolutions.
fn classify_conflicts(
    remote_diff: &HashMap<String, DiffEntry>,
    local_diff: &HashMap<String, DiffEntry>,
) -> (HashMap<String, DiffEntry>, Vec<String>) {
    let mut clean: HashMap<String, DiffEntry> = HashMap::new();
    let mut conflicts: Vec<String> = Vec::new();

    for (path, remote_entry) in remote_diff {
        if let Some(local_entry) = local_diff.get(path) {
            if same_content(remote_entry, local_entry) {
                // Identical content on both sides: apply remote metadata.
                clean.insert(path.clone(), remote_entry.clone());
            } else {
                conflicts.push(path.clone());
            }
        } else if matches!(remote_entry, DiffEntry::AddedTree { .. })
            && local_diff
                .keys()
                .any(|local_path| is_local_change_under_added_tree(path, local_path))
        {
            // No content comparison is possible across a directory/file
            // granularity mismatch (remote `AddedTree("d")` vs. local
            // `Added("d/x")`), so any such overlap is a conflict.
            conflicts.push(path.clone());
        } else {
            clean.insert(path.clone(), remote_entry.clone());
        }
    }
    conflicts.sort();
    (clean, conflicts)
}

/// True when `local_path` lies strictly under the `AddedTree` at `tree_path`
/// and is not part of a `.git` directory. A `.git` change means the user
/// already has a local git worktree there, which pull materialises into
/// rather than stubs (design/08_stub_system.md, "Stubs and Git repositories").
fn is_local_change_under_added_tree(tree_path: &str, local_path: &str) -> bool {
    let Some(rest) = local_path
        .strip_prefix(tree_path)
        .and_then(|r| r.strip_prefix('/'))
    else {
        return false;
    };
    rest.split('/').next() != Some(".git")
}

/// Returns true when two diff entries represent the same object content.
fn same_content(a: &DiffEntry, b: &DiffEntry) -> bool {
    match (a, b) {
        (
            DiffEntry::Added { hash: ha, .. } | DiffEntry::Modified { hash: ha, .. },
            DiffEntry::Added { hash: hb, .. } | DiffEntry::Modified { hash: hb, .. },
        ) => ha == hb,
        (DiffEntry::Deleted, DiffEntry::Deleted) => true,
        (DiffEntry::AddedEmptyDir, DiffEntry::AddedEmptyDir) => true,
        (DiffEntry::Symlink { target: ta, .. }, DiffEntry::Symlink { target: tb, .. }) => ta == tb,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// [line_merge] auto-merge (design/15_line_history_merge.md, "`pull`
// integration point")
// ---------------------------------------------------------------------------

/// True for a diff entry shape that carries an actual blob (`Added` or
/// `Modified`) -- the only shape a line-merge attempt is considered for.
fn is_blob_change(entry: &DiffEntry) -> bool {
    matches!(entry, DiffEntry::Added { .. } | DiffEntry::Modified { .. })
}

/// Read the full content of blob `hash` into memory via `store`, stripping
/// the `ED F0` blob-escape (handled internally by `for_each_blob_chunk`, see
/// `object::deserialise_blob`). Used only for line-merge inputs, which are
/// expected to be small, line-based log files -- not a general-purpose blob
/// reader for arbitrary-size content.
fn read_blob_bytes(store: &dyn ObjectStore, hash: &Hash) -> Result<Vec<u8>, Error> {
    let mut buf = Vec::new();
    codec::chunk::for_each_blob_chunk(store, hash, None, |chunk| {
        buf.extend_from_slice(chunk);
        Ok(())
    })?;
    Ok(buf)
}

/// Attempt a three-way line merge for one path already known to have a
/// blob-bearing (`Added`/`Modified`) entry on both the remote and local
/// side. Returns `Some(merged_bytes)` on a clean merge; `None` if the merge
/// should not be (or could not be) attempted -- including a genuine
/// `MergeOutcome::Conflict`, a non-regular-file working-tree entry, or any
/// I/O error while resolving base/local/remote bytes. In every `None` case
/// the caller leaves the path in `conflicts` untouched: a line-merge attempt
/// must never turn an otherwise-successful pull into a hard failure.
fn try_line_merge_one(
    path: &str,
    remote_entry: &DiffEntry,
    working_path: &std::path::Path,
    base_root: Option<&Hash>,
    lazy: &LazyTreeStore,
) -> Option<Vec<u8>> {
    // Local side: only a regular file is eligible (reject missing paths,
    // directories, symlinks -- same is-a-regular-file check style as
    // `working_file_source`).
    let meta = fs::symlink_metadata(working_path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let local_bytes = fs::read(working_path).ok()?;

    let remote_hash = match remote_entry {
        DiffEntry::Added { hash, .. } | DiffEntry::Modified { hash, .. } => hash,
        _ => return None,
    };
    let remote_bytes = read_blob_bytes(lazy, remote_hash).ok()?;

    // Base side: the blob at `base_root` (clone_root, or the scoped
    // subtree's clone-side hash), or empty bytes if the path had no base
    // entry (e.g. it was added on both sides independently).
    let base_bytes = match base_root {
        None => Vec::new(),
        Some(root) => {
            let components: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
            match tree_ops::navigate(root, &components, lazy) {
                Ok(Some(base_hash)) => read_blob_bytes(lazy, &base_hash).ok()?,
                Ok(None) => Vec::new(),
                Err(_) => return None,
            }
        }
    };

    match line_merge::merge(&base_bytes, &local_bytes, &remote_bytes) {
        MergeOutcome::Clean(bytes) => Some(bytes),
        MergeOutcome::Conflict => None,
    }
}

/// One path resolved to a clean line-merge, pending being written to the
/// working tree. See `resolve_line_merges`.
struct ResolvedLineMerge {
    /// The diff-map key: relative to the repo root for a full pull, or
    /// relative to the scoped subtree root for a scoped pull.
    key: String,
    /// The full repo-root-relative path, for the "Line-merged" report block.
    repo_rel: String,
    /// The merged file content.
    bytes: Vec<u8>,
}

/// Resolve (but do NOT write to disk) a `[line_merge]` auto-merge for every
/// path still in `conflicts` after `classify_conflicts`
/// (design/15_line_history_merge.md, "`pull` integration point"). Removes
/// each successfully-merged path from `conflicts`; every path left in
/// `conflicts` afterwards proceeds through the existing conflict-helper flow
/// exactly as before.
///
/// Deliberately does not touch the filesystem: pull's atomic-abort policy
/// (design/03, design/04) requires that nothing is applied to the working
/// tree while ANY conflict remains, including a path resolved here -- if some
/// other, unrelated path still conflicts, the whole pull aborts and this
/// path's merge must not have been written either. The caller must call
/// `write_resolved_line_merges` only after confirming `conflicts` is empty.
///
/// `base_dir` is the directory a diff-map key is joined onto to reach the
/// working-tree file (`repo.work_dir` for a full pull, `work_dir/pd.rel` for
/// a scoped pull). `base_root` is the base tree root to resolve a path's
/// base blob against (`clone_root`, or the scoped subtree's clone-side
/// hash). `to_repo_rel` converts a diff-map key to a full repo-root-relative
/// path for the `FilterSet::is_line_merge` lookup and for reporting.
fn resolve_line_merges(
    conflicts: &mut Vec<String>,
    remote_diff: &HashMap<String, DiffEntry>,
    local_diff: &HashMap<String, DiffEntry>,
    filter_set: &FilterSet,
    base_dir: &std::path::Path,
    base_root: Option<&Hash>,
    lazy: &LazyTreeStore,
    to_repo_rel: impl Fn(&str) -> String,
) -> Vec<ResolvedLineMerge> {
    let mut resolved = Vec::new();
    let mut remaining = Vec::with_capacity(conflicts.len());

    for path in conflicts.drain(..) {
        let candidate = match (remote_diff.get(&path), local_diff.get(&path)) {
            (Some(r), Some(l)) => is_blob_change(r) && is_blob_change(l),
            _ => false,
        };
        if !candidate {
            remaining.push(path);
            continue;
        }

        let repo_rel = to_repo_rel(&path);
        if !filter_set.is_line_merge(&repo_rel) {
            remaining.push(path);
            continue;
        }

        let remote_entry = remote_diff.get(&path).expect("checked above");
        let abs = base_dir.join(&path);
        match try_line_merge_one(&path, remote_entry, &abs, base_root, lazy) {
            Some(bytes) => resolved.push(ResolvedLineMerge {
                key: path,
                repo_rel,
                bytes,
            }),
            None => remaining.push(path),
        }
    }

    *conflicts = remaining;
    resolved
}

/// Write every resolved line-merge's content directly to the working tree
/// (atomically), bypassing `apply_diff`/`clean_diff` entirely for these
/// paths. Only call once the pull has passed the conflict-abort check (i.e.
/// `conflicts` is empty) -- see `resolve_line_merges`. Returns the
/// repo-root-relative paths written, for the "Line-merged" report block.
///
/// `work_dir` must be the true repository root -- for a scoped pull, `base_dir`
/// can itself be several components below the repository root, and those
/// intermediate components need checking too; passing `base_dir` as the
/// fsguard boundary here would wrongly exempt them (design/03_sync_model.md,
/// "Write safety: the working-tree write boundary").
fn write_resolved_line_merges(
    resolved: &[ResolvedLineMerge],
    work_dir: &std::path::Path,
    base_dir: &std::path::Path,
) -> Result<Vec<String>, Error> {
    let mut merged_paths = Vec::with_capacity(resolved.len());
    for r in resolved {
        let abs = base_dir.join(&r.key);
        crate::fsguard::atomic_write_with(work_dir, &abs, |w| {
            w.write_all(&r.bytes).map_err(Error::Io)
        })?;
        merged_paths.push(r.repo_rel.clone());
    }
    Ok(merged_paths)
}

/// Print the "Line-merged the following paths:" block for `merged`, matching
/// the style of the "preserved" block printed a few lines below it. Does
/// nothing when `merged` is empty.
fn print_merged_block(
    out: &mut Output,
    colored: bool,
    styles: &Styles,
    merged: &[String],
) -> Result<(), Error> {
    if merged.is_empty() {
        return Ok(());
    }
    out.writeln("Line-merged the following paths:")?;
    for p in merged {
        let p_str = paint(colored, styles.modified, &crate::term::escape_display(&p.to_string()));
        out.writeln(&format!("  merged: {}", p_str))?;
    }
    Ok(())
}

/// What the pre-apply working-tree scan saw: each regular file's `(mtime, size)`
/// keyed by repo-relative path. `apply_diff` compares a path against this
/// immediately before overwriting or deleting it (design/03, "Re-verifying a
/// path immediately before it is overwritten"). A path missing from the map was
/// absent, stubbed or ignored when the scan ran.
struct ScanObservation {
    files: HashMap<String, (std::time::SystemTime, u64)>,
    /// Paths the scan saw that are not regular files (symlinks, directories,
    /// stub markers): present, but with no `(mtime, size)` to compare.
    other: std::collections::HashSet<String>,
    /// Paths this pull removed itself while applying the diff (deleted files and
    /// pruned directories). Their absence afterwards is the pull's own doing,
    /// not a user edit (design/03). Interior-mutable: `apply_diff` holds the
    /// observation by shared reference.
    removed: std::cell::RefCell<std::collections::HashSet<String>>,
    /// Logical paths that were file stubs with no real file at scan time; the
    /// real file is expected to stay absent (design/03).
    stubs: std::collections::HashSet<String>,
}

impl ScanObservation {
    fn record_removed(&self, rel: &str) {
        self.removed.borrow_mut().insert(rel.to_string());
    }

    /// Regular files only; no non-file paths were seen. Used where the scan
    /// result's side data is not to hand (tests, and scopes that scan nothing).
    fn new(files: HashMap<String, crate::scan::ScannedFile>) -> Self {
        Self::with_other(files, std::iter::empty())
    }

    fn with_other(
        files: HashMap<String, crate::scan::ScannedFile>,
        other: impl IntoIterator<Item = String>,
    ) -> Self {
        Self::with_stubs(files, other, std::iter::empty())
    }

    fn with_stubs(
        files: HashMap<String, crate::scan::ScannedFile>,
        other: impl IntoIterator<Item = String>,
        stubs: impl IntoIterator<Item = String>,
    ) -> Self {
        ScanObservation {
            files: files
                .into_iter()
                .map(|(path, f)| (path, (f.fs_mtime, f.fs_size)))
                .collect(),
            other: other.into_iter().collect(),
            removed: std::cell::RefCell::new(std::collections::HashSet::new()),
            stubs: stubs.into_iter().collect(),
        }
    }

    #[cfg(test)]
    fn files_for_test(&self) -> HashMap<String, crate::scan::ScannedFile> {
        self.files
            .iter()
            .map(|(p, (mtime, size))| {
                (
                    p.clone(),
                    crate::scan::ScannedFile {
                        fs_mtime: *mtime,
                        fs_size: *size,
                        hash: crate::object::blob_hash(b""),
                        cache_hit: false,
                        fallback_hit: false,
                        blob_sealed: false,
                        observed_at: std::time::SystemTime::now(),
                    },
                )
            })
            .collect()
    }
}

/// True when `abs` no longer matches what the scan observed at `rel`.
///
/// - Observed as a regular file: it must still be a regular file with the same
///   `(mtime, size)`.
/// - Observed as something else (symlink, directory, stub marker): it must
///   still exist. Its old kind may legitimately differ from the incoming one
///   (a type change), so only presence is checked, and a symlink or directory
///   that has been replaced by a regular file is a change.
/// - Not observed: nothing may exist there now. (`symlink_metadata`, so a
///   dangling symlink or a directory still counts as something appearing.)
fn changed_since_scan(obs: &ScanObservation, rel: &str, abs: &std::path::Path) -> bool {
    // A path this pull removed itself is expected to be gone; only something
    // that has appeared there since is a change.
    if obs.removed.borrow().contains(rel) {
        return fs::symlink_metadata(abs).is_ok();
    }
    if let Some((mtime, size)) = obs.files.get(rel) {
        return match fs::symlink_metadata(abs) {
            Ok(md) => !md.is_file() || md.len() != *size || md.modified().ok() != Some(*mtime),
            Err(_) => true,
        };
    }
    if obs.other.contains(rel) {
        return match fs::symlink_metadata(abs) {
            Ok(md) => md.is_file(),
            Err(_) => true,
        };
    }
    // A file stub stands for a real file that is absent by design: the pull
    // updates or removes the stub record and never materialises it, so the real
    // file must simply still be absent.
    if obs.stubs.contains(rel) {
        return fs::symlink_metadata(abs).is_ok();
    }
    fs::symlink_metadata(abs).is_ok()
}

/// The entries of `diff` whose working-tree path would lose data if written now
/// (overwritten, deleted or replaced) but has changed since the scan. Returns
/// repo-relative paths, sorted. Entries that cannot destroy a file the scan
/// could have observed (`AddedEmptyDir`, `AddedTree`, `Symlink` over an absent
/// path) are not regular-file writes and are checked only for "still absent".
fn verify_unchanged_since_scan(
    diff: &HashMap<String, DiffEntry>,
    base_dir: &std::path::Path,
    work_dir: &std::path::Path,
    obs: &ScanObservation,
) -> Vec<String> {
    let mut offenders: Vec<String> = Vec::new();
    for (key, change) in diff {
        let abs = base_dir.join(key);
        let Ok(rel_path) = abs.strip_prefix(work_dir) else {
            continue;
        };
        let rel = rel_path.to_string_lossy().into_owned();
        let touches_a_file = matches!(
            change,
            DiffEntry::Added { .. }
                | DiffEntry::Modified { .. }
                | DiffEntry::Deleted
                | DiffEntry::Symlink { .. }
        );
        if touches_a_file && changed_since_scan(obs, &rel, &abs) {
            // A deletion of a path that was absent at scan time and is still
            // absent is a no-op, not a change: `changed_since_scan` already
            // reports "absent, still absent" as unchanged.
            offenders.push(rel);
        }
    }
    offenders.sort();
    offenders
}

/// Deterministic integration-test hook for the scan-to-apply window (design/03,
/// "Re-verifying a path immediately before it is overwritten"). Compiled only in
/// debug builds and inert unless the variable is set, like the scan hook
/// `OMEMFS_TEST_SCAN_AFTER_LIST_DELAY_MS`.
fn test_hook_before_apply() {
    #[cfg(debug_assertions)]
    if let Ok(ms) = std::env::var("OMEMFS_TEST_PULL_BEFORE_APPLY_DELAY_MS")
        && let Ok(ms) = ms.parse::<u64>()
    {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

/// Report the paths that changed underneath the pull and return the error that
/// aborts it. `Error::Conflict`, because that is what it is: the working tree
/// now holds a local change to a path the remote also changed. Nothing is
/// written for the paths: `clone_root` stays where it was, so the next
/// `omemfs pull` re-scans and reports the edit as an ordinary conflict (with
/// helper files), which is the resolution path users already know.
fn changed_since_scan_error(offenders: &[String]) -> Error {
    eprintln!("error: these paths changed while the pull was in progress; nothing more was written:");
    for p in offenders {
        eprintln!("  {}", crate::term::escape_display(p));
    }
    eprintln!("Run 'omemfs pull' again to review them as conflicts.");
    Error::Conflict
}

/// Re-check one path right before its own write (design/03). The up-front pass
/// covers the whole diff at once; this narrows the remaining window to the
/// distance between this check and the write itself.
fn verify_one_before_write(
    obs: &ScanObservation,
    work_dir: &std::path::Path,
    abs_path: &std::path::Path,
) -> Result<(), Error> {
    let rel = abs_path
        .strip_prefix(work_dir)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !rel.is_empty() && changed_since_scan(obs, &rel, abs_path) {
        return Err(changed_since_scan_error(&[rel]));
    }
    Ok(())
}

fn apply_diff(
    diff: &HashMap<String, DiffEntry>,
    base_dir: &std::path::Path,
    local: &LocalStore,
    store: &dyn ObjectStore,
    work_dir: &std::path::Path,
    stub_threshold: u64,
    remote: &dyn ObjectStore,
    remote_key: Option<&crate::codec::encrypt::EncryptKey>,
    observed: &ScanObservation,
) -> Result<usize, Error> {
    let mut changed = 0;

    // Phase 1 (Improvement B — design/02_storage_format.md, "Multi-root
    // batching"; design/04_cli_spec.md pull step 4): collect every blob hash
    // this diff will need from the remote and fetch the whole batch in one
    // multi-root transfer, before any file is written. Fetching them one at a
    // time gave the worker pool nothing to divide — a leaf blob's own BFS is a
    // single childless node — so a diff of many small files ran effectively
    // serially no matter how `OMEMFS_TRANSFER_CONCURRENCY` was set.
    //
    // Only `Added`/`Modified` entries that will actually be materialised are
    // planned, and only when their branch condition is decidable without
    // observing this run's own writes:
    //
    // - `Added`: an entry at or above `stub_threshold` may be stub-written
    //   instead, but that depends on `stub::stub_would_be_visible_to_git` (a
    //   `git check-ignore` subprocess). It is left unplanned so the check keeps
    //   running exactly once, at exactly the point it runs today.
    // - `Modified`: the stub-update branch is a pure filesystem check on this
    //   path alone, so it is safe to evaluate up front.
    // - `AddedTree` falls back to `materialise_tree`, which walks tree objects
    //   to discover children; its blobs are not part of this batch (design/04
    //   step 4: only leaf-blob fetches are batched).
    //
    // Anything not planned keeps its existing behaviour: `ensure_blob_local`
    // below still fetches on demand when a blob is missing from the cache.
    {
        let mut pending: Vec<Hash> = Vec::new();
        for (rel_path, change) in diff {
            let abs_path = base_dir.join(rel_path);
            match change {
                DiffEntry::Added { hash, size, .. } => {
                    let may_be_stubbed = stub_threshold > 0 && *size >= stub_threshold;
                    if !may_be_stubbed && !local.exists(hash)? {
                        pending.push(hash.clone());
                    }
                }
                DiffEntry::Modified { hash, .. } => {
                    let work_rel = abs_path
                        .strip_prefix(work_dir)
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_else(|_| rel_path.to_string());
                    let updates_stub_only = stub::exists(work_dir, &work_rel) && !abs_path.exists();
                    if !updates_stub_only && !local.exists(hash)? {
                        pending.push(hash.clone());
                    }
                }
                DiffEntry::AddedTree { hash, .. } => {
                    let dir_stub_path = abs_path.join(stub::DIR_STUB_NAME);
                    let visible_to_git = abs_path.join(".git").exists()
                        || stub::stub_would_be_visible_to_git(&dir_stub_path, work_dir);
                    if visible_to_git && !local.exists(hash)? {
                        // A materialised directory needs its whole reachable
                        // graph. Supplying the tree root lets the shared BFS
                        // fetch child trees, manifests, and chunks in parallel.
                        pending.push(hash.clone());
                    }
                }
                DiffEntry::Deleted | DiffEntry::AddedEmptyDir | DiffEntry::Symlink { .. } => {}
            }
        }
        crate::commands::transfer::transfer_objects_many(
            remote, local, &pending, remote_key, true,
        )?;
    }

    test_hook_before_apply();

    // Re-verify every path against the scan, in one pass, after the (possibly
    // minutes-long) download and before the FIRST write (design/03, "Re-verifying
    // a path immediately before it is overwritten"): a path edited in the
    // scan-to-apply window aborts the pull with nothing applied yet.
    let offenders = verify_unchanged_since_scan(diff, base_dir, work_dir, observed);
    if !offenders.is_empty() {
        return Err(changed_since_scan_error(&offenders));
    }

    // Directories touched by deletions, for dir-stub / empty-parent cleanup.
    let mut deleted_dirs: std::collections::HashSet<std::path::PathBuf> =
        std::collections::HashSet::new();

    // Phase 2a: apply every `Deleted` entry FIRST, before any `Added`/
    // `Modified`/`Symlink`/`AddedTree`/`AddedEmptyDir` entry is written.
    //
    // A remote-side type change (design/03_sync_model.md "Diff algorithm")
    // reports the old side's full content as `Deleted` alongside an `Added`
    // (or `Symlink`) for the new content at the SAME path -- e.g. a
    // directory replaced by a file emits `deleted: dir/child` for every old
    // child plus `added: dir` for the new blob. `diff` is a `HashMap`, so its
    // iteration order is arbitrary: writing the new blob before the old
    // directory's children are removed would try to `rename()` a temp file
    // onto a non-empty (or even empty) directory, which the kernel refuses
    // with EISDIR -- the "Is a directory" pull failure this fixes. Applying
    // every deletion first, then letting the directory-cleanup pass below
    // prune the now-empty directory, then applying creations, ensures a
    // type-changed path is always genuinely absent by the time its new
    // content is materialised.
    for (rel_path, change) in diff {
        if !matches!(change, DiffEntry::Deleted) {
            continue;
        }
        let abs_path = base_dir.join(rel_path);
        let work_rel = abs_path
            .strip_prefix(work_dir)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| rel_path.to_string());
        // design/08_stub_system.md, "Interaction with pull's ordinary
        // diff/apply" (the file-stub mirror-image paragraph): a `Deleted`
        // entry at a path that is CURRENTLY a file stub is not necessarily a
        // genuine deletion -- design/03_sync_model.md's diff algorithm also
        // emits `Deleted(path)` as the old-side half of a blob-to-tree type
        // change (a file stub whose remote clone_root entry became a small
        // tree, diffed recursively rather than as a single AddedTree). If
        // apply removed the file stub marker here unconditionally, the
        // subsequent Added entries for that new tree's children (processed
        // in Phase 2b below) would materialise as ordinary real files under
        // this path, silently bypassing reconcile's "tree | file stub ->
        // SwitchToDirStub" row -- by the time reconcile_stubs runs, the one
        // signal it needs (an existing file stub record) would already be
        // gone. Leave the stub marker in place and skip the rest of this
        // entry's handling; reconcile alone decides the outcome once the
        // new clone_root is current (it also correctly handles the genuine
        // "file stub deleted outright on the remote" case via its own
        // `absent -> Remove` row, so nothing is lost by deferring here).
        if stub::exists(work_dir, &work_rel) {
            continue;
        }
        // N1 (design/03_sync_model.md, "Write safety: the working-
        // tree write boundary"): this delete is driven by the remote
        // diff, not a fresh on-disk probe, but it is still a lexical
        // write-tree path that must not follow a symlinked parent.
        let existed = abs_path.exists();
        verify_one_before_write(observed, work_dir, &abs_path)?;
        crate::fsguard::remove_file_tolerant(work_dir, &abs_path)?;
        // The path is gone because of this pull, not a user edit (design/03).
        observed.record_removed(&work_rel);
        if existed {
            changed += 1;
        }
        // Remove stub if it existed.
        stub::remove(work_dir, &work_rel)?;
        if let Some(parent) = abs_path.parent() {
            deleted_dirs.insert(parent.to_path_buf());
        }
    }

    // Prune now-empty directories left behind by deletions. This never
    // touches a directory stub marker itself: whether a directory stub
    // should be removed entirely is decided by `reconcile_stubs` (run after
    // this function returns and `clone_root` is written), by checking
    // whether the new `clone_root` still has ANY entry at that path --
    // design/08_stub_system.md, "Interaction with pull's ordinary
    // diff/apply". A naive on-disk check here ("only the marker remains in
    // this directory") cannot tell a whole-subtree deletion apart from a
    // partial one (a directory stub with SOME, not all, of its logical
    // content deleted on the remote looks identical on disk -- only the
    // marker is ever materialised either way), so it must not decide this;
    // deferring to reconcile_stubs, which does consult the new clone_root,
    // is the only correct place for this decision.
    //
    // Walk each deleted directory and all of its ancestors up to (but not
    // including) work_dir, pruning any that are now genuinely empty (no
    // marker, no files). A directory that still holds a `.omemfs-stub`
    // marker is never "empty" by this check, so this loop is a no-op for any
    // directory stub -- exactly the deferral described above.
    let mut cleanup: std::collections::HashSet<std::path::PathBuf> =
        std::collections::HashSet::new();
    for dir in &deleted_dirs {
        let mut cur = dir.clone();
        while cur.starts_with(work_dir) && cur != work_dir {
            cleanup.insert(cur.clone());
            match cur.parent() {
                Some(p) => cur = p.to_path_buf(),
                None => break,
            }
        }
    }
    // Process deepest paths first so empty children are removed before parents.
    let mut cleanup_sorted: Vec<std::path::PathBuf> = cleanup.into_iter().collect();
    cleanup_sorted.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for dir in cleanup_sorted {
        if fs::read_dir(&dir)
            .map(|mut rd| rd.next().is_none())
            .unwrap_or(false)
        {
            if fs::remove_dir(&dir).is_ok()
                && let Ok(rel) = dir.strip_prefix(work_dir)
            {
                observed.record_removed(&rel.to_string_lossy());
            }
        }
    }

    // Phase 2b: apply every remaining (non-`Deleted`) entry, now that every
    // `Deleted` path -- including a type change's old-side unwind -- is
    // genuinely gone from the working tree and any now-empty directory left
    // behind has been pruned above.
    for (rel_path, change) in diff {
        let abs_path = base_dir.join(rel_path);
        // Compute the path relative to work_dir (correct even for scoped pulls where
        // base_dir != work_dir, so stub operations target the right location).
        let work_rel = abs_path
            .strip_prefix(work_dir)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| rel_path.to_string());
        // design/08_stub_system.md, "Interaction with pull's ordinary
        // diff/apply": a remote change to a file inside a locally
        // directory-stubbed path still shows up here as an ordinary
        // per-entry Added/Modified/Deleted DiffEntry (the diff itself has no
        // notion of local dir-stub state), but writing it would materialise
        // a real file underneath a directory that is supposed to be a
        // whole-unit stub. Skip any entry whose path has a dir-stubbed
        // proper ancestor -- reconcile_stubs (run after this function
        // returns and clone_root is written) is what brings that one
        // directory stub's own record up to date instead. Deletion of such
        // an entry was already skipped the same way in Phase 2a above.
        //
        // This also covers the path itself being a directory stub whose OWN
        // clone_root entry changed kind (tree -> blob / tree -> symlink):
        // apply's ordinary Added/Symlink handling assumes it is writing into
        // an absent-or-plain-file path, and correctly replacing a whole
        // directory-stub directory is exactly reconcile's
        // SwitchToFileStub/MaterialiseSymlink job (design/08, "Interaction
        // with pull's ordinary diff/apply" -- the paragraph on the stubbed
        // path's own entry changing kind). `Modified` never needs this: a
        // dir stub's clone_root entry is a Tree, and `Modified` only ever
        // targets a Blob-to-Blob change.
        //
        // Mirror-image case (design/08, the file-stub paragraph): a file
        // stub whose remote clone_root entry became a small tree (diffed
        // recursively, so its children arrive as ordinary Added entries
        // rather than a single AddedTree) must be left entirely to
        // reconcile too. Phase 2a above already left this path's file stub
        // marker in place instead of removing it on the corresponding
        // Deleted(work_rel) entry; here, a child entry like
        // Added(f.txt/inner.txt) has `work_rel` strictly NESTED under the
        // still-present file stub at "f.txt", which `file_stub_at_or_above`
        // finds. Only the "nested under" shape needs this guard -- unlike
        // the dir-stub case above, an entry AT the exact file-stub path
        // itself never needs deferring here: `Modified` there is the
        // ordinary on-stub update (must run, handled inside its own match
        // arm below), `Symlink` there is ALWAYS materialised directly per
        // design/08 regardless of the previous stub kind (no directory to
        // clear first, so this is safe), and `AddedTree` there is already
        // fully self-contained (its own `stub::remove` above). Excluding an
        // exact match (`found_rel == work_rel`) is what keeps this from
        // regressing those.
        if stub::deepest_ancestor_stub(work_dir, &work_rel)?.is_some()
            || stub::dir_exists(work_dir, &work_rel)
            || stub::file_stub_at_or_above(work_dir, &work_rel)?
                .is_some_and(|(found_rel, _)| found_rel != work_rel)
        {
            continue;
        }
        match change {
            DiffEntry::Added {
                hash,
                size,
                mtime,
                mode,
            } => {
                // Threshold-based auto-stubbing applies to Added entries only
                // (design/04 pull --stub-threshold: "Added entries only").
                // The Git rule: do not place a stub whose file would be visible
                // to Git (design/08); visibility is checked via git check-ignore.
                let stub_path = stub::file_stub_path_for(&abs_path);
                let should_stub = stub_threshold > 0
                    && *size >= stub_threshold
                    && !stub::stub_would_be_visible_to_git(&stub_path, work_dir);
                if should_stub {
                    // Write stub alongside the file, do not download content.
                    stub::write(
                        work_dir,
                        &work_rel,
                        &StubRecord {
                            target_type: crate::stub::StubTargetType::Blob,
                            hash: hash.clone(),
                            size: *size,
                            mtime: *mtime,
                            mode: mode.clone(),
                            blob_count: 0,
                        },
                    )?;
                    // Remove any existing real file at abs_path.
                    if abs_path.exists() {
                        crate::fsguard::remove_file_tolerant(work_dir, &abs_path)?;
                    }
                } else {
                    materialise_blob(
                        &abs_path, work_dir, &work_rel, hash, mtime, mode, store, remote,
                        remote_key, observed,
                    )?;
                }
                changed += 1;
            }
            DiffEntry::Modified {
                hash,
                size,
                mtime,
                mode,
            } => {
                // Modified-on-stub: if the path is currently a stub locally,
                // update the stub record without downloading the content
                // (design/08 "Interaction with pull"). Otherwise materialise.
                if stub::exists(work_dir, &work_rel) && !abs_path.exists() {
                    stub::write(
                        work_dir,
                        &work_rel,
                        &StubRecord {
                            target_type: crate::stub::StubTargetType::Blob,
                            hash: hash.clone(),
                            size: *size,
                            mtime: *mtime,
                            mode: mode.clone(),
                            blob_count: 0,
                        },
                    )?;
                } else {
                    materialise_blob(
                        &abs_path, work_dir, &work_rel, hash, mtime, mode, store, remote,
                        remote_key, observed,
                    )?;
                }
                changed += 1;
            }
            DiffEntry::AddedEmptyDir => {
                crate::fsguard::create_dir_all(work_dir, &abs_path)?;
                changed += 1;
            }
            DiffEntry::Symlink { target, mtime, .. } => {
                // A symlink is always materialised (design/08). Remove any stale
                // stub and any existing entry (file, symlink, or empty dir) at the
                // path, then create the symlink. The existing entry is removed
                // without following it.
                stub::remove(work_dir, &work_rel)?;
                // write_symlink_atomic (via fsguard) ensures the parent
                // directory exists.
                verify_one_before_write(observed, work_dir, &abs_path)?;
                crate::fsmeta::write_symlink_atomic(work_dir, &abs_path, target)?;
                // Restore the link's own mtime (lutimes) so a subsequent scan
                // recomputes the same tree and push stays a no-op.
                crate::fsmeta::restore_symlink_mtime(&abs_path, mtime);
                changed += 1;
            }
            DiffEntry::Deleted => {
                // Already applied in Phase 2a, above.
            }
            DiffEntry::AddedTree {
                hash,
                size,
                mtime,
                blob_count,
            } => {
                // Type-change self-containment (design/03_sync_model.md, "Diff
                // algorithm"): when this AddedTree replaces a blob or symlink
                // (never a tree -- diff_recursive only emits AddedTree for a
                // target-side tree), diff_recursive's Tree branch also inserts
                // Deleted(path) for that old entry, but at the very same
                // HashMap key this AddedTree occupies -- the second insert
                // silently discards the first, so `diff` carries no separate
                // trace of the old entry by the time apply_diff sees it. Clear
                // whatever is actually on disk here unconditionally, the same
                // way the Symlink branch above is self-contained about
                // removing a stale stub/entry, instead of relying on a
                // `Deleted` entry that was never going to survive. A stale
                // file stub is removed first (the old side may have been
                // stubbed), then anything materialised at `abs_path` -- a
                // regular file or a symlink, never a directory here since the
                // old side was never a tree -- is removed too, so create_dir_all/
                // materialise_tree below are never handed a non-directory in
                // their way (the "refusing to write through a symlinked
                // directory" failure this fixes).
                stub::remove(work_dir, &work_rel)?;
                if fs::symlink_metadata(&abs_path)
                    .map(|md| !md.is_dir())
                    .unwrap_or(false)
                {
                    crate::fsguard::remove_file_tolerant(work_dir, &abs_path)?;
                }
                // The entire subtree is stored as a single directory stub —
                // unless the directory stub file (`<dir>/.omemfs-stub`) would be
                // visible to Git, in which case stubs must not be created
                // (design/08). The directory itself being a Git root (a `.git`
                // child) is always treated as visible.
                let dir_stub_path = abs_path.join(stub::DIR_STUB_NAME);
                let visible_to_git = abs_path.join(".git").exists()
                    || stub::stub_would_be_visible_to_git(&dir_stub_path, work_dir);
                if visible_to_git {
                    // Fall back to full materialisation for paths inside a local git
                    // worktree where stub files would be visible to Git.
                    materialise_tree(hash, &abs_path, work_dir, store, remote, remote_key)?;
                } else {
                    crate::fsguard::create_dir_all(work_dir, &abs_path)?;
                    stub::write_dir_stub(
                        work_dir,
                        &work_rel,
                        &StubRecord {
                            target_type: crate::stub::StubTargetType::Tree,
                            hash: hash.clone(),
                            size: *size,
                            mtime: *mtime,
                            mode: None,
                            blob_count: *blob_count,
                        },
                    )?;
                }
                changed += 1;
            }
        }
    }

    Ok(changed)
}

/// Stub reconcile (design/08_stub_system.md, "Stub reconcile (pull and
/// restore)"): after `apply_diff` has run and `clone_root` has been advanced
/// to `new_clone_root`, walk every stub currently in the working tree under
/// `scope_dir` and bring its record in line with `new_clone_root`, per the
/// reconcile table. Called once per pull (full or scoped) after the new
/// `clone_root` is written -- never while a conflict is outstanding, since a
/// conflicting pull returns an error before `clone_root` is ever advanced
/// (`Error::Conflict`, both in `pull_full` and `pull_scoped`), so reconcile
/// never runs against a `clone_root` that does not yet reflect this pull.
///
/// `stub::list` already stops descending at a directory stub (never
/// recursing into a fully- or partially-stubbed directory), which is exactly
/// the behaviour the reconcile table requires: a directory stub's own record
/// is checked and possibly updated, but nothing underneath it is touched,
/// and a partial-expansion directory's real files are left alone -- only its
/// `.omemfs-stub` record is a candidate for reconcile here.
///
/// `clone_root` navigation is local-only (`local`, never the lazy/remote
/// store): a stub's own reconcile never needs to fetch anything -- it either
/// updates metadata already carried by a `TreeEntry` in some ancestor's tree
/// object, or removes the stub -- and this pull has already fetched
/// everything its own diff touched. If an intermediate tree object the walk
/// would need is not in the local cache (e.g. a stub whose entire subtree,
/// including the spine down to it, this pull's scope never touched),
/// `navigate_entry` reports `Error::ObjectNotFound`; that one stub is simply
/// left unreconciled rather than triggering a fetch or aborting the pull --
/// this mirrors reconcile's own "stubbed content is not guaranteed to be
/// locally cached" premise (see the partial-expansion paragraph above) and
/// keeps a lazily-cloned pull's I/O bounded to what its own diff needed.
///
/// `pub(crate)`: also called by `push` (`commands::push::push_full`/
/// `push_scoped`), after it has durably advanced its own `clone_root`, to
/// implement design/08_stub_system.md's "After push, the stub's hash field
/// is updated to reflect the merged tree." Push does not duplicate this
/// decision logic, only triggers it at a different point.
pub(crate) fn reconcile_stubs(
    new_clone_root: &Hash,
    scope_dir: &std::path::Path,
    work_dir: &std::path::Path,
    local: &LocalStore,
) -> Result<(), Error> {
    for (rel, current) in stub::list_under(work_dir, scope_dir)? {
        reconcile_one_stub(new_clone_root, &rel, &current, work_dir, local)?;
    }
    Ok(())
}

/// Like `reconcile_stubs`, but for a single scoped-pull leaf path (`omemfs
/// pull <file>`, where the scope names one file rather than a directory)
/// instead of a directory `stub::list_under` can walk. `rel` is checked
/// directly against `stub::exists`/`stub::dir_exists` for whichever stub kind
/// (if any) is currently on disk at that path; a `rel` that is not currently
/// any kind of stub is a no-op (reconcile never creates a stub for a path
/// that has none -- design/08_stub_system.md).
///
/// `pub(crate)`: also called by `push_scoped` per pushed leaf path -- see
/// `reconcile_stubs`'s doc comment.
pub(crate) fn reconcile_leaf_stub(
    new_clone_root: &Hash,
    rel: &str,
    work_dir: &std::path::Path,
    local: &LocalStore,
) -> Result<(), Error> {
    let current = if let Some(record) = stub::read(work_dir, rel)? {
        record
    } else if let Some(record) = stub::read_dir_stub(work_dir, rel)? {
        record
    } else {
        return Ok(());
    };
    reconcile_one_stub(new_clone_root, rel, &current, work_dir, local)
}

/// Shared per-stub reconcile step used by both `reconcile_stubs` (directory
/// walk) and `reconcile_leaf_stub` (single scoped-pull leaf path): navigate
/// `new_clone_root` to `rel` (local-only -- see `reconcile_stubs`'s doc
/// comment for why a missing intermediate tree object is tolerated rather
/// than fetched) and apply whatever `stub::reconcile_action` decides.
fn reconcile_one_stub(
    new_clone_root: &Hash,
    rel: &str,
    current: &StubRecord,
    work_dir: &std::path::Path,
    local: &LocalStore,
) -> Result<(), Error> {
    let components: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
    let clone_entry = match tree_ops::navigate_entry(new_clone_root, &components, local) {
        Ok(e) => e,
        Err(Error::ObjectNotFound(_)) => return Ok(()),
        Err(e) => return Err(e),
    };
    let is_dir_stub = current.target_type == StubTargetType::Tree;
    let action = stub::reconcile_action(current, clone_entry.as_ref(), is_dir_stub);
    apply_reconcile_action(action, rel, is_dir_stub, work_dir)
}

/// Apply one `stub::ReconcileAction` decided for the stub at `rel` (a file
/// stub if `is_dir_stub` is false, a directory stub otherwise).
fn apply_reconcile_action(
    action: stub::ReconcileAction,
    rel: &str,
    is_dir_stub: bool,
    work_dir: &std::path::Path,
) -> Result<(), Error> {
    use stub::ReconcileAction;
    match action {
        ReconcileAction::UpToDate => Ok(()),
        ReconcileAction::UpdateFields(record) => {
            if is_dir_stub {
                stub::write_dir_stub(work_dir, rel, &record)
            } else {
                stub::write(work_dir, rel, &record)
            }
        }
        ReconcileAction::SwitchToFileStub(record) => {
            // dir stub -> file stub: remove the marker and the (now-empty,
            // aside from the marker just removed) directory, then write the
            // file stub in its place. `fs::remove_dir_all` (not fsguard, not
            // remove_dir) matches restore.rs's ReconcileDirStubToBlob: a
            // fully- or partially-stubbed directory's real files (if any, in
            // the partial-expansion case) are never guaranteed to be safe to
            // keep once clone_root says this path is now a plain blob, and
            // this mirrors the one existing precedent for this exact
            // transition rather than inventing a new one.
            stub::remove_dir_stub(work_dir, rel)?;
            let abs = work_dir.join(rel);
            if abs.exists() {
                fs::remove_dir_all(&abs)?;
            }
            stub::write(work_dir, rel, &record)
        }
        ReconcileAction::SwitchToDirStub(record) => {
            // file stub -> dir stub: remove the file stub marker, create the
            // directory, write the dir stub inside.
            stub::remove(work_dir, rel)?;
            let abs = work_dir.join(rel);
            crate::fsguard::create_dir_all(work_dir, &abs)?;
            stub::write_dir_stub(work_dir, rel, &record)
        }
        ReconcileAction::MaterialiseSymlink { target, mtime } => {
            // Symlinks are always materialised (design/08), for either stub
            // kind. A dir stub's directory must be cleared first (a symlink
            // cannot be created where a non-empty directory exists);
            // fs::remove_dir_all mirrors restore.rs's RestoreSymlink/
            // ReconcileDirStubToBlob handling of the same transition.
            let abs = work_dir.join(rel);
            if is_dir_stub {
                stub::remove_dir_stub(work_dir, rel)?;
                if abs.exists() {
                    fs::remove_dir_all(&abs)?;
                }
            } else {
                stub::remove(work_dir, rel)?;
            }
            crate::fsmeta::write_symlink_atomic(work_dir, &abs, &target)?;
            crate::fsmeta::restore_symlink_mtime(&abs, &mtime);
            Ok(())
        }
        ReconcileAction::Remove => {
            let abs = work_dir.join(rel);
            if is_dir_stub {
                stub::remove_dir_stub(work_dir, rel)?;
                // Remove the directory if it is now empty (it may still
                // hold real files from a partial expansion -- design/08:
                // reconcile never removes those).
                fs::remove_dir(&abs).ok();
            } else {
                stub::remove(work_dir, rel)?;
            }
            Ok(())
        }
    }
}

/// Materialise a blob into `abs_path`, restoring mtime and the executable-bit
/// mode, and removing any stale stub for the path. Pull performs no eager bulk
/// download, so the blob content is fetched from `remote` on demand here (it is
/// generally absent from the local cache).
#[allow(clippy::too_many_arguments)]
fn materialise_blob(
    abs_path: &std::path::Path,
    work_dir: &std::path::Path,
    work_rel: &str,
    hash: &Hash,
    mtime: &Option<chrono::DateTime<chrono::Utc>>,
    mode: &Option<String>,
    store: &dyn ObjectStore,
    remote: &dyn ObjectStore,
    remote_key: Option<&crate::codec::encrypt::EncryptKey>,
    observed: &ScanObservation,
) -> Result<(), Error> {
    ensure_blob_local(remote, store, hash, remote_key)?;
    // The fetch above can take a long time; re-check this path against the
    // scan right before it is overwritten (design/03).
    verify_one_before_write(observed, work_dir, abs_path)?;
    // `materialise_blob_at` (via fsguard) resolves and creates the parent
    // directory itself.
    crate::fsmeta::materialise_blob_at(work_dir, store, hash, abs_path, mtime, mode)?;
    stub::remove(work_dir, work_rel)?;
    Ok(())
}

fn print_conflict_result(conflicts: &[String]) {
    let is_tty = atty::is(atty::Stream::Stderr);
    let colored = crate::term::color_enabled(crate::term::ColorChoice::Auto, is_tty);
    let styles = Styles::new();
    let heading = paint(
        colored,
        styles.deleted,
        "Conflict: helper files written for the following paths:",
    );
    eprintln!("{}", heading);
    for p in conflicts {
        let p_str = paint(colored, styles.modified, &crate::term::escape_display(&p.to_string()));
        eprintln!("  conflict: {}", p_str);
    }
    eprintln!("\nResolve conflicts and push:");
    eprintln!("  omemfs push           (after resolving conflicts)");
    eprintln!("  omemfs restore <path> (discard local changes)");
}

fn plain_println(msg: &str) {
    let mut out = Output::for_stdout();
    let _ = out.writeln(msg);
    let _ = out.finish();
}

/// Render a single diff entry line with color: label in the change color, path in directory/meta.
fn paint_diff_entry(colored: bool, styles: &Styles, path: &str, label: &str) -> String {
    let label_style = match label {
        "added" => styles.added,
        "modified" => styles.modified,
        "deleted" => styles.deleted,
        _ => styles.meta,
    };
    let label_str = paint(colored, label_style, label);
    format!("{}: {}", label_str, path)
}

/// Build a one-entry tree for a regular-file scope without scanning sibling paths.
fn scan_file_as_tree(
    path: &std::path::Path,
    name: &str,
    store: &dyn ObjectStore,
) -> Result<Hash, Error> {
    let metadata = fs::symlink_metadata(path)?;
    let entry = if metadata.file_type().is_symlink() {
        TreeEntry::Symlink {
            name: name.to_string(),
            target: fs::read_link(path)?.to_string_lossy().into_owned(),
            mtime: crate::fsmeta::mtime_from_metadata(&metadata),
        }
    } else {
        TreeEntry::Blob {
            name: name.to_string(),
            hash: crate::codec::chunk::hash_file(path)?,
            mtime: crate::fsmeta::mtime_from_metadata(&metadata),
            size: metadata.len(),
            mode: crate::fsmeta::mode_from_metadata(&metadata),
        }
    };
    tree_ops::build_and_store(vec![entry], store)
}

/// Splice a leaf entry into `base_root`, or remove it when the remote deleted it.
fn splice_entry_into_clone_root(
    base_root: Option<&Hash>,
    components: &[&str],
    entry: Option<TreeEntry>,
    store: &dyn ObjectStore,
) -> Result<Hash, Error> {
    match entry {
        Some(entry) => tree_ops::splice_entry(base_root, components, entry, store),
        None => {
            let base =
                base_root.ok_or_else(|| Error::Other("no base root to remove from".to_string()))?;
            tree_ops::remove_entry(base, components, store)?
                .ok_or_else(|| Error::Other("entry not found in clone_root".to_string()))
        }
    }
}

/// Splice `new_hash` at `components` into `base_root`. If `new_hash` is
/// `None`, remove the entry at that path.
fn splice_into_clone_root(
    base_root: Option<&Hash>,
    components: &[&str],
    new_hash: Option<&Hash>,
    store: &dyn ObjectStore,
) -> Result<Hash, Error> {
    match new_hash {
        None => {
            // Remove the entry: splice with a tombstone is complex, so we
            // rebuild by removing the leaf from the parent tree.
            let base =
                base_root.ok_or_else(|| Error::Other("no base root to remove from".to_string()))?;
            tree_ops::remove_entry(base, components, store)?
                .ok_or_else(|| Error::Other("entry not found in clone_root".to_string()))
        }
        Some(nh) => {
            let leaf_name = *components.last().unwrap();
            let (mtime, size, blob_count) = tree_ops::tree_meta(nh, store)?;
            let entry = TreeEntry::Tree {
                name: leaf_name.to_string(),
                hash: nh.clone(),
                mtime,
                size,
                blob_count,
            };
            tree_ops::splice_entry(base_root, components, entry, store)
        }
    }
}

/// Build a flattened map of clone_root blob/symlink entries, but bounded to the
/// subtrees that exist as real paths in the working tree under `base_dir`.
///
/// The full `flatten_tree_entries` walks the entire clone_root, which — read
/// through `LazyTreeStore` — would re-download the whole clone_root skeleton,
/// defeating the lazy-pull optimization. The working-tree scan only visits
/// materialised paths (stubbed paths are not scanned), so the flattened map
/// only needs entries for paths present on disk. This descends a clone_root
/// child subtree only when the corresponding path exists in the working tree,
/// so unmaterialised (stubbed) subtrees are never fetched.
///
/// Correctness: a path missing from the returned map is simply re-hashed by the
/// scan (the mtime pre-filter is an optimization, never required). The returned
/// map is therefore always a safe subset of the full flatten.
fn flatten_clone_root_present(
    root_hash: &Hash,
    base_dir: &std::path::Path,
    store: &dyn ObjectStore,
) -> Result<HashMap<String, TreeEntry>, Error> {
    let mut map = HashMap::new();
    flatten_present_into(root_hash, "", base_dir, store, &mut map)?;
    Ok(map)
}

fn flatten_present_into(
    hash: &Hash,
    prefix: &str,
    base_dir: &std::path::Path,
    store: &dyn ObjectStore,
    out: &mut HashMap<String, TreeEntry>,
) -> Result<(), Error> {
    let entries = tree_ops::load_all_entries(hash, store)?;
    for entry in entries {
        let rel_path = if prefix.is_empty() {
            entry.name().to_string()
        } else {
            format!("{}/{}", prefix, entry.name())
        };
        match &entry {
            TreeEntry::Tree {
                hash: child_hash, ..
            } => {
                // Only descend if the directory exists on disk in the working
                // tree. A stubbed (unmaterialised) subtree is not present as a
                // real directory and is skipped, so its tree object is never
                // fetched. `symlink_metadata` + `is_dir()` treats a symlink to a
                // directory as not-a-directory, which is correct: the scan does
                // not descend into a symlink.
                let child_abs = base_dir.join(entry.name());
                let is_real_dir = std::fs::symlink_metadata(&child_abs)
                    .map(|m| m.is_dir())
                    .unwrap_or(false);
                if is_real_dir {
                    flatten_present_into(child_hash, &rel_path, &child_abs, store, out)?;
                }
            }
            _ => {
                out.insert(rel_path, entry);
            }
        }
    }
    Ok(())
}

/// Ensure the blob `hash` (possibly a chunked manifest) is present in `local`,
/// downloading it and any chunks from `remote` on demand. Pull performs no eager
/// bulk download, so this is the on-demand fetch path for every changed blob.
fn ensure_blob_local(
    remote: &dyn ObjectStore,
    local: &dyn ObjectStore,
    hash: &Hash,
    remote_key: Option<&crate::codec::encrypt::EncryptKey>,
) -> Result<(), Error> {
    if !local.exists(hash)? {
        crate::commands::transfer::transfer_objects(remote, local, hash, remote_key, true)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Diff
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum DiffEntry {
    Added {
        hash: Hash,
        size: u64,
        mtime: Option<chrono::DateTime<chrono::Utc>>,
        mode: Option<String>,
    },
    Modified {
        hash: Hash,
        size: u64,
        mtime: Option<chrono::DateTime<chrono::Utc>>,
        mode: Option<String>,
    },
    Deleted,
    AddedEmptyDir,
    /// A newly-added tree whose aggregate size meets the stub threshold.
    /// The entire subtree is represented by a single directory stub instead
    /// of individual blob entries.
    AddedTree {
        hash: Hash,
        size: u64,
        mtime: Option<chrono::DateTime<chrono::Utc>>,
        blob_count: u64,
    },
    /// A symlink added or modified (target changed) on the remote. Symlinks
    /// are always materialised (never stubbed) — design/08_stub_system.md.
    /// `was_present` distinguishes an added symlink (false) from a modified one
    /// (true) for the diff label only.
    Symlink {
        target: String,
        mtime: Option<chrono::DateTime<chrono::Utc>>,
        was_present: bool,
    },
}

impl DiffEntry {
    fn label(&self) -> &str {
        match self {
            DiffEntry::Added { .. } => "added",
            DiffEntry::Modified { .. } => "modified",
            DiffEntry::Deleted => "deleted",
            DiffEntry::AddedEmptyDir => "added",
            DiffEntry::AddedTree { .. } => "added",
            DiffEntry::Symlink { was_present, .. } => {
                if *was_present {
                    "modified"
                } else {
                    "added"
                }
            }
        }
    }
}

/// Returns true if the tree object at `hash` has a direct child tree entry named ".git".
/// This identifies a directory that is itself a Git repository root, so its children
/// must not be directory-stubbed (stubs inside a Git working tree are not allowed).
///
/// `hash` here is always a `TreeEntry::Tree`'s own hash (a real tree, by
/// construction), so this is not actually reachable with a blob whose content
/// happens to parse as tree JSON today -- but is gated on `Tree::is_tree_bytes`
/// anyway, matching design/01_object_model.md's "Tree/blob discrimination"
/// (D13) and the other three call sites named there, rather than inferring
/// "is this a tree?" from whether `Tree::deserialise` happens to succeed.
fn tree_contains_git_dir(store: &dyn ObjectStore, hash: &Hash) -> bool {
    let Ok(data) = codec::store_read(store, hash, None) else {
        return false;
    };
    if !Tree::is_tree_bytes(&data) {
        return false;
    }
    let Ok(Tree::Normal { entries }) = Tree::deserialise(&data) else {
        return false;
    };
    entries
        .iter()
        .any(|e| matches!(e, TreeEntry::Tree { name, .. } if name == ".git"))
}

/// Recursively materialise a tree object into `dir`, fetching any missing blobs
/// from `remote` on demand. Used as a fallback when a directory stub cannot be
/// placed inside a local Git working tree.
fn materialise_tree(
    tree_hash: &Hash,
    dir: &std::path::Path,
    work_dir: &std::path::Path,
    store: &dyn ObjectStore,
    remote: &dyn ObjectStore,
    remote_key: Option<&crate::codec::encrypt::EncryptKey>,
) -> Result<(), Error> {
    crate::fsguard::create_dir_all(work_dir, dir)?;
    let data = codec::store_read(store, tree_hash, None)?;
    let Tree::Normal { entries } = Tree::deserialise(&data)?;
    for entry in &entries {
        match entry {
            TreeEntry::Blob {
                name,
                hash,
                mtime,
                mode,
                ..
            } => {
                let abs = dir.join(name);
                ensure_blob_local(remote, store, hash, remote_key)?;
                crate::fsmeta::materialise_blob_at(work_dir, store, hash, &abs, mtime, mode)?;
            }
            TreeEntry::Tree {
                name, hash, mtime, ..
            } => {
                let sub = dir.join(name);
                materialise_tree(hash, &sub, work_dir, store, remote, remote_key)?;
                if let Some(mt) = mtime {
                    let ft = FileTime::from_unix_time(mt.timestamp(), mt.timestamp_subsec_nanos());
                    filetime::set_file_mtime(&sub, ft).ok();
                }
            }
            TreeEntry::Symlink {
                name,
                target,
                mtime,
            } => {
                #[cfg(unix)]
                {
                    let link_path = dir.join(name);
                    crate::fsmeta::write_symlink_atomic(work_dir, &link_path, target)?;
                    crate::fsmeta::restore_symlink_mtime(&link_path, mtime);
                }
                let _ = target;
            }
        }
    }
    Ok(())
}

/// Compute diff from `base` to `target`. Returns map of relative path → change.
///
/// When `stub_threshold > 0`, newly-added tree entries whose aggregate `size` is
/// at or above the threshold are emitted as `DiffEntry::AddedTree` rather than
/// being recursed into. This mirrors the clone threshold rule and allows pull to
/// place directory stubs for large newly-added subtrees.
///
/// `target: None` is not "nothing to compare, so no changes" — it means the
/// entire root being diffed to is absent (design/03_sync_model.md, "Whole-root
/// absence"). When `base` is `Some`, every entry reachable from `base` is
/// therefore reported `Deleted` (the same expansion `mark_deleted_tree`
/// already performs for a deleted subtree found mid-walk by `diff_recursive`).
/// When `base` is also `None`, nothing existed on either side, so the diff is
/// empty. This matters for a scoped pull's local-diff: a `<path>` removed
/// entirely from the working tree has no scoped hash to pass as `target`, and
/// must still be diffed as a full local deletion, not an empty diff, so a
/// conflicting remote change to `<path>` is still caught by
/// `classify_conflicts` (a full pull's own local diff never hits this branch,
/// since its top-level working-tree scan always yields some root hash).
fn diff_trees(
    base: Option<&Hash>,
    target: Option<&Hash>,
    store: &dyn ObjectStore,
    stub_threshold: u64,
) -> Result<HashMap<String, DiffEntry>, Error> {
    let mut result = HashMap::new();
    match target {
        Some(t) => {
            diff_recursive(
                base,
                t,
                store,
                &mut String::new(),
                &mut result,
                stub_threshold,
                false,
            )?;
        }
        None => {
            if let Some(b) = base {
                mark_deleted_tree(b, store, "", &mut result)?;
            }
        }
    }
    Ok(result)
}

fn diff_recursive(
    base: Option<&Hash>,
    target: &Hash,
    store: &dyn ObjectStore,
    prefix: &mut String,
    result: &mut HashMap<String, DiffEntry>,
    stub_threshold: u64,
    in_git_ctx: bool,
) -> Result<(), Error> {
    if base == Some(target) {
        return Ok(());
    }

    let target_entries = tree_ops::load_all_entries(target, store)?;
    // Full base entries (not just hashes): blob comparison needs `mode` so
    // that an executable-bit-only remote change is applied as modified,
    // matching ls's dirty detection.
    let base_entries: HashMap<String, TreeEntry> = if let Some(base_hash) = base {
        tree_ops::load_all_entries(base_hash, store)?
            .into_iter()
            .map(|e| (e.name().to_string(), e))
            .collect()
    } else {
        HashMap::new()
    };

    for entry in &target_entries {
        let name = entry.name();
        let path = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{}/{}", prefix, name)
        };

        match entry {
            TreeEntry::Blob {
                hash,
                size,
                mtime,
                mode,
                ..
            } => {
                match base_entries.get(name) {
                    Some(TreeEntry::Blob {
                        hash: base_hash,
                        mode: base_mode,
                        ..
                    }) => {
                        // A mode-only change still rewrites the content; the
                        // blob is in the local cache, so this is a local
                        // read+write (a chmod-only fast path could avoid it).
                        if base_hash != hash || base_mode != mode {
                            result.insert(
                                path,
                                DiffEntry::Modified {
                                    hash: hash.clone(),
                                    size: *size,
                                    mtime: *mtime,
                                    mode: mode.clone(),
                                },
                            );
                        }
                    }
                    // Entry kind changed (was a tree): design/03_sync_model.md
                    // "Diff algorithm" -- a type change emits deleted(path) +
                    // added(path), and when the old side is a tree that
                    // deleted(path) expands to every blob/symlink reachable
                    // under it, so the receiver removes them (and, via
                    // apply_diff's directory cleanup, the now-empty directory
                    // itself) before this new blob is written in its place.
                    Some(TreeEntry::Tree { hash: old_hash, .. }) => {
                        mark_deleted_tree(old_hash, store, &path, result)?;
                        result.insert(
                            path,
                            DiffEntry::Added {
                                hash: hash.clone(),
                                size: *size,
                                mtime: *mtime,
                                mode: mode.clone(),
                            },
                        );
                    }
                    // Entry kind changed (was a symlink): no subtree to unwind
                    // -- this is a plain overwrite of the single path, so it
                    // is reported as `Added` (matching a fresh write) rather
                    // than `Modified` (which assumes a prior blob hash to
                    // compare against).
                    Some(TreeEntry::Symlink { .. }) => {
                        result.insert(
                            path,
                            DiffEntry::Added {
                                hash: hash.clone(),
                                size: *size,
                                mtime: *mtime,
                                mode: mode.clone(),
                            },
                        );
                    }
                    None => {
                        result.insert(
                            path,
                            DiffEntry::Added {
                                hash: hash.clone(),
                                size: *size,
                                mtime: *mtime,
                                mode: mode.clone(),
                            },
                        );
                    }
                }
            }
            TreeEntry::Tree {
                hash,
                size,
                mtime,
                blob_count,
                ..
            } => {
                let base_tree_hash = match base_entries.get(name) {
                    Some(TreeEntry::Tree { hash: h, .. }) => Some(h),
                    // Entry kind changed (was a blob or symlink): diff against
                    // an empty base so all descendants are reported as added,
                    // but first report the old blob/symlink itself as
                    // deleted (design/03_sync_model.md "Diff algorithm") --
                    // it has no place in the new tree that replaces it.
                    Some(TreeEntry::Blob { .. }) | Some(TreeEntry::Symlink { .. }) => {
                        result.insert(path, DiffEntry::Deleted);
                        None
                    }
                    None => None,
                };
                let prev_len = prefix.len();
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(name);

                // Propagate git-worktree context: true when already inside one,
                // when the entry IS ".git", or when the entry itself contains a
                // ".git" child (meaning it is a git repo root).
                let child_in_git =
                    in_git_ctx || name == ".git" || tree_contains_git_dir(store, hash);

                // Stub the entire subtree when the entry is newly added, large, and
                // not already inside a git working tree that is being materialised.
                // Whether this entry is itself a git repo root does not block
                // whole-unit stubbing (design/08_stub_system.md, "Stubs and Git
                // repositories": a git repo root at/above the threshold is always
                // stubbed as a unit) -- `child_in_git` only matters once we decide
                // to descend instead, to keep this entry's own children from being
                // partially stubbed. Matches `clone.rs`'s `expand_or_stub`, which
                // gates its analogous `should_stub` on the inherited flag only.
                let should_stub = base_tree_hash.is_none()
                    && stub_threshold > 0
                    && *size >= stub_threshold
                    && !in_git_ctx;

                if should_stub {
                    result.insert(
                        prefix.clone(),
                        DiffEntry::AddedTree {
                            hash: hash.clone(),
                            size: *size,
                            mtime: *mtime,
                            blob_count: *blob_count,
                        },
                    );
                } else {
                    let before = result.len();
                    diff_recursive(
                        base_tree_hash,
                        hash,
                        store,
                        prefix,
                        result,
                        stub_threshold,
                        child_in_git,
                    )?;
                    // If this is a new directory and nothing was inserted beneath it,
                    // it is an empty directory — record it explicitly.
                    if base_tree_hash.is_none() && result.len() == before {
                        result.insert(prefix.clone(), DiffEntry::AddedEmptyDir);
                    }
                }
                prefix.truncate(prev_len);
            }
            TreeEntry::Symlink { target, mtime, .. } => {
                match base_entries.get(name) {
                    // Same name, same kind: a modification only when the target changed.
                    Some(TreeEntry::Symlink {
                        target: base_target,
                        ..
                    }) => {
                        if base_target != target {
                            result.insert(
                                path,
                                DiffEntry::Symlink {
                                    target: target.clone(),
                                    mtime: *mtime,
                                    was_present: true,
                                },
                            );
                        }
                    }
                    // Kind changed and the old side was a tree: unwind its
                    // full contents as deleted (design/03_sync_model.md
                    // "Diff algorithm") before the symlink replaces it,
                    // matching the Blob/Tree-swap handling above.
                    Some(TreeEntry::Tree { hash: old_hash, .. }) => {
                        mark_deleted_tree(old_hash, store, &path, result)?;
                        result.insert(
                            path,
                            DiffEntry::Symlink {
                                target: target.clone(),
                                mtime: *mtime,
                                was_present: true,
                            },
                        );
                    }
                    // Kind changed and the old side was a blob: a plain
                    // single-path overwrite, no subtree to unwind.
                    Some(TreeEntry::Blob { .. }) => {
                        result.insert(
                            path,
                            DiffEntry::Symlink {
                                target: target.clone(),
                                mtime: *mtime,
                                was_present: true,
                            },
                        );
                    }
                    // New symlink.
                    None => {
                        result.insert(
                            path,
                            DiffEntry::Symlink {
                                target: target.clone(),
                                mtime: *mtime,
                                was_present: false,
                            },
                        );
                    }
                }
            }
        }
    }

    // Entries deleted from base.
    let target_names: std::collections::HashSet<&str> =
        target_entries.iter().map(|e| e.name()).collect();
    for (name, base_entry) in &base_entries {
        if !target_names.contains(name.as_str()) {
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", prefix, name)
            };
            match base_entry {
                TreeEntry::Tree { hash, .. } => {
                    let mut sub_result = HashMap::new();
                    mark_deleted_tree(hash, store, &path, &mut sub_result)?;
                    result.extend(sub_result);
                }
                TreeEntry::Blob { .. } | TreeEntry::Symlink { .. } => {
                    result.insert(path, DiffEntry::Deleted);
                }
            }
        }
    }
    Ok(())
}

fn mark_deleted_tree(
    hash: &Hash,
    store: &dyn ObjectStore,
    prefix: &str,
    result: &mut HashMap<String, DiffEntry>,
) -> Result<(), Error> {
    for entry in tree_ops::load_all_entries(hash, store)? {
        // Mirrors diff_recursive's path join: an empty prefix (top-level call,
        // scoped to the deleted subtree root) must not add a leading slash.
        let path = if prefix.is_empty() {
            entry.name().to_string()
        } else {
            format!("{}/{}", prefix, entry.name())
        };
        match entry {
            TreeEntry::Blob { .. } | TreeEntry::Symlink { .. } => {
                result.insert(path, DiffEntry::Deleted);
            }
            TreeEntry::Tree { hash, .. } => {
                mark_deleted_tree(&hash, store, &path, result)?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::local::LocalStore;
    use tempfile::TempDir;

    #[test]
    fn working_file_source_present_and_absent() {
        let dir = TempDir::new().unwrap();
        // Absent file -> None (matches the old fs::read(..).ok() semantics).
        let absent = dir.path().join("nope.txt");
        assert!(working_file_source(&absent).is_none());

        // Present regular file -> Some(WorkingFile(_)).
        let present = dir.path().join("yes.txt");
        fs::write(&present, b"hello").unwrap();
        assert!(
            matches!(
                working_file_source(&present),
                Some(HelperSource::WorkingFile(_))
            ),
            "expected Some(WorkingFile(_)) for a regular file"
        );
    }

    #[test]
    fn working_file_source_directory_returns_none() {
        // `File::open` succeeds on a directory on Linux but `io::copy` would fail
        // with EISDIR. `working_file_source` must return `None` for directories so
        // that the conflict report is clean rather than being converted to an I/O error.
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("subdir");
        fs::create_dir(&sub).unwrap();
        assert!(
            working_file_source(&sub).is_none(),
            "expected None for a directory path"
        );
    }

    // -----------------------------------------------------------------------
    // apply_diff blob-fetch batching (Improvement B) --
    // design/02_storage_format.md "Multi-root batching (Improvement B)";
    // design/04_cli_spec.md pull step 4: "Collect the blob hashes referenced
    // by the remote diff that are missing from the local cache and download
    // all of them in a single batched, concurrent transfer ... rather than
    // one blob at a time."
    //
    // `apply_diff` already exists (no new API needs to be invented here,
    // unlike expand's equivalent test), so these tests exercise it directly
    // with a diff full of independent `Added` blob entries.
    // -----------------------------------------------------------------------

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Read-side `ObjectStore` wrapper that tracks concurrently-in-flight
    /// `open_read` calls (with an artificial delay to make overlap
    /// observable) and forces a fixed `default_transfer_concurrency()`.
    /// Duplicated from the equivalent fixtures in `push.rs`'s and
    /// `expand.rs`'s tests -- there is no shared test-utility module in this
    /// crate today, and forcing the concurrency value directly avoids
    /// mutating the process-wide `OMEMFS_TRANSFER_CONCURRENCY` env var across
    /// Rust's parallel test runner.
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

    /// Build a diff of `n` independent `Added` blob entries at distinct
    /// paths, each pointing at a unique single-chunk blob stored in `remote`.
    fn build_added_blobs_diff(
        remote: &dyn ObjectStore,
        n: usize,
    ) -> (HashMap<String, DiffEntry>, Vec<(String, Vec<u8>)>) {
        let mut diff = HashMap::new();
        let mut files = Vec::with_capacity(n);
        for i in 0..n {
            let rel = format!("file{i}.txt");
            let content = format!("pull-test-content-{i}").into_bytes();
            let serialised = crate::object::serialise_blob(&content);
            let hash = crate::object::blob_hash(&content);
            codec::chunk::store_chunked(remote, &hash, &serialised, None).unwrap();
            diff.insert(
                rel.clone(),
                DiffEntry::Added {
                    hash,
                    size: content.len() as u64,
                    mtime: None,
                    mode: None,
                },
            );
            files.push((rel, content));
        }
        (diff, files)
    }

    // -----------------------------------------------------------------------
    // Re-verification right before a write (review R1; design/03 "Re-verifying
    // a path immediately before it is overwritten")
    // -----------------------------------------------------------------------

    fn scanned(path: &std::path::Path) -> crate::scan::ScannedFile {
        let md = fs::metadata(path).unwrap();
        crate::scan::ScannedFile {
            fs_mtime: md.modified().unwrap(),
            fs_size: md.len(),
            hash: crate::object::blob_hash(&fs::read(path).unwrap()),
            cache_hit: false,
            fallback_hit: false,
            blob_sealed: false,
            observed_at: std::time::SystemTime::now(),
        }
    }

    fn modified(content: &[u8]) -> DiffEntry {
        DiffEntry::Modified {
            hash: crate::object::blob_hash(content),
            size: content.len() as u64,
            mtime: None,
            mode: None,
        }
    }

    fn observed(dir: &std::path::Path, names: &[&str]) -> ScanObservation {
        let mut files = HashMap::new();
        for n in names {
            files.insert((*n).to_string(), scanned(&dir.join(n)));
        }
        ScanObservation::new(files)
    }

    #[test]
    fn a_stubbed_path_with_no_real_file_is_unchanged() {
        // Regression: `big.bin.omemfs-stub` stands for a `big.bin` that is absent
        // by design. A remote Modified/Deleted on it must not abort the pull.
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("big.bin.omemfs-stub"), b"{}").unwrap();
        let obs = ScanObservation::with_stubs(HashMap::new(), [], ["big.bin".to_string()]);
        assert!(!changed_since_scan(&obs, "big.bin", &dir.path().join("big.bin")));
    }

    #[test]
    fn a_real_file_appearing_at_a_stubbed_path_is_a_change() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("big.bin.omemfs-stub"), b"{}").unwrap();
        let obs = ScanObservation::with_stubs(HashMap::new(), [], ["big.bin".to_string()]);
        fs::write(dir.path().join("big.bin"), b"user materialised it").unwrap();
        assert!(changed_since_scan(&obs, "big.bin", &dir.path().join("big.bin")));
    }

    #[test]
    fn verify_accepts_a_remote_modification_of_a_stubbed_file() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("big.bin.omemfs-stub"), b"{}").unwrap();
        let obs = ScanObservation::with_stubs(HashMap::new(), [], ["big.bin".to_string()]);
        let mut diff = HashMap::new();
        diff.insert("big.bin".to_string(), modified(b"new"));
        diff.insert("gone.bin".to_string(), DiffEntry::Deleted);
        assert!(verify_unchanged_since_scan(&diff, dir.path(), dir.path(), &obs).is_empty());
    }

    #[test]
    fn a_path_the_pull_removed_itself_is_not_reported_as_changed() {
        // Regression: a directory replaced by a file. The pull deletes the
        // children, prunes the directory, then writes the file at the same path.
        let dir = TempDir::new().unwrap();
        let obs = ScanObservation::with_other(HashMap::new(), ["d".to_string()]);
        // The pull has pruned `d` (it is gone), and says so.
        obs.record_removed("d");
        assert!(!changed_since_scan(&obs, "d", &dir.path().join("d")));
    }

    #[test]
    fn a_path_missing_but_not_removed_by_the_pull_is_still_a_change() {
        let dir = TempDir::new().unwrap();
        let obs = ScanObservation::with_other(HashMap::new(), ["d".to_string()]);
        assert!(changed_since_scan(&obs, "d", &dir.path().join("d")));
    }

    #[test]
    fn a_removed_path_that_a_user_recreated_afterwards_is_a_change() {
        // The pull removed `f`, then the user created their own `f` before our
        // write. Removal by us must not excuse what appeared after it.
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("f"), b"user").unwrap();
        let obs = ScanObservation::with_other(HashMap::new(), ["f".to_string()]);
        obs.record_removed("f");
        assert!(changed_since_scan(&obs, "f", &dir.path().join("f")));
    }

    #[test]
    fn apply_diff_replaces_a_directory_with_a_file_without_a_false_alarm() {
        let remote_dir = TempDir::new().unwrap();
        let remote = LocalStore::for_remote(remote_dir.path());
        let store_dir = TempDir::new().unwrap();
        let store = LocalStore::for_cache(store_dir.path());
        let work = TempDir::new().unwrap();
        fs::create_dir(work.path().join("d")).unwrap();
        fs::write(work.path().join("d/child.txt"), b"1").unwrap();
        let obs = ScanObservation::with_other(
            observed(work.path(), &["d/child.txt"]).files_for_test(),
            ["d".to_string()],
        );
        let content = b"now a file";
        codec::chunk::store_chunked(
            &remote,
            &crate::object::blob_hash(content),
            &crate::object::serialise_blob(content),
            None,
        )
        .unwrap();
        let mut diff = HashMap::new();
        diff.insert("d/child.txt".to_string(), DiffEntry::Deleted);
        diff.insert(
            "d".to_string(),
            DiffEntry::Added {
                hash: crate::object::blob_hash(content),
                size: content.len() as u64,
                mtime: None,
                mode: None,
            },
        );
        apply_diff(&diff, work.path(), &store, &store, work.path(), 0, &remote, None, &obs)
            .unwrap();
        assert_eq!(fs::read(work.path().join("d")).unwrap(), content);
    }

    #[test]
    fn verify_accepts_a_symlink_the_scan_saw_and_that_is_still_there() {
        // Regression: a remote change to a symlink the working tree already has.
        // The scan holds no (mtime, size) for it; "seen, still present" is fine.
        let dir = TempDir::new().unwrap();
        std::os::unix::fs::symlink("old-target", dir.path().join("link")).unwrap();
        let obs = ScanObservation::with_other(HashMap::new(), ["link".to_string()]);
        let mut diff = HashMap::new();
        diff.insert(
            "link".to_string(),
            DiffEntry::Symlink {
                target: "new-target".to_string(),
                mtime: None,
                was_present: true,
            },
        );
        assert!(verify_unchanged_since_scan(&diff, dir.path(), dir.path(), &obs).is_empty());
    }

    #[test]
    fn verify_accepts_a_directory_the_scan_saw_when_the_pull_replaces_it() {
        // Type change: the working tree has a directory the remote replaced with
        // a file. The diff carries Deleted for its children and Added for the path.
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("d")).unwrap();
        fs::write(dir.path().join("d/child.txt"), b"1").unwrap();
        let obs = ScanObservation::with_other(
            observed(dir.path(), &["d/child.txt"]).files_for_test(),
            ["d".to_string()],
        );
        let mut diff = HashMap::new();
        diff.insert("d/child.txt".to_string(), DiffEntry::Deleted);
        diff.insert("d".to_string(), added(b"now a file"));
        assert!(verify_unchanged_since_scan(&diff, dir.path(), dir.path(), &obs).is_empty());
    }

    #[test]
    fn verify_flags_a_seen_symlink_that_has_vanished() {
        let dir = TempDir::new().unwrap();
        let obs = ScanObservation::with_other(HashMap::new(), ["link".to_string()]);
        let mut diff = HashMap::new();
        diff.insert(
            "link".to_string(),
            DiffEntry::Symlink {
                target: "t".to_string(),
                mtime: None,
                was_present: true,
            },
        );
        assert_eq!(
            verify_unchanged_since_scan(&diff, dir.path(), dir.path(), &obs),
            vec!["link".to_string()]
        );
    }

    #[test]
    fn verify_flags_a_seen_symlink_replaced_by_a_regular_file() {
        // Not a benign type change: the user swapped the symlink for their own file.
        let dir = TempDir::new().unwrap();
        std::os::unix::fs::symlink("t", dir.path().join("link")).unwrap();
        let obs = ScanObservation::with_other(HashMap::new(), ["link".to_string()]);
        fs::remove_file(dir.path().join("link")).unwrap();
        fs::write(dir.path().join("link"), b"user data").unwrap();
        let mut diff = HashMap::new();
        diff.insert(
            "link".to_string(),
            DiffEntry::Symlink {
                target: "new".to_string(),
                mtime: None,
                was_present: true,
            },
        );
        assert_eq!(
            verify_unchanged_since_scan(&diff, dir.path(), dir.path(), &obs),
            vec!["link".to_string()]
        );
    }

    #[test]
    fn verify_accepts_paths_unchanged_since_the_scan() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.txt"), b"one").unwrap();
        let obs = observed(dir.path(), &["a.txt"]);
        let mut diff = HashMap::new();
        diff.insert("a.txt".to_string(), modified(b"two"));
        assert!(verify_unchanged_since_scan(&diff, dir.path(), dir.path(), &obs).is_empty());
    }

    #[test]
    fn verify_flags_a_file_edited_after_the_scan() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.txt"), b"one").unwrap();
        let obs = observed(dir.path(), &["a.txt"]);
        // Different size: detected regardless of mtime granularity.
        fs::write(dir.path().join("a.txt"), b"one, edited while downloading").unwrap();
        let mut diff = HashMap::new();
        diff.insert("a.txt".to_string(), modified(b"two"));
        assert_eq!(
            verify_unchanged_since_scan(&diff, dir.path(), dir.path(), &obs),
            vec!["a.txt".to_string()]
        );
    }

    #[test]
    fn verify_flags_a_same_size_edit_via_mtime() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.txt"), b"one").unwrap();
        let obs = observed(dir.path(), &["a.txt"]);
        fs::write(dir.path().join("a.txt"), b"uno").unwrap(); // same length
        let later = std::time::SystemTime::now() + Duration::from_secs(60);
        filetime::set_file_mtime(
            dir.path().join("a.txt"),
            filetime::FileTime::from_system_time(later),
        )
        .unwrap();
        let mut diff = HashMap::new();
        diff.insert("a.txt".to_string(), modified(b"two"));
        assert_eq!(
            verify_unchanged_since_scan(&diff, dir.path(), dir.path(), &obs),
            vec!["a.txt".to_string()]
        );
    }

    #[test]
    fn verify_flags_a_file_deleted_after_the_scan() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.txt"), b"one").unwrap();
        let obs = observed(dir.path(), &["a.txt"]);
        fs::remove_file(dir.path().join("a.txt")).unwrap();
        let mut diff = HashMap::new();
        diff.insert("a.txt".to_string(), modified(b"two"));
        assert_eq!(
            verify_unchanged_since_scan(&diff, dir.path(), dir.path(), &obs),
            vec!["a.txt".to_string()]
        );
    }

    #[test]
    fn verify_flags_a_new_file_appearing_where_the_pull_will_add_one() {
        // The scan found nothing at b.txt; a local file appeared since, and the
        // pull's Added(b.txt) would replace it.
        let dir = TempDir::new().unwrap();
        let obs = ScanObservation::new(HashMap::new());
        fs::write(dir.path().join("b.txt"), b"local addition").unwrap();
        let mut diff = HashMap::new();
        diff.insert("b.txt".to_string(), added(b"remote"));
        assert_eq!(
            verify_unchanged_since_scan(&diff, dir.path(), dir.path(), &obs),
            vec!["b.txt".to_string()]
        );
    }

    #[test]
    fn verify_accepts_an_added_path_that_is_still_absent() {
        let dir = TempDir::new().unwrap();
        let obs = ScanObservation::new(HashMap::new());
        let mut diff = HashMap::new();
        diff.insert("b.txt".to_string(), added(b"remote"));
        assert!(verify_unchanged_since_scan(&diff, dir.path(), dir.path(), &obs).is_empty());
    }

    #[test]
    fn verify_flags_a_deletion_of_a_file_that_appeared_after_the_scan() {
        // Scan saw nothing at c.txt (so the diff was clean); the pull would delete
        // it, but a file is there now.
        let dir = TempDir::new().unwrap();
        let obs = ScanObservation::new(HashMap::new());
        fs::write(dir.path().join("c.txt"), b"new local file").unwrap();
        let mut diff = HashMap::new();
        diff.insert("c.txt".to_string(), DiffEntry::Deleted);
        assert_eq!(
            verify_unchanged_since_scan(&diff, dir.path(), dir.path(), &obs),
            vec!["c.txt".to_string()]
        );
    }

    #[test]
    fn verify_ignores_entries_with_no_working_file_to_lose() {
        // Directory stubs, empty dirs and paths the scan never covered (stubbed
        // or ignored) are not regular files the scan could have observed.
        let dir = TempDir::new().unwrap();
        let obs = ScanObservation::new(HashMap::new());
        let mut diff = HashMap::new();
        diff.insert("d".to_string(), DiffEntry::AddedEmptyDir);
        diff.insert("gone.txt".to_string(), DiffEntry::Deleted); // absent on both
        assert!(verify_unchanged_since_scan(&diff, dir.path(), dir.path(), &obs).is_empty());
    }

    #[test]
    fn verify_reports_every_offending_path_sorted() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.txt"), b"1").unwrap();
        fs::write(dir.path().join("b.txt"), b"2").unwrap();
        let obs = observed(dir.path(), &["a.txt", "b.txt"]);
        fs::write(dir.path().join("b.txt"), b"22").unwrap();
        fs::write(dir.path().join("a.txt"), b"11").unwrap();
        let mut diff = HashMap::new();
        diff.insert("b.txt".to_string(), modified(b"x"));
        diff.insert("a.txt".to_string(), modified(b"y"));
        assert_eq!(
            verify_unchanged_since_scan(&diff, dir.path(), dir.path(), &obs),
            vec!["a.txt".to_string(), "b.txt".to_string()]
        );
    }

    #[test]
    fn apply_diff_refuses_to_overwrite_a_file_edited_after_the_scan() {
        let remote_dir = TempDir::new().unwrap();
        let remote = LocalStore::for_remote(remote_dir.path());
        let store_dir = TempDir::new().unwrap();
        let store = LocalStore::for_cache(store_dir.path());
        let work = TempDir::new().unwrap();

        let content = b"remote version";
        let hash = crate::object::blob_hash(content);
        codec::chunk::store_chunked(&remote, &hash, &crate::object::serialise_blob(content), None)
            .unwrap();
        fs::write(work.path().join("a.txt"), b"scanned").unwrap();
        let obs = observed(work.path(), &["a.txt"]);
        fs::write(work.path().join("a.txt"), b"edited while the pull was downloading").unwrap();

        let mut diff = HashMap::new();
        diff.insert(
            "a.txt".to_string(),
            DiffEntry::Modified {
                hash,
                size: content.len() as u64,
                mtime: None,
                mode: None,
            },
        );
        let err = apply_diff(
            &diff,
            work.path(),
            &store,
            &store,
            work.path(),
            0,
            &remote,
            None,
            &obs,
        )
        .unwrap_err();
        assert!(matches!(err, Error::Conflict), "{err:?}");
        assert_eq!(
            fs::read(work.path().join("a.txt")).unwrap(),
            b"edited while the pull was downloading",
            "the user's edit must survive"
        );
    }

    #[test]
    fn apply_diff_writes_nothing_when_any_path_fails_the_upfront_check() {
        // The check for every path runs before the first write, so one edited
        // file keeps the other, clean, files from being applied first.
        let remote_dir = TempDir::new().unwrap();
        let remote = LocalStore::for_remote(remote_dir.path());
        let (mut diff, _files) = build_added_blobs_diff(&remote, 3);
        let store_dir = TempDir::new().unwrap();
        let store = LocalStore::for_cache(store_dir.path());
        let work = TempDir::new().unwrap();
        fs::write(work.path().join("keep.txt"), b"scanned").unwrap();
        let obs = observed(work.path(), &["keep.txt"]);
        fs::write(work.path().join("keep.txt"), b"edited later, longer").unwrap();
        // The remote blob must exist: the download precedes the check.
        let keep = b"remote keep";
        codec::chunk::store_chunked(
            &remote,
            &crate::object::blob_hash(keep),
            &crate::object::serialise_blob(keep),
            None,
        )
        .unwrap();
        diff.insert("keep.txt".to_string(), modified(keep));

        let err = apply_diff(
            &diff,
            work.path(),
            &store,
            &store,
            work.path(),
            0,
            &remote,
            None,
            &obs,
        )
        .unwrap_err();
        assert!(matches!(err, Error::Conflict));
        for i in 0..3 {
            assert!(
                !work.path().join(format!("file{i}.txt")).exists(),
                "no clean path may be applied when another fails the check"
            );
        }
    }

    #[test]
    fn apply_diff_still_applies_when_nothing_changed_since_the_scan() {
        let remote_dir = TempDir::new().unwrap();
        let remote = LocalStore::for_remote(remote_dir.path());
        let (diff, files) = build_added_blobs_diff(&remote, 4);
        let store_dir = TempDir::new().unwrap();
        let store = LocalStore::for_cache(store_dir.path());
        let work = TempDir::new().unwrap();
        let obs = ScanObservation::new(HashMap::new());
        let changed = apply_diff(
            &diff,
            work.path(),
            &store,
            &store,
            work.path(),
            0,
            &remote,
            None,
            &obs,
        )
        .unwrap();
        assert_eq!(changed, files.len());
    }

    #[test]
    fn apply_diff_rechecks_a_path_right_before_its_own_write() {
        // The up-front pass passes; the file is edited during the (slow) blob
        // fetch. A per-path check just before the write must catch it. The
        // remote store's open_read hook edits the file, simulating that.
        struct EditingStore {
            inner: LocalStore,
            victim: std::path::PathBuf,
        }
        impl ObjectStore for EditingStore {
            fn exists(&self, h: &Hash) -> Result<bool, Error> {
                self.inner.exists(h)
            }
            fn size(&self, h: &Hash) -> Result<u64, Error> {
                self.inner.size(h)
            }
            fn list_with_sizes(&self) -> Result<Vec<(String, u64)>, Error> {
                self.inner.list_with_sizes()
            }
            fn open_read(&self, h: &Hash) -> Result<Box<dyn std::io::Read>, Error> {
                fs::write(&self.victim, b"edited during the download, much longer").unwrap();
                self.inner.open_read(h)
            }
            fn write_from(&self, h: &Hash, r: &mut dyn std::io::Read) -> Result<(), Error> {
                self.inner.write_from(h, r)
            }
        }

        let remote_dir = TempDir::new().unwrap();
        let inner = LocalStore::for_remote(remote_dir.path());
        let content = b"remote version";
        let hash = crate::object::blob_hash(content);
        codec::chunk::store_chunked(&inner, &hash, &crate::object::serialise_blob(content), None)
            .unwrap();
        let work = TempDir::new().unwrap();
        let victim = work.path().join("a.txt");
        fs::write(&victim, b"scanned").unwrap();
        let obs = observed(work.path(), &["a.txt"]);
        let remote = EditingStore { inner, victim: victim.clone() };

        let store_dir = TempDir::new().unwrap();
        let store = LocalStore::for_cache(store_dir.path());
        let mut diff = HashMap::new();
        diff.insert(
            "a.txt".to_string(),
            DiffEntry::Modified {
                hash,
                size: content.len() as u64,
                mtime: None,
                mode: None,
            },
        );
        let err = apply_diff(
            &diff,
            work.path(),
            &store,
            &store,
            work.path(),
            0,
            &remote,
            None,
            &obs,
        )
        .unwrap_err();
        assert!(matches!(err, Error::Conflict), "{err:?}");
        assert_eq!(
            fs::read(&victim).unwrap(),
            b"edited during the download, much longer"
        );
    }

    #[test]
    fn scan_observation_maps_scoped_keys_to_repo_relative_paths() {
        // A scoped pull's diff keys are relative to the scoped subtree; the scan
        // records repo-relative paths. The lookup must join base_dir back on.
        let work = TempDir::new().unwrap();
        fs::create_dir(work.path().join("sub")).unwrap();
        fs::write(work.path().join("sub/f.txt"), b"one").unwrap();
        let mut files = HashMap::new();
        files.insert("sub/f.txt".to_string(), scanned(&work.path().join("sub/f.txt")));
        let obs = ScanObservation::new(files);
        fs::write(work.path().join("sub/f.txt"), b"one and more").unwrap();
        let mut diff = HashMap::new();
        diff.insert("f.txt".to_string(), modified(b"x"));
        assert_eq!(
            verify_unchanged_since_scan(&diff, &work.path().join("sub"), work.path(), &obs),
            vec!["sub/f.txt".to_string()]
        );
    }

    #[test]
    fn apply_diff_materialises_correct_content_for_many_added_blobs() {
        // Correctness only (design/04 pull step 5). This must already pass
        // today, before the batched-fetch change: apply_diff's end result is
        // unaffected by *how many* transfer calls it takes to get there.
        let remote_dir = TempDir::new().unwrap();
        let remote = LocalStore::for_remote(remote_dir.path());
        let (diff, files) = build_added_blobs_diff(&remote, 6);

        let store_dir = TempDir::new().unwrap();
        let store = LocalStore::for_cache(store_dir.path());
        let work_dir = TempDir::new().unwrap();

        let changed = apply_diff(
            &diff,
            work_dir.path(),
            &store,
            &store,
            work_dir.path(),
            0,
            &remote,
            None,
            &ScanObservation::new(HashMap::new()),
        )
        .unwrap();

        assert_eq!(changed, files.len());
        for (rel, content) in &files {
            let path = work_dir.path().join(rel);
            assert_eq!(
                fs::read(&path).unwrap(),
                *content,
                "materialised content mismatch for {rel}"
            );
        }
    }

    #[test]
    fn apply_diff_batches_blob_fetches_for_concurrency() {
        // Improvement B applied to pull's diff-driven fetch path. A diff with
        // N independent `Added` blob entries, each below the CDC min_size
        // chunk threshold, is exactly the case the design doc calls out:
        // each blob's own BFS (inside `ensure_blob_local` ->
        // `transfer_objects`) is a single childless node with nothing to
        // divide across workers, and `apply_diff`'s `for (rel_path, change)
        // in diff` loop (see above) currently calls `materialise_blob` /
        // `ensure_blob_local` once per entry sequentially -- so today's path
        // can never observe concurrency > 1 no matter how
        // `OMEMFS_TRANSFER_CONCURRENCY` is set. Expected to FAIL at runtime
        // today (apply_diff already exists, so this compiles) until pull's
        // fetch path is changed to collect all missing blob hashes and issue
        // one batched `transfer_objects_many` call.
        if std::env::var("OMEMFS_TRANSFER_CONCURRENCY").is_ok() {
            return;
        }

        let remote_dir = TempDir::new().unwrap();
        let inner_remote = LocalStore::for_remote(remote_dir.path());
        let (diff, files) = build_added_blobs_diff(&inner_remote, 6);
        let remote = ConcurrencyTrackingStore::new(inner_remote, 4, Duration::from_millis(20));

        let store_dir = TempDir::new().unwrap();
        let store = LocalStore::for_cache(store_dir.path());
        let work_dir = TempDir::new().unwrap();

        let changed = apply_diff(
            &diff,
            work_dir.path(),
            &store,
            &store,
            work_dir.path(),
            0,
            &remote,
            None,
            &ScanObservation::new(HashMap::new()),
        )
        .unwrap();

        assert_eq!(changed, files.len());
        assert!(
            remote.max_in_flight.load(Ordering::SeqCst) >= 2,
            "pulling {} independent Added blobs should let the worker pool \
             overlap their fetches (max observed: {})",
            files.len(),
            remote.max_in_flight.load(Ordering::SeqCst)
        );
    }

    fn added(content: &[u8]) -> DiffEntry {
        DiffEntry::Added {
            hash: Hash::compute(content),
            size: content.len() as u64,
            mtime: None,
            mode: None,
        }
    }

    fn added_tree() -> DiffEntry {
        DiffEntry::AddedTree {
            hash: Hash::compute(b"tree"),
            size: 100,
            mtime: None,
            blob_count: 2,
        }
    }

    #[test]
    fn classify_conflicts_flags_added_tree_over_local_untracked_descendant() {
        let remote: HashMap<String, DiffEntry> = [("d".to_string(), added_tree())].into();
        let local: HashMap<String, DiffEntry> = [("d/x.txt".to_string(), added(b"local"))].into();
        let (clean, conflicts) = classify_conflicts(&remote, &local);
        assert_eq!(conflicts, vec!["d".to_string()]);
        assert!(clean.is_empty());
    }

    #[test]
    fn classify_conflicts_ignores_local_git_dir_under_added_tree() {
        // A pre-existing local git worktree at the AddedTree's path is the
        // design/08 "materialise inside a git worktree" case, not a conflict.
        let remote: HashMap<String, DiffEntry> = [("g".to_string(), added_tree())].into();
        let local: HashMap<String, DiffEntry> = [
            ("g/.git/HEAD".to_string(), added(b"ref")),
            ("g/.git/config".to_string(), added(b"cfg")),
        ]
        .into();
        let (clean, conflicts) = classify_conflicts(&remote, &local);
        assert!(conflicts.is_empty());
        assert!(clean.contains_key("g"));
    }

    #[test]
    fn classify_conflicts_flags_added_tree_when_local_has_file_beside_git_dir() {
        let remote: HashMap<String, DiffEntry> = [("g".to_string(), added_tree())].into();
        let local: HashMap<String, DiffEntry> = [
            ("g/.git/HEAD".to_string(), added(b"ref")),
            ("g/notes.txt".to_string(), added(b"local")),
        ]
        .into();
        let (_clean, conflicts) = classify_conflicts(&remote, &local);
        assert_eq!(conflicts, vec!["g".to_string()]);
    }

    #[test]
    fn classify_conflicts_does_not_treat_sibling_with_shared_name_prefix_as_related() {
        let remote: HashMap<String, DiffEntry> = [("d".to_string(), added_tree())].into();
        let local: HashMap<String, DiffEntry> = [("dx/y.txt".to_string(), added(b"local"))].into();
        let (clean, conflicts) = classify_conflicts(&remote, &local);
        assert!(conflicts.is_empty());
        assert!(clean.contains_key("d"));
    }

    #[test]
    fn classify_conflicts_keeps_same_content_exact_match_clean() {
        let remote: HashMap<String, DiffEntry> = [("f".to_string(), added(b"same"))].into();
        let local: HashMap<String, DiffEntry> = [("f".to_string(), added(b"same"))].into();
        let (clean, conflicts) = classify_conflicts(&remote, &local);
        assert!(conflicts.is_empty());
        assert!(clean.contains_key("f"));
    }
}
