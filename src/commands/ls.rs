use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use chrono::{DateTime, Utc};

use crate::codec;
use crate::codec::pack::reader::PackReader;
use crate::dtimer_l1;
use crate::error::Error;
use crate::object::{Hash, Tree, TreeEntry};
use crate::repo::Repo;
use crate::scan::{
    ScanResult, ScanSideData, ScannedFile, refresh_stat_cache, scan_and_store_with_cache,
};
use crate::stat_cache::StatCache;
use crate::store::ObjectStore;
use crate::term::{Output, Styles, paint};

/// Which tree (remote root / clone root / working tree) provides the
/// hash, size, blob_count, and mtime columns.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LsSource {
    Clone,
    Remote,
    Working,
}

pub struct LsOptions {
    pub work_dir: PathBuf,
    /// Directory the command was invoked from; relative paths resolve against it.
    pub current_dir: PathBuf,
    pub paths: Vec<PathBuf>,
    pub recursive: bool,
    pub full_hash: bool,
    pub dirty: bool,
    pub no_remote: bool,
    pub source: LsSource,
}

// ---------------------------------------------------------------------------
// Row — one output record before column widths are known
// ---------------------------------------------------------------------------

struct Row {
    /// Local status (`X` column): `M`/`A`/`D`/` ` (see `status_char`), or
    /// [`STATUS_UNKNOWN`] (`?`) when a clone-root tree object needed to
    /// determine this path's status was unreadable (design/04_cli_spec.md
    /// "Local diff self-healing").
    status: char,
    /// Remote status: `M` (modified), `A` (added on remote), `D` (deleted on remote), or ` ` (in sync).
    remote_status: char,
    /// Stub/conflict column `Z`: `!` conflict, `S` stub, `s` indirect stub, ` ` normal.
    z: char,
    hash_str: String,
    size: Option<u64>,
    blob_count: u64,
    mtime: Option<DateTime<Utc>>,
    path: String,
}

