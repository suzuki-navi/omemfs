/// Codec pipeline: serialize → compress → encrypt → store (write)
///                 load → decrypt → decompress → deserialize (read)
///
/// Each stage is in its own submodule:
///   Stage 1 (serialize):  src/object.rs  — Tree::serialise / deserialise, Hash::compute
///   Stage 2 (compress):   src/codec/compress.rs
///   Stage 3 (encrypt):    src/codec/encrypt.rs
///   Stage 4 (store):      src/store/             — ObjectStore trait and backends
pub mod chunk;
pub mod compress;
pub mod encrypt;
pub mod pack;

use crate::{dtimer_l4, dtimer_l5, dtimer_l7};

use std::io::{self, Read};

use crate::error::Error;

/// Encode serialised bytes for storage: compress then encrypt.
/// `key` is `None` when encryption is not configured.
/// `object_hash` is the 32-byte SHA-256 hash of the serialised object; used
/// as the AES-GCM nonce source. Ignored when `key` is `None`.
pub fn encode(
    serialised: &[u8],
    key: Option<&encrypt::EncryptKey>,
    object_hash: &[u8; 32],
) -> Vec<u8> {
    let compressed = {
        let _t = dtimer_l4!("compress");
        compress::compress(serialised)
    };
    let _t = dtimer_l5!("encrypt");
    // `encrypt` is a no-op passthrough when `key` is `None`, so a single call
    // covers both the encrypted and unencrypted cases.
    encrypt::encrypt(compressed, key, object_hash)
}

/// Decode stored bytes to serialised bytes: decrypt then decompress.
/// `key` is `None` when encryption is not configured.
/// `object_hash` is the 32-byte SHA-256 hash used as the AES-GCM nonce source.
/// Ignored when `key` is `None`.
pub fn decode(
    stored: &[u8],
    key: Option<&encrypt::EncryptKey>,
    object_hash: &[u8; 32],
) -> Result<Vec<u8>, Error> {
    let decrypted = {
        let _t = dtimer_l5!("decrypt");
        encrypt::decrypt(stored.to_vec(), key, object_hash)?
    };
    let _t = dtimer_l4!("decompress");
    compress::decompress(&decrypted)
}

/// Convenience: encode `data` and write it to `store` at `hash`.
/// Uses the `write_from` streaming interface internally.
pub fn store_write(
    store: &dyn crate::store::ObjectStore,
    hash: &crate::object::Hash,
    data: &[u8],
    key: Option<&encrypt::EncryptKey>,
) -> Result<(), Error> {
    // Encode-skip optimisation (design/02 — L2 "Write path"): object storage is
    // content-addressed and immutable, so an object that already exists never
    // needs to be re-encoded or re-written. Checking existence here skips the
    // CPU cost of compress + encrypt for objects already present.
    //
    // For a PackWriter the `exists` check is the cheap Bloom + index path:
    // "definitely absent" → write; "maybe present" → confirm via index / HEAD.
    // We do not add a second per-object remote round-trip beyond what `exists`
    // already performs.
    if store.exists(hash)? {
        return Ok(());
    }
    let encoded = encode(data, key, hash.as_bytes_array());
    let mut cursor = io::Cursor::new(encoded);
    store.write_from(hash, &mut cursor)
}

/// Convenience: read from `store` at `hash` and decode the bytes.
/// Uses the `open_read` streaming interface internally.
pub fn store_read(
    store: &dyn crate::store::ObjectStore,
    hash: &crate::object::Hash,
    key: Option<&encrypt::EncryptKey>,
) -> Result<Vec<u8>, Error> {
    let stored = {
        let _t = dtimer_l7!("read_object ({})", store.store_name());
        let mut reader = store.open_read(hash)?;
        let mut stored = Vec::new();
        reader.read_to_end(&mut stored).map_err(Error::Io)?;
        stored
    };
    decode(&stored, key, hash.as_bytes_array())
}

/// Ensure `hash` is present in `local` as decoded (plaintext) bytes, fetching
/// and decoding it from `src` (using `key`) on a miss, and return the decoded
/// bytes either way (read back from `local` on a hit, since the caller needs
/// the content immediately in both cases -- e.g. to `Tree::deserialise` it).
///
/// Consolidates what were two byte-identical copies (refactor-instructions.md
/// E4) in commands/expand.rs and commands/ls.rs. This is NOT a universal
/// "ensure present" helper: `pull.rs`'s `LazyTreeStore::ensure_local` and
/// `cat.rs`'s `ensure_in_local_cache` deliberately do NOT read/decode on a
/// cache hit (they only need presence, and calling this instead would add an
/// unwanted decode on every hit in a traversal hot path); `clone.rs`'s
/// `ensure_tree_in_local` deliberately attempts a local read first and
/// falls back to `src` on ANY failure including corruption (self-healing),
/// not just on absence. Those three are intentionally separate.
pub fn ensure_local_then_read(
    src: &dyn crate::store::ObjectStore,
    local: &dyn crate::store::ObjectStore,
    hash: &crate::object::Hash,
    key: Option<&encrypt::EncryptKey>,
) -> Result<Vec<u8>, Error> {
    if !local.exists(hash)? {
        let data = store_read(src, hash, key)?;
        store_write(local, hash, &data, None)?;
        Ok(data)
    } else {
        store_read(local, hash, None)
    }
}
