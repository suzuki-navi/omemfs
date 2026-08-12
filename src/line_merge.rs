//! Three-way, line-level merge for line-based, append-only log files (e.g.
//! `.zsh_history`) matched by a `.omemfs-filter` `[line_merge]` section (see
//! `design/15_line_history_merge.md`).
//!
//! This module implements the merge algorithm only. Wiring it into `pull`
//! (deciding which paths to try it on, and what to do with the result) is a
//! separate concern handled by the caller (`src/commands/pull.rs`, see
//! design/15_line_history_merge.md, "`pull` integration point").
//!
//! Inputs and outputs are raw bytes, never `String`, so a line containing
//! non-UTF-8 bytes passes through unchanged. `merge` never panics and never
//! returns an `Err`: any input, however malformed, produces a `MergeOutcome`.

use std::ops::Range;

use similar::{Algorithm, DiffOp, capture_diff_slices};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Result of a three-way line merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    /// The merge succeeded; the merged file content.
    Clean(Vec<u8>),
    /// Local and remote made overlapping, independent edits at the same base
    /// gap that this line-level algorithm cannot reconcile. The caller
    /// should fall back to the existing conflict-helper mechanism
    /// (`design/03_sync_model.md`, "Conflict handling").
    Conflict,
}

/// Three-way merge `local` and `remote` against their common `base`.
///
/// See `design/15_line_history_merge.md` ("Merge algorithm") for the full
/// description of Policy A (deletion), the conflict-fallback trigger, and
/// the two ordering modes.
pub fn merge(base: &[u8], local: &[u8], remote: &[u8]) -> MergeOutcome {
    let base_lines = split_lines(base);
    let local_lines = split_lines(local);
    let remote_lines = split_lines(remote);

    let local_ops = capture_diff_slices(Algorithm::Myers, &base_lines, &local_lines);
    let remote_ops = capture_diff_slices(Algorithm::Myers, &base_lines, &remote_lines);

    let local_segments = aggregate_segments(&local_ops, &local_lines);
    let remote_segments = aggregate_segments(&remote_ops, &remote_lines);

    if has_conflict(&local_segments, &remote_segments) {
        return MergeOutcome::Conflict;
    }

    let kept_base_lines = policy_a_survivors(&base_lines, &local_segments, &remote_segments);
    let local_new_lines = changed_other_lines(&local_segments);
    let remote_new_lines = changed_other_lines(&remote_segments);

    let has_timestamp = base_lines
        .iter()
        .chain(local_lines.iter())
        .chain(remote_lines.iter())
        .any(|line| parse_extended_history_timestamp(line).is_some());

    let mut merged: Vec<&[u8]> =
        Vec::with_capacity(kept_base_lines.len() + local_new_lines.len() + remote_new_lines.len());
    merged.extend(kept_base_lines);
    merged.extend(local_new_lines);
    merged.extend(remote_new_lines);

    if has_timestamp {
        // Stable sort: ties (including lines with no timestamp, which all
        // sort as i64::MIN) preserve the relative order established above.
        merged.sort_by_key(|line| parse_extended_history_timestamp(line).unwrap_or(i64::MIN));
    }

    MergeOutcome::Clean(join_lines(&merged))
}

// ---------------------------------------------------------------------------
// Line splitting / joining
// ---------------------------------------------------------------------------

/// Split `content` into lines on `b'\n'`. A trailing newline does not produce
/// a trailing empty line (the terminator is not itself a line); an empty
/// buffer splits to zero lines.
fn split_lines(content: &[u8]) -> Vec<&[u8]> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&[u8]> = content.split(|&b| b == b'\n').collect();
    if content.ends_with(b"\n") {
        lines.pop();
    }
    lines
}

/// Join `lines` with `b'\n'`, appending a trailing `b'\n'` iff `lines` is
/// non-empty. Exact original trailing-newline presence is not preserved
/// bit-for-bit; this is a merge, not a copy.
fn join_lines(lines: &[&[u8]]) -> Vec<u8> {
    let mut out = lines.join(&b"\n"[..]);
    if !lines.is_empty() {
        out.push(b'\n');
    }
    out
}

