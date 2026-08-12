/// Tree navigation and splice utilities.
///
/// `navigate`        — walk from a root hash following path components
/// `navigate_entry`  — same, but return the full TreeEntry at the leaf
/// `splice_entry`    — replace/add a TreeEntry at a path, rebuild intermediates
/// `tree_meta`       — (mtime, size) aggregates for a tree hash
/// `ensure_path_in_store` — download tree hierarchy along a path from src→dst
use chrono::{DateTime, Utc};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::codec;
use crate::codec::pack::reader::PackReader;
use crate::dtimer_l1;
use crate::error::Error;
use crate::object::{Hash, Tree, TreeEntry};
use crate::store::local::LocalStore;
use crate::store::ObjectStore;

thread_local! {
    /// In-process cache of tree entries, keyed by tree object hash. `None` when
    /// disabled (the default). Enabled for the lifetime of a single command via
    /// [`TreeCacheGuard`]. Tree objects are content-addressed and immutable, so a
    /// hash hit always yields the correct entries.
    static TREE_CACHE: RefCell<Option<HashMap<Hash, Rc<Vec<TreeEntry>>>>> =
        const { RefCell::new(None) };
}

/// RAII guard enabling the in-process tree-entry cache on the current thread.
/// The cache populates as `build_and_store` writes trees and is consulted by
/// `load_all_entries`, so a command that builds the working tree (scan) and then
/// re-reads the same trees (listing/diff) avoids a disk read + decompress per
/// tree. Dropping the guard clears the cache, bounding its lifetime to one
/// command. Safe because tree objects are immutable (content-addressed).
pub(crate) struct TreeCacheGuard;

impl TreeCacheGuard {
    pub(crate) fn enable() -> Self {
        TREE_CACHE.with(|c| *c.borrow_mut() = Some(HashMap::new()));
        TreeCacheGuard
    }
}

impl Drop for TreeCacheGuard {
    fn drop(&mut self) {
        TREE_CACHE.with(|c| *c.borrow_mut() = None);
    }
}

fn tree_cache_get(hash: &Hash) -> Option<Rc<Vec<TreeEntry>>> {
    TREE_CACHE.with(|c| c.borrow().as_ref().and_then(|m| m.get(hash).cloned()))
}

fn tree_cache_put(hash: &Hash, entries: &[TreeEntry]) {
    TREE_CACHE.with(|c| {
        if let Some(m) = c.borrow_mut().as_mut() {
            m.entry(hash.clone())
                .or_insert_with(|| Rc::new(entries.to_vec()));
        }
    });
}

/// Navigate from `root_hash` following `path_components`.
/// Returns the hash of the object at that path, or `None` if not found.
pub fn navigate(
    root_hash: &Hash,
    path_components: &[&str],
    store: &dyn ObjectStore,
) -> Result<Option<Hash>, Error> {
    if path_components.is_empty() {
        return Ok(Some(root_hash.clone()));
    }
    let name = path_components[0];
    match find_entry_in_tree(root_hash, name, store)? {
        None => Ok(None),
        Some(e) => {
            let child_hash = match e.hash() {
                Some(h) => h.clone(),
                None => return Ok(None),
            };
            if path_components.len() == 1 {
                Ok(Some(child_hash))
            } else {
                navigate(&child_hash, &path_components[1..], store)
            }
        }
    }
}

/// Navigate from `root_hash` following `path_components`.
/// Returns the full `TreeEntry` at the leaf path, or `None` if not found.
pub fn navigate_entry(
    root_hash: &Hash,
    path_components: &[&str],
    store: &dyn ObjectStore,
) -> Result<Option<TreeEntry>, Error> {
    if path_components.is_empty() {
        return Ok(None);
    }
    let name = path_components[0];
    match find_entry_in_tree(root_hash, name, store)? {
        None => Ok(None),
        Some(e) => {
            if path_components.len() == 1 {
                Ok(Some(e))
            } else {
                let child_hash = match e.hash() {
                    Some(h) => h.clone(),
                    None => return Ok(None),
                };
                navigate_entry(&child_hash, &path_components[1..], store)
            }
        }
    }
}

/// Find a single entry by name in a tree.
pub fn find_entry_in_tree(
    tree_hash: &Hash,
    name: &str,
    store: &dyn ObjectStore,
) -> Result<Option<TreeEntry>, Error> {
    let data = codec::store_read(store, tree_hash, None)?;
    let Tree::Normal { entries } = Tree::deserialise(&data)?;
    Ok(entries.into_iter().find(|e| e.name() == name))
}

