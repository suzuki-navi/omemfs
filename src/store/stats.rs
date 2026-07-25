/// StatsStore — a thin wrapper around a remote ObjectStore that counts I/O operations.
///
/// Wraps only the remote store (not the local cache). All counters are shared via
/// `Arc<IoRecord>` so the same record can be passed to multiple helpers and pack
/// layer code within a single command run.
use std::io;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

use crate::error::Error;
use crate::object::Hash;
use crate::store::ObjectStore;

// ---------------------------------------------------------------------------
// IoRecord
// ---------------------------------------------------------------------------

/// In-memory accumulator for one command run's remote I/O.
pub struct IoRecord {
    pub exists_found: AtomicU64,
    pub exists_miss: AtomicU64,
    pub reads: AtomicU64,
    pub read_bytes: AtomicU64,
    pub writes: AtomicU64,
    pub write_bytes: AtomicU64,
    /// Filled in by calling `set_pack_stats` after PackWriter::finish().
    pub pack_files_written: AtomicU64,
    pub pack_sizes_bytes: Mutex<Vec<u64>>,
    /// Filled in by calling `set_delta_count_after` after `PackWriter::finish()`
    /// returns, for `push` only. `None` for every other command (io_stats
    /// then omits `deltas_after` from the record; see design/04 "io_stats.jsonl"
    /// Notes).
    pub delta_count_after: Mutex<Option<u64>>,
}

impl Default for IoRecord {
    fn default() -> Self {
        IoRecord {
            exists_found: AtomicU64::new(0),
            exists_miss: AtomicU64::new(0),
            reads: AtomicU64::new(0),
            read_bytes: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            write_bytes: AtomicU64::new(0),
            pack_files_written: AtomicU64::new(0),
            pack_sizes_bytes: Mutex::new(Vec::new()),
            delta_count_after: Mutex::new(None),
        }
    }
}

impl IoRecord {
    /// Store pack file statistics gathered from `PackWriter::io_pack_stats()`.
    pub fn set_pack_stats(&self, count: u64, sizes: Vec<u64>) {
        self.pack_files_written.store(count, Relaxed);
        *self.pack_sizes_bytes.lock().unwrap() = sizes;
    }

    /// Store the post-CAS delta index count returned by `PackWriter::finish()`.
    pub fn set_delta_count_after(&self, count: u64) {
        *self.delta_count_after.lock().unwrap() = Some(count);
    }
}

// ---------------------------------------------------------------------------
// StatsStore
// ---------------------------------------------------------------------------

/// Wraps a `Box<dyn ObjectStore>` and records I/O stats into a shared `IoRecord`.
pub struct StatsStore {
    inner: Box<dyn ObjectStore>,
    record: Arc<IoRecord>,
}

impl StatsStore {
    pub fn new(inner: Box<dyn ObjectStore>, record: Arc<IoRecord>) -> Self {
        StatsStore { inner, record }
    }

    /// Return a clone of the shared IoRecord handle.
    #[allow(dead_code)]
    pub fn io_record(&self) -> Arc<IoRecord> {
        Arc::clone(&self.record)
    }
}

impl ObjectStore for StatsStore {
    fn store_name(&self) -> &str {
        self.inner.store_name()
    }

    fn default_transfer_concurrency(&self) -> usize {
        self.inner.default_transfer_concurrency()
    }

    fn exists(&self, hash: &Hash) -> Result<bool, Error> {
        let found = self.inner.exists(hash)?;
        if found {
            self.record.exists_found.fetch_add(1, Relaxed);
        } else {
            self.record.exists_miss.fetch_add(1, Relaxed);
        }
        Ok(found)
    }

    fn size(&self, hash: &Hash) -> Result<u64, Error> {
        let result = self.inner.size(hash);
        match &result {
            Ok(_) => self.record.exists_found.fetch_add(1, Relaxed),
            Err(_) => self.record.exists_miss.fetch_add(1, Relaxed),
        };
        result
    }

    fn list_with_sizes(&self) -> Result<Vec<(String, u64)>, Error> {
        // A LIST is one cheap batch request; do not count it in per-op io counters.
        self.inner.list_with_sizes()
    }

    fn open_read(&self, hash: &Hash) -> Result<Box<dyn io::Read>, Error> {
        self.record.reads.fetch_add(1, Relaxed);
        let reader = self.inner.open_read(hash)?;
        // Wrap in a CountingReadProxy so bytes are counted as the caller reads.
        Ok(Box::new(CountingReadProxy::new(
            reader,
            Arc::clone(&self.record),
        )))
    }

