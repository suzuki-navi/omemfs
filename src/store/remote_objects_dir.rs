/// Fixed 3-level sharding for remote object storage.
///
/// All remote backends (local directory, S3, GCS, Azure) use this layout:
///
///   objects/<hash[0..2]>/<hash[2..4]>/<hash[4..6]>/<hash[6..64]>
///
/// Unlike the local `ObjectsDir` (adaptive depth), this layout is fixed
/// from the first write and never migrates.
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use tempfile::NamedTempFile;

use crate::error::Error;

#[derive(Clone)]
pub struct RemoteObjectsDir {
    pub root: PathBuf,
}

impl RemoteObjectsDir {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        RemoteObjectsDir { root: root.into() }
    }

    /// Return the path where `hex` would be stored, whether or not it exists.
    pub fn expected_path(&self, hex: &str) -> Option<PathBuf> {
        if hex.len() != 64 {
            return None;
        }
        Some(
            self.root
                .join(&hex[0..2])
                .join(&hex[2..4])
                .join(&hex[4..6])
                .join(&hex[6..]),
        )
    }

    /// Find the path where `hex` is stored. Returns `None` if absent.
    pub fn find(&self, hex: &str) -> Option<PathBuf> {
        let path = self.expected_path(hex)?;
        if path.exists() { Some(path) } else { None }
    }

    /// Stream `reader` into the object file for `hex`. Durable: syncs to storage
    /// before returning. Idempotent.
    pub fn write_stream(&self, hex: &str, reader: &mut dyn io::Read) -> Result<(), Error> {
        let path = self
            .expected_path(hex)
            .ok_or_else(|| Error::Other(format!("invalid hash length: {}", hex.len())))?;
        if path.exists() {
            return Ok(());
        }
        let parent = path.parent().unwrap_or(&path);
        fs::create_dir_all(parent)?;
        // Remote objects are written durably so any clone can rely on them
        // being stable once written.
        let mut tmp = NamedTempFile::new_in(parent)?;
        io::copy(reader, &mut tmp).map_err(Error::Io)?;
        tmp.as_file().sync_all().map_err(Error::Io)?;
        tmp.persist(&path).map_err(|e| Error::Io(e.error))?;
        fs::File::open(parent)
            .map_err(Error::Io)?
            .sync_all()
            .map_err(Error::Io)?;
        Ok(())
    }

    /// Return `true` if `hex` exists.
    pub fn exists(&self, hex: &str) -> bool {
        self.find(hex).is_some()
    }

    /// Recursively collect all hex strings stored under the root.
    pub fn iter_hashes(&self) -> Vec<String> {
        let mut out = Vec::new();
        collect_hashes(&self.root, "", &mut out);
        out
    }
}

fn collect_hashes(dir: &Path, prefix: &str, out: &mut Vec<String>) {
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
        let combined = format!("{}{}", prefix, name);
        if path.is_dir() {
            collect_hashes(&path, &combined, out);
        } else if path.is_file() && combined.len() == 64 {
            out.push(combined);
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

    fn write(rod: &RemoteObjectsDir, hex: &str, data: &[u8]) {
        rod.write_stream(hex, &mut io::Cursor::new(data)).unwrap();
    }

    #[test]
    fn write_stream_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let rod = RemoteObjectsDir::new(tmp.path());
        let hex = make_hex(0xab);
        write(&rod, &hex, b"hello");
        assert!(rod.exists(&hex));
        let path = rod.find(&hex).unwrap();
        assert_eq!(fs::read(path).unwrap(), b"hello");
    }

    #[test]
    fn expected_path_structure() {
        let tmp = TempDir::new().unwrap();
        let rod = RemoteObjectsDir::new(tmp.path());
        let hex = make_hex(0xab);
        let path = rod.expected_path(&hex).unwrap();
        // Should be root/ab/ab/ab/<remaining 58 chars>
        let rel: Vec<_> = path
            .strip_prefix(tmp.path())
            .unwrap()
            .components()
            .collect();
        assert_eq!(rel.len(), 4);
        assert_eq!(rel[0].as_os_str(), "ab");
        assert_eq!(rel[1].as_os_str(), "ab");
        assert_eq!(rel[2].as_os_str(), "ab");
        // Remaining 58 characters
        assert_eq!(rel[3].as_os_str().len(), 58);
    }

    #[test]
    fn write_and_find() {
        let tmp = TempDir::new().unwrap();
        let rod = RemoteObjectsDir::new(tmp.path());
        let hex = make_hex(0x12);
        write(&rod, &hex, b"data");
        assert!(rod.exists(&hex));
        let path = rod.find(&hex).unwrap();
        // Verify directory depth: root/12/12/12/<58 chars>
        let rel: Vec<_> = path
            .strip_prefix(tmp.path())
            .unwrap()
            .components()
            .collect();
        assert_eq!(rel.len(), 4);
    }

    #[test]
    fn idempotent_write() {
        let tmp = TempDir::new().unwrap();
        let rod = RemoteObjectsDir::new(tmp.path());
        let hex = make_hex(0x34);
        write(&rod, &hex, b"first");
        write(&rod, &hex, b"second");
        let path = rod.find(&hex).unwrap();
        assert_eq!(fs::read(path).unwrap(), b"first");
    }

    #[test]
    fn iter_hashes_collects_all() {
        let tmp = TempDir::new().unwrap();
        let rod = RemoteObjectsDir::new(tmp.path());
        let hexes: Vec<String> = (0u32..5).map(|i| format!("{:064x}", i)).collect();
        for h in &hexes {
            write(&rod, h, b"x");
        }
        let mut collected = rod.iter_hashes();
        collected.sort();
        let mut expected = hexes.clone();
        expected.sort();
        assert_eq!(collected, expected);
    }

    #[test]
    fn invalid_hash_returns_none() {
        let tmp = TempDir::new().unwrap();
        let rod = RemoteObjectsDir::new(tmp.path());
        assert!(rod.expected_path("tooshort").is_none());
    }
}
