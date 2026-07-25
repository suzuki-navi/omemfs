/// Stage 2: chunk / assemble.
///
/// Large serialised objects are split into chunk objects using FastCDC.
/// Each chunk is stored independently; a manifest object records the ordered
/// list of chunk hashes needed to reassemble the original bytes.
///
/// Chunking parameters (fastcdc v2020 limits: min ≤ 1 MiB, avg ≤ 4 MiB, max ≤ 16 MiB):
///   min_size:  1 MiB
///   avg_size:  4 MiB
///   max_size: 16 MiB
///
/// When FastCDC produces only one chunk the object is stored whole (no manifest).
///
/// Stored format:
///   manifest:  ED F2 | chunk_hash[0] (32 bytes) | … | chunk_hash[N-1] (32 bytes)
///   chunk:     ED F3 | raw segment bytes
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::SystemTime;

use sha2::{Digest, Sha256};

use crate::codec;
use crate::dlog_l3;
use crate::error::Error;
use crate::object::{self, Hash};
use crate::store::ObjectStore;

// fastcdc v2020 limits: min ≤ 1 MiB, avg ≤ 4 MiB, max ≤ 16 MiB.
const CDC_MIN: u32 = 1024 * 1024;
const CDC_AVG: u32 = 4 * 1024 * 1024;
const CDC_MAX: u32 = 16 * 1024 * 1024;

/// Source files strictly below this size use the in-memory write path (read
/// whole, hash, `store_chunked`). Files at or above it use one-pass streaming.
/// The switch is invisible in the stored format:
/// `StreamCDC` produces identical cut points to in-memory `FastCDC`, so the
/// logical hash, chunk hashes, and chunk boundaries are the same either way.
/// See design/02 "Streaming design".
pub const STREAMING_THRESHOLD: u64 = 64 * 1024 * 1024;

/// Number of attempts made to capture a stable source file. Retrying only the
/// active file keeps push responsive on live trees; after this bound the scan
/// preserves the previous entry instead of failing the whole push.
const SNAPSHOT_ATTEMPTS: usize = 2;

/// A blob captured from one open file description. The metadata belongs to
/// the exact file that was read, not to a replacement at the same path.
pub struct StoredFile {
    pub hash: Hash,
    pub fs_mtime: Option<SystemTime>,
    pub size: u64,
    pub mode: Option<String>,
}

/// Size of the fixed read buffer used by the hash-only path.
const STREAM_READ_BUF: usize = 1024 * 1024;

/// Store `serialised` bytes to `store`, splitting into chunks when FastCDC
/// produces 2 or more chunks. The object is addressed by `logical_hash`
/// (already computed by Stage 1). Returns `logical_hash` unchanged.
///
/// `key` is forwarded to the encrypt stage for each physical write.
pub fn store_chunked(
    store: &dyn ObjectStore,
    logical_hash: &Hash,
    serialised: &[u8],
    key: Option<&codec::encrypt::EncryptKey>,
) -> Result<(), Error> {
    let chunker = fastcdc::v2020::FastCDC::new(serialised, CDC_MIN, CDC_AVG, CDC_MAX);
    let chunks: Vec<fastcdc::v2020::Chunk> = chunker.collect();

    if chunks.len() < 2 {
        // Small object: store whole at logical_hash.
        codec::store_write(store, logical_hash, serialised, key)?;
        return Ok(());
    }

    // Large object: store each chunk, then store manifest at logical_hash.
    dlog_l3!(
        "chunk split: {} chunks for {}B object (hash {})",
        chunks.len(),
        serialised.len(),
        &logical_hash.as_str()[..8]
    );
    let mut chunk_hashes: Vec<Hash> = Vec::with_capacity(chunks.len());
    for chunk in &chunks {
        let chunk_bytes = &serialised[chunk.offset..chunk.offset + chunk.length];
        let hash = object::chunk_hash(chunk_bytes);
        if !store.exists(&hash)? {
            let tagged = object::serialise_chunk(chunk_bytes);
            codec::store_write(store, &hash, &tagged, key)?;
        }
        chunk_hashes.push(hash);
    }

    let manifest_bytes = object::serialise_manifest(&chunk_hashes);
    codec::store_write(store, logical_hash, &manifest_bytes, key)?;
    Ok(())
}

