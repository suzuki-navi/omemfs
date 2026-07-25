# Log Analysis Commands

This document describes the `omemfs log` subcommand group, which provides
developer-facing tools for analysing the log files written to `.omemfs/logs/`.

See `06_progress_view.md` for the log file format.

## Overview

```
omemfs log ls     [OPTIONS]         — list log files in the repository
omemfs log show   [OPTIONS] [REF]   — filter and display log lines
omemfs log timers [OPTIONS] [REF]   — aggregate timer spans and show statistics
```

When `REF` is omitted, `.omemfs/logs/latest.log` in the nearest repository root
is used. If no repository root is found, the command errors.

`omemfs log` commands do **not** write a log file for themselves.

---

## Log file naming

Log files are stored as:

```
.omemfs/logs/YYYYMMDD-HHMMSS-{cmd}.log
.omemfs/logs/latest.log                 ← symlink to the most recent file
```

The **logical name** of a log file is its filename without the `.log` suffix
(e.g. `20260522-143012-push`). Logical names are used in `log ls` output and
accepted as `REF` arguments.

---

## Log line format (recap)

```
[omemfs L1 cmd ]    +0.001s push: start
[omemfs L7 sto ]    +1.234s   s3 put_object(delta_index): end (1230ms)
```

Fields:
- Layer tag: 7-character fixed-width string (`L1 cmd ` … `L7 sto `)
- Elapsed seconds: right-aligned in a 9-character field, signed, 3 decimal
  places — format `{:+9.3}s`, e.g. `   +0.001s`
- Indentation: 2 spaces per nesting depth level (reflects `DebugTimer` nesting)
- Message: free-form text; timer messages follow `<label>: start` / `<label>: end (<duration>)` conventions

---

## `omemfs log ls`

List log files in the repository, newest first.

```
omemfs log ls [--cmd <name>] [-n <count>]
```

### Options

- `--cmd <name>`: show only logs whose logical name contains the command name
  (e.g. `--cmd push` matches `20260522-143012-push`).
- `-n <count>`: show only the N most recent entries (default: 10).

### Output

```
.omemfs/logs/
  1  20260522-143012-push  2026-05-22 14:30:12  12.3 K  [latest]
  2  20260522-140501-pull  2026-05-22 14:05:01   8.1 K
  3  20260521-233012-push  2026-05-21 23:30:12  45.2 K
```

Columns:
- `#`: position in the newest-first list; used with `@N` syntax
- Logical name: filename without `.log`
- Timestamp: parsed from the filename stem (`YYYYMMDD-HHMMSS`)
- Size: file size in human-readable form
- `[latest]`: present when `latest.log` is a symlink that resolves to this file.
  On filesystems where the copy fallback was used (no symlink support), no
  `[latest]` marker is shown.

---

## REF argument for `log show` and `log timers`

`REF` accepts four forms:

1. **Omitted** — use `latest.log` (walk up the directory tree to find the repo root).
2. **`@N`** — use the N-th entry from `log ls` (1-based, newest first).
3. **Logical name** — a string with no `/`, no `.`, and not starting with `@`;
   resolved to `<repo>/.omemfs/logs/<name>.log`.
4. **File path** — any other string; used as a `PathBuf` directly.

---

## `omemfs log show`

Display log lines with optional filtering and ANSI colour.

```
omemfs log show [--layer <tag>]... [--grep <pattern>] [REF]
```

### Options

- `--layer <tag>`: show only lines whose layer tag matches. `<tag>` is case-insensitive and accepts `L1`–`L7` or the full tag string (e.g. `L4 cmp`). Repeatable; multiple values are OR-ed.
- `--grep <pattern>`: show only lines whose message contains `<pattern>` (case-sensitive substring match).

### Output

Lines that do not parse as omemfs log lines (i.e. `parse_line` returns `None`)
are silently skipped.  Matching lines are printed as-is from the log file.
When stdout is a TTY, each line's `[omemfs Lx xxx ]` prefix is coloured using
the same ANSI codes as the TTY progress view (see `06_progress_view.md`).

`NO_COLOR` and `CLICOLOR_FORCE` are respected.

---

## `omemfs log timers`

Parse timer spans from a log file and print aggregated statistics.

```
omemfs log timers [--sort <key>] [--layer <tag>]... [REF]
```

### Options

- `--sort <key>`: sort order for the per-label table. Valid keys: `total` (default), `avg`, `count`, `max`.
- `--layer <tag>`: restrict output to the specified layer(s). Same format as `log show`.

### Timer span detection

A timer span is a matched pair of log lines:

```
[omemfs Lx xxx ] +T1s{indent}<label>: start
[omemfs Lx xxx ] +T2s{indent}<label>: end (<duration>)
```

Matching rules:
- Both lines must have the same layer tag.
- Both lines must have the same indentation depth.
- The label text (everything before `: start` / `: end (…)`) must be identical.
- Matching is performed with a stack: when a `start` line is seen, its
  `(depth, label, layer)` key and elapsed time are pushed. When a matching
  `end` line is seen, the span is closed. Unmatched `start` or `end` lines
  are silently ignored.
- Duration is taken from the elapsed-time fields (`T2 − T1`), not from the
  `(Xms)` suffix (which may be rounded).

### Per-label statistics table

```
Label                           Layer  Count   Total     Avg       Max
----------------------------------------------------------------------
push                            L1         1   3.456s   3.456s   3.456s
s3 put_object(delta_index)      L7       123  45.600s    371ms    2.100s
compress blob                   L4       456   8.200s     18ms    120ms
serialize tree                  L2        89   1.050s     12ms     45ms
```

Columns:
- `Label`: the timer label (truncated to 40 chars if necessary)
- `Layer`: e.g. `L7`
- `Count`: number of completed spans
- `Total`: sum of all span durations
- `Avg`: arithmetic mean duration
- `Max`: maximum single span duration

Duration values use the same human-readable formatting as `DebugTimer`:
`Xs` for ≥ 1 second, `Xms` for ≥ 1 ms, `Xus` otherwise.

### Layer breakdown summary

Printed below the per-label table:

```
Layer breakdown (by total time):
  L7 sto   45.600s  63.1%
  L4 cmp    8.200s  11.4%
  L1 cmd    3.456s   4.8%
```

The percentage is relative to the L1 (wall clock) layer total.  If no L1
timers are present in the log, the largest layer total is used as the
reference instead.  Layers with zero time are omitted.

---

## Module layout

```
src/log_parser.rs        — LogLine, TimerSpan, parse_line(), collect_spans()
src/commands/log.rs      — log ls / log show / log timers implementation
```

### `log_parser.rs` public API

```rust
pub struct LogLine {
    pub layer: Layer,
    pub elapsed_secs: f64,
    pub depth: usize,
    pub text: String,
}

pub struct TimerSpan {
    pub label: String,
    pub layer: Layer,
    pub start_secs: f64,
    pub end_secs: f64,
}

impl TimerSpan {
    pub fn duration_secs(&self) -> f64;
}

/// Parse a single log line. Returns None for lines that do not match the format.
pub fn parse_line(line: &str) -> Option<LogLine>;

/// Match start/end pairs and return completed spans in encounter order.
pub fn collect_spans(lines: &[LogLine]) -> Vec<TimerSpan>;
```