// ---------------------------------------------------------------------------
// Segment aggregation
// ---------------------------------------------------------------------------

/// One aggregated segment of a base-vs-other diff.
#[derive(Debug, Clone)]
enum Segment<'a> {
    /// A run of base line indices that survived unchanged on this side
    /// (from an `Equal` op).
    Keep(Range<usize>),
    /// A maximal run of consecutive non-`Equal` ops between two `Equal` ops
    /// (or between a boundary and an `Equal` op), collapsed into one entry
    /// regardless of whether `similar` produced a single `Replace` or a
    /// `Delete` + `Insert` pair for the same edit.
    Change(ChangeSegment<'a>),
}

#[derive(Debug, Clone)]
struct ChangeSegment<'a> {
    /// Union of the base index spans covered by the ops in this run. A
    /// pure-insert run has a zero-length range at the insertion point
    /// rather than a dropped base range.
    base_range: Range<usize>,
    /// The other side's lines that appear in this gap, in file order.
    other_lines: Vec<&'a [u8]>,
}

impl<'a> Segment<'a> {
    fn as_change(&self) -> Option<&ChangeSegment<'a>> {
        match self {
            Segment::Change(c) => Some(c),
            Segment::Keep(_) => None,
        }
    }
}

/// Aggregate raw `similar::DiffOp`s (from a base-vs-`other_lines` diff) into
/// `Keep` / `Change` segments. See the module doc and `design/15` for the
/// aggregation rule.
fn aggregate_segments<'a>(ops: &[DiffOp], other_lines: &[&'a [u8]]) -> Vec<Segment<'a>> {
    let mut segments = Vec::new();
    let mut pending: Option<ChangeSegment<'a>> = None;

    fn flush<'a>(pending: &mut Option<ChangeSegment<'a>>, segments: &mut Vec<Segment<'a>>) {
        if let Some(change) = pending.take() {
            segments.push(Segment::Change(change));
        }
    }

    fn extend_range(pending: &mut Option<ChangeSegment<'_>>, start: usize, end: usize) {
        match pending {
            Some(change) => {
                change.base_range.start = change.base_range.start.min(start);
                change.base_range.end = change.base_range.end.max(end);
            }
            None => {
                *pending = Some(ChangeSegment {
                    base_range: start..end,
                    other_lines: Vec::new(),
                });
            }
        }
    }

    for op in ops {
        match *op {
            DiffOp::Equal {
                old_index, len, ..
            } => {
                flush(&mut pending, &mut segments);
                segments.push(Segment::Keep(old_index..old_index + len));
            }
            DiffOp::Delete {
                old_index, old_len, ..
            } => {
                extend_range(&mut pending, old_index, old_index + old_len);
            }
            DiffOp::Insert {
                old_index,
                new_index,
                new_len,
            } => {
                extend_range(&mut pending, old_index, old_index);
                pending
                    .as_mut()
                    .unwrap()
                    .other_lines
                    .extend_from_slice(&other_lines[new_index..new_index + new_len]);
            }
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                extend_range(&mut pending, old_index, old_index + old_len);
                pending
                    .as_mut()
                    .unwrap()
                    .other_lines
                    .extend_from_slice(&other_lines[new_index..new_index + new_len]);
            }
        }
    }
    flush(&mut pending, &mut segments);
    segments
}

/// True if `a` and `b`, as integer ranges, share at least one index. Two
/// zero-length ranges never overlap under this definition — this is what
/// keeps two independent, coincident pure appends from being mistaken for an
/// overlapping edit (see "Conflict-fallback trigger" in `design/15`).
fn ranges_overlap(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start.max(b.start) < a.end.min(b.end)
}

