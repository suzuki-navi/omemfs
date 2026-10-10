use std::path::{Path, PathBuf};

use filetime::FileTime;
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::object::{Hash, TreeEntry};
use crate::repo::Repo;
use crate::tree_ops;

/// Suffix of the sidecar file that records the tracked metadata (mtime and the
/// executable-bit mode) of the base and remote sides of a conflict. `pull`
/// writes it alongside the `.omemfs-conflict-*` helpers; `accept` reads it to
/// restore the accepted side's metadata so the resolved file matches the state
/// it came from. Without this, `accept-remote` / `accept-base` would leave the
/// file with a fresh mtime, so its tree-entry hash (which includes mtime and
/// mode) would differ from the remote/clone root and the next `push` would
/// needlessly re-upload it. See design/04_cli_spec.md (conflict).
pub(crate) const CONFLICT_META_SUFFIX: &str = ".omemfs-conflict-meta";

/// Tracked metadata for one side of a conflict.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct SideMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

/// What the remote held for a conflicting path at the moment `pull` detected
/// the conflict (design/03_sync_model.md, "Advancing clone_root on
/// resolution"). Recorded alongside the tracked metadata so `accept-*` can
/// splice this exact value into `clone_root`, independently of which side
/// the user ends up accepting for the working-tree content.
///
/// Only the two shapes a conflicting *file* path can take on the remote are
/// covered: a blob (`Blob`) or an absence (`Deleted`, e.g. local-add vs
/// remote-delete). A conflicting path whose remote side is a tree, symlink,
/// or stubbed subtree cannot occur here: `pull`'s conflict detection only
/// ever fires on two diff entries for the *same leaf path* with different
/// content, and only a blob-vs-blob (or blob-vs-absence) shape reaches that
/// comparison for a single file (see `write_conflict_for_path`'s
/// `entry_source`, which likewise only ever produces helper-file bytes for
/// `Added`/`Modified`). `Unrecorded` exists purely as a defensive fallback
/// for any future diff-entry shape that might reach this path but does not
/// yet carry a splice-able hash; `accept-*` leaves `clone_root` untouched for
/// it (the pre-fix behaviour), rather than guessing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum RemoteResolution {
    Blob {
        hash: Hash,
        size: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mtime: Option<chrono::DateTime<chrono::Utc>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<String>,
    },
    Deleted,
    Unrecorded,
}

/// Sidecar metadata for a conflicting path: the tracked metadata of the base
/// and remote sides, plus the remote's resolved value for advancing
/// `clone_root`. The local side's metadata is intentionally absent —
/// `accept-local` keeps the working-tree file as-is, so its metadata needs
/// no restoration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct ConflictMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<SideMeta>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<SideMeta>,
    /// Absent when read from a sidecar written by an older omemfs version
    /// (pre-dating this field): `accept-*` then leaves `clone_root` untouched
    /// for the path, exactly like `Some(RemoteResolution::Unrecorded)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_resolution: Option<RemoteResolution>,
}

impl ConflictMeta {
    /// Write the sidecar JSON for the conflict whose base file is `base_path`.
    /// A no-op when neither side carries any metadata and there is no
    /// clone_root advancement to record. `work_dir` must be the true
    /// repository root (design/03_sync_model.md, "Write safety: the
    /// working-tree write boundary") -- passing a scoped subdirectory would
    /// exempt everything at or above it from the check.
    pub(crate) fn write(
        &self,
        work_dir: &std::path::Path,
        base_path: &std::path::Path,
    ) -> Result<(), Error> {
        if self.base.is_none() && self.remote.is_none() && self.remote_resolution.is_none() {
            return Ok(());
        }
        let meta_path = sidecar_path(base_path);
        let json = serde_json::to_vec_pretty(self).map_err(Error::Json)?;
        crate::fsguard::atomic_write_with(work_dir, &meta_path, |w| {
            std::io::Write::write_all(w, &json).map_err(Error::Io)
        })
    }

    /// Read the sidecar JSON for `base_path`, if present and parseable.
    fn read(base_path: &std::path::Path) -> Option<ConflictMeta> {
        let meta_path = sidecar_path(base_path);
        let data = std::fs::read(&meta_path).ok()?;
        serde_json::from_slice(&data).ok()
    }
}

