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
///
/// Re-exported from `object.rs` (not defined here) so that `scan.rs` (a
/// low-level module) can recognise conflict-artifact filenames without
/// depending on this command module -- see `object::CONFLICT_META_SUFFIX`'s
/// own doc comment. Kept as a `pub(crate) use` rather than switched to a
/// direct reference at every call site, so every existing caller of
/// `conflict::CONFLICT_META_SUFFIX` is unchanged.
pub(crate) use crate::object::CONFLICT_META_SUFFIX;

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
/// Three shapes a conflicting *file* path can take on the remote are
/// covered: a blob (`Blob`), an absence (`Deleted`, e.g. local-add vs
/// remote-delete), or a symlink (`Symlink`, e.g. local-edit vs the remote
/// having turned the path into a symlink -- design/03_sync_model.md, "Remote
/// side is a symlink"). A conflicting path whose remote side is a tree or a
/// stubbed subtree cannot occur here: `pull`'s conflict detection only ever
/// fires on two diff entries for the *same leaf path* with different
/// content, and only a blob-vs-blob, blob-vs-absence, or blob-vs-symlink
/// shape reaches that comparison for a single file (see
/// `write_conflict_for_path`'s `entry_source`, which produces helper-file
/// bytes for `Added`/`Modified` and, for `Symlink`, no helper file at all --
/// "Missing sides" -- but the shape is still captured here). `Unrecorded`
/// exists purely as a defensive fallback for any future diff-entry shape
/// that might reach this path but does not yet carry a splice-able value;
/// `accept-*` leaves `clone_root` untouched for it (the pre-fix behaviour
/// this whole enum replaces), rather than guessing.
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
    /// The remote turned the path into a symlink (design/03_sync_model.md,
    /// "Remote side is a symlink"). Symlinks have no executable-bit `mode`.
    Symlink {
        target: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mtime: Option<chrono::DateTime<chrono::Utc>>,
    },
    Unrecorded,
}

/// Which sides of a conflict had content when `pull` wrote the helpers: each
/// flag is true exactly when the matching `.omemfs-conflict-*` helper was
/// written. `accept-*` decides from this rather than from helper-file
/// presence alone (design/03_sync_model.md, "The sidecar records which sides
/// exist").
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(crate) struct Sides {
    pub base: bool,
    pub local: bool,
    pub remote: bool,
}

impl Sides {
    fn has(&self, side: &AcceptSide) -> bool {
        match side {
            AcceptSide::Base => self.base,
            AcceptSide::Local => self.local,
            AcceptSide::Remote => self.remote,
        }
    }
}

