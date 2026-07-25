use std::path::{Path, PathBuf};

use filetime::FileTime;
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::repo::Repo;

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

/// Sidecar metadata for a conflicting path: the tracked metadata of the base
/// and remote sides. The local side is intentionally absent — `accept-local`
/// keeps the working-tree file as-is, so its metadata needs no restoration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct ConflictMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<SideMeta>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<SideMeta>,
}

impl ConflictMeta {
    /// Write the sidecar JSON for the conflict whose base file is `base_path`.
    /// A no-op when neither side carries any metadata.
    pub(crate) fn write(&self, base_path: &std::path::Path) -> Result<(), Error> {
        if self.base.is_none() && self.remote.is_none() {
            return Ok(());
        }
        let meta_path = sidecar_path(base_path);
        let json = serde_json::to_vec_pretty(self).map_err(Error::Json)?;
        crate::store::local::atomic_write_with_no_fsync(&meta_path, |w| {
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
/// applies stub-record metadata.
fn restore_side_meta(abs_base: &std::path::Path, side: &SideMeta) {
    crate::fsmeta::apply_mode(abs_base, &side.mode);
    if let Some(mt) = side.mtime {
        let ft = FileTime::from_unix_time(mt.timestamp(), mt.timestamp_subsec_nanos());
        filetime::set_file_mtime(abs_base, ft).ok();
    }
}

pub struct ConflictListOptions {
    pub work_dir: PathBuf,
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

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

pub fn run_list(opts: ConflictListOptions) -> Result<(), Error> {
    let repo = Repo::open(&opts.work_dir)?;
    crate::progress::notify_repo_dir(&repo.work_dir);

    let paths = collect_conflicting_base_paths(&opts.work_dir);
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

    let scope_paths = resolve_scope_paths(&opts.work_dir, &opts.current_dir, &opts.paths)?;
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
            if let Err(e) = std::fs::remove_file(&abs) {
                return Err(Error::Io(e));
            }
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
            if meta_sidecar.exists() {
                std::fs::remove_file(&meta_sidecar).map_err(Error::Io)?;
            }
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

    let scope_paths = resolve_scope_paths(&opts.work_dir, &opts.current_dir, &opts.paths)?;
    let conflicting = collect_conflicting_base_paths_scoped(&opts.work_dir, &scope_paths);

    if conflicting.is_empty() {
        let scope_desc = if opts.paths.is_empty() {
            "working tree".to_string()
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
    let mut count = 0usize;

    for base in &sorted {
        let abs_base = opts.work_dir.join(base);
        let src_helper = opts
            .work_dir
            .join(format!("{}{}", base, opts.side.suffix()));

        if opts.dry_run {
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
            continue;
        }

        // Read the metadata sidecar before deleting it below, so the accepted
        // side's tracked mtime/mode can be restored.
        let conflict_meta = ConflictMeta::read(&abs_base);

        if src_helper.exists() {
            // Write through a temporary sibling and rename it into place, so
            // other processes never observe a partially written resolution.
            // Preserve the previous permissions for accept-local; remote/base
            // metadata below deliberately overrides them when available.
            let original_permissions = std::fs::symlink_metadata(&abs_base)
                .ok()
                .map(|metadata| metadata.permissions());
            let data = std::fs::read(&src_helper).map_err(Error::Io)?;
            crate::store::local::atomic_write(&abs_base, &data)?;
            if let Some(permissions) = original_permissions {
                std::fs::set_permissions(&abs_base, permissions).map_err(Error::Io)?;
            }
            // Restore the accepted side's tracked metadata (mtime + executable
            // bit) so the resolved file matches the state it came from. For the
            // remote/base sides this keeps the working tree clean against that
            // root — the next `push` is then a no-op rather than a needless
            // re-upload. The local side has no recorded metadata (it was never
            // changed away from the working-tree file), so nothing is restored.
            if let Some(cm) = &conflict_meta {
                let side = match opts.side {
                    AcceptSide::Remote => cm.remote.as_ref(),
                    AcceptSide::Base => cm.base.as_ref(),
                    AcceptSide::Local => None,
                };
                if let Some(side) = side {
                    restore_side_meta(&abs_base, side);
                }
            }
        } else {
            // Missing side means this side had no content (deleted or not-yet-created).
            // Accept = restore to that state = delete the file.
            if abs_base.exists() {
                std::fs::remove_file(&abs_base).map_err(Error::Io)?;
            }
        }

        // Remove all helper files for this base path (whichever exist), plus the
        // metadata sidecar.
        for suffix in CONFLICT_SUFFIXES {
            let helper = opts.work_dir.join(format!("{}{}", base, suffix));
            if helper.exists() {
                std::fs::remove_file(&helper).map_err(Error::Io)?;
            }
        }
        let meta_sidecar = opts
            .work_dir
            .join(format!("{}{}", base, CONFLICT_META_SUFFIX));
        if meta_sidecar.exists() {
            std::fs::remove_file(&meta_sidecar).map_err(Error::Io)?;
        }

        out.writeln(&format!("  accepted {}: {}", opts.side.label(), base))?;
        count += 1;
    }

    if !opts.dry_run || count > 0 {
        let verb = if opts.dry_run {
            "would be resolved (dry run)"
        } else {
            "resolved"
        };
        out.writeln(&format!(
            "{} conflict{} {}.",
            count,
            if count == 1 { "" } else { "s" },
            verb
        ))?;
    }
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
/// An empty scope list means the entire working tree.
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
                result.insert(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
}

/// Collect all helper file paths (relative to work_dir) in the given scopes.
/// An empty scope list means the entire working tree. For non-empty scopes we
/// walk only the requested subtrees (directory scopes) or check the helpers of
/// a single base path directly (file scopes). The result is deduplicated so
/// overlapping scopes do not yield the same helper twice.
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
            result.push(rel.to_string_lossy().replace('\\', "/"));
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

/// Reject a user scope that would reach its target through a symlink.
fn ensure_scope_is_physical(work_dir: &Path, scope: &str) -> Result<(), Error> {
    let mut current = work_dir.to_path_buf();
    for component in std::path::Path::new(scope).components() {
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(Error::Other(format!("path {} traverses a symlink", scope)));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(Error::Io(error)),
        }
    }
    Ok(())
}

/// Resolve user-supplied paths to repository-root-relative strings.
///
/// Invalid paths are errors: an explicitly supplied invalid scope must never
/// be represented as the empty scope, which means the entire working tree.
fn resolve_scope_paths(
    work_dir: &Path,
    current_dir: &Path,
    paths: &[PathBuf],
) -> Result<Vec<String>, Error> {
    let scopes: Vec<String> = paths
        .iter()
        .map(|path| crate::repo::normalize_path(path, work_dir, current_dir))
        .collect::<Result<_, _>>()?;
    for scope in &scopes {
        ensure_scope_is_physical(work_dir, scope)?;
    }
    Ok(scopes)
}
