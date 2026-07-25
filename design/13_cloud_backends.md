# Cloud Backends (S3 / Azure / GCS)

This is the authoritative reference for the cloud object-storage backends. It
describes the four-adapter architecture, the three points where the backends
differ, and the per-backend SDK, authentication, object-operation, CAS, and
error-mapping details. For the storage layout see `02_storage_format.md`; for
the CAS / sync semantics see `03_sync_model.md`; for the CLI config blocks see
`04_cli_spec.md`; for locking see `12_locking.md`.

## Overview: one trait, four adapters

omemfs has a single synchronous storage abstraction, the `ObjectStore` trait
(`src/store/mod.rs`), and a single root-pointer CAS abstraction, the
`RootPointer` trait (`src/codec/pack/root_pointer.rs`). There are **four**
adapters implementing them — `Local`, `S3`, `Azure`, `GCS` — not one merged
"cloud" backend. The local adapter is unchanged; the three cloud adapters are
added alongside it.

The `ObjectStore` trait stays **synchronous** (it is already `Send + Sync`).
All three cloud SDKs are async, so each cloud adapter holds a shared
`Arc<tokio::runtime::Runtime>` (a multi-thread runtime) and calls `block_on`
internally to drive each SDK call to completion. **Async must not leak past the
backend's `.rs` file**: no `ObjectStore` / `RootPointer` method ever returns a
future, and a backend method is never called from inside a runtime worker
thread (which would deadlock). Parallelism across objects is provided one level
up by the transfer loops (a thread pool over `clone` / `push` / `pull`), not by
async concurrency inside a single op. The shared infrastructure — the runtime
factory, the async-stream → `Box<dyn io::Read>` collect bridge, the sync
`write_from` buffer bridge, and the hex⇄key layout helpers — lives in
`src/store/cloud/` and is reused by all three cloud adapters.

**One runtime per process, not one per call.** `repo::cloud_runtime()` (the
single factory both `store_for_config` and `root_pointer_for_config` call to
obtain the `Arc<CloudRuntime>` they hand to a backend constructor) returns a
process-wide singleton built once behind a `OnceLock` and shared by every
subsequent call, for the lifetime of the process. Before this, each call built
its *own* fresh `CloudRuntime` (its own tokio thread pool); a single command
that needs both a remote's object store and its root pointer (e.g. `push`,
`pull`, via `Repo::pack_reader` / `Repo::pack_writer`) held two independent
runtimes concurrently for no reason. A shared runtime is also the only way to
exercise concurrent `block_on` calls actually contending on one runtime's
worker pool in a test — `MemCloud` (the in-process fake used by the non-cloud
test suite) never touches tokio at all, so this class of bug is only
observable against a live backend (`tests/s3.bats` et al., run against MinIO).
`CloudRuntime::block_on` additionally asserts (debug builds only) that it is
never called from a thread that is itself a worker of the very runtime it
would block on — the "never call block_on from inside a runtime worker
thread" rule above was previously convention-only; it is now a
`debug_assert!` that fails fast (as a panic) instead of silently deadlocking
if ever violated.

The per-backend SDK client itself (the `S3Backend` / `AzureBackend` /
`GcsBackend` instance, i.e. the actual authenticated connection object) is
*not* shared between a remote's object store and its root pointer: each of
`store_for_config` and `root_pointer_for_config` still builds its own backend
instance (both now take the same shared runtime). Sharing the client instance
too would require a wider API change (a combined
`remote_store`/`remote_root_pointer` accessor touching every call site of
either), and no concurrency bug has been found from the duplicate client
construction, so it is left as a deferred proposal (refactor-instructions.md
Phase 11) rather than implemented here.

### The three per-backend difference points

Everything else is shared; each adapter differs only in:

1. **Client construction / authentication** — how the SDK client is built and
   how credentials are supplied.
2. **Conditional-precondition API** — how the index-root CAS expresses
   "create only if absent" and "update only if unchanged".
3. **Error → `Error::CasFailed` mapping** — how a precondition failure (HTTP
   412) is detected in that SDK's error type.

The object-key / blob-name / object-name layout, the async→sync bytes bridge,
the runtime factory, and the hex⇄key helpers are identical across all three.

## Shared layout

All four backends use the identical key layout (see `02_storage_format.md`):

```
<prefix>/objects/<hex[0..2]>/<hex[2..4]>/<hex[4..6]>/<hex[6..64]>
```

