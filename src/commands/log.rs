//! `omemfs log` subcommand group.
//!
//! Provides `log ls` (list log files), `log show` (filter/display) and
//! `log timers` (timer span statistics).
//! Does not write its own log file.
//!
//! See `design/10_log_analysis.md` for the full specification.

use std::collections::HashMap;
use std::io::{self, BufRead};
use std::path::PathBuf;

use crate::debug::Layer;
use crate::error::Error;
use crate::log_parser::{self, LogLine, TimerSpan};
use crate::term;

// ---------------------------------------------------------------------------
// Option types
// ---------------------------------------------------------------------------

pub struct LogLsOptions {
    pub work_dir: PathBuf,
    pub cmd: Option<String>,
    pub count: usize,
}

pub struct LogShowOptions {
    pub work_dir: PathBuf,
    /// REF: None = latest, Some("@N") = nth entry, Some(logical) = by name, Some(path) = direct
    pub file: Option<String>,
    pub layers: Vec<String>,
    pub grep: Option<String>,
}

pub struct LogTimersOptions {
    pub work_dir: PathBuf,
    /// REF: same resolution as LogShowOptions.file
    pub file: Option<String>,
    pub sort: String,
    pub layers: Vec<String>,
}

// ---------------------------------------------------------------------------
// Log entry (used by ls and REF resolution)
// ---------------------------------------------------------------------------

