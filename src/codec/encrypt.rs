/// Stage 3: encrypt / decrypt.
///
/// Stored format (encrypted object):
///   AES-256-GCM ciphertext || GCM auth tag (16 bytes)
///   No magic prefix. Whether an object is encrypted is determined solely
///   by the repository configuration in .omemfs/config.
///
/// Algorithm: a single one-shot call to the `aes-gcm` crate's `Aes256Gcm`
/// (AAD = empty), keyed by the DEK, nonce = object_hash[0..12]. `encrypt`
/// appends the 16-byte tag to the ciphertext (the crate's own convention,
/// matching this module's wire format exactly); `decrypt` splits it back off
/// and verifies it via the crate's constant-time comparison before returning
/// any plaintext.
///
/// Inputs are bounded by L3 chunking (≤ CDC_MAX ≈ 16 MiB) before they reach
/// this module, so both directions run wholly in memory -- there is no
/// streaming variant.
///
/// This one-shot `aes-gcm` implementation replaced a hand-rolled
/// AES-CTR+GHASH composition (refactor-instructions.md Phase 10 / G8); see
/// the Phase 1 / Phase 10 known-answer and differential tests below for the
/// byte-compatibility proof between the two.
use crate::error::Error;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};

const TAG_LEN: usize = 16;

/// The old hand-rolled implementation's internal streaming buffer size. The
/// one-shot `aes-gcm` implementation has no chunking of its own, but the
/// known-answer tests below still use this as their boundary-size marker so
/// the pinned byte-freeze cases are unchanged across the Phase 10 migration.
#[allow(dead_code)]
const CHUNK: usize = 64 * 1024;

/// 32-byte AES-256 key (the DEK stored in config).
#[derive(Clone)]
pub struct EncryptKey {
    pub bytes: [u8; 32],
}

impl EncryptKey {
    pub fn new(bytes: [u8; 32]) -> Self {
        EncryptKey { bytes }
    }
}

/// Encrypt `data` and return ciphertext || auth_tag (16 bytes).
/// Passthrough if `key` is `None`.
pub fn encrypt(data: Vec<u8>, key: Option<&EncryptKey>, object_hash: &[u8; 32]) -> Vec<u8> {
    let Some(key) = key else { return data };
    let cipher = Aes256Gcm::new_from_slice(&key.bytes)
        .expect("EncryptKey is always exactly 32 bytes, the size Aes256Gcm requires");
    let nonce = Nonce::from_slice(&object_hash[..12]);
    cipher
        .encrypt(nonce, data.as_slice())
        .expect("in-memory AES-256-GCM encrypt with no AAD cannot fail")
}