/// Path of the metadata sidecar file for a given base path.
fn sidecar_path(base_path: &std::path::Path) -> std::path::PathBuf {
    let s = base_path.to_string_lossy();
    std::path::PathBuf::from(format!("{}{}", s, CONFLICT_META_SUFFIX))
}

/// Restore mtime and mode onto `abs_base` from `side` (if present). Best-effort:
/// a missing or zero-valued field is left untouched, matching how `expand`
/// applies stub-record metadata. Resolved through `fsguard::open_leaf` (N2,
/// design/03_sync_model.md "Write safety: the working-tree write boundary")
/// rather than a path-based `apply_mode`/`filetime::set_file_mtime` call, so a
/// symlinked component cannot redirect these metadata mutations either.
fn restore_side_meta(work_dir: &std::path::Path, abs_base: &std::path::Path, side: &SideMeta) {
    let Ok(rel) = abs_base.strip_prefix(work_dir) else {
        return;
    };
    // `\` is an ordinary Linux filename byte, not a separator -- do not
    // rewrite it to `/` (design/01_object_model.md invariant #5).
    let rel_str = rel.to_string_lossy().into_owned();
    let Ok((dir, leaf)) = crate::fsguard::open_leaf(work_dir, &rel_str, false) else {
        return;
    };
    dir.set_mode(&leaf, &side.mode).ok();
    if let Some(mt) = side.mtime {
        let ft = FileTime::from_unix_time(mt.timestamp(), mt.timestamp_subsec_nanos());
        dir.set_mtime(&leaf, ft).ok();
    }
}

pub struct ConflictListOptions {
    pub work_dir: PathBuf,
    pub current_dir: PathBuf,
    pub paths: Vec<PathBuf>,
}

pub struct ConflictCleanOptions {
    pub work_dir: PathBuf,
    pub current_dir: PathBuf,
    pub paths: Vec<PathBuf>,
    pub dry_run: bool,
}

pub struct ConflictAcceptOptions {
    pub work_dir: PathBuf,
    pub current_dir: PathBuf,
    pub paths: Vec<PathBuf>,
    pub dry_run: bool,
    pub side: AcceptSide,
}

pub enum AcceptSide {
    Remote,
    Local,
    Base,
}

impl AcceptSide {
    fn suffix(&self) -> &'static str {
        match self {
            AcceptSide::Remote => ".omemfs-conflict-remote",
            AcceptSide::Local => ".omemfs-conflict-local",
            AcceptSide::Base => ".omemfs-conflict-base",
        }
    }

    fn label(&self) -> &'static str {
        match self {
            AcceptSide::Remote => "remote",
            AcceptSide::Local => "local",
            AcceptSide::Base => "base",
        }
    }
}

// Shared with pull.rs (writes these), restore.rs (removes these), and
// scan.rs (recognises/excludes these from the working-tree scan) --
// previously each had its own copy of these literals (refactor-instructions.md D3).
pub(crate) const CONFLICT_SUFFIX_BASE: &str = ".omemfs-conflict-base";
pub(crate) const CONFLICT_SUFFIX_LOCAL: &str = ".omemfs-conflict-local";
pub(crate) const CONFLICT_SUFFIX_REMOTE: &str = ".omemfs-conflict-remote";
pub(crate) const CONFLICT_SUFFIXES: &[&str] = &[
    CONFLICT_SUFFIX_BASE,
    CONFLICT_SUFFIX_LOCAL,
    CONFLICT_SUFFIX_REMOTE,
];

/// L6: every filename suffix that identifies a piece of omemfs-internal
/// conflict bookkeeping alongside a base path -- the three resolution-aid
/// helpers (`CONFLICT_SUFFIXES`) plus the `CONFLICT_META_SUFFIX` metadata
/// sidecar. None of these are user content: they exist only to support
/// `omemfs conflict`. Anything that needs to recognise/remove "any conflict
/// residue for this path" (restore's per-path cleanup, and the working-tree
/// scan's exclusion of internal files) should use this list -- or
/// [`is_conflict_artifact`] -- rather than keeping its own separate copy that
/// can silently drift out of sync with this one (the metadata sidecar being
/// missing from restore's old copy of `CONFLICT_SUFFIXES` is exactly the bug
/// this constant exists to prevent from recurring).
pub(crate) const CONFLICT_ARTIFACT_SUFFIXES: &[&str] = &[
    CONFLICT_SUFFIX_BASE,
    CONFLICT_SUFFIX_LOCAL,
    CONFLICT_SUFFIX_REMOTE,
    CONFLICT_META_SUFFIX,
];

