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
    },
    Dir {
        abs: PathBuf,
        rel: String,
        hash: crate::object::Hash,
        size: u64,
        blob_count: u64,
        mtime: Option<chrono::DateTime<chrono::Utc>>,
    },
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

    let omemfs_dir = repo.work_dir.join(".omemfs");
    let stat_cache = StatCache::read(&omemfs_dir);

    // Resolve each input path to a StubTarget, or collect individual files.
    let mut to_skip: Vec<String> = Vec::new();
    let mut validated: Vec<StubTarget> = Vec::new();

    for path in &opts.paths {
        // Resolve the argument against the cwd, then re-express it relative to
        // the repository root (so paths given from a subdirectory work).
        let rel = crate::repo::normalize_path(path, &repo.work_dir, &opts.current_dir)?;
        let abs = repo.work_dir.join(&rel);

        if abs.is_dir() {
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

            validated.push(StubTarget::File { abs, rel, entry });
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
        eprintln!("warning: '{}' is already stubbed, skipping", rel);
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
            out.writeln(&format!("  would stub: {}", rel))?;
        }
        let count = validated.len();
        out.writeln(&format!("{} file(s) would be stubbed.", count))?;
        out.finish()?;
        phase.complete(format!("{} files (dry run)", count));
        return Ok(());
    }

    let count = validated.len();
    for target in validated {
        match target {
            StubTarget::File { abs, rel, entry } => {
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
                fs::remove_file(&abs)?;
            }
            StubTarget::Dir {
                abs,
                rel,
                hash,
                size,
                blob_count,
                mtime,
            } => {
                // Remove all contents of the directory.
                remove_dir_contents(&abs)?;
                // Write the directory stub inside the now-empty directory.
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
            }
        }
    }

    let mut out = Output::for_stdout();
    out.writeln(&format!("{} file(s) stubbed.", count))?;
    out.finish()?;
    phase.complete(format!("{} files", count));
    Ok(())
}

/// Remove every child of `dir`, leaving `dir` itself intact.
fn remove_dir_contents(dir: &std::path::Path) -> Result<(), Error> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            fs::remove_dir_all(&p)?;
        } else {
            fs::remove_file(&p)?;
        }
    }
    Ok(())
}
