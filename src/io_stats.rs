/// io_stats — append I/O records to `.omemfs/io_stats.jsonl` and read them back.
///
/// Each record is a single JSON line written after a successful remote command.
/// `omemfs stats` displays only the last 20 lines. Before appending, the file
/// is rotated (newest half kept) once it exceeds `MAX_IO_STATS_BYTES`, so it
/// does not grow without bound (refactor-instructions.md G4a; design/04
/// "io_stats.jsonl" Notes).
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::store::stats::IoRecord;

// ---------------------------------------------------------------------------
// IoStatsRecord — one JSONL line
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IoStatsRecord {
    pub ts: String,
    pub cmd: String,
    pub remote: String,
    pub exists_found: u64,
    pub exists_miss: u64,
    pub writes: u64,
    pub write_bytes: u64,
    pub reads: u64,
    pub read_bytes: u64,
    pub pack_files_written: u64,
    pub pack_sizes_bytes: Vec<u64>,
    /// Wall-clock milliseconds this command took, from opening its remote
    /// connection to writing this record. Collected so `omemfs pack`'s
    /// scheduling can be reconsidered later from real data (design/04
    /// "io_stats.jsonl" Notes). Records written before this field existed
    /// lack the key and deserialise to 0 (via `default`).
    #[serde(default)]
    pub duration_ms: u64,
    /// Present only for `push` runs: the number of delta index files listed
    /// in `INDEX_ROOT.delta_hashes` immediately after this push's CAS write
    /// (including the delta this push itself just added). Direct input for a
    /// delta-count-based `omemfs pack` scheduling policy. Non-push records
    /// omit the field entirely (via `skip_serializing_if`) and deserialise to
    /// `None` (via `default`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deltas_after: Option<u64>,
    /// Present only for `pack` runs. Carries pack-layer tuning metrics so
    /// `omemfs stats` can show a "Pack effectiveness" section. Non-pack records
    /// omit the field entirely (via `skip_serializing_if`) and deserialise to
    /// `None` (via `default`), so older records stay forward-compatible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack_detail: Option<PackDetail>,
}

/// Pack-layer tuning metrics, collected during one `omemfs pack` run and used
/// for cloud-cost analysis of the pack size thresholds (PACK_TARGET_SIZE,
/// PACK_MAX_SIZE, CONSOLIDATION_THRESHOLD, COLD_SHARD_SPLIT_THRESHOLD).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct PackDetail {
    /// Number of delta index files merged into the hot index.
    pub deltas_merged: u64,
    /// Consolidation candidate pack files (< CONSOLIDATION_THRESHOLD) found.
    pub packs_before: u64,
    /// New consolidated pack files written.
    pub packs_after: u64,
    /// Total bytes read from candidate packs (slices re-packed).
    pub consolidated_bytes_in: u64,
    /// Total bytes written to new consolidated packs.
    pub consolidated_bytes_out: u64,
    /// Cold shard splits performed this run (0 or 1).
    pub cold_splits: u64,
    /// Final hot index entry count after merge + split + consolidate.
    pub hot_index_entries: u64,
    /// Element count of the regenerated bloom filter.
    pub bloom_elements: u64,
    /// Sizes of the newly-written consolidated pack files.
    pub pack_sizes_after: Vec<u64>,
    /// Lower-bound estimate of bytes made unreachable from INDEX_ROOT by this
    /// run: the previous hot index, the merged-away delta indexes, and the
    /// previous Bloom filter (each unconditionally replaced) counted in full,
    /// plus `consolidated_bytes_in` as an approximation for consolidated small
    /// pack files (see design/04 "omemfs pack" -> "When to run").
    pub orphaned_bytes: u64,
}

// ---------------------------------------------------------------------------
// Retention
// ---------------------------------------------------------------------------

/// Size threshold above which `io_stats.jsonl` is rotated before the next
/// append. `.omemfs/io_stats.jsonl` otherwise grows without bound (refactor-
/// instructions.md G4a).
const MAX_IO_STATS_BYTES: u64 = 10 * 1024 * 1024; // 10 MiB