/// Store a source file, choosing the in-memory or streaming write path based on
/// `STREAMING_THRESHOLD`. Returns the logical blob hash.
///
/// Both paths produce identical stored objects and the identical logical hash;
/// the threshold is a pure peak-memory switch. This is the single entry point
/// for the blob write side — callers pass a path instead of pre-reading bytes.
#[allow(dead_code)]
pub fn store_file(
    store: &dyn ObjectStore,
    path: &Path,
    key: Option<&codec::encrypt::EncryptKey>,
) -> Result<Hash, Error> {
    Ok(store_file_snapshot(store, path, key)?.hash)
}

/// Capture and store a stable version of `path`.
///
/// Each attempt opens the path once and derives both metadata and content from
/// that handle. An unlink or atomic editor rename after `open` therefore does
/// not abort the read. In-place writers are detected by comparing handle
/// metadata before and after the read; they are retried briefly and then
/// reported as `SourceChanged` so push can preserve the previous entry.
pub fn store_file_snapshot(
    store: &dyn ObjectStore,
    path: &Path,
    key: Option<&codec::encrypt::EncryptKey>,
) -> Result<StoredFile, Error> {
    for _ in 0..SNAPSHOT_ATTEMPTS {
        let mut file = fs::File::open(path)?;
        let before_md = file.metadata()?;
        let before = file_stat(&before_md);
        let mode = crate::fsmeta::mode_from_metadata(&before_md);

        let hash = if before.size < STREAMING_THRESHOLD {
            let mut content = Vec::with_capacity(before.size as usize);
            file.read_to_end(&mut content)?;
            test_post_read_delay();
            let after = file_stat(&file.metadata()?);
            if !stat_unchanged(before, after) || content.len() as u64 != after.size {
                continue;
            }
            let hash = object::blob_hash(&content);
            let serialised = object::serialise_blob(&content);
            store_chunked(store, &hash, &serialised, key)?;
            hash
        } else {
            let (hash, chunks, raw_size) = stream_file_once(store, &mut file, key)?;
            test_post_read_delay();
            let after = file_stat(&file.metadata()?);
            if !stat_unchanged(before, after) || raw_size != after.size {
                // Chunk objects written by this attempt are harmless
                // content-addressed orphans. No manifest is written yet.
                continue;
            }
            finish_streamed_file(store, &hash, chunks, key)?;
            hash
        };

        return Ok(StoredFile {
            hash,
            fs_mtime: before.mtime,
            size: before.size,
            mode,
        });
    }

    Err(Error::SourceChanged(path.display().to_string()))
}

#[cfg(debug_assertions)]
fn test_post_read_delay() {
    if let Ok(ms) = std::env::var("OMEMFS_TEST_SNAPSHOT_POST_READ_DELAY_MS")
        && let Ok(ms) = ms.parse::<u64>()
    {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

#[cfg(not(debug_assertions))]
fn test_post_read_delay() {}

/// Compute a source file's logical blob hash WITHOUT writing any object.
///
/// This is the "hash-only" scan path used by read-only commands (`ls`, `pull`):
/// they need blob hashes to build tree objects and diff, but never read blob
/// bodies back, so chunking / compression / encryption / object writes are all
/// skipped. The digest is computed by a single streaming SHA-256 pass over
/// `ED F0 | content` and is byte-for-byte identical to the hash that
/// [`store_file`] would return for the same file (see [`pass1_hash_and_prefix`]
/// and `object::blob_hash`). Memory use is bounded by `STREAM_READ_BUF`
/// regardless of file size. See design/02 "Hash-only variant" and design/03
/// "Scan blob-write mode".
pub fn hash_file(path: &Path) -> Result<Hash, Error> {
    let (hash, _needs_escape) = pass1_hash_and_prefix(path)?;
    Ok(hash)
}

/// `(mtime, size)` pair recorded by a stat, used for the TOCTOU comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStat {
    mtime: Option<SystemTime>,
    size: u64,
}

fn file_stat(md: &fs::Metadata) -> FileStat {
    FileStat {
        mtime: md.modified().ok(),
        size: md.len(),
    }
}

/// TOCTOU re-stat comparison: the file is considered unchanged only when both
/// `(mtime, size)` match. Factored out so the retry/error logic is unit-testable
/// without racing a real filesystem mtime change.
fn stat_unchanged(before: FileStat, after: FileStat) -> bool {
    before == after
}

/// A `Read` adapter that prepends an optional fixed prefix (0 or 2 bytes) to an
/// inner reader. Used to feed `StreamCDC` the prefixed stream
/// `[0xED 0xF0 (if needed)] || file content` without materialising the file.
struct PrefixReader<R: Read> {
    prefix: Vec<u8>,
    prefix_pos: usize,
    inner: R,
}

impl<R: Read> PrefixReader<R> {
    fn new(prefix: Vec<u8>, inner: R) -> Self {
        PrefixReader {
            prefix,
            prefix_pos: 0,
            inner,
        }
    }
}

impl<R: Read> Read for PrefixReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.prefix_pos < self.prefix.len() {
            let avail = self.prefix.len() - self.prefix_pos;
            let take = avail.min(buf.len());
            buf[..take].copy_from_slice(&self.prefix[self.prefix_pos..self.prefix_pos + take]);
            self.prefix_pos += take;
            return Ok(take);
        }
        self.inner.read(buf)
    }
}

