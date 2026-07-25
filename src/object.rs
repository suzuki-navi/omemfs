use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::dlog_l2;
use crate::error::Error;

// ---------------------------------------------------------------------------
// Hash
// ---------------------------------------------------------------------------

/// SHA-256 object hash — 64 hex characters.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Hash {
    hex: String,
    bytes: [u8; 32],
}

impl Hash {
    pub fn from_hex(hex: &str) -> Result<Self, Error> {
        if hex.len() != 64 {
            return Err(Error::InvalidHash);
        }
        let mut bytes = [0u8; 32];
        for (i, chunk) in hex.as_bytes().chunks_exact(2).enumerate() {
            let hi = hex_nibble(chunk[0]).ok_or(Error::InvalidHash)?;
            let lo = hex_nibble(chunk[1]).ok_or(Error::InvalidHash)?;
            bytes[i] = (hi << 4) | lo;
        }
        Ok(Hash {
            hex: hex.to_ascii_lowercase(),
            bytes,
        })
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Hash {
            hex: encode_hex(&bytes),
            bytes,
        }
    }

    /// Compute SHA-256 hash of `data`.
    pub fn compute(data: &[u8]) -> Self {
        let digest = Sha256::digest(data);
        let bytes: [u8; 32] = digest.into();
        Hash::from_bytes(bytes)
    }

    pub fn as_str(&self) -> &str {
        &self.hex
    }

    pub fn as_bytes_array(&self) -> &[u8; 32] {
        &self.bytes
    }
}

impl std::fmt::Display for Hash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.hex)
    }
}

impl Serialize for Hash {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.hex)
    }
}

impl<'de> Deserialize<'de> for Hash {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let hex = String::deserialize(d)?;
        Hash::from_hex(&hex).map_err(serde::de::Error::custom)
    }
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn encode_hex(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(64);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

// ---------------------------------------------------------------------------
// Tree
// ---------------------------------------------------------------------------

/// Entry in a normal tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TreeEntry {
    Blob {
        name: String,
        hash: Hash,
        mtime: Option<DateTime<Utc>>,
        size: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        mode: Option<String>,
    },
    Tree {
        name: String,
        hash: Hash,
        mtime: Option<DateTime<Utc>>,
        size: u64,
        /// Total number of blob (and symlink) leaves reachable from this tree.
        #[serde(default)]
        blob_count: u64,
    },
    Symlink {
        name: String,
        target: String,
        mtime: Option<DateTime<Utc>>,
    },
}

impl TreeEntry {
    pub fn name(&self) -> &str {
        match self {
            TreeEntry::Blob { name, .. } => name,
            TreeEntry::Tree { name, .. } => name,
            TreeEntry::Symlink { name, .. } => name,
        }
    }

    pub fn hash(&self) -> Option<&Hash> {
        match self {
            TreeEntry::Blob { hash, .. } => Some(hash),
            TreeEntry::Tree { hash, .. } => Some(hash),
            TreeEntry::Symlink { .. } => None,
        }
    }

    pub fn size(&self) -> u64 {
        match self {
            TreeEntry::Blob { size, .. } => *size,
            TreeEntry::Tree { size, .. } => *size,
            TreeEntry::Symlink { .. } => 0,
        }
    }

    /// Number of blob/symlink leaves in this entry's subtree.
    /// Blob and Symlink entries return 1; Tree entries return their stored count.
    pub fn blob_count(&self) -> u64 {
        match self {
            TreeEntry::Blob { .. } => 1,
            TreeEntry::Tree { blob_count, .. } => *blob_count,
            TreeEntry::Symlink { .. } => 1,
        }
    }

    pub fn mtime(&self) -> Option<&DateTime<Utc>> {
        match self {
            TreeEntry::Blob { mtime, .. } => mtime.as_ref(),
            TreeEntry::Tree { mtime, .. } => mtime.as_ref(),
            TreeEntry::Symlink { mtime, .. } => mtime.as_ref(),
        }
    }
}

/// A tree object.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Tree {
    Normal { entries: Vec<TreeEntry> },
}