pub fn run(mut opts: LsOptions) -> Result<(), Error> {
    // --remote and --no-remote are mutually exclusive.
    if opts.source == LsSource::Remote && opts.no_remote {
        return Err(Error::Other(
            "--remote and --no-remote cannot be used together".to_string(),
        ));
    }

    let repo = Repo::open(&opts.work_dir)?;
    crate::progress::notify_repo_dir(&repo.work_dir);

    // Resolve every <path> against the cwd and re-express it as a repo-root-
    // anchored absolute path, so all downstream logic (which strips work_dir or
    // joins onto it) operates unchanged regardless of which subdirectory ls was
    // run from. When no <path> is given, the default scope is the cwd: at the
    // repo root this is the empty (whole-tree) scope; from a subdirectory it
    // scopes to that subdirectory (design/04 ls Arguments). An explicit "." or
    // the root itself normalises to "" and is dropped so the whole tree is shown.
    {
        let raw_paths = if opts.paths.is_empty() {
            vec![opts.current_dir.clone()]
        } else {
            std::mem::take(&mut opts.paths)
        };
        let mut anchored: Vec<PathBuf> = Vec::with_capacity(raw_paths.len());
        for p in &raw_paths {
            let rel = crate::repo::normalize_path(p, &opts.work_dir, &opts.current_dir)?;
            if rel.is_empty() {
                // Whole-tree scope: represented as no path argument downstream.
                continue;
            }
            anchored.push(opts.work_dir.join(rel));
        }
        opts.paths = anchored;
    }
    let phase = crate::progress::begin_phase("List");
    let _t = dtimer_l1!("ls");
    // Enable the in-process tree-entry cache for this command: the scan phase
    // builds the working tree and the listing/diff phases re-read the same tree
    // objects. Caching them avoids a disk read + decompress per tree.
    let _tree_cache = crate::tree_ops::TreeCacheGuard::enable();
    let local = repo.local_store();

    let clone_root = repo.read_clone_root()?;

    // --dirty: show only diff between working tree and clone root.
    if opts.dirty {
        let omemfs_dir_dirty = repo.work_dir.join(".omemfs");
        // --dirty honours the cwd / <path> scope just like a plain ls: from a
        // subdirectory (or with a <path> argument) only changes under that scope
        // are listed. Use the scoped working-tree scan so out-of-scope subtrees
        // are not walked, and a scoped STAT_CACHE load for a single path. Computed
        // up front so both the flatten below and the STAT_CACHE load can share it.
        let stat_scope_dirty: Option<String> = single_scope_prefix(&opts.work_dir, &opts.paths);
        // Pass clone_root_flat for mtime stability: without it, mtime-only drift
        // produces a different working tree hash (defeating the early-exit check)
        // and writes new tree objects to the local store on every run. For a
        // single-path scope this is built from only that path's clone-root
        // subtree (design/04 "Scoped working-tree scan", design/03 "Path-scoped
        // push"), instead of flattening the whole clone root; multi-path and
        // unscoped ls fall back to the full flatten. A missing map entry is safe
        // (the scan simply re-hashes the file), so an absent/empty map never
        // causes incorrect behaviour, only a cache miss.
        let clone_root_flat_dirty =
            clone_root
                .as_ref()
                .and_then(|h| match stat_scope_dirty.as_deref() {
                    Some(prefix) => {
                        crate::tree_ops::flatten_tree_entries_scoped(h, prefix, &local).ok()
                    }
                    None => crate::tree_ops::flatten_tree_entries(h, &local).ok(),
                });
        let stat_cache_dirty = match stat_scope_dirty.as_deref() {
            Some(prefix) => StatCache::read_scoped(&omemfs_dir_dirty, prefix),
            None => StatCache::read(&omemfs_dir_dirty),
        };
        // ls is read-only: hash files but skip blob writes (design/03
        // "Scan blob-write mode"). Tree objects are still written for diffing.
        let scan_result_dirty = scan_scoped_for_ls(
            &opts.work_dir,
            &opts.paths,
            clone_root.as_ref(),
            clone_root_flat_dirty.as_ref(),
            &local,
            &stat_cache_dirty,
        )?;
        match stat_scope_dirty.as_deref() {
            Some(prefix) => crate::scan::refresh_stat_cache_scoped(
                stat_cache_dirty,
                &scan_result_dirty.files,
                &omemfs_dir_dirty,
                prefix,
            ),
            None => refresh_stat_cache(
                stat_cache_dirty,
                &scan_result_dirty.files,
                &omemfs_dir_dirty,
            ),
        }
        let working_hash = scan_result_dirty.root_hash;
        if clone_root.as_ref() == Some(&working_hash) {
            phase.complete("0 entries");
            return Ok(());
        }

        // Restrict the diff to the requested scope (empty = whole tree).
        let scoped_prefixes_dirty: Vec<String> = opts
            .paths
            .iter()
            .map(|p| {
                let rel = p.strip_prefix(&opts.work_dir).unwrap_or(p);
                rel.to_string_lossy().replace('\\', "/")
            })
            .collect();
        let scope_dirty = ScopeFilter::new(&scoped_prefixes_dirty);
        let (diff, unknown_dirty) = diff_trees_with_heal(
            &repo,
            clone_root.as_ref(),
            working_hash,
            &local,
            &scope_dirty,
            &scoped_prefixes_dirty,
        )?;
        let mut paths: Vec<&String> = diff.keys().collect();
        paths.sort();
        let mut rows: Vec<Row> = Vec::new();
        for path in paths {
            let row = match &diff[path] {
                DiffRow::Added { working_hash } => Row {
                    status: 'A',
                    remote_status: ' ',
                    z: ' ',
                    hash_str: format_hash(working_hash, opts.full_hash),
                    size: None,
                    blob_count: 1,
                    mtime: None,
                    path: path.clone(),
                },
                DiffRow::AddedEmptyDir { hash } => Row {
                    status: 'A',
                    remote_status: ' ',
                    z: ' ',
                    hash_str: format_hash(hash, opts.full_hash),
                    size: Some(0),
                    blob_count: 0,
                    mtime: None,
                    path: format!("{}/", path),
                },
                DiffRow::Modified { working_hash } => Row {
                    status: 'M',
                    remote_status: ' ',
                    z: ' ',
                    hash_str: format_hash(working_hash, opts.full_hash),
                    size: None,
                    blob_count: 1,
                    mtime: None,
                    path: path.clone(),
                },
                DiffRow::Deleted => Row {
                    status: 'D',
                    remote_status: ' ',
                    z: ' ',
                    hash_str: "-".to_string(),
                    size: None,
                    blob_count: 0,
                    mtime: None,
                    path: path.clone(),
                },
            };
            rows.push(row);
        }
        // A subtree whose diff could not be resolved (see diff_trees_with_heal)
        // has no entry in `diff`, so it would otherwise be silently absent from
        // `--dirty` output even though its dirty status is genuinely unknown.
        // Synthesize a row for it with STATUS_UNKNOWN rather than omitting it.
        for up in &unknown_dirty {
            rows.push(Row {
                status: STATUS_UNKNOWN,
                remote_status: ' ',
                z: ' ',
                hash_str: "-".to_string(),
                size: None,
                blob_count: 0,
                mtime: None,
                path: if up == "." {
                    ".".to_string()
                } else {
                    format!("{}/", up)
                },
            });
        }
        rows.sort_by(|a, b| a.path.cmp(&b.path));
        let count = rows.len();
        print_rows(&rows)?;
        phase.complete(format!("{} entries", count));
        return Ok(());
    }

    // Regular ls: list entries combining clone root state and working tree diff.
    // Computed up front so both the flatten below and the STAT_CACHE load can
    // share it (design/07 "Read optimisation").
    let stat_scope: Option<String> = single_scope_prefix(&opts.work_dir, &opts.paths);
    // Pass clone_root_flat for mtime stability so wt_mtime matches what push
    // would record. For a single-path scope this is built from only that path's
    // clone-root subtree (design/04 "Scoped working-tree scan", design/03
    // "Path-scoped push") instead of flattening the whole clone root; multi-path
    // and unscoped ls fall back to the full flatten. A missing map entry is safe
    // (the scan simply re-hashes the file), so an absent/empty map never causes
    // incorrect behaviour, only a cache miss.
    let clone_root_flat = clone_root
        .as_ref()
        .and_then(|h| match stat_scope.as_deref() {
            Some(prefix) => crate::tree_ops::flatten_tree_entries_scoped(h, prefix, &local).ok(),
            None => crate::tree_ops::flatten_tree_entries(h, &local).ok(),
        });
    let omemfs_dir = repo.work_dir.join(".omemfs");

    // STAT_CACHE scope-limited load (design/07 "Read optimisation"): for a single
    // <path>, parse only that path's slice of STAT_CACHE rather than the whole
    // file (which can be many MB on a large repository). Multiple paths fall back
    // to a full read, as scoped push does; an unscoped ls also reads in full.
    let stat_cache = match stat_scope.as_deref() {
        Some(prefix) => StatCache::read_scoped(&omemfs_dir, prefix),
        None => StatCache::read(&omemfs_dir),
    };

    // ls is read-only: hash files but skip blob writes (design/03 "Scan
    // blob-write mode"). Tree objects are still written so diffing can read them.
    //
    // Scoped working-tree scan (design/04 "Scoped working-tree scan"): when
    // <path> arguments are given, scan only those subtrees instead of the whole
    // working tree, then splice each scanned subtree onto the clone root (or an
    // empty tree when none exists) to reconstruct a full working-tree root hash.
    // Out-of-scope paths reuse the clone-root tree objects unchanged and incur
    // no working-tree I/O. With no <path>, the full working tree is scanned.
    let scan_result = scan_scoped_for_ls(
        &opts.work_dir,
        &opts.paths,
        clone_root.as_ref(),
        clone_root_flat.as_ref(),
        &local,
        &stat_cache,
    )?;
    let working_hash = scan_result.root_hash.clone();
    // Refresh STAT_CACHE with newly-hashed files (best-effort; ignore write errors).
    // A scoped load must write back via the scoped merge so out-of-scope entries
    // survive byte-for-byte (design/07).
    match stat_scope.as_deref() {
        Some(prefix) => crate::scan::refresh_stat_cache_scoped(
            stat_cache,
            &scan_result.files,
            &omemfs_dir,
            prefix,
        ),
        None => refresh_stat_cache(stat_cache, &scan_result.files, &omemfs_dir),
    }

    let target_paths: Vec<PathBuf> = if opts.paths.is_empty() {
        vec![opts.work_dir.clone()]
    } else {
        opts.paths
            .iter()
            .map(|p| {
                if p.is_absolute() {
                    p.clone()
                } else {
                    opts.work_dir.join(p)
                }
            })
            .collect()
    };

    // Collect relative path prefixes for the scoped paths (empty string = root).
    let scoped_prefixes: Vec<String> = if opts.paths.is_empty() {
        vec![]
    } else {
        target_paths
            .iter()
            .map(|t| {
                let rel = t.strip_prefix(&opts.work_dir).unwrap_or(t);
                rel.to_string_lossy().replace('\\', "/")
            })
            .collect()
    };

    // Scope filter used to limit diff computation to the requested paths.
    // When no clone root exists yet AND no path scope is given, keep the diff
    // unscoped because the no-clone-root branch below lists top-level entries
    // from the full diff. With a path scope, the scope is honoured even without
    // a clone root: the scoped working_hash already restricts the diff to the
    // requested subtrees, and the listing falls through to the scoped branch.
    let scope = if clone_root.is_some() || !opts.paths.is_empty() {
        ScopeFilter::new(&scoped_prefixes)
    } else {
        ScopeFilter::unscoped()
    };

    let (diff_map, unknown_paths): (HashMap<String, DiffRow>, HashSet<String>) =
        if clone_root.as_ref() != Some(&working_hash) {
            diff_trees_with_heal(
                &repo,
                clone_root.as_ref(),
                working_hash.clone(),
                &local,
                &scope,
                &scoped_prefixes,
            )?
        } else {
            (HashMap::new(), HashSet::new())
        };

    // The no-clone-root top-level listing applies only to an unscoped ls. With a
    // path scope, fall through to the scoped branch using the working_hash
    // (built by splicing the scanned subtrees onto an empty base) as the root.
    let root_hash = match clone_root.clone().or_else(|| {
        if opts.paths.is_empty() {
            None
        } else {
            Some(working_hash.clone())
        }
    }) {
        None => {
            // No clone root yet — show working tree files as added.
            let mut entries: Vec<(&String, &DiffRow)> = diff_map.iter().collect();
            entries.sort_by_key(|(p, _)| p.as_str());
            let mut rows: Vec<Row> = Vec::new();
            // Track directories already emitted (non-recursive mode).
            let mut emitted_dirs: HashSet<String> = HashSet::new();
            for (path, row) in &entries {
                if opts.recursive {
                    match row {
                        DiffRow::Added { working_hash: wh } => {
                            rows.push(Row {
                                status: 'A',
                                remote_status: ' ',
                                z: ' ',
                                hash_str: format_hash(wh, opts.full_hash),
                                size: None,
                                blob_count: 1,
                                mtime: None,
                                path: path.to_string(),
                            });
                        }
                        DiffRow::AddedEmptyDir { hash } => {
                            rows.push(Row {
                                status: 'A',
                                remote_status: ' ',
                                z: ' ',
                                hash_str: format_hash(hash, opts.full_hash),
                                size: Some(0),
                                blob_count: 0,
                                mtime: None,
                                path: format!("{}/", path),
                            });
                        }
                        _ => {}
                    }
                } else if is_direct_child(path, "") {
                    // Direct child file or empty dir.
                    match row {
                        DiffRow::Added { working_hash: wh } => {
                            rows.push(Row {
                                status: 'A',
                                remote_status: ' ',
                                z: ' ',
                                hash_str: format_hash(wh, opts.full_hash),
                                size: None,
                                blob_count: 1,
                                mtime: None,
                                path: path.to_string(),
                            });
                        }
                        DiffRow::AddedEmptyDir { hash } => {
                            rows.push(Row {
                                status: 'A',
                                remote_status: ' ',
                                z: ' ',
                                hash_str: format_hash(hash, opts.full_hash),
                                size: Some(0),
                                blob_count: 0,
                                mtime: None,
                                path: format!("{}/", path),
                            });
                        }
                        _ => {}
                    }
                } else {
                    // Deeper path — emit its top-level directory once.
                    let dir_name = path.split('/').next().unwrap_or("");
                    if !dir_name.is_empty() && !emitted_dirs.contains(dir_name) {
                        emitted_dirs.insert(dir_name.to_string());
                        rows.push(Row {
                            status: 'A',
                            remote_status: ' ',
                            z: ' ',
                            hash_str: "-".to_string(),
                            size: None,
                            blob_count: 0,
                            mtime: None,
                            path: format!("{}/", dir_name),
                        });
                    }
                }
            }
            let count = rows.len();
            print_rows(&rows)?;
            phase.complete(format!("{} entries", count));
            return Ok(());
        }
        Some(h) => h,
    };

    // Compute the map of paths that differ between clone_root and remote_root.
    // On any failure (remote not configured, timeout, error) all maps are empty
    // and the R column shows spaces for all entries.
    // remote_entries: path → TreeEntry for ALL changed paths (A and M) in the remote root.
    let (remote_status_map, remote_added_entries, remote_entries): (
        HashMap<String, char>,
        HashMap<String, TreeEntry>,
        HashMap<String, TreeEntry>,
    ) = if opts.no_remote {
        (HashMap::new(), HashMap::new(), HashMap::new())
    } else {
        fetch_remote_status_map(&repo, &root_hash, &local, &scope)
    };

    let mut rows: Vec<Row> = Vec::new();

    for target in &target_paths {
        let rel = target.strip_prefix(&opts.work_dir).unwrap_or(target);
        let rel_str = rel.to_string_lossy();

        let parts: Vec<&str> = rel_str.split('/').filter(|s| !s.is_empty()).collect();
        // Aggregate metadata (size, blob_count, mtime) for the directory self-row.
        // Populated for tree targets; None for blobs or the root (handled separately).
        let mut self_dir_meta: Option<(u64, u64, Option<DateTime<Utc>>)> = None;
        // found_in_clone: true when the path was resolved from clone root (not working tree).
        let (hash, is_blob, found_in_clone) = if parts.is_empty() {
            (root_hash.clone(), false, true)
        } else {
            // Resolve the target entry. The clone root always wins; otherwise
            // the precedence between a remote-only added entry and the working
            // tree depends on the source: --working prefers the working tree,
            // while the default clone view prefers the remote-added entry and
            // falls back to the working tree so a locally-created path (not yet
            // pushed) is still listable. `in_clone` records whether the entry
            // came from the clone root, which later forces status 'A' on
            // working-only paths.
            // Navigate the *actual* clone root for the in_clone decision. When
            // there is no clone root (root_hash is the spliced working_hash),
            // the path must not be treated as clone-resolved: it is working-tree
            // only and should be forced to status 'A'.
            let clone_entry = match clone_root.as_ref() {
                Some(_) => crate::tree_ops::navigate_entry(&root_hash, &parts, &local)?,
                None => None,
            };
            let (entry, in_clone): (Option<TreeEntry>, bool) = if clone_entry.is_some() {
                (clone_entry, true)
            } else if opts.source == LsSource::Working {
                match navigate_working(&working_hash, &parts, &local) {
                    Some(we) => (Some(we), false),
                    None => (lookup_remote_added(&remote_added_entries, &rel_str), false),
                }
            } else {
                match lookup_remote_added(&remote_added_entries, &rel_str) {
                    Some(re) => (Some(re), false),
                    None => (navigate_working(&working_hash, &parts, &local), false),
                }
            };
            let entry =
                entry.ok_or_else(|| Error::Other(format!("path not found: {}", rel_str)))?;
            let (h, is_blob, meta) = resolve_entry_meta(&entry, &rel_str)?;
            self_dir_meta = meta;
            (h, is_blob, in_clone)
        };
        // When the path was resolved from the working tree (not clone root), its
        // contents are entirely new — force status 'A' on all descendants.
        let scoped_force_status = if !found_in_clone { Some('A') } else { None };
        if is_blob {
            let status = scoped_force_status.unwrap_or_else(|| status_char(&rel_str, &diff_map));
            let rs = remote_status_map
                .get(rel_str.as_ref())
                .copied()
                .unwrap_or(' ');
            rows.push(Row {
                status,
                remote_status: rs,
                z: ' ',
                hash_str: format_hash(&hash, opts.full_hash),
                size: None,
                blob_count: 1,
                mtime: None,
                path: rel_str.to_string(),
            });
        } else {
            // Emit a self-row for the target directory itself before its children.
            if parts.is_empty() {
                // Root self-row: aggregate stats by reading root tree entries once.
                let (root_mt, root_sz, root_bc) =
                    crate::tree_ops::tree_meta(&root_hash, &local).unwrap_or((None, 0, 0));
                let root_status = if !diff_map.is_empty() { 'M' } else { ' ' };
                let root_r = if !remote_status_map.is_empty() {
                    'M'
                } else {
                    ' '
                };
                rows.push(Row {
                    status: root_status,
                    remote_status: root_r,
                    z: ' ',
                    hash_str: format_hash(&root_hash, opts.full_hash),
                    size: Some(root_sz),
                    blob_count: root_bc,
                    mtime: root_mt,
                    path: ".".to_string(),
                });
            } else if let Some((sz, bc, mt)) = self_dir_meta {
                let dir_path = format!("{}/", rel_str);
                let self_status = scoped_force_status.unwrap_or_else(|| {
                    let pfx = format!("{}/", rel_str);
                    if diff_map.keys().any(|k| k.starts_with(&pfx)) {
                        'M'
                    } else {
                        ' '
                    }
                });
                let rs = remote_status_map.get(&dir_path).copied().unwrap_or(' ');
                rows.push(Row {
                    status: self_status,
                    remote_status: rs,
                    z: ' ',
                    hash_str: format_hash(&hash, opts.full_hash),
                    size: Some(sz),
                    blob_count: bc,
                    mtime: mt,
                    path: dir_path,
                });
            }
            // Only descend if the scoped directory's tree object is present
            // locally. When the scope is itself a stubbed directory, its tree
            // object is absent (only the .omemfs-stub marker exists), so reading
            // it would raise ObjectNotFound; the self-row emitted above already
            // represents it and the stub-marking pass sets its Z to `S`. This
            // mirrors the guard inside collect_tree_rows for nested stubs.
            if local.exists(&hash)? {
                {
                    let _t = dtimer_l1!("collect_tree_rows");
                    collect_tree_rows(
                        &hash,
                        &rel_str,
                        &local,
                        opts.recursive,
                        opts.full_hash,
                        &diff_map,
                        &remote_status_map,
                        scoped_force_status,
                        &scan_result.filters,
                        &mut rows,
                    )?;
                }
            }
        }
    }

    // Gather all diff entries (Added, Modified, Deleted).
    // diff_map is already scoped to the requested paths by diff_trees.
    // Used both for emitting A rows and for propagating M status to parent directories.
    let mut added_sorted: Vec<(&String, &DiffRow)> = diff_map.iter().collect();
    added_sorted.sort_by_key(|(p, _)| p.as_str());

    // Determine the effective scope prefix for non-recursive grouping.
    // When scoped_prefixes is empty, the scope root is ""; otherwise use each prefix.
    // Each root is paired with its slash-terminated form to avoid repeated
    // allocations in the per-entry scope lookups below.
    let scope_roots: Vec<(String, String)> = if scoped_prefixes.is_empty() {
        vec![(String::new(), String::new())]
    } else {
        scoped_prefixes
            .iter()
            .map(|s| (s.clone(), format!("{}/", s)))
            .collect()
    };

    // Collect paths already emitted by collect_tree_rows so we can avoid
    // duplicating directory rows and update their status when needed.
    // This set is kept up to date as new rows are pushed below, so it is
    // built only once instead of being rebuilt before each block.
    let mut existing_paths: HashSet<String> = rows.iter().map(|r| r.path.clone()).collect();

    // Index of directory rows (paths ending with '/') for O(1) status updates,
    // replacing a linear scan over all rows. Indices stay valid because rows
    // are only appended after this point.
    let mut dir_row_index: HashMap<String, usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.path.ends_with('/'))
        .map(|(i, r)| (r.path.clone(), i))
        .collect();

    let mut emitted_add_dirs: HashSet<String> = HashSet::new();
    for (path, row) in added_sorted {
        // Find which scope this path belongs to.
        let (scope_root, scope_slashed) = scope_roots
            .iter()
            .find(|(bare, slashed)| {
                bare.is_empty() || path.as_str() == bare || path.starts_with(slashed.as_str())
            })
            .map(|(b, s)| (b.as_str(), s.as_str()))
            .unwrap_or(("", ""));

        let is_direct = opts.recursive || is_direct_child(path, scope_root);
        if is_direct {
            match row {
                DiffRow::Added { working_hash: wh }
                    // Skip if already emitted by collect_tree_rows (e.g. when --working
                    // resolved the scope path from the working tree).
                    if !existing_paths.contains(path.as_str()) => {
                        existing_paths.insert(path.clone());
                        rows.push(Row {
                            status: 'A',
                            remote_status: ' ',
                            z: ' ',
                            hash_str: format_hash(wh, opts.full_hash),
                            size: None,
                            blob_count: 1,
                            mtime: None,
                            path: path.clone(),
                        });
                    }
                DiffRow::AddedEmptyDir { hash } => {
                    // Skip if already emitted by collect_tree_rows (e.g. when the
                    // scope directory was resolved from the working tree, forcing
                    // status 'A' on its descendants). Without this guard the empty
                    // directory would be listed twice — once here and once by the
                    // forced-'A' tree walk. Mirrors the Added (blob) arm above.
                    let dir_path = format!("{}/", path);
                    if !existing_paths.contains(dir_path.as_str()) {
                        existing_paths.insert(dir_path.clone());
                        dir_row_index.insert(dir_path.clone(), rows.len());
                        rows.push(Row {
                            status: 'A',
                            remote_status: ' ',
                            z: ' ',
                            hash_str: format_hash(hash, opts.full_hash),
                            size: Some(0),
                            blob_count: 0,
                            mtime: None,
                            path: dir_path,
                        });
                    }
                }
                // Modified/Deleted direct children are already shown by collect_tree_rows.
                _ => {}
            }
        }

        // For any diff entry (Added, Modified, Deleted) under a subdirectory,
        // propagate M status to the parent directory row already emitted by
        // collect_tree_rows. This covers Modified/Deleted files whose parent
        // directory would otherwise remain ' '.
        if !is_direct {
            let remainder = if scope_root.is_empty() {
                path.as_str()
            } else {
                path.strip_prefix(scope_slashed).unwrap_or(path)
            };
            let dir_name = remainder.split('/').next().unwrap_or("");
            if !dir_name.is_empty() {
                let dir_path = if scope_root.is_empty() {
                    format!("{}/", dir_name)
                } else {
                    format!("{}/{}/", scope_root, dir_name)
                };
                if !emitted_add_dirs.contains(&dir_path) {
                    emitted_add_dirs.insert(dir_path.clone());
                    // If this directory is already listed (from clone root), update
                    // its status to M rather than adding a duplicate A row.
                    if existing_paths.contains(&dir_path) {
                        if let Some(&idx) = dir_row_index.get(&dir_path) {
                            let r = &mut rows[idx];
                            if r.status == ' ' {
                                r.status = 'M';
                            }
                        }
                    } else if matches!(row, DiffRow::Added { .. } | DiffRow::AddedEmptyDir { .. }) {
                        // Only emit a new A directory row for Added entries; Modified/Deleted
                        // entries always reference paths that exist in clone root, so the
                        // directory row is already present in existing_paths.
                        existing_paths.insert(dir_path.clone());
                        dir_row_index.insert(dir_path.clone(), rows.len());
                        rows.push(Row {
                            status: 'A',
                            remote_status: ' ',
                            z: ' ',
                            hash_str: "-".to_string(),
                            size: None,
                            blob_count: 0,
                            mtime: None,
                            path: dir_path,
                        });
                    }
                }
            }
        }
    }

    // Emit rows for remote-added paths that do not exist in clone_root and
    // would therefore never appear via collect_tree_rows.
    // Only include paths that are direct children of the scope (or all descendants
    // when recursive).
    {
        let mut remote_added_sorted: Vec<(&String, &TreeEntry)> =
            remote_added_entries.iter().collect();
        remote_added_sorted.sort_by_key(|(p, _)| p.as_str());

        for (path, entry) in remote_added_sorted {
            if existing_paths.contains(path.as_str()) {
                continue;
            }
            // remote_added_entries is already scoped by fetch_remote_status_map,
            // so no in-scope check is needed here.
            // Non-recursive: only emit direct children of the scope root.
            let bare = path.trim_end_matches('/');
            let scope_root = scope_roots
                .iter()
                .find(|(b, s)| b.is_empty() || bare == b.as_str() || bare.starts_with(s.as_str()))
                .map(|(b, _)| b.as_str())
                .unwrap_or("");
            if !opts.recursive && !is_direct_child(bare, scope_root) {
                continue;
            }
            let hash_str = match entry.hash() {
                Some(h) => format_hash(h, opts.full_hash),
                None => "-".to_string(),
            };
            existing_paths.insert(path.clone());
            match entry {
                TreeEntry::Blob { size, mtime, .. } => {
                    rows.push(Row {
                        status: ' ',
                        remote_status: 'A',
                        z: ' ',
                        hash_str,
                        size: Some(*size),
                        blob_count: 1,
                        mtime: *mtime,
                        path: path.clone(),
                    });
                }
                TreeEntry::Tree {
                    size,
                    blob_count,
                    mtime,
                    ..
                } => {
                    rows.push(Row {
                        status: ' ',
                        remote_status: 'A',
                        z: ' ',
                        hash_str,
                        size: Some(*size),
                        blob_count: *blob_count,
                        mtime: *mtime,
                        path: path.clone(),
                    });
                }
                TreeEntry::Symlink { mtime, .. } => {
                    rows.push(Row {
                        status: ' ',
                        remote_status: 'A',
                        z: ' ',
                        hash_str: "-".to_string(),
                        size: None,
                        blob_count: 1,
                        mtime: *mtime,
                        path: path.clone(),
                    });
                }
            }
        }
    }

    // Mark ignored paths (design/05 "[ignore] section" Z column rules).
    // An ignored path that is present in clone root shows Z=`i` and X=`D`
    // (it will be removed from the remote on the next push). An ignored path
    // absent from clone root shows Z=`I` and X=` `. Either way the metadata
    // columns (hash/size/mtime) are shown as `-`.
    {
        let filter_set = &scan_result.filters;
        let hash_dash = "-".repeat(if opts.full_hash { 64 } else { 8 });
        // Mark existing rows whose path matches an ignore pattern.
        for row in rows.iter_mut() {
            let bare = row.path.trim_end_matches('/');
            if filter_set.is_ignored(bare) {
                // Determine clone-root membership to choose between `i` and `I`.
                let parts: Vec<&str> = bare.split('/').filter(|s| !s.is_empty()).collect();
                let in_clone = !parts.is_empty()
                    && crate::tree_ops::navigate_entry(&root_hash, &parts, &local)
                        .ok()
                        .flatten()
                        .is_some();
                if in_clone {
                    row.z = 'i';
                    row.status = 'D';
                } else {
                    row.z = 'I';
                    row.status = ' ';
                }
                // Metadata columns are not meaningful for ignored entries.
                row.hash_str = hash_dash.clone();
                row.size = None;
                row.blob_count = 0;
                row.mtime = None;
            }
        }
        // Emit rows for ignored working-tree paths that are not yet in rows,
        // using side data collected during the scan (no extra FS walk).
        for (rel_path, is_dir) in &scan_result.side.ignored {
            // Directories get a trailing `/` in the display path.
            let display_path = if *is_dir {
                format!("{}/", rel_path)
            } else {
                rel_path.clone()
            };
            // Skip if already in the output (e.g. it is in clone_root and was
            // emitted by collect_tree_rows; its z was already updated above).
            if existing_paths.contains(&display_path) {
                continue;
            }
            // Scope filtering: check if this path falls within the requested scope.
            if !scope.in_scope(rel_path) {
                continue;
            }
            // Non-recursive: only emit direct children of the effective scope root.
            if !opts.recursive {
                let scope_root = scope_roots
                    .iter()
                    .find(|(b, s)| {
                        b.is_empty()
                            || rel_path.as_str() == b.as_str()
                            || rel_path.starts_with(s.as_str())
                    })
                    .map(|(b, _)| b.as_str())
                    .unwrap_or("");
                if !is_direct_child(rel_path, scope_root) {
                    continue;
                }
            }
            // Determine clone-root membership to choose between `i` and `I`.
            let parts: Vec<&str> = rel_path.split('/').filter(|s| !s.is_empty()).collect();
            let in_clone = !parts.is_empty()
                && crate::tree_ops::navigate_entry(&root_hash, &parts, &local)
                    .ok()
                    .flatten()
                    .is_some();
            let (z, status) = if in_clone { ('i', 'D') } else { ('I', ' ') };
            rows.push(Row {
                status,
                remote_status: ' ',
                z,
                hash_str: hash_dash.clone(),
                size: None,
                blob_count: 0,
                mtime: None,
                path: display_path,
            });
        }

        // Emit rows for unknown reserved `.omemfs-` files (newer-version
        // artefacts). Show Z=`?`, metadata `-`, never modify them
        // (design/09_reserved_names.md "Forward compatibility").
        for rel_path in &scan_result.side.unknown_reserved {
            if existing_paths.contains(rel_path.as_str()) {
                continue;
            }
            if !scope.in_scope(rel_path) {
                continue;
            }
            if !opts.recursive {
                let scope_root = scope_roots
                    .iter()
                    .find(|(b, s)| {
                        b.is_empty()
                            || rel_path.as_str() == b.as_str()
                            || rel_path.starts_with(s.as_str())
                    })
                    .map(|(b, _)| b.as_str())
                    .unwrap_or("");
                if !is_direct_child(rel_path, scope_root) {
                    continue;
                }
            }
            rows.push(Row {
                status: ' ',
                remote_status: ' ',
                z: '?',
                hash_str: hash_dash.clone(),
                size: None,
                blob_count: 0,
                mtime: None,
                path: rel_path.clone(),
            });
        }
    }

    rows.sort_by(|a, b| a.path.cmp(&b.path));

    // Build stub state sets from the working tree.
    // direct_stubs: paths that are directly stubbed (file or fully-stubbed dir).
    // partial_dirs: directory paths with .omemfs-stub + real files (partial expansion).
    // subtree_stub_dirs: dirs that are not directly stubbed but have stubs in their subtree.
    // Stub records come from scan side data (no extra FS walk).
    let stub_list = &scan_result.side.stubs;
    let mut direct_stubs: HashSet<String> = HashSet::new();
    let mut partial_dirs: HashSet<String> = HashSet::new();
    for (rel_path, record) in stub_list.iter() {
        if record.target_type == crate::stub::StubTargetType::Tree {
            // Determine if this dir is fully stubbed or partially expanded.
            let abs_dir = opts.work_dir.join(rel_path);
            let is_partial = std::fs::read_dir(&abs_dir)
                .map(|rd| {
                    rd.flatten().any(|e| {
                        let name = e.file_name().to_string_lossy().into_owned();
                        name != crate::stub::DIR_STUB_NAME
                    })
                })
                .unwrap_or(false);
            if is_partial {
                partial_dirs.insert(rel_path.clone());
            } else {
                direct_stubs.insert(format!("{}/", rel_path));
            }
        } else {
            direct_stubs.insert(rel_path.clone());
        }
    }
    // Build set of dirs that have at least one stub anywhere in their subtree.
    let mut subtree_stub_dirs: HashSet<String> = HashSet::new();
    for (rel_path, _) in stub_list.iter() {
        let mut cursor = rel_path.as_str();
        while let Some(pos) = cursor.rfind('/') {
            cursor = &cursor[..pos];
            if cursor.is_empty() {
                break;
            }
            subtree_stub_dirs.insert(cursor.to_string());
        }
    }
    for dir in &partial_dirs {
        let mut cursor = dir.as_str();
        while let Some(pos) = cursor.rfind('/') {
            cursor = &cursor[..pos];
            if cursor.is_empty() {
                break;
            }
            subtree_stub_dirs.insert(cursor.to_string());
        }
    }

    // Build the set of stub ancestor prefixes: if a directory is a direct stub,
    // any row whose path starts with that directory prefix is also implicitly stubbed.
    let stub_ancestor_prefixes: HashSet<String> = direct_stubs
        .iter()
        .filter(|p| p.ends_with('/'))
        .cloned()
        .collect();

    // Conflict base paths come from scan side data (no extra FS walk).
    let conflict_paths = &scan_result.side.conflict_paths;
    for row in rows.iter_mut() {
        let bare = row.path.trim_end_matches('/');
        // Conflict takes precedence over all other Z values.
        let has_conflict = if conflict_paths.contains(&row.path) {
            true
        } else if row.path == "." {
            // Root self-row: '!' if any conflict exists anywhere in the tree.
            !conflict_paths.is_empty()
        } else if row.path.ends_with('/') {
            conflict_paths
                .iter()
                .any(|p| p.starts_with(row.path.as_str()))
        } else {
            false
        };
        if has_conflict {
            row.z = '!';
        } else if row.z == 'I' || row.z == 'i' || row.z == '?' {
            // Already marked ignored or unknown-reserved; preserve it (priority
            // above S/s).
        } else if direct_stubs.contains(&row.path) {
            row.z = 'S';
        } else if stub_ancestor_prefixes
            .iter()
            .any(|p| row.path.starts_with(p.as_str()))
        {
            // This entry is inside a dir-stubbed directory — mark it as indirectly stubbed.
            row.z = 'S';
        } else if row.path == "." {
            // Root self-row: 's' if any stub exists anywhere in the tree.
            if !stub_list.is_empty() {
                row.z = 's';
            }
        } else if row.path.ends_with('/') {
            let dir = bare;
            if partial_dirs.contains(dir) || subtree_stub_dirs.contains(dir) {
                row.z = 's';
            }
        }
    }

    // Apply source substitution: replace hash/size/blob_count/mtime with the
    // values from the requested source (remote or working tree).
    match opts.source {
        LsSource::Clone => {} // already populated from clone root
        LsSource::Remote => {
            for row in rows.iter_mut() {
                let bare = row.path.trim_end_matches('/');
                if let Some(entry) = remote_entries
                    .get(bare)
                    .or_else(|| remote_entries.get(&row.path))
                {
                    apply_entry_to_row(row, entry, opts.full_hash);
                }
            }
        }
        LsSource::Working => {
            // Resolved per displayed row by navigating the working tree directly
            // (design/04 "--working"), rather than pre-flattening the whole
            // working tree into a lookup table: this bounds the cost to the rows
            // actually shown, matching how --remote resolves from the diff.
            for row in rows.iter_mut() {
                let bare = row.path.trim_end_matches('/');
                if bare.is_empty() || bare == "." {
                    let (mt, sz, bc) =
                        crate::tree_ops::tree_meta(&working_hash, &local).unwrap_or((None, 0, 0));
                    row.hash_str = format_hash(&working_hash, opts.full_hash);
                    row.size = Some(sz);
                    row.blob_count = bc;
                    row.mtime = mt;
                    continue;
                }
                let parts: Vec<&str> = bare.split('/').filter(|s| !s.is_empty()).collect();
                if let Some(entry) = navigate_working(&working_hash, &parts, &local) {
                    apply_entry_to_row(row, &entry, opts.full_hash);
                }
            }
        }
    }

    // A subtree whose local diff could not be resolved (see
    // diff_trees_with_heal) is not recorded as Added/Modified/Deleted in
    // diff_map, so the normal status_char/force_status logic above leaves its
    // row (and any listed ancestor directory row) at ' ' -- misreporting it as
    // "in sync" when its status is actually unknown. Override those rows'
    // status to STATUS_UNKNOWN here, after every other status/force_status
    // assignment above, so nothing later overwrites it back to ' '.
    if !unknown_paths.is_empty() {
        // Bare directory path (no trailing slash; "." for the root) -> row
        // index, for O(1) lookups while walking each unknown path's ancestors.
        // Owned `String` keys (not borrowed from `rows`) so `rows` can still
        // be mutated below while this map is in scope.
        let dir_idx: HashMap<String, usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.path.ends_with('/') || r.path == ".")
            .map(|(i, r)| (r.path.trim_end_matches('/').to_string(), i))
            .collect();
        for up in &unknown_paths {
            // The unresolvable subtree's own row, if it is listed.
            if let Some(&idx) = dir_idx.get(up.as_str()) {
                rows[idx].status = STATUS_UNKNOWN;
            }
            // Ancestors: a listed ancestor directory cannot vouch for a
            // descendant it could not resolve, so an ancestor otherwise
            // reported "in sync" (' ') is upgraded to the same marker. An
            // ancestor already showing a genuinely detected M/A/D change
            // elsewhere keeps that (more specific) status.
            let mut cur = up.as_str();
            while cur != "." {
                let anc = match cur.rfind('/') {
                    Some(pos) => &cur[..pos],
                    None => ".",
                };
                if let Some(&idx) = dir_idx.get(anc) {
                    if rows[idx].status == ' ' {
                        rows[idx].status = STATUS_UNKNOWN;
                    }
                }
                cur = anc;
            }
        }
    }

    let count = rows.len();
    print_rows(&rows)?;
    phase.complete(format!("{} entries", count));
    Ok(())
}