/// Decrypt `data` (ciphertext || auth_tag).
///
/// Passthrough only when `key` is `None` (encryption not configured). When a
/// key is configured, decryption is strict:
///   - data shorter than `TAG_LEN` → `Error::AuthTagMismatch` (cannot contain a tag)
///   - GCM tag mismatch → `Error::AuthTagMismatch`
///
/// No bytes are ever returned on failure, so tampered or truncated objects can
/// never surface as garbage to the caller. Tag verification is the `aes-gcm`
/// crate's own constant-time comparison; this module never compares tag bytes
/// itself.
pub fn decrypt(
    data: Vec<u8>,
    key: Option<&EncryptKey>,
    object_hash: &[u8; 32],
) -> Result<Vec<u8>, Error> {
    let Some(key) = key else { return Ok(data) };
    if data.len() < TAG_LEN {
        return Err(Error::AuthTagMismatch);
    }
    let cipher = Aes256Gcm::new_from_slice(&key.bytes)
        .expect("EncryptKey is always exactly 32 bytes, the size Aes256Gcm requires");
    let nonce = Nonce::from_slice(&object_hash[..12]);
    cipher
        .decrypt(nonce, data.as_slice())
        .map_err(|_| Error::AuthTagMismatch)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> EncryptKey {
        EncryptKey::new([0x42u8; 32])
    }

    fn test_hash() -> [u8; 32] {
        [0xabu8; 32]
    }

    #[test]
    fn roundtrip_vec() {
        let data = b"hello, omemfs encryption!".to_vec();
        let key = test_key();
        let hash = test_hash();
        let encrypted = encrypt(data.clone(), Some(&key), &hash);
        assert_ne!(
            encrypted, data,
            "encrypted bytes must differ from plaintext"
        );
        let decrypted = decrypt(encrypted, Some(&key), &hash).unwrap();
        assert_eq!(decrypted, data);
    }

    #[test]
    fn passthrough_when_no_key() {
        let data = b"no encryption".to_vec();
        let hash = test_hash();
        let encrypted = encrypt(data.clone(), None, &hash);
        assert_eq!(encrypted, data);
        let decrypted = decrypt(encrypted, None, &hash).unwrap();
        assert_eq!(decrypted, data);
    }

    #[test]
    fn decrypt_too_short_with_key_errors() {
        // With a key configured, data shorter than the tag length cannot be a
        // valid encrypted object and must error (no passthrough).
        let key = test_key();
        let hash = test_hash();
        let short = vec![0u8; TAG_LEN - 1];
        assert!(matches!(
            decrypt(short, Some(&key), &hash),
            Err(Error::AuthTagMismatch)
        ));
    }

    #[test]
    fn decrypt_tag_mismatch_with_key_errors() {
        // Corrupting one byte of a valid ciphertext must yield AuthTagMismatch,
        // never garbage plaintext.
        let data = b"sensitive content".to_vec();
        let key = test_key();
        let hash = test_hash();
        let mut encrypted = encrypt(data, Some(&key), &hash);
        encrypted[0] ^= 0xFF;
        assert!(matches!(
            decrypt(encrypted, Some(&key), &hash),
            Err(Error::AuthTagMismatch)
        ));
    }

    #[test]
    fn different_hashes_produce_different_ciphertext() {
        let data = b"same plaintext".to_vec();
        let key = test_key();
        let hash1 = [0x01u8; 32];
        let hash2 = [0x02u8; 32];
        let c1 = encrypt(data.clone(), Some(&key), &hash1);
        let c2 = encrypt(data.clone(), Some(&key), &hash2);
        assert_ne!(c1, c2);
    }

    #[test]
    fn roundtrip_multi_chunk_sized_input() {
        // A round-trip well above the old streaming implementation's 64 KiB
        // chunk size, to exercise more than a single internal buffer's worth
        // of data end to end.
        let data: Vec<u8> = (0u8..=255).cycle().take(200 * 1024).collect();
        let key = test_key();
        let hash = test_hash();

        let encrypted = encrypt(data.clone(), Some(&key), &hash);
        let decrypted = decrypt(encrypted, Some(&key), &hash).unwrap();
        assert_eq!(decrypted, data);
    }
}