where `hex` is the 64-char storage key (the logical hash, or
`HMAC-SHA256(DEK, hash)` when encrypted). The unencrypted index root lives at
the flat key `<prefix>/INDEX_ROOT`; the encrypted index root lives at the
derived sharded name `HMAC-SHA256(DEK, "omemfs:index-root:v1")`. On the cloud
backends `/` is just an ordinary character in a flat key/blob/object name —
there are no real directories. `list_with_sizes` is **one paginated LIST** per
backend over `<prefix>/objects/`, skipping any key whose re-joined name is not
64 hex characters; the per-object size comes from the LIST response, so no
per-object HEAD/GET is issued.

## S3

| Aspect | Value |
|--------|-------|
| SDK crate | `aws-sdk-s3` 1, `aws-config` 1 |
| Module | `src/store/s3.rs` |
| Auth | static `access_key_id` / `secret_access_key`, or the AWS default credential chain when both are omitted |
| Config fields | `bucket`, `region`, `prefix`, `access_key_id?`, `secret_access_key?`, `endpoint?`, `force_path_style?`, `encryption?` |
| Test harness | MinIO (`endpoint` + `force_path_style: true`), env-gated by `OMEMFS_S3_TEST_ENDPOINT` |

**Client construction.** Built from `aws_config` plus the configured region and
either static credentials or the default chain. The optional `endpoint`
(custom service URL) and `force_path_style` (path-style addressing) fields make
the client target an S3-compatible store such as MinIO.

**Object ops.**

- `exists` / `size`: `HeadObject` — a `NoSuchKey` / 404 means absent (`exists`
  → `false`; `size` → error).
- `list_with_sizes`: paginated `ListObjectsV2` over `<prefix>/objects/`,
  following the continuation token; size is `Object.size`.
- `open_read`: `GetObject`, collecting the body to bytes via the shared bridge.
- `write_from`: `PutObject` with the buffered bytes; idempotent.

**CAS / RootPointer (`S3RootPointer`).** The token is the object **ETag**.
`read` is a `GetObject` (404 → `RootToken::Absent`) capturing the ETag.
`cas_write` is a conditional `PutObject` — `If-None-Match: *` when
`expected = Absent`, `If-Match: <etag>` when `expected = Present(etag)`. A
`412 Precondition Failed` maps to `Error::CasFailed`. No lock file.

## Azure

| Aspect | Value |
|--------|-------|
| SDK crate | `azure_storage_blob` / `azure_core` / `azure_identity` 1.0.0 |
| Module | `src/store/azure.rs` |
| Auth | **Entra ID (Azure AD) only** — `ClientSecretCredential` from `tenant_id` / `client_id` / `client_secret`. No account key, no SAS. |
| Config fields | `account`, `container`, `prefix`, `tenant_id`, `client_id`, `client_secret`, `endpoint?`, `encryption?` |
| Test harness | a real Azure account only (opt-in), env-gated by `OMEMFS_AZURE_TEST_*`. No emulator — see Testing strategy. |

**Client construction.** A blob service client built from a `TokenCredential`
(`ClientSecretCredential`) against the service URL
`https://<account>.blob.core.windows.net` (overridable by `endpoint`). Because
auth is Entra-ID-only, **no Azurite** is used for tests (Azurite authenticates
with the shared-key/SAS model omemfs does not support).

**Object ops.**

- `exists`: `get_properties` / a HEAD-equivalent (404 → `false`).
- `size`: `content_length` from blob properties.
- `list_with_sizes`: `list_blobs` over the prefix, draining the `Pager`; size
  is the blob `content_length`.
- `open_read`: download, collecting the `AsyncResponseBody` via the shared
  bridge.
- `write_from`: upload the buffered bytes; idempotent.

**CAS / RootPointer (`AzureRootPointer`).** The token is the blob **ETag**.
`read` is a download / get-properties (404 → `RootToken::Absent`) capturing the
ETag. `cas_write` uses `.if_not_exists()` when `expected = Absent` and an
`if_match: <etag>` option when `expected = Present(etag)`. A precondition
failure is detected via `err.http_status() == 412` and mapped to
`Error::CasFailed`. No lock file.

## GCS

| Aspect | Value |
|--------|-------|
| SDK crate | `google-cloud-storage` 1.15.0, `google-cloud-auth` 1 |
| Module | `src/store/gcs.rs` |
| Auth | service-account JSON (path → read → parse, or inline) / Application Default Credentials / anonymous (emulator) |
| Config fields | `bucket`, `prefix`, `project_id?`, `credentials_json_path?` \| `credentials_json?`, `endpoint?`, `encryption?` |
| Test harness | A real GCS bucket (env-gated by `OMEMFS_GCS_TEST_ENDPOINT`); emulators are limited — see Transport and Testing strategy. |