// ---------------------------------------------------------------------------
// Scoped working-tree scan (design/04 "Scoped working-tree scan")
// ---------------------------------------------------------------------------

/// Scan the working tree for `ls` and return a [`ScanResult`] whose `root_hash`
/// is a full working-tree root hash.
///
/// When `paths` is empty, this is exactly the unscoped whole-tree scan. When
/// one or more `<path>` arguments are given, only those subtrees are scanned and
/// each scanned subtree (or single file / stub entry) is spliced onto `base`
/// (the clone root, or an empty tree when none exists) so the returned hash is
/// indistinguishable from a full scan to the downstream diff/listing code, while
/// out-of-scope paths incur no working-tree I/O.
///
/// `files` and `side` accumulate only the in-scope entries; that is correct for
/// `ls`, which only lists the requested paths.
fn scan_scoped_for_ls(
    work_dir: &std::path::Path,
    paths: &[PathBuf],
    clone_root: Option<&Hash>,
    clone_root_flat: Option<&HashMap<String, TreeEntry>>,
    store: &dyn ObjectStore,
    stat_cache: &StatCache,
) -> Result<ScanResult, Error> {
    // ls is read-only: hash files but skip blob writes (design/03 "Scan
    // blob-write mode"). Tree objects are still written so diffing can read them.
    const WRITE_BLOBS: bool = false;

    // Unscoped: scan the whole working tree exactly as before.
    if paths.is_empty() {
        return scan_and_store_with_cache(
            work_dir,
            work_dir,
            store,
            clone_root_flat,
            stat_cache,
            WRITE_BLOBS,
        );
    }

    // Load filter rules once for ignore checks (also returned in the result).
    // For a single path, load scope-limited so a scoped ls does not re-walk the
    // whole tree just to discover `.omemfs-filter` files (design/05); multiple
    // paths fall back to a full load, as the returned filters may be queried for
    // any in-scope path across the requested subtrees.
    let filters = match single_scope_prefix(work_dir, paths) {
        Some(prefix) => crate::filter::FilterSet::load_scoped(work_dir, &prefix),
        None => crate::filter::FilterSet::load(work_dir),
    };

    // Start the splice base from the clone root (or an empty tree when none).
    let mut root_hash = match clone_root {
        Some(h) => h.clone(),
        None => crate::tree_ops::build_and_store(vec![], store)?,
    };
    let mut files: HashMap<String, ScannedFile> = HashMap::new();
    let mut side = ScanSideData::default();

    for path in paths {
        // Resolve to a repo-relative path with forward slashes.
        let abs = if path.is_absolute() {
            path.clone()
        } else {
            work_dir.join(path)
        };
        let rel = abs.strip_prefix(work_dir).unwrap_or(&abs);
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        let rel_str = rel_str.trim_matches('/').to_string();

        // Root scope (".") degenerates to a full scan; do it and return.
        if rel_str.is_empty() {
            return scan_and_store_with_cache(
                work_dir,
                work_dir,
                store,
                clone_root_flat,
                stat_cache,
                WRITE_BLOBS,
            );
        }

        let components: Vec<&str> = rel_str.split('/').filter(|s| !s.is_empty()).collect();

        // An ignored path contributes nothing to the working-tree hash; leave the
        // base unchanged so the path is listed from the clone root (Z='i') or is
        // simply absent. This mirrors push, which skips ignored scoped paths.
        if crate::commands::push::is_scoped_path_ignored(&filters, &rel_str) {
            continue;
        }

        // A stubbed path: splice its recorded entry directly (no scan), matching
        // push's single-path stub handling (design/08).
        if let Some(stub_entry) = crate::commands::push::resolve_scoped_stub(work_dir, &rel_str) {
            root_hash =
                crate::tree_ops::splice_entry(Some(&root_hash), &components, stub_entry, store)?;
            continue;
        }

        // A path that does not exist on disk and is not a stub: leave the base
        // unchanged. The path is then resolved from the clone root by the caller
        // (or reported as not found if absent there too).
        if !abs.exists() {
            continue;
        }

        let new_entry = if abs.is_dir() {
            // Scan only this subtree. `scan_and_store_with_cache` derives the
            // rel_prefix from `dir` against `work_dir`, so the recorded `files`
            // and `side` keys are already repo-relative.
            let sub = scan_and_store_with_cache(
                work_dir,
                &abs,
                store,
                clone_root_flat,
                stat_cache,
                WRITE_BLOBS,
            )?;
            files.extend(sub.files);
            merge_side_data(&mut side, sub.side);

            let (mtime, size, blob_count) = crate::tree_ops::tree_meta(&sub.root_hash, store)?;
            TreeEntry::Tree {
                name: components.last().unwrap().to_string(),
                hash: sub.root_hash,
                mtime,
                size,
                blob_count,
            }
        } else {
            // Single file: hash only (no blob write), then build its entry.
            let hash = codec::chunk::hash_file(&abs)?;
            let meta = std::fs::metadata(&abs)?;
            let size = meta.len();
            let mode = crate::fsmeta::mode_from_metadata(&meta);
            let fs_mtime = meta.modified().ok();
            let mtime = crate::fsmeta::mtime_from_metadata(&meta);
            if let Some(fs_mtime) = fs_mtime {
                files.insert(
                    rel_str.clone(),
                    ScannedFile {
                        fs_mtime,
                        fs_size: size,
                        hash: hash.clone(),
                        cache_hit: false,
                        fallback_hit: false,
                    },
                );
            }
            TreeEntry::Blob {
                name: components.last().unwrap().to_string(),
                hash,
                mtime,
                size,
                mode,
            }
        };

        root_hash = crate::tree_ops::splice_entry(Some(&root_hash), &components, new_entry, store)?;
    }

    Ok(ScanResult {
        root_hash,
        files,
        filters,
        side,
    })
}