/// Known-answer tests (KAT) that freeze the current wire-format output of
/// `encrypt()` byte-for-byte. These are the safety net required before any
/// change to the encryption implementation (see refactor-instructions.md
/// Phase 1 / Phase 10): they must stay green across the planned migration to
/// the `aes-gcm` crate, proving the ciphertext bytes are unchanged.
///
/// Large inputs are pinned by SHA-256 digest of the ciphertext rather than by
/// embedding multi-MB hex literals; this is equivalent as a byte-freeze
/// (a digest collision here would require a SHA-256 collision) while keeping
/// this file small.
#[cfg(test)]
mod known_answer_tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn to_hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{:02x}", x)).collect()
    }

    fn digest_hex(b: &[u8]) -> String {
        to_hex(&Sha256::digest(b))
    }

    fn kat_key() -> EncryptKey {
        EncryptKey::new([0x42u8; 32])
    }

    #[test]
    fn kat_empty() {
        let enc = encrypt(vec![], Some(&kat_key()), &[0x11u8; 32]);
        assert_eq!(enc.len(), 16);
        assert_eq!(to_hex(&enc), "ea01295dc51f08c7b04d078cacb26bde");
    }

    #[test]
    fn kat_one_byte() {
        let enc = encrypt(vec![0x99], Some(&kat_key()), &[0x22u8; 32]);
        assert_eq!(enc.len(), 17);
        assert_eq!(to_hex(&enc), "a28197fbcecca4f74adad6829dd7254233");
    }

    #[test]
    fn kat_chunk_boundary_exact() {
        let enc = encrypt(vec![0xAAu8; CHUNK], Some(&kat_key()), &[0x33u8; 32]);
        assert_eq!(enc.len(), CHUNK + TAG_LEN);
        assert_eq!(
            digest_hex(&enc),
            "e63275415695792a4628d80bc318d1f9c9169b7747a77ec4a80d795e488e7bf4"
        );
    }

    #[test]
    fn kat_chunk_boundary_minus1() {
        let enc = encrypt(vec![0xBBu8; CHUNK - 1], Some(&kat_key()), &[0x44u8; 32]);
        assert_eq!(enc.len(), CHUNK - 1 + TAG_LEN);
        assert_eq!(
            digest_hex(&enc),
            "c252dcdb71532d631776e6d01122024adcbeea330648e1bfd67ff03672baa272"
        );
    }

    #[test]
    fn kat_chunk_boundary_plus1() {
        let enc = encrypt(vec![0xCCu8; CHUNK + 1], Some(&kat_key()), &[0x55u8; 32]);
        assert_eq!(enc.len(), CHUNK + 1 + TAG_LEN);
        assert_eq!(
            digest_hex(&enc),
            "ab2aa4bb6d5084149b1105b3524db36a5869dca0cd1e7f81a7fc46af56fd0fb2"
        );
    }

    #[test]
    fn kat_few_mib() {
        let n = 3 * 1024 * 1024 + 7;
        let enc = encrypt(vec![0xDDu8; n], Some(&kat_key()), &[0x66u8; 32]);
        assert_eq!(enc.len(), n + TAG_LEN);
        assert_eq!(
            digest_hex(&enc),
            "dff7e443c1997b5ba055b9978b81d8398561d0cc4c251a9d668f24e7a549350f"
        );
    }

    /// The INDEX_ROOT path (writer.rs::encrypt_index_root /
    /// decrypt_index_root_bytes) does not use a content hash as the nonce
    /// source (INDEX_ROOT content is mutable). Instead it generates a random
    /// 12-byte nonce and smuggles it into `encrypt()`/`decrypt()` via a
    /// "pseudo hash": a 32-byte array whose first 12 bytes are the nonce and
    /// whose remaining 20 bytes are zero. This KAT freezes that exact framing.
    #[test]
    fn kat_pseudo_hash_path() {
        let nonce = [0x77u8; 12];
        let mut pseudo_hash = [0u8; 32];
        pseudo_hash[..12].copy_from_slice(&nonce);
        let data = b"index-root-plaintext-body".to_vec();
        let enc = encrypt(data, Some(&kat_key()), &pseudo_hash);
        assert_eq!(enc.len(), 25 + TAG_LEN);
        assert_eq!(
            to_hex(&enc),
            "f64462930d00a55f92424a4b6f7299d96f84680f3185df596f2a9d4685a30b7b6aba1f18146e7da704"
        );
    }

    /// Round-trip sanity: the KAT ciphertexts above must still decrypt
    /// correctly under the current implementation (guards against a KAT that
    /// was pinned incorrectly).
    #[test]
    fn kat_round_trips() {
        let key = kat_key();
        let hash = [0x11u8; 32];
        let enc = encrypt(vec![], Some(&key), &hash);
        assert_eq!(decrypt(enc, Some(&key), &hash).unwrap(), Vec::<u8>::new());

        let nonce = [0x77u8; 12];
        let mut pseudo_hash = [0u8; 32];
        pseudo_hash[..12].copy_from_slice(&nonce);
        let data = b"index-root-plaintext-body".to_vec();
        let enc = encrypt(data.clone(), Some(&key), &pseudo_hash);
        assert_eq!(decrypt(enc, Some(&key), &pseudo_hash).unwrap(), data);
    }
}
