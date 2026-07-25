#!/usr/bin/env bats
# Tests for `omemfs log ls`, `omemfs log show`, and `omemfs log timers`

load test_helper/common

setup() {
    setup_repo
    # Generate a log file by running push
    printf 'hello' > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
}

teardown() {
    teardown_local_remote
    teardown_test_dir
}

# ---------------------------------------------------------------------------
# log ls
# ---------------------------------------------------------------------------

@test "log ls: lists log files" {
    run "$OMEMFS" log ls
    [ "$status" -eq 0 ]
    # Should show at least one entry with a logical name (no .log suffix)
    [[ "$output" =~ push ]]
    # Logical name must not contain .log
    [[ ! "$output" =~ \.log ]]
}

@test "log ls: shows [latest] marker" {
    run "$OMEMFS" log ls
    [ "$status" -eq 0 ]
    [[ "$output" =~ \[latest\] ]]
}

@test "log ls: shows index numbers" {
    run "$OMEMFS" log ls
    [ "$status" -eq 0 ]
    [[ "$output" =~ "  1  " ]]
}

@test "log ls: -n limits output count" {
    # Generate multiple log files
    printf 'world' > file2.txt
    run "$OMEMFS" push
    printf 'three' > file3.txt
    run "$OMEMFS" push

    run "$OMEMFS" log ls -n 1
    [ "$status" -eq 0 ]
    # Should show only one entry
    count=$(echo "$output" | grep -c '^\s*1\s')
    [ "$count" -eq 1 ]
    # Should not show entry 2
    [[ ! "$output" =~ "  2  " ]]
}

@test "log ls: --cmd filters by command name" {
    # Generate a pull log too (may fail but still creates a log)
    run "$OMEMFS" pull || true

    run "$OMEMFS" log ls --cmd push
    [ "$status" -eq 0 ]
    # All shown entries should be push logs
    [[ "$output" =~ push ]]
}

@test "log ls: default shows at most 10 entries" {
    # Generate 12 log files
    for i in $(seq 1 12); do
        printf "content%d" "$i" > "f${i}.txt"
        run "$OMEMFS" push
    done

    run "$OMEMFS" log ls
    [ "$status" -eq 0 ]
    # Should not show more than 10 numbered entries
    [[ ! "$output" =~ "  11  " ]]
}

# ---------------------------------------------------------------------------
# log show with REF forms
# ---------------------------------------------------------------------------

@test "log show: works without REF (uses latest.log)" {
    run "$OMEMFS" log show
    [ "$status" -eq 0 ]
    # Should contain at least one log line
    [[ "$output" =~ \[omemfs ]]
}

@test "log show: accepts @1 (newest log)" {
    run "$OMEMFS" log show @1
    [ "$status" -eq 0 ]
    [[ "$output" =~ \[omemfs ]]
}

@test "log show: accepts logical name" {
    # Get the logical name from log ls
    run "$OMEMFS" log ls
    [ "$status" -eq 0 ]
    logical_name=$(echo "$output" | grep -oE '[0-9]{8}-[0-9]{6}-[a-z]+' | head -1)
    [ -n "$logical_name" ]

    run "$OMEMFS" log show "$logical_name"
    [ "$status" -eq 0 ]
    [[ "$output" =~ \[omemfs ]]
}

@test "log show: @N out of range gives error" {
    run "$OMEMFS" log show @999
    [ "$status" -ne 0 ]
}

@test "log show: --layer filters output" {
    run "$OMEMFS" log show --layer L1
    [ "$status" -eq 0 ]
    # All lines should be L1
    while IFS= read -r line; do
        [[ -z "$line" ]] || [[ "$line" =~ "L1 cmd" ]]
    done <<< "$output"
}

@test "log show: --grep filters output" {
    run "$OMEMFS" log show --grep "start"
    [ "$status" -eq 0 ]
    # All lines should contain "start"
    while IFS= read -r line; do
        [[ -z "$line" ]] || [[ "$line" =~ start ]]
    done <<< "$output"
}

# ---------------------------------------------------------------------------
# log timers with REF forms
# ---------------------------------------------------------------------------

@test "log timers: works without REF" {
    run "$OMEMFS" log timers
    [ "$status" -eq 0 ]
}

@test "log timers: accepts @1" {
    run "$OMEMFS" log timers @1
    [ "$status" -eq 0 ]
}

@test "log timers: accepts logical name" {
    run "$OMEMFS" log ls
    [ "$status" -eq 0 ]
    logical_name=$(echo "$output" | grep -oE '[0-9]{8}-[0-9]{6}-[a-z]+' | head -1)
    [ -n "$logical_name" ]

    run "$OMEMFS" log timers "$logical_name"
    [ "$status" -eq 0 ]
}

# ---------------------------------------------------------------------------
# Temp log file cleanup when run outside a repository
# ---------------------------------------------------------------------------

@test "no stray temp log file remains when run outside a repository" {
    # Use an isolated temp dir so we only see this invocation's stray files.
    isolated_tmp="$(mktemp -d)"
    outside_dir="$(mktemp -d)"

    # Run a command that requires a repo from a directory with no .omemfs.
    # It must fail (no repo found) but must not leave an omemfs-*.log behind.
    run env TMPDIR="$isolated_tmp" bash -c "cd '$outside_dir' && '$OMEMFS' push"
    [ "$status" -ne 0 ]

    stray="$(find "$isolated_tmp" -maxdepth 1 -name 'omemfs-*.log' 2>/dev/null)"
    [ -z "$stray" ]

    rm -rf "$isolated_tmp" "$outside_dir"
}
