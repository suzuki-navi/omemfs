use std::fs;
use std::path::PathBuf;

use crate::codec;
use crate::dtimer_l1;
use crate::error::Error;
use crate::object::{Tree, TreeEntry};
use crate::repo::Repo;
use crate::store::ObjectStore;
use crate::stub::{self, StubRecord, StubTargetType};
use crate::term::{ColorChoice, Styles, color_enabled, paint};
use crate::tree_ops;

pub struct RestoreOptions {
    pub work_dir: PathBuf,
    /// Directory the command was invoked from; relative paths resolve against it.
    pub current_dir: PathBuf,
    /// Paths to restore. Empty means the entire working tree.
    pub paths: Vec<PathBuf>,
    pub dry_run: bool,
}

pub fn run(opts: RestoreOptions) -> Result<(), Error> {
    let repo = Repo::open(&opts.work_dir)?;
    crate::progress::notify_repo_dir(&repo.work_dir);
    let _lock = repo.acquire_lock()?;
    let _t = dtimer_l1!("restore");
    let clone_root = repo.read_clone_root()?.ok_or_else(|| {
        Error::Other("no clone_root — repository has never been synced".to_string())
    })?;
    let local = repo.local_store();

    // When no paths given, restore the entire working tree (the repo root),
    // regardless of the cwd. Unlike push/pull/ls, restore does not narrow its
    // no-argument default to the cwd subtree (design/04 restore Arguments).
    let paths = if opts.paths.is_empty() {
        vec![opts.work_dir.clone()]
    } else {
        opts.paths
    };

    let phase = crate::progress::begin_phase("Restore working tree");
    let mut counts = Counts::default();
    for path in &paths {
        let rel = crate::repo::normalize_path(path, &repo.work_dir, &opts.current_dir)?;
        if rel.is_empty() {
            // cwd is repo root: full restore
            restore_tree(
                &clone_root,
                &repo.work_dir,
                &repo.work_dir,
                &local,
                &repo.work_dir,
                opts.dry_run,
                &mut counts,
            )?;
            remove_added_files(
                &clone_root,
                &repo.work_dir,
                &repo.work_dir,
                &local,
                &repo.work_dir,
                opts.dry_run,
                &mut counts,
            )?;
        } else {
            restore_path(
                &rel,
                &clone_root,
                &repo.work_dir,
                &local,
                &repo.work_dir,
                opts.dry_run,
                &mut counts,
            )?;
        }
    }
    phase.complete(format!("{} files", counts.restored + counts.deleted));
    print_summary(&counts, opts.dry_run);
    Ok(())
}

// ---------------------------------------------------------------------------
// Core helpers
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Counts {
    restored: usize,
    deleted: usize,
}