/// Return the STAT_CACHE scope prefix for a single-path `ls`, or `None` to use a
/// full read. A single `<path>` yields its repo-relative forward-slash prefix
/// (empty string for the root, which also means "full read"); zero or multiple
/// paths yield `None` (full read), matching scoped push (design/07).
fn single_scope_prefix(work_dir: &std::path::Path, paths: &[PathBuf]) -> Option<String> {
    if paths.len() != 1 {
        return None;
    }
    let p = &paths[0];
    let abs = if p.is_absolute() {
        p.clone()
    } else {
        work_dir.join(p)
    };
    let rel = abs.strip_prefix(work_dir).unwrap_or(&abs);
    let prefix = rel.to_string_lossy().replace('\\', "/");
    let prefix = prefix.trim_matches('/').to_string();
    if prefix.is_empty() {
        None
    } else {
        Some(prefix)
    }
}

/// Merge scoped scan side data (`src`) into the accumulator (`dst`).
fn merge_side_data(dst: &mut ScanSideData, src: ScanSideData) {
    dst.conflict_paths.extend(src.conflict_paths);
    dst.stubs.extend(src.stubs);
    dst.ignored.extend(src.ignored);
    dst.unknown_reserved.extend(src.unknown_reserved);
}

// ---------------------------------------------------------------------------
// Entry resolution helpers
// ---------------------------------------------------------------------------