/// Whole-file conflict check: is there a `Change` segment on each side, both
/// carrying replacement content, whose base ranges overlap?
fn has_conflict(local_segments: &[Segment], remote_segments: &[Segment]) -> bool {
    let local_changes = local_segments
        .iter()
        .filter_map(Segment::as_change)
        .filter(|c| !c.other_lines.is_empty());
    let remote_changes: Vec<&ChangeSegment> = remote_segments
        .iter()
        .filter_map(Segment::as_change)
        .filter(|c| !c.other_lines.is_empty())
        .collect();

    local_changes.into_iter().any(|l| {
        remote_changes
            .iter()
            .any(|r| ranges_overlap(&l.base_range, &r.base_range))
    })
}

/// Policy A: a base line survives iff it is kept (via an `Equal`-derived
/// `Keep` segment) by local, by remote, or both.
fn policy_a_survivors<'a>(
    base_lines: &[&'a [u8]],
    local_segments: &[Segment],
    remote_segments: &[Segment],
) -> Vec<&'a [u8]> {
    let mut kept = vec![false; base_lines.len()];
    for segments in [local_segments, remote_segments] {
        for segment in segments {
            if let Segment::Keep(range) = segment {
                for i in range.clone() {
                    kept[i] = true;
                }
            }
        }
    }
    base_lines
        .iter()
        .enumerate()
        .filter(|(i, _)| kept[*i])
        .map(|(_, line)| *line)
        .collect()
}

/// Collect every `Change` segment's `other_lines`, concatenated in segment
/// order (equivalently, that side's file order).
fn changed_other_lines<'a>(segments: &[Segment<'a>]) -> Vec<&'a [u8]> {
    segments
        .iter()
        .filter_map(Segment::as_change)
        .flat_map(|c| c.other_lines.iter().copied())
        .collect()
}

// ---------------------------------------------------------------------------
// zsh extended_history timestamp parsing
// ---------------------------------------------------------------------------

