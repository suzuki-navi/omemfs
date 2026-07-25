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
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

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

/// Stream `reader` into the object file at the correct shard depth under `dir`.
/// Uses a rename-atomic write with no fsync (the caller must issue a durability
/// barrier before persisting any pointer that references this object).
fn write_stream_in(dir: &Path, hex: &str, reader: &mut dyn io::Read) -> Result<(), Error> {
    // Resume any interrupted migration first.
    if dir.join(MIGRATING_FILE).exists() {
        do_migration(dir)?;
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

fn start_migration(dir: &Path) -> Result<(), Error> {
    // Write .migrating marker before touching any files.
    atomic_write(&dir.join(MIGRATING_FILE), b"")?;
    do_migration(dir)
}

fn do_migration(dir: &Path) -> Result<(), Error> {
    // Move all flat object files into two-char subdirectories.
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
        fs::create_dir_all(&subdir)?;
        let new_path = subdir.join(rest);
        if !new_path.exists() {
            fs::rename(&old_path, &new_path)?;
        } else {
            // Already moved (crash resume): remove the stale flat copy.
            fs::remove_file(&old_path)?;
        }
    }

    // Persist new depth.
    atomic_write(&dir.join(DEPTH_FILE), b"1\n")?;
    // Remove migration marker.
    let migrating = dir.join(MIGRATING_FILE);
    if migrating.exists() {
        fs::remove_file(&migrating)?;
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
