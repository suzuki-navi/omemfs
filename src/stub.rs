/// Stub record: persisted metadata for a file or directory that has been deferred
/// from the working tree.
///
/// File stub:      `<path>.omemfs-stub`   (alongside the original file path)
/// Directory stub: `<dir>/.omemfs-stub`   (inside the original directory)
///
/// The working tree file/directory contents are absent; the stub records enough
/// information for scan to reconstruct the correct TreeEntry and for expand to
/// materialise the entry from the local object cache or remote.
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::object::{Hash, TreeEntry};

/// Suffix appended to the original filename to form a file stub filename.
pub const STUB_SUFFIX: &str = ".omemfs-stub";

/// Filename used inside a directory to form a directory stub.
pub const DIR_STUB_NAME: &str = ".omemfs-stub";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum StubTargetType {
    #[default]
    Blob,
    Tree,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StubRecord {
    #[serde(default)]
    pub target_type: StubTargetType,
    pub hash: Hash,
    pub size: u64,
    pub mtime: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Number of blobs reachable from this entry (for Tree stubs; 0 for Blob stubs).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub blob_count: u64,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Return the file stub path for a given original file path.
/// e.g. `work_dir/foo/bar.txt` → `work_dir/foo/bar.txt.omemfs-stub`
pub fn file_stub_path_for(original: &Path) -> PathBuf {
    let mut s = original.as_os_str().to_owned();
    s.push(STUB_SUFFIX);
    PathBuf::from(s)
}

/// Return the directory stub path for a given directory path.
/// e.g. `work_dir/foo/bar/` → `work_dir/foo/bar/.omemfs-stub`
pub fn dir_stub_path_for(dir: &Path) -> PathBuf {
    dir.join(DIR_STUB_NAME)
}

/// Return the file stub path given work_dir and a repo-relative path.
fn file_stub_abs(work_dir: &Path, rel_path: &str) -> PathBuf {
    file_stub_path_for(&rel_to_abs(work_dir, rel_path))
}

/// Return the directory stub path given work_dir and a repo-relative directory path.
fn dir_stub_abs(work_dir: &Path, rel_path: &str) -> PathBuf {
    dir_stub_path_for(&rel_to_abs(work_dir, rel_path))
}

fn rel_to_abs(work_dir: &Path, rel_path: &str) -> PathBuf {
    let trimmed = rel_path.trim_start_matches('/');
    let mut p = work_dir.to_path_buf();
    for component in trimmed.split('/') {
        if !component.is_empty() {
            p.push(component);
        }
    }
    p
}

// ---------------------------------------------------------------------------
// Git worktree detection
// ---------------------------------------------------------------------------

/// Find the nearest enclosing Git working tree root at or above `path` but
/// strictly below `work_dir`. Returns the directory that directly contains the
/// `.git` entry, or `None` when `path` is not inside a nested Git working tree.
///
/// The omemfs repo root itself being a Git root is intentionally excluded
/// (stubbing the entire repo as a unit is permitted; see design/08).
fn enclosing_git_worktree(path: &Path, work_dir: &Path) -> Option<PathBuf> {
    // Start at the path's own directory (a stub file's directory) and walk up.
    let mut current = if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    };
    loop {
        if current == work_dir || !current.starts_with(work_dir) {
            return None;
        }
        if current.join(".git").exists() {
            return Some(current.to_path_buf());
        }
        match current.parent() {
            Some(p) => current = p,
            None => return None,
        }
    }
}

/// Determine whether a `.omemfs-stub` file placed at `stub_abs` would be visible
/// to Git, i.e. whether placing a stub there is disallowed (design/08 "Stubs and
/// Git repositories").
///
/// Returns `true` (visible → stubbing disallowed) when `stub_abs` is inside a
/// nested Git working tree AND the stub file is NOT matched by `.gitignore`.
/// Visibility is determined by invoking `git check-ignore` in the containing Git
/// working tree:
/// - exit 0 (the path is ignored)        → not visible → returns `false`
/// - exit 1 (the path is not ignored)    → visible     → returns `true`
/// - git absent / any error / non-0/1    → fail-safe: treated as visible → `true`
///
/// When `stub_abs` is not inside a nested Git working tree, returns `false`
/// (the Git rule does not apply).
pub fn stub_would_be_visible_to_git(stub_abs: &Path, work_dir: &Path) -> bool {
    let git_root = match enclosing_git_worktree(stub_abs, work_dir) {
        Some(r) => r,
        None => return false,
    };
    // The path passed to `git check-ignore` must be relative to the worktree
    // root (or absolute); use the absolute path for robustness.
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(&git_root)
        .arg("check-ignore")
        .arg("-q")
        .arg(stub_abs)
        .output();
    match output {
        Ok(out) => match out.status.code() {
            Some(0) => false, // ignored by Git → not visible → stubbing allowed
            Some(1) => true,  // not ignored → visible → stubbing disallowed
            _ => true,        // error (e.g. exit 128) → fail-safe: visible
        },
        Err(_) => true, // git not installed / failed to run → fail-safe: visible
    }
}

// ---------------------------------------------------------------------------
// Existence checks
// ---------------------------------------------------------------------------

/// Check whether a file stub (`<path>.omemfs-stub`) exists for `rel_path`.
pub fn exists(work_dir: &Path, rel_path: &str) -> bool {
    file_stub_abs(work_dir, rel_path).is_file()
}

/// Check whether a directory stub (`<dir>/.omemfs-stub`) exists for `rel_path`.
pub fn dir_exists(work_dir: &Path, rel_path: &str) -> bool {
    dir_stub_abs(work_dir, rel_path).is_file()
}

// ---------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------

/// Read the file stub record for `rel_path`.
pub fn read(work_dir: &Path, rel_path: &str) -> Result<Option<StubRecord>, Error> {
    read_from_path(&file_stub_abs(work_dir, rel_path))
}

/// Read the directory stub record for `rel_path`.
pub fn read_dir_stub(work_dir: &Path, rel_path: &str) -> Result<Option<StubRecord>, Error> {
    read_from_path(&dir_stub_abs(work_dir, rel_path))
}

fn read_from_path(path: &Path) -> Result<Option<StubRecord>, Error> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(Some(serde_json::from_str(&s)?)),
        // `NotADirectory` (ENOTDIR): some component ABOVE the leaf that used
        // to be a directory is no longer one -- e.g. `read_dir_stub(_,
        // "bigdir/x")` builds the path "bigdir/x/.omemfs-stub", and "bigdir"
        // itself was just switched from a directory to a plain file by an
        // in-progress type-change apply/reconcile. This is exactly as absent
        // as a plain ENOENT for the purposes of "is there a stub record
        // here": there is no directory left to hold one. Callers that walk
        // every ancestor of a path unconditionally (`deepest_ancestor_stub`,
        // used by pull's apply step to test each proper ancestor) have no
        // other way to know a shallower ancestor just changed kind under
        // them, so this must not be a hard I/O error.
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(None)
        }
        Err(e) => Err(Error::Io(e)),
    }
}

