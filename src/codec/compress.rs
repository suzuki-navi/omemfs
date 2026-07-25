/// Stage 2: compress / decompress.
///
/// Format table (leading 2 bytes of stored bytes):
///   ED DE ...  tree-dict zstd v1  — zstd compressed with the embedded tree dictionary
///   ED DF ...  plain zstd         — zstd without a dictionary
///   ED D0 ...  escaped raw        — raw content whose first byte is in ED D0..DF
///   anything else  raw            — content stored as-is, no prefix
///
/// The ED Fx range is used by Stage 1 (serialize) for type tags.
/// compress does not emit ED Fx and will not escape it, so there is no
/// collision between the two stages.
///
/// Write algorithm: try all three candidates and store the smallest:
///   1. dict zstd  (ED DE prefix + dict-compressed payload)
///   2. plain zstd (ED DF prefix + plain-compressed payload)
///   3. raw        (escaped if necessary)
///
/// Read algorithm: dispatch on the leading 2 bytes.
///   ED DE → decompress with tree-dict v1
///   ED DF → decompress with plain zstd
///   ED D0 → strip 2-byte escape prefix; remainder is raw
///   else  → raw (content as-is)
use std::io::{Read, Write};

use crate::error::Error;

const MAGIC_DICT_ZSTD: [u8; 2] = [0xED, 0xDE];
const MAGIC_PLAIN_ZSTD: [u8; 2] = [0xED, 0xDF];
const MAGIC_ESCAPED_RAW: [u8; 2] = [0xED, 0xD0];
const ZSTD_LEVEL: i32 = 3;

/// Embedded tree-dict v1, trained by build.rs from representative tree JSON.
static TREE_DICT_V1: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/tree_dict_v1.bin"));

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Compress `data` and return the stored bytes.
///
/// Tries dict-zstd, plain-zstd, and raw; stores the smallest candidate.
pub fn compress(data: &[u8]) -> Vec<u8> {
    let dict_payload = compress_zstd_bytes(data, Some(TREE_DICT_V1));
    let dict_total = 2 + dict_payload.len();
    let plain_payload = compress_zstd_bytes(data, None);
    let plain_total = 2 + plain_payload.len();
    let raw_total = raw_stored_len(data);

    let best = dict_total.min(plain_total).min(raw_total);
    if best == dict_total {
        let mut out = Vec::with_capacity(dict_total);
        out.extend_from_slice(&MAGIC_DICT_ZSTD);
        out.extend_from_slice(&dict_payload);
        return out;
    }
    if best == plain_total {
        let mut out = Vec::with_capacity(plain_total);
        out.extend_from_slice(&MAGIC_PLAIN_ZSTD);
        out.extend_from_slice(&plain_payload);
        return out;
    }
    escape_raw_owned(data)
}