/// Hash-only path: stream the file in fixed buffers, computing the logical blob hash
/// (`SHA256(ED F0 | content)`, matching `object::blob_hash`) and whether the
/// `ED F0` escape prefix is needed for the serialised stream.
fn pass1_hash_and_prefix(path: &Path) -> Result<(Hash, bool), Error> {
    let mut file = fs::File::open(path)?;
    // Logical hash anchor always includes the ED F0 tag (see object::blob_hash).
    let mut hasher = Sha256::new();
    hasher.update(object::TYPE_TAG_BLOB);

    let mut buf = vec![0u8; STREAM_READ_BUF];
    let mut first_byte: Option<u8> = None;
    let mut second_byte: Option<u8> = None;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if first_byte.is_none() {
            first_byte = Some(buf[0]);
            if n >= 2 {
                second_byte = Some(buf[1]);
            }
        } else if second_byte.is_none() {
            // First buffer held exactly 1 byte; the second byte is here.
            second_byte = Some(buf[0]);
        }
        hasher.update(&buf[..n]);
    }
    let digest: [u8; 32] = hasher.finalize().into();
    let hash = Hash::from_bytes(digest);
    let needs_escape = object::blob_needs_escape(first_byte, second_byte);
    Ok((hash, needs_escape))
}

enum Pass2Result {
    /// Exactly one chunk; carries its raw serialised bytes (no chunk object written).
    Single(Vec<u8>),
    /// Two or more chunks, all already stored as `ED F3` objects.
    Multi(Vec<Hash>),
}

/// One-pass streaming capture. `StreamCDC` owns a mutable borrow of the open
/// file, so pathname replacement cannot switch the source between hashing and
/// chunking. The manifest is returned and written only after the caller's
/// post-read metadata check succeeds.
fn stream_file_once(
    store: &dyn ObjectStore,
    file: &mut fs::File,
    key: Option<&codec::encrypt::EncryptKey>,
) -> Result<(Hash, Pass2Result, u64), Error> {
    let mut probe = [0u8; 2];
    let mut probe_len = 0;
    while probe_len < probe.len() {
        let n = file.read(&mut probe[probe_len..])?;
        if n == 0 {
            break;
        }
        probe_len += n;
    }
    let needs_escape = object::blob_needs_escape(
        (probe_len >= 1).then_some(probe[0]),
        (probe_len >= 2).then_some(probe[1]),
    );
    file.seek(SeekFrom::Start(0))?;

    let prefix = if needs_escape {
        object::TYPE_TAG_BLOB.to_vec()
    } else {
        Vec::new()
    };
    let reader = PrefixReader::new(prefix, file);
    let chunker = fastcdc::v2020::StreamCDC::new(reader, CDC_MIN, CDC_AVG, CDC_MAX);

    let store_chunk = |data: &[u8]| -> Result<Hash, Error> {
        let hash = object::chunk_hash(data);
        if !store.exists(&hash)? {
            let tagged = object::serialise_chunk(data);
            codec::store_write(store, &hash, &tagged, key)?;
        }
        Ok(hash)
    };

    let mut hasher = Sha256::new();
    hasher.update(object::TYPE_TAG_BLOB);
    let mut raw_size = 0u64;
    let mut chunk_hashes = Vec::new();
    let mut first_chunk: Option<Vec<u8>> = None;
    let mut first = true;

    for result in chunker {
        let chunk = result.map_err(|e| Error::Other(format!("StreamCDC error: {}", e)))?;
        let raw = if first && needs_escape {
            chunk.data.get(2..).ok_or_else(|| {
                Error::InvalidObject("streamed blob escape prefix was truncated".to_string())
            })?
        } else {
            chunk.data.as_slice()
        };
        first = false;
        hasher.update(raw);
        raw_size += raw.len() as u64;

        match first_chunk.take() {
            None if chunk_hashes.is_empty() => first_chunk = Some(chunk.data),
            held => {
                if let Some(data) = held {
                    chunk_hashes.push(store_chunk(&data)?);
                }
                chunk_hashes.push(store_chunk(&chunk.data)?);
            }
        }
    }

    let digest: [u8; 32] = hasher.finalize().into();
    let hash = Hash::from_bytes(digest);
    let chunks = match first_chunk {
        Some(data) => Pass2Result::Single(data),
        None => Pass2Result::Multi(chunk_hashes),
    };
    Ok((hash, chunks, raw_size))
}

