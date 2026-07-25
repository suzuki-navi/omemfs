pub mod cloud;
pub mod local;
pub mod objects_dir;
pub mod remote_objects_dir;
pub mod stats;

use std::io;

use crate::error::Error;
use crate::object::Hash;

/// Low-level object storage backend — stores and retrieves raw bytes by hash.
pub trait ObjectStore: Send + Sync {
    /// Return a short human-readable name for this store, used in progress logs.
    fn store_name(&self) -> &str {
        "store"
    }

    /// Return `true` if the object exists in this store.
    fn exists(&self, hash: &Hash) -> Result<bool, Error>;

    /// Return the stored byte length of the object addressed by `hash`.
    /// Errors if the object does not exist.
    fn size(&self, hash: &Hash) -> Result<u64, Error>;

    /// List every stored object as (storage_key_hex, byte_size).
    /// Backends that can list with sizes in one call (S3/Azure/GCS LIST) avoid
    /// per-object HEAD. Eager Vec for now.
    fn list_with_sizes(&self) -> Result<Vec<(String, u64)>, Error>;

    /// Open the stored (encoded) bytes for `hash` as a stream.
    /// Returns raw stored bytes (encrypted + compressed); decoding is the
    /// caller's responsibility.
    fn open_read(&self, hash: &Hash) -> Result<Box<dyn io::Read>, Error>;

    /// Write bytes from `reader` into the store at address `hash`. Idempotent:
    /// if the object already exists the write is a no-op.
    fn write_from(&self, hash: &Hash, reader: &mut dyn io::Read) -> Result<(), Error>;

    /// Default number of parallel workers to use when transferring many objects
    /// to/from this store (see `commands::transfer`). Local-filesystem stores
    /// return 1 (serial — disk I/O does not benefit from overlap and the serial
    /// path stays byte-identical). Cloud stores return a higher default so that
    /// network round-trips overlap. Always overridable by the
    /// `OMEMFS_TRANSFER_CONCURRENCY` environment variable.
    fn default_transfer_concurrency(&self) -> usize {
        1
    }
}
