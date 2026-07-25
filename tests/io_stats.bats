#!/usr/bin/env bats
# Tests for io_stats.jsonl recording and omemfs stats "Recent I/O" display.

load test_helper/common

setup() {
    setup_repo
}

teardown() {
    teardown_local_remote
    teardown_test_dir
}

# ---------------------------------------------------------------------------
# Test 1: After push, io_stats.jsonl exists and has a "push" record with writes > 0
# ---------------------------------------------------------------------------

@test "io_stats: push writes a record with writes > 0 to io_stats.jsonl" {
    echo "hello world" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    local jsonl=".omemfs/io_stats.jsonl"
    [ -f "$jsonl" ]

    # The file must contain at least one JSON line with "cmd":"push"
    run grep '"cmd":"push"' "$jsonl"
    [ "$status" -eq 0 ]

    # The writes field must be > 0 (at least the tree and blob objects)
    local writes
    writes=$(grep '"cmd":"push"' "$jsonl" | tail -1 | grep -oE '"writes":[0-9]+' | grep -oE '[0-9]+$')
    [ -n "$writes" ]
    [ "$writes" -gt 0 ]
}

# ---------------------------------------------------------------------------
# Test 2: After pull, io_stats.jsonl contains a "pull" record with reads >= 0
# ---------------------------------------------------------------------------

@test "io_stats: pull writes a record to io_stats.jsonl" {
    # Push some content so pull has something to fetch.
    echo "content to pull" > data.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Clone into a second directory and pull from there.
    local second_dir
    second_dir="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$second_dir"
    [ "$status" -eq 0 ]

    # Modify the original and push again so the second clone has something to pull.
    echo "updated content" > data.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Pull in the second clone.
    pushd "$second_dir" > /dev/null
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]

    local jsonl="$second_dir/.omemfs/io_stats.jsonl"
    [ -f "$jsonl" ]

    # Must contain a "pull" record.
    run grep '"cmd":"pull"' "$jsonl"
    [ "$status" -eq 0 ]

    # reads field must be a non-negative integer.
    local reads
    reads=$(grep '"cmd":"pull"' "$jsonl" | tail -1 | grep -oE '"reads":[0-9]+' | grep -oE '[0-9]+$')
    [ -n "$reads" ]

    popd > /dev/null
    rm -rf "$second_dir"
}

# ---------------------------------------------------------------------------
# Test 3: omemfs stats includes "Recent I/O" section after records exist
# ---------------------------------------------------------------------------

@test "io_stats: omemfs stats shows Recent I/O section when records exist" {
    echo "stats test file" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # io_stats.jsonl should now exist.
    [ -f ".omemfs/io_stats.jsonl" ]

    run "$OMEMFS" stats
    [ "$status" -eq 0 ]
    [[ "$output" == *"Recent I/O"* ]]
    [[ "$output" == *"push"* ]]
}

# ---------------------------------------------------------------------------
# Test 4: omemfs stats output has NO "Recent I/O" when io_stats.jsonl is absent
# ---------------------------------------------------------------------------

@test "io_stats: omemfs stats omits Recent I/O when io_stats.jsonl is absent" {
    # Remove io_stats.jsonl if present (clone during setup may have written it).
    rm -f ".omemfs/io_stats.jsonl"
    [ ! -f ".omemfs/io_stats.jsonl" ]

    run "$OMEMFS" stats
    [ "$status" -eq 0 ]
    [[ "$output" != *"Recent I/O"* ]]
}

# ---------------------------------------------------------------------------
# Test 5: push + pull → two records; stats shows both; JSON has io_history + io_totals
# ---------------------------------------------------------------------------

@test "io_stats: push then pull produce two records; stats JSON includes io_history and io_totals" {
    # Push from the first clone.
    echo "file for two-record test" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Clone into a second directory.
    local second_dir
    second_dir="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$second_dir"
    [ "$status" -eq 0 ]

    # Update and push again so there is something for the second clone to pull.
    echo "updated" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # Pull in the second clone.
    pushd "$second_dir" > /dev/null
    run "$OMEMFS" pull
    [ "$status" -eq 0 ]

    local jsonl="$second_dir/.omemfs/io_stats.jsonl"
    [ -f "$jsonl" ]

    # The second clone should have two records: clone + pull.
    local line_count
    line_count=$(grep -c . "$jsonl" || true)
    [ "$line_count" -ge 2 ]

    # JSON stats must contain io_history and io_totals keys.
    run "$OMEMFS" stats --json
    [ "$status" -eq 0 ]
    [[ "$output" == *'"io_history"'* ]]
    [[ "$output" == *'"io_totals"'* ]]

    popd > /dev/null
    rm -rf "$second_dir"
}

# ---------------------------------------------------------------------------
# Test 6: "Last I/O" summary reports the most recent record, not the oldest
# record in the recent-I/O window.
# ---------------------------------------------------------------------------

@test "io_stats: Last I/O summary shows the most recent record" {
    echo "first" > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    echo "second" >> file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    # "pack" is the most recent command and must be reported, not the older
    # "push" records that precede it in the recent-I/O window.
    run "$OMEMFS" pack
    [ "$status" -eq 0 ]

    run "$OMEMFS" stats
    [ "$status" -eq 0 ]

    local last_io_line
    last_io_line=$(echo "$output" | grep "Last I/O")
    [[ "$last_io_line" == *"pack"* ]]
}
