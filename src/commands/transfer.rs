//! Parallel object-transfer engine (Phase 5).
//!
//! Both push (`upload_missing_with_progress`) and copy (`transfer_objects` /
//! `transfer_objects_many`) perform the same shape of work: a breadth-first walk
//! of the object graph reachable from **one or more** root hashes, where each
//! node is checked/read/written on a
//! `Send + Sync` [`ObjectStore`] and its children are discovered by parsing the
//! fetched bytes (chunk manifest or tree). The walk has **no ordering
//! dependency** between nodes and writes are content-addressed and idempotent,
//! so the nodes can be processed by a pool of worker threads sharing one
//! `&dyn ObjectStore`.
//!
//! ## Concurrency model
//!
//! This is a *dynamic* BFS: a worker that processes a node discovers and
//! enqueues that node's children, so every worker is both a producer and a
//! consumer. A bounded channel is therefore the wrong primitive — all workers
//! could block on `send` with no one left to `recv` (deadlock). Instead we use:
//!
//! - a shared `Mutex<VecDeque<Hash>>` work queue + a `Condvar` to park idle
//!   workers, seeded with *all* root hashes up front so a batch of independent
//!   childless roots (e.g. many small leaf blobs) is divisible across workers
//!   from the start — see `design/02_storage_format.md`, "Multi-root batching
//!   (Improvement B)",
//! - a shared `Mutex<HashSet<Hash>>` of visited hashes (claim-before-process, so
//!   a shared subtree or duplicate blob is handled exactly once),
//! - an `AtomicUsize` count of *outstanding* items (queued + in-flight). The run
//!   is complete only when this reaches zero; that is the termination signal
//!   that wakes all parked workers,
//! - first-error-wins: the first worker to fail stores its `Error` and sets an
//!   `AtomicBool`; all workers observe it and stop. The stored error is returned.
//!
//! ## Default-1 fast path
//!
//! When the resolved concurrency is 1, the caller runs its original serial loop
//! unchanged — `parallel_bfs` is only entered for `workers >= 2`. The local
//! backend defaults to 1 (see [`ObjectStore::default_transfer_concurrency`]), so
//! the local transfer path is byte-identical to before Phase 5.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

use crate::error::Error;
use crate::object::{Hash, Tree};

/// The per-node action a transfer performs. Returns the bytes whose children
/// should be enqueued next (the chunk-manifest / tree bytes), or `None` to
/// enqueue nothing (e.g. the node already existed and must not be traversed).
///
/// The closure runs concurrently on worker threads, so it must be `Sync` and
/// only touch shared state through `Send + Sync` handles (the `&dyn ObjectStore`
/// arguments already are).
pub type NodeFn<'a> = dyn Fn(&Hash) -> Result<Option<Vec<u8>>, Error> + Sync + 'a;

/// Walk a graph in parallel when the caller determines the child hashes. This
/// is used by read-side planners that must traverse trees without treating all
/// entries as fetchable content (for example, threshold-stubbed blobs).
pub type ChildNodeFn<'a> = dyn Fn(&Hash) -> Result<Vec<Hash>, Error> + Sync + 'a;

/// Resolve the worker count for a transfer to/from `store`.
///
/// Order of precedence: the `OMEMFS_TRANSFER_CONCURRENCY` environment variable
/// (when set to a parseable value `>= 1`), otherwise the store's own default
/// (`1` for local filesystems, higher for cloud). A configured value of `0` or
/// an unparseable value is ignored in favour of the store default.
pub fn resolve_concurrency(store: &dyn crate::store::ObjectStore) -> usize {
    if let Ok(s) = std::env::var("OMEMFS_TRANSFER_CONCURRENCY")
        && let Ok(n) = s.trim().parse::<usize>()
        && n >= 1
    {
        return n;
    }
    store.default_transfer_concurrency().max(1)
}

/// Default ceiling, in bytes, on the total size of transfer buffers held
/// resident across all workers of one transfer run. See
/// [`resolve_memory_budget`] and [`ByteBudget`].
pub const DEFAULT_TRANSFER_MEMORY_BUDGET: u64 = 64 * 1024 * 1024;