/// Look up a path in `remote_added_entries`, trying both the directory key
/// (trailing slash) and the bare key. Returns a cloned entry when present.
fn lookup_remote_added(
    remote_added_entries: &HashMap<String, TreeEntry>,
    rel_str: &str,
) -> Option<TreeEntry> {
    remote_added_entries
        .get(format!("{}/", rel_str).as_str())
        .or_else(|| remote_added_entries.get(rel_str))
        .cloned()
}

/// Navigate `parts` from the working tree root, swallowing navigation errors
/// (a missing path simply yields `None`).
fn navigate_working(
    working_hash: &Hash,
    parts: &[&str],
    store: &dyn ObjectStore,
) -> Option<TreeEntry> {
    crate::tree_ops::navigate_entry(working_hash, parts, store)
        .ok()
        .flatten()
}

/// Extract the hash, blob-ness, and directory self-row metadata from a resolved
/// entry. `self_dir_meta` is `Some` for trees (size, blob_count, mtime) and
/// `None` for blobs/symlinks. Errors when a tree entry carries no hash.
fn resolve_entry_meta(
    entry: &TreeEntry,
    rel_str: &str,
) -> Result<(Hash, bool, Option<(u64, u64, Option<DateTime<Utc>>)>), Error> {
    let is_blob = matches!(entry, TreeEntry::Blob { .. } | TreeEntry::Symlink { .. });
    let meta = if is_blob {
        None
    } else {
        Some((entry.size(), entry.blob_count(), entry.mtime().copied()))
    };
    let hash = entry
        .hash()
        .cloned()
        .ok_or_else(|| Error::Other(format!("path not found: {}", rel_str)))?;
    Ok((hash, is_blob, meta))
}

// ---------------------------------------------------------------------------
// Column-aligned output
// ---------------------------------------------------------------------------

fn print_rows(rows: &[Row]) -> Result<(), Error> {
    if rows.is_empty() {
        return Ok(());
    }

    let hash_w = rows.iter().map(|r| r.hash_str.len()).max().unwrap_or(8);
    let size_w = rows
        .iter()
        .map(|r| match r.size {
            Some(n) => digit_count(n),
            None => 1,
        })
        .max()
        .unwrap_or(1);
    let count_w = rows
        .iter()
        .map(|r| digit_count(r.blob_count))
        .max()
        .unwrap_or(1);
    // mtime column width: short format max is 11 chars ("MM-DD HH:MM"); lock to
    // at least 11 so all stages align.
    let mtime_w = rows
        .iter()
        .map(|r| match r.mtime.as_ref() {
            Some(ts) => format_mtime_staged(ts).0.len(),
            None => 1,
        })
        .max()
        .unwrap_or(1)
        .max(11);

    let mut out = Output::for_stdout();
    let colored = out.colored();
    let styles = out.styles;

    for r in rows {
        let status_str = paint_status(colored, r.status, &styles);
        let remote_str = match r.remote_status {
            'M' => paint(colored, styles.modified, "M"),
            'A' => paint(colored, styles.added, "A"),
            'D' => paint(colored, styles.deleted, "D"),
            _ => " ".to_string(),
        };
        let z_str = match r.z {
            '!' => paint(colored, styles.conflict, "!"),
            'I' | 'i' => paint(colored, styles.meta, &r.z.to_string()),
            '?' => paint(colored, styles.conflict, "?"),
            'S' | 's' => paint(colored, styles.stub, &r.z.to_string()),
            _ => " ".to_string(),
        };
        let hash_str = paint(
            colored,
            styles.hash,
            &format!(" {:>width$}", r.hash_str, width = hash_w),
        );
        let size_str = render_size_cell(r.size, size_w, colored, &styles);
        let count_str = render_count_cell(r.blob_count, count_w, colored, &styles);
        let mtime_str = render_mtime_cell(r.mtime.as_ref(), mtime_w, colored, &styles);
        let path_str = if colored && r.path.ends_with('/') {
            paint(true, styles.directory, &r.path)
        } else {
            r.path.clone()
        };
        let line = format!(
            "{}{}{}{} {} {} {} {}",
            remote_str, status_str, z_str, hash_str, size_str, count_str, mtime_str, path_str
        );
        out.writeln(&line)?;
    }
    out.finish()?;
    Ok(())
}

fn paint_status(colored: bool, status: char, styles: &Styles) -> String {
    if !colored {
        return status.to_string();
    }
    match status {
        'A' => paint(true, styles.added, &status.to_string()),
        'M' => paint(true, styles.modified, &status.to_string()),
        'D' => paint(true, styles.deleted, &status.to_string()),
        STATUS_UNKNOWN => paint(true, styles.conflict, &status.to_string()),
        c => c.to_string(),
    }
}

/// Render size column with 3-digit group coloring from the right.
/// Whitespace padding is emitted unstyled to preserve column alignment.
fn render_size_cell(size: Option<u64>, width: usize, colored: bool, styles: &Styles) -> String {
    let raw = match size {
        Some(n) => format!("{:>width$}", n, width = width),
        None => format!("{:>width$}", "-", width = width),
    };
    if !colored {
        return raw;
    }
    match size {
        None => paint(true, styles.meta, &raw),
        Some(n) => {
            let num = n.to_string();
            let pad_len = raw.len() - num.len();
            let pad = &raw[..pad_len];
            let digits: Vec<char> = num.chars().collect();
            let mut pieces: Vec<(usize, String)> = Vec::new();
            let mut group_idx = 0usize;
            let mut buf: Vec<char> = Vec::new();
            for &d in digits.iter().rev() {
                buf.push(d);
                if buf.len() == 3 {
                    let chunk: String = buf.iter().rev().collect();
                    pieces.push((group_idx, chunk));
                    buf.clear();
                    group_idx += 1;
                }
            }
            if !buf.is_empty() {
                let chunk: String = buf.iter().rev().collect();
                pieces.push((group_idx, chunk));
            }
            let grades = &styles.size_digit_grades;
            let top = grades.len() - 1;
            let mut out = pad.to_string();
            for (idx, chunk) in pieces.into_iter().rev() {
                out.push_str(&paint(true, grades[idx.min(top)], &chunk));
            }
            out
        }
    }
}

/// Render blob_count column. Styled with the first size grade (B tier).
fn render_count_cell(count: u64, width: usize, colored: bool, styles: &Styles) -> String {
    let raw = format!("{:>width$}", count, width = width);
    if colored {
        paint(true, styles.size_digit_grades[0], &raw)
    } else {
        raw
    }
}

/// Stages for the short mtime format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MtimeStage {
    /// elapsed < 1 min: "now"
    JustNow,
    /// elapsed 1-59 min: "<N>m"
    MinAgo,
    /// same calendar day (elapsed ≥ 60 min): "MM-DD HH:MM" (two-color)
    Today,
    /// ≤ 14 calendar days excl. today: "MM-DD HH:MM"
    Recent,
    /// otherwise: "YYYY-MM-DD"
    Older,
}

/// Render mtime column with stage-based coloring.
/// Stage Today uses a two-color split: MM-DD in steel blue, HH:MM in sky blue.
fn render_mtime_cell(
    mtime: Option<&DateTime<Utc>>,
    width: usize,
    colored: bool,
    styles: &Styles,
) -> String {
    let (text, stage) = match mtime {
        Some(ts) => format_mtime_staged(ts),
        None => ("-".to_string(), None),
    };
    let raw = format!("{:>width$}", text, width = width);
    if !colored {
        return raw;
    }
    if stage == Some(MtimeStage::Today) && raw.len() >= 11 {
        let pad_len = raw.len().saturating_sub(11);
        let pad = &raw[..pad_len];
        let content = &raw[pad_len..];
        if content.len() == 11 {
            let date_part = &content[..6]; // "MM-DD "
            let time_part = &content[6..]; // "HH:MM"
            return format!(
                "{}{}{}",
                pad,
                paint(true, styles.mtime_today_date, date_part),
                paint(true, styles.mtime_today_time, time_part),
            );
        }
    }
    let style = match stage {
        Some(MtimeStage::JustNow) => styles.mtime_just_now,
        Some(MtimeStage::MinAgo) => styles.mtime_min_ago,
        Some(MtimeStage::Today) => styles.mtime_today_time,
        Some(MtimeStage::Recent) => styles.mtime_recent,
        Some(MtimeStage::Older) => styles.mtime_older,
        None => styles.meta,
    };
    paint(true, style, &raw)
}

fn format_mtime_staged(ts: &DateTime<Utc>) -> (String, Option<MtimeStage>) {
    let now = Utc::now();
    if *ts <= now {
        let elapsed = now.signed_duration_since(*ts);
        let secs = elapsed.num_seconds();
        if secs < 60 {
            return ("now".to_string(), Some(MtimeStage::JustNow));
        }
        let minutes = secs / 60;
        if minutes < 60 {
            return (format!("{}m", minutes), Some(MtimeStage::MinAgo));
        }
    }

    let mtime_date = ts.with_timezone(&chrono::Local).date_naive();
    let now_date = now.with_timezone(&chrono::Local).date_naive();
    let day_diff = (now_date - mtime_date).num_days();

    if day_diff == 0 {
        let s = ts
            .with_timezone(&chrono::Local)
            .format("%m-%d %H:%M")
            .to_string();
        return (s, Some(MtimeStage::Today));
    }
    if day_diff > 0 && day_diff <= 14 {
        let s = ts
            .with_timezone(&chrono::Local)
            .format("%m-%d %H:%M")
            .to_string();
        return (s, Some(MtimeStage::Recent));
    }
    let s = ts
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d")
        .to_string();
    (s, Some(MtimeStage::Older))
}