/// Serialize-layer type tags (ED Fx range).
/// These are prepended by serialize and stripped by deserialize.
/// The compress stage uses ED Dx and will not escape ED Fx.
pub const TYPE_TAG_BLOB: [u8; 2] = [0xED, 0xF0];
pub const TYPE_TAG_TREE: [u8; 2] = [0xED, 0xF1];
/// Chunked manifest: ED F2 followed by raw 32-byte chunk hashes concatenated.
pub const TYPE_TAG_MANIFEST: [u8; 2] = [0xED, 0xF2];
/// Chunk body: ED F3 followed by the raw chunk bytes.
pub const TYPE_TAG_CHUNK: [u8; 2] = [0xED, 0xF3];

/// Returns true when blob content needs the ED F0 escape prefix: its first two
/// bytes are `ED Fx` (x in F0..FF), which would otherwise collide with an L2
/// type tag. Factored out so the in-memory and streaming write paths apply the
/// identical rule.
pub fn blob_needs_escape(first: Option<u8>, second: Option<u8>) -> bool {
    (first == Some(0xED)) && second.is_some_and(|b| b >= 0xF0)
}

/// Serialise blob bytes for storage: apply ED F0 escape if the first byte is
/// in the ED Fx range.
pub fn serialise_blob(content: &[u8]) -> Vec<u8> {
    if blob_needs_escape(content.first().copied(), content.get(1).copied()) {
        let mut out = Vec::with_capacity(2 + content.len());
        out.extend_from_slice(&TYPE_TAG_BLOB);
        out.extend_from_slice(content);
        out
    } else {
        content.to_vec()
    }
}

/// Deserialise blob bytes from storage: strip ED F0 escape if present.
pub fn deserialise_blob(stored: &[u8]) -> &[u8] {
    if stored.starts_with(&TYPE_TAG_BLOB) {
        &stored[2..]
    } else {
        stored
    }
}

/// Serialise a chunk body: ED F3 | chunk_bytes.
pub fn serialise_chunk(chunk_bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + chunk_bytes.len());
    out.extend_from_slice(&TYPE_TAG_CHUNK);
    out.extend_from_slice(chunk_bytes);
    out
}

/// Deserialise a chunk body: strip ED F3 prefix.
/// Returns `None` if the tag is not present.
pub fn deserialise_chunk(stored: &[u8]) -> Option<&[u8]> {
    if stored.starts_with(&TYPE_TAG_CHUNK) {
        Some(&stored[2..])
    } else {
        None
    }
}

/// Compute the chunk hash: SHA256(ED F3 | chunk_bytes).
pub fn chunk_hash(chunk_bytes: &[u8]) -> Hash {
    let mut data = Vec::with_capacity(2 + chunk_bytes.len());
    data.extend_from_slice(&TYPE_TAG_CHUNK);
    data.extend_from_slice(chunk_bytes);
    Hash::compute(&data)
}

/// Serialise a chunked manifest: ED F2 | hash[0] (32 bytes) | hash[1] | ...
pub fn serialise_manifest(chunk_hashes: &[Hash]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + chunk_hashes.len() * 32);
    out.extend_from_slice(&TYPE_TAG_MANIFEST);
    for h in chunk_hashes {
        out.extend_from_slice(h.as_bytes_array());
    }
    out
}

/// Deserialise a chunked manifest: returns chunk hash list, or `None` if the
/// leading tag is not ED F2 or the payload length is not a multiple of 32.
pub fn deserialise_manifest(stored: &[u8]) -> Option<Vec<Hash>> {
    if !stored.starts_with(&TYPE_TAG_MANIFEST) {
        return None;
    }
    let payload = &stored[2..];
    if !payload.len().is_multiple_of(32) {
        return None;
    }
    let hashes = payload
        .chunks_exact(32)
        .map(|chunk| {
            let mut bytes = [0u8; 32];
            bytes.copy_from_slice(chunk);
            Hash::from_bytes(bytes)
        })
        .collect();
    Some(hashes)
}

/// Compute the blob hash: SHA256(ED F0 | content bytes).
pub fn blob_hash(content: &[u8]) -> Hash {
    let mut data = Vec::with_capacity(2 + content.len());
    data.extend_from_slice(&TYPE_TAG_BLOB);
    data.extend_from_slice(content);
    Hash::compute(&data)
}