/// Returns `true` if `name` ends with any suffix in
/// [`CONFLICT_ARTIFACT_SUFFIXES`] -- i.e. `name` is omemfs-internal conflict
/// bookkeeping (a helper or the metadata sidecar), never user content.
pub(crate) fn is_conflict_artifact(name: &str) -> bool {
    CONFLICT_ARTIFACT_SUFFIXES.iter().any(|s| name.ends_with(s))
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

pub fn run_list(opts: ConflictListOptions) -> Result<(), Error> {
    let repo = Repo::open(&opts.work_dir)?;
    crate::progress::notify_repo_dir(&repo.work_dir);

    let scope_paths =
        resolve_scope_paths_or_cwd(&opts.work_dir, &opts.current_dir, &opts.paths)?;
    let paths = collect_conflicting_base_paths_scoped(&opts.work_dir, &scope_paths);
    if paths.is_empty() {
        return Ok(());
    }

    let mut out = crate::term::Output::for_stdout();
    let mut sorted = paths.iter().cloned().collect::<Vec<_>>();
    sorted.sort();
    for p in &sorted {
        out.writeln(&format!("  {}", p))?;
    }
    let n = sorted.len();
    out.writeln(&format!(
        "{} path{} with unresolved conflicts.",
        n,
        if n == 1 { "" } else { "s" }
    ))?;
    out.finish()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// clean
// ---------------------------------------------------------------------------

pub fn run_clean(opts: ConflictCleanOptions) -> Result<(), Error> {
    let repo = Repo::open(&opts.work_dir)?;
    crate::progress::notify_repo_dir(&repo.work_dir);
    // Hold the repo lock: removing conflict helpers must not race with a
    // concurrent pull writing them. See design/12_locking.md.
    let _lock = repo.acquire_lock()?;

    let scope_paths =
        resolve_scope_paths_or_cwd(&opts.work_dir, &opts.current_dir, &opts.paths)?;
    let helper_files = collect_helper_files(&opts.work_dir, &scope_paths);

    let mut out = crate::term::Output::for_stdout();
    let mut cleaned_bases: std::collections::HashSet<String> = std::collections::HashSet::new();

    let mut sorted_helpers = helper_files;
    sorted_helpers.sort();

    for helper_path in &sorted_helpers {
        let abs = opts.work_dir.join(helper_path);
        if opts.dry_run {
            out.writeln(&format!("  would delete: {}", helper_path))?;
        } else {
            // design/03_sync_model.md, "Write safety: the working-tree write
            // boundary": a helper-file delete is a working-tree mutation
            // like any other, so it must go through
            // `fsguard::remove_file_tolerant` rather than a bare
            // `std::fs::remove_file`, which would follow a symlinked
            // ancestor of `helper_path` and could delete content outside
            // `work_dir`.
            crate::fsguard::remove_file_tolerant(&opts.work_dir, &abs)?;
            out.writeln(&format!("  deleted: {}", helper_path))?;
        }
        // Count unique base paths.
        let base = base_path_of_helper(helper_path);
        cleaned_bases.insert(base);
    }

    // Remove the metadata sidecar for each cleaned base path too, so `clean`
    // leaves no conflict residue behind.
    if !opts.dry_run {
        for base in &cleaned_bases {
            let meta_sidecar = opts
                .work_dir
                .join(format!("{}{}", base, CONFLICT_META_SUFFIX));
            crate::fsguard::remove_file_tolerant(&opts.work_dir, &meta_sidecar)?;
        }
    }

    let count = cleaned_bases.len();
    if count == 0 {
        // nothing to do
    } else if opts.dry_run {
        out.writeln(&format!(
            "{} conflict{} would be cleaned (dry run).",
            count,
            if count == 1 { "" } else { "s" }
        ))?;
    } else {
        out.writeln(&format!(
            "{} conflict{} cleaned.",
            count,
            if count == 1 { "" } else { "s" }
        ))?;
    }
    out.finish()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// accept-remote / accept-local / accept-base
// ---------------------------------------------------------------------------

pub fn run_accept(opts: ConflictAcceptOptions) -> Result<(), Error> {
    let repo = Repo::open(&opts.work_dir)?;
    crate::progress::notify_repo_dir(&repo.work_dir);
    let _lock = repo.acquire_lock()?;

    let scope_paths =
        resolve_scope_paths_or_cwd(&opts.work_dir, &opts.current_dir, &opts.paths)?;
    let conflicting = collect_conflicting_base_paths_scoped(&opts.work_dir, &scope_paths);

    if conflicting.is_empty() {
        let scope_desc = if opts.paths.is_empty() {
            "current directory".to_string()
        } else {
            opts.paths
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        return Err(Error::Other(format!(
            "no conflict files found for '{}'",
            scope_desc
        )));
    }

    let mut out = crate::term::Output::for_stdout();
    let mut sorted = conflicting.into_iter().collect::<Vec<_>>();
    sorted.sort();

    if opts.dry_run {
        // --dry-run makes no changes and touches `clone_root` and the remote
        // not at all: report what each path's helper file implies without
        // reading the metadata sidecar or building a tree store.
        let mut count = 0usize;
        for base in &sorted {
            let src_helper = opts
                .work_dir
                .join(format!("{}{}", base, opts.side.suffix()));
            if src_helper.exists() {
                out.writeln(&format!("  would accept {}: {}", opts.side.label(), base))?;
            } else {
                out.writeln(&format!(
                    "  would delete (no {} version): {}",
                    opts.side.label(),
                    base
                ))?;
            }
            count += 1;
        }
        out.writeln(&format!(
            "{} conflict{} would be resolved (dry run).",
            count,
            if count == 1 { "" } else { "s" }
        ))?;
        out.finish()?;
        return Ok(());
    }

    // Read every path's metadata sidecar up front, before any file is
    // touched, so phase A below can compute the fully-advanced clone_root
    // before phase B starts changing the working tree (design/03_sync_model.md,
    // "Advancing clone_root on resolution", "Ordering guarantee").
    let metas: Vec<(String, Option<ConflictMeta>)> = sorted
        .iter()
        .map(|base| {
            let abs_base = opts.work_dir.join(base);
            (base.clone(), ConflictMeta::read(&abs_base))
        })
        .collect();

    // Clone_root tree objects along a resolved path may be absent locally
    // after a lazy clone; route splice/remove reads through the same
    // read-through lazy tree store `pull`/`ls` use (design/03_sync_model.md,
    // "Lazy tree reads"). Constructing the reader itself makes no remote
    // call -- only an actual local-cache miss during splice/remove below
    // reaches the remote -- so this keeps accept fully offline whenever
    // every needed tree is already local.
    let local = repo.local_store();
    let (pack_reader, _remote, remote_key) = repo.pack_reader("origin", None)?;
    let lazy = tree_ops::LazyTreeStore::new(&local, &pack_reader, remote_key.as_ref());

    // ------------------------------------------------------------------
    // Phase A: compute the fully advanced clone_root in memory only --
    // no filesystem change to the working tree, no helper/sidecar removal.
    // Any error here (e.g. a clone_root tree object unresolvable even
    // through `lazy`) aborts with nothing changed: the sidecars read above
    // are not yet acted on, and every helper file and sidecar is still on
    // disk exactly as it was.
    // ------------------------------------------------------------------
    let mut new_clone_root = repo.read_clone_root()?;
    let mut clone_root_advanced = false;

    for (base, conflict_meta) in &metas {
        // `Unrecorded` (defensive fallback, or a sidecar from an older
        // omemfs version) or a missing sidecar leaves clone_root untouched
        // for this path, the pre-fix behaviour.
        let Some(cm) = conflict_meta else { continue };
        match &cm.remote_resolution {
            Some(RemoteResolution::Blob {
                hash,
                size,
                mtime,
                mode,
            }) => {
                let comps: Vec<&str> = base.split('/').filter(|s| !s.is_empty()).collect();
                let leaf_name = comps.last().copied().unwrap_or(base.as_str()).to_string();
                let new_entry = TreeEntry::Blob {
                    name: leaf_name,
                    hash: hash.clone(),
                    mtime: *mtime,
                    size: *size,
                    mode: mode.clone(),
                };
                new_clone_root = Some(tree_ops::splice_entry(
                    new_clone_root.as_ref(),
                    &comps,
                    new_entry,
                    &lazy,
                )?);
                clone_root_advanced = true;
            }
            Some(RemoteResolution::Deleted) => {
                let comps: Vec<&str> = base.split('/').filter(|s| !s.is_empty()).collect();
                if let Some(ref cr) = new_clone_root {
                    if let Some(new_hash) = tree_ops::remove_entry(cr, &comps, &lazy)? {
                        new_clone_root = Some(new_hash);
                        clone_root_advanced = true;
                    }
                }
            }
            Some(RemoteResolution::Unrecorded) | None => {}
        }
    }

    // ------------------------------------------------------------------
    // Phase B: apply the working-tree content change for each path.
    // Helper files and the metadata sidecar are deliberately left in place
    // here -- they are removed only in phase D, after clone_root has been
    // durably advanced in phase C.
    // ------------------------------------------------------------------
    for (base, conflict_meta) in &metas {
        let abs_base = opts.work_dir.join(base);
        let src_helper = opts
            .work_dir
            .join(format!("{}{}", base, opts.side.suffix()));

        if src_helper.exists() {
            // Write through a temporary sibling and rename it into place, so
            // other processes never observe a partially written resolution.
            // Preserve the previous permissions for accept-local; remote/base
            // metadata below deliberately overrides them when available.
            // Routed through `fsguard::atomic_write_durable` rather than a
            // plain `store::local::atomic_write` (design/03_sync_model.md,
            // "Write safety: the working-tree write boundary") -- `work_dir`
            // must be the true repository root.
            let original_permissions = std::fs::symlink_metadata(&abs_base)
                .ok()
                .map(|metadata| metadata.permissions());
            let data = std::fs::read(&src_helper).map_err(Error::Io)?;
            crate::fsguard::atomic_write_durable(&opts.work_dir, &abs_base, &data)?;
            if let Some(permissions) = original_permissions {
                std::fs::set_permissions(&abs_base, permissions).map_err(Error::Io)?;
            }
            // Restore the accepted side's tracked metadata (mtime + executable
            // bit) so the resolved file matches the state it came from. For the
            // remote/base sides this keeps the working tree clean against that
            // root — the next `push` is then a no-op rather than a needless
            // re-upload. The local side has no recorded metadata (it was never
            // changed away from the working-tree file), so nothing is restored.
            if let Some(cm) = conflict_meta {
                let side = match opts.side {
                    AcceptSide::Remote => cm.remote.as_ref(),
                    AcceptSide::Base => cm.base.as_ref(),
                    AcceptSide::Local => None,
                };
                if let Some(side) = side {
                    restore_side_meta(&opts.work_dir, &abs_base, side);
                }
            }
        } else {
            // Missing side means this side had no content (deleted or not-yet-created).
            // Accept = restore to that state = delete the file.
            crate::fsguard::remove_file_tolerant(&opts.work_dir, &abs_base)?;
        }
    }

    // ------------------------------------------------------------------
    // Phase C: write the advanced clone_root once, durably. This must land
    // before phase D removes any helper file or sidecar below (design/
    // 03_sync_model.md, "Advancing clone_root on resolution", "Ordering
    // guarantee") -- otherwise an interruption between the two could lose
    // the sidecar's recorded `remote_resolution` while clone_root remains
    // unadvanced, leaving the conflict unrecoverable.
    // ------------------------------------------------------------------
    if clone_root_advanced {
        if let Some(ref cr) = new_clone_root {
            repo.write_clone_root(cr)?;
        }
    }

    // ------------------------------------------------------------------
    // Phase D: only now remove the helper files and the metadata sidecar
    // for each resolved path, plus report it. Routed through fsguard
    // (N1/design boundary).
    // ------------------------------------------------------------------
    let mut count = 0usize;
    for (base, _) in &metas {
        for suffix in CONFLICT_SUFFIXES {
            let helper = opts.work_dir.join(format!("{}{}", base, suffix));
            crate::fsguard::remove_file_tolerant(&opts.work_dir, &helper)?;
        }
        let meta_sidecar = opts
            .work_dir
            .join(format!("{}{}", base, CONFLICT_META_SUFFIX));
        crate::fsguard::remove_file_tolerant(&opts.work_dir, &meta_sidecar)?;

        out.writeln(&format!("  accepted {}: {}", opts.side.label(), base))?;
        count += 1;
    }

    out.writeln(&format!(
        "{} conflict{} resolved.",
        count,
        if count == 1 { "" } else { "s" }
    ))?;
    out.finish()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Collect unique base paths (the conflicting file path, not the helper path)
/// from the entire working tree.
fn collect_conflicting_base_paths(work_dir: &PathBuf) -> std::collections::HashSet<String> {
    let mut result = std::collections::HashSet::new();
    collect_conflicting_recursive(work_dir, work_dir, &mut result);
    result
}

/// Collect conflicting base paths restricted to the given scope list.
/// An empty scope list, or a list containing the empty string (the
/// normalised form of the repository root), means the entire working tree.
/// Callers (`run_list`/`run_accept`) never pass an empty list themselves --
/// they first default a missing `<path>...` argument to the cwd, which
/// normalises to `""` when the cwd is the repository root.
///
/// For non-empty scopes we walk only the requested subtrees (directory scopes)
/// or check a single base path directly (file scopes), rather than walking the
/// whole tree and filtering afterwards.
fn collect_conflicting_base_paths_scoped(
    work_dir: &PathBuf,
    scopes: &[String],
) -> std::collections::HashSet<String> {
    if scopes.is_empty() {
        return collect_conflicting_base_paths(work_dir);
    }
    let mut result = std::collections::HashSet::new();
    for scope in scopes {
        let abs = work_dir.join(scope);
        if is_real_directory(&abs) {
            collect_conflicting_recursive(work_dir, &abs, &mut result);
        } else if base_has_conflict_helpers(work_dir, scope) {
            // File scope: the base file itself may be absent, so check helpers.
            result.insert(scope.clone());
        }
    }
    result
}

/// Returns true if any of the three conflict helper files exist for `base`.
fn base_has_conflict_helpers(work_dir: &Path, base: &str) -> bool {
    CONFLICT_SUFFIXES
        .iter()
        .any(|s| work_dir.join(format!("{}{}", base, s)).is_file())
}

fn collect_conflicting_recursive(
    work_dir: &PathBuf,
    dir: &std::path::Path,
    result: &mut std::collections::HashSet<String>,
) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };
        let path = entry.path();
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue,
        };
        if name == ".omemfs" {
            continue;
        }
        if file_type.is_dir() {
            collect_conflicting_recursive(work_dir, &path, result);
        } else if let Some(base_name) = strip_conflict_suffix(&name) {
            let base_path = path.parent().unwrap_or(dir).join(base_name);
            if let Ok(rel) = base_path.strip_prefix(work_dir) {
                result.insert(rel.to_string_lossy().into_owned());
            }
        }
    }
}

/// Collect all helper file paths (relative to work_dir) in the given scopes.
/// An empty scope list, or a list containing the empty string (the
/// normalised form of the repository root, see
/// `collect_conflicting_base_paths_scoped`), means the entire working tree.
/// For non-empty scopes we walk only the requested subtrees (directory
/// scopes) or check the helpers of a single base path directly (file
/// scopes). The result is deduplicated so overlapping scopes do not yield
/// the same helper twice.
fn collect_helper_files(work_dir: &PathBuf, scopes: &[String]) -> Vec<String> {
    let mut result = Vec::new();
    if scopes.is_empty() {
        collect_helper_files_recursive(work_dir, work_dir, &mut result);
    } else {
        for scope in scopes {
            let abs = work_dir.join(scope);
            if is_real_directory(&abs) {
                collect_helper_files_recursive(work_dir, &abs, &mut result);
            } else {
                // File scope: collect its helper files directly.
                for suffix in CONFLICT_SUFFIXES {
                    if work_dir.join(format!("{}{}", scope, suffix)).is_file() {
                        result.push(format!("{}{}", scope, suffix));
                    }
                }
            }
        }
    }
    result.sort();
    result.dedup();
    result
}

fn collect_helper_files_recursive(
    work_dir: &PathBuf,
    dir: &std::path::Path,
    result: &mut Vec<String>,
) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };
        let path = entry.path();
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue,
        };
        if name == ".omemfs" {
            continue;
        }
        if file_type.is_dir() {
            collect_helper_files_recursive(work_dir, &path, result);
        } else if is_conflict_helper(&name)
            && let Ok(rel) = path.strip_prefix(work_dir)
        {
            result.push(rel.to_string_lossy().into_owned());
        }
    }
}