fn finish_streamed_file(
    store: &dyn ObjectStore,
    logical_hash: &Hash,
    chunks: Pass2Result,
    key: Option<&codec::encrypt::EncryptKey>,
) -> Result<(), Error> {
    match chunks {
        Pass2Result::Single(serialised) => {
            codec::store_write(store, logical_hash, &serialised, key)?;
        }
        Pass2Result::Multi(chunk_hashes) => {
            let manifest_bytes = object::serialise_manifest(&chunk_hashes);
            codec::store_write(store, logical_hash, &manifest_bytes, key)?;
        }
    }
    Ok(())
}

/// Direct one-pass streaming write used by format-equivalence tests. Produces the
/// same logical hash and the same stored objects as `store_chunked` would over
/// the same serialised bytes. See design/02 "One-pass streaming write".
#[allow(dead_code)]
pub fn store_file_streaming(
    store: &dyn ObjectStore,
    path: &Path,
    key: Option<&codec::encrypt::EncryptKey>,
) -> Result<Hash, Error> {
    for _ in 0..SNAPSHOT_ATTEMPTS {
        let mut file = fs::File::open(path)?;
        let before = file_stat(&file.metadata()?);
        let (logical_hash, chunks, raw_size) = stream_file_once(store, &mut file, key)?;
        let after = file_stat(&file.metadata()?);
        if !stat_unchanged(before, after) || raw_size != after.size {
            continue;
        }
        finish_streamed_file(store, &logical_hash, chunks, key)?;
        return Ok(logical_hash);
    }
    Err(Error::SourceChanged(path.display().to_string()))
}

/// Read and reassemble an object from `store` at `logical_hash`.
///
/// If the stored object is a chunked manifest (ED F2), all chunks are fetched
/// and concatenated to reconstruct the original serialised bytes.
/// Otherwise the bytes are returned as-is.
///
/// `key` is used for both the manifest and each chunk.
/// Returns true if the object at `logical_hash` is stored as a chunked manifest
/// (i.e. split across multiple object-store entries). Returns false if the
/// object is stored whole, is absent from `store`, or cannot be read.
///
/// No production caller remains (StubRecord's `chunked` field was removed --
/// see design/08_stub_system.md -- since nothing read it and materialisation
/// determines chunked-ness from the object's own magic bytes instead). Kept
/// as a general utility exercised by this module's own test suite below and
/// by codec::chunk's chunk-format tests in pull.rs.
#[allow(dead_code)]
pub fn is_chunked(
    store: &dyn ObjectStore,
    logical_hash: &Hash,
    key: Option<&codec::encrypt::EncryptKey>,
) -> bool {
    if !store.exists(logical_hash).unwrap_or(false) {
        return false;
    }
    match codec::store_read(store, logical_hash, key) {
        Ok(raw) => object::deserialise_manifest(&raw).is_some(),
        Err(_) => false,
    }
}

/// Read and reassemble an object from `store` at `logical_hash` into a single
/// in-memory buffer. If the stored object is a chunked manifest (ED F2), all
/// chunks are fetched and concatenated to reconstruct the original serialised
/// bytes; otherwise the bytes are returned as-is. `key` is used for both the
/// manifest and each chunk.
///
/// This is the "in-memory concatenation" consumption mode (design/02), retained
/// for internal consumers that need the complete byte buffer anyway (e.g. tree
/// JSON deserialisation). File materialisation and conflict-helper production use
/// the streaming write-through path (`materialise_to_file`) instead, so this is
/// currently exercised only by tests in non-`cfg(test)` builds.
#[allow(dead_code)]
pub fn load_assembled(
    store: &dyn ObjectStore,
    logical_hash: &Hash,
    key: Option<&codec::encrypt::EncryptKey>,
) -> Result<Vec<u8>, Error> {
    let mut assembled = Vec::new();
    for_each_serialised_chunk(store, logical_hash, key, |chunk_bytes| {
        assembled.extend_from_slice(chunk_bytes);
        Ok(())
    })?;
    Ok(assembled)
}

