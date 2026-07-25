#!/usr/bin/env bats
# Color output tests for omemfs ls.
#
# - Pipe/file: no ANSI escapes.
# - CLICOLOR_FORCE=1: force color even on a pipe.
# - NO_COLOR: disable color even with CLICOLOR_FORCE=1 or under a pty.
# - PTY: color on by default.

load test_helper/common

setup() {
    setup_repo
}

teardown() {
    teardown_local_remote
    teardown_test_dir
}

has_ansi() {
    local s="$1"
    printf '%s' "$s" | grep -qP '\x1b\[[0-9][0-9;]*m'
}

@test "color: ls outputs no ANSI when stdout is a pipe" {
    echo hello > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    ! has_ansi "$output"
}

@test "color: CLICOLOR_FORCE=1 enables color on pipe (ls)" {
    echo hello > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    CLICOLOR_FORCE=1 run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    has_ansi "$output"
}

@test "color: NO_COLOR overrides CLICOLOR_FORCE" {
    echo hello > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    NO_COLOR=1 CLICOLOR_FORCE=1 run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    ! has_ansi "$output"
}

@test "color: ls under a pty emits ANSI escapes" {
    echo hello > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run script -q -c "env '$OMEMFS' ls" /dev/null
    [ "$status" -eq 0 ]
    has_ansi "$output"
}

@test "color: NO_COLOR disables color under a pty" {
    echo hello > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run script -q -c "env NO_COLOR=1 '$OMEMFS' ls" /dev/null
    [ "$status" -eq 0 ]
    ! has_ansi "$output"
}
