//! Parser for `.omemfs/logs/` log files.
//!
//! Log lines have the format:
//!   `[omemfs L1 cmd ] +0.001s   push: start`
//!
//! See `design/10_log_analysis.md` and `design/06_progress_view.md` for details.

use crate::debug::Layer;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A single parsed log line.
#[derive(Debug, Clone, PartialEq)]
pub struct LogLine {
    pub layer: Layer,
    pub elapsed_secs: f64,
    pub depth: usize,
    pub text: String,
}

/// A matched timer span (start + end pair).
#[derive(Debug, Clone, PartialEq)]
pub struct TimerSpan {
    pub label: String,
    /// Nesting depth of this span (0 = root command span, 1 = first-level nested, …).
    /// Determined by the indentation of the log lines.
    pub depth: usize,
    pub layer: Layer,
    pub start_secs: f64,
    pub end_secs: f64,
}

impl TimerSpan {
    pub fn duration_secs(&self) -> f64 {
        self.end_secs - self.start_secs
    }
}

// ---------------------------------------------------------------------------
// parse_line
// ---------------------------------------------------------------------------

/// Parse a single log line. Returns `None` for lines that do not match the format.
///
/// Expected format (from `progress::format_line`):
///   `[omemfs L1 cmd ] +0.001s   push: start`
///
/// The prefix is exactly `[omemfs ` (8 chars) + 7-char layer tag + `] ` (2 chars)
/// followed by a signed elapsed time field and the message with leading spaces.
pub fn parse_line(line: &str) -> Option<LogLine> {
    // Minimum: "[omemfs L1 cmd ] +0.000s " = 8 + 7 + 2 + 8 + 1 = 26 chars
    if !line.starts_with("[omemfs ") {
        return None;
    }
    let rest = &line[8..]; // after "[omemfs "

    // Next 7 chars: layer tag, then "] "
    if rest.len() < 9 {
        return None;
    }
    let tag = &rest[..7];
    let layer = parse_layer_tag(tag)?;

    let rest = &rest[7..];
    if !rest.starts_with("] ") {
        return None;
    }
    let rest = &rest[2..]; // after "] "

    // The elapsed field uses {:+9.3}s format: right-aligned in 9 chars + sign, so
    // there may be leading spaces before the +/- sign. Skip optional leading spaces.
    let rest = rest.trim_start_matches(' ');

    // Elapsed field: find the trailing 's' that terminates the number.
    // The format is like "+1.616s" or "+0.001s".
    let elapsed_end = rest.find('s')?;
    let elapsed_str = &rest[..elapsed_end]; // e.g. "+1.616"
    let elapsed_secs: f64 = elapsed_str.parse().ok()?;
    if !elapsed_secs.is_finite() {
        return None;
    }
    let rest = &rest[elapsed_end + 1..]; // after the 's'

    // After the elapsed field there is exactly one space separator, then the indent
    // (0 or more pairs of spaces from "  ".repeat(depth)), then the message text.
    if !rest.starts_with(' ') {
        return None;
    }
    let rest = &rest[1..]; // consume the single separator space

    // Count leading spaces to determine depth
    let leading_spaces = rest.len() - rest.trim_start_matches(' ').len();
    let depth = leading_spaces / 2;
    let text = rest[leading_spaces..].to_string();

    if text.is_empty() {
        return None;
    }

    Some(LogLine {
        layer,
        elapsed_secs,
        depth,
        text,
    })
}