/// If `path` exceeds [`MAX_IO_STATS_BYTES`], rewrite it keeping only the
/// newest (line-count) half of its records. No-op if the file is absent,
/// under the threshold, or has fewer than 2 non-empty lines (nothing
/// meaningful to trim).
///
/// Best-effort: any I/O error during rotation is silently ignored (this is
/// pure housekeeping -- io_stats is a diagnostic log, and a rotation failure
/// must not abort the command that triggered it; the file simply keeps
/// growing until a future append succeeds in rotating it).
fn rotate_if_oversized(path: &Path) {
    let Ok(meta) = fs::metadata(path) else { return };
    if meta.len() <= MAX_IO_STATS_BYTES {
        return;
    }
    let Ok(content) = fs::read_to_string(path) else {
        return;
    };
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() < 2 {
        return;
    }
    let half = lines.len() / 2;
    let mut out = lines[half..].join("\n");
    out.push('\n');
    let _ = fs::write(path, out);
}

// ---------------------------------------------------------------------------
// Append one record
// ---------------------------------------------------------------------------

/// Build an `IoStatsRecord` from the shared `IoRecord` and append it to the
/// JSONL file at `<omemfs_dir>/io_stats.jsonl`. Errors are silently ignored:
/// io_stats is a diagnostic log; a write failure must not abort the command.
///
/// `duration_ms` is the caller's own wall-clock measurement of the run (see
/// design/04 "io_stats.jsonl" Notes) -- io_stats has no way to time the
/// command itself, since it is only invoked at the very end of a successful
/// run.
pub fn append_record(
    omemfs_dir: &Path,
    cmd: &str,
    remote: &str,
    record: &Arc<IoRecord>,
    duration_ms: u64,
) {
    append_record_with_detail(omemfs_dir, cmd, remote, record, duration_ms, None);
}

/// Like `append_record`, but also stores an optional `PackDetail`. Used by
/// `omemfs pack` to record pack-layer tuning metrics alongside real I/O counts.
pub fn append_record_with_detail(
    omemfs_dir: &Path,
    cmd: &str,
    remote: &str,
    record: &Arc<IoRecord>,
    duration_ms: u64,
    pack_detail: Option<PackDetail>,
) {
    use std::sync::atomic::Ordering::Relaxed;

    let ts = iso8601_utc_now();
    let pack_sizes = record.pack_sizes_bytes.lock().unwrap().clone();
    let deltas_after = *record.delta_count_after.lock().unwrap();

    let entry = IoStatsRecord {
        ts,
        cmd: cmd.to_string(),
        remote: remote.to_string(),
        exists_found: record.exists_found.load(Relaxed),
        exists_miss: record.exists_miss.load(Relaxed),
        writes: record.writes.load(Relaxed),
        write_bytes: record.write_bytes.load(Relaxed),
        reads: record.reads.load(Relaxed),
        read_bytes: record.read_bytes.load(Relaxed),
        pack_files_written: record.pack_files_written.load(Relaxed),
        pack_sizes_bytes: pack_sizes,
        duration_ms,
        deltas_after,
        pack_detail,
    };

    let json = match serde_json::to_string(&entry) {
        Ok(j) => j,
        Err(_) => return,
    };

    let path = omemfs_dir.join("io_stats.jsonl");
    rotate_if_oversized(&path);
    if let Ok(mut file) = fs::OpenOptions::new().append(true).create(true).open(&path) {
        let _ = writeln!(file, "{}", json);
    }
}

// ---------------------------------------------------------------------------
// Read up to `limit` most-recent records
// ---------------------------------------------------------------------------

/// Read the JSONL file and return up to `limit` most-recent parsed records.
/// Returns an empty vec if the file is absent, empty, or unreadable.
pub fn read_recent(omemfs_dir: &Path, limit: usize) -> Vec<IoStatsRecord> {
    let path = omemfs_dir.join("io_stats.jsonl");
    let file = match fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };

    let reader = io::BufReader::new(file);
    let lines: Vec<String> = reader
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.trim().is_empty())
        .collect();

    // Take the last `limit` lines.
    let tail = if lines.len() > limit {
        &lines[lines.len() - limit..]
    } else {
        &lines[..]
    };

    tail.iter()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Read ALL records from the JSONL file (for computing io_totals).
pub fn read_all(omemfs_dir: &Path) -> Vec<IoStatsRecord> {
    let path = omemfs_dir.join("io_stats.jsonl");
    let file = match fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };

    let reader = io::BufReader::new(file);
    reader
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect()
}

// ---------------------------------------------------------------------------
// Timestamp helper
// ---------------------------------------------------------------------------