/// Decompress stored bytes back to the original serialised content.
pub fn decompress(stored: &[u8]) -> Result<Vec<u8>, Error> {
    if stored.len() < 2 {
        return Ok(stored.to_vec());
    }
    match [stored[0], stored[1]] {
        [0xED, 0xDE] => decompress_zstd_bytes(&stored[2..], Some(TREE_DICT_V1)),
        [0xED, 0xDF] => decompress_zstd_bytes(&stored[2..], None),
        [0xED, 0xD0] => Ok(stored[2..].to_vec()),
        _ => Ok(stored.to_vec()),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Compute the stored byte length for the raw candidate without allocating.
fn raw_stored_len(data: &[u8]) -> usize {
    if needs_escape(data) {
        2 + data.len()
    } else {
        data.len()
    }
}

fn needs_escape(data: &[u8]) -> bool {
    matches!(data, [0xED, b, ..] if *b >= 0xD0 && *b <= 0xDF)
}

fn escape_raw_owned(data: &[u8]) -> Vec<u8> {
    if needs_escape(data) {
        let mut out = Vec::with_capacity(2 + data.len());
        out.extend_from_slice(&MAGIC_ESCAPED_RAW);
        out.extend_from_slice(data);
        out
    } else {
        data.to_vec()
    }
}

/// Compress `data` with zstd. If `dict` is `Some`, use it as the compression dictionary.
fn compress_zstd_bytes(data: &[u8], dict: Option<&[u8]>) -> Vec<u8> {
    let mut out = Vec::new();
    match dict {
        Some(d) => {
            let mut encoder = zstd::stream::Encoder::with_dictionary(&mut out, ZSTD_LEVEL, d)
                .expect("zstd dict encoder creation is infallible");
            encoder.write_all(data).expect("zstd encode write");
            encoder.finish().expect("zstd encode finish");
        }
        None => {
            let mut encoder = zstd::stream::Encoder::new(&mut out, ZSTD_LEVEL)
                .expect("zstd encoder creation is infallible");
            encoder.write_all(data).expect("zstd encode write");
            encoder.finish().expect("zstd encode finish");
        }
    }
    out
}

/// Decompress `data` compressed with zstd. If `dict` is `Some`, use the same dictionary.
fn decompress_zstd_bytes(data: &[u8], dict: Option<&[u8]>) -> Result<Vec<u8>, Error> {
    let cursor = std::io::Cursor::new(data);
    let mut out = Vec::new();
    match dict {
        Some(d) => {
            let mut decoder =
                zstd::stream::Decoder::with_dictionary(cursor, d).map_err(Error::Io)?;
            decoder.read_to_end(&mut out).map_err(Error::Io)?;
        }
        None => {
            let mut decoder = zstd::stream::Decoder::new(cursor).map_err(Error::Io)?;
            decoder.read_to_end(&mut out).map_err(Error::Io)?;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_small_text() {
        let data = b"hello world";
        assert_eq!(decompress(&compress(data)).unwrap(), data);
    }

    #[test]
    fn roundtrip_tree_json() {
        let data = br#"{"kind":"normal","entries":[{"kind":"blob","name":"file.txt","hash":"aaaa","mtime":null,"size":10}]}"#;
        assert_eq!(decompress(&compress(data)).unwrap(), data.as_ref());
    }

    #[test]
    fn roundtrip_ed_dx_prefix() {
        // ED Dx range must be escaped by compress.
        let data = vec![0xED, 0xD5, 0x01, 0x02];
        let stored = compress(&data);
        assert_eq!(&stored[..2], &MAGIC_ESCAPED_RAW);
        assert_eq!(decompress(&stored).unwrap(), data);
    }

    #[test]
    fn ed_fx_not_escaped_by_compress() {
        // ED Fx range is used by the serialize stage and must NOT be escaped here.
        let data = vec![0xED, 0xF1, 0x7B, 0x22]; // ED F1 { "
        let stored = compress(&data);
        // Should NOT start with MAGIC_ESCAPED_RAW.
        assert_ne!(&stored[..2], &MAGIC_ESCAPED_RAW);
        assert_eq!(decompress(&stored).unwrap(), data);
    }

    #[test]
    fn jpeg_roundtrips_without_special_handling() {
        let data = vec![0xFF, 0xD8, 0xFF, 0x00, 0x01, 0x02];
        assert_eq!(decompress(&compress(&data)).unwrap(), data);
    }

    #[test]
    fn large_repetitive_uses_zstd() {
        let data: Vec<u8> = br#"{"name":"file.txt","kind":"blob","hash":"a1b2","size":100}"#
            .iter()
            .cloned()
            .cycle()
            .take(4096)
            .collect();
        let stored = compress(&data);
        // Large repetitive content should be compressed (dict or plain zstd), not raw.
        assert!(
            stored.starts_with(&MAGIC_DICT_ZSTD) || stored.starts_with(&MAGIC_PLAIN_ZSTD),
            "expected compressed output, got magic {:02x} {:02x}",
            stored[0],
            stored[1]
        );
        assert_eq!(decompress(&stored).unwrap(), data);
    }

    #[test]
    fn tree_object_uses_dict_compression() {
        // A tree JSON prefixed with ED F1 should be stored with ED DE (dict zstd).
        let json = br#"{"kind":"normal","entries":[{"hash":"a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1b2","kind":"blob","mtime":"2026-05-16T10:00:00Z","name":"README.md","size":1234},{"hash":"b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1b2c3","kind":"tree","mtime":"2026-05-16T10:00:00Z","name":"src","size":56789}]}"#;
        let mut data = Vec::new();
        data.extend_from_slice(&[0xED, 0xF1]);
        data.extend_from_slice(json);

        let stored = compress(&data);
        // Should be dict-compressed (ED DE) or plain-zstd — not raw.
        assert!(
            stored.starts_with(&MAGIC_DICT_ZSTD) || stored.starts_with(&MAGIC_PLAIN_ZSTD),
            "expected compressed output, got magic {:02x} {:02x}",
            stored[0],
            stored[1]
        );
        assert_eq!(decompress(&stored).unwrap(), data);
    }

    #[test]
    fn tree_dict_compression_beats_plain_for_typical_tree() {
        // Typical tree JSON: dict compression should beat plain zstd in compressed size.
        let json = format!(
            r#"{{"kind":"normal","entries":[{}]}}"#,
            (0..10).map(|i| {
                let hash = format!("{:064x}", i as u128 * 17 + 42);
                format!(
                    r#"{{"hash":"{hash}","kind":"blob","mtime":"2026-05-16T10:00:00Z","name":"file{i:04}.txt","size":{}}}"#,
                    100 + i * 50
                )
            }).collect::<Vec<_>>().join(",")
        );
        let mut data = Vec::new();
        data.extend_from_slice(&[0xED, 0xF1]);
        data.extend_from_slice(json.as_bytes());

        let dict_payload = compress_zstd_bytes(&data, Some(TREE_DICT_V1));
        let plain_payload = compress_zstd_bytes(&data, None);
        assert!(
            dict_payload.len() <= plain_payload.len(),
            "dict ({}) should be <= plain ({}) for tree JSON",
            dict_payload.len(),
            plain_payload.len()
        );
    }

    #[test]
    fn roundtrip_via_decompress() {
        let data: Vec<u8> = (0u8..128).cycle().take(8192).collect();
        let stored = compress(&data);
        let out = decompress(&stored).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    #[ignore]
    fn measure_compression_ratios() {
        use crate::object::{Tree, TreeEntry};
        use chrono::{TimeZone, Utc};

        // Helper: build a serialised tree (ED F1 + JSON).
        let make_tree = |n: usize| -> Vec<u8> {
            let entries: Vec<TreeEntry> = (0..n)
                .map(|i| {
                    let hash = crate::object::Hash::compute(format!("file{i:04}").as_bytes());
                    TreeEntry::Blob {
                        name: format!("file{i:04}.txt"),
                        hash,
                        mtime: Some(Utc.with_ymd_and_hms(2026, 5, 16, 10, 0, 0).unwrap()),
                        size: 100 + i as u64 * 50,
                        mode: None,
                    }
                })
                .collect();
            Tree::Normal { entries }.serialise()
        };

        // Helper: blob of random-ish bytes (simulates text file content).
        let make_blob = |n: usize| -> Vec<u8> {
            let content: Vec<u8> = (0..n).map(|i| ((i * 17 + 3) & 0xFF) as u8).collect();
            crate::object::serialise_blob(&content)
        };

        println!(
            "\n{:<35} {:>8} {:>8} {:>8} {:>8} {:>7}",
            "scenario", "raw", "plain", "dict", "stored", "ratio"
        );
        println!("{}", "-".repeat(80));

        for n in [1usize, 3, 5, 10, 20, 50] {
            let data = make_tree(n);
            let plain_payload = compress_zstd_bytes(&data, None);
            let dict_payload = compress_zstd_bytes(&data, Some(TREE_DICT_V1));
            let stored = compress(&data);
            let magic = if stored.starts_with(&MAGIC_DICT_ZSTD) {
                "ED DE"
            } else if stored.starts_with(&MAGIC_PLAIN_ZSTD) {
                "ED DF"
            } else {
                "raw  "
            };
            println!(
                "{:<35} {:>8} {:>8} {:>8} {:>8} {:>6.1}%  {}",
                format!("tree({n} entries)"),
                data.len(),
                2 + plain_payload.len(),
                2 + dict_payload.len(),
                stored.len(),
                stored.len() as f64 / data.len() as f64 * 100.0,
                magic,
            );
        }

        println!();
        for n in [64usize, 256, 1024, 4096, 65536] {
            let data = make_blob(n);
            let plain_payload = compress_zstd_bytes(&data, None);
            let stored = compress(&data);
            let magic = if stored.starts_with(&MAGIC_PLAIN_ZSTD) {
                "ED DF"
            } else {
                "raw  "
            };
            println!(
                "{:<35} {:>8} {:>8} {:>8} {:>8} {:>6.1}%  {}",
                format!("blob({n} bytes)"),
                data.len(),
                2 + plain_payload.len(),
                "-".to_string(),
                stored.len(),
                stored.len() as f64 / data.len() as f64 * 100.0,
                magic,
            );
        }
    }

    #[test]
    fn decompress_dict_zstd() {
        // Verify that decompress handles the ED DE (tree-dict zstd) magic.
        let json = br#"{"kind":"normal","entries":[]}"#;
        let mut data = Vec::new();
        data.extend_from_slice(&[0xED, 0xF1]);
        data.extend_from_slice(json);

        // Force dict compression and prepend the ED DE magic.
        let payload = compress_zstd_bytes(&data, Some(TREE_DICT_V1));
        let mut stored = Vec::new();
        stored.extend_from_slice(&MAGIC_DICT_ZSTD);
        stored.extend_from_slice(&payload);

        let out = decompress(&stored).unwrap();
        assert_eq!(out, data);
    }
}