/// Restore a single relative path (file or directory) from clone_root.
fn restore_path(
    rel: &str,
    clone_root: &crate::object::Hash,
    work_dir: &std::path::Path,
    store: &dyn ObjectStore,
    stub_root: &std::path::Path,
    dry_run: bool,
    counts: &mut Counts,
) -> Result<(), Error> {
    let components: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
    if components.is_empty() {
        // Empty path means full restore — handled by caller.
        return Ok(());
    }

    let entry = tree_ops::navigate_entry(clone_root, &components, store)?;
    let abs = work_dir.join(rel);

    match entry {
        None => {
            // Path absent from clone_root: delete file/stubs from the working tree.
            if abs.is_file() || stub::exists(stub_root, rel) {
                if dry_run {
                    print_action(true, "delete", rel);
                } else {
                    if abs.is_file() {
                        fs::remove_file(&abs)?;
                    }
                    stub::remove(stub_root, rel)?;
                    print_action(false, "delete", rel);
                }
                counts.deleted += 1;
            }
            if stub::dir_exists(stub_root, rel) {
                if dry_run {
                    print_action(true, "delete", rel);
                } else {
                    stub::remove_dir_stub(stub_root, rel)?;
                    // Remove the directory if it is now empty.
                    fs::remove_dir(&abs).ok();
                    print_action(false, "delete", rel);
                }
                counts.deleted += 1;
            }
            if !dry_run {
                remove_conflict_helpers(&abs);
            }
        }
        Some(TreeEntry::Blob {
            hash,
            mtime,
            mode,
            size,
            ..
        }) => {
            // Stub reconcile: update the stub record instead of materialising.
            if stub::exists(stub_root, rel) {
                reconcile_file_stub_to_blob(
                    stub_root, rel, &hash, size, &mtime, &mode, dry_run, counts,
                )?;
                return Ok(());
            }
            // Type mismatch: dir stub where clone_root has a blob.
            if stub::dir_exists(stub_root, rel) {
                reconcile_dir_stub_to_blob(
                    &abs, stub_root, rel, &hash, size, &mtime, &mode, dry_run, counts,
                )?;
                return Ok(());
            }

            // Check if already matching. The mode is only consulted when the
            // content matches (the file is then known to exist on disk).
            let content_matches = !needs_restore_blob(&abs, stub_root, rel, &hash);
            let already_matches = content_matches && crate::fsmeta::mode_matches(&abs, &mode);
            if already_matches && !conflict_helpers_exist(&abs) {
                return Ok(());
            }
            if dry_run {
                if !already_matches {
                    print_action(true, "restore", rel);
                    counts.restored += 1;
                }
                return Ok(());
            }
            if !content_matches {
                write_blob(&abs, &hash, &mtime, &mode, store, stub_root, rel)?;
                print_action(false, "restore", rel);
                counts.restored += 1;
            } else if !already_matches {
                // Content matches but the executable bit differs: chmod only,
                // no content rewrite.
                crate::fsmeta::apply_mode(&abs, &mode);
                print_action(false, "restore", rel);
                counts.restored += 1;
            }
            remove_conflict_helpers(&abs);
        }
        Some(TreeEntry::Tree {
            hash: tree_hash,
            mtime,
            size,
            blob_count,
            ..
        }) => {
            // Stub reconcile: update the dir stub record.
            if stub::dir_exists(stub_root, rel) {
                // Only update the stub record; do NOT recurse into the directory.
                // Blobs under a dir stub are not in the local cache (design/08_stub_system.md),
                // so recursing would cause spurious ObjectNotFound errors.
                reconcile_dir_stub_to_tree(
                    stub_root, rel, &tree_hash, size, &mtime, blob_count, dry_run, counts,
                )?;
            } else if stub::exists(stub_root, rel) {
                // Type mismatch: file stub where clone_root has a tree.
                reconcile_file_stub_to_tree(
                    &abs, stub_root, rel, &tree_hash, size, &mtime, blob_count, dry_run, counts,
                )?;
            } else {
                // Restore all descendants.
                restore_tree(
                    &tree_hash, &abs, work_dir, store, stub_root, dry_run, counts,
                )?;
                // Remove locally-added files under this directory.
                remove_added_files(
                    &tree_hash, &abs, work_dir, store, stub_root, dry_run, counts,
                )?;
            }
        }
        Some(TreeEntry::Symlink { target, mtime, .. }) => {
            // Materialise the symlink to match clone_root (design/08: symlinks
            // are always materialised). Skip when the on-disk entry is already a
            // symlink with the same target.
            if symlink_matches(&abs, &target) {
                return Ok(());
            }
            if dry_run {
                print_action(true, "restore", rel);
                counts.restored += 1;
                return Ok(());
            }
            stub::remove(stub_root, rel).ok();
            stub::remove_dir_stub(stub_root, rel).ok();
            crate::fsmeta::write_symlink_atomic(&abs, &target)?;
            crate::fsmeta::restore_symlink_mtime(&abs, &mtime);
            remove_conflict_helpers(&abs);
            print_action(false, "restore", rel);
            counts.restored += 1;
        }
    }
    Ok(())
}

