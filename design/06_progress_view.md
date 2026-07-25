# Progress View and Debug Logging

This document describes the progress display and debug logging system used during
long-running commands such as `push`, `pull`, and `clone`.

## Overview

Two output channels are always active:

```
Log file:   always written to .omemfs/logs/
TTY view:   stdout progress window, only when stdout is a TTY
```

`stderr` is reserved for fatal errors and warnings that the user must see immediately.

## Log file

### Path and filename

```
.omemfs/logs/YYYYMMDD-HHMMSS-{cmd}.log   ← timestamp + command name
.omemfs/logs/latest.log                  ← symlink to the most recent log
```

The `latest.log` symlink is rewritten on every command invocation. On systems
where symlinks are unavailable (e.g. some Windows configurations), a copy is
written instead.

### Before the repository is located

`dlog` calls may be emitted before the repository root is known. In that case a
temporary file is created under `std::env::temp_dir()` and moved to
`.omemfs/logs/` once the repository is located. If the command exits without
finding a repository, the temporary file is discarded.

### Retention

`.omemfs/logs/` retains at most the newest **200** log files (`latest.log`
excluded from the count). Each time a log file is relocated into
`.omemfs/logs/` (i.e. once per command invocation whose repository is
located), the directory is swept and the oldest files beyond the limit are
deleted. Filenames sort chronologically by construction
(`YYYYMMDD-HHMMSS-{cmd}.log`), so the sweep is a plain lexicographic sort with
no need to stat mtimes. The sweep is best-effort: any I/O error is silently
ignored, since housekeeping must never fail the command that triggered it.

### Format

```
[omemfs L1 cmd ]    +0.001s push: start
[omemfs L7 sto ]    +1.234s   s3 put_object(delta_index): end (1230ms)
```

Each line contains:
- A fixed 7-character layer tag (e.g. `L1 cmd `)
- Elapsed seconds from process start: right-aligned in a 9-character field,
  signed, 3 decimal places (`{:+9.3}s`), e.g. `   +0.001s`
- Indentation of 2 spaces per nesting depth
- A free-form message

Log file parsers must treat the elapsed field as right-aligned with fixed
format `{:+9.3}s` (always a sign character, always 3 decimal places).

ANSI colour codes are not written to the log file.

### Write mechanism

`LogFile` queues lines on an `mpsc` channel and flushes them via a dedicated
background writer thread. All `dlog` calls on the hot path are non-blocking.
The log file receives every event without loss.

## TTY progress view

### TTY detection

`atty::is(atty::Stream::Stdout)` determines whether the TTY view is active.
When stdout is a pipe, redirect, or CI environment, the TTY view is disabled
entirely.

### Phase-tree model

The terminal displays the current state as a tree of named phases.  Phases
are registered at the moment they start; the display tracks their state and
per-phase detail lines.

```
✓ Scan working tree  (234 files, 0.3s)
→ Upload objects
    ...
    upload a1b2c3d4 (89 uploaded, 12 cached)
    upload e5f6a7b8 (90 uploaded, 12 cached)
✓ Finalize remote  (ok)
```

*Not yet implemented:* up-front registration of all phases before any starts,
with a `Pending` (`○`) display for not-yet-started phases, is a possible
future enhancement.

**Phase states:**

| Symbol | State   | Meaning                        |
|--------|---------|--------------------------------|
| `→`    | Running | Currently executing            |
| `✓`    | Done    | Completed successfully         |
| `✗`    | Failed  | Terminated with an error       |

**Detail lines (per Running phase):**

Each running phase shows a scrolling tail of its most recent N detail lines
directly below the phase row.  Older lines are replaced by a single `...` row:

```
→ Compare changes
    ...
    modified:  src/codec/compress.rs
    modified:  src/codec/encrypt.rs     ← most recent
```

The detail line budget per phase is `MAX_DETAIL_LINES` (default: 8).
Completed phases collapse to a single summary line; their detail lines are
discarded.

*Not yet implemented:* in-progress count and progress bar display (e.g.
`[=====>        ] 8/16 blobs`) within a running phase row are a possible
future enhancement.

**Multiple concurrent phases (future parallel support):**

When multiple phases are in the `Running` state simultaneously, each
phase has its own independent detail area:

```
→ Upload (worker A)
    upload a1b2... (5 uploaded, 3 cached)
→ Upload (worker B)
    upload e5f6... (2 uploaded, 1 cached)
```