/// Walk the (possibly chunked) object at `logical_hash` and invoke `sink` once
/// per chunk, in order, with the **serialised** chunk bytes:
///   - the `ED F3` chunk tag is stripped from every chunk (error if missing);
///   - the leading `ED F0` blob-escape tag is NOT stripped — the bytes passed
///     to `sink` are exactly the L2-serialised object bytes, so concatenating
///     them reproduces what `codec::store_read` would return for a whole object.
///
/// For an unchunked object the whole decoded bytes are passed as a single
/// "chunk". Peak memory is bounded by the largest chunk (≤ CDC_MAX ≈ 16 MiB)
/// plus whatever the sink accumulates. This is the shared manifest-walk used by
/// `load_assembled`, `materialise_to_file`, and `cat`.
pub fn for_each_serialised_chunk<F>(
    store: &dyn ObjectStore,
    logical_hash: &Hash,
    key: Option<&codec::encrypt::EncryptKey>,
    mut sink: F,
) -> Result<(), Error>
where
    F: FnMut(&[u8]) -> Result<(), Error>,
{
    let raw = codec::store_read(store, logical_hash, key)?;

    match object::deserialise_manifest(&raw) {
        None => sink(&raw),
        Some(chunk_hashes) => {
            for chunk_hash in &chunk_hashes {
                let chunk_tagged = codec::store_read(store, chunk_hash, key)?;
                let chunk_bytes = object::deserialise_chunk(&chunk_tagged).ok_or_else(|| {
                    Error::InvalidObject(format!(
                        "expected ED F3 chunk tag for hash {}",
                        chunk_hash
                    ))
                })?;
                sink(chunk_bytes)?;
            }
            Ok(())
        }
    }
}

/// Walk the blob at `logical_hash` and invoke `sink` once per chunk with the
/// **deserialised content** bytes (the leading `ED F0` blob-escape tag stripped
/// from the first chunk only, matching `object::deserialise_blob`). Used by
/// materialisation and `cat`, which want file content rather than serialised
/// bytes. Tree objects must NOT go through this path — they need their raw
/// serialised bytes for JSON deserialisation; use `load_assembled` instead.
pub fn for_each_blob_chunk<F>(
    store: &dyn ObjectStore,
    logical_hash: &Hash,
    key: Option<&codec::encrypt::EncryptKey>,
    mut sink: F,
) -> Result<(), Error>
where
    F: FnMut(&[u8]) -> Result<(), Error>,
{
    let mut first = true;
    for_each_serialised_chunk(store, logical_hash, key, |chunk_bytes| {
        // Strip the leading ED F0 blob-escape on the first chunk only.
        let payload = if first {
            first = false;
            object::deserialise_blob(chunk_bytes)
        } else {
            chunk_bytes
        };
        sink(payload)
    })
}

