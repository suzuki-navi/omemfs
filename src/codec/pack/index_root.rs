/// Stage 5: INDEX_ROOT (ED E3) — read / write.
///
/// Stored at the fixed key `<prefix>/INDEX_ROOT` in the remote backend.
/// When encryption is configured, the on-disk format is:
///   nonce (12 bytes) || AES-256-GCM(DEK, nonce, plaintext) || GCM tag (16 bytes)
///
/// Plaintext format:
///   magic            : 2 bytes  (ED E3)
///   version          : 1 byte   (0x01)
///   reserved         : 1 byte   (0x00)
///   remote_root      : 32 bytes (tree hash; all-zero if never pushed)
///   hot_hash         : 32 bytes (hash of hot index file)
///   bloom_hash       : 32 bytes (hash of Bloom filter; all-zero if none)
///   cold_prefix_bits : 1 byte
///   reserved2        : 3 bytes
///   delta_count      : 2 bytes  (big-endian)
///   padding          : 2 bytes
///   delta_hash[0..N] : 32 × delta_count bytes  (newest first)
///   cold_shard[0..2^cold_prefix_bits] : 32 × 2^cold_prefix_bits bytes
use crate::error::Error;
use crate::object::Hash;

pub const MAGIC: [u8; 2] = [0xED, 0xE3];
pub const VERSION: u8 = 0x01;

// Fixed header size up to (and including) padding, without delta or cold arrays.
// 2 (magic) + 1 (version) + 1 (reserved) + 32 (remote_root) + 32 (hot_hash)
// + 32 (bloom_hash) + 1 (cold_prefix_bits) + 3 (reserved2) + 2 (delta_count)
// + 2 (padding) = 108 bytes
const FIXED_HEADER_LEN: usize = 108;

// ---------------------------------------------------------------------------
// IndexRoot
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRoot {
    /// Tree hash of remote state. All-zero bytes if never pushed.
    pub remote_root: [u8; 32],
    /// Hash of the hot index file. All-zero if not yet generated.
    pub hot_hash: [u8; 32],
    /// Hash of the Bloom filter file. All-zero if not yet generated.
    pub bloom_hash: [u8; 32],
    /// Number of hash prefix bits used to address cold shards.
    pub cold_prefix_bits: u8,
    /// Delta index hashes, newest first.
    pub delta_hashes: Vec<[u8; 32]>,
    /// Cold shard hashes, one per 2^cold_prefix_bits address slots.
    pub cold_shards: Vec<[u8; 32]>,
}

impl IndexRoot {
    /// Create a zero-initialised INDEX_ROOT (for a fresh repository).
    pub fn new_empty() -> Self {
        IndexRoot {
            remote_root: [0u8; 32],
            hot_hash: [0u8; 32],
            bloom_hash: [0u8; 32],
            cold_prefix_bits: 0,
            delta_hashes: Vec::new(),
            cold_shards: vec![[0u8; 32]; 1], // 2^0 = 1 slot
        }
    }

    /// Number of cold shard slots: 2^cold_prefix_bits.
    pub fn cold_shard_count(&self) -> usize {
        1usize << (self.cold_prefix_bits as usize)
    }

    // -----------------------------------------------------------------------
    // Serialise
    // -----------------------------------------------------------------------

    pub fn serialise(&self) -> Result<Vec<u8>, Error> {
        let expected_shards = self.cold_shard_count();
        if self.cold_shards.len() != expected_shards {
            return Err(Error::Other(format!(
                "cold_shards length {} does not match 2^cold_prefix_bits ({})",
                self.cold_shards.len(),
                expected_shards
            )));
        }
        let delta_count = self.delta_hashes.len();
        if delta_count > u16::MAX as usize {
            return Err(Error::Other("delta_count exceeds u16::MAX".to_string()));
        }

        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&MAGIC);
        buf.push(VERSION);
        buf.push(0x00); // reserved
        buf.extend_from_slice(&self.remote_root);
        buf.extend_from_slice(&self.hot_hash);
        buf.extend_from_slice(&self.bloom_hash);
        buf.push(self.cold_prefix_bits);
        buf.extend_from_slice(&[0u8; 3]); // reserved2
        buf.extend_from_slice(&(delta_count as u16).to_be_bytes());
        buf.extend_from_slice(&[0u8; 2]); // padding