### Phase API

Commands interact with the phase display through `PhaseHandle` objects obtained
from `progress::begin_phase`.  The handle is cheaply clonable and safe to move
to another thread.

```rust
// In command code (push example)
let phase = progress::begin_phase("Scan working tree");
phase.detail("scanning: src/codec/compress.rs");
phase.complete("234 files");

let phase = progress::begin_phase("Upload objects");
phase.detail(format!("upload {} ({} uploaded, {} cached)", hash_prefix, uploaded, cached));
phase.complete("done");

let phase = progress::begin_phase("Finalize remote");
// ... write INDEX_ROOT ...
phase.complete("ok");
```

`PhaseHandle` communicates updates to the display over a bounded `mpsc` channel.
All methods are safe to call from any thread.

When TTY mode is disabled, `begin_phase` returns a no-op handle — calls
succeed but produce no output.

**`PhaseHandle` methods:**

| Method | Description |
|--------|-------------|
| `detail(line)` | Append a detail line for this phase (best-effort; dropped if channel is full) |
| `complete(summary)` | Mark phase Done with a one-line summary; consumes the handle (guaranteed delivery) |
| `fail(summary)` | Mark phase Failed; consumes the handle (guaranteed delivery) |

**Delivery guarantees:**

`detail()` uses a non-blocking `try_send`: if the channel is full, the update
is silently dropped. This is intentional — the view only shows the most recent
`MAX_DETAIL_LINES` lines per phase anyway, so dropped intermediate lines have
no visible effect. The log file (unbounded channel) always receives every event
without loss, so `omemfs log timers` is unaffected.

`complete()` and `fail()` use a blocking `send` to guarantee that the terminal
always shows the final phase state correctly.

Dropping a `PhaseHandle` without calling `complete` or `fail` leaves the phase
in the `Running` state until the display is torn down.

### Total row count

```
total rows = phase count + Σ detail_line_count(running phases)
```

For sequential commands this reduces to `phase count + detail_lines_of_current_phase`.

### Refresh mechanism

A `RefreshThread` redraws the view at a fixed 100 ms interval.  Updates from
`PhaseHandle` are delivered over an `mpsc` channel and drained by the refresh
thread before each redraw.  No I/O occurs on the calling thread.

The channel capacity is 256 entries. `detail()` calls that arrive when the
channel is full are silently dropped (best-effort). At a redraw rate of 100 ms
and `MAX_DETAIL_LINES` = 8 per phase, this is never observable in practice.

### Cursor operations

Each redraw:
1. `ESC[{N}A` — move up N lines (previous total row height)
2. `ESC[J` — erase from cursor to end of screen
3. Print new view content

ANSI colour is applied in TTY mode and respects `NO_COLOR` / `CLICOLOR_FORCE`.

### Line width and wrapping

Step 1 above requires `N` to equal the number of *physical* terminal rows the
previously printed view occupied. A logical line wider than the terminal wraps
onto multiple physical rows, so the view must predict, for each logical line,
how many physical rows it will occupy:

```
physical_rows(line) = ceil(display_width(line) / terminal_columns)
```

`display_width(line)` is the number of terminal cells the line occupies, **not**
its number of `char`s. The two differ for:

- **East Asian Wide / Fullwidth** characters (e.g. CJK ideographs in file
  paths), which occupy 2 cells each.
- **East Asian Ambiguous** characters, including the phase status symbols `○`
  (U+25CB) and `→` (U+2192), which render as 2 cells in CJK/East-Asian terminal
  locales and 1 cell otherwise.

Display width is computed with the `unicode-width` crate. Because omemfs is used
primarily in a Japanese terminal environment, Ambiguous-width characters are
counted as **2 cells** (`UnicodeWidthStr::width_cjk`). ANSI colour escape
sequences contribute 0 cells and are stripped before measuring.

Note: not every status symbol is Ambiguous. Per the Unicode East Asian Width
data, `✓` (U+2713) and `✗` (U+2717) are **Neutral** and count as 1 cell even
under `width_cjk`. The view trusts the crate's per-character classification
rather than assuming a uniform symbol width.

Mispredicting `display_width` (e.g. counting `char`s) makes `N` too small, so a
redraw moves the cursor up too few rows and leaves stale wrapped rows on screen,
corrupting the display. This is most visible when a detail line is long enough
to wrap.

### Error exit behavior