fn digit_count(n: u64) -> usize {
    if n == 0 {
        return 1;
    }
    let mut v = n;
    let mut d = 0;
    while v > 0 {
        d += 1;
        v /= 10;
    }
    d
}

// ---------------------------------------------------------------------------
// Tree collection
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn collect_tree_rows(
    tree_hash: &Hash,
    prefix: &str,
    store: &dyn ObjectStore,
    recursive: bool,
    full_hash: bool,
    diff_map: &HashMap<String, DiffRow>,
    remote_status_map: &HashMap<String, char>,
    force_status: Option<char>,
    filters: &crate::filter::FilterSet,
    out: &mut Vec<Row>,
) -> Result<(), Error> {
    let entries = crate::tree_ops::load_all_entries(tree_hash, store)?;

    for entry in &entries {
        let path = if prefix.is_empty() || prefix == "." {
            entry.name().to_string()
        } else {
            format!("{}/{}", prefix, entry.name())
        };

        let hash_str = match entry.hash() {
            Some(h) => format_hash(h, full_hash),
            None => "-".repeat(if full_hash { 64 } else { 8 }),
        };

        match entry {
            TreeEntry::Tree {
                hash,
                size,
                blob_count,
                mtime,
                ..
            } => {
                let dir_path = format!("{}/", path);
                let status = force_status.unwrap_or(' ');
                let rs = remote_status_map.get(&dir_path).copied().unwrap_or(' ');
                out.push(Row {
                    status,
                    remote_status: rs,
                    z: ' ',
                    hash_str,
                    size: Some(*size),
                    blob_count: *blob_count,
                    mtime: *mtime,
                    path: dir_path,
                });
                // Do not recurse into ignored directories: their contents are
                // shown as a single line even with `-r` (design/05). The
                // directory row itself is kept (its Z is set to `i`/`I` later).
                //
                // Do not recurse into a stubbed directory either: its tree
                // object is absent from the local store (only the .omemfs-stub
                // marker exists on disk), so loading it would raise
                // ObjectNotFound and abort the whole listing. The directory row
                // is kept and the later stub-marking pass sets its Z to `S`.
                // This mirrors the non-recursive path, which never reads child
                // tree objects and so handles stubs gracefully.
                if recursive && !filters.is_ignored(&path) && store.exists(hash)? {
                    collect_tree_rows(
                        hash,
                        &path,
                        store,
                        recursive,
                        full_hash,
                        diff_map,
                        remote_status_map,
                        force_status,
                        filters,
                        out,
                    )?;
                }
            }
            TreeEntry::Blob { size, mtime, .. } => {
                let status = force_status.unwrap_or_else(|| status_char(&path, diff_map));
                let rs = remote_status_map.get(&path).copied().unwrap_or(' ');
                out.push(Row {
                    status,
                    remote_status: rs,
                    z: ' ',
                    hash_str,
                    size: Some(*size),
                    blob_count: 1,
                    mtime: *mtime,
                    path,
                });
            }
            TreeEntry::Symlink { mtime, .. } => {
                let status = force_status.unwrap_or_else(|| status_char(&path, diff_map));
                let rs = remote_status_map.get(&path).copied().unwrap_or(' ');
                out.push(Row {
                    status,
                    remote_status: rs,
                    z: ' ',
                    hash_str,
                    size: None,
                    blob_count: 1,
                    mtime: *mtime,
                    path,
                });
            }
        }
    }
    Ok(())
}

fn status_char(path: &str, diff_map: &HashMap<String, DiffRow>) -> char {
    match diff_map.get(path) {
        Some(DiffRow::Modified { .. }) => 'M',
        Some(DiffRow::Deleted) => 'D',
        Some(DiffRow::Added { .. }) | Some(DiffRow::AddedEmptyDir { .. }) => 'A',
        None => ' ',
    }
}

fn format_hash(h: &Hash, full: bool) -> String {
    if full {
        h.as_str().to_string()
    } else {
        h.as_str()[..8].to_string()
    }
}

// ---------------------------------------------------------------------------
// Diff for --dirty
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum DiffRow {
    Added { working_hash: Hash },
    AddedEmptyDir { hash: Hash },
    Modified { working_hash: Hash },
    Deleted,
}

/// Status shown in the `X` column when a path's local status could not be
/// determined: a clone-root tree object the diff needed was missing from the
/// local cache and could not be resolved (fetched from the remote and
/// healed, or otherwise proven absent) either — see `diff_trees_with_heal`
/// and design/04_cli_spec.md "Local diff self-healing". Distinct from `' '`
/// ("in sync") so an unresolvable path is never misreported as unchanged.
/// This is a different column from the `Z` column's own `?` ("unrecognised
/// reserved `.omemfs-` file"), so the two do not collide in meaning.
const STATUS_UNKNOWN: char = '?';

/// Compute the local (`X` column) diff between `base` (clone root) and
/// `target` (working tree), tolerating a per-subtree object miss instead of
/// aborting the whole walk (see `diff_recursive`/`mark_deleted`). Returns the
/// normal diff map plus the set of subtree paths (bare, no trailing slash;
/// `"."` for the root) whose status could not be determined at all because a
/// tree object needed to diff them was unreadable through `store`.
fn diff_trees(
    base: Option<&Hash>,
    target: Hash,
    store: &dyn ObjectStore,
    scope: &ScopeFilter,
) -> Result<(HashMap<String, DiffRow>, HashSet<String>), Error> {
    let _t = dtimer_l1!("diff_trees");
    let mut result = HashMap::new();
    let mut unknown = HashSet::new();
    match diff_recursive(
        base,
        &target,
        store,
        scope,
        &mut String::new(),
        &mut result,
        &mut unknown,
    ) {
        Ok(()) => {}
        Err(Error::ObjectNotFound(_)) => {
            // The root tree object itself (base or target) could not be
            // resolved. Extremely unusual (this should always be readable),
            // but degrade gracefully rather than aborting `ls` entirely.
            unknown.insert(".".to_string());
        }
        Err(e) => return Err(e),
    }
    Ok((result, unknown))
}

/// `unknown` accumulates the bare path of every subtree whose diff could not
/// be computed because a tree object it needed (`target`'s own object, or a
/// `base` subtree reached while iterating it) raised `ObjectNotFound`
/// through `store`. Each recursive descent into a child subtree is caught
/// individually (mirroring `remote_diff_recursive`'s per-subtree isolation,
/// which already discards errors from a single recursive call rather than
/// aborting the whole remote diff): a miss there is recorded and the walk
/// continues with the next sibling, so one unresolvable subtree never blanks
/// out or crashes unrelated, resolvable parts of the tree. Any error other
/// than `ObjectNotFound` still propagates immediately (`?`) -- only an object
/// miss is treated as tolerable/self-healable; other failures are real.
fn diff_recursive(
    base: Option<&Hash>,
    target: &Hash,
    store: &dyn ObjectStore,
    scope: &ScopeFilter,
    prefix: &mut String,
    result: &mut HashMap<String, DiffRow>,
    unknown: &mut HashSet<String>,
) -> Result<(), Error> {
    if base == Some(target) {
        return Ok(());
    }

    let target_entries = crate::tree_ops::load_all_entries(target, store)?;
    // Full base entries (not just hashes): blob comparison needs `mode` so
    // that an executable-bit-only change is reported as modified, matching
    // push's tree-hash-based dirty detection.
    let base_map: HashMap<String, TreeEntry> = if let Some(bh) = base {
        crate::tree_ops::load_all_entries(bh, store)?
            .into_iter()
            .map(|e| (e.name().to_string(), e))
            .collect()
    } else {
        HashMap::new()
    };

    for entry in &target_entries {
        let name = entry.name();
        let path = join_path(prefix, name);
        match entry {
            TreeEntry::Blob { hash, mode, .. } => {
                match base_map.get(name) {
                    Some(TreeEntry::Blob {
                        hash: base_hash,
                        mode: base_mode,
                        ..
                    }) => {
                        if (base_hash != hash || base_mode != mode) && scope.in_scope(&path) {
                            result.insert(
                                path,
                                DiffRow::Modified {
                                    working_hash: hash.clone(),
                                },
                            );
                        }
                    }
                    // Entry kind changed (was a tree or symlink): report as modified.
                    Some(_) => {
                        if scope.in_scope(&path) {
                            result.insert(
                                path,
                                DiffRow::Modified {
                                    working_hash: hash.clone(),
                                },
                            );
                        }
                    }
                    None => {
                        if scope.in_scope(&path) {
                            result.insert(
                                path,
                                DiffRow::Added {
                                    working_hash: hash.clone(),
                                },
                            );
                        }
                    }
                }
            }
            TreeEntry::Tree { hash, .. } => {
                let base_tree = match base_map.get(name) {
                    Some(TreeEntry::Tree { hash: h, .. }) => Some(h),
                    // Entry kind changed (was a blob or symlink): diff against
                    // an empty base so all descendants are reported as added.
                    _ => None,
                };
                let prev = prefix.len();
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(name);
                // Skip subtrees that are neither inside the scope nor
                // ancestors of it.
                if scope.should_descend(prefix) {
                    let before = result.len();
                    match diff_recursive(base_tree, hash, store, scope, prefix, result, unknown) {
                        Ok(()) => {
                            // If the tree is new and nothing was inserted beneath it, it is
                            // an empty directory — record it explicitly so `ls` shows it.
                            if base_tree.is_none()
                                && result.len() == before
                                && scope.in_scope(prefix)
                            {
                                result.insert(
                                    prefix.clone(),
                                    DiffRow::AddedEmptyDir { hash: hash.clone() },
                                );
                            }
                        }
                        Err(Error::ObjectNotFound(_)) => {
                            // This subtree's own diff could not be computed (its
                            // target or base tree object was unreadable). Record it
                            // as unknown and keep walking siblings -- do not abort
                            // the rest of the walk over one unresolvable subtree.
                            if scope.in_scope(prefix) {
                                unknown.insert(prefix.clone());
                            }
                        }
                        Err(e) => return Err(e),
                    }
                }
                prefix.truncate(prev);
            }
            TreeEntry::Symlink { .. } => {
                if !base_map.contains_key(name) && scope.in_scope(&path) {
                    result.insert(
                        path,
                        DiffRow::Added {
                            working_hash: Hash::compute(b""),
                        },
                    );
                }
            }
        }
    }

    let target_names: std::collections::HashSet<&str> =
        target_entries.iter().map(|e| e.name()).collect();
    for (name, base_entry) in &base_map {
        if !target_names.contains(name.as_str()) {
            let path = join_path(prefix, name);
            match base_entry {
                TreeEntry::Tree { hash, .. } => {
                    if scope.should_descend(&path) {
                        match mark_deleted(hash, store, scope, &path, result, unknown) {
                            Ok(()) => {}
                            Err(Error::ObjectNotFound(_)) => {
                                if scope.in_scope(&path) {
                                    unknown.insert(path.clone());
                                }
                            }
                            Err(e) => return Err(e),
                        }
                    }
                }
                TreeEntry::Blob { .. } | TreeEntry::Symlink { .. } => {
                    if scope.in_scope(&path) {
                        result.insert(path, DiffRow::Deleted);
                    }
                }
            }
        }
    }
    Ok(())
}

fn join_path(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", prefix, name)
    }
}

