#!/usr/bin/env bats
# Terminal-output safety, log robustness and local file permissions
# (design/04 "Terminal output safety", design/06 "Log file",
#  design/02 "Local directory permissions").

load test_helper/common

setup() {
    setup_repo
}

teardown() {
    teardown_local_remote
    teardown_test_dir
}

ESC=$'\x1b'

# ---------------------------------------------------------------------------
# Terminal escape injection through names
# ---------------------------------------------------------------------------

@test "ls: an ESC in a file name is shown escaped, never as a raw control byte" {
    printf 'x' > "evil${ESC}[2Jname.txt"
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    [[ "$output" != *"$ESC"* ]]
    [[ "$output" == *'evil\x1b[2Jname.txt'* ]]
}

@test "ls: a newline in a file name cannot forge an extra output row" {
    printf 'x' > $'line1\nFORGED ROW'
    printf 'y' > plain.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    # The newline is shown as \n, so the name stays on one row: exactly one
    # row mentions FORGED, and it is the row for "line1...".
    [ "$(printf '%s\n' "$output" | grep -c 'FORGED ROW')" -eq 1 ]
    [ "$(printf '%s\n' "$output" | grep 'FORGED ROW' | grep -c 'line1')" -eq 1 ]
    [[ "$output" == *'line1\nFORGED ROW'* ]]
    # No row starts with the forged text.
    [ "$(printf '%s\n' "$output" | grep -c '^FORGED ROW')" -eq 0 ]
}

@test "ls: printable non-ASCII names are not altered" {
    printf 'x' > '日本語ファイル.txt'
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    [[ "$output" == *'日本語ファイル.txt'* ]]
}

@test "push --dry-run: an ESC in a file name is escaped in the output" {
    printf 'x' > "evil${ESC}]0;title.txt"
    run "$OMEMFS" push --dry-run
    [ "$status" -eq 0 ]
    [[ "$output" != *"$ESC"* ]]
}

@test "conflict list: an ESC in a conflicting name is escaped" {
    name="c${ESC}[31mred.txt"
    printf 'base' > "$name"
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    clone2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$clone2"
    [ "$status" -eq 0 ]
    printf 'local' > "$name"
    (cd "$clone2" && printf 'remote' > "$name" && "$OMEMFS" push)
    run "$OMEMFS" pull
    [ "$status" -ne 0 ]
    rm -rf "$clone2"

    run "$OMEMFS" conflict list
    [ "$status" -eq 0 ]
    [[ "$output" != *"$ESC"* ]]
    [[ "$output" == *'c\x1b[31mred.txt'* ]]
}

@test "log: a name with a newline cannot forge a second log record" {
    printf 'x' > $'evil\n[omemfs L1 cmd ]    +9.999s   push: end (0ms)'
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    # No log line may consist of the forged record.
    run grep -c '^\[omemfs L1 cmd \]    +9.999s   push: end (0ms)$' .omemfs/logs/latest.log
    [ "$output" = "0" ]
}

# ---------------------------------------------------------------------------
# `omemfs log show` / `log ls` robustness
# ---------------------------------------------------------------------------

@test "log show: control characters stored in a log file are escaped on output" {
    printf '[omemfs L1 cmd ]    +0.001s   msg %s[2Jhidden\n' "$ESC" > "$TEST_DIR/hand.log"
    run "$OMEMFS" log show "$TEST_DIR/hand.log"
    [ "$status" -eq 0 ]
    [[ "$output" != *"$ESC"* ]]
    [[ "$output" == *'msg \x1b[2Jhidden'* ]]
}

@test "log show: a multi-byte character inside the prefix region does not panic" {
    {
        printf '[omemfs 日本語] +0.001s   a\n'
        printf '[omemfs L1 cmd ]    +0.001s   ok 日本語\n'
        printf '日本語日本語日本語日本語日本語日本語\n'
    } > "$TEST_DIR/mb.log"
    run "$OMEMFS" log show "$TEST_DIR/mb.log"
    [ "$status" -eq 0 ]
    [[ "$output" == *'ok 日本語'* ]]
    run "$OMEMFS" log timers "$TEST_DIR/mb.log"
    [ "$status" -eq 0 ]
}