/// Sidecar metadata for a conflicting path: the tracked metadata of the base
/// and remote sides, plus the remote's resolved value for advancing
/// `clone_root`. The local side's metadata is intentionally absent —
/// `accept-local` keeps the working-tree file as-is, so its metadata needs
/// no restoration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct ConflictMeta {
    /// Absent when read from a sidecar written before this field existed (or
    /// a hand-made one): `accept-*` then falls back to helper-file presence.
    /// Always filled in by [`write_conflict_artifacts`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sides: Option<Sides>,
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
        if self.sides.is_none()
            && self.base.is_none()
            && self.remote.is_none()
            && self.remote_resolution.is_none()
        {
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
// Re-exported from `object.rs` -- see this module's `CONFLICT_META_SUFFIX`
// re-export above for why, and `object::CONFLICT_SUFFIXES`'s own doc comment.
pub(crate) use crate::object::{
    CONFLICT_ARTIFACT_SUFFIXES, CONFLICT_SUFFIX_BASE, CONFLICT_SUFFIX_LOCAL,
    CONFLICT_SUFFIX_REMOTE, CONFLICT_SUFFIXES, is_conflict_artifact,
};

/// Remove every artifact in [`CONFLICT_ARTIFACT_SUFFIXES`] (the three helper
/// files and the metadata sidecar) for `base_path`, tolerating any that are
/// already absent. Routed through `fsguard::remove_file_tolerant`
/// (design/03_sync_model.md, "Write safety: the working-tree write
/// boundary") rather than a bare `fs::remove_file`.
///
/// The single removal primitive behind both call sites that need "clear all
/// conflict residue for this path": `pull` calls it right before writing a
/// path's fresh helpers/sidecar, so a helper left over from an earlier,
/// differently-shaped conflict on the same path can never be mistaken for
/// part of the conflict `pull` just detected (design/03_sync_model.md,
/// "Missing sides", "Clearing stale residue before writing" -- this was
/// D8's root cause: `accept-*` decides what to apply purely from whether a
/// helper file exists, so a stale one is indistinguishable from a genuine
/// one). `restore`'s own per-path cleanup (`remove_conflict_helpers`) uses it
/// too, so both write paths for "remove all conflict artifacts here" share
/// one implementation instead of two copies that could drift apart.
pub(crate) fn remove_all_conflict_artifacts(
    work_dir: &Path,
    base_path: &Path,
) -> Result<(), Error> {
    let s = base_path.to_string_lossy();
    for suffix in CONFLICT_ARTIFACT_SUFFIXES {
        let p = PathBuf::from(format!("{}{}", s, suffix));
        crate::fsguard::remove_file_tolerant(work_dir, &p)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// write (used by pull)
// ---------------------------------------------------------------------------

/// One side (base / local / remote) of a conflict, resolved to a streamable
/// source instead of an in-memory byte buffer. Each variant is written to its
/// helper file chunk-by-chunk so no whole-blob buffer is ever held -- for a
/// GB-class conflicted file this keeps peak memory bounded at roughly one chunk
/// (<= CDC_MAX ~ 16 MiB) per side rather than the full file size.
pub(crate) enum HelperSource {
    /// A blob in the object store, addressed by its logical hash. Streamed to
    /// the helper file via `codec::chunk::materialise_to_file`.
    Blob(Hash),
    /// A working-tree file held open at construction time. Holding the `File`
    /// rather than a path avoids re-opening and prevents TOCTOU races: if the
    /// file is deleted after it was opened the fd stays valid on Linux.
    WorkingFile(std::fs::File),
}

/// Write the conflict artifacts for one conflicting path: clear every
/// artifact left by an earlier conflict on the same path, write the helper
/// for each side that has content, then write the sidecar with `sides`
/// recording exactly which helpers were written.
///
/// The single write entry point for conflict artifacts (design/03_sync_model.md,
/// "The sidecar records which sides exist", "Single owner"). `work_dir` must
/// be the true repository root. `meta.sides` is overwritten here so it can
/// never disagree with the helpers actually written.
pub(crate) fn write_conflict_artifacts(
    work_dir: &Path,
    base_path: &Path,
    base: Option<&HelperSource>,
    local: Option<&HelperSource>,
    remote: Option<&HelperSource>,
    mut meta: ConflictMeta,
    store: &dyn crate::store::ObjectStore,
) -> Result<(), Error> {
    // A "missing side" in this conflict must never be shadowed by a stale
    // helper from a previous conflict on the same path (design/03_sync_model.md,
    // "Missing sides", "Clearing stale residue before writing").
    remove_all_conflict_artifacts(work_dir, base_path)?;
    for (suffix, source) in [
        (CONFLICT_SUFFIX_BASE, base),
        (CONFLICT_SUFFIX_LOCAL, local),
        (CONFLICT_SUFFIX_REMOTE, remote),
    ] {
        if let Some(src) = source {
            write_helper_file(work_dir, base_path, suffix, src, store)?;
        }
    }
    meta.sides = Some(Sides {
        base: base.is_some(),
        local: local.is_some(),
        remote: remote.is_some(),
    });
    meta.write(work_dir, base_path)
}

/// Stream `source` into the helper file `<base_path><suffix>`.
///
///   - `Blob(hash)`: `materialise_to_file` streams the (possibly chunked) blob
///     chunk-by-chunk into a temp file in the destination dir, then atomically
///     renames it (the `ED F0` blob-escape is stripped from the first chunk).
///   - `WorkingFile(file)`: `io::copy` streams the working-tree file into the
///     same atomic temp+rename writer, never reading it whole.
fn write_helper_file(
    work_dir: &Path,
    base_path: &Path,
    suffix: &str,
    source: &HelperSource,
    store: &dyn crate::store::ObjectStore,
) -> Result<(), Error> {
    let helper_path = {
        let s = base_path.to_string_lossy();
        PathBuf::from(format!("{}{}", s, suffix))
    };
    match source {
        HelperSource::Blob(hash) => {
            crate::codec::chunk::materialise_to_file(work_dir, store, hash, None, &helper_path)?;
        }
        HelperSource::WorkingFile(file) => {
            // Same working-tree write boundary as the `Blob` arm (which gets
            // it via `materialise_to_file`): `helper_path` shares its parent
            // directory with `base_path`, so a directory swapped for a
            // symlink pointing outside `work_dir` must not be followed. See
            // design/03_sync_model.md, "Write safety: the working-tree write
            // boundary".
            crate::fsguard::atomic_write_with(work_dir, &helper_path, |writer| {
                // `&File` implements `Read`, so `io::copy` works without
                // consuming or mutably borrowing through the enum reference.
                std::io::copy(&mut &*file, writer)
                    .map(|_| ())
                    .map_err(Error::Io)
            })?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// read (used by accept-*)
// ---------------------------------------------------------------------------

/// What `accept-<side>` does with one conflicting path.
#[derive(Debug, PartialEq, Eq)]
enum Resolution {
    /// Overwrite the path with the accepted side's helper file.
    Helper(PathBuf),
    /// The accepted side has no content: delete the path.
    Delete,
    /// The remote side is a symlink (no helper is ever written for it): create it.
    RemoteSymlink,
}

/// Decide what accepting `side` means for `base`, from the sidecar's recorded
/// `sides` (falling back to helper-file presence when none were recorded).
/// Errs, without touching anything, when the helper file on disk contradicts
/// the recorded `sides` (design/03_sync_model.md, "The sidecar records which
/// sides exist").
fn resolve_side(
    work_dir: &Path,
    base: &str,
    side: &AcceptSide,
    meta: Option<&ConflictMeta>,
) -> Result<Resolution, Error> {
    let helper = work_dir.join(format!("{}{}", base, side.suffix()));
    let exists = helper.exists();
    let has_content = match meta.and_then(|m| m.sides) {
        Some(sides) => {
            let recorded = sides.has(side);
            if recorded != exists {
                let problem = if recorded { "missing" } else { "unexpected" };
                return Err(Error::Other(format!(
                    "conflict artifacts for '{}' are inconsistent ({} helper {}); \
                     run 'omemfs pull' again, or 'omemfs conflict clean' / 'omemfs restore'",
                    base,
                    side.label(),
                    problem
                )));
            }
            recorded
        }
        None => exists,
    };
    if has_content {
        Ok(Resolution::Helper(helper))
    } else if matches!(side, AcceptSide::Remote)
        && matches!(
            meta.and_then(|m| m.remote_resolution.as_ref()),
            Some(RemoteResolution::Symlink { .. })
        )
    {
        Ok(Resolution::RemoteSymlink)
    } else {
        Ok(Resolution::Delete)
    }
}

/// `resolve_side` for every path; on any inconsistency returns one error that
/// lists all offending paths, before the caller has changed anything.
fn resolve_all(
    work_dir: &Path,
    side: &AcceptSide,
    metas: &[(String, Option<ConflictMeta>)],
) -> Result<Vec<Resolution>, Error> {
    let mut resolutions = Vec::with_capacity(metas.len());
    let mut problems: Vec<String> = Vec::new();
    for (base, meta) in metas {
        match resolve_side(work_dir, base, side, meta.as_ref()) {
            Ok(r) => resolutions.push(r),
            Err(e) => problems.push(e.to_string()),
        }
    }
    if problems.is_empty() {
        Ok(resolutions)
    } else {
        Err(Error::Other(problems.join("\n")))
    }
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

pub fn run_list(opts: ConflictListOptions) -> Result<(), Error> {
    let repo = Repo::open(&opts.work_dir)?;
    crate::progress::notify_repo_dir(&repo.work_dir);

    let scope_paths = resolve_scope_paths_or_cwd(&opts.work_dir, &opts.current_dir, &opts.paths)?;
    let paths = collect_conflicting_base_paths_scoped(&opts.work_dir, &scope_paths);
    if paths.is_empty() {
        return Ok(());
    }

    let mut out = crate::term::Output::for_stdout();
    let mut sorted = paths.iter().cloned().collect::<Vec<_>>();
    sorted.sort();
    for p in &sorted {
        out.writeln(&format!("  {}", crate::term::escape_display(p)))?;
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

    let scope_paths = resolve_scope_paths_or_cwd(&opts.work_dir, &opts.current_dir, &opts.paths)?;
    let helper_files = collect_helper_files(&opts.work_dir, &scope_paths);

    let mut out = crate::term::Output::for_stdout();
    let mut cleaned_bases: std::collections::HashSet<String> = std::collections::HashSet::new();

    let mut sorted_helpers = helper_files;
    sorted_helpers.sort();

    for helper_path in &sorted_helpers {
        let abs = opts.work_dir.join(helper_path);
        if opts.dry_run {
            out.writeln(&format!("  would delete: {}", crate::term::escape_display(helper_path)))?;
        } else {
            // design/03_sync_model.md, "Write safety: the working-tree write
            // boundary": a helper-file delete is a working-tree mutation
            // like any other, so it must go through
            // `fsguard::remove_file_tolerant` rather than a bare
            // `std::fs::remove_file`, which would follow a symlinked
            // ancestor of `helper_path` and could delete content outside
            // `work_dir`.
            crate::fsguard::remove_file_tolerant(&opts.work_dir, &abs)?;
            out.writeln(&format!("  deleted: {}", crate::term::escape_display(helper_path)))?;
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

    let scope_paths = resolve_scope_paths_or_cwd(&opts.work_dir, &opts.current_dir, &opts.paths)?;
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

    // Decide every path's resolution now, dry run or not: an inconsistency
    // between the recorded sides and the helpers on disk aborts here, before
    // any change, and a dry run must not predict a resolution the real run
    // would refuse (design/03_sync_model.md, "The sidecar records which sides
    // exist").
    let resolutions = resolve_all(&opts.work_dir, &opts.side, &metas)?;

    if opts.dry_run {
        // --dry-run makes no changes and touches `clone_root` and the remote
        // not at all: no tree store is built.
        let mut count = 0usize;
        for (base, resolution) in sorted.iter().zip(&resolutions) {
            match resolution {
                Resolution::Delete => out.writeln(&format!(
                    "  would delete (no {} version): {}",
                    opts.side.label(),
                    crate::term::escape_display(base)
                ))?,
                Resolution::Helper(_) | Resolution::RemoteSymlink => out.writeln(&format!(
                    "  would accept {}: {}",
                    opts.side.label(),
                    crate::term::escape_display(base)
                ))?,
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
            Some(RemoteResolution::Symlink { target, mtime }) => {
                // Advance clone_root to the remote's exact symlink value,
                // independently of which side's content ends up in the
                // working tree (design/03_sync_model.md, "Remote side is a
                // symlink") -- the same treatment `Blob` gets above.
                let comps: Vec<&str> = base.split('/').filter(|s| !s.is_empty()).collect();
                let leaf_name = comps.last().copied().unwrap_or(base.as_str()).to_string();
                let new_entry = TreeEntry::Symlink {
                    name: leaf_name,
                    target: target.clone(),
                    mtime: *mtime,
                };
                new_clone_root = Some(tree_ops::splice_entry(
                    new_clone_root.as_ref(),
                    &comps,
                    new_entry,
                    &lazy,
                )?);
                clone_root_advanced = true;
            }
            Some(RemoteResolution::Unrecorded) | None => {}
        }
    }

    // ------------------------------------------------------------------
    // Phase B: apply the working-tree content change for each path.
    // Helper files and the metadata sidecar are deliberately left in place
    // here -- they are removed only in phase D, after clone_root has been
    // durably advanced in phase C.
    //
    // `deleted_paths` records which resolved paths took the "missing side"
    // branch (the file was removed, not overwritten) -- these are the only
    // paths whose parent directory can end up newly emptied by this
    // resolution, so phase D's directory-pruning walk (design/03_sync_model.md,
    // "Pruning directories emptied by conflict resolution") only starts from
    // them.
    // ------------------------------------------------------------------
    let mut deleted_paths: Vec<&str> = Vec::new();
    for ((base, conflict_meta), resolution) in metas.iter().zip(&resolutions) {
        let abs_base = opts.work_dir.join(base);

        if let Resolution::Helper(src_helper) = resolution {
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
            let data = std::fs::read(src_helper).map_err(Error::Io)?;
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
        } else if matches!(resolution, Resolution::RemoteSymlink)
            && let Some(cm) = conflict_meta
            && let Some(RemoteResolution::Symlink { target, mtime }) = &cm.remote_resolution
        {
            // No `.omemfs-conflict-remote` helper is ever written for this
            // shape (design/03_sync_model.md, "Missing sides", "Remote side
            // is a symlink") -- the remote side has no blob content, only a
            // symlink target string. `accept-remote` must still materialise
            // that symlink itself, replacing whatever is currently at the
            // path (a plain file here, but `write_symlink` -- via
            // `replace_symlink` -- also handles a stale empty directory or a
            // dangling symlink left at the path, exactly like `pull`'s own
            // `DiffEntry::Symlink` application), and restore its own mtime
            // via lutimes (symlinks have no executable-bit mode to restore).
            crate::fsguard::write_symlink(&opts.work_dir, &abs_base, target)?;
            crate::fsmeta::restore_symlink_mtime(&abs_base, mtime);
        } else {
            // Missing side means this side had no content (deleted or not-yet-created).
            // Accept = restore to that state = delete the file.
            crate::fsguard::remove_file_tolerant(&opts.work_dir, &abs_base)?;
            deleted_paths.push(base.as_str());
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
        remove_all_conflict_artifacts(&opts.work_dir, &opts.work_dir.join(base))?;

        out.writeln(&format!("  accepted {}: {}", opts.side.label(), crate::term::escape_display(base)))?;
        count += 1;
    }

    // Prune directories newly emptied by this resolution (design/03_sync_model.md,
    // "Pruning directories emptied by conflict resolution"). Only run now, after
    // every helper file and sidecar above has been removed, so a path's own
    // `.omemfs-conflict-*` residue never masks its parent directory as
    // non-empty. Only starts from a path that was actually deleted in phase B
    // (`deleted_paths`) -- a path whose content was overwritten in place never
    // empties its parent. Walk each such path's ancestor chain up to (but not
    // including) the repository root, deepest first, stopping at the first
    // directory that still has content (which also naturally stops at a
    // directory shared with an unrelated, still-populated path).
    for base in deleted_paths {
        let mut cur = Path::new(base);
        while let Some(parent) = cur.parent() {
            let parent_str = parent.to_string_lossy();
            if parent_str.is_empty() {
                break;
            }
            let abs_parent = opts.work_dir.join(parent);
            if !crate::fsguard::remove_dir_if_empty(&opts.work_dir, &abs_parent)? {
                break;
            }
            cur = parent;
        }
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
    use crate::codec;
    use crate::store::local::LocalStore;
    use std::fs;
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
        assert_eq!(
            CONFLICT_ARTIFACT_SUFFIXES.len(),
            CONFLICT_SUFFIXES.len() + 1
        );
    }

    fn meta_with_sides(base: bool, local: bool, remote: bool) -> ConflictMeta {
        ConflictMeta {
            sides: Some(Sides {
                base,
                local,
                remote,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn resolve_side_uses_recorded_sides_when_consistent() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.omemfs-conflict-remote"), b"r").unwrap();
        let meta = meta_with_sides(false, true, true);
        assert_eq!(
            resolve_side(dir.path(), "f", &AcceptSide::Remote, Some(&meta)).unwrap(),
            Resolution::Helper(dir.path().join("f.omemfs-conflict-remote"))
        );
        // Recorded absent and no file: a genuine missing side.
        assert_eq!(
            resolve_side(dir.path(), "f", &AcceptSide::Base, Some(&meta)).unwrap(),
            Resolution::Delete
        );
    }

    #[test]
    fn resolve_side_errors_when_a_recorded_helper_is_missing() {
        let dir = TempDir::new().unwrap();
        let meta = meta_with_sides(true, true, true);
        let err = resolve_side(dir.path(), "f", &AcceptSide::Remote, Some(&meta)).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("inconsistent") && msg.contains("remote helper missing"),
            "{msg}"
        );
    }

    #[test]
    fn resolve_side_errors_when_an_unrecorded_helper_exists() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.omemfs-conflict-remote"), b"stray").unwrap();
        let meta = meta_with_sides(true, true, false);
        let err = resolve_side(dir.path(), "f", &AcceptSide::Remote, Some(&meta)).unwrap_err();
        assert!(err.to_string().contains("remote helper unexpected"));
    }

    #[test]
    fn resolve_side_falls_back_to_helper_presence_without_recorded_sides() {
        let dir = TempDir::new().unwrap();
        // No sidecar at all.
        assert_eq!(
            resolve_side(dir.path(), "f", &AcceptSide::Local, None).unwrap(),
            Resolution::Delete
        );
        std::fs::write(dir.path().join("f.omemfs-conflict-local"), b"l").unwrap();
        assert_eq!(
            resolve_side(dir.path(), "f", &AcceptSide::Local, None).unwrap(),
            Resolution::Helper(dir.path().join("f.omemfs-conflict-local"))
        );
        // A sidecar that predates `sides`.
        let old = ConflictMeta::default();
        assert!(matches!(
            resolve_side(dir.path(), "f", &AcceptSide::Local, Some(&old)).unwrap(),
            Resolution::Helper(_)
        ));
    }

    #[test]
    fn resolve_side_remote_symlink_has_no_helper() {
        let dir = TempDir::new().unwrap();
        let meta = ConflictMeta {
            sides: Some(Sides {
                base: true,
                local: true,
                remote: false,
            }),
            remote_resolution: Some(RemoteResolution::Symlink {
                target: "t".to_string(),
                mtime: None,
            }),
            ..Default::default()
        };
        assert_eq!(
            resolve_side(dir.path(), "f", &AcceptSide::Remote, Some(&meta)).unwrap(),
            Resolution::RemoteSymlink
        );
        // Only the remote side may take the symlink branch.
        assert_eq!(
            resolve_side(dir.path(), "f2", &AcceptSide::Base, Some(&meta))
                .unwrap_err()
                .to_string()
                .contains("inconsistent"),
            true
        );
    }

    #[test]
    fn resolve_all_reports_every_inconsistent_path_and_no_partial_result() {
        let dir = TempDir::new().unwrap();
        let metas = vec![
            ("a".to_string(), Some(meta_with_sides(true, true, true))),
            ("b".to_string(), Some(meta_with_sides(true, true, true))),
        ];
        let msg = resolve_all(dir.path(), &AcceptSide::Remote, &metas)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("'a'") && msg.contains("'b'"), "{msg}");
    }

    #[test]
    fn write_conflict_artifacts_records_sides_and_clears_stale_helpers() {
        let dir = TempDir::new().unwrap();
        let store_dir = TempDir::new().unwrap();
        let store = LocalStore::for_cache(store_dir.path());
        let base = dir.path().join("f.txt");
        std::fs::write(&base, b"local").unwrap();
        // Residue of an earlier conflict.
        std::fs::write(dir.path().join("f.txt.omemfs-conflict-remote"), b"old").unwrap();

        let file = std::fs::File::open(&base).unwrap();
        write_conflict_artifacts(
            dir.path(),
            &base,
            None,
            Some(&HelperSource::WorkingFile(file)),
            None,
            ConflictMeta::default(),
            &store,
        )
        .unwrap();

        assert!(!dir.path().join("f.txt.omemfs-conflict-remote").exists());
        assert_eq!(
            std::fs::read(dir.path().join("f.txt.omemfs-conflict-local")).unwrap(),
            b"local"
        );
        let meta = ConflictMeta::read(&base).unwrap();
        let sides = meta.sides.unwrap();
        assert!(!sides.base && sides.local && !sides.remote);
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
            out_dir.path(),
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
            dir.path(),
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
            dir.path(),
            &src,
            ".omemfs-conflict-local",
            &HelperSource::WorkingFile(file),
            &store,
        )
        .unwrap();
        assert_eq!(fs::read(&helper).unwrap(), b"new content");
    }

    // `HelperSource::WorkingFile` must refuse to write a conflict-helper file
    // through a directory that has been replaced by a symlink pointing
    // outside `work_dir` -- the same guard the `Blob` arm already gets via
    // `codec::chunk::materialise_to_file` (design/03_sync_model.md, "Write
    // safety: the working-tree write boundary").
    #[test]
    #[cfg(unix)]
    fn write_helper_file_refuses_working_file_through_symlinked_parent() {
        let work_dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();

        // "sub" stands in for a directory the clone_root tree tracks, but on
        // disk it has been swapped for a symlink pointing outside the
        // working tree.
        let sub = work_dir.path().join("sub");
        std::os::unix::fs::symlink(outside.path(), &sub).unwrap();

        // The "local" file being conflict-helper-copied lives elsewhere; only
        // `base_path`'s parent -- i.e. `sub` -- matters for the guard.
        let src_dir = TempDir::new().unwrap();
        let src = src_dir.path().join("local.txt");
        fs::write(&src, b"local content").unwrap();
        let file = fs::File::open(&src).unwrap();

        let store_dir = TempDir::new().unwrap();
        let store = LocalStore::for_cache(store_dir.path());

        let base_path = sub.join("file.txt");
        let err = write_helper_file(
            work_dir.path(),
            &base_path,
            ".omemfs-conflict-local",
            &HelperSource::WorkingFile(file),
            &store,
        )
        .unwrap_err();
        assert!(
            matches!(err, Error::SymlinkEscape(_)),
            "expected SymlinkEscape, got {err:?}"
        );
        assert!(
            !outside
                .path()
                .join("file.txt.omemfs-conflict-local")
                .exists(),
            "conflict-helper content must never be written outside work_dir"
        );
    }
}