/// Recursively write all blobs reachable from `tree_hash` into `base_dir`.
/// `work_dir` is the repository root (used for rel_path computation).
/// `stub_root` is passed to `stub::exists`/`stub::remove` (equals `work_dir`).
fn restore_tree(
    tree_hash: &crate::object::Hash,
    base_dir: &std::path::Path,
    work_dir: &std::path::Path,
    store: &dyn ObjectStore,
    stub_root: &std::path::Path,
    dry_run: bool,
    counts: &mut Counts,
) -> Result<(), Error> {
    let data = codec::store_read(store, tree_hash, None)?;
    let Tree::Normal { entries } = Tree::deserialise(&data)?;
    // Ensure the directory itself exists (handles empty directories).
    if !dry_run {
        fs::create_dir_all(base_dir)?;
    }
    for entry in entries {
        match entry {
            TreeEntry::Blob {
                name,
                hash,
                mtime,
                mode,
                size,
                ..
            } => {
                let abs = base_dir.join(&name);
                let rel = rel_path(&abs, work_dir);

                // Stub reconcile: update the stub record instead of materialising.
                if stub::exists(stub_root, &rel) {
                    reconcile_file_stub_to_blob(
                        stub_root, &rel, &hash, size, &mtime, &mode, dry_run, counts,
                    )?;
                    continue;
                }
                // Type mismatch: dir stub where clone_root has a blob.
                if stub::dir_exists(stub_root, &rel) {
                    reconcile_dir_stub_to_blob(
                        &abs, stub_root, &rel, &hash, size, &mtime, &mode, dry_run, counts,
                    )?;
                    continue;
                }

                // The mode is only consulted when the content matches (the
                // file is then known to exist on disk).
                let content_matches = !needs_restore_blob(&abs, stub_root, &rel, &hash);
                let already_matches = content_matches && crate::fsmeta::mode_matches(&abs, &mode);
                if already_matches && !conflict_helpers_exist(&abs) {
                    continue;
                }
                if dry_run {
                    if !already_matches {
                        print_action(true, "restore", &rel);
                        counts.restored += 1;
                    }
                    continue;
                }
                if !content_matches {
                    write_blob(&abs, &hash, &mtime, &mode, store, stub_root, &rel)?;
                    print_action(false, "restore", &rel);
                    counts.restored += 1;
                } else if !already_matches {
                    // Content matches but the executable bit differs: chmod
                    // only, no content rewrite.
                    crate::fsmeta::apply_mode(&abs, &mode);
                    print_action(false, "restore", &rel);
                    counts.restored += 1;
                }
                remove_conflict_helpers(&abs);
            }
            TreeEntry::Tree {
                name,
                hash: child_hash,
                mtime,
                size,
                blob_count,
                ..
            } => {
                let child_dir = base_dir.join(&name);
                let rel = rel_path(&child_dir, work_dir);

                // Stub reconcile: update the dir stub record.
                if stub::dir_exists(stub_root, &rel) {
                    // Only update the stub record; do NOT recurse into the directory.
                    // Blobs under a dir stub are not in the local cache, so recursing
                    // would cause spurious ObjectNotFound errors (design/08_stub_system.md).
                    reconcile_dir_stub_to_tree(
                        stub_root,
                        &rel,
                        &child_hash,
                        size,
                        &mtime,
                        blob_count,
                        dry_run,
                        counts,
                    )?;
                    continue;
                }
                // Type mismatch: file stub where clone_root has a tree.
                if stub::exists(stub_root, &rel) {
                    reconcile_file_stub_to_tree(
                        &child_dir,
                        stub_root,
                        &rel,
                        &child_hash,
                        size,
                        &mtime,
                        blob_count,
                        dry_run,
                        counts,
                    )?;
                    continue;
                }

                restore_tree(
                    &child_hash,
                    &child_dir,
                    work_dir,
                    store,
                    stub_root,
                    dry_run,
                    counts,
                )?;
            }
            TreeEntry::Symlink {
                name,
                target,
                mtime,
            } => {
                let abs = base_dir.join(&name);
                let rel = rel_path(&abs, work_dir);
                if symlink_matches(&abs, &target) {
                    continue;
                }
                if dry_run {
                    print_action(true, "restore", &rel);
                    counts.restored += 1;
                    continue;
                }
                stub::remove(stub_root, &rel).ok();
                stub::remove_dir_stub(stub_root, &rel).ok();
                crate::fsmeta::write_symlink_atomic(&abs, &target)?;
                crate::fsmeta::restore_symlink_mtime(&abs, &mtime);
                remove_conflict_helpers(&abs);
                print_action(false, "restore", &rel);
                counts.restored += 1;
            }
        }
    }
    Ok(())
}

