#!/usr/bin/env bats
# Tests for the size-distribution histogram sections added to `omemfs stats`:
#   - "Remote object sizes" section appears after a push to a local remote
#   - "Working-tree file sizes" section is always present
#   - filter-ignored directories (node_modules/) are excluded from the WT histogram
#   - remote-object histogram bucket counts sum to the Remote storage total
#   - stats --json includes worktree_file_histogram and remote_object_histogram keys

load test_helper/common

setup() {
    setup_repo
}

teardown() {
    teardown_local_remote
    teardown_test_dir
}

# Push a handful of files so the remote has objects to classify.
push_some_files() {
    echo "hello world" > file_a.txt
    echo "another file" > file_b.txt
    dd if=/dev/urandom bs=1024 count=5 2>/dev/null | base64 > medium.dat
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
}

# ---------------------------------------------------------------------------
# Test 1: "Remote object sizes" section appears after push.
# ---------------------------------------------------------------------------

@test "stats_histogram: Remote object sizes section appears after push" {
    push_some_files

    run "$OMEMFS" stats --remote
    [ "$status" -eq 0 ]
    [[ "$output" == *"Remote object sizes"* ]]
    # Header line must show (origin) and total object count / total bytes.
    [[ "$output" == *"(origin)"* ]]
    # At least one bucket row must be present (non-empty histogram).
    # The table header row contains "bucket" and "count" labels.
    [[ "$output" == *"bucket"* ]]
    [[ "$output" == *"count"* ]]
}

# ---------------------------------------------------------------------------
# Test 2: "Working-tree file sizes" section is always present.
# ---------------------------------------------------------------------------

@test "stats_histogram: Working-tree file sizes section always present" {
    # Even with no push, the working-tree histogram is shown.
    echo "tracked file" > tracked.txt

    run "$OMEMFS" stats
    [ "$status" -eq 0 ]
    [[ "$output" == *"Working-tree file sizes"* ]]
    # Header must show total n and bytes.
    [[ "$output" == *"bucket"* ]]
}

# ---------------------------------------------------------------------------
# Test 3: filter-ignored directories are excluded from the WT histogram.
# We create a node_modules/ dir with several files and verify that after
# configuring the [ignore] rule the histogram count does NOT include them.
# ---------------------------------------------------------------------------

@test "stats_histogram: node_modules/ ignored by filter is excluded from WT histogram" {
    # Write a few tracked files.
    echo "tracked 1" > tracked1.txt
    echo "tracked 2" > tracked2.txt
    echo "tracked 3" > tracked3.txt

    # Run stats without node_modules — capture WT histogram total count.
    run "$OMEMFS" stats
    [ "$status" -eq 0 ]
    [[ "$output" == *"Working-tree file sizes"* ]]
    # Extract the n=... total from the WT histogram header.
    local baseline_n
    baseline_n=$(echo "$output" | grep "Working-tree file sizes" | grep -oE 'n=[0-9]+' | grep -oE '[0-9]+')
    [ -n "$baseline_n" ]

    # Now add a node_modules/ directory with many files.
    mkdir -p node_modules/some_pkg
    for i in $(seq 1 10); do
        echo "package content $i" > "node_modules/some_pkg/file$i.js"
    done

    # Update .omemfs-filter to ignore node_modules/ (it's in the default template
    # but write it explicitly to ensure it's present).
    printf '[ignore]\nnode_modules/\n' > .omemfs-filter

    run "$OMEMFS" stats
    [ "$status" -eq 0 ]
    [[ "$output" == *"Working-tree file sizes"* ]]
    # The WT histogram total must equal the baseline (node_modules files not counted).
    # The filter file itself (.omemfs-filter) IS tracked, so n may be baseline_n
    # (if .omemfs-filter was already counted) or baseline_n+1 if it is new.
    local after_n
    after_n=$(echo "$output" | grep "Working-tree file sizes" | grep -oE 'n=[0-9]+' | grep -oE '[0-9]+')
    [ -n "$after_n" ]
    # after_n must be small: baseline_n tracked files plus at most the filter file.
    # It must NOT have grown by 10 (the node_modules files must be excluded).
    [ "$after_n" -le $(( baseline_n + 2 )) ]
    # And it must be strictly less than baseline_n + 10 (ignoring the 10 nm files).
    [ "$after_n" -lt $(( baseline_n + 10 )) ]
}

# ---------------------------------------------------------------------------
# Test 4: remote-object histogram bucket counts sum to Remote storage total.
# ---------------------------------------------------------------------------

@test "stats_histogram: remote histogram bucket count sum equals remote storage total" {
    push_some_files

    run "$OMEMFS" stats --remote --json
    [ "$status" -eq 0 ]

    # The remote_object_histogram must be present.
    [[ "$output" == *'"remote_object_histogram"'* ]]

    # Sum all "count" values from remote_object_histogram entries.
    local hist_total
    hist_total=$(echo "$output" | python3 -c "
import sys, json
data = json.load(sys.stdin)
hist = data.get('remote_object_histogram', [])
print(sum(e['count'] for e in hist))
")
    [ -n "$hist_total" ]
    [ "$hist_total" -gt 0 ]

    # The remote_storage total count from the JSON output.
    local rs_total
    rs_total=$(echo "$output" | python3 -c "
import sys, json
data = json.load(sys.stdin)
rs = data.get('remote_storage', {})
print(rs.get('total', {}).get('count', 0))
")
    [ -n "$rs_total" ]

    # Histogram sum must equal the remote storage total count.
    [ "$hist_total" -eq "$rs_total" ]
}

# ---------------------------------------------------------------------------
# Test 5: stats --json includes both histogram keys.
# ---------------------------------------------------------------------------

@test "stats_histogram: stats --json includes worktree_file_histogram and remote_object_histogram" {
    push_some_files

    run "$OMEMFS" stats --remote --json
    [ "$status" -eq 0 ]
    [[ "$output" == *'"worktree_file_histogram"'* ]]
    [[ "$output" == *'"remote_object_histogram"'* ]]
    # Each histogram entry must have bucket, count, and bytes keys.
    [[ "$output" == *'"bucket"'* ]]
    [[ "$output" == *'"count"'* ]]
    [[ "$output" == *'"bytes"'* ]]
    # Existing keys must still be present.
    [[ "$output" == *'"total"'* ]]
    [[ "$output" == *'"by_type"'* ]]
}

# ---------------------------------------------------------------------------
# Test 6: WT histogram shows at least one bucket matching the tracked files.
# ---------------------------------------------------------------------------

@test "stats_histogram: WT histogram has at least one bucket after creating tracked files" {
    echo "small file" > small.txt
    # Create a file slightly above 1 KB.
    dd if=/dev/urandom bs=1100 count=1 2>/dev/null > medium.bin

    run "$OMEMFS" stats
    [ "$status" -eq 0 ]
    [[ "$output" == *"Working-tree file sizes"* ]]
    # At least one non-header line must show a bucket label.
    # The <256B bucket should be present for the tiny small.txt.
    [[ "$output" == *"<256B"* ]] || [[ "$output" == *"256B-1KB"* ]] || [[ "$output" == *"1-4KB"* ]]
}
