/// Stage 5: index file (ED E2) — read / write.
///
/// All index files (delta, hot, cold shards) share this binary format:
///
/// magic       : 2 bytes  (ED E2)
/// version     : 1 byte   (0x01)
/// reserved    : 1 byte   (0x00)
/// entry_count : 4 bytes  (big-endian)
/// entries     : IndexEntry × entry_count  (sorted ascending by hash)
///
/// Entry format (tag selects variant):
///   tag  : 1 byte
///   hash : 32 bytes
///
///   [tag = 0x01: inline]
///     data_length : 1 byte
///     data        : data_length bytes  (encrypted bytes)
///
///   [tag = 0x02: pack]
///     pack_hash : 32 bytes
///     offset    : 4 bytes  (big-endian)
///     length    : 4 bytes  (big-endian)
///
///   [tag = 0x03: standalone]
///     (no additional fields)
use crate::error::Error;
use crate::object::Hash;

pub const MAGIC: [u8; 2] = [0xED, 0xE2];
pub const VERSION: u8 = 0x01;

const TAG_INLINE: u8 = 0x01;
const TAG_PACK: u8 = 0x02;
const TAG_STANDALONE: u8 = 0x03;

// ---------------------------------------------------------------------------
// IndexEntry
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineEntry {
    pub hash: Hash,
    /// Already-encrypted bytes (< 256 B).
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackEntry {
    pub hash: Hash,
    pub pack_hash: Hash,
    /// Byte offset within the pack file body (after the 2-byte magic).
    pub offset: u32,
    pub length: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StandaloneEntry {
    pub hash: Hash,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexEntry {
    Inline(InlineEntry),
    Pack(PackEntry),
    Standalone(StandaloneEntry),
}

impl IndexEntry {
    pub fn hash(&self) -> &Hash {
        match self {
            IndexEntry::Inline(e) => &e.hash,
            IndexEntry::Pack(e) => &e.hash,
            IndexEntry::Standalone(e) => &e.hash,
        }
    }
}

// ---------------------------------------------------------------------------
// IndexFile
// ---------------------------------------------------------------------------

/// An in-memory representation of an index file.
/// Entries are always kept sorted ascending by hash (hex string order).
#[derive(Debug, Clone)]
pub struct IndexFile {
    entries: Vec<IndexEntry>,
}

impl IndexFile {
    pub fn new() -> Self {
        IndexFile {
            entries: Vec::new(),
        }
    }

    /// Add an entry and keep the list sorted, deduplicating by hash.
    ///
    /// If an entry with the same hash already exists, it is REPLACED by the
    /// new one rather than inserted alongside it. Content-addressed objects
    /// with the same hash are the same logical object, so pushing the same
    /// hash twice (e.g. a dedup race, or a caller that does not itself
    /// guarantee push-once-per-hash) must not create two entries: doing so
    /// would produce two equal-hash entries, which violates the strict
    /// ascending-by-hash invariant `deserialise` checks and would make the
    /// serialised index file unreadable (refactor-instructions.md C1).
    pub fn push(&mut self, entry: IndexEntry) {
        let pos = self
            .entries
            .partition_point(|e| e.hash().as_str() < entry.hash().as_str());
        if pos < self.entries.len() && self.entries[pos].hash() == entry.hash() {
            self.entries[pos] = entry;
        } else {
            self.entries.insert(pos, entry);
        }
    }

    pub fn entries(&self) -> &[IndexEntry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Binary-search for an entry by hash. Returns `None` if not found.
    pub fn find(&self, hash: &Hash) -> Option<&IndexEntry> {
        let pos = self
            .entries
            .partition_point(|e| e.hash().as_str() < hash.as_str());
        if pos < self.entries.len() && self.entries[pos].hash() == hash {
            Some(&self.entries[pos])
        } else {
            None
        }
    }

    // -----------------------------------------------------------------------
    // Serialise
    // -----------------------------------------------------------------------

    /// Serialise to the binary wire format (plaintext — caller handles encrypt).
    pub fn serialise(&self) -> Result<Vec<u8>, Error> {
        let count = self.entries.len();
        if count > u32::MAX as usize {
            return Err(Error::Other(
                "index entry count exceeds u32::MAX".to_string(),
            ));
        }
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&MAGIC);
        buf.push(VERSION);
        buf.push(0x00); // reserved
        buf.extend_from_slice(&(count as u32).to_be_bytes());

        for entry in &self.entries {
            match entry {
                IndexEntry::Inline(e) => {
                    buf.push(TAG_INLINE);
                    buf.extend_from_slice(e.hash.as_bytes_array());
                    buf.push(e.data.len() as u8);
                    buf.extend_from_slice(&e.data);
                }
                IndexEntry::Pack(e) => {
                    buf.push(TAG_PACK);
                    buf.extend_from_slice(e.hash.as_bytes_array());
                    buf.extend_from_slice(e.pack_hash.as_bytes_array());
                    buf.extend_from_slice(&e.offset.to_be_bytes());
                    buf.extend_from_slice(&e.length.to_be_bytes());
                }
                IndexEntry::Standalone(e) => {
                    buf.push(TAG_STANDALONE);
                    buf.extend_from_slice(e.hash.as_bytes_array());
                }
            }
        }
        Ok(buf)
    }

    // -----------------------------------------------------------------------
    // Deserialise
    // -----------------------------------------------------------------------

    /// Deserialise from the binary wire format (plaintext — caller handles decrypt).
    pub fn deserialise(data: &[u8]) -> Result<Self, Error> {
        let mut pos = 0;

        // magic
        if data.len() < 8 {
            return Err(Error::InvalidObject("index file too short".to_string()));
        }
        if data[0..2] != MAGIC {
            return Err(Error::InvalidObject(format!(
                "index file bad magic {:02X} {:02X}",
                data[0], data[1]
            )));
        }
        pos += 2;

        // version
        let version = data[pos];
        if version != VERSION {
            return Err(Error::InvalidObject(format!(
                "index file unknown version {}",
                version
            )));
        }
        pos += 1;
        pos += 1; // reserved

        let entry_count =
            u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        pos += 4;

        let mut entries: Vec<IndexEntry> = Vec::with_capacity(entry_count);

        for _ in 0..entry_count {
            if pos >= data.len() {
                return Err(Error::InvalidObject("index file truncated".to_string()));
            }
            let tag = data[pos];
            pos += 1;

            // hash: 32 bytes
            if pos + 32 > data.len() {
                return Err(Error::InvalidObject(
                    "index file truncated in hash".to_string(),
                ));
            }
            let hash = Hash::from_bytes(data[pos..pos + 32].try_into().unwrap());
            pos += 32;

            match tag {
                TAG_INLINE => {
                    if pos >= data.len() {
                        return Err(Error::InvalidObject("inline entry truncated".to_string()));
                    }
                    let data_len = data[pos] as usize;
                    pos += 1;
                    if pos + data_len > data.len() {
                        return Err(Error::InvalidObject(
                            "inline entry data truncated".to_string(),
                        ));
                    }
                    let entry_data = data[pos..pos + data_len].to_vec();
                    pos += data_len;
                    entries.push(IndexEntry::Inline(InlineEntry {
                        hash,
                        data: entry_data,
                    }));
                }
                TAG_PACK => {
                    if pos + 32 + 4 + 4 > data.len() {
                        return Err(Error::InvalidObject("pack entry truncated".to_string()));
                    }
                    let pack_hash = Hash::from_bytes(data[pos..pos + 32].try_into().unwrap());
                    pos += 32;
                    let offset = u32::from_be_bytes([
                        data[pos],
                        data[pos + 1],
                        data[pos + 2],
                        data[pos + 3],
                    ]);
                    pos += 4;
                    let length = u32::from_be_bytes([
                        data[pos],
                        data[pos + 1],
                        data[pos + 2],
                        data[pos + 3],
                    ]);
                    pos += 4;
                    entries.push(IndexEntry::Pack(PackEntry {
                        hash,
                        pack_hash,
                        offset,
                        length,
                    }));
                }
                TAG_STANDALONE => {
                    entries.push(IndexEntry::Standalone(StandaloneEntry { hash }));
                }
                other => {
                    return Err(Error::InvalidObject(format!(
                        "unknown index entry tag 0x{:02X}",
                        other
                    )));
                }
            }
        }

        // Verify entries are sorted.
        for i in 1..entries.len() {
            if entries[i].hash().as_str() <= entries[i - 1].hash().as_str() {
                return Err(Error::InvalidObject("index entries not sorted".to_string()));
            }
        }

        Ok(IndexFile { entries })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_hash(seed: u8) -> Hash {
        Hash::from_bytes([seed; 32])
    }

    #[test]
    fn roundtrip_empty() {
        let idx = IndexFile::new();
        let bytes = idx.serialise().unwrap();
        let idx2 = IndexFile::deserialise(&bytes).unwrap();
        assert_eq!(idx2.len(), 0);
    }

    #[test]
    fn roundtrip_inline() {
        let mut idx = IndexFile::new();
        let h = make_hash(0x01);
        idx.push(IndexEntry::Inline(InlineEntry {
            hash: h.clone(),
            data: vec![0xAB, 0xCD],
        }));
        let bytes = idx.serialise().unwrap();
        let idx2 = IndexFile::deserialise(&bytes).unwrap();
        assert_eq!(idx2.len(), 1);
        match &idx2.entries()[0] {
            IndexEntry::Inline(e) => {
                assert_eq!(e.hash, h);
                assert_eq!(e.data, vec![0xAB, 0xCD]);
            }
            _ => panic!("expected inline"),
        }
    }

    #[test]
    fn roundtrip_pack() {
        let mut idx = IndexFile::new();
        let h = make_hash(0x02);
        let ph = make_hash(0xFF);
        idx.push(IndexEntry::Pack(PackEntry {
            hash: h.clone(),
            pack_hash: ph.clone(),
            offset: 100,
            length: 200,
        }));
        let bytes = idx.serialise().unwrap();
        let idx2 = IndexFile::deserialise(&bytes).unwrap();
        assert_eq!(idx2.len(), 1);
        match &idx2.entries()[0] {
            IndexEntry::Pack(e) => {
                assert_eq!(e.hash, h);
                assert_eq!(e.pack_hash, ph);
                assert_eq!(e.offset, 100);
                assert_eq!(e.length, 200);
            }
            _ => panic!("expected pack"),
        }
    }

    #[test]
    fn roundtrip_standalone() {
        let mut idx = IndexFile::new();
        let h = make_hash(0x03);
        idx.push(IndexEntry::Standalone(StandaloneEntry { hash: h.clone() }));
        let bytes = idx.serialise().unwrap();
        let idx2 = IndexFile::deserialise(&bytes).unwrap();
        assert_eq!(idx2.len(), 1);
        match &idx2.entries()[0] {
            IndexEntry::Standalone(e) => assert_eq!(e.hash, h),
            _ => panic!("expected standalone"),
        }
    }

    #[test]
    fn roundtrip_mixed_sorted() {
        let mut idx = IndexFile::new();
        // Insert in reverse order to verify push() sorts correctly.
        idx.push(IndexEntry::Standalone(StandaloneEntry {
            hash: make_hash(0x30),
        }));
        idx.push(IndexEntry::Inline(InlineEntry {
            hash: make_hash(0x10),
            data: vec![1],
        }));
        idx.push(IndexEntry::Pack(PackEntry {
            hash: make_hash(0x20),
            pack_hash: make_hash(0xAA),
            offset: 0,
            length: 50,
        }));

        let bytes = idx.serialise().unwrap();
        let idx2 = IndexFile::deserialise(&bytes).unwrap();
        assert_eq!(idx2.len(), 3);
        // Must be sorted ascending by hash hex.
        let hashes: Vec<&str> = idx2.entries().iter().map(|e| e.hash().as_str()).collect();
        let mut sorted = hashes.clone();
        sorted.sort();
        assert_eq!(hashes, sorted);
    }

    #[test]
    fn find_hit_and_miss() {
        let mut idx = IndexFile::new();
        let h1 = make_hash(0x10);
        let h2 = make_hash(0x20);
        idx.push(IndexEntry::Standalone(StandaloneEntry { hash: h1.clone() }));
        assert!(idx.find(&h1).is_some());
        assert!(idx.find(&h2).is_none());
    }

    #[test]
    fn bad_magic_rejected() {
        let mut bytes = IndexFile::new().serialise().unwrap();
        bytes[0] = 0x00;
        assert!(IndexFile::deserialise(&bytes).is_err());
    }

    #[test]
    fn unsorted_rejected() {
        // Build a valid index then swap two entries in the raw bytes to break sort order.
        let mut idx = IndexFile::new();
        idx.push(IndexEntry::Standalone(StandaloneEntry {
            hash: make_hash(0x10),
        }));
        idx.push(IndexEntry::Standalone(StandaloneEntry {
            hash: make_hash(0x20),
        }));
        let mut bytes = idx.serialise().unwrap();
        // Swap the hash bytes of the two standalone entries (each entry = 1 tag + 32 hash).
        // Header is 8 bytes. Entry size = 33 bytes (tag=1, hash=32).
        let base = 8;
        let entry_size = 33; // tag(1) + hash(32)
        // Swap hash bytes of entry 0 and entry 1
        for i in 1..33 {
            bytes.swap(base + i, base + entry_size + i);
        }
        assert!(IndexFile::deserialise(&bytes).is_err());
    }

    // refactor-instructions.md C1: pushing the same hash twice must not
    // create two entries (deserialise rejects equal-hash neighbours as an
    // unsorted index). Un-ignored now that push() dedupes by hash.
    #[test]
    fn duplicate_hash_push_round_trips() {
        let mut idx = IndexFile::new();
        let h = make_hash(0x42);
        idx.push(IndexEntry::Standalone(StandaloneEntry { hash: h.clone() }));
        idx.push(IndexEntry::Standalone(StandaloneEntry { hash: h.clone() }));
        let bytes = idx.serialise().unwrap();
        let idx2 = IndexFile::deserialise(&bytes)
            .expect("index file with a duplicate-hash entry must still deserialise");
        assert_eq!(
            idx2.len(),
            1,
            "pushing the same hash twice must not create two entries"
        );
    }

    #[test]
    fn duplicate_hash_push_replaces_not_skips() {
        // The second push for a hash must win (last-write-wins), not be
        // silently dropped -- a caller may push a more complete entry the
        // second time (e.g. Standalone -> Pack after consolidation).
        let mut idx = IndexFile::new();
        let h = make_hash(0x50);
        idx.push(IndexEntry::Standalone(StandaloneEntry { hash: h.clone() }));
        idx.push(IndexEntry::Pack(PackEntry {
            hash: h.clone(),
            pack_hash: make_hash(0xAA),
            offset: 10,
            length: 20,
        }));
        assert_eq!(idx.len(), 1);
        match &idx.entries()[0] {
            IndexEntry::Pack(e) => {
                assert_eq!(e.hash, h);
                assert_eq!(e.offset, 10);
            }
            other => panic!("expected the second push (Pack) to win, got {other:?}"),
        }
    }
}
