use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use crate::{dlog_l1, dtimer_l1};

use filetime::FileTime;

use crate::codec;
use crate::codec::pack::reader::PackReader;
use crate::error::Error;
use crate::io_stats;
use crate::object::{Hash, Tree, TreeEntry};
use crate::repo::Repo;
use crate::scan::{refresh_stat_cache, scan_and_store_with_cache};
use crate::stat_cache::StatCache;
use crate::store::ObjectStore;
use crate::store::local::LocalStore;
use crate::store::stats::IoRecord;
use crate::stub::{self, StubRecord};
use crate::term::{Output, Styles, paint};
use crate::tree_ops;

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
        let omemfs_dir = repo.work_dir.join(".omemfs");
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
    crate::commands::push::post_clone_sync_guard(clone_root.as_ref(), index_root.is_some())?;

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
    let omemfs_dir = repo.work_dir.join(".omemfs");
    let stat_cache = StatCache::read(&omemfs_dir);
    let working_hash = {
        let phase = crate::progress::begin_phase("Scan working tree");
        let _t = dtimer_l1!("scan working tree");
        let scan_result = scan_and_store_with_cache(
            &repo.work_dir,
            &repo.work_dir,
            &local,
            clone_root_flat.as_ref(),
            &stat_cache,
            false,
        )?;
        let file_count = scan_result.files.len();
        refresh_stat_cache(stat_cache, &scan_result.files, &omemfs_dir);
        dlog_l1!(
            "working tree root: {}",
            &scan_result.root_hash.as_str()[..8]
        );
        phase.complete(format!("{} files", file_count));
        scan_result.root_hash
    };
    let local_diff = diff_trees(clone_root.as_ref(), Some(&working_hash), &lazy, 0)?;

    // Classify conflicts and build a clean (non-conflicting) diff.
    let (clean_diff, conflicts) = classify_conflicts(&remote_diff, &local_diff);

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

    // Atomic-abort policy (design/03, design/04): if ANY conflict is detected,
    // NOTHING is applied to the working tree. Write the conflict helper files,
    // leave the working tree otherwise untouched, do NOT update clone_root, and
    // exit non-zero. Conflicts must be detected BEFORE any working-tree mutation.
    if !conflicts.is_empty() {
        for path in &conflicts {
            let abs = repo.work_dir.join(path);
            write_conflict_for_path(
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
    )?;
    phase_apply.complete(format!("{} paths", changed));

    repo.write_clone_root(&remote_root)?;

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

        let preserved: Vec<&String> = local_diff
            .keys()
            .filter(|p| !remote_diff.contains_key(*p))
            .collect();
        if !preserved.is_empty() {
            out.writeln("Your local modifications to the following paths were preserved:")?;
            for p in preserved {
                let p_str = paint(colored, styles.modified, p);
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

    rels = crate::commands::push::deduplicate_paths(rels);

    let local = repo.local_store();
    let (pack_reader, _remote, remote_key) = repo.pack_reader(remote_name, Some(&io_record))?;

    let clone_root = repo.read_clone_root()?;
    let index_root = pack_reader.read_index_root()?;
    crate::commands::push::post_clone_sync_guard(clone_root.as_ref(), index_root.is_some())?;

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
    let omemfs_dir = repo.work_dir.join(".omemfs");
    // STAT_CACHE is read once in full (not per-path scoped, unlike push's
    // single-path `read_scoped`) and written back once at the end from the
    // union of every path's scan results (refactor-instructions.md Phase 8
    // step 4: preserve pull's existing "full read + per-path update +
    // write_if_dirty" semantics -- only the *scan* becomes per-path here).
    let stat_cache = StatCache::read(&omemfs_dir);
    let mut scanned_files: HashMap<String, crate::scan::ScannedFile> = HashMap::new();

    let mut path_diffs: Vec<PathDiff> = Vec::new();
    for rel in &rels {
        let components: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();

        // Both the remote_root and clone_root sides are read lazily (no skeleton
        // or path pre-download): navigate / diff / mark_deleted_tree go through
        // the read-through store, which fetches only the tree objects actually
        // traversed and only the subtrees that differ. Changed blob content is
        // fetched on demand in apply_diff.
        let remote_scoped_hash = tree_ops::navigate(&remote_root, &components, &lazy)?;
        let clone_scoped_hash = if let Some(ref cr) = clone_root {
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
        let working_scoped_hash = if scoped_abs.exists() {
            let phase = crate::progress::begin_phase("Scan working tree");
            let scan_result = scan_and_store_with_cache(
                &repo.work_dir,
                &scoped_abs,
                &local,
                clone_root_flat.as_ref(),
                &stat_cache,
                false,
            )?;
            let file_count = scan_result.files.len();
            scanned_files.extend(scan_result.files);
            phase.complete(format!("{} files", file_count));
            Some(scan_result.root_hash)
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
        });
    }
    refresh_stat_cache(stat_cache, &scanned_files, &omemfs_dir);

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
    // conflict_owner[path] maps each conflict path to its pd index.
    let mut pd_clean: Vec<HashMap<String, DiffEntry>> = vec![HashMap::new(); path_diffs.len()];
    let mut all_conflicts: Vec<String> = Vec::new();
    let mut conflict_owner: HashMap<String, usize> = HashMap::new();

    for (i, pd) in path_diffs.iter().enumerate() {
        let (clean, conflicts) = classify_conflicts(&pd.remote_diff, &pd.local_diff);
        pd_clean[i].extend(clean);
        for path in conflicts {
            conflict_owner.insert(path.clone(), i);
            all_conflicts.push(path);
        }
    }
    all_conflicts.sort();

    if !all_conflicts.is_empty() {
        // Pre-compute clone_scoped_hash once per pd that owns at least one conflict.
        let mut clone_scoped_by_pd: Vec<Option<Hash>> = vec![None; path_diffs.len()];
        for &i in conflict_owner.values() {
            if clone_scoped_by_pd[i].is_none()
                && let Some(ref cr) = clone_root
            {
                let comps: Vec<&str> = path_diffs[i]
                    .components
                    .iter()
                    .map(|s| s.as_str())
                    .collect();
                clone_scoped_by_pd[i] = tree_ops::navigate(cr, &comps, &lazy)?;
            }
        }
        // Write conflict helpers for each conflicting path.
        for path in &all_conflicts {
            let i = conflict_owner[path];
            let pd = &path_diffs[i];
            // The conflict path key is relative to the scoped subtree root, so it
            // must be joined under the scoped prefix (pd.rel), not the repo root.
            let abs = repo.work_dir.join(&pd.rel).join(path);
            let clone_scoped_hash = clone_scoped_by_pd[i].as_ref();
            write_conflict_for_path(
                &abs,
                path,
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
    {
        let phase = crate::progress::begin_phase("Apply changes");
        for (i, pd) in path_diffs.iter().enumerate() {
            let scoped_abs = repo.work_dir.join(&pd.rel);
            total_changed += apply_diff(
                &pd_clean[i],
                &scoped_abs,
                &local,
                &lazy,
                &repo.work_dir,
                stub_threshold,
                &pack_reader,
                remote_key.as_ref(),
            )?;
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
        new_clone_root = Some(splice_into_clone_root(
            new_clone_root.as_ref(),
            &comps,
            pd.remote_scoped_hash.as_ref(),
            &lazy,
        )?);
    }
    if let Some(ref cr) = new_clone_root {
        repo.write_clone_root(cr)?;
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
        out.finish()?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// One side (base / local / remote) of a conflict, resolved to a streamable
/// source instead of an in-memory byte buffer. Each variant is written to its
/// helper file chunk-by-chunk so no whole-blob buffer is ever held — for a
/// GB-class conflicted file this keeps peak memory bounded at roughly one chunk
/// (≤ CDC_MAX ≈ 16 MiB) per side rather than the full file size.
enum HelperSource {
    /// A blob in the local object store, addressed by its logical hash. Streamed
    /// to the helper file via `codec::chunk::materialise_to_file`.
    Blob(Hash),
    /// A working-tree file held open at construction time. Holding the `File`
    /// rather than a path avoids re-opening and prevents TOCTOU races: if the
    /// file is deleted after `working_file_source` opens it the fd remains valid
    /// on Linux, matching the old in-memory-bytes outcome. The variant stores an
    /// `Option` so `write_helper_file` can take the file out of a shared
    /// reference via `std::mem::replace`.
    WorkingFile(fs::File),
}

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
    use crate::commands::conflict::{ConflictMeta, SideMeta};
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
    ConflictMeta { base, remote }
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

/// Write `.omemfs-conflict-{base,local,remote}` helper files alongside `path`.
/// Skips a side when its source is None (the path did not exist on that side).
/// Each side is streamed to its helper file, so no whole-blob buffer is held.
fn write_conflict_helpers(
    path: &std::path::Path,
    base: Option<&HelperSource>,
    local: Option<&HelperSource>,
    remote: Option<&HelperSource>,
    store: &dyn ObjectStore,
) -> Result<(), Error> {
    use crate::commands::conflict::{
        CONFLICT_SUFFIX_BASE, CONFLICT_SUFFIX_LOCAL, CONFLICT_SUFFIX_REMOTE,
    };
    if let Some(src) = base {
        write_helper_file(path, CONFLICT_SUFFIX_BASE, src, store)?;
    }
    if let Some(src) = local {
        write_helper_file(path, CONFLICT_SUFFIX_LOCAL, src, store)?;
    }
    if let Some(src) = remote {
        write_helper_file(path, CONFLICT_SUFFIX_REMOTE, src, store)?;
    }
    Ok(())
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
    write_conflict_helpers(
        abs,
        base_src.as_ref(),
        local_src.as_ref(),
        remote_src.as_ref(),
        lazy,
    )?;
    // Record the base/remote tracked metadata so `accept` can restore it.
    let base_entry = match base_hash {
        Some(h) => {
            let comps: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
            tree_ops::navigate_entry(h, &comps, lazy)?
        }
        None => None,
    };
    build_conflict_meta(base_entry.as_ref(), remote_diff.get(path)).write(abs)?;
    Ok(())
}

/// Stream `source` into the helper file `<base_path><suffix>`.
///
/// The helper path is built and the parent directory is ensured exactly as
/// before; the only change is how bytes flow:
///   - `Blob(hash)`: `materialise_to_file` streams the (possibly chunked) blob
///     chunk-by-chunk into a temp file in the destination dir, then atomically
///     renames it. This matches the old behaviour: the helper was a plain file
///     overwrite with deserialised blob content (the `ED F0` blob-escape is
///     stripped from the first chunk, exactly as `deserialise_blob` did).
///   - `WorkingFile(path)`: `io::copy` streams the working-tree file into the
///     same atomic temp+rename writer, never reading it whole.
fn write_helper_file(
    base_path: &std::path::Path,
    suffix: &str,
    source: &HelperSource,
    store: &dyn ObjectStore,
) -> Result<(), Error> {
    let helper_path = {
        let s = base_path.to_string_lossy();
        std::path::PathBuf::from(format!("{}{}", s, suffix))
    };
    if let Some(parent) = helper_path.parent() {
        fs::create_dir_all(parent)?;
    }
    match source {
        HelperSource::Blob(hash) => {
            codec::chunk::materialise_to_file(store, hash, None, &helper_path)?;
        }
        HelperSource::WorkingFile(file) => {
            // `&File` implements `Read`, so `io::copy` works without consuming or
            // mutably borrowing through the enum reference.
            crate::store::local::atomic_write_with_no_fsync(&helper_path, |writer| {
                std::io::copy(&mut &*file, writer)
                    .map(|_| ())
                    .map_err(Error::Io)
            })?;
        }
    }
    Ok(())
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
        } else {
            clean.insert(path.clone(), remote_entry.clone());
        }
    }
    conflicts.sort();
    (clean, conflicts)
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

fn apply_diff(
    diff: &HashMap<String, DiffEntry>,
    base_dir: &std::path::Path,
    local: &LocalStore,
    store: &dyn ObjectStore,
    work_dir: &std::path::Path,
    stub_threshold: u64,
    remote: &dyn ObjectStore,
    remote_key: Option<&crate::codec::encrypt::EncryptKey>,
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
                        .map(|p| p.to_string_lossy().replace('\\', "/"))
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
        crate::commands::push::transfer_objects_many(remote, local, &pending, remote_key, true)?;
    }

    // Directories touched by deletions, for dir-stub / empty-parent cleanup.
    let mut deleted_dirs: std::collections::HashSet<std::path::PathBuf> =
        std::collections::HashSet::new();
    for (rel_path, change) in diff {
        let abs_path = base_dir.join(rel_path);
        // Compute the path relative to work_dir (correct even for scoped pulls where
        // base_dir != work_dir, so stub operations target the right location).
        let work_rel = abs_path
            .strip_prefix(work_dir)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| rel_path.to_string());
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
                        fs::remove_file(&abs_path)?;
                    }
                } else {
                    materialise_blob(
                        &abs_path, work_dir, &work_rel, hash, mtime, mode, store, remote,
                        remote_key,
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
                        remote_key,
                    )?;
                }
                changed += 1;
            }
            DiffEntry::AddedEmptyDir => {
                fs::create_dir_all(&abs_path)?;
                changed += 1;
            }
            DiffEntry::Symlink { target, mtime, .. } => {
                // A symlink is always materialised (design/08). Remove any stale
                // stub and any existing entry (file, symlink, or empty dir) at the
                // path, then create the symlink. The existing entry is removed
                // without following it.
                stub::remove(work_dir, &work_rel)?;
                // write_symlink_atomic ensures the parent directory exists.
                crate::fsmeta::write_symlink_atomic(&abs_path, target)?;
                // Restore the link's own mtime (lutimes) so a subsequent scan
                // recomputes the same tree and push stays a no-op.
                crate::fsmeta::restore_symlink_mtime(&abs_path, mtime);
                changed += 1;
            }
            DiffEntry::Deleted => {
                if abs_path.exists() {
                    fs::remove_file(&abs_path)?;
                    changed += 1;
                }
                // Remove stub if it existed.
                stub::remove(work_dir, &work_rel)?;
                if let Some(parent) = abs_path.parent() {
                    deleted_dirs.insert(parent.to_path_buf());
                }
            }
            DiffEntry::AddedTree {
                hash,
                size,
                mtime,
                blob_count,
            } => {
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
                    materialise_tree(hash, &abs_path, store, remote, remote_key)?;
                } else {
                    fs::create_dir_all(&abs_path)?;
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

    // Clean up directory stubs and empty directories left behind by deletions.
    // When the remote deletes a path that is (or is under) a local directory
    // stub, the per-blob Deleted entries never match a real file, so the
    // leftover `.omemfs-stub` marker would re-introduce the deleted subtree on
    // the next push (the "deleted tree resurrection" bug). Remove the marker
    // and prune now-empty parent directories (design/08 reconcile: "absent on
    // remote → remove stub; remove directory if empty").
    //
    // Walk each deleted directory and all of its ancestors up to (but not
    // including) work_dir, removing dir stubs that no longer have logical
    // content and pruning empty directories.
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
        let dir_stub = dir.join(stub::DIR_STUB_NAME);
        // Remove a directory stub marker whose subtree is now fully deleted
        // (only the marker remains inside the directory).
        if dir_stub.is_file() {
            let only_marker = fs::read_dir(&dir)
                .map(|rd| {
                    !rd.flatten()
                        .any(|e| e.file_name().to_string_lossy() != stub::DIR_STUB_NAME)
                })
                .unwrap_or(false);
            if only_marker {
                fs::remove_file(&dir_stub).ok();
            }
        }
        // Prune the directory if it is now empty.
        if fs::read_dir(&dir)
            .map(|mut rd| rd.next().is_none())
            .unwrap_or(false)
        {
            fs::remove_dir(&dir).ok();
        }
    }

    Ok(changed)
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
) -> Result<(), Error> {
    ensure_blob_local(remote, store, hash, remote_key)?;
    if let Some(parent) = abs_path.parent() {
        fs::create_dir_all(parent)?;
    }
    crate::fsmeta::materialise_blob_at(store, hash, abs_path, mtime, mode)?;
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
        let p_str = paint(colored, styles.modified, p);
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

/// A read-through object store over the local cache that fetches a single
/// missing object from the remote (via the pack reader) on a local miss.
///
/// After a lazy, stub-aware clone the local cache does not contain the
/// clone_root tree objects of stubbed subtrees. Pull reads clone_root tree
/// objects in the diff, in `navigate`, in `mark_deleted_tree`, and when
/// resolving a conflict base. Routing those reads through this store fetches
/// only the tree objects actually traversed: the diff compares two subtree
/// hashes before reading either tree and skips equal-hash subtrees, so a
/// clone_root read served here fetches only the subtrees that differ from the
/// remote root. This replaces the previous eager full-skeleton pre-download.
///
/// The pack reader returns still-encrypted bytes on a remote hit, so a miss is
/// decoded with `remote_key` and re-stored in the local cache as plaintext
/// (the local cache is never encrypted). All subsequent reads of the object are
/// served locally. Reads are routed through `codec::store_read(.., None)` by
/// callers, which is correct for both the local-hit (plaintext) path and the
/// freshly-cached plaintext returned here.
struct LazyTreeStore<'a> {
    local: &'a LocalStore,
    pack_reader: &'a PackReader,
    remote_key: Option<&'a crate::codec::encrypt::EncryptKey>,
}

impl<'a> LazyTreeStore<'a> {
    fn new(
        local: &'a LocalStore,
        pack_reader: &'a PackReader,
        remote_key: Option<&'a crate::codec::encrypt::EncryptKey>,
    ) -> Self {
        LazyTreeStore {
            local,
            pack_reader,
            remote_key,
        }
    }

    /// Ensure `hash` is present in the local cache as plaintext, fetching it
    /// from the remote (decoding with `remote_key`) on a miss.
    fn ensure_local(&self, hash: &Hash) -> Result<(), Error> {
        if self.local.exists(hash)? {
            return Ok(());
        }
        let plaintext = codec::store_read(self.pack_reader, hash, self.remote_key)?;
        codec::store_write(self.local, hash, &plaintext, None)?;
        Ok(())
    }
}

impl ObjectStore for LazyTreeStore<'_> {
    fn exists(&self, hash: &Hash) -> Result<bool, Error> {
        if self.local.exists(hash)? {
            return Ok(true);
        }
        self.pack_reader.exists(hash)
    }

    fn size(&self, hash: &Hash) -> Result<u64, Error> {
        self.ensure_local(hash)?;
        self.local.size(hash)
    }

    fn list_with_sizes(&self) -> Result<Vec<(String, u64)>, Error> {
        self.local.list_with_sizes()
    }

    fn open_read(&self, hash: &Hash) -> Result<Box<dyn std::io::Read>, Error> {
        self.ensure_local(hash)?;
        self.local.open_read(hash)
    }

    fn write_from(&self, hash: &Hash, reader: &mut dyn std::io::Read) -> Result<(), Error> {
        self.local.write_from(hash, reader)
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
        crate::commands::push::transfer_objects(remote, local, hash, remote_key, true)?;
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
fn tree_contains_git_dir(store: &dyn ObjectStore, hash: &Hash) -> bool {
    let Ok(data) = codec::store_read(store, hash, None) else {
        return false;
    };
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
    store: &dyn ObjectStore,
    remote: &dyn ObjectStore,
    remote_key: Option<&crate::codec::encrypt::EncryptKey>,
) -> Result<(), Error> {
    fs::create_dir_all(dir)?;
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
                crate::fsmeta::materialise_blob_at(store, hash, &abs, mtime, mode)?;
            }
            TreeEntry::Tree {
                name, hash, mtime, ..
            } => {
                let sub = dir.join(name);
                materialise_tree(hash, &sub, store, remote, remote_key)?;
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
                    crate::fsmeta::write_symlink_atomic(&link_path, target)?;
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
fn diff_trees(
    base: Option<&Hash>,
    target: Option<&Hash>,
    store: &dyn ObjectStore,
    stub_threshold: u64,
) -> Result<HashMap<String, DiffEntry>, Error> {
    let mut result = HashMap::new();
    if let Some(t) = target {
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
                    // Entry kind changed (was a tree or symlink): report as modified.
                    Some(_) => {
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
                    // an empty base so all descendants are reported as added.
                    _ => None,
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
                // not inside a git working tree (where stub files are not allowed).
                let should_stub = base_tree_hash.is_none()
                    && stub_threshold > 0
                    && *size >= stub_threshold
                    && !child_in_git;

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
                    // Kind changed (was a blob or tree): replace with a symlink.
                    Some(_) => {
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

    /// Deterministic pseudo-random byte generator (xorshift64*), enough for
    /// FastCDC to find multiple cut points. Not for crypto.
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

    #[test]
    fn write_helper_file_streams_blob_content() {
        // A multi-chunk blob must be written to its helper file with byte-identical
        // content (ED F0 escape stripped exactly once, no chunk tags leaking).
        let content = pseudo_random(0x4242_1717, 10 * 1024 * 1024);
        let store_dir = TempDir::new().unwrap();
        let store = LocalStore::for_cache(store_dir.path());
        let hash = crate::object::blob_hash(&content);
        let serialised = crate::object::serialise_blob(&content);
        codec::chunk::store_chunked(&store, &hash, &serialised, None).unwrap();
        assert!(
            codec::chunk::is_chunked(&store, &hash, None),
            "expected multi-chunk blob"
        );

        let out_dir = TempDir::new().unwrap();
        let base = out_dir.path().join("file.bin");
        write_helper_file(
            &base,
            ".omemfs-conflict-remote",
            &HelperSource::Blob(hash),
            &store,
        )
        .unwrap();

        let helper = out_dir.path().join("file.bin.omemfs-conflict-remote");
        assert_eq!(fs::read(&helper).unwrap(), content);
    }

    #[test]
    fn write_helper_file_streams_working_file() {
        // The local side stream-copies the working-tree file verbatim.
        let content = pseudo_random(0x9999_0001, 3 * 1024 * 1024);
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("file.bin");
        fs::write(&src, &content).unwrap();

        // The store argument is unused for a WorkingFile source.
        let store_dir = TempDir::new().unwrap();
        let store = LocalStore::for_cache(store_dir.path());
        let file = fs::File::open(&src).unwrap();
        write_helper_file(
            &src,
            ".omemfs-conflict-local",
            &HelperSource::WorkingFile(file),
            &store,
        )
        .unwrap();

        let helper = dir.path().join("file.bin.omemfs-conflict-local");
        assert_eq!(fs::read(&helper).unwrap(), content);
    }

    #[test]
    fn write_helper_file_overwrites_existing() {
        // A pre-existing helper file is overwritten (atomic temp+rename), matching
        // the old fs::write behaviour.
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("file.bin");
        fs::write(&src, b"new content").unwrap();
        let helper = dir.path().join("file.bin.omemfs-conflict-local");
        fs::write(&helper, b"stale helper content that is longer").unwrap();

        let store_dir = TempDir::new().unwrap();
        let store = LocalStore::for_cache(store_dir.path());
        let file = fs::File::open(&src).unwrap();
        write_helper_file(
            &src,
            ".omemfs-conflict-local",
            &HelperSource::WorkingFile(file),
            &store,
        )
        .unwrap();
        assert_eq!(fs::read(&helper).unwrap(), b"new content");
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
}
