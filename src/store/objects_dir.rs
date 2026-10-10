/// Adaptive-depth object directory.
///
/// Files start flat (`objects/<64-char-hash>`) at depth 0.
/// When a directory exceeds THRESHOLD files, it is split into 256
/// two-char subdirectories and depth is incremented. Each subdirectory
/// tracks its own depth independently.
///
/// Migration is lazy: a `.migrating` marker is written first, then each
/// file is moved atomically. If the process crashes during migration, the
/// next write call resumes from where it left off.
///
/// Layout examples:
///   depth 0: `objects/<64>`
///   depth 1: `objects/<2>/<62>`
///   depth 2: `objects/<2>/<2>/<60>`
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use tempfile::NamedTempFile;

use crate::error::Error;
use crate::store::local::atomic_write;

const THRESHOLD: usize = 1000;
const DEPTH_FILE: &str = ".depth";
const MIGRATING_FILE: &str = ".migrating";

#[derive(Clone)]
pub struct ObjectsDir {
    pub root: PathBuf,
}

impl ObjectsDir {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        ObjectsDir { root: root.into() }
    }

    /// Find the path where `hex` (remaining hash chars) is stored under `dir`.
    /// Returns `None` if the object is not present.
    pub fn find(&self, hex: &str) -> Option<PathBuf> {
        find_in(&self.root, hex)
    }

    /// Stream `reader` into the object file for `hex`, migrating the shard if needed.
    /// Idempotent. No fsync is issued; the caller must issue a durability barrier
    /// before persisting any pointer (clone_root, STAT_CACHE) that references this object.
    pub fn write_stream(&self, hex: &str, reader: &mut dyn io::Read) -> Result<(), Error> {
        write_stream_in(&self.root, hex, reader)
    }

    /// Return `true` if `hex` exists anywhere in the tree.
    pub fn exists(&self, hex: &str) -> bool {
        self.find(hex).is_some()
    }

    /// Recursively collect all hex strings stored under the root.
    /// Used by `cat` for prefix searches.
    pub fn iter_hashes(&self) -> Vec<String> {
        let mut out = Vec::new();
        collect_hashes(&self.root, &mut String::new(), &mut out);
        out
    }
}

// ---------------------------------------------------------------------------
// Core recursive helpers
// ---------------------------------------------------------------------------