struct LogEntry {
    /// Logical name (filename without .log suffix)
    logical_name: String,
    path: PathBuf,
    /// Parsed timestamp string, e.g. "2026-05-22 14:30:12"
    timestamp: String,
    size: u64,
    is_latest: bool,
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

pub fn run_ls(opts: LogLsOptions) -> Result<(), Error> {
    let logs_dir = find_logs_dir(&opts.work_dir)?;
    let latest_target = resolve_latest_target(&logs_dir);
    let mut entries = scan_log_entries(&logs_dir, &latest_target)?;

    if let Some(cmd) = &opts.cmd {
        entries.retain(|e| e.logical_name.contains(cmd.as_str()));
    }
    entries.truncate(opts.count);

    let logical_width = entries
        .iter()
        .map(|e| e.logical_name.len())
        .max()
        .unwrap_or(12)
        .max(12);

    println!("{}:", logs_dir.display());
    for (i, entry) in entries.iter().enumerate() {
        let latest_marker = if entry.is_latest { "  [latest]" } else { "" };
        println!(
            "  {:>2}  {:<lw$}  {}  {}{}",
            i + 1,
            entry.logical_name,
            entry.timestamp,
            fmt_size(entry.size),
            latest_marker,
            lw = logical_width,
        );
    }
    Ok(())
}

pub fn run_show(opts: LogShowOptions) -> Result<(), Error> {
    let path = resolve_ref(opts.file.as_deref(), &opts.work_dir)?;
    let layer_filter = parse_layer_filters(&opts.layers)?;
    let lines = read_raw_lines(&path)?;
    let use_color = term::color_enabled(term::ColorChoice::Auto, atty::is(atty::Stream::Stdout));

    for raw in &lines {
        let Some(parsed) = log_parser::parse_line(raw) else {
            continue;
        };
        if !layer_matches(&parsed.layer, &layer_filter) {
            continue;
        }
        if let Some(pat) = &opts.grep
            && !parsed.text.contains(pat.as_str())
        {
            continue;
        }
        if use_color {
            let colored = colorize_prefix(raw, parsed.layer);
            println!("{}", colored);
        } else {
            println!("{}", raw);
        }
    }
    Ok(())
}

pub fn run_timers(opts: LogTimersOptions) -> Result<(), Error> {
    let path = resolve_ref(opts.file.as_deref(), &opts.work_dir)?;
    let layer_filter = parse_layer_filters(&opts.layers)?;
    let lines = read_raw_lines(&path)?;
    let parsed: Vec<LogLine> = lines
        .iter()
        .filter_map(|l| log_parser::parse_line(l))
        .collect();

    let mut spans = log_parser::collect_spans(&parsed);

    if !layer_filter.is_empty() {
        spans.retain(|s| layer_filter.contains(&s.layer));
    }

    let sort_key = opts.sort.to_lowercase();
    print_timers(&spans, &sort_key);
    Ok(())
}

// ---------------------------------------------------------------------------
// Log directory resolution
// ---------------------------------------------------------------------------

fn find_logs_dir(work_dir: &std::path::Path) -> Result<PathBuf, Error> {
    let mut dir = work_dir;
    loop {
        let candidate = dir.join(".omemfs").join("logs");
        if candidate.is_dir() {
            return Ok(candidate);
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => break,
        }
    }
    Err(Error::Other(
        "no .omemfs/logs/ found; run a command first".to_string(),
    ))
}

// ---------------------------------------------------------------------------
// Log entry scanning
// ---------------------------------------------------------------------------

fn scan_log_entries(
    logs_dir: &std::path::Path,
    latest_target: &Option<String>,
) -> Result<Vec<LogEntry>, Error> {
    let read_dir = std::fs::read_dir(logs_dir)
        .map_err(|e| Error::Other(format!("cannot read logs directory: {}", e)))?;

    let mut entries: Vec<LogEntry> = Vec::new();
    for entry in read_dir {
        let entry = entry.map_err(|e| Error::Other(format!("cannot read log entry: {}", e)))?;
        let name = entry.file_name().to_string_lossy().into_owned();

        // Skip latest.log symlink and non-.log files
        if name == "latest.log" || !name.ends_with(".log") {
            continue;
        }

        let logical_name = name[..name.len() - 4].to_string();
        let timestamp = parse_timestamp_from_stem(&logical_name);
        let path = entry.path();
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let is_latest = latest_target
            .as_deref()
            .map(|t| t == name || t == logical_name)
            .unwrap_or(false);

        entries.push(LogEntry {
            logical_name,
            path,
            timestamp,
            size,
            is_latest,
        });
    }

    // Sort newest first (by logical name which starts with YYYYMMDD-HHMMSS)
    entries.sort_by(|a, b| b.logical_name.cmp(&a.logical_name));
    Ok(entries)
}

/// Resolve the filename that latest.log points to (just the filename, no dir).
fn resolve_latest_target(logs_dir: &std::path::Path) -> Option<String> {
    let latest = logs_dir.join("latest.log");
    // Try reading as symlink first
    #[cfg(unix)]
    if let Ok(target) = std::fs::read_link(&latest) {
        return target.file_name().map(|n| n.to_string_lossy().into_owned());
    }
    // Fallback: compare file contents via metadata (inode equality not portable)
    None
}

/// Parse "YYYYMMDD-HHMMSS-cmd" stem into "YYYY-MM-DD HH:MM:SS".
fn parse_timestamp_from_stem(stem: &str) -> String {
    // Expected prefix: YYYYMMDD-HHMMSS
    if stem.len() < 15 {
        return stem.to_string();
    }
    let date = &stem[..8];
    let time = &stem[9..15];
    if date.len() == 8 && time.len() == 6 && stem.as_bytes()[8] == b'-' {
        format!(
            "{}-{}-{} {}:{}:{}",
            &date[..4],
            &date[4..6],
            &date[6..8],
            &time[..2],
            &time[2..4],
            &time[4..6],
        )
    } else {
        stem.to_string()
    }
}

fn fmt_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} M", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} K", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

// ---------------------------------------------------------------------------
// REF resolution
// ---------------------------------------------------------------------------

/// Resolve a REF string (or None) to a log file path.
///
/// Resolution order:
///   1. None          → latest.log (walk up directory tree)
///   2. "@N"          → N-th entry from scan_log_entries (1-based)
///   3. logical name  → <logs_dir>/<name>.log (no '/', no '.', not starting with '@')
///   4. otherwise     → treat as PathBuf directly
fn resolve_ref(file_ref: Option<&str>, work_dir: &std::path::Path) -> Result<PathBuf, Error> {
    match file_ref {
        None => resolve_latest(work_dir),
        Some(r) if r.starts_with('@') => {
            // Need the log directory and entry list — find them once here.
            let logs_dir = find_logs_dir(work_dir)?;
            let latest_target = resolve_latest_target(&logs_dir);
            resolve_at_ref(r, &logs_dir, &latest_target)
        }
        Some(r) if is_logical_name(r) => {
            let logs_dir = find_logs_dir(work_dir)?;
            resolve_logical_name(r, &logs_dir)
        }
        Some(r) => Ok(PathBuf::from(r)),
    }
}