fn is_conflict_helper(name: &str) -> bool {
    CONFLICT_SUFFIXES.iter().any(|s| name.ends_with(s))
}

fn strip_conflict_suffix(name: &str) -> Option<&str> {
    for suffix in CONFLICT_SUFFIXES {
        if let Some(base) = name.strip_suffix(suffix) {
            return Some(base);
        }
    }
    None
}

fn base_path_of_helper(helper_rel: &str) -> String {
    for suffix in CONFLICT_SUFFIXES {
        if let Some(base) = helper_rel.strip_suffix(suffix) {
            return base.to_string();
        }
    }
    helper_rel.to_string()
}

/// Returns whether `path` itself is a directory, without following a symlink.
fn is_real_directory(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_dir())
        .unwrap_or(false)
}

/// Resolve user-supplied paths to repository-root-relative strings.
///
/// Invalid paths are errors: an explicitly supplied invalid scope must never
/// be represented as the empty scope, which means the entire working tree.
///
/// This used to also reject a scope that reached its target through a
/// symlink (`ensure_scope_is_physical`, a third independent lexical/TOCTOU-
/// prone boundary walk). That check is now dead weight: every write this
/// module performs (`run_accept`'s content write and helper/sidecar removals)
/// is guarded intrinsically by `fsguard` (design/03_sync_model.md, "Write
/// safety: the working-tree write boundary"), so a symlinked scope is
/// refused at write time regardless of what this function returns.
fn resolve_scope_paths(
    work_dir: &Path,
    current_dir: &Path,
    paths: &[PathBuf],
) -> Result<Vec<String>, Error> {
    paths
        .iter()
        .map(|path| crate::repo::normalize_path(path, work_dir, current_dir))
        .collect::<Result<_, _>>()
}