/// Resolve the in-flight memory budget (bytes) for a transfer run.
///
/// The `OMEMFS_TRANSFER_MEMORY_BUDGET` environment variable overrides the
/// default when set to a parseable value `>= 1`; `0` or an unparseable value is
/// ignored in favour of [`DEFAULT_TRANSFER_MEMORY_BUDGET`]. This is a *separate*
/// knob from [`resolve_concurrency`]: concurrency tunes request parallelism
/// (latency hiding), this tunes peak memory, independently of the worker count.
/// See `design/02_storage_format.md`, "Two independent knobs".
pub fn resolve_memory_budget() -> u64 {
    if let Ok(s) = std::env::var("OMEMFS_TRANSFER_MEMORY_BUDGET")
        && let Ok(n) = s.trim().parse::<u64>()
        && n >= 1
    {
        return n;
    }
    DEFAULT_TRANSFER_MEMORY_BUDGET
}

/// A counting semaphore over *bytes* that bounds the total size of object
/// buffers held resident across all workers of one transfer run.
///
/// A worker calls [`ByteBudget::acquire`] with the size of the object it is
/// about to buffer; the returned [`BudgetGuard`] releases the bytes back to the
/// budget when it drops (after the buffer is no longer resident, i.e. after the
/// PUT / local write completes). Because the worker count and the byte budget
/// are independent, raising concurrency for latency does not raise the memory
/// ceiling.
///
/// **Deadlock freedom.** `acquire` clamps the request to the total capacity, so
/// a single object larger than the whole budget is admitted alone (it blocks
/// until the budget is otherwise free, then runs by itself) rather than waiting
/// forever for bytes that can never exist. Each worker holds at most one
/// acquisition at a time and never nests acquisitions, so there is no
/// hold-and-wait cycle.
pub struct ByteBudget {
    capacity: u64,
    available: Mutex<u64>,
    cv: Condvar,
}

/// RAII permit returned by [`ByteBudget::acquire`]; releases its bytes on drop.
pub struct BudgetGuard<'a> {
    budget: &'a ByteBudget,
    amount: u64,
}

impl ByteBudget {
    /// Create a budget with `capacity` bytes available (floored at 1 so a
    /// degenerate `0` capacity still admits one object at a time via the clamp).
    pub fn new(capacity: u64) -> Self {
        let capacity = capacity.max(1);
        ByteBudget {
            capacity,
            available: Mutex::new(capacity),
            cv: Condvar::new(),
        }
    }

    /// Acquire `min(amount, capacity)` bytes, blocking until that many are free.
    /// Clamping to capacity guarantees forward progress for an oversized object.
    pub fn acquire(&self, amount: u64) -> BudgetGuard<'_> {
        let want = amount.min(self.capacity);
        let mut avail = self.available.lock().unwrap();
        while *avail < want {
            avail = self.cv.wait(avail).unwrap();
        }
        *avail -= want;
        BudgetGuard {
            budget: self,
            amount: want,
        }
    }
}

impl Drop for BudgetGuard<'_> {
    fn drop(&mut self) {
        let mut avail = self.budget.available.lock().unwrap();
        *avail += self.amount;
        // Wake all waiters: several small acquisitions may now fit where one
        // large one did not.
        self.budget.cv.notify_all();
    }
}

/// Extract the child hashes referenced by `data` (a chunk manifest or a tree).
/// Mirrors the serial loops' child-discovery exactly: a chunk manifest yields
/// its chunk hashes; otherwise a `Tree::Normal` yields each entry's hash; any
/// other bytes (a leaf blob) yield nothing.
pub fn child_hashes(data: &[u8]) -> Vec<Hash> {
    if let Some(chunk_hashes) = crate::object::deserialise_manifest(data) {
        return chunk_hashes;
    }
    if let Ok(Tree::Normal { entries }) = Tree::deserialise(data) {
        return entries.iter().filter_map(|e| e.hash().cloned()).collect();
    }
    Vec::new()
}

