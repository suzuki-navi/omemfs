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

/// Returns true if `name` is exactly the metadata-directory name `.omemfs`
/// (no hyphen). See design/09_reserved_names.md, "`.omemfs` exclusion scope".
pub fn is_metadata_dir_name(name: &str) -> bool {
    name == ".omemfs"
}

/// Returns true if `name` belongs to the reserved `.omemfs-` namespace, in
/// either the standalone/dir-interior form (`.omemfs-<kind>`) or the
/// file-adjacent form (`<name>.omemfs-<kind>`). See design/09_reserved_names.md.
///
/// This is the single definition of the reserved-name substring predicate,
/// shared by two different call sites for two different purposes:
/// - `src/scan.rs` uses it (via its own re-export) to decide which local
///   working-tree names to *exclude* from a scan before a tree is ever
///   built.
/// - `validate_entry_name` below uses it to *reject* a tree that somehow
///   contains one of these names as an entry anyway -- see "Reserved names
///   in remote tree entries (untrusted input)" in design/09_reserved_names.md
///   for why a remote tree entry gets no forward-compatibility exception
///   here, unlike the local-scan side.
///
/// Defined in `object.rs` (not `scan.rs`) so that `validate_entry_name` can
/// call it without introducing a dependency from `object.rs` (a low-level
/// module with no knowledge of commands/scanning) onto `scan.rs` (a
/// higher-level module that already depends on `object.rs`); `scan.rs`
/// re-exports it under its existing public name to keep every existing
/// caller of `scan::is_reserved_name` unchanged.
pub fn is_reserved_name(name: &str) -> bool {
    name.contains(".omemfs-")
}

// ---------------------------------------------------------------------------
// Conflict-artifact filename suffixes
// ---------------------------------------------------------------------------

/// Suffixes of the three `omemfs conflict` resolution-aid helper files
/// (`<path>.omemfs-conflict-{base,local,remote}`).
///
/// Defined here (not in `commands::conflict`) for the same reason
/// [`is_reserved_name`] is defined here rather than in `scan.rs`: `scan.rs`
/// (a low-level module) needs this list to exclude conflict-helper files from
/// a working-tree scan, and a dependency from `scan.rs` onto a specific
/// command module (`commands::conflict`) would be backwards -- `scan.rs`
/// should not need to know which *command* consumes these files, only that
/// they are not user content. `commands::conflict` re-exports this under its
/// existing name (`pub(crate) use`) so none of its own call sites need to
/// change.
pub(crate) const CONFLICT_SUFFIX_BASE: &str = ".omemfs-conflict-base";
pub(crate) const CONFLICT_SUFFIX_LOCAL: &str = ".omemfs-conflict-local";
pub(crate) const CONFLICT_SUFFIX_REMOTE: &str = ".omemfs-conflict-remote";
pub(crate) const CONFLICT_SUFFIXES: &[&str] = &[
    CONFLICT_SUFFIX_BASE,
    CONFLICT_SUFFIX_LOCAL,
    CONFLICT_SUFFIX_REMOTE,
];

/// Suffix of the `omemfs conflict` metadata sidecar
/// (`<path>.omemfs-conflict-meta`). See `commands::conflict::CONFLICT_META_
/// SUFFIX`'s own doc comment (re-exported from here) for what it records.
pub(crate) const CONFLICT_META_SUFFIX: &str = ".omemfs-conflict-meta";

/// L6: every filename suffix that identifies a piece of omemfs-internal
/// conflict bookkeeping alongside a base path -- the three resolution-aid
/// helpers ([`CONFLICT_SUFFIXES`]) plus the [`CONFLICT_META_SUFFIX`] metadata
/// sidecar. None of these are user content: they exist only to support
/// `omemfs conflict`. Anything that needs to recognise/remove "any conflict
/// residue for this path" should use this list -- or [`is_conflict_artifact`]
/// -- rather than keeping its own separate copy that can silently drift out
/// of sync with this one.
pub(crate) const CONFLICT_ARTIFACT_SUFFIXES: &[&str] = &[
    CONFLICT_SUFFIX_BASE,
    CONFLICT_SUFFIX_LOCAL,
    CONFLICT_SUFFIX_REMOTE,
    CONFLICT_META_SUFFIX,
];

/// Returns `true` if `name` ends with any suffix in
/// [`CONFLICT_ARTIFACT_SUFFIXES`] -- i.e. `name` is omemfs-internal conflict
/// bookkeeping (a helper or the metadata sidecar), never user content.
pub(crate) fn is_conflict_artifact(name: &str) -> bool {
    CONFLICT_ARTIFACT_SUFFIXES.iter().any(|s| name.ends_with(s))
}

