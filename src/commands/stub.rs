use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

use crate::error::Error;
use crate::object::{TreeEntry, blob_hash};
use crate::repo::Repo;
use crate::stat_cache::StatCache;
use crate::stub::{self, StubRecord, StubTargetType};
use crate::term::Output;
use crate::tree_ops;

pub struct StubOptions {
    pub work_dir: PathBuf,
    /// Directory the command was invoked from; relative paths resolve against it.
    pub current_dir: PathBuf,
    /// Paths to stub (relative to the cwd).
    pub paths: Vec<PathBuf>,
    pub dry_run: bool,
}

/// A single item to stub, either a file or a whole directory.
enum StubTarget {
    File {
        abs: PathBuf,
        rel: String,
        entry: TreeEntry,
        /// `(mtime, size)` seen when the file's hash was verified.
        seen: Snapshot,
    },
    Dir {
        abs: PathBuf,
        rel: String,
        hash: crate::object::Hash,
        size: u64,
        blob_count: u64,
        mtime: Option<chrono::DateTime<chrono::Utc>>,
        /// Every regular file present when the directory was verified.
        seen: DirSnapshot,
    },
}

/// A regular file's `(mtime, size)` at the moment its content was verified
/// (design/08, "omemfs stub", "Re-verifying immediately before the delete").
#[derive(Debug, Clone, Copy)]
struct Snapshot {
    mtime: SystemTime,
    size: u64,
}

impl Snapshot {
    fn of_file(path: &std::path::Path) -> std::io::Result<Self> {
        let md = fs::symlink_metadata(path)?;
        Ok(Snapshot {
            mtime: md.modified()?,
            size: md.len(),
        })
    }

    /// `Ok` when `path` is still a regular file with the recorded `(mtime, size)`.
    fn check_file(&self, path: &std::path::Path) -> Result<(), ()> {
        match fs::symlink_metadata(path) {
            Ok(md) if md.is_file() && md.len() == self.size && md.modified().ok() == Some(self.mtime) => {
                Ok(())
            }
            _ => Err(()),
        }
    }
}

/// The `(mtime, size)` of every regular file under a directory, keyed by path
/// relative to it. Symlinks are recorded as present (never followed) so a
/// symlink that appears later counts as a change. The stub marker is excluded:
/// it is written by this command between the snapshot and the check.
#[derive(Debug, Default)]
struct DirSnapshot {
    files: std::collections::HashMap<String, Snapshot>,
    others: std::collections::HashSet<String>,
}

impl DirSnapshot {
    fn of(dir: &std::path::Path) -> std::io::Result<Self> {
        let mut snap = DirSnapshot::default();
        snap.walk(dir, "")?;
        Ok(snap)
    }

