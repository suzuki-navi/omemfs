use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use crate::codec;
use crate::codec::chunk::blob_fully_cached;
use crate::dtimer_l1;
use crate::error::Error;
use crate::io_stats;
use crate::object::{Hash, Tree, TreeEntry};
use crate::repo::Repo;
use crate::store::ObjectStore;
use crate::store::stats::IoRecord;
use crate::stub::{self, StubRecord, StubTargetType};
use crate::commands::transfer;
use crate::term::{ColorChoice, Styles, color_enabled, paint};
use crate::tree_ops;

pub struct RestoreOptions {
    pub work_dir: PathBuf,
    /// Directory the command was invoked from; relative paths resolve against it.
    pub current_dir: PathBuf,
    /// Paths to restore. Empty means the entire working tree.
    pub paths: Vec<PathBuf>,
    pub dry_run: bool,
    /// Remote to fetch missing objects from, if the plan finds any (default: "origin").
    pub remote_name: String,
    /// Never contact the remote at all, including to heal a missing
    /// clone_root tree object. A path that would otherwise need a fetch is
    /// left un-restored and reported under `unavailable:` instead.
    pub no_remote: bool,
}

/// `restore` runs in three phases -- see `design/04_cli_spec.md`, "omemfs
/// restore" Behaviour:
///
/// 1. Plan (`plan_restore` below): walk `clone_root` against the working
///    tree, purely locally, deciding every [`Action`] without writing
///    anything. Blobs needing content that is not [`blob_fully_cached`] are
///    collected into `Plan::pending`; clone_root tree objects the walk could
///    not read locally are collected into `Plan::unresolved_trees`.
/// 2. Fetch: skipped entirely -- no remote config read, no cloud client
///    built -- when both collections are empty. Otherwise, unresolved trees
///    are healed and the plan is re-run (looping until stable), then every
///    pending blob is fetched in one batched transfer.
/// 3. Apply: every planned action is applied. An action whose blob could not
///    be resolved in phase 2 is skipped and reported under `unavailable:`
///    instead, rather than failing with a bare "object not found: <hash>".
pub fn run(opts: RestoreOptions) -> Result<(), Error> {
    let started = std::time::Instant::now();
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
    let paths: Vec<PathBuf> = if opts.paths.is_empty() {
        vec![opts.work_dir.clone()]
    } else {
        opts.paths.clone()
    };

    // ------------------------------------------------------------------
    // Phase 1: Plan. Local-only; makes no write of any kind.
    // ------------------------------------------------------------------
    let phase1 = crate::progress::begin_phase("Plan restore");
    let mut plan = plan_restore(
        &clone_root,
        &paths,
        &repo.work_dir,
        &opts.current_dir,
        &local,
    )?;
    phase1.complete(format!("{} action(s)", plan.actions.len()));

    if opts.dry_run {
        // --dry-run stops here: zero remote I/O, zero local cache writes
        // (design/04_cli_spec.md, "omemfs restore" Options).
        let counts = report_plan(&plan, &local, &repo.work_dir)?;
        print_summary(&counts, true);
        return Ok(());
    }

    // ------------------------------------------------------------------
    // Phase 2: Fetch missing objects. Skipped entirely when nothing is
    // missing, which keeps a fully-cached restore exactly as offline-safe
    // as it was before this feature existed (no `repo.pack_reader()` call
    // at all in that case).
    // ------------------------------------------------------------------
    let io_record = Arc::new(IoRecord::default());
    let mut unavailable_hashes: HashSet<Hash> = HashSet::new();
    // The first non-recoverable fetch failure encountered anywhere in Phase
    // 2 (opening the remote, healing a tree object, or the batched blob
    // transfer). Kept (rather than discarded, as before) so its real cause
    // can be reported once, after the summary, instead of every genuine
    // failure -- auth/DEK mismatch, remote data corruption, a misconfigured
    // or mistyped `--remote`, a full local disk -- silently degrading into
    // an indistinguishable `unavailable:` line. `Error::is_recoverable_
    // fetch_miss` (`src/error.rs`) decides which failures are ordinary
    // "not fetched into this clone yet" cases that need no such warning.
    let mut fetch_warning: Option<Error> = None;
    if !plan.pending.is_empty() || !plan.unresolved_trees.is_empty() {
        let phase2 = crate::progress::begin_phase("Fetch objects");
        if opts.no_remote {
            unavailable_hashes.extend(plan.pending.iter().cloned());
            phase2.complete("skipped (--no-remote)");
        } else {
            match repo.pack_reader(&opts.remote_name, Some(&io_record)) {
                Ok((pack_reader, _remote, remote_key)) => {
                    let remote_key = remote_key.as_ref();

                    // Heal every clone_root tree object the plan could not
                    // read locally, then re-run the plan from scratch so the
                    // newly-healed subtree's own actions (and possibly its
                    // own further-unresolved trees) are picked up. Loop
                    // until no new hash remains to try. A hash that fails to
                    // heal is never retried again in this run.
                    //
                    // Each iteration heals every currently-unresolved hash
                    // concurrently rather than one at a time (design/02_
                    // storage_format.md, "Multi-root batching" -- the same
                    // principle the batched blob transfer below, and
                    // `expand`'s parallel walk, already apply): healing one
                    // tree object never discovers further hashes to heal in
                    // the same pass (new unresolved trees only surface after
                    // the `plan_restore` re-run), so a plain parallel map
                    // over `to_heal` is enough -- no shared queue/visited-set
                    // machinery is needed.
                    //
                    // M6: `transfer::parallel_map` (not `rayon`'s
                    // `par_iter().map(...).collect()`, which held every
                    // healed object's full `Vec<u8>` payload resident for the
                    // whole batch) runs on `workers` threads honouring the
                    // same `OMEMFS_TRANSFER_CONCURRENCY` knob every other
                    // network fan-out in this codebase respects, and the
                    // fetched bytes are discarded immediately (`.map(|_|
                    // ())`) -- a heal's only lasting effect is populating the
                    // local cache, which the next `plan_restore` re-run below
                    // reads back from. Peak memory here is therefore `workers
                    // * one object`, not `to_heal.len() * one object`.
                    //
                    // `MAX_HEAL_ITERATIONS` is a safety net, not an expected
                    // limit: real clone_root hierarchies are nowhere near
                    // this deep, so hitting it only happens in a bug
                    // scenario. When it does, whatever is left over is
                    // reported under `unavailable:` in Phase 3 exactly like
                    // any other permanently-unresolved hash, rather than the
                    // command hanging or erroring out.
                    const MAX_HEAL_ITERATIONS: usize = 10_000;
                    let heal_workers = transfer::resolve_concurrency(&pack_reader)
                        .max(transfer::resolve_concurrency(&local));
                    let mut permanently_unresolved: HashSet<Hash> = HashSet::new();
                    let mut iterations = 0usize;
                    loop {
                        let to_heal: Vec<Hash> = plan
                            .unresolved_trees
                            .iter()
                            .filter(|(h, _)| !permanently_unresolved.contains(h))
                            .map(|(h, _)| h.clone())
                            .collect();
                        if to_heal.is_empty() {
                            break;
                        }
                        iterations += 1;
                        if iterations > MAX_HEAL_ITERATIONS {
                            permanently_unresolved.extend(to_heal);
                            break;
                        }
                        let results: Vec<(Hash, Result<(), Error>)> = transfer::parallel_map(
                            &to_heal,
                            heal_workers,
                            &|hash| {
                                (
                                    hash.clone(),
                                    codec::ensure_local_then_read(
                                        &pack_reader,
                                        &local,
                                        hash,
                                        remote_key,
                                    )
                                    .map(|_bytes| ()),
                                )
                            },
                        );
                        let mut healed_any = false;
                        for (hash, result) in results {
                            match result {
                                Ok(_) => healed_any = true,
                                Err(e) => {
                                    if fetch_warning.is_none() && !e.is_recoverable_fetch_miss() {
                                        fetch_warning = Some(e);
                                    }
                                    permanently_unresolved.insert(hash);
                                }
                            }
                        }
                        if !healed_any {
                            break;
                        }
                        plan = plan_restore(
                            &clone_root,
                            &paths,
                            &repo.work_dir,
                            &opts.current_dir,
                            &local,
                        )?;
                    }
                    // Whatever remains in `plan.unresolved_trees` at this
                    // point (only entries `permanently_unresolved` failed to
                    // heal, since a successful heal drops out of the next
                    // `plan_restore` re-run above) is reported by path in
                    // Phase 3 directly from `plan.unresolved_trees` -- no
                    // separate hash-keyed bookkeeping needed here.

                    // One batched, concurrent, SnapshotOnly transfer for
                    // every blob still missing after healing (never
                    // LiveFallback -- see design/02_storage_format.md,
                    // "Snapshot read path"). A failure here (e.g. the
                    // remote becomes unreachable partway through) is not
                    // fatal to the whole command: whatever could not be
                    // transferred is simply still missing afterward, and is
                    // reported per-path in phase 3 rather than aborting. Its
                    // cause, when not an ordinary cache miss, is still
                    // captured for the end-of-run warning.
                    let pending_before = plan.pending.len();
                    if let Err(e) = crate::commands::push::transfer_objects_many(
                        &pack_reader,
                        &local,
                        &plan.pending,
                        remote_key,
                        true,
                    ) && fetch_warning.is_none()
                        && !e.is_recoverable_fetch_miss()
                    {
                        fetch_warning = Some(e);
                    }
                    let mut still_missing = 0usize;
                    for hash in &plan.pending {
                        if !blob_fully_cached(&local, hash)? {
                            unavailable_hashes.insert(hash.clone());
                            still_missing += 1;
                        }
                    }
                    phase2.complete(format!(
                        "{} object(s) fetched",
                        pending_before.saturating_sub(still_missing)
                    ));
                }
                Err(e) => {
                    // Remote not configured, or otherwise could not be
                    // opened at all: degrade exactly like --no-remote
                    // rather than aborting the whole command. `plan` is left
                    // untouched, so its `unresolved_trees` (Phase 1's) is
                    // exactly what Phase 3 reports. The real cause (e.g. a
                    // misconfigured or mistyped remote name) is still
                    // captured for the end-of-run warning, unless it is an
                    // ordinary "remote unreachable" case.
                    if !e.is_recoverable_fetch_miss() {
                        fetch_warning = Some(e);
                    }
                    unavailable_hashes.extend(plan.pending.iter().cloned());
                    phase2.complete("remote unavailable");
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Phase 3: Apply.
    // ------------------------------------------------------------------
    let phase3 = crate::progress::begin_phase("Restore working tree");
    let mut counts = Counts::default();
    let mut unavailable_paths: Vec<String> = Vec::new();
    let mut failed_paths: Vec<String> = Vec::new();
    for action in plan.actions {
        // L5: capture a display path for this action *before* `apply_action`
        // takes ownership of it, so a `failed:` line can still name the path
        // even though `action` itself may have been partially consumed by
        // the attempt.
        let path = action_display_path(&action);
        if let Err(e) = apply_action(
            action,
            &repo.work_dir,
            &repo.work_dir,
            &local,
            &unavailable_hashes,
            &mut counts,
            &mut unavailable_paths,
        ) {
            // L5: continue-on-error, uniformly for every non-panic error --
            // no special-casing by error kind. A single bad path (a
            // permission error, a stale stub record whose path turned out to
            // be a non-empty directory -- L7 -- or any other genuine I/O
            // failure) must not discard every other, unrelated action
            // already queued in this same Apply loop. Note this can leave
            // *this one* path partially applied (e.g. a file half-written);
            // that was already a pre-existing risk for any single action
            // failing, on its own, before this change -- the change here
            // only stops it from being contagious to unrelated paths.
            print_failed(&path, &e);
            counts.failed += 1;
            failed_paths.push(path);
        }
    }
    // A directory whose clone_root tree object could never be read (locally
    // or, after phase 2, from the remote) has no action at all in `plan` --
    // there was nothing to enumerate underneath it. Report it by path here,
    // directly from the plan's final `unresolved_trees`, rather than
    // silently skipping it.
    for (_, rel) in &plan.unresolved_trees {
        print_action(false, "unavailable", rel);
        counts.unavailable += 1;
        unavailable_paths.push(rel.clone());
    }
    phase3.complete(format!("{} file(s)", counts.restored + counts.deleted));
    print_summary(&counts, false);

    // Problem 3 (design/04_cli_spec.md, "omemfs restore" -> "Missing
    // objects"): a fetch failure that is not an ordinary "not yet fetched"
    // cache miss gets one explicit line naming its real cause, printed after
    // the summary rather than only ever showing up as an unexplained
    // `unavailable:` path.
    if let Some(e) = &fetch_warning {
        eprintln!("warning: fetch from '{}' failed: {}", opts.remote_name, e);
    }

    let duration_ms = started.elapsed().as_millis() as u64;
    io_stats::append_record(
        &repo.work_dir.join(".omemfs"),
        "restore",
        &opts.remote_name,
        &io_record,
        duration_ms,
    );

    // L5: the exit code and message account for both kinds of unrestored
    // path -- an unresolved object (`unavailable:`) and a genuine per-action
    // I/O failure (`failed:`) -- since both leave the working tree short of
    // matching `clone_root` at that path.
    if !unavailable_paths.is_empty() || !failed_paths.is_empty() {
        return Err(Error::Other(format!(
            "{} path(s) could not be restored",
            unavailable_paths.len() + failed_paths.len()
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Plan / Action
// ---------------------------------------------------------------------------

/// One elementary decision made by the plan phase, reusing exactly the same
/// predicates the pre-3-phase implementation applied inline
/// (`needs_restore_blob`, `crate::fsmeta::mode_matches`, the stub-reconcile
/// branch conditions, `conflict_helpers_exist`). Building a `Plan` writes
/// nothing to the working tree, `.omemfs/objects/`, or any stub file.
enum Action {
    /// Overwrite `rel` with the blob at `hash` (content differs).
    RestoreBlobContent {
        rel: String,
        hash: Hash,
        mtime: Option<chrono::DateTime<chrono::Utc>>,
        mode: Option<String>,
    },
    /// Content at `rel` already matches; only the executable-bit mode needs
    /// fixing (chmod, no content rewrite -- no fetch needed either).
    RestoreBlobModeOnly {
        rel: String,
        mode: Option<String>,
    },
    /// Content and mode both already match; nothing visible to restore, but
    /// stray conflict helper files (if any) are still cleaned up silently.
    CleanupOnly { rel: String },
    RestoreSymlink {
        rel: String,
        target: String,
        mtime: Option<chrono::DateTime<chrono::Utc>>,
    },
    /// `rel` (an explicitly-named path) is absent from `clone_root`: delete
    /// whichever of a file/file-stub and a directory-stub is present, and
    /// clean up conflict helpers unconditionally either way.
    DeleteAbsent {
        rel: String,
        delete_file: bool,
        delete_dir_stub: bool,
    },
    /// A locally-added file or symlink with no entry in `clone_root` at all
    /// (found while scanning a directory for extras): delete it.
    DeleteFile { rel: String },
    /// A locally-added directory with no entry in `clone_root` at all:
    /// delete it and everything under it, recursively, at apply/report time.
    DeleteAddedDir { rel: String },
    /// Ensure an (empty) directory exists, restoring an empty tree entry.
    EnsureDir { rel: String },
    /// A stray file stub whose logical entry is no longer in `clone_root`,
    /// found while scanning a directory for extras.
    DeleteOrphanFileStub {
        logical_rel: String,
        stub_abs: PathBuf,
    },
    /// L6: a conflict helper (`<path>.omemfs-conflict-{base,local,remote}`)
    /// or the `<path>.omemfs-conflict-meta` metadata sidecar, found while
    /// scanning a directory for extras (i.e. its own base path has no
    /// restore/delete action of its own to piggy-back this cleanup onto --
    /// see `remove_conflict_helpers`/`conflict_helpers_exist` for the other
    /// case, where it does). Removed silently: this is omemfs-internal
    /// bookkeeping, never user content, so unlike `DeleteFile` it is neither
    /// printed as `deleted:` nor counted in the summary -- the same
    /// treatment an orphaned stub file already gets.
    DeleteConflictArtifact { rel: String },
    ReconcileFileStubToBlob {
        rel: String,
        hash: Hash,
        size: u64,
        mtime: Option<chrono::DateTime<chrono::Utc>>,
        mode: Option<String>,
    },
    ReconcileDirStubToTree {
        rel: String,
        hash: Hash,
        size: u64,
        mtime: Option<chrono::DateTime<chrono::Utc>>,
        blob_count: u64,
    },
    ReconcileDirStubToBlob {
        rel: String,
        hash: Hash,
        size: u64,
        mtime: Option<chrono::DateTime<chrono::Utc>>,
        mode: Option<String>,
    },
    ReconcileFileStubToTree {
        rel: String,
        hash: Hash,
        size: u64,
        mtime: Option<chrono::DateTime<chrono::Utc>>,
        blob_count: u64,
    },
}

/// Result of the plan phase: every action `restore` intends to take, plus
/// everything it would need from the remote to carry all of them out. See
/// `04_cli_spec.md`, "omemfs restore" -> "Missing objects".
#[derive(Default)]
struct Plan {
    actions: Vec<Action>,
    /// Blob hashes named by a `RestoreBlobContent` action that are not (yet)
    /// [`blob_fully_cached`] in the local cache.
    pending: Vec<Hash>,
    /// `clone_root` tree object hashes the plan walk needed to read but which
    /// were absent from the local cache, paired with the working-tree path
    /// whose subtree that object roots (kept so an unhealed entry can still
    /// be reported under `unavailable:` by path -- see `run`'s Phase 3).
    /// Healing one (fetching it from the remote into the local cache) and
    /// re-running the plan may reveal further actions -- and further entries
    /// here -- for the subtree it roots.
    unresolved_trees: Vec<(Hash, String)>,
}

/// Build the full [`Plan`] for `paths` (or the whole working tree when
/// `paths` is exactly `[work_dir]`, matching `run`'s no-argument default).
/// Purely local and side-effect-free: safe to call repeatedly (`run`'s Phase
/// 2 heal loop does) and safe to call for `--dry-run`.
fn plan_restore(
    clone_root: &Hash,
    paths: &[PathBuf],
    work_dir: &std::path::Path,
    current_dir: &std::path::Path,
    local: &dyn ObjectStore,
) -> Result<Plan, Error> {
    let mut plan = Plan::default();
    let rels: Vec<String> = paths
        .iter()
        .map(|path| crate::repo::normalize_path(path, work_dir, current_dir))
        .collect::<Result<Vec<_>, _>>()?;
    // Problem 1 (design/04_cli_spec.md, "omemfs restore" Arguments):
    // deduplicate and collapse ancestor/descendant relationships among the
    // requested paths BEFORE planning anything. Without this, `restore sub
    // sub` (an exact duplicate) or `restore . sub` (an overlap) planned --
    // and therefore deleted -- the same locally-added file once per matching
    // argument; the second `fs::remove_file` for it hit a bare ENOENT and
    // aborted Apply entirely.
    let rels = collapse_dedup_paths(rels);
    for rel in &rels {
        if rel.is_empty() {
            plan_tree(clone_root, work_dir, work_dir, local, work_dir, &mut plan)?;
            plan_remove_added_files(clone_root, work_dir, work_dir, local, work_dir, &mut plan)?;
        } else {
            plan_path(rel, clone_root, work_dir, local, work_dir, &mut plan)?;
        }
    }
    // plan_tree and plan_remove_added_files independently read the same
    // clone_root tree object for the same directory, so a directory whose
    // tree object is unreadable is recorded twice (once by each walk).
    // Deduplicate by path so Phase 3 never reports the same unavailable
    // directory more than once.
    let mut seen_rels: HashSet<String> = HashSet::new();
    plan.unresolved_trees
        .retain(|(_, rel)| seen_rels.insert(rel.clone()));
    // L4: several paths can reference identical content (the same hash), so
    // `plan_restore_blob_action` (below) can push the same hash onto
    // `plan.pending` once per matching path. `transfer_objects_many` already
    // dedups internally for the fetch itself, but the phase-2 "N object(s)
    // fetched" count is computed from `plan.pending.len()` before the fetch,
    // so an undeduped `pending` overcounts. Dedup by hash the same way
    // `unresolved_trees` above is deduped by path.
    let mut seen_hashes: HashSet<Hash> = HashSet::new();
    plan.pending.retain(|h| seen_hashes.insert(h.clone()));
    // Insurance against any other source of duplicate actions (e.g. a future
    // planning path that does not go through `collapse_dedup_paths` above):
    // an exact duplicate -- same path AND same action kind -- is dropped
    // rather than applied twice. This is a safety net, not the primary fix;
    // the primary fix is the path-level dedup above plus the NotFound-
    // tolerant deletes in `apply_action`.
    dedup_actions(&mut plan.actions);
    Ok(plan)
}

/// Deduplicate `rels` and collapse any path that is an ancestor of (or
/// identical to) another into a single occurrence of the ancestor. An empty
/// string (the whole working tree) is the ultimate ancestor of every other
/// path, so its presence anywhere in `rels` collapses the whole list to just
/// `[""]`.
fn collapse_dedup_paths(rels: Vec<String>) -> Vec<String> {
    if rels.iter().any(|r| r.is_empty()) {
        return vec![String::new()];
    }
    let mut kept: Vec<String> = Vec::new();
    'outer: for rel in rels {
        for existing in &kept {
            if is_ancestor_or_equal(existing, &rel) {
                continue 'outer;
            }
        }
        // `rel` is not covered by anything already kept; drop any kept path
        // that `rel` itself now covers (`rel` is its ancestor).
        kept.retain(|existing| !is_ancestor_or_equal(&rel, existing));
        kept.push(rel);
    }
    kept
}

/// Returns `true` if `ancestor` is `other` itself, or a path component
/// prefix of it (`"sub"` is an ancestor of `"sub/nested.txt"` but not of
/// `"subdir"`).
fn is_ancestor_or_equal(ancestor: &str, other: &str) -> bool {
    ancestor == other || other.starts_with(&format!("{}/", ancestor))
}

/// A stable identity for an `Action`, used only to detect and drop exact
/// duplicates (same path, same kind) in `dedup_actions`. Not a full
/// structural comparison -- two actions with the same identity but different
/// payloads (e.g. a different `mode`) are still treated as duplicates, since
/// by construction only one planning call ever produces an action for a
/// given path (`collapse_dedup_paths` already prevents that call from
/// running twice for equivalent input paths).
fn action_identity(action: &Action) -> (&'static str, String) {
    match action {
        Action::RestoreBlobContent { rel, .. } => ("RestoreBlobContent", rel.clone()),
        Action::RestoreBlobModeOnly { rel, .. } => ("RestoreBlobModeOnly", rel.clone()),
        Action::CleanupOnly { rel } => ("CleanupOnly", rel.clone()),
        Action::RestoreSymlink { rel, .. } => ("RestoreSymlink", rel.clone()),
        Action::DeleteAbsent { rel, .. } => ("DeleteAbsent", rel.clone()),
        Action::DeleteFile { rel } => ("DeleteFile", rel.clone()),
        Action::DeleteAddedDir { rel } => ("DeleteAddedDir", rel.clone()),
        Action::EnsureDir { rel } => ("EnsureDir", rel.clone()),
        Action::DeleteOrphanFileStub {
            logical_rel,
            stub_abs,
        } => (
            "DeleteOrphanFileStub",
            format!("{}::{}", logical_rel, stub_abs.display()),
        ),
        Action::DeleteConflictArtifact { rel } => ("DeleteConflictArtifact", rel.clone()),
        Action::ReconcileFileStubToBlob { rel, .. } => ("ReconcileFileStubToBlob", rel.clone()),
        Action::ReconcileDirStubToTree { rel, .. } => ("ReconcileDirStubToTree", rel.clone()),
        Action::ReconcileDirStubToBlob { rel, .. } => ("ReconcileDirStubToBlob", rel.clone()),
        Action::ReconcileFileStubToTree { rel, .. } => ("ReconcileFileStubToTree", rel.clone()),
    }
}

/// Drop exact duplicate actions (same path, same kind) -- see
/// `action_identity`'s doc comment for what "duplicate" means here.
fn dedup_actions(actions: &mut Vec<Action>) {
    let mut seen: HashSet<(&'static str, String)> = HashSet::new();
    actions.retain(|a| seen.insert(action_identity(a)));
}

/// Read the tree object at `hash` from `store` (the local cache). On
/// `ObjectNotFound`, defer: record `(hash, rel)` in `plan.unresolved_trees`
/// and return `Ok(None)` rather than erroring, so the caller can skip the
/// subtree for now instead of aborting the whole plan walk. `rel` is the
/// working-tree path of the directory `hash` roots, kept so this subtree can
/// still be reported under `unavailable:` by path if it is never healed.
fn read_tree_entries(
    store: &dyn ObjectStore,
    hash: &Hash,
    rel: &str,
    plan: &mut Plan,
) -> Result<Option<Vec<TreeEntry>>, Error> {
    match codec::store_read(store, hash, None) {
        Ok(data) => {
            let Tree::Normal { entries } = Tree::deserialise(&data)?;
            Ok(Some(entries))
        }
        Err(Error::ObjectNotFound(_)) => {
            plan.unresolved_trees.push((hash.clone(), rel.to_string()));
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// Plan a single relative path (file or directory) from clone_root.
fn plan_path(
    rel: &str,
    clone_root: &Hash,
    work_dir: &std::path::Path,
    store: &dyn ObjectStore,
    stub_root: &std::path::Path,
    plan: &mut Plan,
) -> Result<(), Error> {
    let components: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
    if components.is_empty() {
        // Empty path means full restore — handled by caller.
        return Ok(());
    }

    let entry = match tree_ops::navigate_entry(clone_root, &components, store) {
        Ok(e) => e,
        Err(Error::ObjectNotFound(hex)) => {
            // An ancestor's own clone_root tree object could not be read
            // locally. Defer this path entirely; healing that tree object
            // and re-planning may resolve it.
            if let Ok(h) = Hash::from_hex(&hex) {
                plan.unresolved_trees.push((h, rel.to_string()));
            }
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    let abs = work_dir.join(rel);

    match entry {
        None => {
            // Path absent from clone_root: plan to delete file/stubs from
            // the working tree, and unconditionally clean up any conflict
            // helpers regardless of whether either delete applies.
            let delete_file = abs.is_file() || stub::exists(stub_root, rel);
            let delete_dir_stub = stub::dir_exists(stub_root, rel);
            plan.actions.push(Action::DeleteAbsent {
                rel: rel.to_string(),
                delete_file,
                delete_dir_stub,
            });
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
                plan_reconcile_file_stub_to_blob(stub_root, rel, &hash, size, &mtime, &mode, plan)?;
                return Ok(());
            }
            // Type mismatch: dir stub where clone_root has a blob.
            if stub::dir_exists(stub_root, rel) {
                plan_reconcile_dir_stub_to_blob(rel, &hash, size, &mtime, &mode, plan)?;
                return Ok(());
            }
            plan_restore_blob_action(&abs, rel, &hash, &mtime, &mode, store, stub_root, plan)?;
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
                // Blobs under a dir stub are not in the local cache
                // (design/08_stub_system.md), so recursing would cause
                // spurious ObjectNotFound errors.
                plan_reconcile_dir_stub_to_tree(
                    stub_root, rel, &tree_hash, size, &mtime, blob_count, plan,
                )?;
            } else if stub::exists(stub_root, rel) {
                // Type mismatch: file stub where clone_root has a tree.
                plan_reconcile_file_stub_to_tree(rel, &tree_hash, size, &mtime, blob_count, plan)?;
            } else {
                // Plan to restore all descendants.
                plan_tree(&tree_hash, &abs, work_dir, store, stub_root, plan)?;
                // Plan to delete locally-added files under this directory.
                plan_remove_added_files(&tree_hash, &abs, work_dir, store, stub_root, plan)?;
            }
        }
        Some(TreeEntry::Symlink { target, mtime, .. }) => {
            // Materialise the symlink to match clone_root (design/08: symlinks
            // are always materialised). Skip when the on-disk entry is already a
            // symlink with the same target.
            if symlink_matches(&abs, &target) {
                return Ok(());
            }
            plan.actions.push(Action::RestoreSymlink {
                rel: rel.to_string(),
                target,
                mtime,
            });
        }
    }
    Ok(())
}

/// Shared decision for a `clone_root` blob entry, used by both `plan_path`
/// and `plan_tree`'s per-entry loop. Reuses the same `needs_restore_blob` /
/// `mode_matches` / `conflict_helpers_exist` predicates the pre-3-phase
/// implementation used inline.
#[allow(clippy::too_many_arguments)]
fn plan_restore_blob_action(
    abs: &std::path::Path,
    rel: &str,
    hash: &Hash,
    mtime: &Option<chrono::DateTime<chrono::Utc>>,
    mode: &Option<String>,
    store: &dyn ObjectStore,
    stub_root: &std::path::Path,
    plan: &mut Plan,
) -> Result<(), Error> {
    // Check if already matching. The mode is only consulted when the
    // content matches (the file is then known to exist on disk).
    let content_matches = !needs_restore_blob(abs, stub_root, rel, hash);
    let already_matches = content_matches && crate::fsmeta::mode_matches(abs, mode);
    if already_matches && !conflict_helpers_exist(abs) {
        return Ok(());
    }
    if !content_matches {
        if !blob_fully_cached(store, hash)? {
            plan.pending.push(hash.clone());
        }
        plan.actions.push(Action::RestoreBlobContent {
            rel: rel.to_string(),
            hash: hash.clone(),
            mtime: *mtime,
            mode: mode.clone(),
        });
    } else if !already_matches {
        // Content matches but the executable bit differs: chmod only,
        // no content rewrite, no fetch needed.
        plan.actions.push(Action::RestoreBlobModeOnly {
            rel: rel.to_string(),
            mode: mode.clone(),
        });
    } else {
        // Content and mode both match; only stray conflict helpers remain.
        plan.actions.push(Action::CleanupOnly {
            rel: rel.to_string(),
        });
    }
    Ok(())
}

/// Plan restoring all blobs reachable from `tree_hash` into `base_dir`.
/// `work_dir` is the repository root (used for rel_path computation).
/// `stub_root` is passed to `stub::exists`/`stub::dir_exists` (equals `work_dir`).
fn plan_tree(
    tree_hash: &Hash,
    base_dir: &std::path::Path,
    work_dir: &std::path::Path,
    store: &dyn ObjectStore,
    stub_root: &std::path::Path,
    plan: &mut Plan,
) -> Result<(), Error> {
    let rel_base = rel_path(base_dir, work_dir);
    let entries = match read_tree_entries(store, tree_hash, &rel_base, plan)? {
        Some(e) => e,
        None => return Ok(()), // deferred: recorded in plan.unresolved_trees
    };
    // Ensure the directory itself exists (handles empty directories).
    plan.actions.push(Action::EnsureDir {
        rel: rel_base,
    });
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
                    plan_reconcile_file_stub_to_blob(
                        stub_root, &rel, &hash, size, &mtime, &mode, plan,
                    )?;
                    continue;
                }
                // Type mismatch: dir stub where clone_root has a blob.
                if stub::dir_exists(stub_root, &rel) {
                    plan_reconcile_dir_stub_to_blob(&rel, &hash, size, &mtime, &mode, plan)?;
                    continue;
                }
                plan_restore_blob_action(&abs, &rel, &hash, &mtime, &mode, store, stub_root, plan)?;
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
                    plan_reconcile_dir_stub_to_tree(
                        stub_root,
                        &rel,
                        &child_hash,
                        size,
                        &mtime,
                        blob_count,
                        plan,
                    )?;
                    continue;
                }
                // Type mismatch: file stub where clone_root has a tree.
                if stub::exists(stub_root, &rel) {
                    plan_reconcile_file_stub_to_tree(&rel, &child_hash, size, &mtime, blob_count, plan)?;
                    continue;
                }
                plan_tree(&child_hash, &child_dir, work_dir, store, stub_root, plan)?;
            }
            TreeEntry::Symlink { name, target, mtime } => {
                let abs = base_dir.join(&name);
                let rel = rel_path(&abs, work_dir);
                if symlink_matches(&abs, &target) {
                    continue;
                }
                plan.actions.push(Action::RestoreSymlink { rel, target, mtime });
            }
        }
    }
    Ok(())
}

/// Plan deletion of working-tree files under `base_dir` that are absent from
/// the tree at `tree_hash`.
fn plan_remove_added_files(
    tree_hash: &Hash,
    base_dir: &std::path::Path,
    work_dir: &std::path::Path,
    store: &dyn ObjectStore,
    stub_root: &std::path::Path,
    plan: &mut Plan,
) -> Result<(), Error> {
    let rel_base = rel_path(base_dir, work_dir);
    let entries = match read_tree_entries(store, tree_hash, &rel_base, plan)? {
        Some(e) => e,
        None => return Ok(()), // deferred: recorded in plan.unresolved_trees (may
                                // duplicate plan_tree's own entry for the same
                                // directory; deduplicated in plan_restore).
    };
    let known: HashSet<String> = entries.iter().map(|e| e.name().to_string()).collect();

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
        // Conflict helper files (`<path>.omemfs-conflict-{base,local,remote}`)
        // are never entries in `clone_root`, so -- like any other locally-
        // added file -- they fall straight through to the "not in known"
        // branch below and are planned for deletion here. This is the only
        // place that removes a conflict helper whose own base path has no
        // restore/delete action of its own (an unchanged directory, an
        // unchanged symlink, or a locally-added file that is itself being
        // deleted -- design/04_cli_spec.md, "omemfs restore" Plan bullet on
        // conflict helpers). A helper already removed as a side effect of
        // its base path's own Blob/Symlink action (`remove_conflict_
        // helpers`, run earlier in `plan.actions` order for the same
        // top-level call) is simply gone by the time this action applies --
        // tolerated by `apply_action`'s NotFound-tolerant delete, not
        // excluded here.
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
                plan.actions.push(Action::DeleteOrphanFileStub {
                    logical_rel,
                    stub_abs,
                });
            }
            continue;
        }
        // L6: a conflict helper or the metadata sidecar found here has no
        // restore/delete action of its own for its base path (see the
        // comment above) -- and, like a stub file, is omemfs-internal
        // residue rather than user content. Route it to the silent
        // `DeleteConflictArtifact` action instead of falling into the
        // generic "unknown file" `DeleteFile` branch below, which would
        // report it as `deleted:` and count it as if it were a real file the
        // user is losing.
        if crate::commands::conflict::is_conflict_artifact(&name) {
            let rel = rel_path(&base_dir.join(&name), work_dir);
            plan.actions.push(Action::DeleteConflictArtifact { rel });
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
                // ObjectNotFound. The dir stub's own entry is reconciled by
                // plan_tree; there are no locally-added files to remove inside
                // a fully-stubbed directory. See design/08_stub_system.md.
                if !stub::dir_exists(stub_root, &child_rel)
                    && let Some(child_entry) = entries.iter().find(|e| e.name() == name)
                    && let TreeEntry::Tree {
                        hash: child_hash, ..
                    } = child_entry
                {
                    plan_remove_added_files(child_hash, &abs, work_dir, store, stub_root, plan)?;
                }
            }
        } else {
            // Not in clone_root: plan to delete.
            let abs = base_dir.join(&name);
            let rel = rel_path(&abs, work_dir);
            if file_type.is_file() || file_type.is_symlink() {
                plan.actions.push(Action::DeleteFile { rel });
            } else if file_type.is_dir() {
                plan.actions.push(Action::DeleteAddedDir { rel });
            }
        }
    }
    Ok(())
}

fn plan_reconcile_file_stub_to_blob(
    stub_root: &std::path::Path,
    rel: &str,
    hash: &Hash,
    size: u64,
    mtime: &Option<chrono::DateTime<chrono::Utc>>,
    mode: &Option<String>,
    plan: &mut Plan,
) -> Result<(), Error> {
    let current = stub::read(stub_root, rel)?;
    let needs_update = current
        .is_none_or(|r| r.hash != *hash || r.size != size || r.mtime != *mtime || r.mode != *mode);
    if needs_update {
        plan.actions.push(Action::ReconcileFileStubToBlob {
            rel: rel.to_string(),
            hash: hash.clone(),
            size,
            mtime: *mtime,
            mode: mode.clone(),
        });
    }
    Ok(())
}

fn plan_reconcile_dir_stub_to_tree(
    stub_root: &std::path::Path,
    rel: &str,
    hash: &Hash,
    size: u64,
    mtime: &Option<chrono::DateTime<chrono::Utc>>,
    blob_count: u64,
    plan: &mut Plan,
) -> Result<(), Error> {
    let current = stub::read_dir_stub(stub_root, rel)?;
    let needs_update = current.is_none_or(|r| {
        r.hash != *hash || r.size != size || r.mtime != *mtime || r.blob_count != blob_count
    });
    if needs_update {
        plan.actions.push(Action::ReconcileDirStubToTree {
            rel: rel.to_string(),
            hash: hash.clone(),
            size,
            mtime: *mtime,
            blob_count,
        });
    }
    Ok(())
}

/// Type mismatch (dir stub / clone_root blob): unconditional, matching the
/// pre-3-phase implementation (no `needs_update` gating for a type change).
fn plan_reconcile_dir_stub_to_blob(
    rel: &str,
    hash: &Hash,
    size: u64,
    mtime: &Option<chrono::DateTime<chrono::Utc>>,
    mode: &Option<String>,
    plan: &mut Plan,
) -> Result<(), Error> {
    plan.actions.push(Action::ReconcileDirStubToBlob {
        rel: rel.to_string(),
        hash: hash.clone(),
        size,
        mtime: *mtime,
        mode: mode.clone(),
    });
    Ok(())
}

/// Type mismatch (file stub / clone_root tree): unconditional, matching the
/// pre-3-phase implementation.
fn plan_reconcile_file_stub_to_tree(
    rel: &str,
    hash: &Hash,
    size: u64,
    mtime: &Option<chrono::DateTime<chrono::Utc>>,
    blob_count: u64,
    plan: &mut Plan,
) -> Result<(), Error> {
    plan.actions.push(Action::ReconcileFileStubToTree {
        rel: rel.to_string(),
        hash: hash.clone(),
        size,
        mtime: *mtime,
        blob_count,
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// Report (--dry-run) and Apply (real run)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Counts {
    restored: usize,
    deleted: usize,
    unavailable: usize,
    /// L5: paths whose Apply raised a genuine I/O error (never a missing
    /// object -- that is `unavailable` above). Reported as `failed: <path>:
    /// <cause>` lines and included in the exit code / summary alongside
    /// `unavailable`.
    failed: usize,
}

/// Report a whole plan for `--dry-run`: prints and counts every action, but
/// performs no I/O beyond reading the local cache (to tell "would restore"
/// from "would fetch" for a `RestoreBlobContent` action).
fn report_plan(
    plan: &Plan,
    local: &dyn ObjectStore,
    work_dir: &std::path::Path,
) -> Result<Counts, Error> {
    let mut counts = Counts::default();
    for action in &plan.actions {
        report_action(action, work_dir, work_dir, local, &mut counts)?;
    }
    // A directory whose clone_root tree object is not locally cached: report
    // it the same way a pending blob is reported ("would fetch:"), since a
    // real restore's fetch phase would need to heal it before it can even
    // determine what -- if anything -- needs restoring underneath it.
    for (_, rel) in &plan.unresolved_trees {
        print_action(true, "fetch", rel);
        counts.restored += 1;
    }
    Ok(counts)
}

fn report_action(
    action: &Action,
    work_dir: &std::path::Path,
    stub_root: &std::path::Path,
    local: &dyn ObjectStore,
    counts: &mut Counts,
) -> Result<(), Error> {
    match action {
        Action::RestoreBlobContent { rel, hash, .. } => {
            if blob_fully_cached(local, hash)? {
                print_action(true, "restore", rel);
            } else {
                print_action(true, "fetch", rel);
            }
            counts.restored += 1;
        }
        Action::RestoreBlobModeOnly { rel, .. } | Action::RestoreSymlink { rel, .. } => {
            print_action(true, "restore", rel);
            counts.restored += 1;
        }
        Action::CleanupOnly { .. } | Action::EnsureDir { .. } => {}
        Action::DeleteAbsent {
            rel,
            delete_file,
            delete_dir_stub,
        } => {
            if *delete_file {
                print_action(true, "delete", rel);
                counts.deleted += 1;
            }
            if *delete_dir_stub {
                print_action(true, "delete", rel);
                counts.deleted += 1;
            }
        }
        Action::DeleteFile { rel } => {
            print_action(true, "delete", rel);
            counts.deleted += 1;
        }
        Action::DeleteAddedDir { rel } => {
            let abs = work_dir.join(rel);
            remove_added_dir(&abs, work_dir, stub_root, true, counts)?;
        }
        Action::DeleteOrphanFileStub { logical_rel, .. } => {
            print_action(true, "delete", logical_rel);
            counts.deleted += 1;
        }
        // L6: omemfs-internal residue, never user content -- no report, no
        // count, in `--dry-run` either.
        Action::DeleteConflictArtifact { .. } => {}
        Action::ReconcileFileStubToBlob { rel, .. }
        | Action::ReconcileDirStubToTree { rel, .. }
        | Action::ReconcileDirStubToBlob { rel, .. }
        | Action::ReconcileFileStubToTree { rel, .. } => {
            print_action(true, "restore", rel);
            counts.restored += 1;
        }
    }
    Ok(())
}

/// Apply a single planned action for real. `still_unavailable` names blob
/// hashes phase 2 could not resolve; a `RestoreBlobContent` action naming one
/// of them is skipped (the working tree is left as found) and reported under
/// `unavailable:` instead of being applied.
#[allow(clippy::too_many_arguments)]
fn apply_action(
    action: Action,
    work_dir: &std::path::Path,
    stub_root: &std::path::Path,
    local: &dyn ObjectStore,
    still_unavailable: &HashSet<Hash>,
    counts: &mut Counts,
    unavailable_paths: &mut Vec<String>,
) -> Result<(), Error> {
    match action {
        Action::RestoreBlobContent {
            rel,
            hash,
            mtime,
            mode,
        } => {
            if still_unavailable.contains(&hash) {
                print_action(false, "unavailable", &rel);
                counts.unavailable += 1;
                unavailable_paths.push(rel);
                return Ok(());
            }
            let abs = work_dir.join(&rel);
            write_blob(work_dir, &abs, &hash, &mtime, &mode, local, stub_root, &rel)?;
            print_action(false, "restore", &rel);
            counts.restored += 1;
            remove_conflict_helpers(&abs);
        }
        Action::RestoreBlobModeOnly { rel, mode } => {
            let abs = work_dir.join(&rel);
            // Problem 4 (design/04_cli_spec.md, "omemfs restore" Apply
            // bullet): Plan found the content already matching, so this
            // action carries no fetch and normally applies immediately --
            // but Phase 2's fetch for *other* pending blobs can still take
            // real wall-clock time in between, during which something
            // outside omemfs could have removed this path. Re-check right
            // before the chmod below, so the command does not otherwise
            // print "restored:" for a path that no longer exists.
            if !abs.exists() {
                print_action(false, "unavailable", &rel);
                counts.unavailable += 1;
                unavailable_paths.push(rel);
                return Ok(());
            }
            // N2 (design/03_sync_model.md, "Write safety: the working-tree
            // write boundary"): this is the "mode-only, no content write"
            // case -- there is no guarded write already in flight to piggy-
            // back the chmod onto, so the parent is resolved through
            // `fsguard` directly instead of a path-based `apply_mode` call,
            // which would otherwise follow a symlinked component.
            let (dir, leaf) = crate::fsguard::open_leaf(work_dir, &rel, false)?;
            dir.set_mode(&leaf, &mode)?;
            print_action(false, "restore", &rel);
            counts.restored += 1;
            remove_conflict_helpers(&abs);
        }
        Action::CleanupOnly { rel } => {
            let abs = work_dir.join(&rel);
            remove_conflict_helpers(&abs);
        }
        Action::RestoreSymlink { rel, target, mtime } => {
            let abs = work_dir.join(&rel);
            stub::remove(stub_root, &rel).ok();
            stub::remove_dir_stub(stub_root, &rel).ok();
            crate::fsmeta::write_symlink_atomic(work_dir, &abs, &target)?;
            crate::fsmeta::restore_symlink_mtime(&abs, &mtime);
            remove_conflict_helpers(&abs);
            print_action(false, "restore", &rel);
            counts.restored += 1;
        }
        Action::DeleteAbsent {
            rel,
            delete_file,
            delete_dir_stub,
        } => {
            let abs = work_dir.join(&rel);
            // Problem 4: `delete_file` was decided at Plan time from a
            // one-shot `abs.is_file() || stub::exists(...)` check. Re-check
            // at Apply time too -- Phase 2's fetch may have given enough
            // wall-clock time for something outside omemfs to create this
            // path after Plan found nothing here, and a path clone_root
            // does not have must still not survive `restore`.
            let delete_file = delete_file || abs.is_file() || stub::exists(stub_root, &rel);
            if delete_file {
                // L7: `crate::fsguard::remove_file_tolerant`'s `unlinkat`
                // only tolerates `NotFound` -- an `EISDIR` from it must
                // still propagate at its other call sites, where a
                // directory appearing unexpectedly is a real-bug signal, not
                // something to paper over. Here specifically, though, a
                // stale FILE-type stub record (or a plain file) whose path
                // is actually a directory on disk is an expected case: the
                // type may have changed between Plan and Apply, or the stub
                // record is simply stale. Check the on-disk type first
                // (`symlink_metadata`, never following a symlink there) so
                // this one path gets its own handling instead of hitting
                // `EISDIR` from the tolerant helper.
                match fs::symlink_metadata(&abs) {
                    Ok(md) if md.is_dir() => {
                        // Never turn a "delete one file" action into a
                        // recursive directory delete -- a Plan/Apply race
                        // making `abs` a non-empty directory would make
                        // `remove_dir_all` a real data-loss hazard. Drop the
                        // stale stub record, then require the directory to
                        // already be empty; a non-empty directory surfaces
                        // as a genuine error for this one path only (L5
                        // turns that into a `failed:` line rather than
                        // aborting the rest of Apply).
                        stub::remove(stub_root, &rel)?;
                        fs::remove_dir(&abs)?;
                        print_action(false, "delete", &rel);
                        counts.deleted += 1;
                    }
                    _ => {
                        // N1 (design/03_sync_model.md, "Write safety: the
                        // working-tree write boundary"): this delete is
                        // driven by a lexically-planned path, exactly like a
                        // naive write would be, so it must go through the
                        // same guarded boundary.
                        crate::fsguard::remove_file_tolerant(work_dir, &abs)?;
                        stub::remove(stub_root, &rel)?;
                        print_action(false, "delete", &rel);
                        counts.deleted += 1;
                    }
                }
            }
            if delete_dir_stub {
                stub::remove_dir_stub(stub_root, &rel)?;
                // Remove the directory if it is now empty.
                fs::remove_dir(&abs).ok();
                print_action(false, "delete", &rel);
                counts.deleted += 1;
            }
            remove_conflict_helpers(&abs);
        }
        Action::DeleteFile { rel } => {
            let abs = work_dir.join(&rel);
            // Problem 1 (design/04_cli_spec.md, "omemfs restore" Errors):
            // tolerate the target already being gone -- either because a
            // duplicate/overlapping path argument already planned (and
            // applied) the same delete once, because this file is a
            // conflict helper already removed as a side effect of its base
            // path's own action (problem 2), or because it raced with an
            // external delete between Plan and Apply. Mirrors `stub::
            // remove`'s NotFound-tolerant pattern (`src/stub.rs`). Routed
            // through fsguard (N1) rather than a bare path-based
            // `fs::remove_file` -- see `Action::DeleteAbsent` above.
            crate::fsguard::remove_file_tolerant(work_dir, &abs)?;
            stub::remove(stub_root, &rel)?;
            print_action(false, "delete", &rel);
            counts.deleted += 1;
        }
        Action::DeleteAddedDir { rel } => {
            let abs = work_dir.join(&rel);
            remove_added_dir(&abs, work_dir, stub_root, false, counts)?;
            // Remove the directory itself once contents are cleared.
            fs::remove_dir(&abs).ok();
        }
        Action::EnsureDir { rel } => {
            crate::fsguard::create_dir_all(work_dir, &work_dir.join(&rel))?;
        }
        Action::DeleteOrphanFileStub {
            logical_rel,
            stub_abs,
        } => {
            // NotFound-tolerant for the same reasons as `Action::DeleteFile`
            // above (problem 1). Routed through fsguard (N1).
            crate::fsguard::remove_file_tolerant(work_dir, &stub_abs)?;
            print_action(false, "delete", &logical_rel);
            counts.deleted += 1;
        }
        Action::DeleteConflictArtifact { rel } => {
            // L6: omemfs-internal residue, never user content -- removed the
            // same NotFound-tolerant way as any other stray file here, but
            // with no `deleted:` line and no count increment (matching how
            // an orphaned stub file inside a removed directory is already
            // handled in `remove_added_dir`, below).
            let abs = work_dir.join(&rel);
            crate::fsguard::remove_file_tolerant(work_dir, &abs)?;
        }
        Action::ReconcileFileStubToBlob {
            rel,
            hash,
            size,
            mtime,
            mode,
        } => {
            stub::write(
                stub_root,
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
            print_action(false, "restore", &rel);
            counts.restored += 1;
        }
        Action::ReconcileDirStubToTree {
            rel,
            hash,
            size,
            mtime,
            blob_count,
        } => {
            stub::write_dir_stub(
                stub_root,
                &rel,
                &StubRecord {
                    target_type: StubTargetType::Tree,
                    hash,
                    size,
                    mtime,
                    mode: None,
                    blob_count,
                },
            )?;
            print_action(false, "restore", &rel);
            counts.restored += 1;
        }
        Action::ReconcileDirStubToBlob {
            rel,
            hash,
            size,
            mtime,
            mode,
        } => {
            stub::remove_dir_stub(stub_root, &rel)?;
            let abs = work_dir.join(&rel);
            if abs.exists() {
                fs::remove_dir_all(&abs)?;
            }
            stub::write(
                stub_root,
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
            print_action(false, "restore", &rel);
            counts.restored += 1;
        }
        Action::ReconcileFileStubToTree {
            rel,
            hash,
            size,
            mtime,
            blob_count,
        } => {
            stub::remove(stub_root, &rel)?;
            let abs = work_dir.join(&rel);
            crate::fsguard::create_dir_all(work_dir, &abs)?;
            stub::write_dir_stub(
                stub_root,
                &rel,
                &StubRecord {
                    target_type: StubTargetType::Tree,
                    hash,
                    size,
                    mtime,
                    mode: None,
                    blob_count,
                },
            )?;
            print_action(false, "restore", &rel);
            counts.restored += 1;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared filesystem helpers (unchanged decision/mutation logic)
// ---------------------------------------------------------------------------

/// Recursively delete all files in an added directory. Shared by `report_action`
/// (dry_run == true: enumerate and print only) and `apply_action` (dry_run ==
/// false: actually delete).
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
                let _ = crate::fsguard::remove_file_tolerant(work_dir, &abs);
            }
            continue;
        }
        // L6: a conflict helper or the metadata sidecar inside a directory
        // that is itself being wholly removed is the same kind of
        // omemfs-internal residue, treated the same way -- silently, with no
        // `deleted:` line and no count increment.
        if crate::commands::conflict::is_conflict_artifact(&name) {
            if !dry_run {
                let _ = crate::fsguard::remove_file_tolerant(work_dir, &abs);
            }
            continue;
        }
        let rel = rel_path(&abs, work_dir);
        let file_type = entry.file_type().map_err(Error::Io)?;
        if file_type.is_file() || file_type.is_symlink() {
            if dry_run {
                print_action(true, "delete", &rel);
            } else {
                // NotFound-tolerant for the same reasons as
                // `Action::DeleteFile` (problem 1). Routed through fsguard
                // (N1) rather than a bare path-based `fs::remove_file`.
                crate::fsguard::remove_file_tolerant(work_dir, &abs)?;
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
/// Removes any stub for this path. `materialise_blob_at` (via `fsguard`)
/// resolves and creates `abs`'s parent directory itself, so there is no
/// separate `create_dir_all` step here (Problem: a bare `fs::create_dir_all`
/// used to run ahead of the guarded content write, creating directories
/// outside the working tree even when the write itself was correctly
/// refused -- design/03_sync_model.md, "Write safety: the working-tree write
/// boundary").
fn write_blob(
    work_dir: &std::path::Path,
    abs: &std::path::Path,
    hash: &Hash,
    mtime: &Option<chrono::DateTime<chrono::Utc>>,
    mode: &Option<String>,
    store: &dyn ObjectStore,
    stub_root: &std::path::Path,
    rel: &str,
) -> Result<(), Error> {
    crate::fsmeta::materialise_blob_at(work_dir, store, hash, abs, mtime, mode)?;
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
    expected_hash: &Hash,
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

fn print_summary(counts: &Counts, dry_run: bool) {
    let total = counts.restored + counts.deleted;
    if total == 0 && counts.unavailable == 0 && counts.failed == 0 {
        print_action(false, "plain", "Nothing to restore.");
        return;
    }
    if dry_run {
        print_action(
            false,
            "plain",
            &format!("{} path(s) would be restored.", total),
        );
    } else if counts.unavailable > 0 || counts.failed > 0 {
        // L5: append an `, N path(s) failed` clause alongside the existing
        // `unavailable` one, present only when non-zero, so a run with
        // exactly one of the two kinds of unrestored path reads naturally
        // (matching the pre-existing `unavailable`-only phrasing) while a
        // run with both still reports each count explicitly.
        let mut summary = format!("{} path(s) restored", total);
        if counts.unavailable > 0 {
            summary.push_str(&format!(", {} path(s) unavailable", counts.unavailable));
        }
        if counts.failed > 0 {
            summary.push_str(&format!(", {} path(s) failed", counts.failed));
        }
        summary.push('.');
        print_action(false, "plain", &summary);
    } else {
        print_action(false, "plain", &format!("{} path(s) restored.", total));
    }
}

/// L5: a best-effort display path for `action`, computed by borrowing it --
/// safe to call immediately before `apply_action` consumes `action` by
/// value, so a `failed:` line can still name the path an action that raised
/// an error was for. Reuses `action_identity`'s own per-variant path
/// extraction rather than duplicating it; the composite identity string
/// `action_identity` builds for `DeleteOrphanFileStub` is irrelevant here --
/// only its first (path) component is used, which for every variant is
/// already the right thing to show a user.
fn action_display_path(action: &Action) -> String {
    action_identity(action).1
}

/// Print a `failed: <path>: <cause>` line for an action whose Apply raised a
/// genuine error (L5, design/04_cli_spec.md "Errors"). A separate function
/// from `print_action` rather than a new `kind` there: `print_action`'s kinds
/// each have their own small, fixed vocabulary of prefixes, whereas this one
/// embeds a caller-supplied, unbounded error message.
fn print_failed(path: &str, err: &Error) {
    let path = display_path(path);
    let colored = color_enabled(ColorChoice::Auto, atty::is(atty::Stream::Stdout));
    let styles = Styles::new();
    let text = if colored {
        format!("{}: {}: {}", paint(colored, styles.deleted, "failed"), path, err)
    } else {
        format!("failed: {}: {}", path, err)
    };
    crate::progress::emit_output_line(&text);
}

/// M7: substitute `.` for an empty path, for display purposes only. The
/// repository root's working-tree path is genuinely `""` internally (e.g.
/// `plan_tree`'s `rel_base` for the root call, and `work_dir.join("")` is a
/// valid, correct path) -- that representation must not change, only how it
/// is rendered to the user in an action line.
fn display_path(path: &str) -> &str {
    if path.is_empty() { "." } else { path }
}

/// Print a single action line. `kind` selects a color and prefix:
/// "restore"/"delete" → the usual indented action lines (added/deleted
/// colors); "fetch" → an un-indented "would fetch:" line, used only by
/// `--dry-run` reporting (see "Missing objects", design/04_cli_spec.md);
/// "unavailable" → an un-indented "unavailable:" line, used only by a real
/// (non-dry-run) run that could not resolve an object; "plain" → unstyled.
///
/// Uses `progress::emit_output_line` so the line is interleaved correctly
/// with the TTY progress window.
///
/// For `"restore"`/`"delete"`, the "  <verb>: <path>" text is built exactly
/// once, here, from the verb and `path` directly. The previous version took
/// a pre-formatted message from the caller and re-parsed it by stripping a
/// hardcoded prefix to recover `path` for coloring -- the caller formats,
/// the callee un-formats -- so a one-character change to either prefix
/// string would silently break coloring (refactor-instructions.md F2).
///
/// M7: `path` is substituted through [`display_path`] first (for every kind
/// except `"plain"`, whose `path` argument is a whole free-form message, not
/// a working-tree path), so an action line for the repository root -- whose
/// `rel`/`rel_base` is genuinely `""` internally -- never prints as a bare
/// `unavailable: ` or `would fetch: ` with nothing after the colon.
fn print_action(dry_run: bool, kind: &str, path: &str) {
    let path = if kind == "plain" { path } else { display_path(path) };
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
        "fetch" => {
            if colored {
                format!("{}: {}", paint(colored, styles.modified, "would fetch"), path)
            } else {
                format!("would fetch: {}", path)
            }
        }
        "unavailable" => {
            if colored {
                format!("{}: {}", paint(colored, styles.deleted, "unavailable"), path)
            } else {
                format!("unavailable: {}", path)
            }
        }
        _ => path.to_string(),
    };
    crate::progress::emit_output_line(&text);
}

/// Remove the three conflict helper files, and the metadata sidecar, for
/// `base_path` if they exist.
///
/// L6: this used to iterate only `CONFLICT_SUFFIXES` (the three helpers),
/// which meant restoring a single conflicted path left its
/// `.omemfs-conflict-meta` sidecar behind -- it was only ever cleaned up
/// incidentally by a whole-tree restore's "unknown extra file" scan (see
/// `Action::DeleteConflictArtifact`), and even then it used to be
/// misreported as a `deleted:` line as if it were user content. Iterating
/// `CONFLICT_ARTIFACT_SUFFIXES` here fixes the single-path case directly.
fn remove_conflict_helpers(base_path: &std::path::Path) {
    for suffix in crate::commands::conflict::CONFLICT_ARTIFACT_SUFFIXES {
        let p = helper_path(base_path, suffix);
        if p.exists() {
            let _ = fs::remove_file(&p);
        }
    }
}

/// Returns true if any conflict helper file (or the metadata sidecar) exists
/// alongside `base_path`. See `remove_conflict_helpers` (L6).
fn conflict_helpers_exist(base_path: &std::path::Path) -> bool {
    crate::commands::conflict::CONFLICT_ARTIFACT_SUFFIXES
        .iter()
        .any(|suffix| helper_path(base_path, suffix).exists())
}

fn helper_path(base: &std::path::Path, suffix: &str) -> std::path::PathBuf {
    let s = base.to_string_lossy();
    std::path::PathBuf::from(format!("{}{}", s, suffix))
}

fn rel_path(abs: &std::path::Path, work_dir: &std::path::Path) -> String {
    abs.strip_prefix(work_dir)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| abs.to_string_lossy().into_owned())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::local::LocalStore;
    use tempfile::TempDir;

    /// Store `content` as a blob (in-memory path, matching `chunk.rs`'s own
    /// test helper) in `store` and return its logical hash.
    fn store_blob(store: &LocalStore, content: &[u8]) -> Hash {
        let hash = crate::object::blob_hash(content);
        let serialised = crate::object::serialise_blob(content);
        codec::chunk::store_chunked(store, &hash, &serialised, None).unwrap();
        hash
    }

    /// Build and store a `Tree::Normal` with the given entries; returns its hash.
    fn store_tree(store: &LocalStore, entries: Vec<TreeEntry>) -> Hash {
        let bytes = Tree::Normal { entries }.serialise();
        let hash = Hash::compute(&bytes);
        codec::store_write(store, &hash, &bytes, None).unwrap();
        hash
    }

    fn blob_entry(name: &str, hash: Hash, size: u64) -> TreeEntry {
        TreeEntry::Blob {
            name: name.to_string(),
            hash,
            size,
            mtime: None,
            mode: None,
        }
    }

    fn tree_entry(name: &str, hash: Hash, blob_count: u64) -> TreeEntry {
        TreeEntry::Tree {
            name: name.to_string(),
            hash,
            size: 0,
            mtime: None,
            blob_count,
        }
    }

    #[test]
    fn plan_pending_is_empty_when_blob_is_already_fully_cached() {
        // Content differs from the working tree (so a RestoreBlobContent
        // action IS created), but the object is already fully cached
        // locally -- the fetch phase must have nothing to do.
        let local_dir = TempDir::new().unwrap();
        let local = LocalStore::for_cache(local_dir.path());
        let content = b"clone-root content".to_vec();
        let hash = store_blob(&local, &content);
        let root = store_tree(&local, vec![blob_entry("file.txt", hash, content.len() as u64)]);

        let work_dir = TempDir::new().unwrap();
        fs::write(work_dir.path().join("file.txt"), b"different local content").unwrap();

        let plan = plan_restore(
            &root,
            &[work_dir.path().to_path_buf()],
            work_dir.path(),
            work_dir.path(),
            &local,
        )
        .unwrap();

        assert!(
            plan.pending.is_empty(),
            "object is already fully cached locally; pending must be empty"
        );
        assert!(
            plan
                .actions
                .iter()
                .any(|a| matches!(a, Action::RestoreBlobContent { .. })),
            "a restore action must still be planned for the differing content"
        );
    }

    #[test]
    fn plan_pending_has_one_entry_when_blob_is_missing() {
        // Same as above, but the blob object was never stored locally at
        // all -- the fetch phase must have exactly this one hash to fetch.
        let local_dir = TempDir::new().unwrap();
        let local = LocalStore::for_cache(local_dir.path());
        let content = b"clone-root content, never cached locally".to_vec();
        let hash = crate::object::blob_hash(&content);
        let root = store_tree(&local, vec![blob_entry("file.txt", hash.clone(), content.len() as u64)]);

        let work_dir = TempDir::new().unwrap();
        // File absent from the working tree entirely (e.g. a deleted stub).

        let plan = plan_restore(
            &root,
            &[work_dir.path().to_path_buf()],
            work_dir.path(),
            work_dir.path(),
            &local,
        )
        .unwrap();

        assert_eq!(plan.pending, vec![hash]);
        assert!(plan.unresolved_trees.is_empty());
    }

    #[test]
    fn plan_does_not_descend_into_a_dir_stub_and_leaves_pending_empty() {
        // A directory that is currently a dir stub, whose recorded blobs are
        // NOT cached locally at all. The plan must only reconcile the stub
        // record -- never plan to restore (or fetch) anything underneath it.
        let local_dir = TempDir::new().unwrap();
        let local = LocalStore::for_cache(local_dir.path());
        let uncached_content = b"never touched by the plan walk".to_vec();
        let uncached_hash = crate::object::blob_hash(&uncached_content);
        let sub_tree = store_tree(
            &local,
            vec![blob_entry(
                "big.bin",
                uncached_hash,
                uncached_content.len() as u64,
            )],
        );
        let root = store_tree(&local, vec![tree_entry("sub", sub_tree.clone(), 1)]);

        let work_dir = TempDir::new().unwrap();
        fs::create_dir_all(work_dir.path().join("sub")).unwrap();
        // A stale dir stub record (size deliberately wrong) so a reconcile
        // action is expected.
        stub::write_dir_stub(
            work_dir.path(),
            "sub",
            &StubRecord {
                target_type: StubTargetType::Tree,
                hash: sub_tree,
                size: 999,
                mtime: None,
                mode: None,
                blob_count: 1,
            },
        )
        .unwrap();

        let plan = plan_restore(
            &root,
            &[work_dir.path().to_path_buf()],
            work_dir.path(),
            work_dir.path(),
            &local,
        )
        .unwrap();

        assert!(
            plan.pending.is_empty(),
            "nothing under a dir stub must ever be planned for fetch"
        );
        // The only visible (non-`EnsureDir`) action must be the dir-stub
        // reconcile itself -- nothing underneath the stub is ever planned.
        // `EnsureDir` for the root directory itself is expected (emitted
        // unconditionally for every `plan_tree` call, including the root),
        // is silent in both report and apply, and carries no fetch risk.
        let visible: Vec<&Action> = plan
            .actions
            .iter()
            .filter(|a| !matches!(a, Action::EnsureDir { .. }))
            .collect();
        assert_eq!(visible.len(), 1, "only the dir-stub reconcile itself");
        assert!(matches!(visible[0], Action::ReconcileDirStubToTree { .. }));
    }

    #[test]
    fn plan_records_unresolved_tree_without_misdetecting_pending() {
        // The root tree references a child tree whose OWN object was never
        // written to the local store at all (simulating a clone_root tree
        // object missing from the local cache). The plan must record it in
        // `unresolved_trees` and must NOT throw, abort the whole walk, or
        // report any blob as pending (it never got far enough to see one).
        let local_dir = TempDir::new().unwrap();
        let local = LocalStore::for_cache(local_dir.path());

        // Build (but do not store) the child tree, so its hash is known but
        // unreadable from `local`.
        let content = b"lives inside the unreadable subtree".to_vec();
        let missing_blob_hash = crate::object::blob_hash(&content);
        let child_bytes = Tree::Normal {
            entries: vec![blob_entry(
                "deep.bin",
                missing_blob_hash,
                content.len() as u64,
            )],
        }
        .serialise();
        let child_hash = Hash::compute(&child_bytes);
        // Deliberately NOT written to `local`.

        let root = store_tree(&local, vec![tree_entry("sub", child_hash.clone(), 1)]);

        let work_dir = TempDir::new().unwrap();
        // "sub" does not exist in the working tree at all (the "D" state).

        let plan = plan_restore(
            &root,
            &[work_dir.path().to_path_buf()],
            work_dir.path(),
            work_dir.path(),
            &local,
        )
        .unwrap();

        assert_eq!(
            plan.unresolved_trees,
            vec![(child_hash, "sub".to_string())]
        );
        assert!(
            plan.pending.is_empty(),
            "a blob inside an unreadable subtree must never be reported as pending"
        );
    }

    // -----------------------------------------------------------------------
    // M7: root path rendered as "." in action lines
    // -----------------------------------------------------------------------

    #[test]
    fn display_path_substitutes_dot_for_the_empty_root_path_only() {
        assert_eq!(display_path(""), ".");
        assert_eq!(display_path("sub/file.txt"), "sub/file.txt");
        assert_eq!(display_path("."), ".");
    }

    // -----------------------------------------------------------------------
    // L4: pending fetch set deduplicated by hash
    // -----------------------------------------------------------------------

    #[test]
    fn plan_pending_has_one_entry_when_two_paths_share_identical_missing_content() {
        // Two distinct paths reference the exact same (never-cached) blob
        // content. The plan's pending-fetch set must contribute exactly one
        // entry for the shared hash, not one per path -- the phase-2 "N
        // object(s) fetched" summary counts distinct objects.
        let local_dir = TempDir::new().unwrap();
        let local = LocalStore::for_cache(local_dir.path());
        let content = b"identical content referenced by two paths".to_vec();
        let hash = crate::object::blob_hash(&content);
        let root = store_tree(
            &local,
            vec![
                blob_entry("a.txt", hash.clone(), content.len() as u64),
                blob_entry("b.txt", hash.clone(), content.len() as u64),
            ],
        );

        let work_dir = TempDir::new().unwrap();
        // Neither path exists in the working tree at all.

        let plan = plan_restore(
            &root,
            &[work_dir.path().to_path_buf()],
            work_dir.path(),
            work_dir.path(),
            &local,
        )
        .unwrap();

        assert_eq!(
            plan.pending,
            vec![hash],
            "the pending set must be deduplicated to one entry for the shared hash"
        );
        let restore_actions = plan
            .actions
            .iter()
            .filter(|a| matches!(a, Action::RestoreBlobContent { .. }))
            .count();
        assert_eq!(
            restore_actions, 2,
            "the plan's action list still has one RestoreBlobContent action per path"
        );
    }

    // -----------------------------------------------------------------------
    // Problem 1: path dedup / collapse
    // -----------------------------------------------------------------------

    #[test]
    fn collapse_dedup_paths_drops_exact_duplicates() {
        assert_eq!(
            collapse_dedup_paths(vec!["sub".to_string(), "sub".to_string()]),
            vec!["sub".to_string()]
        );
    }

    #[test]
    fn collapse_dedup_paths_collapses_descendant_into_ancestor() {
        let mut result = collapse_dedup_paths(vec!["a".to_string(), "a/b".to_string()]);
        result.sort();
        assert_eq!(result, vec!["a".to_string()]);

        let mut result = collapse_dedup_paths(vec!["a/b".to_string(), "a".to_string()]);
        result.sort();
        assert_eq!(result, vec!["a".to_string()], "order must not matter");
    }

    #[test]
    fn collapse_dedup_paths_empty_string_subsumes_everything() {
        // An empty rel means "the whole working tree" (`plan_restore`'s
        // caller passes `[work_dir]` for a no-argument restore, which
        // normalises to ""). It must collapse every other path, matching
        // `omemfs restore . sub` behaving exactly like `omemfs restore`.
        assert_eq!(
            collapse_dedup_paths(vec!["".to_string(), "sub".to_string()]),
            vec!["".to_string()]
        );
        assert_eq!(
            collapse_dedup_paths(vec!["sub".to_string(), "".to_string()]),
            vec!["".to_string()]
        );
    }

    #[test]
    fn collapse_dedup_paths_keeps_unrelated_siblings_and_similar_prefixes() {
        // "sub" and "subdir" share a string prefix but are NOT an
        // ancestor/descendant pair (no "/" boundary) -- both must survive.
        let mut result = collapse_dedup_paths(vec!["sub".to_string(), "subdir".to_string()]);
        result.sort();
        assert_eq!(result, vec!["sub".to_string(), "subdir".to_string()]);
    }

    #[test]
    fn plan_restore_duplicate_path_arguments_do_not_duplicate_actions() {
        // End-to-end reproduction of the reported regression at the
        // `plan_restore` level: `restore sub sub` (an exact duplicate
        // argument) must plan exactly one delete for a locally-added file
        // under `sub`, not two.
        let local_dir = TempDir::new().unwrap();
        let local = LocalStore::for_cache(local_dir.path());
        let root = store_tree(&local, vec![tree_entry("sub", store_tree(&local, vec![]), 0)]);

        let work_dir = TempDir::new().unwrap();
        fs::create_dir_all(work_dir.path().join("sub")).unwrap();
        fs::write(work_dir.path().join("sub/extra.txt"), b"added").unwrap();

        let sub_path = work_dir.path().join("sub");
        let plan = plan_restore(
            &root,
            &[sub_path.clone(), sub_path],
            work_dir.path(),
            work_dir.path(),
            &local,
        )
        .unwrap();

        let delete_count = plan
            .actions
            .iter()
            .filter(|a| matches!(a, Action::DeleteFile { rel } if rel == "sub/extra.txt"))
            .count();
        assert_eq!(
            delete_count, 1,
            "a duplicate path argument must not duplicate the delete action"
        );
    }

    // -----------------------------------------------------------------------
    // Problem 2: conflict helper cleanup when the base path has no action
    // -----------------------------------------------------------------------

    #[test]
    fn plan_remove_added_files_plans_deletion_of_conflict_helpers_for_a_directory_base_path() {
        // "sub" is a directory in clone_root; conflict helper files named
        // after it (`sub.omemfs-conflict-*`) are never seen by the
        // TreeEntry::Tree branch (it only recurses), so they must be picked
        // up here as unknown files in the parent scan. L6: because these are
        // omemfs-internal residue rather than user content, they get the
        // silent `DeleteConflictArtifact` action (removed, but never
        // reported as `deleted:` or counted), not the generic `DeleteFile`
        // action an arbitrary unknown file would get.
        let local_dir = TempDir::new().unwrap();
        let local = LocalStore::for_cache(local_dir.path());
        let sub_tree = store_tree(&local, vec![]);
        let root = store_tree(&local, vec![tree_entry("sub", sub_tree, 0)]);

        let work_dir = TempDir::new().unwrap();
        fs::create_dir_all(work_dir.path().join("sub")).unwrap();
        fs::write(
            work_dir.path().join("sub.omemfs-conflict-base"),
            b"base",
        )
        .unwrap();

        let plan = plan_restore(
            &root,
            &[work_dir.path().to_path_buf()],
            work_dir.path(),
            work_dir.path(),
            &local,
        )
        .unwrap();

        assert!(
            plan.actions.iter().any(
                |a| matches!(a, Action::DeleteConflictArtifact { rel } if rel == "sub.omemfs-conflict-base")
            ),
            "a conflict helper for a directory base path must still be planned for (silent) deletion"
        );
        assert!(
            !plan
                .actions
                .iter()
                .any(|a| matches!(a, Action::DeleteFile { rel } if rel == "sub.omemfs-conflict-base")),
            "a conflict artifact must never be planned as a reported DeleteFile"
        );
    }

    #[test]
    fn plan_remove_added_files_plans_silent_deletion_of_a_stray_meta_sidecar() {
        // A lone `.omemfs-conflict-meta` sidecar with no corresponding
        // helper files (e.g. left behind by an older build that never
        // cleaned it up on a single-path restore -- the exact L6 bug) must
        // also be planned as a silent `DeleteConflictArtifact`, not a
        // reported `DeleteFile`.
        let local_dir = TempDir::new().unwrap();
        let local = LocalStore::for_cache(local_dir.path());
        let content = b"hello".to_vec();
        let hash = store_blob(&local, &content);
        let root = store_tree(&local, vec![blob_entry("file.txt", hash, content.len() as u64)]);

        let work_dir = TempDir::new().unwrap();
        fs::write(work_dir.path().join("file.txt"), &content).unwrap();
        fs::write(
            work_dir.path().join("file.txt.omemfs-conflict-meta"),
            b"{}",
        )
        .unwrap();

        let plan = plan_restore(
            &root,
            &[work_dir.path().to_path_buf()],
            work_dir.path(),
            work_dir.path(),
            &local,
        )
        .unwrap();

        assert!(
            plan.actions.iter().any(
                |a| matches!(a, Action::DeleteConflictArtifact { rel } if rel == "file.txt.omemfs-conflict-meta")
            ),
            "a stray meta sidecar must be planned for silent removal"
        );
    }

    // -----------------------------------------------------------------------
    // Problem 4: apply-time re-verification against a Plan/Apply race
    // -----------------------------------------------------------------------

    #[test]
    fn apply_restore_blob_mode_only_reports_unavailable_if_file_vanished_before_apply() {
        // Simulates the race the audit describes: Plan decided only a mode
        // fix was needed (content already matched), but by the time Apply
        // runs this action the file is gone -- e.g. removed externally
        // during Phase 2's fetch for some other, unrelated pending blob.
        let work_dir = TempDir::new().unwrap();
        let abs = work_dir.path().join("script.sh");
        fs::write(&abs, b"content").unwrap();
        fs::remove_file(&abs).unwrap();

        let action = Action::RestoreBlobModeOnly {
            rel: "script.sh".to_string(),
            mode: Some("755".to_string()),
        };
        let local_dir = TempDir::new().unwrap();
        let local = LocalStore::for_cache(local_dir.path());
        let mut counts = Counts::default();
        let mut unavailable_paths = Vec::new();
        apply_action(
            action,
            work_dir.path(),
            work_dir.path(),
            &local,
            &HashSet::new(),
            &mut counts,
            &mut unavailable_paths,
        )
        .unwrap();

        assert_eq!(counts.unavailable, 1);
        assert_eq!(counts.restored, 0);
        assert_eq!(unavailable_paths, vec!["script.sh".to_string()]);
        assert!(!abs.exists());
    }

    #[test]
    fn apply_delete_absent_removes_a_file_that_appeared_after_planning() {
        // Plan-time found nothing at this path (`delete_file: false`), but
        // by Apply time something outside omemfs created it -- e.g. during
        // Phase 2's fetch for an unrelated pending blob. Apply must still
        // remove it rather than silently leaving a path clone_root does not
        // have.
        let work_dir = TempDir::new().unwrap();
        let abs = work_dir.path().join("late.txt");
        fs::write(&abs, b"raced in after planning").unwrap();

        let action = Action::DeleteAbsent {
            rel: "late.txt".to_string(),
            delete_file: false,
            delete_dir_stub: false,
        };
        let local_dir = TempDir::new().unwrap();
        let local = LocalStore::for_cache(local_dir.path());
        let mut counts = Counts::default();
        let mut unavailable_paths = Vec::new();
        apply_action(
            action,
            work_dir.path(),
            work_dir.path(),
            &local,
            &HashSet::new(),
            &mut counts,
            &mut unavailable_paths,
        )
        .unwrap();

        assert!(!abs.exists());
        assert_eq!(counts.deleted, 1);
    }

    // -----------------------------------------------------------------------
    // L7: DeleteAbsent hitting a directory where a file was expected
    // -----------------------------------------------------------------------

    #[test]
    fn apply_delete_absent_removes_an_empty_stale_directory() {
        // A stale FILE-type stub record (or plain-file expectation) whose
        // path is actually an EMPTY directory on disk: Apply must remove the
        // stale record and the (empty) directory, reporting it the same way
        // an ordinary successful delete is reported.
        let work_dir = TempDir::new().unwrap();
        let abs = work_dir.path().join("p");
        fs::create_dir(&abs).unwrap();

        let action = Action::DeleteAbsent {
            rel: "p".to_string(),
            delete_file: true,
            delete_dir_stub: false,
        };
        let local_dir = TempDir::new().unwrap();
        let local = LocalStore::for_cache(local_dir.path());
        let mut counts = Counts::default();
        let mut unavailable_paths = Vec::new();
        apply_action(
            action,
            work_dir.path(),
            work_dir.path(),
            &local,
            &HashSet::new(),
            &mut counts,
            &mut unavailable_paths,
        )
        .unwrap();

        assert!(!abs.exists(), "the now-empty stale directory must be removed");
        assert_eq!(counts.deleted, 1);
    }

    #[test]
    fn apply_delete_absent_fails_for_a_non_empty_stale_directory_without_recursing() {
        // Same stale-record scenario, but the directory is NOT empty. Apply
        // must never turn this into a recursive `remove_dir_all` -- it must
        // fail this one path (which, thanks to L5, the caller in `run()`
        // turns into a single `failed:` line rather than aborting the rest
        // of Apply) and leave the directory and its contents untouched.
        let work_dir = TempDir::new().unwrap();
        let abs = work_dir.path().join("p");
        fs::create_dir(&abs).unwrap();
        fs::write(abs.join("inner.txt"), b"still here").unwrap();

        let action = Action::DeleteAbsent {
            rel: "p".to_string(),
            delete_file: true,
            delete_dir_stub: false,
        };
        let local_dir = TempDir::new().unwrap();
        let local = LocalStore::for_cache(local_dir.path());
        let mut counts = Counts::default();
        let mut unavailable_paths = Vec::new();
        let result = apply_action(
            action,
            work_dir.path(),
            work_dir.path(),
            &local,
            &HashSet::new(),
            &mut counts,
            &mut unavailable_paths,
        );

        assert!(result.is_err(), "a non-empty stale directory must be a genuine error");
        assert!(abs.is_dir(), "the directory must be left untouched");
        assert!(
            abs.join("inner.txt").exists(),
            "the directory's contents must be left untouched"
        );
        assert_eq!(counts.deleted, 0);
    }

    // -----------------------------------------------------------------------
    // Problem 1: NotFound-tolerant deletes
    // -----------------------------------------------------------------------

    #[test]
    fn apply_delete_file_tolerates_the_target_already_being_gone() {
        // Simulates two planned actions racing to delete the same path
        // (duplicate/overlapping path arguments, or a conflict helper
        // already removed by its base path's own action): the second
        // Action::DeleteFile must not error just because the first one
        // already removed the file.
        let work_dir = TempDir::new().unwrap();
        // Deliberately never created: `rel` names a path that is already
        // absent by the time this action applies.
        let action = Action::DeleteFile {
            rel: "already-gone.txt".to_string(),
        };
        let local_dir = TempDir::new().unwrap();
        let local = LocalStore::for_cache(local_dir.path());
        let mut counts = Counts::default();
        let mut unavailable_paths = Vec::new();
        apply_action(
            action,
            work_dir.path(),
            work_dir.path(),
            &local,
            &HashSet::new(),
            &mut counts,
            &mut unavailable_paths,
        )
        .unwrap();

        assert_eq!(counts.deleted, 1);
        assert!(unavailable_paths.is_empty());
    }
}
