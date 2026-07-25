use std::io::Read;
use std::path::PathBuf;

use crate::codec;
use crate::codec::encrypt::EncryptKey;
use crate::codec::pack::bloom::BloomFilter;
use crate::codec::pack::index::IndexFile;
use crate::codec::pack::index_root::IndexRoot;
use crate::dtimer_l1;
use crate::error::Error;
use crate::object::{Hash, Tree};
use crate::repo::Repo;
use crate::store::ObjectStore;
use crate::store::local::LocalStore;
use crate::term::Output;

/// Minimum length accepted for a hash prefix (as opposed to a full 64-char
/// hash), both when validating a user-supplied prefix and when deciding
/// whether a candidate string in a `cat` target looks like a hash prefix at
/// all. Was previously two independent `< 4`/`>= 4` checks with duplicated
/// error message text (refactor-instructions.md D5).
const MIN_HASH_PREFIX_LEN: usize = 4;

pub struct CatOptions {
    pub work_dir: PathBuf,
    /// `<ref>[/<path>]`, `index-root`, or a hash
    pub target: String,
    /// Print only the resolved 64-character hash instead of object content
    pub hash_only: bool,
    /// Remote name (default: "origin")
    pub remote_name: String,
}

pub fn run(opts: CatOptions) -> Result<(), Error> {
    // Handle index-root separately — it reads from the remote pack layer.
    if opts.target == "index-root" {
        if opts.hash_only {
            return Err(Error::Other(
                "--hash is not supported for pack-layer objects".to_string(),
            ));
        }
        let repo = Repo::open(&opts.work_dir)?;
        return print_index_root(&repo, &opts.remote_name);
    }

    let repo = Repo::open(&opts.work_dir)?;
    crate::progress::notify_repo_dir(&repo.work_dir);
    let phase = crate::progress::begin_phase("Cat");
    let _t = dtimer_l1!("cat");
    let local = repo.local_store();

    // Split target into ref and optional sub-path.
    let (ref_str, sub_path) = split_target(&opts.target);

    let (hash, remote_store): (Hash, Option<LocalStore>) = match ref_str {
        "clone-root" => {
            let h = repo.read_clone_root()?.ok_or_else(|| {
                Error::Other("no clone_root — repository has never been synced".to_string())
            })?;
            (h, None)
        }
        "remote-root" => {
            let (pack_reader, remote, _remote_key) = repo.pack_reader(&opts.remote_name, None)?;
            let h = pack_reader
                .read_root()?
                .ok_or_else(|| Error::Other(format!("no index root on {}", opts.remote_name)))?;
            (h, Some(remote))
        }
        _ => {
            // Try local first; fall back to remote pack-layer if not found locally.
            match resolve_hash(ref_str, &local) {
                Ok(h) => (h, None),
                Err(Error::ObjectNotFound(_)) if sub_path.is_none() => {
                    // Hash not in local cache — try remote pack layer.
                    if opts.hash_only {
                        return Err(Error::Other(
                            "--hash is not supported for pack-layer objects".to_string(),
                        ));
                    }
                    let remote = repo.remote_store(&opts.remote_name)?;
                    let remote_key = remote.encrypt_key.clone();
                    // A short prefix (4..64 chars) is resolved against the remote
                    // pack index so the user can paste the abbreviated hash shown
                    // by `ls`. A full 64-char hash skips the scan and is fetched
                    // directly.
                    if ref_str.len() == 64 {
                        // A full hash may name any stored object — a logical
                        // blob/tree, or a pack-layer artifact (pack file, index
                        // file, bloom filter). Show the pack-layer diagnostic
                        // view, which inspects the leading bytes and renders the
                        // appropriate form.
                        let hash = parse_hash_for_remote(ref_str)?;
                        return print_pack_object(&hash, &remote, &remote_key);
                    }
                    // A short prefix is resolved against the remote pack index,
                    // which only enumerates logical-object entries (blobs, trees,
                    // chunk manifests) — never pack/index/bloom storage keys. So a
                    // resolved prefix always names a logical object: fetch it via
                    // the pack reader (handling inline / pack-sliced / standalone /
                    // chunked entries) into the local cache and fall through to the
                    // logical display path below, so the user sees the object's
                    // content rather than a pack-layer summary.
                    let hash =
                        resolve_prefix_on_remote(&repo, &remote, ref_str, &opts.remote_name)?;
                    let pack_reader = crate::codec::pack::reader::PackReader::new(
                        Box::new(remote.clone()),
                        repo.local_store(),
                        repo.packcache_store(),
                        repo.objcache_store(),
                        repo.remote_root_pointer(&opts.remote_name)?,
                        remote_key.clone(),
                    );
                    crate::commands::push::transfer_objects(
                        &pack_reader,
                        &local,
                        &hash,
                        remote_key.as_ref(),
                        true,
                    )?;
                    (hash, None)
                }
                Err(e) => return Err(e),
            }
        }
    };

    // For remote-root, ensure the root tree object is in the local cache.
    if let Some(ref remote) = remote_store {
        ensure_in_local_cache(&hash, remote, &local, remote.encrypt_key.as_ref())?;
    }

    let final_hash = if let Some(path) = sub_path {
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if let Some(ref remote) = remote_store {
            crate::tree_ops::ensure_path_in_store(
                remote,
                &local,
                &hash,
                &parts[..parts.len().saturating_sub(1)],
            )?;
        }
        crate::tree_ops::navigate(&hash, &parts, &local)?
            .ok_or_else(|| Error::ObjectNotFound(format!("{}/{}", hash, path)))?
    } else {
        hash
    };

    if opts.hash_only {
        let mut out = Output::for_stdout();
        out.writeln(final_hash.as_str()).map_err(Error::Io)?;
        out.finish().map_err(Error::Io)?;
        phase.complete("hash");
        return Ok(());
    }

    let data = codec::store_read(&local, &final_hash, None)?;

    if let Ok(tree) = Tree::deserialise(&data) {
        let value = serde_json::to_value(&tree).map_err(Error::Json)?;
        println_json(&value)?;
        phase.complete("tree");
    } else {
        // Stream the blob to stdout chunk by chunk (bounded memory). This also
        // handles chunked manifests: for_each_blob_chunk walks the chunk list
        // and strips the ED F3 / first-chunk ED F0 tags. Trees are never
        // chunked, so the deserialise above already covers them.
        //
        // Mark the phase done, then freeze the phase view *before* streaming:
        // blob bytes go straight to stdout (unbuffered, possibly huge), so the
        // periodic redraw must be stopped first or it would erase the output.
        // The completed phase list stays on screen above the streamed bytes.
        phase.complete("blob");
        crate::progress::freeze_phase_view();
        let mut out = Output::raw_stdout();
        let w = out.writer();
        codec::chunk::for_each_blob_chunk(&local, &final_hash, None, |chunk_bytes| {
            w.write_all(chunk_bytes).map_err(Error::Io)
        })?;
        out.finish().map_err(Error::Io)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Remote pack-layer helpers
// ---------------------------------------------------------------------------

/// Parse a full 64-character hex hash for remote pack-layer lookup.
/// Prefix lookups are not supported for pack-layer objects.
fn parse_hash_for_remote(s: &str) -> Result<Hash, Error> {
    if s.len() != 64 {
        return Err(Error::Other(
            "pack-layer lookup requires a full 64-character hash".to_string(),
        ));
    }
    Hash::from_hex(s).map_err(|_| Error::Other(format!("invalid hash: {}", s)))
}

/// Resolve a hash `prefix` (4..64 chars) against the remote pack index. Used
/// when the object is not in the local cache, so the user can paste the short
/// hash that `ls` prints. Errors if the prefix is too short, matches nothing,
/// or is ambiguous (matches more than one stored object).
fn resolve_prefix_on_remote(
    repo: &Repo,
    remote: &LocalStore,
    prefix: &str,
    remote_name: &str,
) -> Result<Hash, Error> {
    if prefix.len() < MIN_HASH_PREFIX_LEN {
        return Err(Error::Other(format!(
            "hash prefix must be at least {} characters",
            MIN_HASH_PREFIX_LEN
        )));
    }
    let remote_key = remote.encrypt_key.clone();
    let pack_reader = crate::codec::pack::reader::PackReader::new(
        Box::new(remote.clone()),
        repo.local_store(),
        repo.packcache_store(),
        repo.objcache_store(),
        repo.remote_root_pointer(remote_name)?,
        remote_key,
    );
    let mut matches = pack_reader.resolve_prefix(prefix)?;
    match matches.len() {
        0 => Err(Error::ObjectNotFound(prefix.to_string())),
        1 => Ok(matches.remove(0)),
        n => {
            // Show a few candidates to help the user disambiguate.
            let sample: Vec<String> = matches
                .iter()
                .take(4)
                .map(|h| h.as_str().to_string())
                .collect();
            Err(Error::Other(format!(
                "ambiguous hash prefix '{}' matches {} objects: {}{}",
                prefix,
                n,
                sample.join(", "),
                if n > sample.len() { ", ..." } else { "" },
            )))
        }
    }
}

fn print_index_root(repo: &Repo, remote_name: &str) -> Result<(), Error> {
    let remote = repo.remote_store(remote_name)?;

    // Read the raw index-root bytes through the backend-pluggable root pointer
    // rather than touching the filesystem directly, so this works for cloud
    // backends. An absent pointer is the same "no index root" case as before.
    let raw = match repo.remote_root_pointer(remote_name)?.read()?.0 {
        Some(b) => b,
        None => {
            return Err(Error::Other(format!("no index root on {}", remote_name)));
        }
    };

    let plaintext =
        crate::codec::pack::decrypt_index_root_bytes(&raw, remote.encrypt_key.as_ref())?;
    let ir = IndexRoot::deserialise(&plaintext)?;
    print_index_root_json(&ir)
}

fn print_index_root_json(ir: &IndexRoot) -> Result<(), Error> {
    let hash_or_null = |bytes: &[u8; 32]| -> serde_json::Value {
        if *bytes == [0u8; 32] {
            serde_json::Value::Null
        } else {
            serde_json::Value::String(Hash::from_bytes(*bytes).as_str().to_string())
        }
    };

    let delta_hashes: Vec<serde_json::Value> = ir.delta_hashes.iter().map(&hash_or_null).collect();
    let cold_shards: Vec<serde_json::Value> = ir.cold_shards.iter().map(&hash_or_null).collect();

    let obj = serde_json::json!({
        "remote_root": hash_or_null(&ir.remote_root),
        "hot_hash": hash_or_null(&ir.hot_hash),
        "bloom_hash": hash_or_null(&ir.bloom_hash),
        "cold_prefix_bits": ir.cold_prefix_bits,
        "delta_hashes": delta_hashes,
        "cold_shards": cold_shards,
    });
    println_json(&obj)
}

fn print_pack_object(
    hash: &Hash,
    remote: &LocalStore,
    remote_key: &Option<EncryptKey>,
) -> Result<(), Error> {
    let storage_key = remote.storage_key_of(hash);

    // Read raw (encrypted) bytes from remote objects/.
    let mut reader = remote
        .open_read(hash)
        .map_err(|_| Error::ObjectNotFound(hash.as_str().to_string()))?;
    let mut raw = Vec::new();
    reader.read_to_end(&mut raw).map_err(Error::Io)?;

    // Check leading 2 bytes before decryption: pack files are not encrypted.
    if raw.len() >= 2 && raw[0] == 0xED && raw[1] == 0xE0 {
        // Pack file — show stored byte count only.
        let obj = serde_json::json!({
            "type": "pack-file",
            "logical_hash": hash.as_str(),
            "storage_key": storage_key.as_str(),
            "stored_bytes": raw.len(),
        });
        return println_json(&obj);
    }

    // Decrypt to inspect contents.
    let plaintext =
        crate::codec::encrypt::decrypt(raw, remote_key.as_ref(), hash.as_bytes_array())?;

    if plaintext.len() < 2 {
        return Err(Error::InvalidObject("object too short".to_string()));
    }

    match (plaintext[0], plaintext[1]) {
        (0xED, 0xE2) => {
            // Index file (hot / delta / cold).
            let idx = IndexFile::deserialise(&plaintext)?;
            print_index_file_json(&idx, hash, &storage_key)
        }
        (0xED, 0xE4) => {
            // Bloom filter.
            let bf = BloomFilter::deserialise(&plaintext)?;
            print_bloom_json(&bf, hash, &storage_key)
        }
        _ => {
            // Logical object (blob/tree) or other.
            let mut out = Output::for_stdout();
            out.writeln(&format!(
                "this is a logical object; use 'omemfs cat {}' to view its content",
                hash.as_str()
            ))
            .map_err(Error::Io)?;
            out.finish().map_err(Error::Io)
        }
    }
}

fn print_index_file_json(
    idx: &IndexFile,
    logical_hash: &Hash,
    storage_key: &Hash,
) -> Result<(), Error> {
    let entries: Vec<serde_json::Value> = idx
        .entries()
        .iter()
        .map(|e| {
            use crate::codec::pack::index::IndexEntry;
            match e {
                IndexEntry::Inline(ie) => serde_json::json!({
                    "type": "inline",
                    "hash": ie.hash.as_str(),
                    "data_length": ie.data.len(),
                }),
                IndexEntry::Pack(pe) => serde_json::json!({
                    "type": "pack",
                    "hash": pe.hash.as_str(),
                    "pack_hash": pe.pack_hash.as_str(),
                    "offset": pe.offset,
                    "length": pe.length,
                }),
                IndexEntry::Standalone(se) => serde_json::json!({
                    "type": "standalone",
                    "hash": se.hash.as_str(),
                }),
            }
        })
        .collect();

    let obj = serde_json::json!({
        "type": "index-file",
        "logical_hash": logical_hash.as_str(),
        "storage_key": storage_key.as_str(),
        "entry_count": idx.len(),
        "entries": entries,
    });
    println_json(&obj)
}

fn print_bloom_json(
    bf: &BloomFilter,
    logical_hash: &Hash,
    storage_key: &Hash,
) -> Result<(), Error> {
    let set_bits = count_set_bits_public(bf);
    let fill_rate = if bf.num_bits > 0 {
        set_bits as f64 / bf.num_bits as f64
    } else {
        0.0
    };

    let obj = serde_json::json!({
        "type": "bloom-filter",
        "logical_hash": logical_hash.as_str(),
        "storage_key": storage_key.as_str(),
        "num_hash_functions": bf.num_hash_functions,
        "num_bits": bf.num_bits,
        "element_count": bf.element_count,
        "fill_rate": (fill_rate * 1_000_000.0).round() / 1_000_000.0,
    });
    println_json(&obj)
}

/// Count set bits by re-serialising the Bloom filter and reading the bit array.
fn count_set_bits_public(bf: &BloomFilter) -> u64 {
    // Serialise to get the bit bytes (header is 20 bytes per HEADER_LEN).
    let bytes = bf.serialise();
    const HEADER_LEN: usize = 20;
    if bytes.len() <= HEADER_LEN {
        return 0;
    }
    bytes[HEADER_LEN..]
        .iter()
        .map(|b| b.count_ones() as u64)
        .sum()
}

// ---------------------------------------------------------------------------
// JSON output helpers
// ---------------------------------------------------------------------------

/// Print a JSON value as pretty-printed text, with syntax highlighting when
/// stdout is a TTY (respects NO_COLOR and CLICOLOR_FORCE).
///
/// Output is routed through [`Output`]: in TTY mode it is buffered and deposited
/// into the active `ProgressContext`, so the JSON appears below the phase list
/// instead of racing with — and being erased by — the periodic redraw.
fn println_json(value: &serde_json::Value) -> Result<(), Error> {
    let mut out = Output::for_stdout();
    let text = serde_json::to_string_pretty(value).unwrap();
    if out.colored() {
        out.writeln(&colorize_json(&text)).map_err(Error::Io)?;
    } else {
        out.writeln(&text).map_err(Error::Io)?;
    }
    out.finish().map_err(Error::Io)
}

/// Apply ANSI color codes to a pretty-printed JSON string.
/// Colors: keys = sky blue (75), string values = green (114),
///         numbers/booleans = amber (179), null = grey (242).
fn colorize_json(json: &str) -> String {
    use std::fmt::Write;

    // ANSI SGR helpers: \x1b[38;5;<n>m ... \x1b[0m
    fn fg(n: u8, s: &str) -> String {
        format!("\x1b[38;5;{}m{}\x1b[0m", n, s)
    }

    let mut out = String::with_capacity(json.len() * 2);
    let mut chars = json.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                // Collect the full quoted string (handle escapes).
                let mut s = String::from('"');
                let mut escaped = false;
                for c in chars.by_ref() {
                    s.push(c);
                    if escaped {
                        escaped = false;
                    } else if c == '\\' {
                        escaped = true;
                    } else if c == '"' {
                        break;
                    }
                }
                // Determine if this is a key (followed by ':') or a value.
                // Skip whitespace to peek.
                let mut peek_buf = String::new();
                while chars.peek() == Some(&' ')
                    || chars.peek() == Some(&'\n')
                    || chars.peek() == Some(&'\r')
                {
                    let ws = chars.next().unwrap();
                    peek_buf.push(ws);
                }
                if chars.peek() == Some(&':') {
                    // JSON key — sky blue (75)
                    write!(out, "{}", fg(75, &s)).unwrap();
                } else {
                    // String value — green (114)
                    write!(out, "{}", fg(114, &s)).unwrap();
                }
                out.push_str(&peek_buf);
            }
            // Numbers, booleans: amber (179). Match start of a token.
            c @ ('0'..='9' | '-') => {
                let mut tok = String::from(c);
                while matches!(chars.peek(), Some('0'..='9' | '.' | 'e' | 'E' | '+' | '-')) {
                    tok.push(chars.next().unwrap());
                }
                write!(out, "{}", fg(179, &tok)).unwrap();
            }
            't' => {
                // true
                let rest: String = chars.by_ref().take(3).collect();
                if rest == "rue" {
                    write!(out, "{}", fg(179, "true")).unwrap();
                } else {
                    out.push('t');
                    out.push_str(&rest);
                }
            }
            'f' => {
                // false
                let rest: String = chars.by_ref().take(4).collect();
                if rest == "alse" {
                    write!(out, "{}", fg(179, "false")).unwrap();
                } else {
                    out.push('f');
                    out.push_str(&rest);
                }
            }
            'n' => {
                // null
                let rest: String = chars.by_ref().take(3).collect();
                if rest == "ull" {
                    write!(out, "{}", fg(242, "null")).unwrap();
                } else {
                    out.push('n');
                    out.push_str(&rest);
                }
            }
            other => out.push(other),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Logical object helpers
// ---------------------------------------------------------------------------

/// Split `target` into a (ref, sub_path) pair.
///
/// The ref and sub-path may be separated by either `:` or `/`, so both
/// `<hash>:<path>` (as the README and CLI help document it) and `<hash>/<path>`
/// resolve identically. The earliest `:` or `/` is the separator. Hash refs are
/// hex-only, so a leading `:` is unambiguous.
fn split_target(target: &str) -> (&str, Option<&str>) {
    for alias in &["clone-root", "remote-root"] {
        if target == *alias {
            return (alias, None);
        }
        // Accept either separator after an alias (e.g. `clone-root:docs/x`).
        if let Some(rest) = target
            .strip_prefix(&format!("{}/", alias))
            .or_else(|| target.strip_prefix(&format!("{}:", alias)))
        {
            return (alias, if rest.is_empty() { None } else { Some(rest) });
        }
    }

    if let Some(pos) = target.find([':', '/']) {
        let candidate = &target[..pos];
        if candidate.len() >= MIN_HASH_PREFIX_LEN
            && candidate.chars().all(|c| c.is_ascii_hexdigit())
        {
            let rest = &target[pos + 1..];
            return (candidate, if rest.is_empty() { None } else { Some(rest) });
        }
    }

    (target, None)
}

/// Resolve a full 64-char hash or a unique prefix (4+ chars) to a `Hash` in the local store.
fn resolve_hash(s: &str, store: &LocalStore) -> Result<Hash, Error> {
    if s.len() == 64 {
        return Hash::from_hex(s)
            .map_err(|_| Error::ObjectNotFound(s.to_string()))
            .and_then(|h| {
                if store.exists(&h).unwrap_or(false) {
                    Ok(h)
                } else {
                    Err(Error::ObjectNotFound(s.to_string()))
                }
            });
    }

    if s.len() < MIN_HASH_PREFIX_LEN {
        return Err(Error::Other(format!(
            "hash prefix must be at least {} characters",
            MIN_HASH_PREFIX_LEN
        )));
    }

    let mut matches: Vec<Hash> = store
        .iter_hashes()
        .into_iter()
        .filter(|hex| hex.starts_with(s))
        .filter_map(|hex| Hash::from_hex(&hex).ok())
        .collect();

    match matches.len() {
        0 => Err(Error::ObjectNotFound(s.to_string())),
        1 => Ok(matches.remove(0)),
        _ => Err(Error::AmbiguousHash(s.to_string())),
    }
}

/// Fetch `hash` from `remote` into `local` if not already cached.
fn ensure_in_local_cache(
    hash: &Hash,
    remote: &dyn ObjectStore,
    local: &dyn ObjectStore,
    remote_key: Option<&EncryptKey>,
) -> Result<(), Error> {
    if !local.exists(hash)? {
        let data = codec::store_read(remote, hash, remote_key)?;
        codec::store_write(local, hash, &data, None)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::split_target;

    #[test]
    fn bare_hash_has_no_subpath() {
        assert_eq!(split_target("a3f89b2c"), ("a3f89b2c", None));
    }

    #[test]
    fn hash_slash_path_splits() {
        assert_eq!(
            split_target("a3f89b2c/docs/guide.md"),
            ("a3f89b2c", Some("docs/guide.md"))
        );
    }

    #[test]
    fn hash_colon_path_splits() {
        // The README and CLI help document the `<hash>:<path>` form; it must
        // resolve identically to the slash form.
        assert_eq!(
            split_target("a3f89b2c:docs/guide.md"),
            ("a3f89b2c", Some("docs/guide.md"))
        );
    }

    #[test]
    fn short_prefix_with_colon_splits() {
        // A 4-char prefix is the documented minimum; the colon separator must
        // work for prefixes just as it does for full hashes.
        assert_eq!(split_target("a3f8:metalone"), ("a3f8", Some("metalone")));
    }

    #[test]
    fn earliest_separator_wins() {
        // A colon before the first slash is the separator; the rest of the
        // path (slashes included) is the sub-path.
        assert_eq!(split_target("a3f8:a/b/c"), ("a3f8", Some("a/b/c")));
        // A slash before the first colon: the separator is the slash, so a
        // literal colon may appear inside the sub-path.
        assert_eq!(split_target("a3f8/a:b"), ("a3f8", Some("a:b")));
    }

    #[test]
    fn trailing_separator_yields_no_subpath() {
        assert_eq!(split_target("a3f89b2c:"), ("a3f89b2c", None));
        assert_eq!(split_target("a3f89b2c/"), ("a3f89b2c", None));
    }

    #[test]
    fn alias_accepts_both_separators() {
        assert_eq!(split_target("clone-root"), ("clone-root", None));
        assert_eq!(
            split_target("clone-root/docs/x"),
            ("clone-root", Some("docs/x"))
        );
        assert_eq!(
            split_target("clone-root:docs/x"),
            ("clone-root", Some("docs/x"))
        );
        assert_eq!(
            split_target("remote-root:readme.md"),
            ("remote-root", Some("readme.md"))
        );
    }

    #[test]
    fn non_hex_ref_is_not_split() {
        // A ref that is neither hex nor an alias is treated as a whole target
        // with no sub-path, so resolution can report a clean "not found".
        assert_eq!(split_target("not-a-hash:x"), ("not-a-hash:x", None));
    }
}