/// Recursively record every entry under a base-only (deleted-in-target)
/// subtree as `DiffRow::Deleted`. `unknown` is threaded through for the same
/// reason as in `diff_recursive`: a nested subtree here can independently
/// fail to resolve (e.g. a deleted, never-locally-fetched sub-subtree), and
/// each recursive descent is caught individually by its own caller (see the
/// `Tree` arm below and `diff_recursive`'s deleted-entries loop) so a miss
/// there does not abort marking the rest of this subtree's siblings deleted.
fn mark_deleted(
    hash: &Hash,
    store: &dyn ObjectStore,
    scope: &ScopeFilter,
    prefix: &str,
    result: &mut HashMap<String, DiffRow>,
    unknown: &mut HashSet<String>,
) -> Result<(), Error> {
    for entry in crate::tree_ops::load_all_entries(hash, store)? {
        let path = format!("{}/{}", prefix, entry.name());
        match entry {
            TreeEntry::Blob { .. } | TreeEntry::Symlink { .. } => {
                if scope.in_scope(&path) {
                    result.insert(path, DiffRow::Deleted);
                }
            }
            TreeEntry::Tree { hash, .. } => {
                if scope.should_descend(&path) {
                    match mark_deleted(&hash, store, scope, &path, result, unknown) {
                        Ok(()) => {}
                        Err(Error::ObjectNotFound(_)) => {
                            if scope.in_scope(&path) {
                                unknown.insert(path.clone());
                            }
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
        }
    }
    Ok(())
}

/// Maximum time budget for the `LazyTreeStore`-backed local diff walk in
/// [`diff_trees_with_heal`]. Unlike [`REMOTE_LOOKUP_TIMEOUT`] (below, which
/// bounds a single `INDEX_ROOT` read), this bounds the *whole* recursive
/// local diff walk, which may issue one remote fetch per differing or
/// missing subtree rather than a single round trip. A longer budget avoids
/// spuriously falling back to the degraded local-only pass just because a
/// healthy remote takes longer than a single lookup would for a walk that
/// touches several subtrees.
const LOCAL_DIFF_HEAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Compute the local (`X` column) diff for `ls`: `base` (clone root, or its
/// scoped equivalent) vs `target` (the working tree), self-healing a local
/// cache miss on a `base` tree object by fetching it from the `origin`
/// remote -- the same [`crate::tree_ops::LazyTreeStore`] `pull` uses (see its
/// doc comment and design/03_sync_model.md "Lazy tree reads") -- bounded by
/// [`LOCAL_DIFF_HEAL_TIMEOUT`] so `ls` never blocks on a slow or unreachable
/// remote (design/04_cli_spec.md "Local diff self-healing").
///
/// - When `base` is `None` (no clone root yet), `diff_recursive` never reads
///   a base tree object at all (there is nothing to heal), so the
///   thread/timeout machinery is skipped entirely and the diff runs directly
///   against `local`.
/// - Otherwise, the diff is attempted through a `LazyTreeStore` on a
///   detached worker thread bounded by the timeout, mirroring
///   `read_root_with_timeout`'s pattern: the thread owns clones of
///   everything it touches, so it is safe for it to outlive the wait if the
///   timeout fires. On success within budget, the (possibly self-healed)
///   result is used exactly as-is.
/// - On timeout, or if the walk returns an error other than a per-subtree
///   object miss (`diff_recursive`/`mark_deleted` already tolerate that
///   class internally -- see their doc comments -- so only a genuinely
///   unrecoverable error, e.g. the remote being completely unreachable,
///   reaches here), the attempt is abandoned and a local-only pass is run
///   instead. That pass is itself tolerant of a per-subtree miss (see
///   `diff_trees`), so `ls` still succeeds -- with the affected path(s)
///   marked [`STATUS_UNKNOWN`] -- even when the remote cannot be reached at
///   all within the budget.
fn diff_trees_with_heal(
    repo: &Repo,
    base: Option<&Hash>,
    target: Hash,
    local: &crate::store::local::LocalStore,
    scope: &ScopeFilter,
    scoped_prefixes: &[String],
) -> Result<(HashMap<String, DiffRow>, HashSet<String>), Error> {
    let Some(base_hash) = base else {
        return diff_trees(None, target, local, scope);
    };

    if let Ok((pack_reader, _remote, remote_key)) = repo.pack_reader("origin", None) {
        let base_owned = base_hash.clone();
        let target_owned = target.clone();
        let local_owned = local.clone();
        let prefixes_owned = scoped_prefixes.to_vec();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let lazy =
                crate::tree_ops::LazyTreeStore::new(&local_owned, &pack_reader, remote_key.as_ref());
            let heal_scope = ScopeFilter::new(&prefixes_owned);
            let result = diff_trees(Some(&base_owned), target_owned, &lazy, &heal_scope);
            // The receiver may already be gone (timeout fired); ignore send errors.
            let _ = tx.send(result);
        });
        if let Ok(result) = rx.recv_timeout(LOCAL_DIFF_HEAL_TIMEOUT) {
            if result.is_ok() {
                return result;
            }
            // A non-tolerated error surfaced from the healed attempt: fall
            // through to the local-only pass below.
        }
        // Timeout (or the worker's send failed, e.g. it panicked): fall
        // through to the local-only pass below.
    }

    diff_trees(Some(base_hash), target, local, scope)
}

/// Returns true when `path` is a direct child of `prefix`.
/// `prefix` is empty for the root, or a slash-free directory path.
/// A "direct child" has no additional `/` beyond the prefix separator.
fn is_direct_child(path: &str, prefix: &str) -> bool {
    if prefix.is_empty() {
        !path.contains('/')
    } else {
        match path.strip_prefix(prefix).and_then(|r| r.strip_prefix('/')) {
            Some(child) => !child.contains('/'),
            None => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Scope filter (path-scoped listings)
// ---------------------------------------------------------------------------

/// Precomputed scope filter for path-scoped listings. Holds each scope prefix
/// in bare and slash-terminated forms so tree walks can test membership
/// without per-entry allocations.
struct ScopeFilter {
    /// `(bare, bare + "/")` pairs. Empty means unscoped (everything matches).
    prefixes: Vec<(String, String)>,
}

impl ScopeFilter {
    /// Build a filter from relative scope prefixes. An empty list, or any
    /// empty prefix (the root), yields an unscoped filter.
    fn new(scoped_prefixes: &[String]) -> Self {
        if scoped_prefixes.iter().any(|p| p.is_empty()) {
            return Self::unscoped();
        }
        let prefixes = scoped_prefixes
            .iter()
            .map(|p| (p.clone(), format!("{}/", p)))
            .collect();
        Self { prefixes }
    }

    fn unscoped() -> Self {
        Self {
            prefixes: Vec::new(),
        }
    }

    /// True when `path` is a scope prefix itself or located inside one.
    fn in_scope(&self, path: &str) -> bool {
        if self.prefixes.is_empty() {
            return true;
        }
        self.prefixes
            .iter()
            .any(|(bare, slashed)| path == bare || path.starts_with(slashed.as_str()))
    }

    /// True when a tree walk should descend into `dir_path`: the directory is
    /// inside a scope, or is an ancestor of one.
    fn should_descend(&self, dir_path: &str) -> bool {
        if self.prefixes.is_empty() {
            return true;
        }
        self.prefixes.iter().any(|(bare, slashed)| {
            dir_path == bare
                || dir_path.starts_with(slashed.as_str())
                || (bare.starts_with(dir_path)
                    && bare.as_bytes().get(dir_path.len()) == Some(&b'/'))
        })
    }
}

// ---------------------------------------------------------------------------
// Source substitution helpers (for --remote / --working)
// ---------------------------------------------------------------------------

/// Replace hash/size/blob_count/mtime in `row` with values from `entry`.
fn apply_entry_to_row(row: &mut Row, entry: &TreeEntry, full_hash: bool) {
    match entry {
        TreeEntry::Blob {
            hash, size, mtime, ..
        } => {
            row.hash_str = format_hash(hash, full_hash);
            row.size = Some(*size);
            row.blob_count = 1;
            row.mtime = *mtime;
        }
        TreeEntry::Tree {
            hash,
            size,
            blob_count,
            mtime,
            ..
        } => {
            row.hash_str = format_hash(hash, full_hash);
            row.size = Some(*size);
            row.blob_count = *blob_count;
            row.mtime = *mtime;
        }
        TreeEntry::Symlink { mtime, .. } => {
            row.hash_str = "-".to_string();
            row.size = None;
            row.blob_count = 1;
            row.mtime = *mtime;
        }
    }
}

// ---------------------------------------------------------------------------
// Remote changed paths
// ---------------------------------------------------------------------------
/// Maximum time to wait for the remote `INDEX_ROOT` lookup in `ls` before
/// giving up and treating it like an error (R column blank for all entries).
const REMOTE_LOOKUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Read the remote root hash from `INDEX_ROOT`, bounded by
/// [`REMOTE_LOOKUP_TIMEOUT`].
///
/// The blocking read runs on a detached worker thread that owns its inputs
/// (cloned `LocalStore`s, a boxed `RootPointer`, key), so it is safe for the
/// thread to outlive the wait when a timeout fires. Returns:
/// - `Some(hash)` when the remote has a non-empty root;
/// - `None` on timeout, on any read/decode error, or when the remote has no
///   recorded root yet — all of which the caller treats identically (R blank).
fn read_root_with_timeout(
    remote: Box<dyn crate::store::ObjectStore>,
    local_cache: crate::store::local::LocalStore,
    packcache: crate::store::local::LocalStore,
    objcache: crate::store::local::LocalStore,
    root_pointer: Box<dyn crate::codec::pack::root_pointer::RootPointer>,
    remote_key: Option<crate::codec::encrypt::EncryptKey>,
) -> Option<Hash> {
    use std::sync::mpsc;
    let (tx, rx) = mpsc::channel::<Option<Hash>>();
    // The boxed root pointer is `Send + Sync`, so it can be moved onto the
    // detached worker thread that owns all of its inputs.
    std::thread::spawn(move || {
        let reader = PackReader::new(
            remote,
            local_cache,
            packcache,
            objcache,
            root_pointer,
            remote_key,
        );
        let result = reader.read_root().ok().flatten();
        // The receiver may already be gone (timeout fired); ignore send errors.
        let _ = tx.send(result);
    });
    rx.recv_timeout(REMOTE_LOOKUP_TIMEOUT).unwrap_or_default()
}

/// Returns (status_map, added_entries, all_remote_entries).
/// - status_map: path → 'M' / 'A' / 'D' for every changed path
/// - added_entries: path → TreeEntry for remote-only ('A') paths
/// - all_remote_entries: path → TreeEntry for all remote-changed paths (A and M),
///   keyed without trailing slash for blobs, with trailing slash for trees.
///   Used by --remote source to substitute hash/size/blob_count/mtime.
fn fetch_remote_status_map(
    repo: &crate::repo::Repo,
    clone_root: &Hash,
    local: &dyn ObjectStore,
    scope: &ScopeFilter,
) -> (
    HashMap<String, char>,
    HashMap<String, TreeEntry>,
    HashMap<String, TreeEntry>,
) {
    // One root pointer for the synchronous pack_reader here; a second,
    // independent pointer is built below for the timeout worker thread (it
    // takes ownership).
    let (pack_reader, remote, remote_key) = match repo.pack_reader("origin", None) {
        Ok(v) => v,
        Err(_) => return (HashMap::new(), HashMap::new(), HashMap::new()),
    };
    // Bound the remote INDEX_ROOT lookup with a timeout. The read is a blocking
    // filesystem call today (and future network backends will also block), so it
    // runs on a detached worker thread; on timeout we abandon the wait and treat
    // it exactly like the error path (R column blank). The worker may outlive the
    // wait, so it owns clones of everything it touches (design/04 "Remote check
    // behaviour").
    let timeout_root_pointer = match repo.remote_root_pointer("origin") {
        Ok(p) => p,
        Err(_) => return (HashMap::new(), HashMap::new(), HashMap::new()),
    };
    let remote_root = match read_root_with_timeout(
        Box::new(remote),
        repo.local_store(),
        repo.packcache_store(),
        repo.objcache_store(),
        timeout_root_pointer,
        remote_key.clone(),
    ) {
        Some(h) => h,
        None => return (HashMap::new(), HashMap::new(), HashMap::new()),
    };
    if remote_root == *clone_root {
        return (HashMap::new(), HashMap::new(), HashMap::new());
    }
    let mut out = HashMap::new();
    let mut added_entries = HashMap::new();
    let mut all_remote_entries = HashMap::new();
    let _ = remote_diff_recursive(
        Some(clone_root),
        &remote_root,
        local,
        &pack_reader,
        remote_key.as_ref(),
        scope,
        &mut String::new(),
        &mut out,
        &mut added_entries,
        &mut all_remote_entries,
    );
    (out, added_entries, all_remote_entries)
}

/// Recursively compare `clone_hash` and `remote_hash`, collecting paths with their
/// remote status (`'M'`, `'A'`, `'D'`) into `out`.
/// `added_entries`: populated with the `TreeEntry` for each remote-added path ('A' status).
/// `all_remote_entries`: populated with the `TreeEntry` for all remote-changed paths (A and M).
/// Tree objects missing from `local` are fetched from `remote`.
/// Subtrees with matching hashes are skipped entirely (short-circuit).
#[allow(clippy::too_many_arguments)]
fn remote_diff_recursive(
    clone_hash: Option<&Hash>,
    remote_hash: &Hash,
    local: &dyn ObjectStore,
    remote: &dyn ObjectStore,
    remote_key: Option<&crate::codec::encrypt::EncryptKey>,
    scope: &ScopeFilter,
    prefix: &mut String,
    out: &mut HashMap<String, char>,
    added_entries: &mut HashMap<String, TreeEntry>,
    all_remote_entries: &mut HashMap<String, TreeEntry>,
) -> Result<(), Error> {
    if clone_hash == Some(remote_hash) {
        return Ok(());
    }

    let remote_data = codec::ensure_local_then_read(remote, local, remote_hash, remote_key)?;
    let Tree::Normal {
        entries: remote_entries,
    } = Tree::deserialise(&remote_data)?;

    let clone_entries: Vec<TreeEntry> = if let Some(ch) = clone_hash {
        if !local.exists(ch)? {
            Vec::new()
        } else {
            match codec::store_read(local, ch, None) {
                Ok(d) => match Tree::deserialise(&d) {
                    Ok(Tree::Normal { entries }) => entries,
                    Err(_) => Vec::new(),
                },
                Err(_) => Vec::new(),
            }
        }
    } else {
        Vec::new()
    };
    let clone_map: HashMap<String, Hash> = clone_entries
        .iter()
        .filter_map(|e| e.hash().map(|h| (e.name().to_string(), h.clone())))
        .collect();
    // Blob mode lookup (C4): clone_map only carries the hash, which is not
    // enough to detect a mode-only (chmod) remote change when the content
    // hash is unchanged -- mirrors the local diff_recursive's base_map, which
    // keeps the full TreeEntry for exactly this reason.
    let clone_blob_modes: HashMap<String, Option<String>> = clone_entries
        .iter()
        .filter_map(|e| match e {
            TreeEntry::Blob { name, mode, .. } => Some((name.clone(), mode.clone())),
            _ => None,
        })
        .collect();

    let remote_names: HashSet<&str> = remote_entries.iter().map(|e| e.name()).collect();

    for entry in &remote_entries {
        let name = entry.name();
        let entry_path = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{}/{}", prefix, name)
        };
        match entry {
            TreeEntry::Blob { hash, mode, .. } => {
                if !scope.in_scope(&entry_path) {
                    continue;
                }
                let status = if clone_map.contains_key(name) {
                    // A mode-only (chmod) change still rewrites the tree hash
                    // (see push's dirty detection), so report it as modified
                    // even when the blob content hash is unchanged.
                    let mode_changed = clone_blob_modes.get(name) != Some(mode);
                    if clone_map.get(name) != Some(hash) || mode_changed {
                        'M'
                    } else {
                        continue;
                    }
                } else {
                    added_entries.insert(entry_path.clone(), entry.clone());
                    'A'
                };
                all_remote_entries.insert(entry_path.clone(), entry.clone());
                out.insert(entry_path, status);
            }
            TreeEntry::Tree { hash, .. } => {
                let clone_child = clone_map.get(name);
                if clone_child == Some(hash) {
                    continue;
                }
                if !scope.should_descend(&entry_path) {
                    continue;
                }
                if scope.in_scope(&entry_path) {
                    let is_new = clone_child.is_none();
                    let dir_status = if is_new { 'A' } else { 'M' };
                    let dir_path = format!("{}/", entry_path);
                    if is_new {
                        added_entries.insert(dir_path.clone(), entry.clone());
                    }
                    all_remote_entries.insert(dir_path.clone(), entry.clone());
                    out.insert(dir_path, dir_status);
                }
                let prev = prefix.len();
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(name);
                let _ = remote_diff_recursive(
                    clone_child,
                    hash,
                    local,
                    remote,
                    remote_key,
                    scope,
                    prefix,
                    out,
                    added_entries,
                    all_remote_entries,
                );
                prefix.truncate(prev);
            }
            TreeEntry::Symlink { .. } => {
                if !scope.in_scope(&entry_path) {
                    continue;
                }
                let status = if clone_map.contains_key(name) {
                    'M'
                } else {
                    added_entries.insert(entry_path.clone(), entry.clone());
                    'A'
                };
                all_remote_entries.insert(entry_path.clone(), entry.clone());
                out.insert(entry_path, status);
            }
        }
    }

    for name in clone_map.keys() {
        if !remote_names.contains(name.as_str()) {
            let entry_path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", prefix, name)
            };
            if scope.in_scope(&entry_path) {
                out.insert(entry_path, 'D');
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// STAT_CACHE refresh
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::local::LocalStore;
    use tempfile::TempDir;

    fn blob_entry(name: &str, content: &[u8]) -> TreeEntry {
        TreeEntry::Blob {
            name: name.to_string(),
            hash: Hash::compute(content),
            mtime: None,
            size: content.len() as u64,
            mode: None,
        }
    }

    fn tree_entry(name: &str, hash: Hash) -> TreeEntry {
        TreeEntry::Tree {
            name: name.to_string(),
            hash,
            mtime: None,
            size: 0,
            blob_count: 0,
        }
    }

    /// Build and store a tree object from `entries`, returning its hash.
    fn store_tree(entries: Vec<TreeEntry>, store: &LocalStore) -> Hash {
        crate::tree_ops::build_and_store(entries, store).unwrap()
    }

    /// This is the crux of the fix: a `base` (clone-root) subtree whose own
    /// tree object is missing from `store` must not abort the whole diff.
    /// `diff_recursive`/`mark_deleted` catch `Error::ObjectNotFound` at each
    /// recursive descent and record the unresolved subtree in `unknown`
    /// instead of propagating the error, so a sibling subtree that IS
    /// resolvable is still diffed correctly.
    #[test]
    fn diff_trees_tolerates_missing_base_subtree_and_diffs_siblings() {
        let dir = TempDir::new().unwrap();
        let store = LocalStore::for_cache(dir.path());

        // "a": present only as a dangling hash reference in the base root --
        // its own tree object is deliberately never written (simulating an
        // unresolvable local cache miss with no remote to heal from).
        let a_base_hash = Hash::compute(b"a-base-never-written");

        // "b": a normal, fully-resolvable subtree on both sides, with a
        // blob whose content differs (base "old" vs target "new").
        let b_base_hash = store_tree(vec![blob_entry("file.txt", b"old")], &store);
        let b_target_hash = store_tree(vec![blob_entry("file.txt", b"new")], &store);

        let base_root = store_tree(
            vec![
                tree_entry("a", a_base_hash),
                tree_entry("b", b_base_hash.clone()),
            ],
            &store,
        );

        // "a"'s target-side tree object (the working tree always has a real,
        // freshly-written object here -- it is the base side that is missing).
        let a_target_hash = store_tree(vec![blob_entry("x.txt", b"x")], &store);
        let target_root = store_tree(
            vec![
                tree_entry("a", a_target_hash),
                tree_entry("b", b_target_hash),
            ],
            &store,
        );

        let (diff_map, unknown) = diff_trees(
            Some(&base_root),
            target_root,
            &store,
            &ScopeFilter::unscoped(),
        )
        .expect("a missing base subtree must not abort the whole diff");

        assert_eq!(
            unknown,
            std::collections::HashSet::from(["a".to_string()]),
            "the unresolvable subtree must be recorded as unknown, by its bare path"
        );
        assert!(
            !diff_map.contains_key("a"),
            "an unresolvable subtree must not be reported as a normal Added/Modified/Deleted entry"
        );
        assert!(
            matches!(diff_map.get("b/file.txt"), Some(DiffRow::Modified { .. })),
            "the sibling subtree 'b' must still be diffed correctly \
             (expected b/file.txt: Modified), got {:?}",
            diff_map.get("b/file.txt")
        );
    }

    /// A non-`ObjectNotFound` error must still propagate (not be swallowed):
    /// only an object-miss is tolerated. There is no store-level way to
    /// inject an arbitrary I/O error here without a custom `ObjectStore`, so
    /// this instead exercises the sibling-independence guarantee at a deeper
    /// nesting level, complementing the shallow case above.
    #[test]
    fn diff_trees_tolerates_missing_base_subtree_at_nested_depth() {
        let dir = TempDir::new().unwrap();
        let store = LocalStore::for_cache(dir.path());

        // base: root -> "src" -> "sub" (never written) and "src" -> "lib.rs" (blob).
        let sub_base_hash = Hash::compute(b"sub-base-never-written");
        let src_base_hash = store_tree(
            vec![
                tree_entry("sub", sub_base_hash),
                blob_entry("lib.rs", b"old lib"),
            ],
            &store,
        );
        let base_root = store_tree(vec![tree_entry("src", src_base_hash)], &store);

        // target: root -> "src" -> "sub" (freshly written, different content)
        // and "src" -> "lib.rs" (changed content).
        let sub_target_hash = store_tree(vec![blob_entry("inner.txt", b"inner")], &store);
        let src_target_hash = store_tree(
            vec![
                tree_entry("sub", sub_target_hash),
                blob_entry("lib.rs", b"new lib"),
            ],
            &store,
        );
        let target_root = store_tree(vec![tree_entry("src", src_target_hash)], &store);

        let (diff_map, unknown) = diff_trees(
            Some(&base_root),
            target_root,
            &store,
            &ScopeFilter::unscoped(),
        )
        .expect("a missing base subtree at any depth must not abort the whole diff");

        assert_eq!(
            unknown,
            std::collections::HashSet::from(["src/sub".to_string()]),
        );
        assert!(
            matches!(diff_map.get("src/lib.rs"), Some(DiffRow::Modified { .. })),
            "the sibling blob 'src/lib.rs' must still be diffed correctly, got {:?}",
            diff_map.get("src/lib.rs")
        );
    }

    /// A root whose own base tree object is unreadable must not crash `ls`
    /// either: `diff_trees` wraps the top-level `diff_recursive` call the
    /// same way each recursive descent wraps its children.
    #[test]
    fn diff_trees_tolerates_missing_root_base() {
        let dir = TempDir::new().unwrap();
        let store = LocalStore::for_cache(dir.path());

        let base_root = Hash::compute(b"root-base-never-written");
        let target_root = store_tree(vec![blob_entry("file.txt", b"content")], &store);

        let (diff_map, unknown) = diff_trees(
            Some(&base_root),
            target_root,
            &store,
            &ScopeFilter::unscoped(),
        )
        .expect("an unreadable root base must not abort ls");

        assert_eq!(unknown, std::collections::HashSet::from([".".to_string()]));
        assert!(diff_map.is_empty());
    }
}