/// Delete working-tree files under `base_dir` that are absent from the tree at `tree_hash`.
fn remove_added_files(
    tree_hash: &crate::object::Hash,
    base_dir: &std::path::Path,
    work_dir: &std::path::Path,
    store: &dyn ObjectStore,
    stub_root: &std::path::Path,
    dry_run: bool,
    counts: &mut Counts,
) -> Result<(), Error> {
    let data = codec::store_read(store, tree_hash, None)?;
    let Tree::Normal { entries } = Tree::deserialise(&data)?;
    let known: std::collections::HashSet<String> =
        entries.iter().map(|e| e.name().to_string()).collect();

    let read_dir = match fs::read_dir(base_dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::Io(e)),
    };

    for entry in read_dir {
        let entry = entry.map_err(Error::Io)?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name == ".omemfs" {
            continue;
        }
        // File stubs: check whether the logical entry is still in clone_root.
        if stub::is_stub_filename(&name) {
            // `.omemfs-stub` is the dir-stub marker; it belongs to this directory
            // and is handled via the directory's own entry in the parent tree.
            if name != stub::DIR_STUB_NAME
                && let Some(logical) = stub::logical_name(&name)
                && !logical.is_empty()
                && !known.contains(logical)
            {
                let stub_abs = base_dir.join(&name);
                let logical_rel = rel_path(&base_dir.join(logical), work_dir);
                if dry_run {
                    print_action(true, "delete", &logical_rel);
                } else {
                    fs::remove_file(&stub_abs).map_err(Error::Io)?;
                    print_action(false, "delete", &logical_rel);
                }
                counts.deleted += 1;
            }
            continue;
        }
        let file_type = entry.file_type().map_err(Error::Io)?;
        if known.contains(&name) {
            // Recurse into known subdirectories that are trees in clone_root.
            let abs = base_dir.join(&name);
            if file_type.is_dir() {
                let child_rel = rel_path(&abs, work_dir);
                // A fully-stubbed directory must not be recursed into: its
                // clone_root tree object (and descendant blobs) are not in the
                // local cache after a lazy clone, so reading them would fail with
                // ObjectNotFound. The dir stub's own entry is reconciled in
                // restore_tree; there are no locally-added files to remove inside
                // a fully-stubbed directory. See design/08_stub_system.md.
                if !stub::dir_exists(stub_root, &child_rel)
                    && let Some(child_entry) = entries.iter().find(|e| e.name() == name)
                    && let TreeEntry::Tree {
                        hash: child_hash, ..
                    } = child_entry
                {
                    remove_added_files(
                        child_hash, &abs, work_dir, store, stub_root, dry_run, counts,
                    )?;
                }
            }
        } else {
            // Not in clone_root: delete.
            let abs = base_dir.join(&name);
            let rel = rel_path(&abs, work_dir);
            if file_type.is_file() || file_type.is_symlink() {
                if dry_run {
                    print_action(true, "delete", &rel);
                } else {
                    fs::remove_file(&abs)?;
                    stub::remove(stub_root, &rel)?;
                    print_action(false, "delete", &rel);
                }
                counts.deleted += 1;
            } else if file_type.is_dir() {
                // Recursively delete added directories (including any stubs inside).
                remove_added_dir(&abs, work_dir, stub_root, dry_run, counts)?;
                if !dry_run {
                    // Remove the directory itself once contents are cleared.
                    fs::remove_dir(&abs).ok();
                }
            }
        }
    }
    Ok(())
}

/// Recursively delete all files in an added directory.
fn remove_added_dir(
    dir: &std::path::Path,
    work_dir: &std::path::Path,
    stub_root: &std::path::Path,
    dry_run: bool,
    counts: &mut Counts,
) -> Result<(), Error> {
    let read_dir = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::Io(e)),
    };
    for entry in read_dir {
        let entry = entry.map_err(Error::Io)?;
        let abs = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        // Delete stub files inside removed directories silently (they are an
        // implementation detail of the parent directory being removed).
        if stub::is_stub_filename(&name) {
            if !dry_run {
                let _ = fs::remove_file(&abs);
            }
            continue;
        }
        let rel = rel_path(&abs, work_dir);
        let file_type = entry.file_type().map_err(Error::Io)?;
        if file_type.is_file() || file_type.is_symlink() {
            if dry_run {
                print_action(true, "delete", &rel);
            } else {
                fs::remove_file(&abs)?;
                stub::remove(stub_root, &rel)?;
                print_action(false, "delete", &rel);
            }
            counts.deleted += 1;
        } else if file_type.is_dir() {
            remove_added_dir(&abs, work_dir, stub_root, dry_run, counts)?;
            if !dry_run {
                fs::remove_dir(&abs).ok();
            }
        }
    }
    Ok(())
}

/// Write a blob to `abs_path`, restoring mtime and the executable-bit mode.
/// Removes any stub for this path.
fn write_blob(
    abs: &std::path::Path,
    hash: &crate::object::Hash,
    mtime: &Option<chrono::DateTime<chrono::Utc>>,
    mode: &Option<String>,
    store: &dyn ObjectStore,
    stub_root: &std::path::Path,
    rel: &str,
) -> Result<(), Error> {
    if let Some(parent) = abs.parent() {
        fs::create_dir_all(parent)?;
    }
    crate::fsmeta::materialise_blob_at(store, hash, abs, mtime, mode)?;
    stub::remove(stub_root, rel)?;
    Ok(())
}

