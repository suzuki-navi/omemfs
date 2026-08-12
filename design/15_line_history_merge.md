# Line-Based History File Merge

> **Status: Implemented.** `pull` now auto-merges any path matching
> `[line_merge]` that was modified on both local and remote, writing the
> merged content directly to the working tree instead of producing conflict
> helper files. It falls back to the normal conflict-helper-file flow
> (`03_sync_model.md`, "Conflict handling") only when `line_merge::merge`
> returns `Conflict`, or when the path does not meet the preconditions below
> (e.g. a delete on one side, a directory/symlink, or a non-regular-file
> working-tree entry).

## Goal

Avoid conflict-helper churn for line-based, append-only log files — for
example `.zsh_history` — where local and remote have both merely appended new
lines since the last sync. Today such a file triggers the standard conflict
flow (three `.omemfs-conflict-*` helper files, pull aborted) even though the
two sides usually did not make any conflicting edit, only independent
appends. The planned feature lets `pull` merge these files automatically
instead.

## Config

A `[line_merge]` section in `.omemfs-filter` lists gitignore-subset patterns,
using exactly the same syntax and hierarchical application as `[ignore]` and
`[aggregate]` (see `design/05_ignore_and_aggregate.md`). A path matching
`[line_merge]` in any ancestor `.omemfs-filter` is a candidate for the
automatic merge described below.

## Non-goals

- Not a general-purpose text three-way merge. It is specific to line-based,
  append-only logs.
- Not applied to paths that do not match `[line_merge]`; those follow the
  existing conflict flow unchanged.
- Does not merge in-place edits to a single line. It only reconciles
  line-level insertions and deletions relative to the shared base
  (clone root). A genuine overlapping edit to the same base line falls back
  to the existing conflict-helper mechanism (see "Conflict-fallback trigger"
  below).

## Merge algorithm

Implemented in `src/line_merge.rs` as:

```rust
pub enum MergeOutcome { Clean(Vec<u8>), Conflict }
pub fn merge(base: &[u8], local: &[u8], remote: &[u8]) -> MergeOutcome
```

A three-way, line-level merge (diff3-style) between:

- **base** — the blob at `clone_root` (last synced state).
- **local** — the file in the working tree (current local state).
- **remote** — the blob at `remote_root` (latest remote state).

`merge` never panics and never returns an error: on any input, including
empty inputs and inputs containing invalid UTF-8, it returns either a clean
merged buffer or a `Conflict` signal. Wiring `Conflict` into the existing
conflict-helper-file flow is the caller's responsibility (see "Planned `pull`
integration point" below) — this module only detects the condition.

### Lines are byte slices, not UTF-8 text

Input buffers are split on `b'\n'` into `Vec<&[u8]>`, never decoded to
`String`/`&str`. This means a line containing non-UTF-8 bytes (e.g. a shell
command with a stray invalid byte) passes through untouched instead of
tripping a decode error. The only text-aware step is timestamp detection
(see "Ordering" below), which parses a short, strictly-ASCII prefix and falls
back to "no timestamp" on any decode failure rather than erroring out. A
buffer that ends with `\n` does not produce a trailing empty line (the
terminator is not itself a line); an empty buffer splits to zero lines.

### Diff engine