    fn write_from(&self, hash: &Hash, reader: &mut dyn io::Read) -> Result<(), Error> {
        // Check whether the inner store will skip the write (idempotent: skip if exists).
        // We count bytes only for writes that actually happen.
        let already_present = self.inner.exists(hash).unwrap_or(false);
        if already_present {
            // Inner will skip; drain the reader (required by interface contract) but
            // do not count this as a write.
            io::copy(reader, &mut io::sink()).map_err(Error::Io)?;
            return Ok(());
        }

        // Not present: the write will happen. Buffer the bytes so we can count them
        // and then pass to the inner store.
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).map_err(Error::Io)?;
        let byte_count = buf.len() as u64;

        let mut cursor = io::Cursor::new(&buf);
        self.inner.write_from(hash, &mut cursor)?;

        // Count bytes only after a successful write.
        self.record.writes.fetch_add(1, Relaxed);
        self.record.write_bytes.fetch_add(byte_count, Relaxed);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// CountingReadProxy — wraps a Box<dyn Read> to count bytes as they are read
// ---------------------------------------------------------------------------

struct CountingReadProxy {
    inner: Box<dyn io::Read>,
    record: Arc<IoRecord>,
    bytes: u64,
}

impl CountingReadProxy {
    fn new(inner: Box<dyn io::Read>, record: Arc<IoRecord>) -> Self {
        CountingReadProxy {
            inner,
            record,
            bytes: 0,
        }
    }
}

impl io::Read for CountingReadProxy {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.bytes += n as u64;
        Ok(n)
    }
}

impl Drop for CountingReadProxy {
    fn drop(&mut self) {
        self.record.read_bytes.fetch_add(self.bytes, Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::local::LocalStore;
    use std::sync::atomic::Ordering::Relaxed;
    use tempfile::TempDir;

    fn make_stats_store(tmp: &TempDir) -> (StatsStore, Arc<IoRecord>) {
        let record = Arc::new(IoRecord::default());
        let inner = LocalStore::for_cache(tmp.path());
        let store = StatsStore::new(Box::new(inner), Arc::clone(&record));
        (store, record)
    }

    #[test]
    fn delta_count_after_defaults_to_none() {
        let record = IoRecord::default();
        assert!(record.delta_count_after.lock().unwrap().is_none());
    }

    #[test]
    fn set_delta_count_after_stores_the_value() {
        let record = IoRecord::default();
        record.set_delta_count_after(7);
        assert_eq!(*record.delta_count_after.lock().unwrap(), Some(7));
    }

    #[test]
    fn size_on_present_object_increments_exists_found() {
        let tmp = TempDir::new().unwrap();
        let (store, record) = make_stats_store(&tmp);

        let data = b"stats size present";
        let hash = crate::object::Hash::compute(data);
        store
            .write_from(&hash, &mut std::io::Cursor::new(data))
            .unwrap();

        // Reset write-side counters; we only care about HEAD counters here.
        let before_found = record.exists_found.load(Relaxed);
        let before_miss = record.exists_miss.load(Relaxed);

        let result = store.size(&hash);
        assert!(result.is_ok(), "size of present object must succeed");
        assert_eq!(
            record.exists_found.load(Relaxed),
            before_found + 1,
            "exists_found must increment by 1 for a successful size call"
        );
        assert_eq!(
            record.exists_miss.load(Relaxed),
            before_miss,
            "exists_miss must not change for a successful size call"
        );
    }

    #[test]
    fn size_on_absent_object_increments_exists_miss() {
        let tmp = TempDir::new().unwrap();
        let (store, record) = make_stats_store(&tmp);

        let absent_hash = crate::object::Hash::compute(b"does not exist in store");

        let before_found = record.exists_found.load(Relaxed);
        let before_miss = record.exists_miss.load(Relaxed);

        let result = store.size(&absent_hash);
        assert!(result.is_err(), "size of absent object must fail");
        assert_eq!(
            record.exists_miss.load(Relaxed),
            before_miss + 1,
            "exists_miss must increment by 1 for a failed size call"
        );
        assert_eq!(
            record.exists_found.load(Relaxed),
            before_found,
            "exists_found must not change for a failed size call"
        );
    }
}
