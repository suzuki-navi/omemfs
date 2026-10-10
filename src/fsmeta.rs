//! Filesystem metadata helpers shared by all working-tree materialisation
//! paths (pull, clone, expand, restore).

use chrono::{DateTime, TimeZone, Utc};
use filetime::FileTime;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::error::Error;
use crate::object::{Hash, TreeEntry};
use crate::store::ObjectStore;

/// Restore the mtime of a symlink itself (not the file it points at).
///
/// `filetime::set_file_mtime` follows symlinks on Linux and would touch the
/// target, so symlink materialisation must use `set_symlink_file_times`
/// (lutimes) instead. The atime is set to the same value as the mtime because
/// the object model does not track atime. A `None` mtime (e.g. an entry whose
/// mtime could not be read at scan time) is a no-op.
///
/// Best-effort: errors are ignored, consistent with how
/// `filetime::set_file_mtime(...).ok()` is treated at the regular-file call
/// sites. See design/01_object_model.md (symlink `mtime`) and
/// design/03_sync_model.md (apply step).
pub fn restore_symlink_mtime(path: &Path, mtime: &Option<DateTime<Utc>>) {
    let Some(mt) = mtime else { return };
    let ft = FileTime::from_unix_time(mt.timestamp(), mt.timestamp_subsec_nanos());
    filetime::set_symlink_file_times(path, ft, ft).ok();
}

/// Apply the executable-bit `mode` from a tree entry / stub record to a file.
///
/// `Some(_)` sets the execute bits (`perms | 0o111`); `None` clears them
/// (`perms & !0o111`). Read/write bits are preserved as-is (umask-derived),
/// so e.g. `0o600` becomes `0o711`, not a forced `0o755`.
///
/// Best-effort: errors are ignored, consistent with how
/// `filetime::set_file_mtime(...).ok()` is treated at all call sites.
/// `set_permissions` is only called when the bits actually change, so it
/// does not disturb mtime on already-correct files.
pub fn apply_mode(path: &Path, mode: &Option<String>) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    let cur = meta.permissions().mode();
    let new = if mode.is_some() {
        cur | 0o111
    } else {
        cur & !0o111
    };
    if new != cur {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(new)).ok();
    }
}

/// Returns true when the on-disk owner execute bit (0o100) agrees with the
/// entry `mode`. Scan records `mode` from the 0o100 bit only, while
/// `apply_mode` sets/clears all of 0o111 — this asymmetry is intentional, so
/// the comparison here uses the same 0o100 bit as scan.
pub fn mode_matches(path: &Path, mode: &Option<String>) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let on_disk_exec = meta.permissions().mode() & 0o100 != 0;
    on_disk_exec == mode.is_some()
}

/// Derive the tree-entry `mode` field from filesystem metadata: `Some("755")`
/// when the owner execute bit (0o100) is set, `None` otherwise. This is the
/// deriving counterpart of `apply_mode` (which writes it back) and
/// `mode_matches` (which compares it) -- all three agree on the 0o100 bit.
///
/// Consolidates a derivation that was previously copy-pasted at scan.rs,
/// commands/push.rs (x2), and commands/ls.rs (refactor-instructions.md D1).
pub fn mode_from_metadata(meta: &std::fs::Metadata) -> Option<String> {
    if meta.permissions().mode() & 0o100 != 0 {
        Some("755".to_string())
    } else {
        None
    }
}

/// Convert filesystem metadata's mtime (`SystemTime`) to the UTC `DateTime`
/// stored in a tree entry. Returns `None` if the mtime is unavailable (e.g.
/// unsupported platform) or out of the representable range.
pub fn mtime_from_metadata(meta: &std::fs::Metadata) -> Option<DateTime<Utc>> {
    let st = meta.modified().ok()?;
    let dur = st.duration_since(std::time::UNIX_EPOCH).ok()?;
    Utc.timestamp_opt(dur.as_secs() as i64, dur.subsec_nanos())
        .single()
}