// ---------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------

/// Write a file stub record for `rel_path`.
pub fn write(work_dir: &Path, rel_path: &str, record: &StubRecord) -> Result<(), Error> {
    write_to_path(work_dir, &file_stub_abs(work_dir, rel_path), record)
}

/// Write a directory stub record for `rel_path`.
pub fn write_dir_stub(work_dir: &Path, rel_path: &str, record: &StubRecord) -> Result<(), Error> {
    write_to_path(work_dir, &dir_stub_abs(work_dir, rel_path), record)
}

fn write_to_path(work_dir: &Path, path: &Path, record: &StubRecord) -> Result<(), Error> {
    // Same guard as blob/symlink materialisation (design/03_sync_model.md,
    // "Write safety: the working-tree write boundary"): a stub record is
    // still a write into the working tree, and must not follow a directory
    // that has been swapped for a symlink pointing outside `work_dir`.
    // `atomic_write_durable` (fsync file + dir) matches the previous
    // `store::local::atomic_write` call this replaces.
    let json = serde_json::to_string(record).map_err(Error::Json)?;
    crate::fsguard::atomic_write_durable(work_dir, path, json.as_bytes())
}

// ---------------------------------------------------------------------------
// Remove
// ---------------------------------------------------------------------------

/// Remove the file stub for `rel_path` (no-op if it does not exist).
pub fn remove(work_dir: &Path, rel_path: &str) -> Result<(), Error> {
    crate::fsguard::remove_file_tolerant(work_dir, &file_stub_abs(work_dir, rel_path))
}