        for dh in &self.delta_hashes {
            buf.extend_from_slice(dh);
        }
        for cs in &self.cold_shards {
            buf.extend_from_slice(cs);
        }
        Ok(buf)
    }

    // -----------------------------------------------------------------------
    // Deserialise
    // -----------------------------------------------------------------------

    pub fn deserialise(data: &[u8]) -> Result<Self, Error> {
        if data.len() < FIXED_HEADER_LEN {
            return Err(Error::InvalidObject(format!(
                "INDEX_ROOT too short: {} bytes",
                data.len()
            )));
        }

        let mut pos = 0;

        if data[0..2] != MAGIC {
            return Err(Error::InvalidObject(format!(
                "INDEX_ROOT bad magic {:02X} {:02X}",
                data[0], data[1]
            )));
        }
        pos += 2;

        let version = data[pos];
        if version != VERSION {
            return Err(Error::InvalidObject(format!(
                "INDEX_ROOT unknown version {}",
                version
            )));
        }
        pos += 1;
        pos += 1; // reserved

        let remote_root: [u8; 32] = data[pos..pos + 32].try_into().unwrap();
        pos += 32;
        let hot_hash: [u8; 32] = data[pos..pos + 32].try_into().unwrap();
        pos += 32;
        let bloom_hash: [u8; 32] = data[pos..pos + 32].try_into().unwrap();
        pos += 32;

        let cold_prefix_bits = data[pos];
        pos += 1;
        pos += 3; // reserved2

        let delta_count = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;
        pos += 2; // padding

        let cold_shard_count = 1usize << (cold_prefix_bits as usize);

        let needed = delta_count * 32 + cold_shard_count * 32;
        if pos + needed > data.len() {
            return Err(Error::InvalidObject(format!(
                "INDEX_ROOT truncated: need {} more bytes, have {}",
                needed,
                data.len() - pos
            )));
        }

        let mut delta_hashes: Vec<[u8; 32]> = Vec::with_capacity(delta_count);
        for _ in 0..delta_count {
            delta_hashes.push(data[pos..pos + 32].try_into().unwrap());
            pos += 32;
        }

        let mut cold_shards: Vec<[u8; 32]> = Vec::with_capacity(cold_shard_count);
        for _ in 0..cold_shard_count {
            cold_shards.push(data[pos..pos + 32].try_into().unwrap());
            pos += 32;
        }

        Ok(IndexRoot {
            remote_root,
            hot_hash,
            bloom_hash,
            cold_prefix_bits,
            delta_hashes,
            cold_shards,
        })
    }

    // -----------------------------------------------------------------------
    // Helpers for Hash wrappers
    // -----------------------------------------------------------------------

    pub fn remote_root_hash(&self) -> Option<Hash> {
        if self.remote_root == [0u8; 32] {
            None
        } else {
            Some(Hash::from_bytes(self.remote_root))
        }
    }

    pub fn hot_hash_opt(&self) -> Option<Hash> {
        if self.hot_hash == [0u8; 32] {
            None
        } else {
            Some(Hash::from_bytes(self.hot_hash))
        }
    }

    pub fn bloom_hash_opt(&self) -> Option<Hash> {
        if self.bloom_hash == [0u8; 32] {
            None
        } else {
            Some(Hash::from_bytes(self.bloom_hash))
        }
    }

    pub fn delta_hashes_as_hashes(&self) -> Vec<Hash> {
        self.delta_hashes
            .iter()
            .map(|b| Hash::from_bytes(*b))
            .collect()
    }

    pub fn cold_shard_hash(&self, prefix: usize) -> Option<Hash> {
        let h = self.cold_shards.get(prefix)?;
        if *h == [0u8; 32] {
            None
        } else {
            Some(Hash::from_bytes(*h))
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_empty() {
        let root = IndexRoot::new_empty();
        let bytes = root.serialise().unwrap();
        let root2 = IndexRoot::deserialise(&bytes).unwrap();
        assert_eq!(root, root2);
        assert_eq!(root2.cold_shard_count(), 1);
        assert_eq!(root2.delta_hashes.len(), 0);
    }

    #[test]
    fn roundtrip_with_deltas_and_shards() {
        let mut root = IndexRoot::new_empty();
        root.remote_root = [0xAB; 32];
        root.hot_hash = [0x01; 32];
        root.bloom_hash = [0x02; 32];
        root.cold_prefix_bits = 2; // 4 shards
        root.cold_shards = vec![[0x10; 32], [0x11; 32], [0x12; 32], [0x13; 32]];
        root.delta_hashes = vec![[0xD1; 32], [0xD2; 32]]; // newest first

        let bytes = root.serialise().unwrap();
        let root2 = IndexRoot::deserialise(&bytes).unwrap();
        assert_eq!(root, root2);
        assert_eq!(root2.cold_shard_count(), 4);
        assert_eq!(root2.delta_hashes.len(), 2);
    }

    #[test]
    fn bad_magic_rejected() {
        let root = IndexRoot::new_empty();
        let mut bytes = root.serialise().unwrap();
        bytes[0] = 0x00;
        assert!(IndexRoot::deserialise(&bytes).is_err());
    }

    #[test]
    fn wrong_shard_count_rejected() {
        let mut root = IndexRoot::new_empty();
        root.cold_prefix_bits = 1; // expects 2 shards
        root.cold_shards = vec![[0u8; 32]; 3]; // wrong: 3 shards
        assert!(root.serialise().is_err());
    }

    #[test]
    fn truncated_rejected() {
        let root = IndexRoot::new_empty();
        let bytes = root.serialise().unwrap();
        assert!(IndexRoot::deserialise(&bytes[..FIXED_HEADER_LEN - 1]).is_err());
    }

    #[test]
    fn helper_methods() {
        let mut root = IndexRoot::new_empty();
        assert!(root.remote_root_hash().is_none());
        assert!(root.hot_hash_opt().is_none());
        assert!(root.bloom_hash_opt().is_none());

        root.remote_root = [0xAA; 32];
        assert!(root.remote_root_hash().is_some());

        root.delta_hashes = vec![[0xDD; 32]];
        let hs = root.delta_hashes_as_hashes();
        assert_eq!(hs.len(), 1);
        assert_eq!(hs[0].as_bytes_array(), &[0xDD; 32]);
    }
}
