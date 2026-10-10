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
// Filter sizing (design/02_storage_format.md "Filter sizing")
// ---------------------------------------------------------------------------

/// Target false-positive rate for all dynamically-sized filters. Fixed (not a
/// per-repository knob) because `DEFAULT_NUM_HASH_FUNCTIONS = 7` is the
/// optimal hash count only at this `p` -- see design/02 "Rejected
/// alternatives".
pub const FP_RATE: f64 = 0.01;

/// Floor on any capacity `capacity_for` produces, and the fallback capacity
/// used when a fresh filter must be built with no element-count hint at all.
/// Matches the size the old fixed-100_000 constant always produced, so a
/// repository that has never run `omemfs pack` sees no behavioural change.
pub const MIN_CAPACITY: u64 = 100_000;

/// Margin multiplier applied to the observed element count before rounding up
/// to the nearest 1-2-5 ladder step. A filter built for `capacity_for(n)`
/// stays at its designed false-positive rate until the element count it was
/// built for grows by 50%.
const CAPACITY_MARGIN_NUMERATOR: u128 = 3;
const CAPACITY_MARGIN_DENOMINATOR: u128 = 2;

/// The designed capacity for a filter that must hold `element_count` elements
/// at `FP_RATE`, per design/02 "Filter sizing":
///
/// ```text
/// capacity_for(n) = ladder_ceil(max(n, 1) * 3 / 2), floored at MIN_CAPACITY
/// ladder = {1, 2, 5} x 10^k   (100_000, 200_000, 500_000, 1_000_000, ...)
/// ```
///
/// Monotonically non-decreasing in `element_count`. Never returns less than
/// `MIN_CAPACITY`, even for `element_count == 0`.
pub fn capacity_for(element_count: u64) -> u64 {
    let n = element_count.max(1) as u128;
    let margin = (n * CAPACITY_MARGIN_NUMERATOR) / CAPACITY_MARGIN_DENOMINATOR;
    ladder_ceil(margin as u64).max(MIN_CAPACITY)
}

/// Round `value` up to the nearest step of the {1, 2, 5} x 10^k ladder.
/// Returns 0 only for `value == 0` (not reachable from `capacity_for`, which
/// always floors its result at `MIN_CAPACITY`).
fn ladder_ceil(value: u64) -> u64 {
    if value == 0 {
        return 0;
    }
    let mut power: u64 = 1;
    loop {
        for &step in &[1u64, 2, 5] {
            if let Some(candidate) = step.checked_mul(power)
                && candidate >= value
            {
                return candidate;
            }
        }
        match power.checked_mul(10) {
            Some(next) => power = next,
            // u64 overflow: no realistic element count reaches this; return
            // the largest representable ladder step rather than panic.
            None => return 5u64.checked_mul(power).unwrap_or(u64::MAX),
        }
    }
}

/// Build an empty filter sized for `element_count` elements at `FP_RATE`,
/// using `DEFAULT_NUM_HASH_FUNCTIONS` hash rounds. This is the only way a
/// fresh (non-loaded) filter should be constructed on either the push path
/// (`PackWriter::load_remote_bloom`) or `omemfs pack` (`regenerate_bloom`),
/// so the sizing formula lives in exactly one place.
///
/// Always returns a filter with `num_bits >= 1`: `capacity_for` never returns
/// less than `MIN_CAPACITY`, which at `FP_RATE = 0.01` always yields a
/// strictly positive bit count, but the guard below is kept as a defensive
/// backstop against future changes to those constants -- `may_contain` and
/// `insert` divide by `num_bits` and must never see zero.
pub fn sized_for(element_count: u64) -> BloomFilter {
    let capacity = capacity_for(element_count);
    let bf = BloomFilter::new(capacity, FP_RATE, DEFAULT_NUM_HASH_FUNCTIONS);
    if bf.num_bits >= 1 {
        return bf;
    }
    // Defensive fallback: force a single usable bit rather than let a future
    // constant change reintroduce the `% 0` panic in `may_contain`.
    BloomFilter::from_parts(DEFAULT_NUM_HASH_FUNCTIONS, 1, 0, vec![0u8]).expect(
        "a 1-bit filter with a 1-byte bit array is always structurally valid",
    )
}