/// Returns true if `abs` is already a symlink pointing at `target`.
fn symlink_matches(abs: &std::path::Path, target: &str) -> bool {
    match fs::symlink_metadata(abs) {
        Ok(md) if md.file_type().is_symlink() => fs::read_link(abs)
            .map(|t| t.to_string_lossy() == target)
            .unwrap_or(false),
        _ => false,
    }
}

/// Returns `true` if the file needs to be (re)written from the store.
fn needs_restore_blob(
    abs: &std::path::Path,
    stub_root: &std::path::Path,
    rel: &str,
    expected_hash: &crate::object::Hash,
) -> bool {
    // If a stub exists, the real file is absent — needs restore.
    if stub::exists(stub_root, rel) {
        return true;
    }
    // If the file is absent, needs restore.
    let content = match fs::read(abs) {
        Ok(c) => c,
        Err(_) => return true,
    };
    // Check content hash.
    crate::object::blob_hash(&content) != *expected_hash
}

// ---------------------------------------------------------------------------
// Stub reconcile helpers (design/08_stub_system.md — reconcile table)
// ---------------------------------------------------------------------------

/// blob entry in clone_root / file stub on disk: update stub fields if stale.
fn reconcile_file_stub_to_blob(
    stub_root: &std::path::Path,
    rel: &str,
    hash: &crate::object::Hash,
    size: u64,
    mtime: &Option<chrono::DateTime<chrono::Utc>>,
    mode: &Option<String>,
    dry_run: bool,
    counts: &mut Counts,
) -> Result<(), Error> {
    let current = stub::read(stub_root, rel)?;
    let needs_update = current
        .is_none_or(|r| r.hash != *hash || r.size != size || r.mtime != *mtime || r.mode != *mode);
    if !needs_update {
        return Ok(());
    }
    if dry_run {
        print_action(true, "restore", rel);
        counts.restored += 1;
        return Ok(());
    }
    stub::write(
        stub_root,
        rel,
        &StubRecord {
            target_type: StubTargetType::Blob,
            hash: hash.clone(),
            size,
            mtime: *mtime,
            mode: mode.clone(),
            blob_count: 0,
        },
    )?;
    print_action(false, "restore", rel);
    counts.restored += 1;
    Ok(())
}

/// tree entry in clone_root / dir stub on disk: update stub fields if stale.
fn reconcile_dir_stub_to_tree(
    stub_root: &std::path::Path,
    rel: &str,
    hash: &crate::object::Hash,
    size: u64,
    mtime: &Option<chrono::DateTime<chrono::Utc>>,
    blob_count: u64,
    dry_run: bool,
    counts: &mut Counts,
) -> Result<(), Error> {
    let current = stub::read_dir_stub(stub_root, rel)?;
    let needs_update = current.is_none_or(|r| {
        r.hash != *hash || r.size != size || r.mtime != *mtime || r.blob_count != blob_count
    });
    if !needs_update {
        return Ok(());
    }
    if dry_run {
        print_action(true, "restore", rel);
        counts.restored += 1;
        return Ok(());
    }
    stub::write_dir_stub(
        stub_root,
        rel,
        &StubRecord {
            target_type: StubTargetType::Tree,
            hash: hash.clone(),
            size,
            mtime: *mtime,
            mode: None,
            blob_count,
        },
    )?;
    print_action(false, "restore", rel);
    counts.restored += 1;
    Ok(())
}

/// blob entry in clone_root / dir stub on disk (type mismatch): replace dir stub with file stub.
fn reconcile_dir_stub_to_blob(
    abs: &std::path::Path,
    stub_root: &std::path::Path,
    rel: &str,
    hash: &crate::object::Hash,
    size: u64,
    mtime: &Option<chrono::DateTime<chrono::Utc>>,
    mode: &Option<String>,
    dry_run: bool,
    counts: &mut Counts,
) -> Result<(), Error> {
    if dry_run {
        print_action(true, "restore", rel);
        counts.restored += 1;
        return Ok(());
    }
    stub::remove_dir_stub(stub_root, rel)?;
    if abs.exists() {
        fs::remove_dir_all(abs)?;
    }
    stub::write(
        stub_root,
        rel,
        &StubRecord {
            target_type: StubTargetType::Blob,
            hash: hash.clone(),
            size,
            mtime: *mtime,
            mode: mode.clone(),
            blob_count: 0,
        },
    )?;
    print_action(false, "restore", rel);
    counts.restored += 1;
    Ok(())
}

