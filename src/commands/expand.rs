use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use filetime::FileTime;

use crate::codec;
use crate::dtimer_l1;
use crate::error::Error;
use crate::io_stats;
use crate::object::{Tree, TreeEntry};
use crate::repo::Repo;
use crate::store::ObjectStore;
use crate::store::stats::IoRecord;
use crate::stub::{self, StubTargetType};

pub struct ExpandOptions {
    pub work_dir: PathBuf,
    /// Directory the command was invoked from; relative paths resolve against it.
    pub current_dir: PathBuf,
    /// Paths to expand (relative to the cwd). If empty, expand all stubs.
    pub paths: Vec<PathBuf>,
    pub remote_name: String,
    pub dry_run: bool,
    /// Only expand stubs whose size is strictly below this threshold. 0 means expand all.
    pub stub_threshold: u64,
}

pub fn run(opts: ExpandOptions) -> Result<(), Error> {
    let started = std::time::Instant::now();
    let repo = Repo::open(&opts.work_dir)?;
    crate::progress::notify_repo_dir(&repo.work_dir);
    // Hold the repo lock: expand writes working-tree files and must not race
    // with a concurrent push scanning a partially-written file. See
    // design/12_locking.md.
    let _lock = repo.acquire_lock()?;
    let _t = dtimer_l1!("expand");
    let local = repo.local_store();

    // Collect all stubs, then apply path scope filter.
    // When no paths given, default to the current working directory.
    let all_stubs = stub::list(&repo.work_dir)?;
    let scoped_paths: Vec<String> = if opts.paths.is_empty() {
        Vec::new()
    } else {
        opts.paths
            .iter()
            .map(|path| crate::repo::normalize_path(path, &repo.work_dir, &opts.current_dir))
            .collect::<Result<_, _>>()?
    };
    let scoped: Vec<(String, crate::stub::StubRecord)> =
        if opts.paths.is_empty() || scoped_paths.iter().any(|scope| scope.is_empty()) {
            all_stubs
        } else {
            all_stubs
                .into_iter()
                .filter(|(rel, _)| {
                    scoped_paths
                        .iter()
                        .any(|scope| rel == scope || rel.starts_with(&format!("{}/", scope)))
                })
                .collect()
        };

    // Apply stub_threshold: partition into (expand, keep-stubbed).
    // threshold == 0 means expand everything.
    // Stubs that exactly match a user-specified path bypass the threshold.
    let (to_expand, kept_stubbed): (Vec<_>, Vec<_>) =
        scoped.into_iter().partition(|(rel, record)| {
            opts.stub_threshold == 0
                || record.size < opts.stub_threshold
                || scoped_paths.iter().any(|scope| rel == scope)
        });

    if to_expand.is_empty() {
        if !kept_stubbed.is_empty() {
            crate::progress::emit_output_line(&format!(
                "Nothing to expand ({} stub(s) at or above threshold kept).",
                kept_stubbed.len()
            ));
        } else {
            crate::progress::emit_output_line("Nothing to expand.");
        }
        return Ok(());
    }

    let io_record = Arc::new(IoRecord::default());
    let (pack_reader, _remote, remote_key) =
        repo.pack_reader(&opts.remote_name, Some(&io_record))?;
    let remote_key = remote_key.as_ref();

    let phase = crate::progress::begin_phase("Expand stubs");
    let mut count = 0usize;
    for (rel_path, record) in &to_expand {
        match &record.target_type {
            StubTargetType::Tree => {
                if opts.dry_run {
                    crate::progress::emit_output_line(&format!("  would expand: {}", rel_path));
                    continue;
                }
                // The tree object is fetched on demand by expand_tree below.

                let abs_dir = opts.work_dir.join(rel_path);
                // Recursively materialise the tree into the directory, re-stubbing children
                // that are at or above the threshold.
                // The directory stub marker is removed AFTER successful expansion so that
                // a mid-expansion failure leaves the stub in place for retry.
                // expand_tree returns the number of files (blobs + symlinks)
                // actually materialised, so the reported count reflects files
                // written rather than the number of top-level stub records
                // processed.
                let materialised = expand_tree(
                    &record.hash,
                    &abs_dir,
                    &opts.work_dir,
                    rel_path,
                    &local,
                    &pack_reader,
                    remote_key,
                    opts.stub_threshold,
                )?;
                // Remove the directory stub marker only on success.
                stub::remove_dir_stub(&repo.work_dir, rel_path)?;

                if let Some(mt) = record.mtime {
                    let ft = FileTime::from_unix_time(mt.timestamp(), mt.timestamp_subsec_nanos());
                    filetime::set_file_mtime(&abs_dir, ft).ok();
                }
                count += materialised;
            }
            StubTargetType::Blob => {
                // --dry-run is a pure report: it must not fetch any object from
                // the remote or write to the local cache (design/04 expand
                // --dry-run: "show what would be expanded without writing files
                // or removing stub records").
                if opts.dry_run {
                    crate::progress::emit_output_line(&format!("  would expand: {}", rel_path));
                    continue;
                }

                // Ensure the blob (and all its chunks) are in the local cache.
                // transfer_objects walks per object, so peak memory stays
                // bounded by one chunk (≤ CDC_MAX) regardless of file size.
                if !local.exists(&record.hash)? {
                    crate::commands::push::transfer_objects(
                        &pack_reader,
                        &local,
                        &record.hash,
                        remote_key,
                        true,
                    )?;
                }

                let abs_path = opts.work_dir.join(rel_path);
                if let Some(parent) = abs_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                // Streaming materialisation from the local cache (bounded memory).
                codec::chunk::materialise_to_file(&local, &record.hash, None, &abs_path)?;

                if let Some(mt) = record.mtime {
                    let ft = FileTime::from_unix_time(mt.timestamp(), mt.timestamp_subsec_nanos());
                    filetime::set_file_mtime(&abs_path, ft).ok();
                }
                // The temp + rename in materialise_to_file resets permissions —
                // the mode must be re-applied after it.
                crate::fsmeta::apply_mode(&abs_path, &record.mode);

                stub::remove(&repo.work_dir, rel_path)?;
                count += 1;
            }
        }
    }

    if opts.dry_run {
        let summary = if !kept_stubbed.is_empty() {
            format!(
                "{} would expand, {} kept",
                to_expand.len(),
                kept_stubbed.len()
            )
        } else {
            format!("{} would expand", to_expand.len())
        };
        phase.complete(summary);
        if !kept_stubbed.is_empty() {
            crate::progress::emit_output_line(&format!(
                "{} stub(s) would be expanded, {} stub(s) at or above threshold kept.",
                to_expand.len(),
                kept_stubbed.len()
            ));
        } else {
            crate::progress::emit_output_line(&format!(
                "{} stub(s) would be expanded.",
                to_expand.len()
            ));
        }
    } else {
        let summary = if !kept_stubbed.is_empty() {
            format!("{} expanded, {} kept", count, kept_stubbed.len())
        } else {
            format!("{} expanded", count)
        };
        phase.complete(summary);
        if !kept_stubbed.is_empty() {
            crate::progress::emit_output_line(&format!(
                "{} file(s) expanded, {} stub(s) at or above threshold kept.",
                count,
                kept_stubbed.len()
            ));
        } else {
            crate::progress::emit_output_line(&format!("{} file(s) expanded.", count));
        }
    }

    if !opts.dry_run {
        let omemfs_dir = repo.work_dir.join(".omemfs");
        let duration_ms = started.elapsed().as_millis() as u64;
        io_stats::append_record(
            &omemfs_dir,
            "expand",
            &opts.remote_name,
            &io_record,
            duration_ms,
        );
    }
    Ok(())
}