/// Invert `capacity_for`'s bit-count computation: recover the capacity that a
/// filter with `num_bits` bits was designed for at `FP_RATE`. Used for
/// fill-rate reporting (`omemfs cat <bloom-hash>`, `omemfs stats`) to show how
/// far a loaded filter's `element_count` is from the capacity its current
/// size implies, without any format change (design/02 "Fill-rate reporting").
///
/// Returns 0 for `num_bits == 0` (no meaningful capacity).
pub fn designed_capacity(num_bits: u64) -> u64 {
    if num_bits == 0 {
        return 0;
    }
    let ln2_sq = std::f64::consts::LN_2 * std::f64::consts::LN_2;
    let n = (num_bits as f64) * ln2_sq / (-FP_RATE.ln());
    n.round().max(0.0) as u64
}

/// Estimate the current false-positive rate of a filter holding
/// `element_count` elements in `num_bits` bits with `num_hash_functions` hash
/// rounds, using the standard Bloom filter approximation:
///
/// ```text
/// p ~= (1 - e^(-k*n/m))^k
/// ```
///
/// Used for fill-rate reporting alongside `designed_capacity`. Unlike a
/// fill-rate computed from the actual bit array, this only needs the three
/// scalar fields already carried by the filter (or recorded in
/// `PackDetail`), so it works even where the physical bit array is not
/// available (`omemfs stats`'s `pack_detail` record).
///
/// Returns `1.0` (worst case) for `num_bits == 0`, a degenerate case that
/// should not occur in practice since `sized_for` always yields `num_bits >=
/// 1`.
pub fn estimated_fp_rate(element_count: u64, num_bits: u64, num_hash_functions: u8) -> f64 {
    if num_bits == 0 {
        return 1.0;
    }
    let k = num_hash_functions as f64;
    let n = element_count as f64;
    let m = num_bits as f64;
    (1.0 - (-k * n / m).exp()).powf(k)
}

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

    // -----------------------------------------------------------------------
    // Filter sizing (design/02_storage_format.md "Filter sizing")
    // -----------------------------------------------------------------------

    #[test]
    fn capacity_for_boundary_values() {
        // n=0 and n=1 both floor at MIN_CAPACITY.
        assert_eq!(capacity_for(0), MIN_CAPACITY);
        assert_eq!(capacity_for(1), MIN_CAPACITY);
        // 99_999 * 1.5 = 149_998.5 -> floor 149_998 -> ladder-ceil to 200_000.
        assert_eq!(capacity_for(99_999), 200_000);
        // 100_000 * 1.5 = 150_000 -> ladder-ceil to 200_000.
        assert_eq!(capacity_for(100_000), 200_000);
        // 100_001 * 1.5 = 150_001.5 -> floor 150_001 -> ladder-ceil to 200_000.
        assert_eq!(capacity_for(100_001), 200_000);
        // 276_000 * 1.5 = 414_000 -> ladder-ceil to 500_000.
        assert_eq!(capacity_for(276_000), 500_000);
    }

    #[test]
    fn capacity_for_never_below_min_capacity() {
        for n in [0u64, 1, 2, 10, 1_000] {
            assert!(
                capacity_for(n) >= MIN_CAPACITY,
                "capacity_for({n}) must never be below MIN_CAPACITY"
            );
        }
    }

    #[test]
    fn capacity_for_is_monotonically_non_decreasing() {
        let samples: Vec<u64> = vec![
            0, 1, 10, 100, 1_000, 50_000, 99_999, 100_000, 100_001, 150_000, 276_000, 500_000,
            1_000_000, 1_999_999, 2_000_000, 10_000_000,
        ];
        let mut prev = 0u64;
        for &n in &samples {
            let cap = capacity_for(n);
            assert!(
                cap >= prev,
                "capacity_for({n}) = {cap} must be >= capacity_for(prev) = {prev}"
            );
            prev = cap;
        }
    }

    #[test]
    fn capacity_for_lands_on_the_ladder() {
        // Every result must be exactly {1, 2, 5} x 10^k for some k, never an
        // arbitrary intermediate value.
        for n in [0u64, 1, 42, 12_345, 276_000, 3_000_000, 42_000_000] {
            let cap = capacity_for(n);
            let mut ok = false;
            let mut power = 1u64;
            'outer: for _ in 0..20 {
                for &step in &[1u64, 2, 5] {
                    if step.saturating_mul(power) == cap {
                        ok = true;
                        break 'outer;
                    }
                }
                power = power.saturating_mul(10);
            }
            assert!(ok, "capacity_for({n}) = {cap} is not on the 1-2-5 ladder");
        }
    }

    #[test]
    fn sized_for_never_panics_and_has_at_least_one_bit() {
        for n in [0u64, 1, 2, 100, 99_999, 100_000, 100_001, 276_000, 10_000_000] {
            let bf = sized_for(n);
            assert!(bf.num_bits >= 1, "sized_for({n}).num_bits must be >= 1");
            // Must not panic on insert/may_contain (the original bug this
            // guards against: `% num_bits` divide-by-zero when num_bits == 0).
            let h = make_hash((n % 256) as u8);
            bf.may_contain(&h);
            let mut bf2 = bf;
            bf2.insert(&h);
            assert!(bf2.may_contain(&h));
        }
    }

    #[test]
    fn sized_for_uses_fp_rate_and_default_hash_functions() {
        let bf = sized_for(500_000);
        assert_eq!(bf.num_hash_functions, DEFAULT_NUM_HASH_FUNCTIONS);
        // Sanity: num_bits must roughly match the direct optimal-bits formula
        // for capacity_for(500_000) at FP_RATE.
        let expected_capacity = capacity_for(500_000);
        let direct = BloomFilter::new(expected_capacity, FP_RATE, DEFAULT_NUM_HASH_FUNCTIONS);
        assert_eq!(bf.num_bits, direct.num_bits);
    }

    #[test]
    fn designed_capacity_round_trips_through_sized_for() {
        // designed_capacity(sized_for(n).num_bits) must recover capacity_for(n)
        // (the capacity that num_bits was actually built for), up to a small
        // rounding tolerance from the ceil() in the underlying bit-count formula.
        for n in [0u64, 1, 100_000, 276_000, 3_000_000] {
            let capacity = capacity_for(n);
            let bf = sized_for(n);
            let recovered = designed_capacity(bf.num_bits);
            let diff = capacity.abs_diff(recovered);
            assert!(
                diff <= 1,
                "designed_capacity({}) = {} must be within 1 of capacity_for({}) = {}",
                bf.num_bits,
                recovered,
                n,
                capacity
            );
        }
    }

    #[test]
    fn designed_capacity_zero_for_zero_bits() {
        assert_eq!(designed_capacity(0), 0);
    }

    #[test]
    fn estimated_fp_rate_is_near_target_at_designed_load() {
        // A filter loaded to exactly its ladder capacity_for(n) (i.e. before
        // the 1.5x margin is exhausted) should estimate a false-positive rate
        // in the neighbourhood of FP_RATE, not wildly higher or lower.
        let n = 500_000u64;
        let bf = sized_for(n);
        let p = estimated_fp_rate(n, bf.num_bits, bf.num_hash_functions);
        assert!(
            p > 0.0001 && p < 0.05,
            "estimated_fp_rate should be near FP_RATE=0.01 at designed load, got {p}"
        );
    }

    #[test]
    fn estimated_fp_rate_increases_as_elements_grow_past_capacity() {
        let bf = sized_for(100_000);
        let p_at_capacity = estimated_fp_rate(100_000, bf.num_bits, bf.num_hash_functions);
        let p_over_capacity = estimated_fp_rate(500_000, bf.num_bits, bf.num_hash_functions);
        assert!(
            p_over_capacity > p_at_capacity,
            "false-positive rate must increase as more elements are inserted \
             into a filter of fixed size"
        );
    }

    #[test]
    fn estimated_fp_rate_zero_bits_is_worst_case() {
        assert_eq!(estimated_fp_rate(10, 0, DEFAULT_NUM_HASH_FUNCTIONS), 1.0);
    }
}