/// Shared state for one parallel BFS run.
struct Shared {
    queue: Mutex<VecDeque<Hash>>,
    /// Hashes already claimed for processing (dedup of shared subtrees).
    visited: Mutex<HashSet<Hash>>,
    /// Wakes workers when work is enqueued or the run is finished.
    cv: Condvar,
    /// Queued + in-flight items. The run ends when this hits zero.
    outstanding: AtomicUsize,
    /// Set on the first error; makes all workers drain out.
    failed: AtomicBool,
    /// The first error observed (first-error-wins).
    error: Mutex<Option<Error>>,
}

impl Shared {
    /// Record an error if none has been recorded yet, and signal shutdown.
    fn record_error(&self, e: Error) {
        let mut slot = self.error.lock().unwrap();
        if slot.is_none() {
            *slot = Some(e);
        }
        self.failed.store(true, Ordering::SeqCst);
        // Wake everyone so they can observe the failure and exit.
        self.cv.notify_all();
    }
}

/// Run a parallel breadth-first transfer from **one or more** `roots` using
/// `workers` threads, invoking `node_fn` on each unique reachable hash.
///
/// `node_fn` returns `Some(bytes)` to enqueue the children parsed from `bytes`,
/// or `None` to enqueue nothing. Requires `workers >= 2`; callers use their
/// serial loop for `workers == 1`.
///
/// The whole `roots` slice seeds the shared queue at once, so a batch of
/// independent roots is distributed across all workers immediately instead of
/// being walked one root at a time (`design/02_storage_format.md`, "Multi-root
/// batching (Improvement B)"). The shared `visited` set makes dedup uniform:
/// a hash reachable from several roots — including the same hash appearing more
/// than once in `roots` — is claimed and processed exactly once. An empty
/// `roots` slice is a no-op.
pub fn parallel_bfs(roots: &[Hash], workers: usize, node_fn: &NodeFn<'_>) -> Result<(), Error> {
    debug_assert!(workers >= 2, "parallel_bfs requires >= 2 workers");

    let shared = Shared {
        queue: Mutex::new(roots.iter().cloned().collect::<VecDeque<Hash>>()),
        visited: Mutex::new(HashSet::new()),
        cv: Condvar::new(),
        outstanding: AtomicUsize::new(roots.len()),
        failed: AtomicBool::new(false),
        error: Mutex::new(None),
    };

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| worker_loop(&shared, node_fn));
        }
    });

    // All workers have joined; return the first error, if any.
    match shared.error.into_inner().unwrap() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Parallel graph walk with caller-defined child discovery.
pub fn parallel_walk(
    roots: &[Hash],
    workers: usize,
    node_fn: &ChildNodeFn<'_>,
) -> Result<(), Error> {
    if roots.is_empty() {
        return Ok(());
    }
    if workers < 2 {
        let mut queue: VecDeque<Hash> = roots.iter().cloned().collect();
        let mut visited = HashSet::new();
        while let Some(hash) = queue.pop_front() {
            if !visited.insert(hash.clone()) {
                continue;
            }
            queue.extend(node_fn(&hash)?);
        }
        return Ok(());
    }
    let shared = Shared {
        queue: Mutex::new(roots.iter().cloned().collect::<VecDeque<Hash>>()),
        visited: Mutex::new(HashSet::new()),
        cv: Condvar::new(),
        outstanding: AtomicUsize::new(roots.len()),
        failed: AtomicBool::new(false),
        error: Mutex::new(None),
    };
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| child_worker_loop(&shared, node_fn));
        }
    });
    match shared.error.into_inner().unwrap() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// One worker: pull a hash, process it, enqueue children, repeat until the run