fn parse_layer_tag(tag: &str) -> Option<Layer> {
    match tag {
        "L1 cmd " => Some(Layer::L1),
        "L2 ser " => Some(Layer::L2),
        "L3 chk " => Some(Layer::L3),
        "L4 cmp " => Some(Layer::L4),
        "L5 enc " => Some(Layer::L5),
        "L6 pak " => Some(Layer::L6),
        "L7 sto " => Some(Layer::L7),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// collect_spans
// ---------------------------------------------------------------------------

/// Match `label: start` / `label: end (…)` pairs and return completed spans.
///
/// Matching key: `(depth, label, layer)`. A stack entry is pushed on `start`
/// and popped (closing the span) on a matching `end`. Unmatched lines are
/// silently ignored.
pub fn collect_spans(lines: &[LogLine]) -> Vec<TimerSpan> {
    // Stack: key → list of start times (LIFO for nested same-label timers)
    let mut stack: HashMap<(usize, String, usize), Vec<f64>> = HashMap::new();
    let mut spans: Vec<TimerSpan> = Vec::new();

    for line in lines {
        if let Some(label) = line.text.strip_suffix(": start") {
            let key = (line.depth, label.to_string(), line.layer.idx());
            stack.entry(key).or_default().push(line.elapsed_secs);
        } else if let Some(rest) = line.text.strip_suffix(')') {
            // Matches "label: end (Xms)" or "label: end (Xs)" or "label: end (Xus)"
            if let Some(paren_pos) = rest.rfind(" (") {
                let label_end = &rest[..paren_pos];
                if let Some(label) = label_end.strip_suffix(": end") {
                    let key = (line.depth, label.to_string(), line.layer.idx());
                    if let Some(stack_entry) = stack.get_mut(&key) {
                        if let Some(start_secs) = stack_entry.pop() {
                            spans.push(TimerSpan {
                                label: label.to_string(),
                                depth: line.depth,
                                layer: line.layer,
                                start_secs,
                                end_secs: line.elapsed_secs,
                            });
                        }
                        if stack_entry.is_empty() {
                            stack.remove(&key);
                        }
                    }
                }
            }
        }
    }

    spans
}

// ---------------------------------------------------------------------------
// Layer tag parsing for CLI (accepts "L1", "L4 cmp", etc.)
// ---------------------------------------------------------------------------

/// Parse a user-supplied layer filter string (case-insensitive).
///
/// Accepts short forms like `"L1"` or full tag forms like `"L4 cmp"`.
pub fn parse_layer_filter(s: &str) -> Option<Layer> {
    let s = s.trim().to_uppercase();
    match s.as_str() {
        "L1" | "L1 CMD" | "L1 CMD " => Some(Layer::L1),
        "L2" | "L2 SER" | "L2 SER " => Some(Layer::L2),
        "L3" | "L3 CHK" | "L3 CHK " => Some(Layer::L3),
        "L4" | "L4 CMP" | "L4 CMP " => Some(Layer::L4),
        "L5" | "L5 ENC" | "L5 ENC " => Some(Layer::L5),
        "L6" | "L6 PAK" | "L6 PAK " => Some(Layer::L6),
        "L7" | "L7 STO" | "L7 STO " => Some(Layer::L7),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn l1(elapsed: f64, depth: usize, text: &str) -> LogLine {
        LogLine {
            layer: Layer::L1,
            elapsed_secs: elapsed,
            depth,
            text: text.to_string(),
        }
    }

    fn l7(elapsed: f64, depth: usize, text: &str) -> LogLine {
        LogLine {
            layer: Layer::L7,
            elapsed_secs: elapsed,
            depth,
            text: text.to_string(),
        }
    }

    // -- parse_line ----------------------------------------------------------

    #[test]
    fn parse_basic_line() {
        // Actual format from format_line: {:+9.3}s produces "   +0.001s" (9 chars + 's')
        let line = "[omemfs L1 cmd ]    +1.616s clone: start";
        let parsed = parse_line(line).expect("should parse");
        assert_eq!(parsed.layer, Layer::L1);
        assert!((parsed.elapsed_secs - 1.616).abs() < 1e-9);
        assert_eq!(parsed.depth, 0);
        assert_eq!(parsed.text, "clone: start");
    }

    #[test]
    fn parse_basic_line_short_elapsed() {
        let line = "[omemfs L1 cmd ]    +0.001s push: start";
        let parsed = parse_line(line).expect("should parse");
        assert_eq!(parsed.layer, Layer::L1);
        assert!((parsed.elapsed_secs - 0.001).abs() < 1e-9);
        assert_eq!(parsed.depth, 0);
        assert_eq!(parsed.text, "push: start");
    }

    #[test]
    fn parse_indented_line() {
        // depth 2 → 4 leading spaces; real format uses right-aligned elapsed
        let line = "[omemfs L7 sto ]    +1.234s     s3 put_object(delta_index): end (1230ms)";
        let parsed = parse_line(line).expect("should parse");
        assert_eq!(parsed.layer, Layer::L7);
        assert!((parsed.elapsed_secs - 1.234).abs() < 1e-9);
        assert_eq!(parsed.depth, 2);
        assert_eq!(parsed.text, "s3 put_object(delta_index): end (1230ms)");
    }

    #[test]
    fn parse_all_layers() {
        let cases = [
            ("[omemfs L1 cmd ]    +0.000s text", Layer::L1),
            ("[omemfs L2 ser ]    +0.000s text", Layer::L2),
            ("[omemfs L3 chk ]    +0.000s text", Layer::L3),
            ("[omemfs L4 cmp ]    +0.000s text", Layer::L4),
            ("[omemfs L5 enc ]    +0.000s text", Layer::L5),
            ("[omemfs L6 pak ]    +0.000s text", Layer::L6),
            ("[omemfs L7 sto ]    +0.000s text", Layer::L7),
        ];
        for (line, expected_layer) in &cases {
            let parsed = parse_line(line).expect(line);
            assert_eq!(parsed.layer, *expected_layer, "line: {}", line);
        }
    }

    #[test]
    fn parse_returns_none_for_garbage() {
        assert!(parse_line("").is_none());
        assert!(parse_line("hello world").is_none());
        assert!(parse_line("[omemfs XX xxx ]    +0.001s text").is_none());
    }

    #[test]
    fn parse_negative_elapsed() {
        // The format uses {:+9.3} so the field is always signed.
        // Negative values should not appear in practice but the parser must handle them.
        let line = "[omemfs L1 cmd ]   -0.001s text";
        let parsed = parse_line(line).expect("should parse");
        assert!(parsed.elapsed_secs < 0.0);
    }

    #[test]
    fn parse_real_log_line() {
        // Actual line from a real log file
        let line = "[omemfs L1 cmd ]    +1.616s clone: start";
        let parsed = parse_line(line).expect("should parse real line");
        assert_eq!(parsed.layer, Layer::L1);
        assert!((parsed.elapsed_secs - 1.616).abs() < 1e-9);
        assert_eq!(parsed.depth, 0);
        assert_eq!(parsed.text, "clone: start");
    }

    // -- collect_spans -------------------------------------------------------

    #[test]
    fn collect_single_span() {
        let lines = vec![
            l1(0.0, 0, "push: start"),
            l1(3.456, 0, "push: end (3456ms)"),
        ];
        let spans = collect_spans(&lines);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].label, "push");
        assert_eq!(spans[0].layer, Layer::L1);
        assert!((spans[0].start_secs - 0.0).abs() < 1e-9);
        assert!((spans[0].end_secs - 3.456).abs() < 1e-9);
        assert!((spans[0].duration_secs() - 3.456).abs() < 1e-9);
    }

    #[test]
    fn collect_nested_spans() {
        let lines = vec![
            l1(0.0, 0, "push: start"),
            l7(0.1, 1, "s3 put: start"),
            l7(1.2, 1, "s3 put: end (1100ms)"),
            l1(3.0, 0, "push: end (3000ms)"),
        ];
        let spans = collect_spans(&lines);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].label, "s3 put");
        assert_eq!(spans[0].layer, Layer::L7);
        assert_eq!(spans[1].label, "push");
        assert_eq!(spans[1].layer, Layer::L1);
    }

    #[test]
    fn collect_repeated_label() {
        let lines = vec![
            l7(0.0, 0, "upload: start"),
            l7(0.5, 0, "upload: end (500ms)"),
            l7(1.0, 0, "upload: start"),
            l7(1.8, 0, "upload: end (800ms)"),
        ];
        let spans = collect_spans(&lines);
        assert_eq!(spans.len(), 2);
        assert!((spans[0].duration_secs() - 0.5).abs() < 1e-6);
        assert!((spans[1].duration_secs() - 0.8).abs() < 1e-6);
    }

    #[test]
    fn collect_unmatched_start_ignored() {
        let lines = vec![
            l1(0.0, 0, "push: start"),
            // no matching end
        ];
        let spans = collect_spans(&lines);
        assert_eq!(spans.len(), 0);
    }

    #[test]
    fn collect_unmatched_end_ignored() {
        let lines = vec![l1(1.0, 0, "push: end (1000ms)")];
        let spans = collect_spans(&lines);
        assert_eq!(spans.len(), 0);
    }

    #[test]
    fn collect_same_label_different_depth() {
        // Same label at different depths are separate spans.
        let lines = vec![
            l1(0.0, 0, "work: start"),
            l1(0.1, 1, "work: start"),
            l1(0.5, 1, "work: end (400ms)"),
            l1(1.0, 0, "work: end (1000ms)"),
        ];
        let spans = collect_spans(&lines);
        assert_eq!(spans.len(), 2);
        // Inner span first (popped first)
        assert!((spans[0].start_secs - 0.1).abs() < 1e-9);
        assert!((spans[1].start_secs - 0.0).abs() < 1e-9);
    }

    #[test]
    fn collect_same_label_different_layer() {
        // Same label and depth but different layers are separate spans.
        let lines = vec![
            l1(0.0, 0, "op: start"),
            l7(0.0, 0, "op: start"),
            l7(0.5, 0, "op: end (500ms)"),
            l1(1.0, 0, "op: end (1000ms)"),
        ];
        let spans = collect_spans(&lines);
        assert_eq!(spans.len(), 2);
    }

    #[test]
    fn collect_span_depth_is_propagated() {
        // A depth-0 root span containing a depth-1 nested span: verify that
        // TimerSpan.depth reflects the nesting level of each span independently.
        let lines = vec![
            l1(0.0, 0, "ls: start"),
            l1(0.1, 1, "scan: start"),
            l1(0.5, 1, "scan: end (400ms)"),
            l1(1.0, 0, "ls: end (1000ms)"),
        ];
        let spans = collect_spans(&lines);
        assert_eq!(spans.len(), 2);
        // Inner span (depth 1) is completed first
        let scan_span = spans.iter().find(|s| s.label == "scan").expect("scan span");
        assert_eq!(scan_span.depth, 1);
        let ls_span = spans.iter().find(|s| s.label == "ls").expect("ls span");
        assert_eq!(ls_span.depth, 0);
    }

    // -- parse_layer_filter --------------------------------------------------

    #[test]
    fn parse_layer_filter_short() {
        assert_eq!(parse_layer_filter("L1"), Some(Layer::L1));
        assert_eq!(parse_layer_filter("l4"), Some(Layer::L4));
        assert_eq!(parse_layer_filter("L7"), Some(Layer::L7));
    }

    #[test]
    fn parse_layer_filter_invalid() {
        assert_eq!(parse_layer_filter("L8"), None);
        assert_eq!(parse_layer_filter("foo"), None);
    }

    #[test]
    fn parse_rejects_non_finite_elapsed() {
        for elapsed in ["NaN", "inf", "-inf"] {
            let line = format!("[omemfs L1 cmd ] {elapsed}s timer: start");
            assert!(parse_line(&line).is_none(), "{elapsed} must be rejected");
        }
    }
}
