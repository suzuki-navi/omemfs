pub mod bloom;
pub mod index;
pub mod index_root;
pub mod reader;
pub mod root_pointer;
pub mod writer;

use std::path::{Path, PathBuf};

use crate::codec::encrypt::EncryptKey;
use crate::error::Error;
use crate::object::Hash;
use crate::store::ObjectStore;

/// Context string used to derive the index root name on encrypted remotes.
/// Encoded as ASCII bytes. Domain-separated from content-object storage keys
/// (which HMAC over a 32-byte SHA-256 logical hash). See design/02
/// "Index root name derivation".
pub const INDEX_ROOT_CONTEXT: &[u8] = b"omemfs:index-root:v1";

/// Fixed file name for the index root on unencrypted remotes.
pub const INDEX_ROOT_FILE: &str = "INDEX_ROOT";

/// Fixed file name for the CAS lock (both encryption modes).
pub const INDEX_ROOT_LOCK_FILE: &str = "INDEX_ROOT.lock";

/// Derive the encrypted-remote index root name:
/// `HMAC-SHA256(DEK, "omemfs:index-root:v1")`, hex-encoded.
///
/// This is the FINAL storage key (not a logical hash): it must NOT be routed
/// through the logical→storage-key derivation again, which would apply a second
/// HMAC.
pub fn index_root_name(key: &EncryptKey) -> Hash {
    crate::store::local::hmac_sha256_msg(&key.bytes, INDEX_ROOT_CONTEXT)
}

/// Resolve the physical path of the index root under `remote_base`.
///
/// - Encrypted (`key = Some`): `objects/<n[0..2]>/<n[2..4]>/<n[4..6]>/<n[6..64]>`
///   where `n = index_root_name(key)`. Same sharding as content objects.
/// - Unencrypted (`key = None`): the fixed file `<remote_base>/INDEX_ROOT`.
pub fn index_root_path(remote_base: &Path, key: Option<&EncryptKey>) -> PathBuf {
    match key {
        None => remote_base.join(INDEX_ROOT_FILE),
        Some(k) => {
            let n = index_root_name(k);
            let hex = n.as_str();
            remote_base
                .join("objects")
                .join(&hex[0..2])
                .join(&hex[2..4])
                .join(&hex[4..6])
                .join(&hex[6..])
        }
    }
}

/// Path of the CAS lock file. Always the fixed name `INDEX_ROOT.lock`,
/// regardless of encryption mode (design/02, design/03).
pub fn index_root_lock_path(remote_base: &Path) -> PathBuf {
    remote_base.join(INDEX_ROOT_LOCK_FILE)
}

/// Decrypt INDEX_ROOT bytes (`nonce || ciphertext || tag`) when `key` is set.
/// When `key` is `None`, the bytes are returned unchanged.
///
/// INDEX_ROOT content is mutable, so unlike content objects it cannot use a
/// content hash as the nonce source. Instead a random 12-byte nonce is stored
/// alongside the ciphertext, and smuggled into `codec::encrypt::decrypt()` via
/// a "pseudo hash": a 32-byte array whose first 12 bytes are the nonce and
/// whose remaining 20 bytes are zero (see `writer.rs::encrypt_index_root` for
/// the matching encrypt side).
///
/// Single shared implementation for `PackWriter`/`PackReader`
/// (`decrypt_index_root` methods), `commands::cat`, and `commands::pack` --
/// previously each had its own byte-identical copy of this logic.
pub fn decrypt_index_root_bytes(raw: &[u8], key: Option<&EncryptKey>) -> Result<Vec<u8>, Error> {
    let Some(key) = key else {
        return Ok(raw.to_vec());
    };
    if raw.len() < 12 {
        return Err(Error::InvalidObject(
            "INDEX_ROOT too short to contain nonce".to_string(),
        ));
    }
    let nonce: [u8; 12] = raw[..12].try_into().unwrap();
    let ciphertext = &raw[12..];
    let mut pseudo_hash = [0u8; 32];
    pseudo_hash[..12].copy_from_slice(&nonce);
    crate::codec::encrypt::decrypt(ciphertext.to_vec(), Some(key), &pseudo_hash)
}