/// Build a `TreeEntry::Blob` for a real file on disk at `abs_path`, given its
/// already-computed `hash`. Fetches metadata once and derives `size`/`mtime`/
/// `mode` from it via `mtime_from_metadata`/`mode_from_metadata`.
///
/// Consolidates the fetch-metadata-then-build-Blob-entry sequence that was
/// previously copy-pasted at commands/push.rs (x2) and commands/ls.rs
/// (refactor-instructions.md D1). `scan.rs`'s own copy is not routed through
/// this helper because it already holds a `Metadata` it fetched earlier in
/// the same pass (via `symlink_metadata`) and does not build a `TreeEntry`
/// directly at that point; it uses `mode_from_metadata` alone instead.
#[allow(dead_code)]
pub fn tree_entry_blob_from_fs(
    abs_path: &Path,
    name: String,
    hash: Hash,
) -> Result<TreeEntry, Error> {
    let meta = std::fs::metadata(abs_path)?;
    Ok(TreeEntry::Blob {
        name,
        hash,
        mtime: mtime_from_metadata(&meta),
        size: meta.len(),
        mode: mode_from_metadata(&meta),
    })
}

/// Create (or atomically replace) a symlink at `path` pointing to `target`.
///
/// Routes through `fsguard::write_symlink` (design/03_sync_model.md, "Write
/// safety: the working-tree write boundary"), which resolves `path`'s parent
/// component-by-component from a `work_dir` directory fd and creates the
/// symlink at a temporary sibling name before renaming it into place, so
/// readers never observe a half-written or missing link, and a component
/// strictly below `work_dir` that has been swapped for a symlink is refused
/// rather than followed. `work_dir` must be the true repository root, not a
/// scoped subdirectory -- passing a scoped directory would exempt everything
/// at or above it from the check.
pub fn write_symlink_atomic(work_dir: &Path, path: &Path, target: &str) -> Result<(), Error> {
    crate::fsguard::write_symlink(work_dir, path, target)
}

