/// Stage 5: Bloom filter (ED E4) — read / write.
///
/// Stored as a CAS object (content-addressed) with magic ED E4.
/// Encrypted with AES-256-GCM; nonce derived from file hash (hash[0..12]).
///
/// Plaintext format:
///   magic             : 2 bytes  (ED E4)
///   version           : 1 byte   (0x01)
///   num_hash_functions: 1 byte   (recommended: 7)
///   num_bits          : 8 bytes  (big-endian u64)
///   element_count     : 8 bytes  (big-endian u64)
///   bits              : ceil(num_bits / 8) bytes
use crate::error::Error;
use crate::object::Hash;

pub const MAGIC: [u8; 2] = [0xED, 0xE4];
pub const VERSION: u8 = 0x01;

/// Recommended number of hash functions for ~1% false-positive rate.
pub const DEFAULT_NUM_HASH_FUNCTIONS: u8 = 7;

// Fixed header: 2 (magic) + 1 (version) + 1 (num_hash_functions)
//             + 8 (num_bits) + 8 (element_count) = 20 bytes
const HEADER_LEN: usize = 20;

// ---------------------------------------------------------------------------
// BloomFilter
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct BloomFilter {
    pub num_hash_functions: u8,
    pub num_bits: u64,
    pub element_count: u64,
    bits: Vec<u8>,
}

impl BloomFilter {
    /// Create a new empty Bloom filter sized for `expected_elements` at the
    /// given false-positive rate. Uses `num_hash_functions` hash rounds.
    ///
    /// `num_bits` is computed as: ceil(-n * ln(p) / ln(2)^2)
    /// where n = expected_elements, p = false_positive_rate.
    pub fn new(expected_elements: u64, false_positive_rate: f64, num_hash_functions: u8) -> Self {
        let m = optimal_num_bits(expected_elements, false_positive_rate);
        let byte_count = (m as usize).div_ceil(8);
        BloomFilter {
            num_hash_functions,
            num_bits: m,
            element_count: 0,
            bits: vec![0u8; byte_count],
        }
    }

    /// Create directly from raw parts (used in deserialise).
    pub fn from_parts(
        num_hash_functions: u8,
        num_bits: u64,
        element_count: u64,
        bits: Vec<u8>,
    ) -> Result<Self, Error> {
        let expected_bytes = (num_bits as usize).div_ceil(8);
        if bits.len() != expected_bytes {
            return Err(Error::InvalidObject(format!(
                "Bloom filter bits length {} != expected {}",
                bits.len(),
                expected_bytes
            )));
        }
        Ok(BloomFilter {
            num_hash_functions,
            num_bits,
            element_count,
            bits,
        })
    }

    /// Insert a hash into the filter.
    pub fn insert(&mut self, hash: &Hash) {
        for i in 0..self.num_hash_functions {
            let bit = bit_index(hash.as_bytes_array(), i, self.num_bits);
            set_bit(&mut self.bits, bit);
        }
        self.element_count += 1;
    }

    /// Test membership.
    /// Returns `true` if the hash *may* be present (possible false positive).
    /// Returns `false` if the hash is *definitely absent*.
    pub fn may_contain(&self, hash: &Hash) -> bool {
        for i in 0..self.num_hash_functions {
            let bit = bit_index(hash.as_bytes_array(), i, self.num_bits);
            if !get_bit(&self.bits, bit) {
                return false;
            }
        }
        true
    }

    // -----------------------------------------------------------------------
    // Serialise
    // -----------------------------------------------------------------------

    pub fn serialise(&self) -> Vec<u8> {
        let mut buf: Vec<u8> = Vec::with_capacity(HEADER_LEN + self.bits.len());
        buf.extend_from_slice(&MAGIC);
        buf.push(VERSION);
        buf.push(self.num_hash_functions);
        buf.extend_from_slice(&self.num_bits.to_be_bytes());
        buf.extend_from_slice(&self.element_count.to_be_bytes());
        buf.extend_from_slice(&self.bits);
        buf
    }

    // -----------------------------------------------------------------------
    // Deserialise
    // -----------------------------------------------------------------------