**Transport (hybrid REST + gRPC).** The `google-cloud-storage` client uses two
transports: the **data plane** (`write_object` / `read_object`) is **REST**
(`/upload/storage/v1/...`), while the **control plane** (`StorageControl`
`get_object` metadata / list) is **gRPC**. Real GCS serves both on a single host
(`storage.googleapis.com:443`), so omemfs's single `endpoint` config is correct
in production. Two consequences:
1. **Status detection must be transport-aware.** A 404/412 condition may arrive
   as a REST HTTP status OR as the equivalent gRPC `Code` (404↔`NotFound`,
   412↔`FailedPrecondition`, 409↔`Aborted`). The `has_status` helper checks both
   (`err.http_status_code()` AND `err.status().code`); checking only the HTTP
   code silently misses gRPC-transported errors (the control-plane `get_object`
   path), which would mis-map a missing index root from "absent" to a hard error
   and a CAS conflict from `CasFailed` to a generic error. Regression-locked by
   the `has_status_*` unit tests in `src/store/cloud/gcs.rs`.
2. **storage-testbench cannot run the full path.** It serves REST and gRPC on
   separate ports, so no single `endpoint` value satisfies both planes; the
   testbench therefore cannot exercise an end-to-end push/pull. See Testing
   strategy.

**Client construction.** A storage client built from service-account
credentials (file path or inline JSON), falling back to ADC. With an `endpoint`
configured and no credentials, the client is built `with_endpoint(...)` and
anonymous credentials.

**Object ops.**

- `exists` / `size` / generation: `StorageControl` get-object — yields the
  object size and its generation number; absent → 404.
- `list_with_sizes`: `list_objects` over the prefix, iterating `by_item`; size
  is the object `size`.
- `open_read`: `read_object`, collecting the stream via the shared bridge.
- `write_from`: `write_object` with the buffered bytes (`send_buffered`);
  idempotent.

**CAS / RootPointer (`GcsRootPointer`).** The token is the object
**generation number** (an `i64`). `read` captures the generation (404 →
`RootToken::Absent`). `cas_write` uses `set_if_generation_match(0)` when
`expected = Absent` (create-if-missing) and `set_if_generation_match(gen)` when
`expected = Present(gen)`. A precondition failure is detected via `has_status(…,
412)` — which matches both the REST `412` and the gRPC `FailedPrecondition` code
(see Transport above) — and mapped to `Error::CasFailed`. No lock file.

## Error mapping

Each adapter maps its SDK error type into `crate::error::Error`. The only
semantically load-bearing mapping is the **412 → `Error::CasFailed`** detection
described per backend above (S3/Azure ETag mismatch, GCS generation mismatch):
this is what surfaces a concurrent push as the "remote has been updated since
last sync" error rather than an opaque I/O failure. A 404 on a read/exists is
mapped to "absent" (not an error) where the contract expects it (`exists` →
`Ok(false)`, root-pointer `read` → `RootToken::Absent`). All other SDK errors
(network failure, auth failure, 5xx, timeout) are mapped to a generic storage
I/O error and returned as `Err`, not coerced to "absent" -- a transient backend
failure must never be indistinguishable from a real 404. `CloudObjectIo::head_exists`
returns `Result<bool, Error>` for exactly this reason, and `CloudObjects::exists`
(the `ObjectsBackend::Cloud` entry point) propagates that `Err` unchanged rather
than defaulting to `false` on any error (the earlier `unwrap_or(false)` behavior
was a bug, not the intended contract: it made a transient network error at
`exists()` time indistinguishable from "object genuinely absent", which could
surface downstream as a spurious `ObjectNotFound` — e.g. `PackReader::resolve`'s
standalone-object probe falls through to a real not-found once `exists` answers
`false` — instead of the transient error it actually was).

## Testing strategy

Integration tests against the cloud backends are **env-gated and skipped by
default** (the default test suite needs no cloud account or emulator). They run
only when the corresponding endpoint/credentials env vars are set.