fn shard_depth(dir: &Path) -> u8 {
    let p = dir.join(DEPTH_FILE);
    fs::read_to_string(&p)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn find_in(dir: &Path, hex: &str) -> Option<PathBuf> {
    let depth = shard_depth(dir);
    let migrating = dir.join(MIGRATING_FILE).exists();

    if depth == 0 {
        // Flat layout.
        let flat = dir.join(hex);
        if flat.exists() {
            return Some(flat);
        }
        // During migration some files may already be in subdirs.
        if migrating && hex.len() >= 2 {
            let sub = dir.join(&hex[..2]).join(&hex[2..]);
            if sub.exists() {
                return Some(sub);
            }
        }
        None
    } else {
        // Subdirectory layout.
        if hex.len() < 2 {
            return None;
        }
        let subdir = dir.join(&hex[..2]);
        find_in(&subdir, &hex[2..])
    }
}

/// M5: a per-process, per-shard-directory mutex guarding both "resume an
/// in-progress migration" (`write_stream_in`) and "decide to start a new
/// migration" (`start_migration`), so two threads in the same process racing
/// through either sequence for the *same* shard directory serialise on this
/// lock instead of both calling `do_migration` concurrently (which used to
/// let one thread observe a bare `NotFound` on a rename/remove the other had
/// just performed, and abort via `?`). This is a per-process lock only --
/// see `do_migration`'s own `NotFound`-tolerance below for the cross-process
/// case (e.g. `omemfs ls`/`omemfs cat` writing into `.omemfs/objects/`
/// without holding the repo lock, concurrently with another `omemfs`
/// invocation, or with `restore`'s parallel heal loop in the same process
/// but a different `ObjectsDir` -- the map below is keyed by directory path,
/// so unrelated shard directories never contend with each other).
fn migration_lock(dir: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    let map = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = map.lock().unwrap();
    guard
        .entry(dir.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// Stream `reader` into the object file at the correct shard depth under `dir`.
/// Uses a rename-atomic write with no fsync (the caller must issue a durability
/// barrier before persisting any pointer that references this object).
fn write_stream_in(dir: &Path, hex: &str, reader: &mut dyn io::Read) -> Result<(), Error> {
    // Resume any interrupted migration first. Guarded by `migration_lock`
    // (M5): without it, two threads racing through this check could both
    // call `do_migration` concurrently.
    if dir.join(MIGRATING_FILE).exists() {
        let lock = migration_lock(dir);
        let _guard = lock.lock().unwrap();
        // Re-check under the lock: another thread may have already finished
        // resuming this migration while this thread waited for the lock.
        if dir.join(MIGRATING_FILE).exists() {
            do_migration(dir)?;
        }
    }

    let depth = shard_depth(dir);

    if depth == 0 {
        let path = dir.join(hex);
        if path.exists() {
            return Ok(());
        }
        fs::create_dir_all(dir)?;
        // No fsync: local cache objects are flushed to durable storage by the
        // caller's barrier (syncfs) before clone_root / STAT_CACHE is written.
        let mut tmp = NamedTempFile::new_in(dir)?;
        io::copy(reader, &mut tmp).map_err(Error::Io)?;
        tmp.persist(&path).map_err(|e| Error::Io(e.error))?;

        // Check threshold and migrate if needed.
        if hex.len() >= 2 && count_objects(dir) > THRESHOLD {
            start_migration(dir)?;
        }
        Ok(())
    } else {
        if hex.len() < 2 {
            return Err(Error::Other(
                "hash too short for current shard depth".to_string(),
            ));
        }
        let subdir = dir.join(&hex[..2]);
        write_stream_in(&subdir, &hex[2..], reader)
    }
}

/// Called by `write_stream_in` once it has observed `count_objects(dir) >
/// THRESHOLD` (the normal production path), but also called directly by
/// several tests below to force a migration well under `THRESHOLD` -- so
/// this function does not re-derive or re-check that threshold decision
/// itself; it trusts the caller and unconditionally proceeds.
fn start_migration(dir: &Path) -> Result<(), Error> {
    // M5: serialise against `write_stream_in`'s "resume" branch (and against
    // another thread also calling `start_migration` for this same shard
    // directory), so two threads that both crossed THRESHOLD before either
    // acquired the lock don't both write the marker and both run
    // `do_migration` concurrently.
    let lock = migration_lock(dir);
    let _guard = lock.lock().unwrap();
    // Re-check under the lock: another thread may have already started (and
    // possibly already finished) this same migration while this thread was
    // waiting for the lock. Only write the marker if it isn't already
    // there -- `do_migration` below is idempotent either way, so this is
    // purely to avoid redundant marker churn, not a correctness requirement:
    // even if two threads both proceeded to write the marker back-to-back
    // ordered by this same lock, or both ran `do_migration`, the outcome
    // would be identical.
    if !dir.join(MIGRATING_FILE).exists() {
        atomic_write(&dir.join(MIGRATING_FILE), b"")?;
    }
    do_migration(dir)
}

/// Move all flat object files into two-char subdirectories. Idempotent and
/// safely re-entrant across concurrent actors (M5): every fallible step below
/// tolerates `NotFound`, treating it as "another actor already moved/removed
/// this entry" rather than propagating. `write_stream_in`/`start_migration`
/// hold a per-process lock (`migration_lock`) before calling this in the
/// common case; this `NotFound`-tolerance is what additionally makes two
/// *separate processes* -- e.g. `omemfs ls` and `omemfs restore`'s heal loop,
/// neither of which holds the repo lock while writing into
/// `.omemfs/objects/` -- safe to run through this function concurrently too.
/// Calling this twice in a row on an already-migrated directory (no flat
/// files left, `.migrating` already gone) is a deliberate no-op, not an
/// error.
fn do_migration(dir: &Path) -> Result<(), Error> {
    let entries: Vec<_> = fs::read_dir(dir)?.flatten().collect();
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let old_path = entry.path();
        if !old_path.is_file() {
            continue;
        }
        if name.len() < 2 {
            continue;
        }
        let prefix = &name[..2];
        let rest = &name[2..];
        let subdir = dir.join(prefix);
        // `create_dir_all` already tolerates the directory already existing
        // (a concurrent actor may have created it first).
        fs::create_dir_all(&subdir)?;
        let new_path = subdir.join(rest);
        if !new_path.exists() {
            match fs::rename(&old_path, &new_path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    // Another actor already moved (or removed the stale
                    // flat copy of) this entry between our `read_dir`
                    // snapshot and this `rename`. Nothing left to do.
                    continue;
                }
                Err(e) => return Err(Error::Io(e)),
            }
        } else {
            // Already moved (crash resume, or a concurrent actor): remove
            // the stale flat copy, tolerating it already being gone too.
            match fs::remove_file(&old_path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(Error::Io(e)),
            }
        }
    }

    // Persist new depth.
    atomic_write(&dir.join(DEPTH_FILE), b"1\n")?;
    // Remove migration marker, tolerating it already being gone (another
    // actor finished the same migration first).
    match fs::remove_file(dir.join(MIGRATING_FILE)) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(Error::Io(e)),
    }
    Ok(())
}