/// Return the current UTC time as an ISO-8601 string (e.g. "2026-06-13T02:05:11Z").
/// Uses `chrono` which is already in Cargo.toml.
fn iso8601_utc_now() -> String {
    use chrono::Utc;
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn base_record() -> IoStatsRecord {
        IoStatsRecord {
            ts: "2026-06-13T01:45:12Z".to_string(),
            cmd: "pack".to_string(),
            remote: "origin".to_string(),
            exists_found: 1,
            exists_miss: 2,
            writes: 3,
            write_bytes: 4,
            reads: 5,
            read_bytes: 6,
            pack_files_written: 0,
            pack_sizes_bytes: vec![],
            duration_ms: 0,
            deltas_after: None,
            pack_detail: None,
        }
    }

    #[test]
    fn roundtrip_without_pack_detail_omits_field() {
        let rec = base_record();
        let json = serde_json::to_string(&rec).unwrap();
        // The field must be omitted entirely when None.
        assert!(
            !json.contains("pack_detail"),
            "pack_detail must be omitted: {}",
            json
        );
        let back: IoStatsRecord = serde_json::from_str(&json).unwrap();
        assert!(back.pack_detail.is_none());
        assert_eq!(back.cmd, "pack");
    }

    #[test]
    fn roundtrip_with_pack_detail() {
        let mut rec = base_record();
        rec.pack_detail = Some(PackDetail {
            deltas_merged: 3,
            packs_before: 5,
            packs_after: 2,
            consolidated_bytes_in: 1887436,
            consolidated_bytes_out: 1887436,
            cold_splits: 1,
            hot_index_entries: 128,
            bloom_elements: 140,
            pack_sizes_after: vec![1153433, 716800],
            orphaned_bytes: 2097152,
        });
        let json = serde_json::to_string(&rec).unwrap();
        assert!(json.contains("pack_detail"));
        let back: IoStatsRecord = serde_json::from_str(&json).unwrap();
        let d = back
            .pack_detail
            .expect("pack_detail must survive round-trip");
        assert_eq!(d.deltas_merged, 3);
        assert_eq!(d.packs_before, 5);
        assert_eq!(d.packs_after, 2);
        assert_eq!(d.consolidated_bytes_in, 1887436);
        assert_eq!(d.consolidated_bytes_out, 1887436);
        assert_eq!(d.cold_splits, 1);
        assert_eq!(d.hot_index_entries, 128);
        assert_eq!(d.bloom_elements, 140);
        assert_eq!(d.pack_sizes_after, vec![1153433, 716800]);
        assert_eq!(d.orphaned_bytes, 2097152);
    }

    #[test]
    fn legacy_record_without_pack_detail_deserialises() {
        // A record written before pack_detail existed (no field) must parse.
        let json = r#"{"ts":"2026-06-13T00:00:00Z","cmd":"push","remote":"origin",
            "exists_found":0,"exists_miss":0,"writes":1,"write_bytes":10,
            "reads":0,"read_bytes":0,"pack_files_written":0,"pack_sizes_bytes":[]}"#;
        let back: IoStatsRecord = serde_json::from_str(json).unwrap();
        assert!(back.pack_detail.is_none());
        assert_eq!(back.cmd, "push");
        // Fields added after this record shape existed must default cleanly.
        assert_eq!(back.duration_ms, 0);
        assert!(back.deltas_after.is_none());
    }

    // --- duration_ms / deltas_after (pack-scheduling data) --------------

    #[test]
    fn duration_ms_round_trips() {
        let mut rec = base_record();
        rec.duration_ms = 8123;
        let json = serde_json::to_string(&rec).unwrap();
        assert!(json.contains("\"duration_ms\":8123"));
        let back: IoStatsRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.duration_ms, 8123);
    }

    #[test]
    fn roundtrip_without_deltas_after_omits_field() {
        // Non-push records (deltas_after == None) must omit the key entirely,
        // matching the existing pack_detail convention.
        let rec = base_record();
        let json = serde_json::to_string(&rec).unwrap();
        assert!(
            !json.contains("deltas_after"),
            "deltas_after must be omitted: {}",
            json
        );
        let back: IoStatsRecord = serde_json::from_str(&json).unwrap();
        assert!(back.deltas_after.is_none());
    }

    #[test]
    fn roundtrip_with_deltas_after() {
        let mut rec = base_record();
        rec.cmd = "push".to_string();
        rec.deltas_after = Some(42);
        let json = serde_json::to_string(&rec).unwrap();
        assert!(json.contains("\"deltas_after\":42"));
        let back: IoStatsRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.deltas_after, Some(42));
    }

    #[test]
    fn legacy_pack_detail_without_orphaned_bytes_defaults_to_zero() {
        // A pack_detail object written before orphaned_bytes existed (no key)
        // must still parse, defaulting the new field to 0.
        let json = r#"{"ts":"2026-06-13T00:00:00Z","cmd":"pack","remote":"origin",
            "exists_found":0,"exists_miss":0,"writes":1,"write_bytes":10,
            "reads":0,"read_bytes":0,"pack_files_written":0,"pack_sizes_bytes":[],
            "duration_ms":500,
            "pack_detail":{"deltas_merged":1,"packs_before":0,"packs_after":0,
              "consolidated_bytes_in":0,"consolidated_bytes_out":0,"cold_splits":0,
              "hot_index_entries":1,"bloom_elements":1,"pack_sizes_after":[]}}"#;
        let back: IoStatsRecord = serde_json::from_str(json).unwrap();
        let d = back.pack_detail.expect("pack_detail must still parse");
        assert_eq!(d.orphaned_bytes, 0);
    }

    // --- G4a: io_stats.jsonl retention ---------------------------------

    #[test]
    fn rotate_if_oversized_is_a_noop_under_the_threshold() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("io_stats.jsonl");
        fs::write(&path, "line1\nline2\nline3\n").unwrap();
        rotate_if_oversized(&path);
        assert_eq!(fs::read_to_string(&path).unwrap(), "line1\nline2\nline3\n");
    }

    #[test]
    fn rotate_if_oversized_keeps_only_the_newest_half() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("io_stats.jsonl");
        // Build a file just over MAX_IO_STATS_BYTES made of many small lines,
        // each individually identifiable by index so the "kept newest half"
        // claim can be checked precisely.
        let line = "x".repeat(100);
        let lines_needed = (MAX_IO_STATS_BYTES as usize / (line.len() + 1)) + 10;
        let mut content = String::new();
        for i in 0..lines_needed {
            content.push_str(&format!("{{\"i\":{},\"pad\":\"{}\"}}\n", i, line));
        }
        fs::write(&path, &content).unwrap();
        assert!(fs::metadata(&path).unwrap().len() > MAX_IO_STATS_BYTES);

        rotate_if_oversized(&path);

        let rotated = fs::read_to_string(&path).unwrap();
        let kept: Vec<&str> = rotated.lines().collect();
        let expected_half = lines_needed / 2;
        assert_eq!(kept.len(), lines_needed - expected_half);
        // The FIRST kept line must be the one at the halfway index (oldest
        // lines dropped, newest half retained, order preserved).
        assert!(kept[0].contains(&format!("\"i\":{}", expected_half)));
        // The LAST kept line must be the very last line originally written.
        assert!(kept[kept.len() - 1].contains(&format!("\"i\":{}", lines_needed - 1)));
    }

    #[test]
    fn append_record_rotates_before_appending_when_oversized() {
        let tmp = tempfile::TempDir::new().unwrap();
        let omemfs_dir = tmp.path();
        let path = omemfs_dir.join("io_stats.jsonl");
        let line = "x".repeat(100);
        let lines_needed = (MAX_IO_STATS_BYTES as usize / (line.len() + 1)) + 10;
        let mut content = String::new();
        for i in 0..lines_needed {
            content.push_str(&format!("{{\"i\":{},\"pad\":\"{}\"}}\n", i, line));
        }
        fs::write(&path, &content).unwrap();

        let record = Arc::new(IoRecord::default());
        append_record(omemfs_dir, "push", "origin", &record, 42);

        // File must have shrunk (rotated) and still end with a valid, freshly
        // appended IoStatsRecord.
        let after = fs::read_to_string(&path).unwrap();
        assert!(
            after.len() < content.len(),
            "file must shrink after rotation"
        );
        let last_line = after.lines().last().unwrap();
        let parsed: IoStatsRecord = serde_json::from_str(last_line).unwrap();
        assert_eq!(parsed.cmd, "push");
        assert_eq!(parsed.duration_ms, 42);
    }
}