/// Recursively materialise the tree at `tree_hash` into `base_dir`.
/// Downloads any objects missing from `local` from `remote`.
/// Children whose size is >= `stub_threshold` (and threshold > 0) are left as stubs
/// rather than being fully expanded.
///
/// Returns the number of files (blobs and symlinks) actually materialised into
/// the working tree. Children left stubbed do not count; nested directories
/// contribute the files materialised within them.
fn expand_tree(
    tree_hash: &crate::object::Hash,
    base_dir: &std::path::Path,
    work_dir: &std::path::Path,
    rel_base: &str,
    local: &dyn ObjectStore,
    remote: &dyn ObjectStore,
    remote_key: Option<&crate::codec::encrypt::EncryptKey>,
    stub_threshold: u64,
) -> Result<usize, Error> {
    fs::create_dir_all(base_dir)?;
    let mut materialised = 0usize;

    let data = codec::ensure_local_then_read(remote, local, tree_hash, remote_key)?;
    let Tree::Normal { entries } = Tree::deserialise(&data)?;

    for entry in entries {
        match entry {
            TreeEntry::Blob {
                name,
                hash,
                size,
                mtime,
                mode,
            } => {
                let rel_path = format!("{}/{}", rel_base, name);
                let abs_for_stub = base_dir.join(&name);
                let stub_path = stub::file_stub_path_for(&abs_for_stub);
                let keep_stubbed = stub_threshold > 0
                    && size >= stub_threshold
                    && !stub::stub_would_be_visible_to_git(&stub_path, work_dir);
                if keep_stubbed {
                    // Leave as a stub.
                    stub::write(
                        work_dir,
                        &rel_path,
                        &stub::StubRecord {
                            target_type: stub::StubTargetType::Blob,
                            hash,
                            size,
                            mtime,
                            mode,
                            blob_count: 0,
                        },
                    )?;
                    continue;
                }
                let abs = base_dir.join(&name);
                // Ensure the blob (and all chunks) are cached locally, walking
                // per object so peak memory stays bounded by one chunk.
                if !local.exists(&hash)? {
                    crate::commands::push::transfer_objects(
                        remote, local, &hash, remote_key, true,
                    )?;
                }
                crate::fsmeta::materialise_blob_at(local, &hash, &abs, &mtime, &mode)?;
                materialised += 1;
            }
            TreeEntry::Tree {
                name,
                hash,
                mtime,
                size,
                blob_count,
            } => {
                let rel_path = format!("{}/{}", rel_base, name);
                let sub_dir = base_dir.join(&name);
                let dir_stub_path = sub_dir.join(stub::DIR_STUB_NAME);
                let keep_stubbed = stub_threshold > 0
                    && size >= stub_threshold
                    && !stub::stub_would_be_visible_to_git(&dir_stub_path, work_dir);
                if keep_stubbed {
                    // Leave as a directory stub.
                    fs::create_dir_all(&sub_dir)?;
                    stub::write_dir_stub(
                        work_dir,
                        &rel_path,
                        &stub::StubRecord {
                            target_type: stub::StubTargetType::Tree,
                            hash,
                            size,
                            mtime,
                            mode: None,
                            blob_count,
                        },
                    )?;
                    continue;
                }
                materialised += expand_tree(
                    &hash,
                    &sub_dir,
                    work_dir,
                    &rel_path,
                    local,
                    remote,
                    remote_key,
                    stub_threshold,
                )?;
                if let Some(mt) = mtime {
                    let ft = FileTime::from_unix_time(mt.timestamp(), mt.timestamp_subsec_nanos());
                    filetime::set_file_mtime(&sub_dir, ft).ok();
                }
            }
            TreeEntry::Symlink {
                name,
                target,
                mtime,
            } => {
                #[cfg(unix)]
                {
                    let link_path = base_dir.join(&name);
                    crate::fsmeta::write_symlink_atomic(&link_path, &target)?;
                    crate::fsmeta::restore_symlink_mtime(&link_path, &mtime);
                }
                let _ = target;
                materialised += 1;
            }
        }
    }
    Ok(materialised)
}
