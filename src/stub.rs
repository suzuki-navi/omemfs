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
use crate::object::Hash;
use crate::store::local::atomic_write;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    let trimmed = rel_path.trim_start_matches('/').replace('\\', "/");
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
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::Io(e)),
    }
}

// ---------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------

/// Write a file stub record for `rel_path`.
pub fn write(work_dir: &Path, rel_path: &str, record: &StubRecord) -> Result<(), Error> {
    write_to_path(&file_stub_abs(work_dir, rel_path), record)
}

/// Write a directory stub record for `rel_path`.
pub fn write_dir_stub(work_dir: &Path, rel_path: &str, record: &StubRecord) -> Result<(), Error> {
    write_to_path(&dir_stub_abs(work_dir, rel_path), record)
}

fn write_to_path(path: &Path, record: &StubRecord) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string(record).map_err(Error::Json)?;
    atomic_write(path, json.as_bytes())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Remove
// ---------------------------------------------------------------------------

/// Remove the file stub for `rel_path` (no-op if it does not exist).
pub fn remove(work_dir: &Path, rel_path: &str) -> Result<(), Error> {
    remove_path(&file_stub_abs(work_dir, rel_path))
}

/// Remove the directory stub for `rel_path` (no-op if it does not exist).
pub fn remove_dir_stub(work_dir: &Path, rel_path: &str) -> Result<(), Error> {
    remove_path(&dir_stub_abs(work_dir, rel_path))
}

fn remove_path(path: &Path) -> Result<(), Error> {
    match fs::remove_file(path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::Io(e)),
    }
}

// ---------------------------------------------------------------------------
// List (all stubs in working tree)
// ---------------------------------------------------------------------------

/// Enumerate all stubs recursively under `work_dir`.
/// Returns `(rel_path, StubRecord)` pairs.
/// - File stubs: `rel_path` is the logical file path (without `.omemfs-stub` suffix).
/// - Directory stubs: `rel_path` is the directory path (without trailing slash).
pub fn list(work_dir: &Path) -> Result<Vec<(String, StubRecord)>, Error> {
    let mut result = Vec::new();
    collect(work_dir, work_dir, &mut result)?;
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
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
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
                .map(|p| p.to_string_lossy().replace('\\', "/"))
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