Line-level diffs are computed with the [`similar`](https://docs.rs/similar)
crate (`similar = "2"` in `Cargo.toml`), using `similar::capture_diff_slices`
with `Algorithm::Myers`. Two independent diffs are computed, both against the
same base line slice:

- `base` vs `local` → `local_ops: Vec<similar::DiffOp>`
- `base` vs `remote` → `remote_ops: Vec<similar::DiffOp>`

Each `DiffOp` is one of `Equal`, `Delete`, `Insert`, `Replace`, each carrying
`old_index`/`old_len` (position/length in `base`) and `new_index`/`new_len`
(position/length in the other side).

### Segment aggregation

The raw ops from each diff are aggregated into a `Vec<Segment>`:

- `Keep(Range<usize>)` — built from an `Equal` op; a run of base line indices
  that survived unchanged on this side.
- `Change { base_range: Range<usize>, other_lines: Vec<&[u8]> }` — built from
  a **maximal run of consecutive non-`Equal` ops** between two `Equal` ops
  (or between a boundary and an `Equal` op). `base_range` is the union of the
  `old_index..old_index+old_len` spans of every op in the run (a pure
  `Insert` contributes a zero-length span at its `old_index`, marking an
  insertion point rather than a dropped base range). `other_lines` is the
  concatenation, in op order, of every op's `new_index..new_index+new_len`
  slice from the other side (a pure `Delete` contributes nothing).

Aggregating this way means the merge logic never has to care whether
`similar` handed back one `Replace` op or a `Delete` immediately followed by
an `Insert` for the same edit — both shapes collapse into one `Change`
segment with identical `base_range` and `other_lines`.

### Deletion policy ("Policy A")

For each base line index `i`, define `local_kept(i)` / `remote_kept(i)` as
"`i` falls inside a `Keep` segment of the base-vs-local / base-vs-remote
diff". A base line survives in the merged output iff `local_kept(i) ||
remote_kept(i)`; it is dropped only if both diffs put `i` inside a `Change`
segment. This never loses a line that either side still holds, and it
specifically tolerates `HISTSIZE`-style front-truncation (a shell trimming
its history file from the front) on either or both sides without treating
the trimmed-away lines as lost history — as long as the other side (or
neither side, per the conflict rule below) still has that base line, it is
kept.

### Conflict-fallback trigger

A genuine overlapping edit — the same base range rewritten independently on
both sides — cannot be resolved by this line-level algorithm and is signalled
as a whole-file `MergeOutcome::Conflict` rather than silently picking a
side. Concretely: the merge is a conflict iff there exists a `Change`
segment `L` from the local diff and a `Change` segment `R` from the remote
diff such that:

1. `L.other_lines` is non-empty (local put replacement content at that gap), and
2. `R.other_lines` is non-empty (remote put replacement content at that same gap), and
3. `L.base_range` and `R.base_range` overlap as integer ranges (i.e. there is
   at least one base index inside both), so it is the *same* gap.

Condition 3 is a true range intersection: two zero-length ranges (e.g. two
independent pure appends that both land at the same base-end position) never
overlap under this definition, since neither contains any index. This is
what keeps ordinary concurrent appends — the common case for a log file —
from ever being misclassified as a conflict: appending on both sides
produces two `Change` segments with zero-length, coincident `base_range`s,
which do not satisfy condition 3.

A **one-sided rewrite** — only local or only remote has non-empty
`other_lines` at a given gap, the other side left the gap alone (a `Keep`) or
dropped it with no replacement (a `Delete`-only `Change`) — is *not* a
conflict: the base line (if still held by the non-rewriting side) is kept via
Policy A, and the rewriting side's new content is added as its own new
line(s). Nothing is silently discarded and nothing is silently overwritten.

### Building the clean output

When no conflict is detected:

- `kept_base_lines` = base lines at indices that survive Policy A, in base order.
- `local_new_lines` = `other_lines` of every `Change` segment from the local
  diff, concatenated in segment order (equivalently, local's file order,
  since diff ops are monotonic in both `old_index` and `new_index`).
- `remote_new_lines` = the same, from the remote diff's `Change` segments.

### Ordering

- **Timestamped mode**: if at least one line among `base`, `local`, or
  `remote` matches the zsh `extended_history` prefix `: <epoch>:<duration>;
  <command>` (a line starting with `": "`, then decimal digits, then `":"`,
  then more decimal digits, then `";"`), the candidate set
  (`kept_base_lines ∪ local_new_lines ∪ remote_new_lines`) is stable-sorted
  by the parsed epoch. A line that does not itself match the prefix sorts as
  `i64::MIN`, so non-timestamped lines float to the front; ties (including
  ties at `i64::MIN`) preserve the input's relative order because the sort is
  stable.
- **Fallback mode** (no line anywhere matches the timestamp prefix): the
  deterministic order is `kept_base_lines`, then `local_new_lines`, then
  `remote_new_lines`, with no re-sorting.

Only the short ASCII timestamp prefix is decoded as text (byte-level digit
checks, or `std::str::from_utf8` on just that prefix); a decode or parse
failure on the prefix is treated as "no timestamp", never a panic or error —
this keeps the byte-level guarantee above intact even in timestamped mode.

### No de-duplication

Identical lines are not de-duplicated. A repeated command is a legitimate,
separate event in a history log, not a duplicate to collapse; it survives as
its own line wherever the algorithm above places it.

### Output encoding

Merged lines are rejoined with `b'\n'` separators. A trailing `b'\n'` is
appended iff the merged line list is non-empty (an empty merge result is the
empty byte string, not a lone newline). Exact original trailing-newline
presence is not preserved bit-for-bit — this is a merge, not a copy.

## `pull` integration point

After `classify_conflicts` partitions each path into `clean_diff` /
`conflicts`, `pull` (both the full-repo path and the path-scoped path, in
`src/commands/pull.rs`) runs a merge pass over the remaining `conflicts`,
before the conflict-abort check:

- A path is only a merge candidate when both the remote and local diff
  entries for it are `Added`/`Modified` (a real blob on both sides — a
  delete, directory, or symlink on either side is skipped and falls straight
  through to the conflict flow), the path matches `[line_merge]`
  (`FilterSet::is_line_merge`), and the working-tree path is currently a
  regular file.
- For a candidate, `pull` resolves base bytes (the blob at `clone_root`, or
  empty if the path had no base entry), local bytes (read from the working
  tree), and remote bytes (the blob referenced by the remote diff entry), and
  calls `line_merge::merge`. Any I/O error while resolving these bytes is
  treated as "cannot attempt this merge" — the path is left in `conflicts`
  rather than failing the pull.
- On `MergeOutcome::Clean(bytes)`, the path is removed from `conflicts`
  immediately, but `bytes` is **not** written yet — the merge is only
  *resolved*, held in memory, at this point. This split matters for the
  atomic-abort policy (`03_sync_model.md`, `04_cli_spec.md`): if some other,
  unrelated path is still conflicting, the whole pull aborts and nothing —
  including this already-resolved merge — may be applied to the working
  tree. Only after the conflict-abort check confirms `conflicts` is empty
  does `pull` write every resolved merge's bytes directly to its
  working-tree file (atomically, via
  `crate::store::local::atomic_write_with_no_fsync`). A merged path is never
  added to `clean_diff` (so `apply_diff` never overwrites the merged content
  with the raw remote blob). `clone_root` still advances to the remote root
  exactly as it does for every other path, matching `03_sync_model.md`'s
  "Dirty pull (no conflict)" semantics for a file that is now dirty relative
  to the new `clone_root`.
- On `MergeOutcome::Conflict`, the path is left in `conflicts` unchanged and
  proceeds through the existing conflict-helper-file flow exactly as before.

Successfully merged paths are reported in the pull output as a
"Line-merged the following paths:" block, alongside the existing "preserved"
block.

## Default `omemfs clone` template patterns

The default template shipped by `omemfs clone` (see
`design/05_ignore_and_aggregate.md`, "Default template on `omemfs clone`")
includes a `[line_merge]` section listing:

- `.zsh_history`
- `.bash_history`
- `.python_history`

These were chosen because they are common shell/REPL history files with a
simple newline-per-command (or newline-per-entry) format, making them
straightforward candidates for the line-level merge described above.