/// Remove the directory stub for `rel_path` (no-op if it does not exist).
pub fn remove_dir_stub(work_dir: &Path, rel_path: &str) -> Result<(), Error> {
    crate::fsguard::remove_file_tolerant(work_dir, &dir_stub_abs(work_dir, rel_path))
}

// ---------------------------------------------------------------------------
// List (all stubs in working tree)
// ---------------------------------------------------------------------------

/// Enumerate all stubs recursively under `work_dir`.
/// Returns `(rel_path, StubRecord)` pairs.
/// - File stubs: `rel_path` is the logical file path (without `.omemfs-stub` suffix).
/// - Directory stubs: `rel_path` is the directory path (without trailing slash).
pub fn list(work_dir: &Path) -> Result<Vec<(String, StubRecord)>, Error> {
    list_under(work_dir, work_dir)
}

/// Like [`list`], but bounded to stubs found under `dir` (a subdirectory of
/// `root`, or `root` itself), while still returning `rel_path`s relative to
/// `root` -- so a caller can scope the walk (e.g. a scoped pull's own
/// subtree) without losing the repo-root-relative paths its `clone_root`
/// navigation needs. `root` must be an ancestor of (or equal to) `dir`.
pub fn list_under(root: &Path, dir: &Path) -> Result<Vec<(String, StubRecord)>, Error> {
    let mut result = Vec::new();
    collect(root, dir, &mut result)?;
    Ok(result)
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, StubRecord)>) -> Result<(), Error> {
    let rd = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::Io(e)),
    };
    let mut has_dir_stub = false;
    let mut entries_to_recurse: Vec<PathBuf> = Vec::new();

    for entry in rd.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();

        if name == ".omemfs" {
            continue;
        }

        if path.is_file() && name == DIR_STUB_NAME {
            // Directory stub for this directory.
            if let Ok(Some(record)) = read_from_path(&path) {
                let rel = dir
                    .strip_prefix(root)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if !rel.is_empty() {
                    out.push((rel, record));
                }
            }
            has_dir_stub = true;
            continue;
        }

        if path.is_dir() {
            entries_to_recurse.push(path);
            continue;
        }

        if path.is_file() && name.ends_with(STUB_SUFFIX) {
            // File stub: derive logical rel_path by stripping suffix.
            let logical_path = path.with_file_name(&name[..name.len() - STUB_SUFFIX.len()]);
            let rel = logical_path
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            if rel.is_empty() {
                continue;
            }
            if let Ok(s) = fs::read_to_string(&path)
                && let Ok(record) = serde_json::from_str::<StubRecord>(&s)
            {
                out.push((rel, record));
            }
        }
    }

    // Only recurse into subdirectories if this directory is NOT itself dir-stubbed.
    if !has_dir_stub {
        for sub in entries_to_recurse {
            collect(root, &sub, out)?;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Stub frontier lookup (used by `expand` to resolve a path nested inside an
// unexpanded directory stub -- design/04_cli_spec.md, "Paths below the stub
// frontier").
// ---------------------------------------------------------------------------

/// Find the deepest on-disk directory-stub record that is a *proper* ancestor
/// of `rel_path` (never `rel_path` itself -- callers only reach this after a
/// direct match on `rel_path` has already failed).
///
/// Checks every ancestor directory from deepest to shallowest and returns the
/// first (deepest) one with an on-disk directory-stub marker, along with its
/// `rel_path` and `StubRecord`. Returns `None` when no ancestor is stubbed.
///
/// Deepest-first also makes this naturally resume a previously interrupted
/// `expand` lowering: if an ancestor was already partially lowered, this walk
/// finds whichever deeper level is still marked, not the original top-level
/// ancestor.
pub fn deepest_ancestor_stub(
    work_dir: &Path,
    rel_path: &str,
) -> Result<Option<(String, StubRecord)>, Error> {
    let components: Vec<&str> = rel_path.split('/').filter(|c| !c.is_empty()).collect();
    for end in (1..components.len()).rev() {
        let ancestor = components[..end].join("/");
        if let Some(record) = read_dir_stub(work_dir, &ancestor)? {
            return Ok(Some((ancestor, record)));
        }
    }
    Ok(None)
}

/// Find the deepest on-disk FILE stub record that is `rel_path` itself, or a
/// proper ancestor of it. Unlike `deepest_ancestor_stub` (which only ever
/// considers proper ancestors, because its callers have already ruled out a
/// direct match at the path itself), this also checks `rel_path` itself
/// first -- pull's apply step (design/08_stub_system.md, "Interaction with
/// pull's ordinary diff/apply", the file-stub mirror-image paragraph) needs
/// to recognise both shapes a blob-to-tree type change's diff entries can
/// take: `Deleted(f.txt)` where `f.txt` IS the file stub, and
/// `Added(f.txt/inner.txt)` where `f.txt` is a proper ancestor of it.
///
/// Checks `rel_path` itself, then every ancestor directory from deepest to
/// shallowest, returning the first one with an on-disk file-stub marker,
/// along with its `rel_path` and `StubRecord`. Returns `None` when neither
/// `rel_path` nor any ancestor is a file stub.
pub fn file_stub_at_or_above(
    work_dir: &Path,
    rel_path: &str,
) -> Result<Option<(String, StubRecord)>, Error> {
    if let Some(record) = read(work_dir, rel_path)? {
        return Ok(Some((rel_path.to_string(), record)));
    }
    let components: Vec<&str> = rel_path.split('/').filter(|c| !c.is_empty()).collect();
    for end in (1..components.len()).rev() {
        let ancestor = components[..end].join("/");
        if let Some(record) = read(work_dir, &ancestor)? {
            return Ok(Some((ancestor, record)));
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Filename helpers
// ---------------------------------------------------------------------------

/// Returns true if `name` is a file stub filename (ends with `.omemfs-stub`).
pub fn is_stub_filename(name: &str) -> bool {
    name.ends_with(STUB_SUFFIX)
}

/// Given a file stub filename, return the logical filename (strip `.omemfs-stub` suffix).
/// Returns `None` if the name does not end with the suffix.
pub fn logical_name(stub_name: &str) -> Option<&str> {
    stub_name.strip_suffix(STUB_SUFFIX)
}

// ---------------------------------------------------------------------------
// Reconcile (design/08_stub_system.md, "Stub reconcile (pull and restore)")
// ---------------------------------------------------------------------------

/// One outcome of checking an existing stub record against the corresponding
/// entry (if any) in a new `clone_root`, per the reconcile table in
/// design/08_stub_system.md. `reconcile_action` (below) is the single place
/// that table's decision is made; `restore.rs`'s `plan_reconcile_*` family
/// duplicates the same decisions inline as part of its own `Plan`/`Action`
/// walk (predating this shared helper -- restore has its own execution
/// model, so it is not rebuilt on top of this enum), while `pull.rs`'s
/// `reconcile_stubs` (added for D7) matches on this enum directly.
#[derive(Debug, Clone, PartialEq)]
pub enum ReconcileAction {
    /// Already in sync: `clone_root`'s entry at this path is the stub's own
    /// kind (blob for a file stub, tree for a dir stub) and every recorded
    /// field already matches. Nothing to do.
    UpToDate,
    /// `clone_root` still has a blob at this path (file stub case) or a tree
    /// (dir stub case), but at least one recorded field is stale. Carries the
    /// fresh `StubRecord` to write in place of the current one.
    UpdateFields(StubRecord),
    /// The dir stub at this path must become a file stub: `clone_root` now
    /// has a blob here. Carries the new file-stub `StubRecord` to write after
    /// the directory (and its `.omemfs-stub` marker) are removed.
    SwitchToFileStub(StubRecord),
    /// The file stub at this path must become a dir stub: `clone_root` now
    /// has a tree here. Carries the new dir-stub `StubRecord` to write inside
    /// the freshly-created directory, after the file stub marker is removed.
    SwitchToDirStub(StubRecord),
    /// `clone_root` now has a symlink here: symlinks are always materialised
    /// (never stubbed), so the stub (file or dir) must be removed and the
    /// symlink created in its place. Carries the symlink's `target`/`mtime`.
    MaterialiseSymlink {
        target: String,
        mtime: Option<DateTime<Utc>>,
    },
    /// `clone_root` has no entry at all at this path (deleted on the
    /// remote): the stub (file or dir) must be removed.
    Remove,
}

/// Decide the reconcile action for a stub whose current on-disk record is
/// `current` (as `read`/`read_dir_stub` returned it) and whose corresponding
/// `clone_root` entry is `clone_entry` (as `tree_ops::navigate_entry`
/// returned it -- `None` when the path is absent from `clone_root`).
///
/// `is_dir_stub` distinguishes a file stub from a directory stub: it decides
/// which `StubTargetType`/table row applies when `clone_entry` matches the
/// stub's own kind (a dir stub facing a `Tree`, or a file stub facing a
/// `Blob`), and therefore whether the *fields-only* update path
/// (`UpdateFields`) or a *kind-switch* path is taken for a given
/// `clone_entry` shape.
///
/// This is a pure decision function: it reads no filesystem state beyond
/// what the caller already passed in, and performs no I/O of its own. See
/// `ReconcileAction`'s variants for what each outcome means and what the
/// caller (`pull::reconcile_stubs`) must do to apply it.
pub fn reconcile_action(
    current: &StubRecord,
    clone_entry: Option<&TreeEntry>,
    is_dir_stub: bool,
) -> ReconcileAction {
    match clone_entry {
        None => ReconcileAction::Remove,
        Some(TreeEntry::Symlink { target, mtime, .. }) => ReconcileAction::MaterialiseSymlink {
            target: target.clone(),
            mtime: *mtime,
        },
        Some(TreeEntry::Blob {
            hash,
            size,
            mtime,
            mode,
            ..
        }) => {
            if is_dir_stub {
                // Type mismatch (dir stub / clone_root blob): unconditional
                // switch, matching restore.rs's plan_reconcile_dir_stub_to_blob
                // (no needs-update gating for a type change).
                ReconcileAction::SwitchToFileStub(StubRecord {
                    target_type: StubTargetType::Blob,
                    hash: hash.clone(),
                    size: *size,
                    mtime: *mtime,
                    mode: mode.clone(),
                    blob_count: 0,
                })
            } else if current.hash != *hash
                || current.size != *size
                || current.mtime != *mtime
                || current.mode != *mode
            {
                ReconcileAction::UpdateFields(StubRecord {
                    target_type: StubTargetType::Blob,
                    hash: hash.clone(),
                    size: *size,
                    mtime: *mtime,
                    mode: mode.clone(),
                    blob_count: 0,
                })
            } else {
                ReconcileAction::UpToDate
            }
        }
        Some(TreeEntry::Tree {
            hash,
            size,
            mtime,
            blob_count,
            ..
        }) => {
            if is_dir_stub {
                if current.hash != *hash
                    || current.size != *size
                    || current.mtime != *mtime
                    || current.blob_count != *blob_count
                {
                    ReconcileAction::UpdateFields(StubRecord {
                        target_type: StubTargetType::Tree,
                        hash: hash.clone(),
                        size: *size,
                        mtime: *mtime,
                        mode: None,
                        blob_count: *blob_count,
                    })
                } else {
                    ReconcileAction::UpToDate
                }
            } else {
                // Type mismatch (file stub / clone_root tree): unconditional
                // switch, matching restore.rs's plan_reconcile_file_stub_to_tree.
                ReconcileAction::SwitchToDirStub(StubRecord {
                    target_type: StubTargetType::Tree,
                    hash: hash.clone(),
                    size: *size,
                    mtime: *mtime,
                    mode: None,
                    blob_count: *blob_count,
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn dummy_record() -> StubRecord {
        StubRecord {
            target_type: StubTargetType::Tree,
            hash: Hash::compute(b"dummy"),
            size: 100,
            mtime: None,
            mode: None,
            blob_count: 0,
        }
    }

    #[test]
    fn read_dir_stub_returns_none_when_an_intermediate_component_is_a_file_not_a_directory() {
        // Regression: read_dir_stub("bigdir/x") builds the path
        // "bigdir/x/.omemfs-stub". If "bigdir" has just been reconciled/
        // materialised from a directory into a plain file (a dir-stub ->
        // file-stub or dir-stub -> blob transition, or simply a type change
        // applied by pull's ordinary diff/apply), the intermediate component
        // "bigdir" is no longer a directory, so the OS reports ENOTDIR
        // (io::ErrorKind::NotADirectory), not ENOENT (NotFound). This must be
        // treated the same as "no stub here" (Ok(None)), not propagated as a
        // hard I/O error -- deepest_ancestor_stub (used by pull's apply step
        // to test every proper ancestor of a path) walks ancestors
        // unconditionally and has no other way to know one of them just
        // changed kind under it.
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("bigdir"), b"now a file").unwrap();
        let result = read_dir_stub(dir.path(), "bigdir/x");
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn read_returns_none_when_an_intermediate_component_is_a_file_not_a_directory() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("bigdir"), b"now a file").unwrap();
        let result = read(dir.path(), "bigdir/x");
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn deepest_ancestor_stub_does_not_error_when_an_ancestor_just_became_a_file() {
        // End-to-end version of the two tests above, through the actual
        // caller pull.rs uses.
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("bigdir"), b"now a file").unwrap();
        let result = deepest_ancestor_stub(dir.path(), "bigdir/a.txt");
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn list_under_scopes_to_the_given_subdirectory_but_keeps_root_relative_paths() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "top.txt", &dummy_record()).unwrap();
        write(dir.path(), "sub/inner.txt", &dummy_record()).unwrap();
        write(dir.path(), "other/elsewhere.txt", &dummy_record()).unwrap();

        let found = list_under(dir.path(), &dir.path().join("sub")).unwrap();
        let rels: Vec<&str> = found.iter().map(|(rel, _)| rel.as_str()).collect();
        assert_eq!(rels, vec!["sub/inner.txt"]);
    }

    #[test]
    fn list_under_root_itself_matches_list() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "top.txt", &dummy_record()).unwrap();
        write(dir.path(), "sub/inner.txt", &dummy_record()).unwrap();

        let mut via_list = list(dir.path()).unwrap();
        let mut via_list_under = list_under(dir.path(), dir.path()).unwrap();
        via_list.sort_by(|a, b| a.0.cmp(&b.0));
        via_list_under.sort_by(|a, b| a.0.cmp(&b.0));
        let rels_a: Vec<&str> = via_list.iter().map(|(rel, _)| rel.as_str()).collect();
        let rels_b: Vec<&str> = via_list_under
            .iter()
            .map(|(rel, _)| rel.as_str())
            .collect();
        assert_eq!(rels_a, rels_b);
    }

    #[test]
    fn deepest_ancestor_stub_finds_nearest_marker() {
        let dir = TempDir::new().unwrap();
        write_dir_stub(dir.path(), "a", &dummy_record()).unwrap();
        let found = deepest_ancestor_stub(dir.path(), "a/b/c").unwrap();
        assert_eq!(found.map(|(rel, _)| rel), Some("a".to_string()));
    }

    #[test]
    fn deepest_ancestor_stub_prefers_deepest_when_multiple_ancestors_stubbed() {
        let dir = TempDir::new().unwrap();
        write_dir_stub(dir.path(), "a", &dummy_record()).unwrap();
        write_dir_stub(dir.path(), "a/b", &dummy_record()).unwrap();
        let found = deepest_ancestor_stub(dir.path(), "a/b/c/d").unwrap();
        assert_eq!(found.map(|(rel, _)| rel), Some("a/b".to_string()));
    }

    #[test]
    fn deepest_ancestor_stub_returns_none_with_no_stubbed_ancestor() {
        let dir = TempDir::new().unwrap();
        let found = deepest_ancestor_stub(dir.path(), "a/b/c").unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn deepest_ancestor_stub_ignores_a_marker_at_the_path_itself() {
        let dir = TempDir::new().unwrap();
        // A marker at "a/b" itself (not a proper ancestor of "a/b") must not
        // be returned -- callers only reach this function after a direct
        // match on the path itself has already failed.
        write_dir_stub(dir.path(), "a/b", &dummy_record()).unwrap();
        let found = deepest_ancestor_stub(dir.path(), "a/b").unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn file_stub_at_or_above_finds_a_marker_at_the_path_itself() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "f.txt", &dummy_record()).unwrap();
        let found = file_stub_at_or_above(dir.path(), "f.txt").unwrap();
        assert_eq!(found.map(|(rel, _)| rel), Some("f.txt".to_string()));
    }

    #[test]
    fn file_stub_at_or_above_finds_a_marker_at_a_proper_ancestor() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "f.txt", &dummy_record()).unwrap();
        let found = file_stub_at_or_above(dir.path(), "f.txt/inner.txt").unwrap();
        assert_eq!(found.map(|(rel, _)| rel), Some("f.txt".to_string()));
    }

    #[test]
    fn file_stub_at_or_above_finds_the_deepest_ancestor_marker() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "a", &dummy_record()).unwrap();
        write(dir.path(), "a.d/b", &dummy_record()).unwrap();
        let found = file_stub_at_or_above(dir.path(), "a.d/b/c").unwrap();
        assert_eq!(found.map(|(rel, _)| rel), Some("a.d/b".to_string()));
    }

    #[test]
    fn file_stub_at_or_above_returns_none_with_no_marker_anywhere() {
        let dir = TempDir::new().unwrap();
        let found = file_stub_at_or_above(dir.path(), "a/b/c").unwrap();
        assert!(found.is_none());
    }

    // -----------------------------------------------------------------
    // reconcile_action
    // -----------------------------------------------------------------

    fn blob_entry(hash: Hash, size: u64, mode: Option<&str>) -> TreeEntry {
        TreeEntry::Blob {
            name: "x".to_string(),
            hash,
            mtime: None,
            size,
            mode: mode.map(|m| m.to_string()),
        }
    }

    fn tree_entry(hash: Hash, size: u64, blob_count: u64) -> TreeEntry {
        TreeEntry::Tree {
            name: "x".to_string(),
            hash,
            mtime: None,
            size,
            blob_count,
        }
    }

    fn symlink_entry(target: &str) -> TreeEntry {
        TreeEntry::Symlink {
            name: "x".to_string(),
            target: target.to_string(),
            mtime: None,
        }
    }

    fn file_stub_record(hash: Hash, size: u64) -> StubRecord {
        StubRecord {
            target_type: StubTargetType::Blob,
            hash,
            size,
            mtime: None,
            mode: None,
            blob_count: 0,
        }
    }

    fn dir_stub_record(hash: Hash, size: u64, blob_count: u64) -> StubRecord {
        StubRecord {
            target_type: StubTargetType::Tree,
            hash,
            size,
            mtime: None,
            mode: None,
            blob_count,
        }
    }

    #[test]
    fn reconcile_action_file_stub_up_to_date_when_fields_match_blob() {
        let hash = Hash::compute(b"a");
        let current = file_stub_record(hash.clone(), 10);
        let entry = blob_entry(hash, 10, None);
        assert_eq!(
            reconcile_action(&current, Some(&entry), false),
            ReconcileAction::UpToDate
        );
    }

    #[test]
    fn reconcile_action_file_stub_updates_fields_when_hash_changed() {
        let old_hash = Hash::compute(b"a");
        let new_hash = Hash::compute(b"b");
        let current = file_stub_record(old_hash, 10);
        let entry = blob_entry(new_hash.clone(), 20, Some("755"));
        match reconcile_action(&current, Some(&entry), false) {
            ReconcileAction::UpdateFields(rec) => {
                assert_eq!(rec.hash, new_hash);
                assert_eq!(rec.size, 20);
                assert_eq!(rec.mode, Some("755".to_string()));
                assert_eq!(rec.target_type, StubTargetType::Blob);
            }
            other => panic!("expected UpdateFields, got {other:?}"),
        }
    }

    #[test]
    fn reconcile_action_dir_stub_up_to_date_when_fields_match_tree() {
        let hash = Hash::compute(b"a");
        let current = dir_stub_record(hash.clone(), 40, 2);
        let entry = tree_entry(hash, 40, 2);
        assert_eq!(
            reconcile_action(&current, Some(&entry), true),
            ReconcileAction::UpToDate
        );
    }

    #[test]
    fn reconcile_action_dir_stub_updates_fields_when_blob_count_changed() {
        let hash = Hash::compute(b"a");
        let current = dir_stub_record(hash.clone(), 40, 2);
        let entry = tree_entry(hash.clone(), 60, 3);
        match reconcile_action(&current, Some(&entry), true) {
            ReconcileAction::UpdateFields(rec) => {
                assert_eq!(rec.hash, hash);
                assert_eq!(rec.size, 60);
                assert_eq!(rec.blob_count, 3);
                assert_eq!(rec.target_type, StubTargetType::Tree);
            }
            other => panic!("expected UpdateFields, got {other:?}"),
        }
    }

    #[test]
    fn reconcile_action_dir_stub_switches_to_file_stub_when_clone_root_is_a_blob() {
        let hash = Hash::compute(b"a");
        let current = dir_stub_record(Hash::compute(b"old"), 40, 2);
        let entry = blob_entry(hash.clone(), 5, None);
        match reconcile_action(&current, Some(&entry), true) {
            ReconcileAction::SwitchToFileStub(rec) => {
                assert_eq!(rec.target_type, StubTargetType::Blob);
                assert_eq!(rec.hash, hash);
                assert_eq!(rec.size, 5);
            }
            other => panic!("expected SwitchToFileStub, got {other:?}"),
        }
    }

    #[test]
    fn reconcile_action_file_stub_switches_to_dir_stub_when_clone_root_is_a_tree() {
        let hash = Hash::compute(b"a");
        let current = file_stub_record(Hash::compute(b"old"), 5);
        let entry = tree_entry(hash.clone(), 40, 2);
        match reconcile_action(&current, Some(&entry), false) {
            ReconcileAction::SwitchToDirStub(rec) => {
                assert_eq!(rec.target_type, StubTargetType::Tree);
                assert_eq!(rec.hash, hash);
                assert_eq!(rec.blob_count, 2);
            }
            other => panic!("expected SwitchToDirStub, got {other:?}"),
        }
    }

    #[test]
    fn reconcile_action_materialises_symlink_for_file_stub() {
        let current = file_stub_record(Hash::compute(b"old"), 5);
        let entry = symlink_entry("/target");
        match reconcile_action(&current, Some(&entry), false) {
            ReconcileAction::MaterialiseSymlink { target, .. } => {
                assert_eq!(target, "/target");
            }
            other => panic!("expected MaterialiseSymlink, got {other:?}"),
        }
    }

    #[test]
    fn reconcile_action_materialises_symlink_for_dir_stub() {
        let current = dir_stub_record(Hash::compute(b"old"), 5, 1);
        let entry = symlink_entry("/target");
        match reconcile_action(&current, Some(&entry), true) {
            ReconcileAction::MaterialiseSymlink { target, .. } => {
                assert_eq!(target, "/target");
            }
            other => panic!("expected MaterialiseSymlink, got {other:?}"),
        }
    }

    #[test]
    fn reconcile_action_removes_file_stub_when_clone_root_entry_is_absent() {
        let current = file_stub_record(Hash::compute(b"old"), 5);
        assert_eq!(
            reconcile_action(&current, None, false),
            ReconcileAction::Remove
        );
    }

    #[test]
    fn reconcile_action_removes_dir_stub_when_clone_root_entry_is_absent() {
        let current = dir_stub_record(Hash::compute(b"old"), 5, 1);
        assert_eq!(
            reconcile_action(&current, None, true),
            ReconcileAction::Remove
        );
    }
}