impl Tree {
    /// Hash of the empty tree (`Tree::Normal { entries: [] }`). This is the
    /// value `clone_root` holds when a clone has never successfully synced any
    /// content (design/02 "clone_root"). Used by the post-clone sync guard to
    /// distinguish a genuinely empty remote from a wrong-key / reset remote.
    pub fn empty_tree_hash() -> Hash {
        let serialised = Tree::Normal { entries: vec![] }.serialise();
        Hash::compute(&serialised)
    }

    /// Serialise to bytes: ED F1 prefix followed by minimised JSON.
    /// Entries must already be sorted by name before calling this.
    pub fn serialise(&self) -> Vec<u8> {
        let json = serde_json::to_vec(self).expect("tree serialisation is infallible");
        let hash = Hash::compute(&json);
        dlog_l2!(
            "serialize tree: {} {} entries, {}B JSON",
            &hash.as_str()[..8],
            match self {
                Tree::Normal { entries } => entries.len(),
            },
            json.len()
        );
        let mut out = Vec::with_capacity(2 + json.len());
        out.extend_from_slice(&TYPE_TAG_TREE);
        out.extend_from_slice(&json);
        out
    }

    /// Deserialise from stored bytes (with or without ED F1 prefix).
    pub fn deserialise(data: &[u8]) -> Result<Self, Error> {
        let json = if data.starts_with(&TYPE_TAG_TREE) {
            &data[2..]
        } else {
            data
        };
        Ok(serde_json::from_slice(json)?)
    }

    /// Compute the object hash: SHA256(ED F1 | tree JSON bytes).
    #[cfg(test)]
    pub fn hash(&self) -> Hash {
        Hash::compute(&self.serialise())
    }

    /// Return entries sorted by name (normal trees only).
    pub fn sorted_entries(entries: Vec<TreeEntry>) -> Vec<TreeEntry> {
        let mut v = entries;
        v.sort_by(|a, b| a.name().cmp(b.name()));
        v
    }

    /// Compute `mtime` for a tree entry pointing to this tree (max mtime of
    /// all entries; `None` for empty trees).
    pub fn aggregate_mtime(entries: &[TreeEntry]) -> Option<DateTime<Utc>> {
        entries.iter().filter_map(|e| e.mtime().copied()).max()
    }

    /// Compute `size` for a tree entry pointing to this tree (sum of blob sizes).
    pub fn aggregate_size(entries: &[TreeEntry]) -> u64 {
        entries.iter().map(|e| e.size()).sum()
    }

    /// Compute `blob_count` for a tree entry pointing to this tree (sum of blob counts).
    pub fn aggregate_blob_count(entries: &[TreeEntry]) -> u64 {
        entries.iter().map(|e| e.blob_count()).sum()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_roundtrip() {
        let hex = "a3f89b2c1d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a";
        let h = Hash::from_hex(hex).unwrap();
        assert_eq!(h.as_str(), hex);
    }

    #[test]
    fn hash_invalid_length() {
        assert!(Hash::from_hex("abc").is_err());
    }

    #[test]
    fn hash_compute() {
        let h = Hash::compute(b"Hello, world!");
        assert_eq!(h.as_str().len(), 64);
    }

    #[test]
    fn empty_tree_serialise() {
        let t = Tree::Normal { entries: vec![] };
        let bytes = t.serialise();
        // First 2 bytes are the ED F1 type tag.
        assert_eq!(&bytes[..2], &TYPE_TAG_TREE);
        let json = String::from_utf8(bytes[2..].to_vec()).unwrap();
        assert_eq!(json, r#"{"kind":"normal","entries":[]}"#);
    }

    #[test]
    fn tree_deserialise_strips_type_tag() {
        let t = Tree::Normal { entries: vec![] };
        let bytes = t.serialise();
        let t2 = Tree::deserialise(&bytes).unwrap();
        assert!(matches!(t2, Tree::Normal { .. }));
    }

    #[test]
    fn tree_hash_deterministic() {
        let t1 = Tree::Normal { entries: vec![] };
        let t2 = Tree::Normal { entries: vec![] };
        assert_eq!(t1.hash(), t2.hash());
    }
}