On successful command exit, `ProgressContext::finish` performs a final redraw
and leaves the phase list on screen, showing the complete record of what was
done.

On error exit (the command returned `Err`), the entire progress area is erased
before the error message is printed to stderr, so no stale phase rows or detail
lines remain above the error. This applies uniformly to all subcommands that
use `begin_phase`.

The command result's error status is passed from `main.rs` into
`ProgressContext::finish`, which forwards it to `PhaseView::finish`. In TTY
mode, when `errored == true`, the view emits `ESC[{N}A ESC[J` (move up N rows
and clear to end of screen) to erase the progress area before returning control
to the caller.

## Command output — two output modes

There are two ways for a command to produce user-visible output.  Choose based
on whether the output should appear in real time or only after the command
finishes.

### Buffered output (final results)

Commands that produce a result at the end (e.g. `ls`, `push`, `pull`, `stats`)
accumulate their results in an `Output` buffer rather than writing directly to
stdout.

```rust
let mut out = Output::for_stdout();
out.writeln("result line")?;
out.finish()?;   // deposits buffer into ProgressContext
```

`Output::finish()` deposits the buffer into `ProgressContext`.  At command exit,
`ProgressContext::finish()` performs a final redraw of the phase list (leaving it
visible on screen) and then writes the buffered output below it, guaranteeing
the order:

```
phase list stays on screen → command output printed below it
```

`Output::for_stdout()` automatically detects TTY/non-TTY and selects buffered or
direct mode.  **Do not use `println!` directly in commands that have a
`dtimer_l1!`** — the direct write races with the TTY redraw and can be erased.

### Inline streaming output (per-item progress)

Commands that process items one by one and should show progress in real time
(e.g. `restore`, `expand`) use `progress::emit_output_line`.

```rust
crate::progress::emit_output_line("  restored: path/to/file");
```

In TTY mode this temporarily clears the progress area, writes the line, then
immediately redraws the progress window below it — without waiting for the next
100 ms tick.  The output accumulates above the progress window as the command
runs.

In non-TTY mode it falls back to a direct write to stdout.

### Raw streamed output (binary / large payloads)

Commands that stream a raw, possibly very large payload straight to stdout —
most notably `cat` writing blob (file) content — must not buffer it in memory,
so they cannot use the `Output` buffer.  Writing directly to stdout would,
however, race with the periodic redraw, which could erase the bytes already
written.

For this case, seal the phase view *before* streaming:

```rust
phase.complete("blob");            // mark the phase done (row stays on screen)
crate::progress::freeze_phase_view();
let mut out = Output::raw_stdout(); // unbuffered, direct-to-stdout sink
let w = out.writer();
for_each_blob_chunk(&store, &hash, None, |chunk| w.write_all(chunk))?;
out.finish()?;
```

`progress::freeze_phase_view()` stops the refresh thread, performs one final
redraw of the completed phase list, then *seals* the view: every subsequent
phase-view method (`refresh`, `redraw`, `finish`, `emit_inline`) becomes a
no-op.  This guarantees the order:

```
completed phase list stays on screen → raw bytes streamed below it
```

After sealing, `ProgressContext::finish()` skips the phase-view redraw (it is
already sealed) but still flushes the log file and any deposited command
output.  `Output::raw_stdout()` provides an unbuffered, non-colored sink whose
`writer()` is used to stream the chunks.

### Summary

| Use case | API | TTY behaviour |
|---|---|---|
| Final result / structured output | `Output::for_stdout()` + `writeln` | Buffered; shown below the phase list after it is finalized |
| Per-item streaming progress | `progress::emit_output_line` | Inline; interleaved above progress window |
| Raw binary / large stream | `freeze_phase_view()` + `Output::raw_stdout()` | Phase list sealed on screen; bytes streamed directly below it |
| Debug / timing | `dlog_l1!` / `dtimer_l1!` | Shown inside the scrolling progress window |

## Architecture layers

Each `dlog` / `dtimer` call is tagged with a layer that identifies which layer
of the codec pipeline the message originates from. Seven layers are defined in [`02_storage_format.md`](02_storage_format.md):