/// Parse a zsh `extended_history` leading timestamp: `: <epoch>:<duration>;
/// <command>` — a line starting with `": "`, followed by decimal digits,
/// `":"`, more decimal digits, then `";"`. Returns the epoch on success.
///
/// Only ever decodes the short ASCII prefix as text; any malformed or
/// non-matching input (including invalid UTF-8 anywhere in the line) yields
/// `None` rather than panicking.
fn parse_extended_history_timestamp(line: &[u8]) -> Option<i64> {
    let rest = line.strip_prefix(b": ")?;

    let colon = rest.iter().position(|&b| b == b':')?;
    let epoch_bytes = &rest[..colon];
    if epoch_bytes.is_empty() || !epoch_bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }

    let after_colon = &rest[colon + 1..];
    let semicolon = after_colon.iter().position(|&b| b == b';')?;
    let duration_bytes = &after_colon[..semicolon];
    if duration_bytes.is_empty() || !duration_bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }

    std::str::from_utf8(epoch_bytes).ok()?.parse::<i64>().ok()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(strs: &[&str]) -> Vec<u8> {
        // Join with `\n`, no trailing newline, to mirror a file that has
        // just been appended to (no trailing blank line).
        strs.join("\n").into_bytes()
    }

    fn clean(outcome: MergeOutcome) -> Vec<u8> {
        match outcome {
            MergeOutcome::Clean(bytes) => bytes,
            MergeOutcome::Conflict => panic!("expected Clean, got Conflict"),
        }
    }

    fn merged_lines(outcome: MergeOutcome) -> Vec<String> {
        let bytes = clean(outcome);
        if bytes.is_empty() {
            return Vec::new();
        }
        String::from_utf8(bytes)
            .unwrap()
            .split('\n')
            .filter(|s| !s.is_empty() || false)
            .map(|s| s.to_string())
            .collect()
    }

    // --- 1. Pure local append ---

    #[test]
    fn pure_local_append() {
        let base = lines(&["a", "b"]);
        let local = lines(&["a", "b", "c"]);
        let remote = lines(&["a", "b"]);
        assert_eq!(merged_lines(merge(&base, &local, &remote)), vec!["a", "b", "c"]);
    }

    // --- 2. Pure remote append (symmetric) ---

    #[test]
    fn pure_remote_append() {
        let base = lines(&["a", "b"]);
        let local = lines(&["a", "b"]);
        let remote = lines(&["a", "b", "c"]);
        assert_eq!(merged_lines(merge(&base, &local, &remote)), vec!["a", "b", "c"]);
    }

    // --- 3. Both append, non-overlapping ---

    #[test]
    fn both_append_non_overlapping_fallback_order() {
        let base = lines(&["a"]);
        let local = lines(&["a", "x"]);
        let remote = lines(&["a", "y"]);
        let result = merged_lines(merge(&base, &local, &remote));
        // Fallback order (no timestamps): base, then local-only, then remote-only.
        assert_eq!(result, vec!["a", "x", "y"]);
    }

    // --- 4. Front-truncation tolerance ---

    #[test]
    fn front_truncation_tolerance_nothing_lost() {
        let base = lines(&["1", "2", "3"]);
        // Local: front-truncated (drops "1") and appends "x".
        let local = lines(&["2", "3", "x"]);
        // Remote: untouched and appends "y".
        let remote = lines(&["1", "2", "3", "y"]);
        let result = merged_lines(merge(&base, &local, &remote));
        for expected in ["1", "2", "3", "x", "y"] {
            assert!(
                result.contains(&expected.to_string()),
                "expected {:?} in merged result {:?}",
                expected,
                result
            );
        }
        assert_eq!(result.len(), 5, "unexpected merged result: {:?}", result);
    }

    // --- 5. Front-truncation on both sides, no replacement content ---

    #[test]
    fn front_truncation_both_sides_no_replacement_drops_line_cleanly() {
        // Both sides trim away "1" from the front with nothing added at that
        // gap. Neither side has replacement content there, so this is a
        // clean drop, not a conflict.
        let base = lines(&["1", "2", "3"]);
        let local = lines(&["2", "3"]);
        let remote = lines(&["2", "3"]);
        let outcome = merge(&base, &local, &remote);
        // merge() always appends a trailing newline for a non-empty result
        // (see "Output encoding" in design/15), so the expected bytes need
        // one even though the `lines()` test helper does not add one.
        let mut expected = lines(&["2", "3"]);
        expected.push(b'\n');
        assert_eq!(outcome, MergeOutcome::Clean(expected));
    }

    // --- 6. Duplicate lines are not de-duplicated ---

    #[test]
    fn duplicate_lines_survive_uncollapsed() {
        let base = lines(&["a"]);
        let local = lines(&["a", "a"]);
        let remote = lines(&["a"]);
        let result = merged_lines(merge(&base, &local, &remote));
        assert_eq!(result, vec!["a", "a"]);
    }

    // --- 7. zsh extended_history timestamp ordering ---

    #[test]
    fn timestamped_lines_are_fully_sorted() {
        // Base holds t=100 and t=400. Local adds t=200 (interleaved) and
        // t=500 (append). Remote adds t=300 (interleaved) and t=600
        // (append). Neither side drops anything, so nothing is dropped by
        // Policy A and this is a clean merge.
        let base = lines(&[": 100:0;cmd_100", ": 400:0;cmd_400"]);
        let local = lines(&[
            ": 100:0;cmd_100",
            ": 200:0;cmd_200",
            ": 400:0;cmd_400",
            ": 500:0;cmd_500",
        ]);
        let remote = lines(&[
            ": 100:0;cmd_100",
            ": 300:0;cmd_300",
            ": 400:0;cmd_400",
            ": 600:0;cmd_600",
        ]);
        let result = merged_lines(merge(&base, &local, &remote));
        let expected = vec![
            ": 100:0;cmd_100".to_string(),
            ": 200:0;cmd_200".to_string(),
            ": 300:0;cmd_300".to_string(),
            ": 400:0;cmd_400".to_string(),
            ": 500:0;cmd_500".to_string(),
            ": 600:0;cmd_600".to_string(),
        ];
        assert_eq!(result, expected);
    }

    // --- 8. No-timestamp fallback ordering (exact order) ---

    #[test]
    fn no_timestamp_fallback_exact_order() {
        let base = lines(&["base1", "base2"]);
        let local = lines(&["base1", "base2", "local_new1", "local_new2"]);
        let remote = lines(&["base1", "base2", "remote_new1", "remote_new2"]);
        let result = merged_lines(merge(&base, &local, &remote));
        assert_eq!(
            result,
            vec![
                "base1",
                "base2",
                "local_new1",
                "local_new2",
                "remote_new1",
                "remote_new2",
            ]
        );
    }

    // --- 9. Genuine overlapping edit -> Conflict ---

    #[test]
    fn overlapping_edit_to_same_line_is_conflict() {
        let base = lines(&["a"]);
        let local = lines(&["local_edit"]);
        let remote = lines(&["remote_edit"]);
        assert_eq!(merge(&base, &local, &remote), MergeOutcome::Conflict);
    }

    // --- 10. One-sided rewrite is NOT a conflict ---

    #[test]
    fn one_sided_rewrite_is_not_a_conflict() {
        let base = lines(&["a"]);
        let local = lines(&["local_edit"]);
        let remote = lines(&["a"]); // untouched
        let result = merged_lines(merge(&base, &local, &remote));
        assert!(result.contains(&"a".to_string()), "result: {:?}", result);
        assert!(
            result.contains(&"local_edit".to_string()),
            "result: {:?}",
            result
        );
    }

    // --- 11. Empty base, both sides add fresh content ---

    #[test]
    fn empty_base_both_sides_added_fresh_content() {
        let base: Vec<u8> = Vec::new();
        let local = lines(&["local1", "local2"]);
        let remote = lines(&["remote1"]);
        let result = merged_lines(merge(&base, &local, &remote));
        assert_eq!(result.len(), 3);
        for expected in ["local1", "local2", "remote1"] {
            assert!(result.contains(&expected.to_string()));
        }
    }

    // --- 12. Empty local / empty remote edge cases ---

    #[test]
    fn empty_local_survives_via_remote_keep() {
        // Local deleted everything; remote kept the base and appended.
        let base = lines(&["a", "b"]);
        let local: Vec<u8> = Vec::new();
        let remote = lines(&["a", "b", "c"]);
        let result = merged_lines(merge(&base, &local, &remote));
        assert_eq!(result, vec!["a", "b", "c"]);
    }

    #[test]
    fn empty_remote_survives_via_local_keep() {
        let base = lines(&["a", "b"]);
        let local = lines(&["a", "b", "c"]);
        let remote: Vec<u8> = Vec::new();
        let result = merged_lines(merge(&base, &local, &remote));
        assert_eq!(result, vec!["a", "b", "c"]);
    }

    #[test]
    fn both_local_and_remote_empty_drops_base_cleanly() {
        // Both sides deleted everything, with nothing added: this is a
        // clean drop (no replacement content anywhere), not a conflict.
        let base = lines(&["a", "b"]);
        let local: Vec<u8> = Vec::new();
        let remote: Vec<u8> = Vec::new();
        assert_eq!(merge(&base, &local, &remote), MergeOutcome::Clean(Vec::new()));
    }

    // --- 13. Invalid UTF-8 bytes survive unchanged ---

    #[test]
    fn invalid_utf8_line_survives_unchanged() {
        let mut bad_line = b"cmd_".to_vec();
        bad_line.push(0xFF); // invalid UTF-8 continuation byte in isolation
        bad_line.extend_from_slice(b"_tail");

        let mut base = b"a\n".to_vec();
        base.extend_from_slice(b"b");

        let mut local = b"a\n".to_vec();
        local.extend_from_slice(&bad_line);
        local.push(b'\n');
        local.extend_from_slice(b"b");

        let remote = base.clone();

        let outcome = merge(&base, &local, &remote);
        let bytes = clean(outcome);
        // The exact bad-line byte sequence must appear somewhere in the output.
        assert!(
            bytes
                .windows(bad_line.len())
                .any(|w| w == bad_line.as_slice()),
            "invalid-UTF-8 line not found intact in merged output: {:?}",
            bytes
        );
    }
}