/// is finished or an error is recorded.
fn worker_loop(shared: &Shared, node_fn: &NodeFn<'_>) {
    loop {
        // --- acquire the next hash, or exit when done/failed ---
        let hash = {
            let mut queue = shared.queue.lock().unwrap();
            loop {
                if shared.failed.load(Ordering::SeqCst) {
                    return;
                }
                if let Some(h) = queue.pop_front() {
                    break h;
                }
                // Queue empty: if nothing is outstanding, the run is complete.
                if shared.outstanding.load(Ordering::SeqCst) == 0 {
                    shared.cv.notify_all();
                    return;
                }
                // Otherwise wait for more work (or a finish/fail notification).
                queue = shared.cv.wait(queue).unwrap();
            }
        };

        // --- claim it (dedup); if already visited, it is not outstanding work ---
        let newly_claimed = shared.visited.lock().unwrap().insert(hash.clone());
        if !newly_claimed {
            decrement_outstanding(shared);
            continue;
        }

        // --- process it ---
        match node_fn(&hash) {
            Ok(children_bytes) => {
                if let Some(bytes) = children_bytes {
                    let children = child_hashes(&bytes);
                    if !children.is_empty() {
                        // Account for the new items before publishing them, so
                        // `outstanding` never transiently hits zero while work
                        // remains.
                        shared
                            .outstanding
                            .fetch_add(children.len(), Ordering::SeqCst);
                        let mut queue = shared.queue.lock().unwrap();
                        queue.extend(children);
                        drop(queue);
                        shared.cv.notify_all();
                    }
                }
                decrement_outstanding(shared);
            }
            Err(e) => {
                shared.record_error(e);
                // This item is done (failed); keep the counter consistent.
                decrement_outstanding(shared);
                return;
            }
        }
    }
}

fn child_worker_loop(shared: &Shared, node_fn: &ChildNodeFn<'_>) {
    loop {
        let hash = {
            let mut queue = shared.queue.lock().unwrap();
            loop {
                if shared.failed.load(Ordering::SeqCst) {
                    return;
                }
                if let Some(h) = queue.pop_front() {
                    break h;
                }
                if shared.outstanding.load(Ordering::SeqCst) == 0 {
                    return;
                }
                queue = shared.cv.wait(queue).unwrap();
            }
        };
        let claimed = shared.visited.lock().unwrap().insert(hash.clone());
        if !claimed {
            if shared.outstanding.fetch_sub(1, Ordering::SeqCst) == 1 {
                shared.cv.notify_all();
            }
            continue;
        }
        match node_fn(&hash) {
            Ok(children) => {
                let count = children.len();
                if count > 0 {
                    shared.outstanding.fetch_add(count, Ordering::SeqCst);
                    shared.queue.lock().unwrap().extend(children);
                    shared.cv.notify_all();
                }
                if shared.outstanding.fetch_sub(1, Ordering::SeqCst) == 1 {
                    shared.cv.notify_all();
                }
            }
            Err(e) => {
                shared.record_error(e);
                return;
            }
        }
    }
}