| Layer | Tag       | Modules                           | Codec layer                    |
|-------|-----------|-----------------------------------|--------------------------------|
| L1    | `L1 cmd ` | `commands/`, `scan.rs`, `tree_ops.rs` | command logic, working-tree scan (outside codec pipeline) |
| L2    | `L2 ser ` | `object.rs`                       | [L2 ser: serialize / deserialize](02_storage_format.md#l2-ser-serialize--deserialize) |
| L3    | `L3 chk ` | `codec/chunk.rs`                  | [L3 chk: chunk / assemble](02_storage_format.md#l3-chk-chunk--assemble) |
| L4    | `L4 cmp ` | `codec/compress.rs`               | [L4 cmp: compress / decompress](02_storage_format.md#l4-cmp-compress--decompress) |
| L5    | `L5 enc ` | `codec/encrypt.rs`                | [L5 enc: encrypt / decrypt](02_storage_format.md#l5-enc-encrypt--decrypt) |
| L6    | `L6 pak ` | `codec/pack/`                     | [L6 pak: pack / unpack](02_storage_format.md#l6-pak-pack--unpack) |
| L7    | `L7 sto ` | `store/`                          | [L7 sto: store / load](02_storage_format.md#l7-sto-store--load) |

ANSI colour codes for each layer (TTY mode only):

```
L1 cmd  → 32 (green)    command logic
L2 ser  → 36 (cyan)     serialize
L3 chk  → 33 (yellow)   chunk
L4 cmp  → 35 (magenta)  compress
L5 enc  → 31 (red)      encrypt
L6 pak  → 34 (blue)     pack
L7 sto  → 37 (white)    store I/O
```

## Macros

```rust
dlog_l1!("message {}", value);   // emit a Layer 1 log line
dtimer_l1!("label");             // RAII timer: emits "label: start" and "label: end (Xms)"

dlog_l2!(...);  dtimer_l2!(...);
dlog_l3!(...);  dtimer_l3!(...);
dlog_l4!(...);  dtimer_l4!(...);
dlog_l5!(...);  dtimer_l5!(...);
dlog_l6!(...);  dtimer_l6!(...);
dlog_l7!(...);  dtimer_l7!(...);
```

`DebugTimer` (returned by `dtimer_lN!`) manages a thread-local `DEPTH` counter
so that nested calls produce correctly indented log lines. The counter is
incremented on construction and decremented on drop.

## `ls` phase timers

The previously-unmeasured `ls` phases are wrapped in L1 RAII timers so that
`omemfs log timers` reports their Total / Avg / Max in the per-label table:

| Label                | Phase measured                                              |
|----------------------|-------------------------------------------------------------|
| `stat_cache read`    | load + parse `STAT_CACHE` (`StatCache::read`)               |
| `stat_cache write`   | encode + atomic write `STAT_CACHE` (`StatCache::write`)     |
| `scan`               | working-tree walk + hash + tree build (`scan_and_store_with_cache`) |
| `flatten_tree`       | flatten a tree into a path→entry map (`flatten_tree_entries`) |
| `diff_trees`         | diff base vs target tree (`diff_trees`)                     |
| `collect_tree_rows`  | build listing rows from a tree (`collect_tree_rows`)        |
| `build_working_flat` | build the flat working-tree entry map (`build_working_flat`)|

These phase timers are themselves L1 and nest inside the root command span
(also L1). To avoid double-counting wall clock, the Layer breakdown counts only
depth-0 (root command) spans toward the L1 total; the per-label table still
lists every nested phase. Sub-layer totals (L2–L7) continue to count spans at
all depths.

## Initialisation

```rust
// main.rs
let progress_ctx = Arc::new(ProgressContext::new(command_name, is_tty)?);
progress::set_context(Arc::clone(&progress_ctx));

// After the repository root is known:
progress_ctx.set_repo_dir(repo.root_path());

// At command exit:
progress_ctx.finish();
progress::clear_context();
```

## Module layout

```
src/progress/
  mod.rs          ProgressContext + emit / emit_output_line / set_context / deposit_output
                  + begin_phase() free function
  log_file.rs     LogFile (background writer thread)
  phase_view.rs   PhaseView (phase-tree TTY display) + PhaseHandle + PhaseUpdate
src/debug.rs      Layer enum + DebugTimer + dlog_lN! / dtimer_lN! macros
src/term/
  output.rs       Output (buffered stdout wrapper; for_stdout() selects TTY/non-TTY mode)
```

## Dependencies

```toml
atty = "0.2"   # TTY detection
```

`crossterm` is not required. Cursor movement and screen clearing are performed
with raw ANSI escape sequences.