fn resolve_latest(work_dir: &std::path::Path) -> Result<PathBuf, Error> {
    let mut dir = work_dir;
    loop {
        let candidate = dir.join(".omemfs").join("logs").join("latest.log");
        if candidate.exists() {
            return Ok(candidate);
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => break,
        }
    }
    Err(Error::Other(
        "no .omemfs/logs/latest.log found; run a command first or provide a REF argument"
            .to_string(),
    ))
}

fn resolve_at_ref(
    at_ref: &str,
    logs_dir: &std::path::Path,
    latest_target: &Option<String>,
) -> Result<PathBuf, Error> {
    let n: usize = at_ref[1..]
        .parse()
        .map_err(|_| Error::Other(format!("invalid REF '{}': expected @<number>", at_ref)))?;
    if n == 0 {
        return Err(Error::Other(format!(
            "invalid REF '{}': index must be 1 or greater",
            at_ref
        )));
    }
    let entries = scan_log_entries(logs_dir, latest_target)?;
    entries
        .into_iter()
        .nth(n - 1)
        .map(|e| e.path)
        .ok_or_else(|| Error::Other(format!("no log entry at index {}", n)))
}

fn resolve_logical_name(name: &str, logs_dir: &std::path::Path) -> Result<PathBuf, Error> {
    let path = logs_dir.join(format!("{}.log", name));
    if path.exists() {
        Ok(path)
    } else {
        Err(Error::Other(format!(
            "log file not found for logical name '{}' (tried {})",
            name,
            logs_dir.join(format!("{}.log", name)).display()
        )))
    }
}

/// Returns true if the string looks like a logical name rather than a file path.
/// A logical name has no '/', no '.', and does not start with '@'.
fn is_logical_name(s: &str) -> bool {
    !s.starts_with('@') && !s.contains('/') && !s.contains('.')
}

// ---------------------------------------------------------------------------
// Raw line reading
// ---------------------------------------------------------------------------

fn read_raw_lines(path: &std::path::Path) -> Result<Vec<String>, Error> {
    let file = std::fs::File::open(path)
        .map_err(|e| Error::Other(format!("cannot open log file '{}': {}", path.display(), e)))?;
    let reader = io::BufReader::new(file);
    let lines: Result<Vec<String>, _> = reader.lines().collect();
    lines.map_err(|e| Error::Other(format!("error reading log file: {}", e)))
}

// ---------------------------------------------------------------------------
// Layer filter helpers
// ---------------------------------------------------------------------------

fn parse_layer_filters(specs: &[String]) -> Result<Vec<Layer>, Error> {
    let mut out = Vec::new();
    for s in specs {
        match log_parser::parse_layer_filter(s) {
            Some(l) => out.push(l),
            None => {
                return Err(Error::Other(format!(
                    "unknown layer '{}'; valid values are L1–L7",
                    s
                )));
            }
        }
    }
    Ok(out)
}

fn layer_matches(layer: &Layer, filter: &[Layer]) -> bool {
    filter.is_empty() || filter.contains(layer)
}

// ---------------------------------------------------------------------------
// Colorize prefix for `log show`
// ---------------------------------------------------------------------------

fn colorize_prefix(raw: &str, layer: Layer) -> String {
    // Replace "[omemfs Lx xxx ]" with a coloured version.
    // The prefix is exactly "[omemfs " (8) + 7-char tag + "]" = 16 chars.
    if raw.len() < 16 {
        return raw.to_string();
    }
    let prefix = &raw[..16]; // "[omemfs L1 cmd ]"
    let rest = &raw[16..];
    format!("\x1b[{}m{}\x1b[0m{}", layer.color_code(), prefix, rest)
}

// ---------------------------------------------------------------------------
// Timer statistics output
// ---------------------------------------------------------------------------

#[derive(Default)]
struct LabelStats {
    layer: Option<Layer>,
    count: u64,
    total_secs: f64,
    max_secs: f64,
}

impl LabelStats {
    fn add(&mut self, span: &TimerSpan) {
        self.layer = Some(span.layer);
        self.count += 1;
        let d = span.duration_secs();
        self.total_secs += d;
        if d > self.max_secs {
            self.max_secs = d;
        }
    }

    fn avg_secs(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.total_secs / self.count as f64
        }
    }
}