/// Validate a single tree-entry name before it is ever joined onto a
/// filesystem path (e.g. `base_dir.join(entry.name())` in `pull`/`tree_ops`).
///
/// Rejects:
/// - the empty string
/// - `.` or `..` (the reserved "self" / "parent" names)
/// - any name containing `/` (hierarchy is already encoded by the tree
///   structure itself; a single entry name must never smuggle in a path
///   separator, absolute or relative)
/// - any name containing a NUL byte (not a valid filesystem name component)
/// - the reserved metadata-directory name `.omemfs`, or any name in the
///   reserved `.omemfs-` namespace (see [`is_metadata_dir_name`] and
///   [`is_reserved_name`]) -- a tree read from the remote is untrusted
///   input, and accepting either would let a hostile or corrupted tree
///   overwrite the local metadata directory or be materialised as a fake
///   stub marker / conflict helper once written into the working tree. See
///   design/09_reserved_names.md, "Reserved names in remote tree entries
///   (untrusted input)".
///
/// **Exception**: the exact standalone name `.omemfs-filter` is accepted
/// even though it matches the `.omemfs-` namespace predicate above.
/// `.omemfs-filter` is a *tracked* configuration file (design/09
/// "`.omemfs-filter` -- filter configuration"): `scan_and_store` includes it
/// in every tree it builds (and `omemfs clone --new` writes a default one
/// at the repository root), so a well-formed tree routinely -- not just
/// hypothetically -- contains a `.omemfs-filter` entry. This is the exact
/// opposite of `.omemfs-stub` / `.omemfs-conflict-*`, which
/// `scan_and_store` always excludes and which therefore never legitimately
/// appear in a tree at all. Rejecting `.omemfs-filter` here would make
/// every `push`/`pull`/`clone`/`ls` on a repository with a filter file --
/// i.e. nearly every repository -- fail with `Error::InvalidObject`
/// (regression fixed by this exception; see design/09_reserved_names.md,
/// "The `.omemfs-filter` exception", for the full rationale, including why
/// this carve-out does not weaken the security rationale for rejecting
/// every other name in the namespace).
///
/// This exception is applied here, in `validate_entry_name`, rather than by
/// changing [`is_reserved_name`] itself: `is_reserved_name` is also
/// re-exported by `src/scan.rs` and used there (together with
/// `is_known_reserved_name`, which already special-cases `.omemfs-filter`
/// as tracked rather than excluded) to decide which local names are
/// candidates for the "unknown reserved name" forward-compatibility
/// warning during a working-tree scan. Special-casing `.omemfs-filter`
/// inside `is_reserved_name` itself would make `is_unknown_reserved_name`
/// stop recognising a file-adjacent look-alike like `x.omemfs-filter` (not
/// a real filter file under this design, since `.omemfs-filter` is always
/// standalone/dir-interior -- see design/09) as belonging to the reserved
/// namespace at all, silently changing scan-side behaviour that has nothing
/// to do with this remote-tree trust boundary. Keeping the exception local
/// to `validate_entry_name` limits its effect to exactly the problem being
/// fixed: what `Tree::deserialise` accepts as a tree-entry name.
///
/// `\` (backslash) is deliberately **allowed**: this project targets Linux
/// (see `CLAUDE.md`), where `\` is not a path separator but an ordinary,
/// completely legal filename byte. It must be stored and restored verbatim.
/// Correspondingly, no path-string normalisation anywhere in this codebase
/// may treat `\` as a separator (e.g. rewrite it to `/`) — doing so would
/// silently turn a single legitimate entry name into a multi-component path.
///
/// See design/01_object_model.md, "Invariants" #5.
fn validate_entry_name(name: &str) -> Result<(), Error> {
    if name.is_empty() {
        return Err(Error::InvalidObject(
            "tree entry name must not be empty".to_string(),
        ));
    }
    if name == "." || name == ".." {
        return Err(Error::InvalidObject(format!(
            "tree entry name must not be {name:?}"
        )));
    }
    if name.contains('/') {
        return Err(Error::InvalidObject(format!(
            "tree entry name must not contain a path separator: {name:?}"
        )));
    }
    if name.contains('\0') {
        return Err(Error::InvalidObject(format!(
            "tree entry name must not contain a NUL byte: {name:?}"
        )));
    }
    if is_metadata_dir_name(name) {
        return Err(Error::InvalidObject(format!(
            "tree entry name must not be the reserved metadata directory name: {name:?}"
        )));
    }
    // `.omemfs-filter` is a tracked file, not an excluded reserved marker --
    // see the "Exception" doc comment above -- so it is carved out of the
    // reserved-namespace rejection below rather than being treated as
    // untrusted-remote-tree evidence like every other `.omemfs-` name.
    if name != ".omemfs-filter" && is_reserved_name(name) {
        return Err(Error::InvalidObject(format!(
            "tree entry name must not be in the reserved .omemfs- namespace: {name:?}"
        )));
    }
    Ok(())
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

    /// Lightweight "is this a tree object?" check: tests only for the `ED F1`
    /// type-tag prefix, with no JSON parsing at all (design/01_object_model.md,
    /// "Tree/blob discrimination"; D13).
    ///
    /// This exists for the handful of call sites that need to decide "tree or
    /// blob?" for an object of otherwise-unknown type -- `cat`, `stats`,
    /// `pull`'s git-dir check, and `transfer`'s child-discovery walk -- before
    /// D13 those call sites answered that question by checking whether
    /// `Tree::deserialise` happened to succeed, which conflates two different
    /// things: "does this have the tree tag" and "does the JSON after the tag
    /// parse." An ordinary blob whose *content* happens to be well-formed tree
    /// JSON (e.g. a file literally containing `{"kind":"normal","entries":[]}`)
    /// has no `ED F1` tag but still parses successfully via `Tree::deserialise`
    /// (which deliberately accepts data with or without the tag, for callers
    /// that already know they are reading a tree) -- `Tree::deserialise`
    /// alone cannot tell that object apart from a real tree. `is_tree_bytes`
    /// can, because a blob's stored bytes can never legitimately begin with an
    /// untagged `ED F1` (`serialise_blob` escapes any blob content that would
    /// otherwise start with an `ED Fx` byte pair, see `blob_needs_escape`
    /// above), so the tag alone is a sound discriminator. Callers still route
    /// a tag-confirmed tree through `Tree::deserialise` afterwards to actually
    /// parse it (and to reject a tagged-but-corrupt tree, exactly as before).
    pub fn is_tree_bytes(data: &[u8]) -> bool {
        data.starts_with(&TYPE_TAG_TREE)
    }

    /// Deserialise from stored bytes (with or without ED F1 prefix).
    ///
    /// Validates every entry name (see [`validate_entry_name`]) before
    /// returning. This is the single point every tree-reading command goes
    /// through (`pull`, `expand`, `clone`, `restore`, `ls`, `stats`, `pack`,
    /// `cat`, and the shared helpers in `tree_ops.rs`), so the check protects
    /// all of them without any command-specific validation (design/01
    /// "Invariants" #5).
    pub fn deserialise(data: &[u8]) -> Result<Self, Error> {
        let json = if data.starts_with(&TYPE_TAG_TREE) {
            &data[2..]
        } else {
            data
        };
        let tree: Tree = serde_json::from_slice(json)?;
        match &tree {
            Tree::Normal { entries } => {
                for entry in entries {
                    validate_entry_name(entry.name())?;
                }
            }
        }
        Ok(tree)
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

    // -----------------------------------------------------------------------
    // `Tree::is_tree_bytes` (D13, design/01_object_model.md "Tree/blob
    // discrimination"): a lightweight ED F1-tag-only check, with no JSON
    // parsing, used by the four "is this a tree?" discriminator call sites
    // (cat.rs, stats.rs, pull.rs, transfer.rs) instead of inferring the
    // answer from whether `Tree::deserialise` happens to succeed.
    // -----------------------------------------------------------------------

    #[test]
    fn is_tree_bytes_true_for_properly_tagged_tree() {
        let t = Tree::Normal { entries: vec![] };
        let bytes = t.serialise();
        assert!(Tree::is_tree_bytes(&bytes));
    }

    #[test]
    fn is_tree_bytes_false_for_untagged_json_that_parses_as_a_tree() {
        // The exact false-positive `Tree::deserialise` alone cannot rule
        // out: this is ordinary blob content (no ED F1 tag) whose bytes
        // happen to be valid tree JSON. `Tree::deserialise` on this data
        // succeeds (it accepts data with or without the tag), but
        // `is_tree_bytes` must say "not a tree" -- it is the presence of
        // the tag, not whether the JSON parses, that answers the
        // discrimination question (D13's fix for the "referenced local
        // object is missing" permanent push failure).
        let data = br#"{"kind":"normal","entries":[]}"#;
        assert!(
            Tree::deserialise(data).is_ok(),
            "sanity check: this data must still parse as a tree via deserialise"
        );
        assert!(!Tree::is_tree_bytes(data));
    }

    #[test]
    fn is_tree_bytes_false_for_untagged_non_json_content() {
        assert!(!Tree::is_tree_bytes(b"hello, world"));
    }

    #[test]
    fn is_tree_bytes_false_for_empty_data() {
        assert!(!Tree::is_tree_bytes(b""));
    }

    #[test]
    fn is_tree_bytes_false_for_a_single_byte_matching_only_the_first_tag_byte() {
        assert!(!Tree::is_tree_bytes(&[0xED]));
    }

    #[test]
    fn is_tree_bytes_false_for_a_different_ed_fx_tag() {
        // ED F0 is the blob escape tag (TYPE_TAG_BLOB), not the tree tag.
        assert!(!Tree::is_tree_bytes(&TYPE_TAG_BLOB));
    }

    #[test]
    fn tree_hash_deterministic() {
        let t1 = Tree::Normal { entries: vec![] };
        let t2 = Tree::Normal { entries: vec![] };
        assert_eq!(t1.hash(), t2.hash());
    }

    // -----------------------------------------------------------------------
    // Entry name validation (design/01_object_model.md, Invariants #5).
    //
    // A hand-built tree with an unsafe entry name is fed straight into
    // `Tree::deserialise`'s JSON parser, bypassing the normal
    // `TreeEntry::Blob { name, .. }` construction path entirely -- this is the
    // only way to exercise validation of an adversarial/corrupted object
    // (real construction sites always derive `name` from a real filesystem
    // entry, which can never be empty, "..", or contain '/').
    // -----------------------------------------------------------------------

    fn tree_json_with_entry_name(name: &str) -> Vec<u8> {
        // Minimal well-formed blob entry, only `name` varies.
        let json = format!(
            r#"{{"kind":"normal","entries":[{{"kind":"blob","name":{},"hash":"{}","mtime":null,"size":0}}]}}"#,
            serde_json::to_string(name).unwrap(),
            "a".repeat(64),
        );
        json.into_bytes()
    }

    fn assert_rejected(name: &str) {
        let data = tree_json_with_entry_name(name);
        let result = Tree::deserialise(&data);
        assert!(
            matches!(result, Err(Error::InvalidObject(_))),
            "entry name {name:?} should be rejected as InvalidObject, got {result:?}"
        );
    }

    #[test]
    fn deserialise_rejects_empty_entry_name() {
        assert_rejected("");
    }

    #[test]
    fn deserialise_rejects_dot_entry_name() {
        assert_rejected(".");
    }

    #[test]
    fn deserialise_rejects_dotdot_entry_name() {
        assert_rejected("..");
    }

    #[test]
    fn deserialise_rejects_path_traversal_entry_name() {
        assert_rejected("../../etc/passwd");
    }

    #[test]
    fn deserialise_rejects_absolute_path_entry_name() {
        assert_rejected("/etc/cron.d/x");
    }

    #[test]
    fn deserialise_accepts_backslash_in_entry_name() {
        // `\` is an ordinary Linux filename byte, not a path separator; it
        // must be stored and restored verbatim (design/01 "Invariants" #5).
        let data = tree_json_with_entry_name("a\\b");
        let tree = Tree::deserialise(&data).unwrap();
        let Tree::Normal { entries } = tree;
        assert_eq!(entries[0].name(), "a\\b");
    }

    #[test]
    fn deserialise_rejects_nul_byte_in_entry_name() {
        assert_rejected("a\0b");
    }

    // -------------------------------------------------------------------
    // Direct `validate_entry_name` unit tests (the function itself, not
    // just its effect through `Tree::deserialise`).
    // -------------------------------------------------------------------

    #[test]
    fn validate_entry_name_accepts_backslash() {
        assert!(validate_entry_name("a\\b").is_ok());
    }

    #[test]
    fn validate_entry_name_rejects_empty() {
        assert!(validate_entry_name("").is_err());
    }

    #[test]
    fn validate_entry_name_rejects_dot() {
        assert!(validate_entry_name(".").is_err());
    }

    #[test]
    fn validate_entry_name_rejects_dotdot() {
        assert!(validate_entry_name("..").is_err());
    }

    #[test]
    fn validate_entry_name_rejects_slash() {
        assert!(validate_entry_name("a/b").is_err());
    }

    #[test]
    fn validate_entry_name_rejects_nul_byte() {
        assert!(validate_entry_name("a\0b").is_err());
    }

    #[test]
    fn deserialise_accepts_ordinary_entry_name() {
        let data = tree_json_with_entry_name("normal-file.txt");
        let tree = Tree::deserialise(&data).unwrap();
        let Tree::Normal { entries } = tree;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name(), "normal-file.txt");
    }

    #[test]
    fn deserialise_rejects_bad_name_on_any_entry_not_just_the_first() {
        // A tree with one valid entry followed by one invalid entry must
        // still be rejected -- validation must not stop at the first entry.
        let json = format!(
            r#"{{"kind":"normal","entries":[{{"kind":"blob","name":"ok.txt","hash":"{}","mtime":null,"size":0}},{{"kind":"symlink","name":"..","target":"x","mtime":null}}]}}"#,
            "b".repeat(64),
        );
        let result = Tree::deserialise(json.as_bytes());
        assert!(matches!(result, Err(Error::InvalidObject(_))));
    }

    // -------------------------------------------------------------------
    // Reserved-name rejection in remote tree entries (design/09
    // "Reserved names in remote tree entries (untrusted input)",
    // design/01 "Invariants" #5).
    //
    // A tree read from the remote is untrusted input: nothing guarantees
    // its entry names were produced by a real `scan_and_store` (which
    // already excludes these names -- see design/09 "Scan exclusions").
    // `Tree::deserialise` must reject any entry whose name falls in the
    // reserved `.omemfs` / `.omemfs-*` namespace, so that materialising a
    // hand-crafted or corrupted tree can never overwrite the metadata
    // directory or be misread as a stub marker / conflict helper.
    // -------------------------------------------------------------------

    #[test]
    fn deserialise_rejects_metadata_directory_entry_name() {
        assert_rejected(".omemfs");
    }

    #[test]
    fn deserialise_rejects_stub_marker_entry_name() {
        assert_rejected("report.pdf.omemfs-stub");
    }

    #[test]
    fn deserialise_rejects_conflict_helper_entry_name() {
        assert_rejected("main.rs.omemfs-conflict-remote");
    }

    #[test]
    fn deserialise_accepts_filter_file_entry_name() {
        // `.omemfs-filter` is a tracked configuration file (design/09
        // "`.omemfs-filter` -- filter configuration"): `scan_and_store`
        // includes it in every tree it builds, so a well-formed tree
        // routinely contains this entry (e.g. the default filter file
        // `omemfs clone --new` writes at the repository root). Unlike
        // `.omemfs-stub`/`.omemfs-conflict-*`, which `scan_and_store` always
        // excludes and which must never appear in a tree, `.omemfs-filter`
        // must be accepted here or every push/pull/clone/ls on essentially
        // any repository would fail (see design/09 "The `.omemfs-filter`
        // exception").
        let data = tree_json_with_entry_name(".omemfs-filter");
        assert!(Tree::deserialise(&data).is_ok());
    }

    #[test]
    fn deserialise_rejects_directory_interior_stub_entry_name() {
        assert_rejected(".omemfs-stub");
    }

    #[test]
    fn deserialise_rejects_unknown_reserved_entry_name() {
        // Even a name that is not a *known* reserved kind (forward
        // compatibility on the local-scan side) is rejected here: a remote
        // tree entry never gets the "unknown reserved name, skip and warn"
        // treatment that the local scan gives it (design/09).
        assert_rejected(".omemfs-some-future-kind");
    }

    #[test]
    fn deserialise_accepts_ordinary_name_not_matching_reserved_pattern_regression() {
        // Existing ordinary names must keep working: no regression from
        // adding the reserved-name check.
        let data = tree_json_with_entry_name("normal-file.txt");
        assert!(Tree::deserialise(&data).is_ok());
    }

    #[test]
    fn deserialise_accepts_name_containing_omemfs_without_hyphen() {
        // "omemfs" (no leading dot, no hyphen) is an ordinary substring and
        // must not be rejected -- only the exact ".omemfs" name and the
        // ".omemfs-" substring pattern are reserved.
        let data = tree_json_with_entry_name("my-omemfs-notes.txt");
        assert!(Tree::deserialise(&data).is_ok());
    }

    #[test]
    fn deserialise_rejects_name_containing_omemfs_dash_substring_anywhere() {
        // Follows the same substring predicate as `scan::is_reserved_name`
        // (`name.contains(".omemfs-")`) for consistency with the local-scan
        // side, even though this makes a name that merely *contains* the
        // pattern -- not a genuine reserved kind -- collateral damage. This
        // is a deliberate consistency trade-off (design/09), not an
        // oversight: see the "Reserved names in remote tree entries" section.
        assert_rejected("weird.omemfs-lookalike.txt");
    }

    #[test]
    fn validate_entry_name_rejects_metadata_directory_name() {
        assert!(validate_entry_name(".omemfs").is_err());
    }

    #[test]
    fn validate_entry_name_rejects_reserved_namespace() {
        assert!(validate_entry_name("file.txt.omemfs-stub").is_err());
    }
}