    pub fn deserialise(data: &[u8]) -> Result<Self, Error> {
        if data.len() < HEADER_LEN {
            return Err(Error::InvalidObject(format!(
                "Bloom filter too short: {} bytes",
                data.len()
            )));
        }

        if data[0..2] != MAGIC {
            return Err(Error::InvalidObject(format!(
                "Bloom filter bad magic {:02X} {:02X}",
                data[0], data[1]
            )));
        }

        let version = data[2];
        if version != VERSION {
            return Err(Error::InvalidObject(format!(
                "Bloom filter unknown version {}",
                version
            )));
        }

        let num_hash_functions = data[3];
        let num_bits = u64::from_be_bytes(data[4..12].try_into().unwrap());
        let element_count = u64::from_be_bytes(data[12..20].try_into().unwrap());

        let expected_bytes = (num_bits as usize).div_ceil(8);
        if data.len() != HEADER_LEN + expected_bytes {
            return Err(Error::InvalidObject(format!(
                "Bloom filter bits size mismatch: expected {} bytes, got {}",
                expected_bytes,
                data.len() - HEADER_LEN
            )));
        }

        let bits = data[HEADER_LEN..].to_vec();
        BloomFilter::from_parts(num_hash_functions, num_bits, element_count, bits)
    }
}

// ---------------------------------------------------------------------------
// Bit helpers
// ---------------------------------------------------------------------------

fn set_bit(bits: &mut [u8], index: u64) {
    let byte = (index / 8) as usize;
    let bit = (index % 8) as u32;
    bits[byte] |= 1u8 << bit;
}

fn get_bit(bits: &[u8], index: u64) -> bool {
    let byte = (index / 8) as usize;
    let bit = (index % 8) as u32;
    (bits[byte] >> bit) & 1 == 1
}

/// Derive the k-th bit index for a hash.
/// Uses siphash-like double-hashing: index_k = (h1 + k * h2) mod num_bits
/// where h1 and h2 are derived from the hash bytes.
fn bit_index(hash_bytes: &[u8; 32], k: u8, num_bits: u64) -> u64 {
    // Use first 8 bytes as h1, next 8 bytes as h2.
    let h1 = u64::from_le_bytes(hash_bytes[0..8].try_into().unwrap());
    let h2 = u64::from_le_bytes(hash_bytes[8..16].try_into().unwrap());
    h1.wrapping_add((k as u64).wrapping_mul(h2)) % num_bits
}

/// Optimal number of bits: ceil(-n * ln(p) / ln(2)^2)
fn optimal_num_bits(n: u64, p: f64) -> u64 {
    let ln2_sq = std::f64::consts::LN_2 * std::f64::consts::LN_2;
    let m = -(n as f64) * p.ln() / ln2_sq;
    m.ceil() as u64
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
    fn insert_and_may_contain() {
        let mut bf = BloomFilter::new(100, 0.01, DEFAULT_NUM_HASH_FUNCTIONS);
        let h1 = make_hash(0x01);
        let h2 = make_hash(0x02);
        assert!(!bf.may_contain(&h1));
        bf.insert(&h1);
        assert!(bf.may_contain(&h1));
        // h2 not inserted — should not be present (no false positive for this trivial case)
        let _ = bf.may_contain(&h2); // just must not panic
    }

    #[test]
    fn no_false_negatives() {
        let mut bf = BloomFilter::new(1000, 0.01, DEFAULT_NUM_HASH_FUNCTIONS);
        let hashes: Vec<Hash> = (0u8..=255).map(make_hash).collect();
        for h in &hashes {
            bf.insert(h);
        }
        for h in &hashes {
            assert!(bf.may_contain(h), "false negative for {:?}", h);
        }
    }

    #[test]
    fn roundtrip_empty() {
        let bf = BloomFilter::new(100, 0.01, DEFAULT_NUM_HASH_FUNCTIONS);
        let bytes = bf.serialise();
        let bf2 = BloomFilter::deserialise(&bytes).unwrap();
        assert_eq!(bf2.num_bits, bf.num_bits);
        assert_eq!(bf2.element_count, 0);
    }

    #[test]
    fn roundtrip_with_data() {
        let mut bf = BloomFilter::new(1000, 0.01, DEFAULT_NUM_HASH_FUNCTIONS);
        for i in 0u8..100 {
            bf.insert(&make_hash(i));
        }
        let bytes = bf.serialise();
        let bf2 = BloomFilter::deserialise(&bytes).unwrap();
        assert_eq!(bf2.element_count, 100);
        for i in 0u8..100 {
            assert!(bf2.may_contain(&make_hash(i)));
        }
    }

    #[test]
    fn bad_magic_rejected() {
        let bf = BloomFilter::new(10, 0.01, DEFAULT_NUM_HASH_FUNCTIONS);
        let mut bytes = bf.serialise();
        bytes[0] = 0x00;
        assert!(BloomFilter::deserialise(&bytes).is_err());
    }

    #[test]
    fn truncated_rejected() {
        let bf = BloomFilter::new(10, 0.01, DEFAULT_NUM_HASH_FUNCTIONS);
        let bytes = bf.serialise();
        assert!(BloomFilter::deserialise(&bytes[..HEADER_LEN - 1]).is_err());
    }
}