/// Stream `hash`'s content (already present in `store`) into `abs_path` via
/// temp+rename, then restore `mtime` and the executable-bit `mode`.
///
/// Consolidates the streaming-materialise + mtime + mode sequence that was
/// copy-pasted at five call sites (refactor-instructions.md E2): pull.rs's
/// `materialise_blob` and `materialise_tree`'s blob arm, clone.rs's
/// `expand_or_stub`'s blob arm, expand.rs's `expand_tree`'s blob arm, and
/// restore.rs's `write_blob`.
///
/// Deliberately NOT consolidated here: parent-directory creation and stub
/// removal, which some callers need and others don't (some already know the
/// parent exists; some sites have no stub to remove) -- and, more
/// importantly, ensuring `hash` is actually present in `store` before
/// calling this, which differs meaningfully per caller (see
/// `codec::ensure_local_then_read`'s doc comment for why that step resists
/// a single shared implementation). Callers remain responsible for both.
pub fn materialise_blob_at(
    work_dir: &Path,
    store: &dyn ObjectStore,
    hash: &Hash,
    abs_path: &Path,
    mtime: &Option<DateTime<Utc>>,
    mode: &Option<String>,
) -> Result<(), Error> {
    // Streaming materialisation (temp + rename, bounded memory): a crash
    // mid-write must not leave a half-written file that looks like a local
    // modification. See design/11 and design/02 "Streaming read".
    //
    // The working-tree write boundary (`fsguard`) is enforced inside
    // `materialise_to_file` itself, not here: `expand`'s top-level blob-stub
    // branch and `pull`'s conflict-helper-file write call
    // `codec::chunk::materialise_to_file` directly, bypassing this wrapper,
    // so checking one level down is what actually covers every call site
    // (design/03_sync_model.md, "Write safety: the working-tree write
    // boundary"). `work_dir` must be the true repository root, not a scoped
    // subdirectory.
    crate::codec::chunk::materialise_to_file(work_dir, store, hash, None, abs_path)?;
    // Apply mtime/mode via the same guarded directory chain (N2): re-resolve
    // the parent through `fsguard` rather than a path-based `filetime`/
    // `fs::set_permissions` call, so a symlinked component cannot redirect
    // these metadata mutations either. Best-effort, matching the pre-fsguard
    // silent-on-error semantics of `apply_mode`/`filetime::set_file_mtime`:
    // the content write above already succeeded, so a failure here can only
    // be an extremely narrow post-write race.
    if let Ok(rel) = abs_path.strip_prefix(work_dir) {
        // `\` is an ordinary Linux filename byte, not a separator -- do not
        // rewrite it to `/` (design/01_object_model.md invariant #5).
        let rel_str = rel.to_string_lossy().into_owned();
        if let Ok((dir, leaf)) = crate::fsguard::open_leaf(work_dir, &rel_str, false) {
            if let Some(mt) = mtime {
                let ft = FileTime::from_unix_time(mt.timestamp(), mt.timestamp_subsec_nanos());
                dir.set_mtime(&leaf, ft).ok();
            }
            dir.set_mode(&leaf, mode).ok();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_file(dir: &Path, perms: u32) -> std::path::PathBuf {
        let path = dir.join("f");
        fs::write(&path, b"x").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(perms)).unwrap();
        path
    }

    fn perms_of(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn apply_mode_sets_exec_bits() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = make_file(tmp.path(), 0o644);
        apply_mode(&f, &Some("755".to_string()));
        assert_eq!(perms_of(&f), 0o755);
    }

    #[test]
    fn apply_mode_clears_exec_bits() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = make_file(tmp.path(), 0o755);
        apply_mode(&f, &None);
        assert_eq!(perms_of(&f), 0o644);
    }

    #[test]
    fn apply_mode_preserves_rw_bits() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = make_file(tmp.path(), 0o600);
        apply_mode(&f, &Some("755".to_string()));
        assert_eq!(perms_of(&f), 0o711);
    }

    #[test]
    fn apply_mode_is_idempotent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = make_file(tmp.path(), 0o644);
        apply_mode(&f, &None);
        assert_eq!(perms_of(&f), 0o644);
        apply_mode(&f, &Some("755".to_string()));
        apply_mode(&f, &Some("755".to_string()));
        assert_eq!(perms_of(&f), 0o755);
    }

    #[test]
    #[cfg(unix)]
    fn restore_symlink_mtime_sets_link_not_target() {
        use chrono::TimeZone;
        let tmp = tempfile::TempDir::new().unwrap();
        // A target file with a "fresh" mtime, and a symlink pointing at it.
        let target = tmp.path().join("target");
        fs::write(&target, b"x").unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink("target", &link).unwrap();

        let want = Utc.with_ymd_and_hms(2020, 1, 2, 3, 4, 5).unwrap();
        restore_symlink_mtime(&link, &Some(want));

        // The link's own mtime is updated (lstat, no follow)...
        let link_meta = fs::symlink_metadata(&link).unwrap();
        let link_mtime = FileTime::from_last_modification_time(&link_meta);
        assert_eq!(link_mtime.unix_seconds(), want.timestamp());

        // ...while the target file's mtime is left untouched (not 2020).
        let target_meta = fs::metadata(&target).unwrap();
        let target_mtime = FileTime::from_last_modification_time(&target_meta);
        assert_ne!(target_mtime.unix_seconds(), want.timestamp());
    }

    #[test]
    #[cfg(unix)]
    fn restore_symlink_mtime_none_is_noop() {
        let tmp = tempfile::TempDir::new().unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink("nowhere", &link).unwrap();
        // Must not panic and must leave the link in place.
        restore_symlink_mtime(&link, &None);
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn mode_matches_both_polarities() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = make_file(tmp.path(), 0o644);
        assert!(mode_matches(&f, &None));
        assert!(!mode_matches(&f, &Some("755".to_string())));
        fs::set_permissions(&f, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(mode_matches(&f, &Some("755".to_string())));
        assert!(!mode_matches(&f, &None));
    }

    #[test]
    fn mode_from_metadata_both_polarities() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = make_file(tmp.path(), 0o644);
        assert_eq!(mode_from_metadata(&fs::metadata(&f).unwrap()), None);
        fs::set_permissions(&f, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            mode_from_metadata(&fs::metadata(&f).unwrap()),
            Some("755".to_string())
        );
    }

    #[test]
    fn mode_from_metadata_agrees_with_apply_mode_and_mode_matches() {
        // The three 0o100-bit helpers (derive / apply / compare) must be
        // mutually consistent: derive-then-apply-then-derive is a fixed point,
        // and mode_matches must agree with mode_from_metadata's answer.
        let tmp = tempfile::TempDir::new().unwrap();
        let f = make_file(tmp.path(), 0o600);
        for mode in [None, Some("755".to_string())] {
            apply_mode(&f, &mode);
            let derived = mode_from_metadata(&fs::metadata(&f).unwrap());
            assert_eq!(derived, mode);
            assert!(mode_matches(&f, &mode));
        }
    }

    #[test]
    fn mtime_from_metadata_round_trips_through_set_file_mtime() {
        use chrono::TimeZone;
        let tmp = tempfile::TempDir::new().unwrap();
        let f = make_file(tmp.path(), 0o644);
        let want = Utc.with_ymd_and_hms(2024, 6, 15, 12, 30, 0).unwrap();
        let ft = FileTime::from_unix_time(want.timestamp(), want.timestamp_subsec_nanos());
        filetime::set_file_mtime(&f, ft).unwrap();
        let got = mtime_from_metadata(&fs::metadata(&f).unwrap());
        assert_eq!(got, Some(want));
    }

    // -------------------------------------------------------------------
    // Working-tree write boundary (design/03_sync_model.md, "Write safety:
    // the working-tree write boundary"). Deeper coverage of the guard
    // mechanism itself (symlinked intermediate components at any depth, a
    // symlinked `work_dir` root, component validation, etc.) lives in
    // `src/fsguard.rs`'s own unit tests; these just confirm
    // `materialise_blob_at`/`write_symlink_atomic` are wired to it.
    // -------------------------------------------------------------------

    #[test]
    #[cfg(unix)]
    fn materialise_blob_at_refuses_to_write_through_symlinked_parent() {
        let work_dir = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();

        let sub = work_dir.path().join("sub");
        std::os::unix::fs::symlink(outside.path(), &sub).unwrap();

        let cache_dir = tempfile::TempDir::new().unwrap();
        let store = crate::store::local::LocalStore::for_cache(cache_dir.path());
        let content = b"payload";
        let hash = crate::object::blob_hash(content);
        let serialised = crate::object::serialise_blob(content);
        crate::codec::chunk::store_chunked(&store, &hash, &serialised, None).unwrap();

        let dest = sub.join("child.txt");
        let err = materialise_blob_at(work_dir.path(), &store, &hash, &dest, &None, &None)
            .unwrap_err();
        assert!(matches!(err, Error::SymlinkEscape(_)));
        assert!(
            !outside.path().join("child.txt").exists(),
            "content must never be written outside work_dir"
        );
    }

    #[test]
    fn materialise_blob_at_writes_normally_through_real_dirs() {
        // Regression guard: the guard must not block ordinary materialisation
        // through real, non-symlinked directories.
        let work_dir = tempfile::TempDir::new().unwrap();
        let sub = work_dir.path().join("sub");
        fs::create_dir_all(&sub).unwrap();

        let cache_dir = tempfile::TempDir::new().unwrap();
        let store = crate::store::local::LocalStore::for_cache(cache_dir.path());
        let content = b"payload";
        let hash = crate::object::blob_hash(content);
        let serialised = crate::object::serialise_blob(content);
        crate::codec::chunk::store_chunked(&store, &hash, &serialised, None).unwrap();

        let dest = sub.join("child.txt");
        materialise_blob_at(work_dir.path(), &store, &hash, &dest, &None, &None).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), content);
    }

    #[test]
    #[cfg(unix)]
    fn write_symlink_atomic_refuses_to_write_through_symlinked_parent() {
        let work_dir = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();

        let sub = work_dir.path().join("sub");
        std::os::unix::fs::symlink(outside.path(), &sub).unwrap();

        let dest = sub.join("link");
        let err = write_symlink_atomic(work_dir.path(), &dest, "target").unwrap_err();
        assert!(matches!(err, Error::SymlinkEscape(_)));
        assert!(
            !outside.path().join("link").exists(),
            "symlink must never be created outside work_dir"
        );
    }

    #[test]
    fn write_symlink_atomic_writes_normally_through_real_dirs() {
        // The parent directory ("sub") does not exist yet -- write_symlink_
        // atomic (via fsguard) must create it, matching the old behaviour.
        let work_dir = tempfile::TempDir::new().unwrap();
        let sub = work_dir.path().join("sub");

        let dest = sub.join("link");
        write_symlink_atomic(work_dir.path(), &dest, "target").unwrap();
        assert_eq!(fs::read_link(&dest).unwrap(), Path::new("target"));
    }

    #[test]
    fn tree_entry_blob_from_fs_builds_expected_fields() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = make_file(tmp.path(), 0o755);
        fs::write(&f, b"hello").unwrap();
        let hash = Hash::compute(b"hello");
        let entry = tree_entry_blob_from_fs(&f, "f".to_string(), hash.clone()).unwrap();
        match entry {
            TreeEntry::Blob {
                name,
                hash: h,
                size,
                mode,
                ..
            } => {
                assert_eq!(name, "f");
                assert_eq!(h, hash);
                assert_eq!(size, 5);
                assert_eq!(mode, Some("755".to_string()));
            }
            other => panic!("expected Blob, got {other:?}"),
        }
    }
}