/// Fetch a content-addressed L6 object (delta / hot / cold shard index file,
/// or Bloom filter) as plaintext, consulting `objcache` first and falling
/// back to fetching + decrypting from `remote` on a miss (caching the
/// plaintext into `objcache` for next time).
///
/// Shared byte-level primitive underneath `load_index_file` (below) and the
/// read path's Bloom filter loader (`PackReader::load_bloom_filter`) --
/// both index files and the Bloom filter are immutable, content-addressed
/// `ED E2`/`ED E4` objects cached the same way; only the final deserialise
/// type differs.
pub fn load_cached_plaintext(
    remote: &dyn crate::store::ObjectStore,
    objcache: &crate::store::local::LocalStore,
    remote_key: Option<&EncryptKey>,
    hash: &Hash,
) -> Result<Vec<u8>, Error> {
    use std::io::Read as _;
    if objcache.exists(hash)? {
        // Objcache hit: read plaintext directly, no remote fetch needed.
        let mut r = objcache.open_read(hash)?;
        let mut buf = Vec::new();
        r.read_to_end(&mut buf).map_err(Error::Io)?;
        Ok(buf)
    } else {
        // Cache miss: fetch from remote, decrypt, and store as plaintext.
        let mut r = remote.open_read(hash)?;
        let mut stored = Vec::new();
        r.read_to_end(&mut stored).map_err(Error::Io)?;
        let plaintext = crate::codec::encrypt::decrypt(stored, remote_key, hash.as_bytes_array())?;
        // Write plaintext to objcache so subsequent calls hit the cache.
        let mut cursor = std::io::Cursor::new(&plaintext);
        objcache.write_from(hash, &mut cursor)?;
        Ok(plaintext)
    }
}

/// Load index file `hash` (delta / hot / cold shard, remote magic `ED E2`) as
/// plaintext, consulting `objcache` first and falling back to fetching +
/// decrypting from `remote` on a miss (caching the plaintext into `objcache`
/// for next time).
///
/// Single shared implementation for `PackReader::load_index_file` and
/// `PackWriter::load_index_file`, which were byte-identical (refactor-
/// instructions.md E9) -- both structs hold the same field shapes
/// (`remote: Box<dyn ObjectStore>`, `objcache: LocalStore`,
/// `remote_key: Option<EncryptKey>`), so this takes them directly rather
/// than requiring a shared struct.
pub fn load_index_file(
    remote: &dyn crate::store::ObjectStore,
    objcache: &crate::store::local::LocalStore,
    remote_key: Option<&EncryptKey>,
    hash: &Hash,
) -> Result<index::IndexFile, Error> {
    let raw = load_cached_plaintext(remote, objcache, remote_key, hash)?;
    index::IndexFile::deserialise(&raw)
}

#[cfg(test)]
mod derivation_tests {
    use super::*;
    use crate::codec::encrypt::EncryptKey;
    use crate::store::local::LocalStore;

    #[test]
    fn index_root_name_is_deterministic_and_64_hex() {
        let key = EncryptKey::new([7u8; 32]);
        let a = index_root_name(&key);
        let b = index_root_name(&key);
        assert_eq!(a, b, "derivation must be deterministic");
        assert_eq!(a.as_str().len(), 64, "must be 64 hex chars");
        assert!(a.as_str().chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn index_root_name_differs_across_deks() {
        let a = index_root_name(&EncryptKey::new([1u8; 32]));
        let b = index_root_name(&EncryptKey::new([2u8; 32]));
        assert_ne!(a, b, "different DEKs must produce different names");
    }

    #[test]
    fn index_root_name_differs_from_storage_key_of_logical_hash() {
        // The index root name must not collide with the storage key of an
        // arbitrary content object's logical hash under the same DEK. The two
        // use different HMAC messages (a 21-byte context string vs a 32-byte
        // logical hash), so they must differ.
        let key = EncryptKey::new([9u8; 32]);
        let logical = Hash::compute(b"some content object");
        let store = LocalStore::for_remote_encrypted("/tmp/does-not-matter", key.clone());
        let storage_key = store.storage_key_of(&logical);
        let ir_name = index_root_name(&key);
        assert_ne!(ir_name, storage_key);
    }

    #[test]
    fn encrypted_path_is_sharded_under_objects() {
        let key = EncryptKey::new([3u8; 32]);
        let base = std::path::Path::new("/remote");
        let p = index_root_path(base, Some(&key));
        let rel = p.strip_prefix("/remote/objects").unwrap();
        let comps: Vec<_> = rel.components().collect();
        assert_eq!(comps.len(), 4, "objects/<2>/<2>/<2>/<58>");
        assert_eq!(comps[3].as_os_str().len(), 58);
    }

    #[test]
    fn unencrypted_path_is_fixed_file() {
        let base = std::path::Path::new("/remote");
        let p = index_root_path(base, None);
        assert_eq!(p, base.join("INDEX_ROOT"));
    }
}