| Backend | Harness | Gate | CAS coverage |
|---------|---------|------|--------------|
| S3 | MinIO (custom `endpoint` + path-style) | `OMEMFS_S3_TEST_ENDPOINT` | MinIO enforces `If-Match` / `If-None-Match` (verified live) |
| GCS | a real GCS bucket (emulators limited — see below) | `OMEMFS_GCS_TEST_ENDPOINT` | real-bucket `ifGenerationMatch`; the gRPC↔HTTP `412` mapping is unit-tested |
| Azure | a real Azure account (opt-in); otherwise an in-memory fake | `OMEMFS_AZURE_TEST_*` | the in-memory fake (`MemCloud`) enforces the version token + 412 |

The S3 suite (`tests/s3.bats`) has been run end-to-end against MinIO — clone /
push (full, scoped, delete) / pull / concurrent-push CAS / cat / stats / expand
/ pack all pass — so the S3 adapter is verified live, not just by inspection.

**GCS emulators are insufficient for the full path.** Two separate problems:
- *fake-gcs-server* does **not** enforce `ifGenerationMatch` (it ignores the
  precondition and accepts the write), so it cannot exercise the GCS CAS path —
  a concurrent-push test would incorrectly pass on both writers.
- *storage-testbench* does enforce `ifGenerationMatch`, but it serves REST and
  gRPC on **separate ports**, while the client needs one host for both its REST
  data plane and its gRPC control plane (see Transport under GCS). No single
  `endpoint` satisfies both, so it cannot run an end-to-end push/pull either.

Consequently `tests/gcs.bats` targets a **real GCS bucket**; the transport-split
status detection it surfaced is regression-locked by the `has_status_*` unit
tests. (storage-testbench remains "not an officially supported product".)

**Azure has no local emulator** in this design: Azurite only supports the
shared-key / SAS auth model, and omemfs uses Entra ID exclusively. Azure
integration tests therefore require a real account and are opt-in; the
shared in-memory `MemCloud` fake carries the Azure CAS coverage in the default
unit suite (it enforces a version token and returns a CAS failure on a stale
token, exactly like the ETag precondition).

## Parallel transfer (`OMEMFS_TRANSFER_CONCURRENCY`)

The object-graph transfer loops run on a thread pool sharing the single
`&dyn ObjectStore`. The engine (`src/commands/transfer.rs`) does a breadth-first
walk of the objects reachable from a root hash, with N workers each claiming a
hash (dedup via a shared visited set), doing the `exists`/`read`/`write` on the
shared store, and enqueuing the children parsed from the fetched bytes. Because
each worker both consumes and produces work, the queue is an unbounded
`Mutex<VecDeque>` + `Condvar` with an atomic outstanding-work counter for
termination (a bounded channel would deadlock); the first worker to error wins
and all workers drain out.

Two loops use it: `push`'s upload-of-missing and the shared `transfer_objects`
copy (which also serves `clone`'s per-file fetch and `expand`). The working-tree
materialisation phases of `clone`/`pull` stay serial (parent-before-child
filesystem ordering), but their remote fetches go through `transfer_objects` and
so are parallelised.

The knob `OMEMFS_TRANSFER_CONCURRENCY` sets the worker count: default **1**
(serial) for the local backend, **8** for cloud backends (resolved via
`ObjectStore::default_transfer_concurrency`, which `StatsStore` / `PackReader` /
`PackWriter` delegate to their underlying remote). At concurrency 1 the callers
run their original serial loop unchanged, so the local path is byte-identical.
Object writes are content-addressed and idempotent, so they need no ordering or
locking between workers; the only mutable pointer, the index root, is written by
a single CAS in `finish()` after all parallel PUTs have joined (see
`03_sync_model.md` and `12_locking.md`).

Peak transfer memory is governed by a **separate** knob,
`OMEMFS_TRANSFER_MEMORY_BUDGET` (default 64 MiB), not by the worker count. The
worker count is tuned for request parallelism / latency hiding; the byte budget
is a counting semaphore in `src/commands/transfer.rs` that bounds the total size
of object buffers resident across all workers at once, so raising concurrency
does not raise the memory ceiling. Each cloud op still buffers a whole object
(bounded at ≤ ~16 MiB by L3 chunking), and the `StatsStore` wrapper's extra
counting copy (`src/store/stats.rs:126-127`) is charged against the same budget;
see `02_storage_format.md`, "Two independent knobs: concurrency and memory" for
the full model and the clamp-to-capacity rule that keeps a single oversized
object from deadlocking. On S3 specifically, `put_multipart`'s parts are sliced
from the buffered object as refcounted `Bytes` views rather than copied per part
(`chunk.to_vec()`), so a large multipart upload does not transiently double its
own buffer.