    fn walk(&mut self, dir: &std::path::Path, prefix: &str) -> std::io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if prefix.is_empty() && name == stub::DIR_STUB_NAME {
                continue;
            }
            let rel = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let path = entry.path();
            let md = fs::symlink_metadata(&path)?;
            if md.is_dir() {
                self.walk(&path, &rel)?;
            } else if md.is_file() {
                self.files.insert(
                    rel,
                    Snapshot {
                        mtime: md.modified()?,
                        size: md.len(),
                    },
                );
            } else {
                self.others.insert(rel);
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn contains(&self, rel: &str) -> bool {
        self.files.contains_key(rel) || self.others.contains(rel)
    }

    /// `Ok` when the directory holds exactly the recorded files, unchanged.
    /// `Err(path)` names the first difference found (sorted, so it is stable).
    fn check(&self, dir: &std::path::Path) -> Result<(), String> {
        let now = DirSnapshot::of(dir).map_err(|_| String::new())?;
        let mut diffs: Vec<&String> = Vec::new();
        for (rel, snap) in &self.files {
            match now.files.get(rel) {
                Some(cur) if cur.size == snap.size && cur.mtime == snap.mtime => {}
                _ => diffs.push(rel),
            }
        }
        for rel in now.files.keys().chain(now.others.iter()) {
            if !self.files.contains_key(rel) && !self.others.contains(rel) {
                diffs.push(rel);
            }
        }
        for rel in &self.others {
            if !now.others.contains(rel) {
                diffs.push(rel);
            }
        }
        diffs.sort();
        match diffs.first() {
            Some(rel) => Err((*rel).clone()),
            None => Ok(()),
        }
    }
}

/// Deterministic integration-test hook for the verify-to-delete window (design/08,
/// "Re-verifying immediately before the delete"). Compiled only in debug builds
/// and inert unless the variable is set, like `OMEMFS_TEST_SCAN_AFTER_LIST_DELAY_MS`.
fn test_hook_before_delete() {
    #[cfg(debug_assertions)]
    if let Ok(ms) = std::env::var("OMEMFS_TEST_STUB_BEFORE_DELETE_DELAY_MS")
        && let Ok(ms) = ms.parse::<u64>()
    {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

/// The error for a target that changed after it was verified.
fn changed_while_stubbing(rel: &str) -> Error {
    Error::Other(format!(
        "cannot stub '{}': '{}' changed while being stubbed; nothing was deleted for it",
        crate::term::escape_display(rel),
        crate::term::escape_display(rel),
    ))
}

pub fn run(opts: StubOptions) -> Result<(), Error> {
    if opts.paths.is_empty() {
        return Err(Error::Other(
            "nothing specified, nothing stubbed".to_string(),
        ));
    }

    let repo = Repo::open(&opts.work_dir)?;
    crate::progress::notify_repo_dir(&repo.work_dir);
    let phase = crate::progress::begin_phase("Stub files");
    let _lock = repo.acquire_lock()?;
    let local = repo.local_store();

    let clone_root = repo.read_clone_root()?.ok_or_else(|| {
        Error::Other("no clone_root — repository has never been synced".to_string())
    })?;

    // Pre-flatten clone_root for O(1) blob lookups. For directory entries we
    // use navigate_entry directly. Use the stub-boundary-aware flatten: reading
    // through the local-only store, an already-stubbed subtree's tree object is
    // absent, and the eager flatten would abort with `object not found` before
    // any input path is even examined (design/04 "omemfs stub").
    let clone_blob_entries = tree_ops::flatten_tree_entries_local(&clone_root, &local)?;

    let omemfs_dir = repo.omemfs_dir();
    let stat_cache = StatCache::read(&omemfs_dir);

    // Resolve each input path to a StubTarget, or collect individual files.
    let mut to_skip: Vec<String> = Vec::new();
    let mut validated: Vec<StubTarget> = Vec::new();

    for path in &opts.paths {
        // Resolve the argument against the cwd, then re-express it relative to
        // the repository root (so paths given from a subdirectory work).
        let rel = crate::repo::normalize_path(path, &repo.work_dir, &opts.current_dir)?;
        let abs = repo.work_dir.join(&rel);

        // fsguard::is_real_dir (design/03_sync_model.md, "Write safety: the
        // working-tree write boundary") answers "is this a real, symlink-free
        // directory below work_dir?" instead of `Path::is_dir()`, which
        // follows symlinks: a directory target that has been swapped for a
        // symlink pointing outside work_dir must be refused here, before any
        // decision is made about deleting its "contents", not silently
        // treated as either a directory or a plain file.
        if crate::fsguard::is_real_dir(&repo.work_dir, &abs)? {
            // --- Directory stub ---
            // Check whether already stubbed (dir/.omemfs-stub exists).
            if stub::dir_exists(&repo.work_dir, &rel) {
                to_skip.push(rel.clone());
                continue;
            }

            // Refuse to stub a directory whose `.omemfs-stub` marker would be
            // visible to Git (design/08 "Stubs and Git repositories").
            let dir_stub_path = abs.join(crate::stub::DIR_STUB_NAME);
            if stub::stub_would_be_visible_to_git(&dir_stub_path, &repo.work_dir) {
                return Err(Error::Other(format!(
                    "cannot stub '{}': the .omemfs-stub file would be visible to Git \
                     (add it to .gitignore, or stub the entire Git repo root)",
                    rel
                )));
            }

            // Look up the directory's tree hash in clone_root.
            let components: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
            let dir_entry = tree_ops::navigate_entry(&clone_root, &components, &local)?
                .ok_or_else(|| Error::Other(format!(
                    "cannot stub '{}': not found in clone_root (run 'omemfs push' to sync first)",
                    rel
                )))?;

            let (hash, size, blob_count, mtime) = match &dir_entry {
                TreeEntry::Tree {
                    hash,
                    size,
                    blob_count,
                    mtime,
                    ..
                } => (hash.clone(), *size, *blob_count, *mtime),
                _ => {
                    return Err(Error::Other(format!(
                        "cannot stub '{}': clone_root entry is not a directory",
                        rel
                    )));
                }
            };

            // Snapshot BEFORE the verifying scan: a file edited while the scan
            // is reading it then differs from the snapshot at the re-check,
            // whereas a snapshot taken after the scan would record the edited
            // state and let it through (design/08).
            let seen = DirSnapshot::of(&abs)?;

            // Check that working tree matches clone_root for this directory.
            // We do this by scanning just the directory and comparing the tree hash.
            let scan_result = crate::scan::scan_and_store_with_cache(
                &repo.work_dir,
                &abs,
                &local,
                Some(&clone_blob_entries),
                &stat_cache,
                true, // conservative: keep blob objects local (design/03)
            )?;
            if scan_result.root_hash != hash {
                return Err(Error::Other(format!(
                    "cannot stub '{}': working tree differs from clone_root (run 'omemfs push' to save changes first)",
                    rel
                )));
            }
            validated.push(StubTarget::Dir {
                abs,
                rel,
                hash,
                size,
                blob_count,
                mtime,
                seen,
            });
        } else if abs.is_file() {
            // --- File stub ---
            if stub::exists(&repo.work_dir, &rel) {
                to_skip.push(rel.clone());
                continue;
            }

            // Refuse to stub a file whose `<name>.omemfs-stub` marker would be
            // visible to Git (design/08 "Stubs and Git repositories").
            let file_stub_path = crate::stub::file_stub_path_for(&abs);
            if stub::stub_would_be_visible_to_git(&file_stub_path, &repo.work_dir) {
                return Err(Error::Other(format!(
                    "cannot stub '{}': the .omemfs-stub file would be visible to Git \
                     (add it to .gitignore, or stub the entire Git repo root)",
                    rel
                )));
            }

            let entry = clone_blob_entries.get(rel.as_str()).cloned().ok_or_else(|| {
                Error::Other(format!(
                    "cannot stub '{}': not found in clone_root (run 'omemfs push' to sync first)",
                    rel
                ))
            })?;

            let clone_hash = match &entry {
                TreeEntry::Blob { hash, .. } => hash.clone(),
                _ => {
                    return Err(Error::Other(format!(
                        "cannot stub '{}': clone_root entry is not a file",
                        rel
                    )));
                }
            };

            // Fast path: if StatCache has a safe entry for this file with the
            // correct size and mtime, the cached hash is trustworthy — skip the
            // full read. Fall back to a full read on any cache miss.
            // (mtime, size) is captured BEFORE the content is read: an edit
            // made during the read then fails the re-check before the delete.
            let seen = Snapshot::of_file(&abs)?;
            let verified = if let Ok(meta) = fs::metadata(&abs) {
                let fs_size = meta.len();
                let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                if let Some(cached_hash) = stat_cache.lookup_current(&rel, mtime, fs_size) {
                    cached_hash == &clone_hash
                } else {
                    let content = fs::read(&abs)?;
                    blob_hash(&content) == clone_hash
                }
            } else {
                let content = fs::read(&abs)?;
                blob_hash(&content) == clone_hash
            };
            if !verified {
                return Err(Error::Other(format!(
                    "cannot stub '{}': working tree differs from clone_root (run 'omemfs push' to save changes first)",
                    rel
                )));
            }

            validated.push(StubTarget::File { abs, rel, entry, seen });
        } else if stub::exists(&repo.work_dir, &rel) || stub::dir_exists(&repo.work_dir, &rel) {
            to_skip.push(rel.clone());
        } else {
            return Err(Error::Other(format!(
                "path does not exist: {}",
                path.display()
            )));
        }
    }

    for rel in &to_skip {
        eprintln!("warning: '{}' is already stubbed, skipping", crate::term::escape_display(rel));
    }

    if validated.is_empty() {
        // Buffer through `Output`: the "Stub files" phase row is on screen, so
        // a direct print would race with the periodic redraw. The line is
        // flushed below the phase list at command exit.
        let mut out = Output::for_stdout();
        out.writeln("Nothing to stub.")?;
        out.finish()?;
        phase.complete("0 files");
        return Ok(());
    }

    if opts.dry_run {
        let mut out = Output::for_stdout();
        for target in &validated {
            let rel = match target {
                StubTarget::File { rel, .. } | StubTarget::Dir { rel, .. } => rel,
            };
            out.writeln(&format!("  would stub: {}", crate::term::escape_display(rel)))?;
        }
        let count = validated.len();
        out.writeln(&format!("{} file(s) would be stubbed.", count))?;
        out.finish()?;
        phase.complete(format!("{} files (dry run)", count));
        return Ok(());
    }

    let count = validated.len();
    test_hook_before_delete();
    for target in validated {
        match target {
            StubTarget::File { abs, rel, entry, seen } => {
                let (hash, size, mtime, mode) = match entry {
                    TreeEntry::Blob {
                        hash,
                        size,
                        mtime,
                        mode,
                        ..
                    } => (hash, size, mtime, mode),
                    _ => unreachable!(),
                };
                stub::write(
                    &repo.work_dir,
                    &rel,
                    &StubRecord {
                        target_type: StubTargetType::Blob,
                        hash,
                        size,
                        mtime,
                        mode,
                        blob_count: 0,
                    },
                )?;
                // Re-check right before the delete (design/08). On a change the
                // file is kept and the stub record written above is removed
                // again, so the unsaved edit is never lost.
                if seen.check_file(&abs).is_err() {
                    stub::remove(&repo.work_dir, &rel)?;
                    return Err(changed_while_stubbing(&rel));
                }
                fs::remove_file(&abs)?;
            }
            StubTarget::Dir {
                abs,
                rel,
                hash,
                size,
                blob_count,
                mtime,
                seen,
            } => {
                // design/08_stub_system.md, "omemfs stub": write the stub
                // record BEFORE deleting anything, so an interruption
                // partway through the delete below leaves the directory in
                // the well-understood "partial expansion" state (stub
                // marker + some leftover real files) rather than risking
                // losing both the content and the stub record if the
                // process died before the stub write landed.
                stub::write_dir_stub(
                    &repo.work_dir,
                    &rel,
                    &StubRecord {
                        target_type: StubTargetType::Tree,
                        hash,
                        size,
                        blob_count,
                        mtime,
                        mode: None,
                    },
                )?;
                // Re-check right before deleting (design/08): a directory edited
                // since it was verified is left untouched and the marker written
                // above is removed again.
                if let Err(changed) = seen.check(&abs) {
                    stub::remove_dir_stub(&repo.work_dir, &rel)?;
                    return Err(changed_while_stubbing(&if changed.is_empty() {
                        rel.clone()
                    } else {
                        format!("{rel}/{changed}")
                    }));
                }
                // Now remove every other entry, leaving only the just-written
                // `.omemfs-stub` marker behind. Routed through
                // `fsguard::remove_dir_all` (design/03_sync_model.md, "Write
                // safety: the working-tree write boundary"): a symlink
                // discovered anywhere under `abs` -- including `abs` itself
                // having been swapped for one between the `is_real_dir`
                // check above and here -- is refused rather than followed.
                // `stub::DIR_STUB_NAME` is passed as `keep` so the marker
                // just written above is not itself swept up by this same
                // "delete everything in this directory" call.
                crate::fsguard::remove_dir_all(
                    &repo.work_dir,
                    &abs,
                    &[stub::DIR_STUB_NAME],
                )?;
            }
        }
    }

    let mut out = Output::for_stdout();
    out.writeln(&format!("{} file(s) stubbed.", count))?;
    out.finish()?;
    phase.complete(format!("{} files", count));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn set_mtime(path: &std::path::Path, secs_from_now: i64) {
        let t = if secs_from_now >= 0 {
            SystemTime::now() + std::time::Duration::from_secs(secs_from_now as u64)
        } else {
            SystemTime::now() - std::time::Duration::from_secs((-secs_from_now) as u64)
        };
        filetime::set_file_mtime(path, filetime::FileTime::from_system_time(t)).unwrap();
    }

    #[test]
    fn snapshot_of_an_unchanged_file_matches() {
        let d = TempDir::new().unwrap();
        let f = d.path().join("a.bin");
        fs::write(&f, b"content").unwrap();
        let snap = Snapshot::of_file(&f).unwrap();
        assert!(snap.check_file(&f).is_ok());
    }

    #[test]
    fn a_file_edited_after_the_snapshot_is_detected_by_size() {
        let d = TempDir::new().unwrap();
        let f = d.path().join("a.bin");
        fs::write(&f, b"content").unwrap();
        let snap = Snapshot::of_file(&f).unwrap();
        fs::write(&f, b"content, and an unsaved edit").unwrap();
        assert!(snap.check_file(&f).is_err());
    }

    #[test]
    fn a_same_size_edit_is_detected_by_mtime() {
        let d = TempDir::new().unwrap();
        let f = d.path().join("a.bin");
        fs::write(&f, b"content").unwrap();
        let snap = Snapshot::of_file(&f).unwrap();
        fs::write(&f, b"CONTENT").unwrap();
        set_mtime(&f, 120);
        assert!(snap.check_file(&f).is_err());
    }

    #[test]
    fn a_file_deleted_after_the_snapshot_is_detected() {
        let d = TempDir::new().unwrap();
        let f = d.path().join("a.bin");
        fs::write(&f, b"content").unwrap();
        let snap = Snapshot::of_file(&f).unwrap();
        fs::remove_file(&f).unwrap();
        assert!(snap.check_file(&f).is_err());
    }

    #[test]
    fn a_directory_snapshot_records_every_regular_file() {
        let d = TempDir::new().unwrap();
        fs::create_dir(d.path().join("sub")).unwrap();
        fs::write(d.path().join("a.txt"), b"1").unwrap();
        fs::write(d.path().join("sub/b.txt"), b"22").unwrap();
        let snap = DirSnapshot::of(d.path()).unwrap();
        assert!(snap.check(d.path()).is_ok());
    }

    #[test]
    fn a_directory_snapshot_detects_an_edited_file() {
        let d = TempDir::new().unwrap();
        fs::create_dir(d.path().join("sub")).unwrap();
        fs::write(d.path().join("sub/b.txt"), b"22").unwrap();
        let snap = DirSnapshot::of(d.path()).unwrap();
        fs::write(d.path().join("sub/b.txt"), b"22 edited").unwrap();
        let err = snap.check(d.path()).unwrap_err();
        assert_eq!(err, "sub/b.txt");
    }

    #[test]
    fn a_directory_snapshot_detects_a_new_file() {
        let d = TempDir::new().unwrap();
        fs::write(d.path().join("a.txt"), b"1").unwrap();
        let snap = DirSnapshot::of(d.path()).unwrap();
        fs::write(d.path().join("brand-new.txt"), b"created while stubbing").unwrap();
        assert_eq!(snap.check(d.path()).unwrap_err(), "brand-new.txt");
    }

    #[test]
    fn a_directory_snapshot_detects_a_deleted_file() {
        let d = TempDir::new().unwrap();
        fs::write(d.path().join("a.txt"), b"1").unwrap();
        let snap = DirSnapshot::of(d.path()).unwrap();
        fs::remove_file(d.path().join("a.txt")).unwrap();
        assert_eq!(snap.check(d.path()).unwrap_err(), "a.txt");
    }

    #[test]
    fn a_directory_snapshot_ignores_the_stub_marker() {
        // The marker is written between the snapshot and the check (step b).
        let d = TempDir::new().unwrap();
        fs::write(d.path().join("a.txt"), b"1").unwrap();
        let snap = DirSnapshot::of(d.path()).unwrap();
        fs::write(d.path().join(stub::DIR_STUB_NAME), b"{}").unwrap();
        assert!(snap.check(d.path()).is_ok());
    }

    #[test]
    fn a_directory_snapshot_treats_a_new_symlink_as_a_change() {
        let d = TempDir::new().unwrap();
        fs::write(d.path().join("a.txt"), b"1").unwrap();
        let snap = DirSnapshot::of(d.path()).unwrap();
        std::os::unix::fs::symlink("a.txt", d.path().join("link")).unwrap();
        assert_eq!(snap.check(d.path()).unwrap_err(), "link");
    }

    #[test]
    fn a_directory_snapshot_of_a_symlink_is_not_followed() {
        // A symlink to a directory outside must not be walked.
        let d = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("secret.txt"), b"x").unwrap();
        std::os::unix::fs::symlink(outside.path(), d.path().join("out")).unwrap();
        let snap = DirSnapshot::of(d.path()).unwrap();
        assert!(snap.check(d.path()).is_ok());
        assert!(!snap.contains("out/secret.txt"));
    }
}