/// Remove the entry at `path_components` from the tree at `base_hash`.
/// Rebuilds all intermediate trees bottom-up and writes them to `store`.
/// Returns the new root tree hash, or `None` if the entry was not found.
///
/// Empty intermediate trees are kept (not removed) as empty tree objects.
pub fn remove_entry(
    base_hash: &Hash,
    path_components: &[&str],
    store: &dyn ObjectStore,
) -> Result<Option<Hash>, Error> {
    assert!(
        !path_components.is_empty(),
        "path_components must be non-empty"
    );
    let name = path_components[0];

    let mut entries = load_all_entries(base_hash, store)?;

    if path_components.len() == 1 {
        // Leaf: remove if present.
        let before = entries.len();
        entries.retain(|e| e.name() != name);
        if entries.len() == before {
            // Entry not found.
            return Ok(None);
        }
    } else {
        // Intermediate: recurse into child.
        let child_entry = entries.iter().find(|e| e.name() == name).cloned();
        let child_base = child_entry.as_ref().and_then(|e| e.hash().cloned());
        let Some(child_hash) = child_base else {
            return Ok(None);
        };
        let Some(new_child_hash) = remove_entry(&child_hash, &path_components[1..], store)? else {
            return Ok(None);
        };
        let (child_mtime, child_size, child_blob_count) = tree_meta(&new_child_hash, store)?;
        let new_child = TreeEntry::Tree {
            name: name.to_string(),
            hash: new_child_hash,
            mtime: child_mtime,
            size: child_size,
            blob_count: child_blob_count,
        };
        if let Some(pos) = entries.iter().position(|e| e.name() == name) {
            entries[pos] = new_child;
        }
    }

    Ok(Some(build_and_store(entries, store)?))
}

/// Splice `new_entry` at `path_components` into the tree at `base_hash`
/// (empty tree if `None`). Rebuilds all intermediate trees bottom-up and
/// writes them to `store`. Returns the new root tree hash.
///
/// `new_entry.name()` must equal `path_components.last()`.
pub fn splice_entry(
    base_hash: Option<&Hash>,
    path_components: &[&str],
    new_entry: TreeEntry,
    store: &dyn ObjectStore,
) -> Result<Hash, Error> {
    assert!(
        !path_components.is_empty(),
        "path_components must be non-empty"
    );
    let name = path_components[0];

    let mut entries = match base_hash {
        Some(h) => load_all_entries(h, store)?,
        None => vec![],
    };

    if path_components.len() == 1 {
        // Leaf: replace or add.
        if let Some(pos) = entries.iter().position(|e| e.name() == name) {
            entries[pos] = new_entry;
        } else {
            entries.push(new_entry);
        }
    } else {
        // Intermediate: recurse into child, then rebuild the intermediate entry.
        let child_base = entries
            .iter()
            .find(|e| e.name() == name)
            .and_then(|e| e.hash().cloned());
        let new_child_hash =
            splice_entry(child_base.as_ref(), &path_components[1..], new_entry, store)?;
        let (child_mtime, child_size, child_blob_count) = tree_meta(&new_child_hash, store)?;
        let new_child = TreeEntry::Tree {
            name: name.to_string(),
            hash: new_child_hash,
            mtime: child_mtime,
            size: child_size,
            blob_count: child_blob_count,
        };
        if let Some(pos) = entries.iter().position(|e| e.name() == name) {
            entries[pos] = new_child;
        } else {
            entries.push(new_child);
        }
    }

    build_and_store(entries, store)
}

/// Return (aggregate_mtime, aggregate_size, aggregate_blob_count) for a tree object.
pub fn tree_meta(
    hash: &Hash,
    store: &dyn ObjectStore,
) -> Result<(Option<DateTime<Utc>>, u64, u64), Error> {
    let entries = load_all_entries(hash, store)?;
    Ok((
        Tree::aggregate_mtime(&entries),
        Tree::aggregate_size(&entries),
        Tree::aggregate_blob_count(&entries),
    ))
}