/// `resolve_scope_paths`, but first defaults an empty `paths` to the current
/// directory, exactly like `push`/`pull` (design/04_cli_spec.md, "omemfs
/// conflict"). If cwd == repo root, this normalizes to "" and scans the whole
/// working tree. Shared by `run_list`, `run_clean`, and `run_accept`, which
/// all apply this same default.
fn resolve_scope_paths_or_cwd(
    work_dir: &Path,
    current_dir: &Path,
    paths: &[PathBuf],
) -> Result<Vec<String>, Error> {
    if paths.is_empty() {
        resolve_scope_paths(work_dir, current_dir, &[current_dir.to_path_buf()])
    } else {
        resolve_scope_paths(work_dir, current_dir, paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_conflict_artifact_matches_all_three_helpers_and_the_meta_sidecar() {
        assert!(is_conflict_artifact("file.txt.omemfs-conflict-base"));
        assert!(is_conflict_artifact("file.txt.omemfs-conflict-local"));
        assert!(is_conflict_artifact("file.txt.omemfs-conflict-remote"));
        assert!(is_conflict_artifact("file.txt.omemfs-conflict-meta"));
    }

    #[test]
    fn is_conflict_artifact_rejects_ordinary_and_unrelated_reserved_names() {
        assert!(!is_conflict_artifact("file.txt"));
        assert!(!is_conflict_artifact("file.txt.omemfs-stub"));
        assert!(!is_conflict_artifact(".omemfs-stub"));
    }

    #[test]
    fn conflict_artifact_suffixes_is_a_superset_of_conflict_suffixes() {
        for suffix in CONFLICT_SUFFIXES {
            assert!(CONFLICT_ARTIFACT_SUFFIXES.contains(suffix));
        }
        assert!(CONFLICT_ARTIFACT_SUFFIXES.contains(&CONFLICT_META_SUFFIX));
        assert_eq!(CONFLICT_ARTIFACT_SUFFIXES.len(), CONFLICT_SUFFIXES.len() + 1);
    }
}