/// Count actual object files in `dir` (excludes dotfiles and subdirectories).
fn count_objects(dir: &Path) -> usize {
    fs::read_dir(dir)
        .map(|iter| {
            iter.flatten()
                .filter(|e| {
                    let n = e.file_name().to_string_lossy().into_owned();
                    !n.starts_with('.') && e.path().is_file()
                })
                .count()
        })
        .unwrap_or(0)
}

/// Recursively collect full hex strings.
fn collect_hashes(dir: &Path, prefix: &mut String, out: &mut Vec<String>) {
    let depth = shard_depth(dir);
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if path.is_dir() && depth > 0 {
            prefix.push_str(&name);
            collect_hashes(&path, prefix, out);
            let new_len = prefix.len().saturating_sub(name.len());
            prefix.truncate(new_len);
        } else if path.is_file() {
            let mut full = prefix.clone();
            full.push_str(&name);
            out.push(full);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_hex(seed: u8) -> String {
        format!("{:02x}", seed).repeat(32)
    }

    fn write(od: &ObjectsDir, hex: &str, data: &[u8]) {
        od.write_stream(hex, &mut io::Cursor::new(data)).unwrap();
    }

    #[test]
    fn write_stream_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let od = ObjectsDir::new(tmp.path());
        let hex = make_hex(0xab);
        write(&od, &hex, b"hello");
        assert!(od.exists(&hex));
        let path = od.find(&hex).unwrap();
        assert_eq!(fs::read(path).unwrap(), b"hello");
    }

    #[test]
    fn write_and_find_flat() {
        let tmp = TempDir::new().unwrap();
        let od = ObjectsDir::new(tmp.path());
        let hex = make_hex(0xab);
        write(&od, &hex, b"data");
        assert!(od.exists(&hex));
        assert_eq!(od.find(&hex).unwrap().parent().unwrap(), tmp.path());
    }

    #[test]
    fn write_stream_idempotent() {
        let tmp = TempDir::new().unwrap();
        let od = ObjectsDir::new(tmp.path());
        let hex = make_hex(0xcd);
        write(&od, &hex, b"first");
        write(&od, &hex, b"second");
        let path = od.find(&hex).unwrap();
        assert_eq!(fs::read(path).unwrap(), b"first");
    }

    #[test]
    fn migration_on_threshold() {
        let tmp = TempDir::new().unwrap();
        let od = ObjectsDir::new(tmp.path());

        // Write THRESHOLD+1 objects to trigger migration.
        for i in 0u32..=(THRESHOLD as u32) {
            let hex = format!("{:064x}", i);
            write(&od, &hex, b"x");
        }

        // After migration, new objects should be in subdirs.
        let hex_last = format!("{:064x}", THRESHOLD as u32 + 1);
        write(&od, &hex_last, b"y");
        assert!(od.exists(&hex_last));

        // Depth file should now exist.
        assert_eq!(shard_depth(tmp.path()), 1);
    }

    #[test]
    fn find_after_migration() {
        let tmp = TempDir::new().unwrap();
        let od = ObjectsDir::new(tmp.path());

        // Write some objects flat.
        let hexes: Vec<String> = (0u32..10).map(|i| format!("{:064x}", i)).collect();
        for h in &hexes {
            write(&od, h, b"data");
        }

        // Force migration.
        start_migration(tmp.path()).unwrap();

        // All objects still findable.
        for h in &hexes {
            assert!(od.exists(h), "missing after migration: {}", h);
        }
    }

    #[test]
    fn write_stream_across_migration() {
        let tmp = TempDir::new().unwrap();
        let od = ObjectsDir::new(tmp.path());

        // Write some flat objects, then force migration.
        let hexes: Vec<String> = (0u32..5).map(|i| format!("{:064x}", i)).collect();
        for h in &hexes {
            write(&od, h, b"before");
        }
        start_migration(tmp.path()).unwrap();

        // Write via write_stream after migration — should land in shard subdir.
        let new_hex = format!("{:064x}", 99u32);
        write(&od, &new_hex, b"after");
        assert!(od.exists(&new_hex));

        // Pre-migration objects still findable.
        for h in &hexes {
            assert!(od.exists(h), "missing after migration: {}", h);
        }
    }

    /// design/07 "Seal epoch": `.seal_epoch` is a dotfile directly inside
    /// `.omemfs/objects/`, at the same level as `.depth` / `.migrating`. Every
    /// existing directory-walk helper in this module already skips dotfiles
    /// (`name.starts_with('.')`), so a `.seal_epoch` marker must be invisible
    /// to `iter_hashes`, `count_objects` (migration threshold), and migration
    /// itself -- no enumeration path needs to change to accommodate it.
    #[test]
    fn seal_epoch_marker_is_invisible_to_directory_walks() {
        let tmp = TempDir::new().unwrap();
        let od = ObjectsDir::new(tmp.path());

        // Write a couple of real objects plus the seal_epoch marker directly
        // alongside them, exactly where StatCache places it.
        write(&od, &make_hex(0x01), b"a");
        write(&od, &make_hex(0x02), b"b");
        fs::write(tmp.path().join(".seal_epoch"), [0x42u8; 16]).unwrap();

        // iter_hashes (used by `cat`'s prefix search) must not surface it.
        let hashes = od.iter_hashes();
        assert_eq!(hashes.len(), 2, "the marker must not be counted as an object");
        assert!(!hashes.iter().any(|h| h.starts_with(".seal_epoch")));

        // count_objects (migration threshold) must not count it either: write
        // exactly THRESHOLD more real objects and confirm migration triggers
        // at the same point it would without the marker present.
        for i in 3u32..=(THRESHOLD as u32 + 1) {
            write(&od, &format!("{:064x}", i), b"x");
        }
        assert_eq!(
            shard_depth(tmp.path()),
            1,
            "migration threshold must be reached by real objects alone"
        );

        // The marker must survive migration untouched (do_migration also
        // skips dotfiles) and must never be moved into a shard subdirectory.
        assert_eq!(
            fs::read(tmp.path().join(".seal_epoch")).unwrap(),
            vec![0x42u8; 16],
            "the marker must survive migration in place, unmoved"
        );
    }

    /// M5: two threads (or, cross-process, two separate `omemfs` invocations
    /// neither of which holds the repo lock while writing into
    /// `.omemfs/objects/` -- e.g. `omemfs ls`/`omemfs cat`, or restore's
    /// parallel heal loop) can enter `write_stream_in`'s "resume migration"
    /// branch for the *same* shard directory at the same time. Without a
    /// lock, one thread's `fs::rename`/`fs::remove_file` can observe
    /// `NotFound` for an entry the other thread already moved, and abort via
    /// `?`. Every concurrent write here must still succeed.
    #[test]
    fn concurrent_writes_resume_migration_exactly_once_without_errors() {
        let tmp = TempDir::new().unwrap();
        let od = Arc::new(ObjectsDir::new(tmp.path()));

        // Preload past THRESHOLD, flat, then simulate a migration that was
        // started (marker present) but never finished -- e.g. a crash, or
        // another actor that only got partway.
        let hexes: Vec<String> = (0u32..=(THRESHOLD as u32)).map(|i| format!("{:064x}", i)).collect();
        for h in &hexes {
            write(&od, h, b"data");
        }
        fs::write(tmp.path().join(MIGRATING_FILE), "").unwrap();

        // N threads race to write brand-new objects while MIGRATING_FILE is
        // still present. Each must resume/complete the migration (or observe
        // it already finished by a sibling) and land its own write
        // successfully -- none may abort with a bare NotFound.
        let n = 8u32;
        let mut handles = Vec::new();
        for i in 0..n {
            let od = Arc::clone(&od);
            let hex = format!("{:064x}", THRESHOLD as u32 + 10 + i);
            handles.push(std::thread::spawn(move || {
                od.write_stream(&hex, &mut io::Cursor::new(b"new".to_vec()))
            }));
        }
        for h in handles {
            h.join()
                .unwrap()
                .expect("concurrent write into a resuming shard must not error");
        }

        // Everything -- pre-existing and newly written -- must still be findable.
        for h in &hexes {
            assert!(od.exists(h), "missing after concurrent migration resume: {}", h);
        }
        for i in 0..n {
            let hex = format!("{:064x}", THRESHOLD as u32 + 10 + i);
            assert!(od.exists(&hex), "new object not findable: {}", hex);
        }

        // The shard must end at depth 1 (a single migration), not deeper.
        assert_eq!(shard_depth(tmp.path()), 1);
    }

    /// M5: two threads can also cross `THRESHOLD` together and both decide to
    /// start a *new* migration. `start_migration` must serialise on the same
    /// per-shard lock and re-check under it, so only one migration actually
    /// runs; `do_migration`'s own idempotence (exercised directly below) is
    /// the second, cross-process layer of defence.
    #[test]
    fn concurrent_writes_crossing_threshold_together_migrate_exactly_once() {
        let tmp = TempDir::new().unwrap();
        let od = Arc::new(ObjectsDir::new(tmp.path()));

        // Preload to exactly THRESHOLD (one below the trigger), flat.
        let hexes: Vec<String> = (0u32..(THRESHOLD as u32)).map(|i| format!("{:064x}", i)).collect();
        for h in &hexes {
            write(&od, h, b"data");
        }
        assert_eq!(shard_depth(tmp.path()), 0);

        // N threads each write one more object concurrently: every one of
        // them individually observes `count_objects(dir) > THRESHOLD` once
        // enough siblings have landed, so several may call `start_migration`
        // at roughly the same time.
        let n = 8u32;
        let mut handles = Vec::new();
        for i in 0..n {
            let od = Arc::clone(&od);
            let hex = format!("{:064x}", THRESHOLD as u32 + i);
            handles.push(std::thread::spawn(move || {
                od.write_stream(&hex, &mut io::Cursor::new(b"new".to_vec()))
            }));
        }
        for h in handles {
            h.join()
                .unwrap()
                .expect("a write racing another thread across THRESHOLD must not error");
        }

        for h in &hexes {
            assert!(od.exists(h));
        }
        for i in 0..n {
            let hex = format!("{:064x}", THRESHOLD as u32 + i);
            assert!(od.exists(&hex));
        }
        assert_eq!(shard_depth(tmp.path()), 1, "must migrate exactly once, not stay at depth 0 or double-migrate");
    }

    /// M5: `do_migration` must be idempotent even outside the lock-protected
    /// call sites -- calling it directly, twice in a row, on a directory that
    /// is already fully migrated (no flat files left, `.migrating` already
    /// gone) must be a no-op, not an error. This is the cross-process safety
    /// net `migration_lock` (per-process only) cannot provide on its own.
    #[test]
    fn do_migration_twice_on_already_migrated_dir_is_a_noop() {
        let tmp = TempDir::new().unwrap();
        let od = ObjectsDir::new(tmp.path());
        let hexes: Vec<String> = (0u32..5).map(|i| format!("{:064x}", i)).collect();
        for h in &hexes {
            write(&od, h, b"data");
        }
        start_migration(tmp.path()).unwrap();
        assert_eq!(shard_depth(tmp.path()), 1);

        do_migration(tmp.path()).unwrap();
        assert_eq!(shard_depth(tmp.path()), 1);
        for h in &hexes {
            assert!(od.exists(h));
        }
    }

    #[test]
    fn crash_resume_migration() {
        let tmp = TempDir::new().unwrap();
        let od = ObjectsDir::new(tmp.path());

        let hexes: Vec<String> = (0u32..5).map(|i| format!("{:064x}", i)).collect();
        for h in &hexes {
            write(&od, h, b"data");
        }

        // Simulate crash mid-migration: write .migrating but only partially move files.
        fs::write(tmp.path().join(MIGRATING_FILE), "").unwrap();
        // Move only first file manually.
        let first = &hexes[0];
        let subdir = tmp.path().join(&first[..2]);
        fs::create_dir_all(&subdir).unwrap();
        fs::rename(tmp.path().join(first), subdir.join(&first[2..])).unwrap();

        // Next write should resume migration.
        let new_hex = format!("{:064x}", 99u32);
        write(&od, &new_hex, b"new");

        // All objects should be findable.
        for h in &hexes {
            assert!(od.exists(h), "missing after resume: {}", h);
        }
        assert!(od.exists(&new_hex));
    }
}