@test "log ls: a log file with a multi-byte name does not panic" {
    : > ".omemfs/logs/2026日本語1-143012-push.log"
    : > ".omemfs/logs/日本語日本語日本語日本語日本語.log"
    run "$OMEMFS" log ls
    [ "$status" -eq 0 ]
}

@test "stats: a multi-byte timestamp in io_stats.jsonl does not panic" {
    printf 'x' > file.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    printf '%s\n' '{"ts":"2026-日本語T02:05:11Z_padding_to_be_long_enough"}' >> .omemfs/io_stats.jsonl
    run "$OMEMFS" stats
    [[ "$output" != *"panicked"* ]]
}

# ---------------------------------------------------------------------------
# Log file names
# ---------------------------------------------------------------------------

@test "log: rapid invocations in the same second each keep their own log file" {
    before="$(find .omemfs/logs -name '*.log' ! -name latest.log | wc -l)"
    for _ in 1 2 3 4 5 6; do
        "$OMEMFS" ls > /dev/null
    done
    after="$(find .omemfs/logs -name '*.log' ! -name latest.log | wc -l)"
    [ "$((after - before))" -eq 6 ]
}

@test "log: a second log in the same second is a normal logical name for log show" {
    for _ in 1 2 3 4; do
        "$OMEMFS" ls > /dev/null
    done
    name="$(find .omemfs/logs -name '*-ls-*.log' -printf '%f\n' | head -n 1)"
    if [ -z "$name" ]; then
        skip "no same-second collision happened in this run"
    fi
    run "$OMEMFS" log show "${name%.log}"
    [ "$status" -eq 0 ]
}

@test "log: a pre-created predictable temp name cannot block commands" {
    # The old temp name was omemfs-YYYYMMDD-HHMMSS-<cmd>.log in $TMPDIR. Occupy
    # every plausible name for this and the next few seconds with directories.
    isolated_tmp="$(mktemp -d)"
    now=$(date +%s)
    for off in 0 1 2 3 4 5; do
        ts="$(date -d "@$((now + off))" +%Y%m%d-%H%M%S)"
        mkdir "$isolated_tmp/omemfs-${ts}-ls.log"
    done
    run env TMPDIR="$isolated_tmp" "$OMEMFS" ls
    [ "$status" -eq 0 ]
    rm -rf "$isolated_tmp"
}

# ---------------------------------------------------------------------------
# Permissions
# ---------------------------------------------------------------------------

mode_of() {
    # -L: latest.log is a symlink, whose own mode is meaningless (always 777).
    stat -L -c '%a' "$1"
}

@test "permissions: .omemfs is 0700 and its own files are 0600 even under umask 000" {
    d="$(mktemp -d)"
    r="$(mktemp -d)"
    run bash -c "umask 000; cd '$d' && '$OMEMFS' clone --new --url '$r' . && printf x > f.txt && '$OMEMFS' push"
    [ "$status" -eq 0 ]

    [ "$(mode_of "$d/.omemfs")" = "700" ]
    [ "$(mode_of "$d/.omemfs/logs")" = "700" ]
    [ "$(mode_of "$d/.omemfs/config")" = "600" ]
    [ "$(mode_of "$d/.omemfs/clone_root")" = "600" ]
    [ "$(mode_of "$d/.omemfs/clone_root.lock")" = "600" ]
    [ "$(mode_of "$d/.omemfs/logs/latest.log")" = "600" ]
    if [ -f "$d/.omemfs/io_stats.jsonl" ]; then
        [ "$(mode_of "$d/.omemfs/io_stats.jsonl")" = "600" ]
    fi
    while IFS= read -r f; do
        [ "$(mode_of "$f")" = "600" ]
    done < <(find "$d/.omemfs/logs" -name '*.log' -type f)
    rm -rf "$d" "$r"
}

@test "permissions: an existing repository's modes are not changed" {
    chmod 755 .omemfs
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    [ "$(mode_of .omemfs)" = "755" ]
}