/// Sum per-layer total time (seconds) for the Layer breakdown.
///
/// L1 phase timers nest inside the root command span (also L1); counting them
/// would double-count wall clock. So L1 sums only depth-0 (root) spans, while
/// sub-layers (L2–L7) sum spans at all depths.
pub(crate) fn compute_layer_totals(spans: &[TimerSpan]) -> [f64; 7] {
    let mut totals = [0.0_f64; 7];
    for span in spans {
        if span.layer == Layer::L1 && span.depth != 0 {
            continue;
        }
        totals[span.layer.idx()] += span.duration_secs();
    }
    totals
}

fn print_timers(spans: &[TimerSpan], sort_key: &str) {
    // Compute layer totals from raw spans (depth-aware) for the Layer breakdown.
    let layer_totals = compute_layer_totals(spans);

    // Aggregate by (label, layer)
    let mut map: HashMap<(String, usize), LabelStats> = HashMap::new();
    for span in spans {
        let key = (span.label.clone(), span.layer.idx());
        map.entry(key).or_default().add(span);
    }

    if map.is_empty() {
        println!("No timer spans found.");
        return;
    }

    // Sort
    let mut rows: Vec<((String, usize), LabelStats)> = map.into_iter().collect();
    match sort_key {
        "avg" => rows.sort_by(|a, b| b.1.avg_secs().total_cmp(&a.1.avg_secs())),
        "count" => rows.sort_by_key(|row| std::cmp::Reverse(row.1.count)),
        "max" => rows.sort_by(|a, b| b.1.max_secs.total_cmp(&a.1.max_secs)),
        _ => rows.sort_by(|a, b| b.1.total_secs.total_cmp(&a.1.total_secs)),
    }

    // Determine column widths
    let label_width = rows
        .iter()
        .map(|((label, _), _)| label.chars().count().min(40))
        .max()
        .unwrap_or(5)
        .max(5);

    println!(
        "{:<lw$}  {:<5}  {:>6}  {:>9}  {:>9}  {:>9}",
        "Label",
        "Layer",
        "Count",
        "Total",
        "Avg",
        "Max",
        lw = label_width,
    );
    println!(
        "{}",
        "-".repeat(label_width + 2 + 5 + 2 + 6 + 2 + 9 + 2 + 9 + 2 + 9)
    );

    for ((label, _), stats) in &rows {
        let layer = stats.layer.unwrap();
        let label_display = truncate_chars(label, 40);

        println!(
            "{:<lw$}  {:<5}  {:>6}  {:>9}  {:>9}  {:>9}",
            label_display,
            layer_tag_short(layer),
            stats.count,
            fmt_duration(stats.total_secs),
            fmt_duration(stats.avg_secs()),
            fmt_duration(stats.max_secs),
            lw = label_width,
        );
    }

    // Layer breakdown summary — percentages are relative to L1 (wall clock)
    let l1_total = layer_totals[Layer::L1.idx()];
    let reference = if l1_total > 0.0 {
        l1_total
    } else {
        layer_totals.iter().cloned().fold(0.0_f64, f64::max)
    };
    if reference > 0.0 {
        println!();
        println!("Layer breakdown (by total time):");

        let all_layers = [
            Layer::L1,
            Layer::L2,
            Layer::L3,
            Layer::L4,
            Layer::L5,
            Layer::L6,
            Layer::L7,
        ];
        let mut layer_rows: Vec<(Layer, f64)> = all_layers
            .iter()
            .map(|&l| (l, layer_totals[l.idx()]))
            .filter(|(_, t)| *t > 0.0)
            .collect();
        layer_rows.sort_by(|a, b| b.1.total_cmp(&a.1));

        for (layer, total) in &layer_rows {
            println!(
                "  {}  {:>9}  {:>5.1}%",
                layer.tag(),
                fmt_duration(*total),
                total / reference * 100.0,
            );
        }
    }
}

fn layer_tag_short(layer: Layer) -> &'static str {
    match layer {
        Layer::L1 => "L1",
        Layer::L2 => "L2",
        Layer::L3 => "L3",
        Layer::L4 => "L4",
        Layer::L5 => "L5",
        Layer::L6 => "L6",
        Layer::L7 => "L7",
    }
}

/// Truncate `s` to at most `max_chars` Unicode scalar values, slicing on a
/// char boundary. Byte-index slicing (`&s[..max_chars]`) panics when the cut
/// point lands inside a multi-byte UTF-8 sequence (e.g. Japanese labels), so
/// this iterates by `char` instead.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