/// Ensure the tree objects along `path_components` (excluding the leaf) are
/// present in `dst`. Downloads any missing objects from `src`.
///
/// Used before `splice_entry` when the base tree comes from a remote store
/// that is separate from the store used for splicing.
pub fn ensure_path_in_store(
    src: &dyn ObjectStore,
    dst: &dyn ObjectStore,
    root_hash: &Hash,
    path_components: &[&str],
) -> Result<(), Error> {
    if !dst.exists(root_hash)? {
        let mut reader = src.open_read(root_hash)?;
        dst.write_from(root_hash, &mut *reader)?;
    }
    // Stop before the leaf: we only need intermediate tree objects.
    if path_components.len() <= 1 {
        return Ok(());
    }
    let name = path_components[0];
    if let Some(child) = find_entry_in_tree(root_hash, name, dst)?
        && let Some(child_hash) = child.hash().cloned()
    {
        ensure_path_in_store(src, dst, &child_hash, &path_components[1..])?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Load all entries from a tree.
pub(crate) fn load_all_entries(
    hash: &Hash,
    store: &dyn ObjectStore,
) -> Result<Vec<TreeEntry>, Error> {
    if let Some(cached) = tree_cache_get(hash) {
        return Ok((*cached).clone());
    }
    let data = codec::store_read(store, hash, None)?;
    let Tree::Normal { entries } = Tree::deserialise(&data)?;
    tree_cache_put(hash, &entries);
    Ok(entries)
}

/// Build a tree from `entries` and store it.
pub(crate) fn build_and_store(
    entries: Vec<TreeEntry>,
    store: &dyn ObjectStore,
) -> Result<Hash, Error> {
    let entries = Tree::sorted_entries(entries);
    let tree = Tree::Normal { entries };
    let bytes = tree.serialise();
    let hash = Hash::compute(&bytes);
    // Skip encode+write when the object already exists (content-addressed, immutable).
    if !store.exists(&hash)? {
        codec::store_write(store, &hash, &bytes, None)?;
    }
    // Populate the in-process tree cache (no-op when disabled) so a later listing
    // phase reuses these entries without a disk read + decompress.
    let Tree::Normal { entries } = &tree;
    tree_cache_put(&hash, entries);
    Ok(hash)
}

/// Flatten a tree into a map from relative path (forward-slash, no leading
/// slash) to the corresponding `TreeEntry`. Only blob/symlink entries are
/// included; intermediate tree entries are not.
///
/// Used to pass clone root metadata to `scan_and_store` for the mtime
/// pre-filter optimisation.
pub fn flatten_tree_entries(
    root_hash: &Hash,
    store: &dyn ObjectStore,
) -> Result<std::collections::HashMap<String, crate::object::TreeEntry>, Error> {
    let _t = dtimer_l1!("flatten_tree");
    let mut map = std::collections::HashMap::new();
    flatten_into(root_hash, "", store, &mut map)?;
    Ok(map)
}

fn flatten_into(
    hash: &Hash,
    prefix: &str,
    store: &dyn ObjectStore,
    out: &mut std::collections::HashMap<String, crate::object::TreeEntry>,
) -> Result<(), Error> {
    let entries = load_all_entries(hash, store)?;
    for entry in entries {
        let rel_path = if prefix.is_empty() {
            entry.name().to_string()
        } else {
            format!("{}/{}", prefix, entry.name())
        };
        match &entry {
            crate::object::TreeEntry::Tree {
                hash: child_hash, ..
            } => {
                flatten_into(child_hash, &rel_path, store, out)?;
            }
            _ => {
                out.insert(rel_path, entry);
            }
        }
    }
    Ok(())
}

/// Push variant of [`flatten_tree_entries`]. It also retains intermediate tree
/// entries so a directory that vanishes after its parent was listed can be
/// preserved as a whole instead of being encoded as empty or deleted.
pub fn flatten_tree_entries_for_push(
    root_hash: &Hash,
    store: &dyn ObjectStore,
) -> Result<std::collections::HashMap<String, crate::object::TreeEntry>, Error> {
    let _t = dtimer_l1!("flatten_tree");
    let mut map = std::collections::HashMap::new();
    flatten_into_for_push(root_hash, "", store, &mut map)?;
    Ok(map)
}

fn flatten_into_for_push(
    hash: &Hash,
    prefix: &str,
    store: &dyn ObjectStore,
    out: &mut std::collections::HashMap<String, crate::object::TreeEntry>,
) -> Result<(), Error> {
    let entries = load_all_entries(hash, store)?;
    for entry in entries {
        let rel_path = if prefix.is_empty() {
            entry.name().to_string()
        } else {
            format!("{}/{}", prefix, entry.name())
        };
        if let crate::object::TreeEntry::Tree {
            hash: child_hash, ..
        } = &entry
        {
            let child_hash = child_hash.clone();
            out.insert(rel_path.clone(), entry);
            // A fully stubbed subtree legitimately has no local tree object.
            // Retain its parent entry for best-effort preservation, but do not
            // cross the stub boundary.
            if store.exists(&child_hash)? {
                flatten_into_for_push(&child_hash, &rel_path, store, out)?;
            }
        } else {
            out.insert(rel_path, entry);
        }
    }
    Ok(())
}

/// Flatten only the subtree of `root_hash` found at `rel`, keeping the result
/// map's keys as full repo-relative paths (e.g. `blog/post.md`, not
/// `post.md`), so they match the scan's `rel_path` lookups.
///
/// Used by scoped push (design/03 "Path-scoped push") to build the mtime
/// pre-filter map for `scan_and_store` without walking the whole clone root:
/// only the spine of tree objects along `rel`'s path components is read (via
/// [`navigate_entry`]); the subtree found at the end of that spine is then
/// flattened. Clone-root tree objects outside `rel` -- other than the spine --
/// are never read.
///
/// - If `rel` is absent from the tree rooted at `root_hash`, returns an empty
///   map.
/// - If `rel` resolves to a blob or symlink, returns a single-entry map keyed
///   by `rel` itself.
/// - If `rel` resolves to a tree, returns that subtree's flattened blob/symlink
///   entries, with keys carrying the `rel` prefix.
///
/// As with [`flatten_tree_entries`], a map miss is always safe: this is purely
/// an optimisation for `scan_and_store`'s mtime pre-filter, not a correctness
/// requirement -- an entry missing from the map is simply re-hashed by the
/// scan (design/03 "Path-scoped push").
///
/// `rel` must be a repo-relative forward-slash path with no leading or
/// trailing slash, and must be non-empty (an empty `rel` means "whole tree",
/// which is [`flatten_tree_entries`]'s job, not this function's).
pub fn flatten_tree_entries_scoped(
    root_hash: &Hash,
    rel: &str,
    store: &dyn ObjectStore,
) -> Result<std::collections::HashMap<String, crate::object::TreeEntry>, Error> {
    let _t = dtimer_l1!("flatten_tree");
    let components: Vec<&str> = rel.split('/').collect();
    let mut map = std::collections::HashMap::new();
    match navigate_entry(root_hash, &components, store)? {
        None => {
            // Path absent from the clone root: empty map, no error (safe).
        }
        Some(crate::object::TreeEntry::Tree {
            hash: child_hash, ..
        }) => {
            flatten_into(&child_hash, rel, store, &mut map)?;
        }
        Some(entry) => {
            // Blob or symlink leaf: single-entry map keyed by the full rel path.
            map.insert(rel.to_string(), entry);
        }
    }
    Ok(map)
}

/// Scoped push variant retaining tree entries inside the selected subtree.
pub fn flatten_tree_entries_scoped_for_push(
    root_hash: &Hash,
    rel: &str,
    store: &dyn ObjectStore,
) -> Result<std::collections::HashMap<String, crate::object::TreeEntry>, Error> {
    let _t = dtimer_l1!("flatten_tree");
    let components: Vec<&str> = rel.split('/').collect();
    let mut map = std::collections::HashMap::new();
    match navigate_entry(root_hash, &components, store)? {
        None => {}
        Some(entry @ crate::object::TreeEntry::Tree { .. }) => {
            let child_hash = entry.hash().cloned().expect("tree has hash");
            map.insert(rel.to_string(), entry);
            flatten_into_for_push(&child_hash, rel, store, &mut map)?;
        }
        Some(entry) => {
            map.insert(rel.to_string(), entry);
        }
    }
    Ok(map)
}

/// Like [`flatten_tree_entries`], but stub-boundary aware: a child tree whose
/// object is not present in `store` is not descended into. This is the
/// local-store counterpart used by callers (e.g. `stub`) that read `clone_root`
/// through the local-only object store, where a stubbed subtree's tree object is
/// legitimately absent. Reading it would raise `ObjectNotFound` and abort.
///
/// Correctness: a materialised directory always has its tree object persisted
/// locally (clone/expand fetch and store it), so every physically-present file
/// remains in the returned map. Stubbed subtrees hold no real files on disk and
/// so contribute no file-stub targets; omitting them is therefore safe. The
/// result is always a subset of the full flatten.
pub fn flatten_tree_entries_local(
    root_hash: &Hash,
    store: &dyn ObjectStore,
) -> Result<std::collections::HashMap<String, crate::object::TreeEntry>, Error> {
    let mut map = std::collections::HashMap::new();
    flatten_local_into(root_hash, "", store, &mut map)?;
    Ok(map)
}

fn flatten_local_into(
    hash: &Hash,
    prefix: &str,
    store: &dyn ObjectStore,
    out: &mut std::collections::HashMap<String, crate::object::TreeEntry>,
) -> Result<(), Error> {
    let entries = load_all_entries(hash, store)?;
    for entry in entries {
        let rel_path = if prefix.is_empty() {
            entry.name().to_string()
        } else {
            format!("{}/{}", prefix, entry.name())
        };
        match &entry {
            crate::object::TreeEntry::Tree {
                hash: child_hash, ..
            } => {
                // Descend only when the child tree object is held locally; a
                // stubbed subtree's object is absent and must not be read.
                if store.exists(child_hash)? {
                    flatten_local_into(child_hash, &rel_path, store, out)?;
                }
            }
            _ => {
                out.insert(rel_path, entry);
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Lazy tree store
// ---------------------------------------------------------------------------

/// A read-through object store over the local cache that fetches a single
/// missing object from the remote (via the pack reader) on a local miss.
///
/// After a lazy, stub-aware clone the local cache does not contain the
/// clone_root tree objects of stubbed subtrees. `pull` reads clone_root tree
/// objects in the diff, in `navigate`, in `mark_deleted_tree`, and when
/// resolving a conflict base. `ls` reads clone_root tree objects in its local
/// (working-tree-vs-clone-root) diff. Routing those reads through this store
/// fetches only the tree objects actually traversed: the diff compares two
/// subtree hashes before reading either tree and skips equal-hash subtrees,
/// so a clone_root read served here fetches only the subtrees that differ
/// from the remote root (for `pull`) or from the working tree (for `ls`).
/// This replaces an eager full-skeleton pre-download.
///
/// The pack reader returns still-encrypted bytes on a remote hit, so a miss is
/// decoded with `remote_key` and re-stored in the local cache as plaintext
/// (the local cache is never encrypted). All subsequent reads of the object are
/// served locally. Reads are routed through `codec::store_read(.., None)` by
/// callers, which is correct for both the local-hit (plaintext) path and the
/// freshly-cached plaintext returned here.
///
/// Shared by `pull` (unbounded — pull's whole purpose is to talk to the
/// remote, so it always waits for a fetch to finish) and `ls` (bounded by a
/// timeout in the caller — see `commands::ls::LOCAL_DIFF_HEAL_TIMEOUT` — since
/// `ls` is meant to stay responsive against a slow or unreachable remote and
/// falls back to a local-only, per-subtree-tolerant diff instead of blocking
/// or aborting; design/04_cli_spec.md "Local diff self-healing").
pub(crate) struct LazyTreeStore<'a> {
    local: &'a LocalStore,
    pack_reader: &'a PackReader,
    remote_key: Option<&'a crate::codec::encrypt::EncryptKey>,
}

impl<'a> LazyTreeStore<'a> {
    pub(crate) fn new(
        local: &'a LocalStore,
        pack_reader: &'a PackReader,
        remote_key: Option<&'a crate::codec::encrypt::EncryptKey>,
    ) -> Self {
        LazyTreeStore {
            local,
            pack_reader,
            remote_key,
        }
    }

    /// Ensure `hash` is present in the local cache as plaintext, fetching it
    /// from the remote (decoding with `remote_key`) on a miss.
    fn ensure_local(&self, hash: &Hash) -> Result<(), Error> {
        if self.local.exists(hash)? {
            return Ok(());
        }
        let plaintext = codec::store_read(self.pack_reader, hash, self.remote_key)?;
        codec::store_write(self.local, hash, &plaintext, None)?;
        Ok(())
    }
}

impl ObjectStore for LazyTreeStore<'_> {
    fn exists(&self, hash: &Hash) -> Result<bool, Error> {
        if self.local.exists(hash)? {
            return Ok(true);
        }
        self.pack_reader.exists(hash)
    }

    fn size(&self, hash: &Hash) -> Result<u64, Error> {
        self.ensure_local(hash)?;
        self.local.size(hash)
    }

    fn list_with_sizes(&self) -> Result<Vec<(String, u64)>, Error> {
        self.local.list_with_sizes()
    }

    fn open_read(&self, hash: &Hash) -> Result<Box<dyn std::io::Read>, Error> {
        self.ensure_local(hash)?;
        self.local.open_read(hash)
    }

    fn write_from(&self, hash: &Hash, reader: &mut dyn std::io::Read) -> Result<(), Error> {
        self.local.write_from(hash, reader)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::ObjectStore;
    use std::collections::HashMap;
    use std::io::Read;
    use std::sync::{Arc, Mutex};

    struct MemStore {
        data: Mutex<HashMap<String, Vec<u8>>>,
        write_count: Arc<Mutex<usize>>,
        read_count: Arc<Mutex<usize>>,
    }

    impl MemStore {
        fn new() -> Self {
            MemStore {
                data: Mutex::new(HashMap::new()),
                write_count: Arc::new(Mutex::new(0)),
                read_count: Arc::new(Mutex::new(0)),
            }
        }

        fn write_count(&self) -> usize {
            *self.write_count.lock().unwrap()
        }

        fn read_count(&self) -> usize {
            *self.read_count.lock().unwrap()
        }
    }

    impl ObjectStore for MemStore {
        fn open_read(&self, hash: &Hash) -> Result<Box<dyn std::io::Read>, crate::error::Error> {
            let data = self
                .data
                .lock()
                .unwrap()
                .get(hash.as_str())
                .cloned()
                .ok_or_else(|| crate::error::Error::ObjectNotFound(hash.as_str().to_string()))?;
            *self.read_count.lock().unwrap() += 1;
            Ok(Box::new(std::io::Cursor::new(data)))
        }
        fn write_from(
            &self,
            hash: &Hash,
            reader: &mut dyn std::io::Read,
        ) -> Result<(), crate::error::Error> {
            let mut data = Vec::new();
            reader
                .read_to_end(&mut data)
                .map_err(crate::error::Error::Io)?;
            self.data
                .lock()
                .unwrap()
                .insert(hash.as_str().to_string(), data);
            *self.write_count.lock().unwrap() += 1;
            Ok(())
        }
        fn exists(&self, hash: &Hash) -> Result<bool, crate::error::Error> {
            Ok(self.data.lock().unwrap().contains_key(hash.as_str()))
        }
        fn size(&self, hash: &Hash) -> Result<u64, crate::error::Error> {
            let data = self.data.lock().unwrap();
            match data.get(hash.as_str()) {
                Some(v) => Ok(v.len() as u64),
                None => Err(crate::error::Error::ObjectNotFound(
                    hash.as_str().to_string(),
                )),
            }
        }
        fn list_with_sizes(&self) -> Result<Vec<(String, u64)>, crate::error::Error> {
            let data = self.data.lock().unwrap();
            Ok(data
                .iter()
                .map(|(k, v)| (k.clone(), v.len() as u64))
                .collect())
        }
    }

    fn make_blob_entry(name: &str) -> TreeEntry {
        let content = name.as_bytes();
        let hash = Hash::compute(content);
        TreeEntry::Blob {
            name: name.to_string(),
            hash,
            mtime: None,
            size: content.len() as u64,
            mode: None,
        }
    }

    #[test]
    fn build_normal_tree() {
        let store = MemStore::new();
        let entries: Vec<TreeEntry> = (0..10)
            .map(|i| make_blob_entry(&format!("file{:04}", i)))
            .collect();
        let hash = build_and_store(entries, &store).unwrap();
        let data = crate::codec::store_read(&store, &hash, None).unwrap();
        let tree = Tree::deserialise(&data).unwrap();
        assert!(matches!(tree, Tree::Normal { .. }));
    }

    // build_and_store must skip encode+write when the object already exists.
    #[test]
    fn build_and_store_skips_encode_on_second_call() {
        let store = MemStore::new();
        let entries: Vec<TreeEntry> = (0..5)
            .map(|i| make_blob_entry(&format!("f{}", i)))
            .collect();

        let hash1 = build_and_store(entries.clone(), &store).unwrap();
        let writes_after_first = store.write_count();

        // Second call with identical entries must produce the same hash and
        // must NOT call write_from again (encode skipped).
        let hash2 = build_and_store(entries, &store).unwrap();
        let writes_after_second = store.write_count();

        assert_eq!(hash1, hash2);
        assert_eq!(
            writes_after_first, writes_after_second,
            "second call should not write"
        );
    }

    // flatten_tree_entries_local skips a child tree whose object is absent
    // (a stubbed subtree), while the eager flatten errors on it.
    #[test]
    fn flatten_local_skips_absent_subtree() {
        let store = MemStore::new();

        // Build and store a child tree, then capture its hash. The child is
        // referenced by the root but NOT stored, simulating a stubbed subtree.
        let child_entries = vec![make_blob_entry("c.txt")];
        let absent_child_hash = build_and_store(child_entries, &store).unwrap();
        // Remove the child object so it is referenced but absent locally.
        store
            .data
            .lock()
            .unwrap()
            .remove(absent_child_hash.as_str());

        // Root tree: one materialised file plus a directory entry pointing at
        // the now-absent child tree.
        let root_entries = vec![
            make_blob_entry("top.txt"),
            TreeEntry::Tree {
                name: "stubbed".to_string(),
                hash: absent_child_hash.clone(),
                mtime: None,
                size: 5,
                blob_count: 1,
            },
        ];
        let root_hash = build_and_store(root_entries, &store).unwrap();

        // The eager flatten reads the absent child and errors.
        assert!(matches!(
            flatten_tree_entries(&root_hash, &store),
            Err(crate::error::Error::ObjectNotFound(_))
        ));

        // The boundary-aware flatten skips the absent subtree and returns the
        // materialised file only.
        let map = flatten_tree_entries_local(&root_hash, &store).unwrap();
        assert!(
            map.contains_key("top.txt"),
            "materialised file must be present"
        );
        assert!(
            !map.contains_key("stubbed/c.txt"),
            "stubbed subtree must be skipped"
        );
        assert_eq!(map.len(), 1);
    }

    // load_all_entries must hit the cache when enabled, avoiding a store read.
    #[test]
    fn cache_hit_on_load_all_entries() {
        let store = MemStore::new();
        let entries: Vec<TreeEntry> = vec![make_blob_entry("a.txt"), make_blob_entry("b.txt")];
        let entries_clone = entries.clone();

        // Enable the cache guard (simulating ls command behaviour).
        let _guard = TreeCacheGuard::enable();

        // build_and_store populates the cache.
        let hash = build_and_store(entries, &store).unwrap();
        let reads_after_build = store.read_count();

        // load_all_entries for the same hash must hit the cache (read count unchanged).
        let loaded = load_all_entries(&hash, &store).unwrap();
        let reads_after_load = store.read_count();

        assert_eq!(
            reads_after_build, reads_after_load,
            "cache hit should not read from store"
        );
        assert_eq!(loaded.len(), entries_clone.len());
        for (i, entry) in loaded.iter().enumerate() {
            assert_eq!(entry.name(), entries_clone[i].name());
        }
    }

    // Cache must be disabled by default: without a guard, load_all_entries
    // reads from the store every time (no cross-command leakage).
    #[test]
    fn cache_disabled_by_default() {
        let store = MemStore::new();
        let entries: Vec<TreeEntry> = vec![make_blob_entry("file.txt")];

        // NO guard: cache is disabled.
        let hash = build_and_store(entries, &store).unwrap();
        let reads_after_build = store.read_count();

        // load_all_entries must read from the store (not cached).
        let _ = load_all_entries(&hash, &store).unwrap();
        let reads_after_load = store.read_count();

        assert!(
            reads_after_load > reads_after_build,
            "without guard, load_all_entries must read from store"
        );
    }

    // Dropping the guard clears the cache: subsequent load_all_entries calls
    // read from the store again.
    #[test]
    fn guard_drop_clears_cache() {
        let store = MemStore::new();
        let entries: Vec<TreeEntry> = vec![make_blob_entry("x.txt")];

        let hash = {
            // Inner scope: guard is active, cache is populated.
            let _guard = TreeCacheGuard::enable();
            let h = build_and_store(entries, &store).unwrap();
            // Verify cache hit while guard is alive.
            let reads_before = store.read_count();
            let _ = load_all_entries(&h, &store).unwrap();
            let reads_after = store.read_count();
            assert_eq!(reads_before, reads_after, "cache hit inside guard");
            h
        }; // guard dropped here

        // After guard drop, cache is cleared: load_all_entries must read from store.
        let reads_after_drop = store.read_count();
        let _ = load_all_entries(&hash, &store).unwrap();
        let reads_final = store.read_count();

        assert!(
            reads_final > reads_after_drop,
            "after guard drop, load_all_entries must read from store"
        );
    }

    // ---------------------------------------------------------------------
    // flatten_tree_entries_scoped
    // ---------------------------------------------------------------------

    // Scoping to one of two sibling directories must return exactly that
    // directory's blobs (keyed with the full repo-relative prefix), and must
    // NOT read the sibling subtree's tree object at all (the key regression
    // assertion: push_scoped must stop walking the whole clone root).
    #[test]
    fn scoped_flatten_sibling_dir_excludes_other_subtree_reads() {
        let store = MemStore::new();

        // No cache guard: every load_all_entries call is a real store read,
        // matching cache_disabled_by_default's setup.
        let blog_entries = vec![make_blob_entry("post1.md"), make_blob_entry("post2.md")];
        let blog_hash = build_and_store(blog_entries, &store).unwrap();

        let docs_entries = vec![make_blob_entry("readme.md"), make_blob_entry("guide.md")];
        let docs_hash = build_and_store(docs_entries, &store).unwrap();

        let root_entries = vec![
            TreeEntry::Tree {
                name: "blog".to_string(),
                hash: blog_hash.clone(),
                mtime: None,
                size: 0,
                blob_count: 2,
            },
            TreeEntry::Tree {
                name: "docs".to_string(),
                hash: docs_hash.clone(),
                mtime: None,
                size: 0,
                blob_count: 2,
            },
        ];
        let root_hash = build_and_store(root_entries, &store).unwrap();

        let reads_before = store.read_count();
        let map = flatten_tree_entries_scoped(&root_hash, "blog", &store).unwrap();
        let reads_after = store.read_count();

        assert!(map.contains_key("blog/post1.md"));
        assert!(map.contains_key("blog/post2.md"));
        assert_eq!(map.len(), 2, "only blog's two blobs must be present");
        assert!(
            !map.contains_key("docs/readme.md"),
            "docs must not be flattened"
        );
        assert!(
            !map.contains_key("docs/guide.md"),
            "docs must not be flattened"
        );

        // Expected reads: the root tree (to find "blog") + blog's own tree
        // (to flatten it). docs' subtree object is never read. Blobs are not
        // read by flatten (only their TreeEntry metadata is copied).
        assert_eq!(
            reads_after - reads_before,
            2,
            "must read only the root tree and blog's tree, never docs"
        );
    }

    // Scoping to a path whose leaf is a blob (not a directory) must return a
    // single-entry map keyed by the full rel path.
    #[test]
    fn scoped_flatten_blob_leaf_returns_single_entry() {
        let store = MemStore::new();

        let root_entries = vec![make_blob_entry("readme.md"), make_blob_entry("other.md")];
        let root_hash = build_and_store(root_entries, &store).unwrap();

        let map = flatten_tree_entries_scoped(&root_hash, "readme.md", &store).unwrap();

        assert_eq!(map.len(), 1);
        assert!(map.contains_key("readme.md"));
    }

    // Scoping to a path absent from the tree must return an empty map, with
    // no error (map-absence-is-safe: the scan simply re-hashes that path).
    #[test]
    fn scoped_flatten_absent_path_is_empty() {
        let store = MemStore::new();

        let root_entries = vec![make_blob_entry("readme.md")];
        let root_hash = build_and_store(root_entries, &store).unwrap();

        let map = flatten_tree_entries_scoped(&root_hash, "missing", &store).unwrap();
        assert!(map.is_empty());

        let map = flatten_tree_entries_scoped(&root_hash, "missing/nested", &store).unwrap();
        assert!(map.is_empty());
    }

    // A nested scope ("a/b") must read only the spine (root, "a") plus "b"'s
    // own subtree, even when sibling subtrees exist under both the root and
    // under "a".
    #[test]
    fn scoped_flatten_nested_scope_reads_only_spine() {
        let store = MemStore::new();

        let b_entries = vec![make_blob_entry("leaf.txt")];
        let b_hash = build_and_store(b_entries, &store).unwrap();

        // Sibling of "b" under "a" -- must never be read.
        let sib_in_a_entries = vec![make_blob_entry("other_leaf.txt")];
        let sib_in_a_hash = build_and_store(sib_in_a_entries, &store).unwrap();

        let a_entries = vec![
            TreeEntry::Tree {
                name: "b".to_string(),
                hash: b_hash.clone(),
                mtime: None,
                size: 0,
                blob_count: 1,
            },
            TreeEntry::Tree {
                name: "sib_in_a".to_string(),
                hash: sib_in_a_hash.clone(),
                mtime: None,
                size: 0,
                blob_count: 1,
            },
        ];
        let a_hash = build_and_store(a_entries, &store).unwrap();

        // Sibling of "a" under the root -- must never be read.
        let sib_at_root_entries = vec![make_blob_entry("root_sibling.txt")];
        let sib_at_root_hash = build_and_store(sib_at_root_entries, &store).unwrap();

        let root_entries = vec![
            TreeEntry::Tree {
                name: "a".to_string(),
                hash: a_hash.clone(),
                mtime: None,
                size: 0,
                blob_count: 2,
            },
            TreeEntry::Tree {
                name: "sib_at_root".to_string(),
                hash: sib_at_root_hash.clone(),
                mtime: None,
                size: 0,
                blob_count: 1,
            },
        ];
        let root_hash = build_and_store(root_entries, &store).unwrap();

        let reads_before = store.read_count();
        let map = flatten_tree_entries_scoped(&root_hash, "a/b", &store).unwrap();
        let reads_after = store.read_count();

        assert_eq!(map.len(), 1);
        assert!(map.contains_key("a/b/leaf.txt"));
        assert!(!map.contains_key("a/sib_in_a/other_leaf.txt"));
        assert!(!map.contains_key("sib_at_root/root_sibling.txt"));

        // Expected reads: root tree + "a" tree (the spine) + "b" tree (the
        // subtree flattened at the end of the spine). Neither sibling subtree
        // is ever read.
        assert_eq!(
            reads_after - reads_before,
            3,
            "must read only root, a, and b -- never the sibling subtrees"
        );
    }

    // -----------------------------------------------------------------------
    // LazyTreeStore
    // -----------------------------------------------------------------------

    /// Build a `(PackWriter, PackReader)` pair over a fresh temp-dir "remote",
    /// mirroring `codec::pack::reader::tests::setup` -- the writer publishes
    /// objects into the same on-disk remote the reader (and, through it, a
    /// `LazyTreeStore`) reads from.
    fn setup_pack(
        tmp: &std::path::Path,
    ) -> (
        crate::codec::pack::writer::PackWriter,
        PackReader,
        LocalStore,
    ) {
        use crate::codec::pack::root_pointer::LocalRootPointer;
        use crate::codec::pack::writer::PackWriter;

        let base = tmp.to_path_buf();
        std::fs::create_dir_all(base.join("objects")).unwrap();
        std::fs::create_dir_all(base.join("tmp")).unwrap();

        let local_cache_dir = base.join("local_cache");
        std::fs::create_dir_all(&local_cache_dir).unwrap();
        let packcache_dir = base.join("packcache");
        std::fs::create_dir_all(&packcache_dir).unwrap();
        let objcache_dir = base.join("objcache");
        std::fs::create_dir_all(&objcache_dir).unwrap();
        let writer_objcache_dir = base.join("writer_objcache");
        std::fs::create_dir_all(&writer_objcache_dir).unwrap();

        let writer = PackWriter::new(
            Box::new(LocalStore::for_remote(&base)),
            Box::new(LocalRootPointer::new(base.clone(), None)),
            LocalStore::for_cache(&writer_objcache_dir),
            None,
        )
        .unwrap();

        let local_cache = LocalStore::for_cache(&local_cache_dir);
        let reader = PackReader::new(
            Box::new(LocalStore::for_remote(&base)),
            local_cache.clone(),
            LocalStore::for_cache(&packcache_dir),
            LocalStore::for_cache(&objcache_dir),
            Box::new(LocalRootPointer::new(base.clone(), None)),
            None,
        );

        (writer, reader, local_cache)
    }

    #[test]
    fn lazy_tree_store_heals_local_miss_and_caches_locally() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (mut writer, reader, local) = setup_pack(tmp.path());

        // Publish an object on the "remote" only -- the local cache never
        // sees it directly, mirroring a clone-root tree object a lazy clone
        // never fetched.
        let content = b"remote-only tree object content".to_vec();
        let hash = Hash::compute(&content);
        writer
            .write_from(&hash, &mut std::io::Cursor::new(&content))
            .unwrap();
        writer.finish(&Hash::compute(b"root")).unwrap();

        assert!(
            !local.exists(&hash).unwrap(),
            "object must be absent from the local cache before healing"
        );

        let lazy = LazyTreeStore::new(&local, &reader, None);
        assert!(
            lazy.exists(&hash).unwrap(),
            "LazyTreeStore.exists must see the remote-only object"
        );
        let mut got = Vec::new();
        lazy.open_read(&hash).unwrap().read_to_end(&mut got).unwrap();
        assert_eq!(got, content, "must read back byte-identical content");

        // The read-through fetch must have cached the plaintext locally, so a
        // subsequent read (from `ls`, `pull`, or anything else) is local.
        assert!(
            local.exists(&hash).unwrap(),
            "a local miss healed through LazyTreeStore must be cached locally afterward"
        );
        let cached = codec::store_read(&local, &hash, None).unwrap();
        assert_eq!(cached, content);
    }

    #[test]
    fn lazy_tree_store_reports_object_not_found_when_absent_everywhere() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (_writer, reader, local) = setup_pack(tmp.path());

        // Nothing was ever published to the remote or the local cache for
        // this hash.
        let hash = Hash::compute(b"never written anywhere");
        assert!(!local.exists(&hash).unwrap());
        assert!(!reader.exists(&hash).unwrap());

        let lazy = LazyTreeStore::new(&local, &reader, None);
        // `Box<dyn Read>` (the `Ok` side) does not implement `Debug`, so
        // `Result::unwrap_err` (which requires `T: Debug`) cannot be used here.
        let err = match lazy.open_read(&hash) {
            Err(e) => e,
            Ok(_) => panic!("expected an error, got Ok"),
        };
        assert!(
            matches!(err, Error::ObjectNotFound(_)),
            "a miss on both sides must surface as Error::ObjectNotFound (not panic, \
             not some other error variant), so callers can tolerate it per-subtree; got {:?}",
            err
        );
    }
}