/// tree entry in clone_root / file stub on disk (type mismatch): replace file stub with dir stub.
fn reconcile_file_stub_to_tree(
    abs: &std::path::Path,
    stub_root: &std::path::Path,
    rel: &str,
    hash: &crate::object::Hash,
    size: u64,
    mtime: &Option<chrono::DateTime<chrono::Utc>>,
    blob_count: u64,
    dry_run: bool,
    counts: &mut Counts,
) -> Result<(), Error> {
    if dry_run {
        print_action(true, "restore", rel);
        counts.restored += 1;
        return Ok(());
    }
    stub::remove(stub_root, rel)?;
    fs::create_dir_all(abs)?;
    stub::write_dir_stub(
        stub_root,
        rel,
        &StubRecord {
            target_type: StubTargetType::Tree,
            hash: hash.clone(),
            size,
            mtime: *mtime,
            mode: None,
            blob_count,
        },
    )?;
    print_action(false, "restore", rel);
    counts.restored += 1;
    Ok(())
}

fn print_summary(counts: &Counts, dry_run: bool) {
    let total = counts.restored + counts.deleted;
    if total == 0 {
        print_action(false, "plain", "Nothing to restore.");
        return;
    }
    if dry_run {
        print_action(
            false,
            "plain",
            &format!("{} path(s) would be restored.", total),
        );
    } else {
        print_action(false, "plain", &format!("{} path(s) restored.", total));
    }
}

/// Print a single action line. `kind` selects a color:
/// "restore" → added (green), "delete" → deleted (red), "plain" → unstyled.
///
/// Uses `progress::emit_output_line` so the line is interleaved correctly
/// with the TTY progress window.
/// Print one restore/delete action line for `path`, or an already-fully-
/// formatted summary line when `kind == "plain"` (`path` is then the whole
/// message, e.g. "3 path(s) restored.").
///
/// For `"restore"`/`"delete"`, the "  <verb>: <path>" text is built exactly
/// once, here, from the verb and `path` directly. The previous version took
/// a pre-formatted message from the caller and re-parsed it by stripping a
/// hardcoded prefix to recover `path` for coloring -- the caller formats,
/// the callee un-formats -- so a one-character change to either prefix
/// string would silently break coloring (refactor-instructions.md F2).
fn print_action(dry_run: bool, kind: &str, path: &str) {
    let colored = color_enabled(ColorChoice::Auto, atty::is(atty::Stream::Stdout));
    let styles = Styles::new();
    let text = match kind {
        "restore" => {
            let prefix = if dry_run { "would restore" } else { "restored" };
            if colored {
                format!("  {}: {}", paint(colored, styles.added, prefix), path)
            } else {
                format!("  {}: {}", prefix, path)
            }
        }
        "delete" => {
            let prefix = if dry_run { "would delete" } else { "deleted" };
            if colored {
                format!("  {}: {}", paint(colored, styles.deleted, prefix), path)
            } else {
                format!("  {}: {}", prefix, path)
            }
        }
        _ => path.to_string(),
    };
    crate::progress::emit_output_line(&text);
}

/// Remove the three conflict helper files for `base_path` if they exist.
fn remove_conflict_helpers(base_path: &std::path::Path) {
    for suffix in crate::commands::conflict::CONFLICT_SUFFIXES {
        let p = helper_path(base_path, suffix);
        if p.exists() {
            let _ = fs::remove_file(&p);
        }
    }
}

/// Returns true if any conflict helper file exists alongside `base_path`.
fn conflict_helpers_exist(base_path: &std::path::Path) -> bool {
    crate::commands::conflict::CONFLICT_SUFFIXES
        .iter()
        .any(|suffix| helper_path(base_path, suffix).exists())
}

fn helper_path(base: &std::path::Path, suffix: &str) -> std::path::PathBuf {
    let s = base.to_string_lossy();
    std::path::PathBuf::from(format!("{}{}", s, suffix))
}

fn rel_path(abs: &std::path::Path, work_dir: &std::path::Path) -> String {
    abs.strip_prefix(work_dir)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| abs.to_string_lossy().replace('\\', "/"))
}