/// Format a duration in seconds as a human-readable string.
fn fmt_duration(secs: f64) -> String {
    if secs >= 1.0 {
        format!("{:.3}s", secs)
    } else if secs >= 0.001 {
        format!("{}ms", (secs * 1000.0).round() as u64)
    } else {
        format!("{}us", (secs * 1_000_000.0).round() as u64)
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_chars_ascii_under_limit() {
        assert_eq!(truncate_chars("hello", 40), "hello");
    }

    #[test]
    fn truncate_chars_ascii_over_limit() {
        let s = "a".repeat(50);
        assert_eq!(truncate_chars(&s, 40).chars().count(), 40);
    }

    #[test]
    fn truncate_chars_multibyte_no_panic() {
        // 30 Japanese characters (3 bytes each in UTF-8). Truncating to 40 chars
        // returns the whole string; truncating to 10 chars must slice on a char
        // boundary without panicking (byte-index 40 would land mid-character).
        let s = "あ".repeat(30);
        assert_eq!(truncate_chars(&s, 40), s);
        let cut = truncate_chars(&s, 10);
        assert_eq!(cut.chars().count(), 10);
        assert_eq!(cut, "あ".repeat(10));
    }

    #[test]
    fn is_logical_name_true() {
        assert!(is_logical_name("20260522-143012-push"));
        assert!(is_logical_name("20260522-143012-pull"));
    }

    #[test]
    fn is_logical_name_false_for_paths() {
        assert!(!is_logical_name("/absolute/path.log"));
        assert!(!is_logical_name("relative/path.log"));
        assert!(!is_logical_name("file.log"));
    }

    #[test]
    fn is_logical_name_false_for_at_ref() {
        assert!(!is_logical_name("@1"));
        assert!(!is_logical_name("@10"));
    }

    #[test]
    fn parse_timestamp_well_formed() {
        let ts = parse_timestamp_from_stem("20260522-143012-push");
        assert_eq!(ts, "2026-05-22 14:30:12");
    }

    #[test]
    fn parse_timestamp_short_stem() {
        let ts = parse_timestamp_from_stem("short");
        assert_eq!(ts, "short");
    }

    #[test]
    fn fmt_size_bytes() {
        assert_eq!(fmt_size(500), "500 B");
    }

    #[test]
    fn fmt_size_kilo() {
        assert_eq!(fmt_size(2048), "2.0 K");
    }

    #[test]
    fn fmt_size_mega() {
        assert_eq!(fmt_size(1024 * 1024), "1.0 M");
    }

    #[test]
    fn compute_layer_totals_excludes_nested_l1() {
        // depth-0 L1 span (10s) + depth-1 L1 span (3s, nested) + depth-1 L7 span (1s).
        // L1 total must be 10.0 (not 13.0); L7 total must be 1.0.
        let spans = vec![
            TimerSpan {
                label: "ls".into(),
                depth: 0,
                layer: Layer::L1,
                start_secs: 0.0,
                end_secs: 10.0,
            },
            TimerSpan {
                label: "scan".into(),
                depth: 1,
                layer: Layer::L1,
                start_secs: 0.5,
                end_secs: 3.5,
            },
            TimerSpan {
                label: "store".into(),
                depth: 1,
                layer: Layer::L7,
                start_secs: 1.0,
                end_secs: 2.0,
            },
        ];
        let totals = compute_layer_totals(&spans);
        assert!(
            (totals[Layer::L1.idx()] - 10.0).abs() < 1e-9,
            "L1 total should be 10.0, got {}",
            totals[Layer::L1.idx()]
        );
        assert!(
            (totals[Layer::L7.idx()] - 1.0).abs() < 1e-9,
            "L7 total should be 1.0, got {}",
            totals[Layer::L7.idx()]
        );
    }

    #[test]
    fn print_timers_does_not_panic_for_non_finite_spans() {
        let spans = vec![
            TimerSpan {
                label: "bad".into(),
                depth: 0,
                layer: Layer::L1,
                start_secs: f64::NAN,
                end_secs: 1.0,
            },
            TimerSpan {
                label: "good".into(),
                depth: 0,
                layer: Layer::L1,
                start_secs: 0.0,
                end_secs: 2.0,
            },
        ];
        for sort_key in ["avg", "max", "total"] {
            print_timers(&spans, sort_key);
        }
    }
}