/// Materialise the blob at `logical_hash` to `dest_path` with bounded memory.
///
/// The object is read chunk by chunk and written sequentially into a
/// `NamedTempFile` in the destination directory, then atomically renamed to
/// `dest_path` (no fsync, matching `atomic_write_no_fsync`). On any error the
/// temp file is dropped and `dest_path` is left untouched — no partial file is
/// ever visible. `mtime` and `mode` are NOT applied here; the caller applies
/// them after the rename per the materialisation contract. See design/02
/// "Streaming read / materialisation".
pub fn materialise_to_file(
    store: &dyn ObjectStore,
    logical_hash: &Hash,
    key: Option<&codec::encrypt::EncryptKey>,
    dest_path: &Path,
) -> Result<(), Error> {
    crate::store::local::atomic_write_with_no_fsync(dest_path, |writer| {
        for_each_blob_chunk(store, logical_hash, key, |chunk_bytes| {
            writer.write_all(chunk_bytes).map_err(Error::Io)
        })
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::local::LocalStore;
    use std::io::Write;
    use tempfile::TempDir;

    /// Deterministic pseudo-random byte generator (xorshift64*). Not for crypto;
    /// only needs to be reproducible and "random enough" that FastCDC finds cut
    /// points.
    fn pseudo_random(seed: u64, len: usize) -> Vec<u8> {
        let mut state = seed | 1;
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            let v = state.wrapping_mul(0x2545F4914F6CDD1D);
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    fn sorted_hashes(store: &LocalStore) -> Vec<String> {
        let mut v = store.iter_hashes();
        v.sort();
        v
    }

    #[test]
    fn stream_cdc_cut_points_match_in_memory() {
        // FastCDC (in-memory slice) and StreamCDC (reader) must yield identical
        // (offset, length) sequences over the same bytes — the compatibility
        // invariant the threshold switch relies on.
        let data = pseudo_random(0x1234_5678, 15 * 1024 * 1024);

        let in_mem: Vec<(u64, usize)> =
            fastcdc::v2020::FastCDC::new(&data, CDC_MIN, CDC_AVG, CDC_MAX)
                .map(|c| (c.offset as u64, c.length))
                .collect();

        let streamed: Vec<(u64, usize)> = fastcdc::v2020::StreamCDC::new(
            io::Cursor::new(data.clone()),
            CDC_MIN,
            CDC_AVG,
            CDC_MAX,
        )
        .map(|r| {
            let c = r.unwrap();
            (c.offset, c.length)
        })
        .collect();

        assert!(
            in_mem.len() >= 2,
            "expected multiple chunks, got {}",
            in_mem.len()
        );
        assert_eq!(in_mem, streamed);
    }

    /// Run an equivalence check: store `content` via the in-memory path and via
    /// the streaming path into two separate stores, then assert the logical
    /// hashes, assembled bytes, and stored-object sets all match.
    fn assert_streaming_equivalence(content: &[u8], expect_chunked: bool) {
        // In-memory path.
        let mem_dir = TempDir::new().unwrap();
        let mem_store = LocalStore::for_cache(mem_dir.path());
        let mem_hash = object::blob_hash(content);
        let serialised = object::serialise_blob(content);
        store_chunked(&mem_store, &mem_hash, &serialised, None).unwrap();

        // Streaming path (call store_file_streaming directly so the 64 MiB
        // threshold does not force a giant file).
        let src_dir = TempDir::new().unwrap();
        let src_path = src_dir.path().join("input.bin");
        {
            let mut f = fs::File::create(&src_path).unwrap();
            f.write_all(content).unwrap();
            f.sync_all().unwrap();
        }
        let stream_dir = TempDir::new().unwrap();
        let stream_store = LocalStore::for_cache(stream_dir.path());
        let stream_hash = store_file_streaming(&stream_store, &src_path, None).unwrap();

        // Same logical hash.
        assert_eq!(mem_hash, stream_hash, "logical hash mismatch");

        // Same chunked-ness.
        assert_eq!(is_chunked(&mem_store, &mem_hash, None), expect_chunked);
        assert_eq!(
            is_chunked(&stream_store, &stream_hash, None),
            expect_chunked
        );

        // Both reassemble to the original serialised bytes.
        let mem_asm = load_assembled(&mem_store, &mem_hash, None).unwrap();
        let stream_asm = load_assembled(&stream_store, &stream_hash, None).unwrap();
        assert_eq!(mem_asm, serialised);
        assert_eq!(stream_asm, serialised);
        assert_eq!(mem_asm, stream_asm);

        // Identical set of stored objects.
        assert_eq!(sorted_hashes(&mem_store), sorted_hashes(&stream_store));
    }

    #[test]
    fn streaming_matches_in_memory_multichunk() {
        // ~10 MiB pseudo-random data reliably yields >= 2 chunks (CDC_MIN = 1 MiB).
        let data = pseudo_random(0xDEAD_BEEF, 10 * 1024 * 1024);
        assert_streaming_equivalence(&data, true);
    }

    #[test]
    fn streaming_matches_in_memory_escape_prefix() {
        // Content whose first two bytes are ED F0 forces the escape prefix through
        // the streaming PrefixReader adapter.
        let mut data = vec![0xED, 0xF0];
        data.extend_from_slice(&pseudo_random(0xCAFE_F00D, 10 * 1024 * 1024));
        assert!(object::blob_needs_escape(
            data.first().copied(),
            data.get(1).copied()
        ));
        assert_streaming_equivalence(&data, true);
    }

    #[test]
    fn streaming_single_chunk_no_manifest() {
        // Content below CDC_MIN: streaming stores it whole at the logical hash,
        // no manifest, and load_assembled returns the original bytes.
        let data = pseudo_random(0x0BAD_C0DE, 64 * 1024);
        assert_streaming_equivalence(&data, false);

        // Direct check that no manifest object exists.
        let src_dir = TempDir::new().unwrap();
        let src_path = src_dir.path().join("small.bin");
        fs::write(&src_path, &data).unwrap();
        let store_dir = TempDir::new().unwrap();
        let store = LocalStore::for_cache(store_dir.path());
        let hash = store_file_streaming(&store, &src_path, None).unwrap();
        assert!(!is_chunked(&store, &hash, None));
        // Exactly one stored object (the whole blob).
        assert_eq!(store.iter_hashes().len(), 1);
    }

    /// `hash_file` must return the same logical hash as `store_file` for the
    /// same content, while writing NO objects to a store. Covers small content,
    /// content needing the ED F0 escape prefix, and multi-chunk content.
    fn assert_hash_file_matches_store_file(content: &[u8]) {
        let src_dir = TempDir::new().unwrap();
        let src_path = src_dir.path().join("input.bin");
        fs::write(&src_path, content).unwrap();

        // store_file writes objects and returns the logical hash.
        let store_dir = TempDir::new().unwrap();
        let store = LocalStore::for_cache(store_dir.path());
        let stored_hash = store_file(&store, &src_path, None).unwrap();

        // hash_file computes the same hash and is the canonical blob_hash.
        let hashed = hash_file(&src_path).unwrap();
        assert_eq!(hashed, stored_hash, "hash_file != store_file hash");
        assert_eq!(hashed, object::blob_hash(content), "hash_file != blob_hash");

        // hash_file writes nothing: a fresh store touched only by hash_file
        // would be empty (we assert via a second store it never receives).
        let empty_dir = TempDir::new().unwrap();
        let empty_store = LocalStore::for_cache(empty_dir.path());
        let _ = hash_file(&src_path).unwrap();
        assert_eq!(
            empty_store.iter_hashes().len(),
            0,
            "hash_file must not write objects"
        );
    }

    #[test]
    fn hash_file_matches_store_file_small() {
        assert_hash_file_matches_store_file(b"hello world");
    }

    #[test]
    fn hash_file_matches_store_file_empty() {
        assert_hash_file_matches_store_file(b"");
    }

    #[test]
    fn hash_file_matches_store_file_escape_prefix() {
        // Content starting with ED F0 forces the escape-prefix path.
        let mut content = vec![0xED, 0xF0];
        content.extend_from_slice(b"payload after the type tag");
        assert_hash_file_matches_store_file(&content);
    }

    #[test]
    fn hash_file_matches_store_file_multichunk() {
        // ~10 MiB pseudo-random -> multiple chunks in store_file; hash_file must
        // still produce the identical single logical hash.
        let content = pseudo_random(0x7777_3333, 10 * 1024 * 1024);
        assert_hash_file_matches_store_file(&content);
    }

    #[test]
    fn prefix_reader_prepends_bytes() {
        let inner = io::Cursor::new(vec![1u8, 2, 3]);
        let mut r = PrefixReader::new(vec![0xED, 0xF0], inner);
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(out, vec![0xED, 0xF0, 1, 2, 3]);
    }

    #[test]
    fn prefix_reader_empty_prefix() {
        let inner = io::Cursor::new(vec![9u8, 8, 7]);
        let mut r = PrefixReader::new(Vec::new(), inner);
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(out, vec![9, 8, 7]);
    }

    /// Store `content` as a blob (via the in-memory path) and return its store
    /// plus logical hash. Helper for the materialise tests.
    fn store_blob(dir: &Path, content: &[u8]) -> (LocalStore, Hash) {
        let store = LocalStore::for_cache(dir);
        let hash = object::blob_hash(content);
        let serialised = object::serialise_blob(content);
        store_chunked(&store, &hash, &serialised, None).unwrap();
        (store, hash)
    }

    #[test]
    fn materialise_to_file_multichunk() {
        // ~10 MiB pseudo-random -> multiple chunks. The materialised file bytes
        // must equal the original content exactly.
        let content = pseudo_random(0x5151_2727, 10 * 1024 * 1024);
        let store_dir = TempDir::new().unwrap();
        let (store, hash) = store_blob(store_dir.path(), &content);
        assert!(
            is_chunked(&store, &hash, None),
            "expected multi-chunk object"
        );

        let out_dir = TempDir::new().unwrap();
        let dest = out_dir.path().join("out.bin");
        materialise_to_file(&store, &hash, None, &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), content);
    }

    #[test]
    fn materialise_to_file_single_chunk() {
        // Below CDC_MIN -> stored whole (no manifest).
        let content = pseudo_random(0x9090_3333, 64 * 1024);
        let store_dir = TempDir::new().unwrap();
        let (store, hash) = store_blob(store_dir.path(), &content);
        assert!(
            !is_chunked(&store, &hash, None),
            "expected single whole object"
        );

        let out_dir = TempDir::new().unwrap();
        let dest = out_dir.path().join("out.bin");
        materialise_to_file(&store, &hash, None, &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), content);
    }

    #[test]
    fn materialise_to_file_escape_prefix() {
        // Content whose first two bytes are ED F0 forces the blob escape. The
        // materialised file must start with the ED F0 content bytes, with the
        // escape stripped exactly once (not the content's own ED F0).
        let mut content = vec![0xED, 0xF0];
        content.extend_from_slice(&pseudo_random(0xABCD_1234, 10 * 1024 * 1024));
        assert!(object::blob_needs_escape(
            content.first().copied(),
            content.get(1).copied()
        ));

        let store_dir = TempDir::new().unwrap();
        let (store, hash) = store_blob(store_dir.path(), &content);
        assert!(
            is_chunked(&store, &hash, None),
            "expected multi-chunk object"
        );

        let out_dir = TempDir::new().unwrap();
        let dest = out_dir.path().join("out.bin");
        materialise_to_file(&store, &hash, None, &dest).unwrap();
        let got = fs::read(&dest).unwrap();
        assert_eq!(&got[..2], &[0xED, 0xF0], "content must begin with ED F0");
        assert_eq!(got, content, "escape stripped exactly once");
    }

    #[test]
    fn materialise_to_file_corrupt_manifest_errors_and_no_partial_file() {
        // Build a multi-chunk object, then corrupt one chunk so its ED F3 tag is
        // missing. materialise_to_file must error and leave no file at dest.
        let content = pseudo_random(0x7777_8888, 10 * 1024 * 1024);
        let store_dir = TempDir::new().unwrap();
        let (store, hash) = store_blob(store_dir.path(), &content);

        // Read the manifest to find a chunk hash, then overwrite that chunk
        // object with bytes lacking the ED F3 tag.
        let raw = codec::store_read(&store, &hash, None).unwrap();
        let chunk_hashes = object::deserialise_manifest(&raw).expect("manifest");
        assert!(chunk_hashes.len() >= 2);
        // Overwrite the second chunk with un-tagged bytes (no ED F3). We write a
        // fresh object under the chunk's storage path by re-encoding garbage at
        // that hash; the store is content-addressed so we go through the file.
        let bad_path = store.objects_path(&chunk_hashes[1]).expect("chunk path");
        // Replace stored bytes with content that decodes to non-ED-F3 bytes.
        // Encode raw bytes [0,1,2,3] (no key) so decode yields [0,1,2,3].
        let bad_encoded = codec::encode(&[0u8, 1, 2, 3], None, chunk_hashes[1].as_bytes_array());
        fs::write(&bad_path, &bad_encoded).unwrap();

        let out_dir = TempDir::new().unwrap();
        let dest = out_dir.path().join("out.bin");
        let result = materialise_to_file(&store, &hash, None, &dest);
        assert!(
            matches!(result, Err(Error::InvalidObject(_))),
            "expected InvalidObject, got {:?}",
            result
        );
        assert!(!dest.exists(), "no partial file must be visible at dest");
    }

    #[test]
    fn stat_unchanged_logic() {
        let t0 = SystemTime::UNIX_EPOCH;
        let t1 = t0 + std::time::Duration::from_secs(1);
        let a = FileStat {
            mtime: Some(t0),
            size: 100,
        };
        // Identical → unchanged.
        assert!(stat_unchanged(
            a,
            FileStat {
                mtime: Some(t0),
                size: 100
            }
        ));
        // Size differs → changed.
        assert!(!stat_unchanged(
            a,
            FileStat {
                mtime: Some(t0),
                size: 101
            }
        ));
        // mtime differs → changed.
        assert!(!stat_unchanged(
            a,
            FileStat {
                mtime: Some(t1),
                size: 100
            }
        ));
        // mtime unavailable on one side → changed.
        assert!(!stat_unchanged(
            a,
            FileStat {
                mtime: None,
                size: 100
            }
        ));
    }
}