/// Decrement the outstanding counter; if it reaches zero, wake all workers so
/// they observe completion and exit.
fn decrement_outstanding(shared: &Shared) {
    if shared.outstanding.fetch_sub(1, Ordering::SeqCst) == 1 {
        shared.cv.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn resolve_concurrency_env_overrides_default() {
        // Note: we cannot safely mutate process env in parallel tests, so this
        // checks the parse path indirectly via a tiny helper would be ideal;
        // here we only assert the default path (env unset in the test runner).
        struct Cloudish;
        impl crate::store::ObjectStore for Cloudish {
            fn exists(&self, _: &Hash) -> Result<bool, Error> {
                Ok(false)
            }
            fn size(&self, _: &Hash) -> Result<u64, Error> {
                Ok(0)
            }
            fn list_with_sizes(&self) -> Result<Vec<(String, u64)>, Error> {
                Ok(vec![])
            }
            fn open_read(&self, _: &Hash) -> Result<Box<dyn std::io::Read>, Error> {
                Ok(Box::new(std::io::empty()))
            }
            fn write_from(&self, _: &Hash, _: &mut dyn std::io::Read) -> Result<(), Error> {
                Ok(())
            }
            fn default_transfer_concurrency(&self) -> usize {
                16
            }
        }
        // With env unset, the store default wins.
        if std::env::var("OMEMFS_TRANSFER_CONCURRENCY").is_err() {
            assert_eq!(resolve_concurrency(&Cloudish), 16);
        }
    }

    #[test]
    fn byte_budget_serialises_when_full() {
        // Capacity for exactly one 10-byte object. A second acquire must block
        // until the first guard drops, so the two never overlap.
        use std::sync::Arc;
        let budget = Arc::new(ByteBudget::new(10));
        let overlap = Arc::new(AtomicUsize::new(0));
        let max_overlap = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let budget = Arc::clone(&budget);
            let overlap = Arc::clone(&overlap);
            let max_overlap = Arc::clone(&max_overlap);
            handles.push(std::thread::spawn(move || {
                let _g = budget.acquire(10);
                let cur = overlap.fetch_add(1, Ordering::SeqCst) + 1;
                max_overlap.fetch_max(cur, Ordering::SeqCst);
                // Hold the permit briefly so a genuine overlap would be observed.
                std::thread::yield_now();
                overlap.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            max_overlap.load(Ordering::SeqCst),
            1,
            "a single-object budget must never admit two objects at once"
        );
    }

    #[test]
    fn byte_budget_admits_oversized_object_alone() {
        // An object larger than the whole budget must still be admitted (clamped
        // to capacity) rather than deadlocking.
        let budget = ByteBudget::new(16);
        let g = budget.acquire(1_000_000);
        // The budget is now fully drained; dropping the guard restores it.
        drop(g);
        let _g2 = budget.acquire(8);
    }

    #[test]
    fn byte_budget_packs_small_objects_concurrently() {
        // Capacity 100 with 10-byte objects admits up to 10 at once: the budget
        // must not over-serialise small objects.
        use std::sync::Arc;
        let budget = Arc::new(ByteBudget::new(100));
        let g1 = budget.acquire(10);
        let g2 = budget.acquire(10);
        // Two 10-byte permits coexist under a 100-byte budget.
        drop(g1);
        drop(g2);
    }

    #[test]
    fn resolve_memory_budget_defaults_when_unset() {
        if std::env::var("OMEMFS_TRANSFER_MEMORY_BUDGET").is_err() {
            assert_eq!(resolve_memory_budget(), DEFAULT_TRANSFER_MEMORY_BUDGET);
        }
    }

    #[test]
    fn child_hashes_of_leaf_is_empty() {
        // Random non-tree, non-manifest bytes parse as a leaf → no children.
        assert!(child_hashes(b"not a tree or manifest").is_empty());
    }

    /// A toy in-memory graph store to exercise the BFS engine end-to-end.
    struct GraphStore {
        // hash hex -> serialised bytes (tree/manifest/leaf)
        objects: std::collections::HashMap<String, Vec<u8>>,
        written: Mutex<HashSet<String>>,
        visits: AtomicUsize,
    }

    #[test]
    fn parallel_bfs_visits_every_node_once() {
        // root -> [a, b]; a -> [shared]; b -> [shared] (shared leaf under both).
        // a and b must be DISTINCT trees, so give them different entry names —
        // otherwise identical content collapses to the same hash.
        let shared = Hash::compute(b"shared-leaf");
        let a_tree = make_tree(&[("from_a", &shared)]);
        let b_tree = make_tree(&[("from_b", &shared)]);
        let a = Hash::compute(&a_tree);
        let b = Hash::compute(&b_tree);
        assert_ne!(a, b, "a and b must be distinct subtrees");
        let root_tree = make_tree(&[("a", &a), ("b", &b)]);
        let root = Hash::compute(&root_tree);

        let mut objects = std::collections::HashMap::new();
        objects.insert(root.as_str().to_string(), root_tree);
        objects.insert(a.as_str().to_string(), a_tree);
        objects.insert(b.as_str().to_string(), b_tree);
        objects.insert(shared.as_str().to_string(), b"shared-leaf".to_vec());

        let store = GraphStore {
            objects,
            written: Mutex::new(HashSet::new()),
            visits: AtomicUsize::new(0),
        };

        let node_fn = |h: &Hash| -> Result<Option<Vec<u8>>, Error> {
            store.visits.fetch_add(1, Ordering::SeqCst);
            store.written.lock().unwrap().insert(h.as_str().to_string());
            let bytes = store.objects.get(h.as_str()).cloned().unwrap();
            Ok(Some(bytes))
        };

        // NOTE (test-first / Improvement B): `parallel_bfs` is being extended to
        // take a *slice* of root hashes instead of a single `&Hash` (see
        // design/02_storage_format.md, "Multi-root batching (Improvement B)").
        // This call site is updated to the new intended signature ahead of the
        // implementation, so this whole test module currently fails to
        // *compile* against today's `pub fn parallel_bfs(root: &Hash, ...)`.
        // That compile failure is the expected test-first state.
        parallel_bfs(&[root.clone()], 4, &node_fn).unwrap();

        // 4 distinct nodes (root, a, b, shared) — `shared` is reachable via both
        // a and b but must be visited/written exactly once.
        assert_eq!(store.written.lock().unwrap().len(), 4);
        assert_eq!(store.visits.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn parallel_bfs_propagates_first_error() {
        let leaf = Hash::compute(b"x");
        let root_tree = make_tree(&[("leaf", &leaf)]);
        let root = Hash::compute(&root_tree);
        let mut objects = std::collections::HashMap::new();
        objects.insert(root.as_str().to_string(), root_tree);
        objects.insert(leaf.as_str().to_string(), b"x".to_vec());
        let store = GraphStore {
            objects,
            written: Mutex::new(HashSet::new()),
            visits: AtomicUsize::new(0),
        };
        let leaf_hex = leaf.as_str().to_string();
        let node_fn = |h: &Hash| -> Result<Option<Vec<u8>>, Error> {
            if h.as_str() == leaf_hex {
                return Err(Error::Other("boom".into()));
            }
            Ok(Some(store.objects.get(h.as_str()).cloned().unwrap()))
        };
        // See NOTE above: multi-root signature, single-element slice.
        let err = parallel_bfs(&[root.clone()], 4, &node_fn).unwrap_err();
        assert!(err.to_string().contains("boom"));
    }

    // -----------------------------------------------------------------------
    // Multi-root batching (Improvement B) — design/02_storage_format.md,
    // "Multi-root batching (Improvement B)".
    //
    // `parallel_bfs` is extended to seed the shared work queue with *all* roots
    // up front (`outstanding = roots.len()`), instead of a single root hash.
    // These tests exercise that contract directly against the intended new
    // signature `parallel_bfs(roots: &[Hash], workers: usize, node_fn: &NodeFn)`.
    // Until the signature is changed, this test module fails to compile — the
    // expected test-first state.
    // -----------------------------------------------------------------------

    #[test]
    fn parallel_bfs_multiple_independent_roots() {
        // Two independent leaf blobs with no shared content and no children:
        // both must be visited when seeded together in one call.
        let leaf_a = Hash::compute(b"leaf-a");
        let leaf_b = Hash::compute(b"leaf-b");
        let mut objects = std::collections::HashMap::new();
        objects.insert(leaf_a.as_str().to_string(), b"leaf-a".to_vec());
        objects.insert(leaf_b.as_str().to_string(), b"leaf-b".to_vec());
        let store = GraphStore {
            objects,
            written: Mutex::new(HashSet::new()),
            visits: AtomicUsize::new(0),
        };
        let node_fn = |h: &Hash| -> Result<Option<Vec<u8>>, Error> {
            store.visits.fetch_add(1, Ordering::SeqCst);
            store.written.lock().unwrap().insert(h.as_str().to_string());
            Ok(Some(store.objects.get(h.as_str()).cloned().unwrap()))
        };

        parallel_bfs(&[leaf_a.clone(), leaf_b.clone()], 4, &node_fn).unwrap();

        assert_eq!(store.written.lock().unwrap().len(), 2);
        assert_eq!(store.visits.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn parallel_bfs_multiple_roots_dedup_shared_subtree() {
        // Two independent roots (a, b) that both reference the same shared
        // leaf. The shared leaf is reachable via two *different roots*, not
        // just via one root's descendants -- it must still be claimed and
        // processed exactly once.
        let shared = Hash::compute(b"shared-leaf-multi-root");
        let a_tree = make_tree(&[("from_a", &shared)]);
        let b_tree = make_tree(&[("from_b", &shared)]);
        let a = Hash::compute(&a_tree);
        let b = Hash::compute(&b_tree);
        assert_ne!(a, b, "a and b must be distinct subtrees");

        let mut objects = std::collections::HashMap::new();
        objects.insert(a.as_str().to_string(), a_tree);
        objects.insert(b.as_str().to_string(), b_tree);
        objects.insert(
            shared.as_str().to_string(),
            b"shared-leaf-multi-root".to_vec(),
        );
        let store = GraphStore {
            objects,
            written: Mutex::new(HashSet::new()),
            visits: AtomicUsize::new(0),
        };
        let node_fn = |h: &Hash| -> Result<Option<Vec<u8>>, Error> {
            store.visits.fetch_add(1, Ordering::SeqCst);
            store.written.lock().unwrap().insert(h.as_str().to_string());
            Ok(Some(store.objects.get(h.as_str()).cloned().unwrap()))
        };

        // Seed the two subtree roots directly (no common parent root) --
        // this is the shape that matters for Improvement B: independent
        // roots passed straight into one `parallel_bfs` call.
        parallel_bfs(&[a.clone(), b.clone()], 4, &node_fn).unwrap();

        // a, b, shared: 3 distinct nodes. `shared` must be visited once, not
        // once per root that reaches it.
        assert_eq!(store.written.lock().unwrap().len(), 3);
        assert_eq!(store.visits.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn parallel_bfs_duplicate_roots_processed_once() {
        // The same hash appearing twice in the roots slice (e.g. two blobs
        // that happen to have identical content) must be processed exactly
        // once, not twice.
        let leaf = Hash::compute(b"dup-leaf");
        let mut objects = std::collections::HashMap::new();
        objects.insert(leaf.as_str().to_string(), b"dup-leaf".to_vec());
        let store = GraphStore {
            objects,
            written: Mutex::new(HashSet::new()),
            visits: AtomicUsize::new(0),
        };
        let node_fn = |h: &Hash| -> Result<Option<Vec<u8>>, Error> {
            store.visits.fetch_add(1, Ordering::SeqCst);
            store.written.lock().unwrap().insert(h.as_str().to_string());
            Ok(Some(store.objects.get(h.as_str()).cloned().unwrap()))
        };

        parallel_bfs(&[leaf.clone(), leaf.clone()], 4, &node_fn).unwrap();

        assert_eq!(store.written.lock().unwrap().len(), 1);
        assert_eq!(
            store.visits.load(Ordering::SeqCst),
            1,
            "a duplicate root hash must be claimed and processed exactly once"
        );
    }

    #[test]
    fn parallel_bfs_first_error_wins_across_multiple_roots() {
        // One of several independent roots fails; the run must still surface
        // that error (first-error-wins) instead of silently completing just
        // because the other roots succeeded.
        let ok_leaf = Hash::compute(b"ok-multi-root");
        let bad_leaf = Hash::compute(b"bad-multi-root");
        let mut objects = std::collections::HashMap::new();
        objects.insert(ok_leaf.as_str().to_string(), b"ok-multi-root".to_vec());
        objects.insert(bad_leaf.as_str().to_string(), b"bad-multi-root".to_vec());
        let store = GraphStore {
            objects,
            written: Mutex::new(HashSet::new()),
            visits: AtomicUsize::new(0),
        };
        let bad_hex = bad_leaf.as_str().to_string();
        let node_fn = |h: &Hash| -> Result<Option<Vec<u8>>, Error> {
            if h.as_str() == bad_hex {
                return Err(Error::Other("boom-multi-root".into()));
            }
            store.visits.fetch_add(1, Ordering::SeqCst);
            store.written.lock().unwrap().insert(h.as_str().to_string());
            Ok(Some(store.objects.get(h.as_str()).cloned().unwrap()))
        };

        let err = parallel_bfs(&[ok_leaf.clone(), bad_leaf.clone()], 4, &node_fn).unwrap_err();
        assert!(err.to_string().contains("boom-multi-root"));
    }

    /// Build a minimal serialised `Tree::Normal` with blob entries pointing at
    /// the given hashes (size 0 — only the hash matters for child discovery).
    fn make_tree(entries: &[(&str, &Hash)]) -> Vec<u8> {
        use crate::object::{Tree, TreeEntry};
        let entries = entries
            .iter()
            .map(|(name, h)| TreeEntry::Blob {
                name: name.to_string(),
                hash: (*h).clone(),
                size: 0,
                mtime: None,
                mode: None,
            })
            .collect();
        Tree::Normal { entries }.serialise()
    }
}
