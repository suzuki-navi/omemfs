#!/usr/bin/env bats
# Working-tree staging files (design/09 ".omemfs-tmp-*", design/11) and
# io_stats rotation (design/04 "io_stats.jsonl", design/11).

load test_helper/common

setup() {
    setup_repo
}

teardown() {
    teardown_local_remote
    teardown_test_dir
}

@test "staging: a leftover .omemfs-tmp file is not pushed and not listed" {
    printf 'real' > real.txt
    printf 'half-written' > .omemfs-tmp-1a2b-3c4d-0
    mkdir sub
    printf 'y' > sub/kept.txt
    printf 'half' > sub/.omemfs-tmp-9-9-9

    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    run "$OMEMFS" ls -r
    [ "$status" -eq 0 ]
    [[ "$output" == *real.txt* ]]
    [[ "$output" == *sub/kept.txt* ]]
    [[ "$output" != *omemfs-tmp* ]]

    # A second clone must not receive it either.
    clone2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$clone2"
    [ "$status" -eq 0 ]
    [ ! -e "$clone2/.omemfs-tmp-1a2b-3c4d-0" ]
    [ ! -e "$clone2/sub/.omemfs-tmp-9-9-9" ]
    [ -f "$clone2/sub/kept.txt" ]
    rm -rf "$clone2"
}

@test "staging: a leftover is not reported as an unknown reserved file" {
    printf 'x' > .omemfs-tmp-1-2-3
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    [[ "$output" != *"unknown reserved"* ]]
    run "$OMEMFS" ls
    [ "$status" -eq 0 ]
    [[ "$output" != *"unknown reserved"* ]]
}

@test "staging: a leftover next to a tracked file does not make push see a change" {
    printf 'real' > real.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    printf 'junk' > .omemfs-tmp-1-2-3
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    [[ "$output" == *"Nothing to push"* || "$output" == *"up to date"* || "$output" == *"nothing"* ]]
}

@test "staging: restore removes a leftover silently" {
    printf 'real' > real.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    printf 'junk' > .omemfs-tmp-1-2-3
    run "$OMEMFS" restore
    [ "$status" -eq 0 ]
    [ ! -e .omemfs-tmp-1-2-3 ]
    [[ "$output" != *omemfs-tmp* ]]
}

@test "staging: a normal pull leaves no staging file behind" {
    printf 'v1' > f.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    clone2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$clone2"
    [ "$status" -eq 0 ]
    printf 'v2' > f.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    (cd "$clone2" && run "$OMEMFS" pull && [ "$status" -eq 0 ])
    [ "$(cat "$clone2/f.txt")" = "v2" ]
    run find "$clone2" -path "$clone2/.omemfs" -prune -o -name '.omemfs-tmp-*' -print
    [ -z "$output" ]
    rm -rf "$clone2"
}

@test "staging: a stale (older than 24h) leftover is swept by the next write into that directory" {
    printf 'v1' > f.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    clone2="$(mktemp -d)"
    run "$OMEMFS" clone --existing --url "$REMOTE_DIR" "$clone2"
    [ "$status" -eq 0 ]
    printf 'old' > "$clone2/.omemfs-tmp-old-leftover"
    touch -d '2 days ago' "$clone2/.omemfs-tmp-old-leftover"
    printf 'new' > "$clone2/.omemfs-tmp-fresh-leftover"

    printf 'v2' > f.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    (cd "$clone2" && "$OMEMFS" pull)

    [ ! -e "$clone2/.omemfs-tmp-old-leftover" ]
    [ -e "$clone2/.omemfs-tmp-fresh-leftover" ]
    rm -rf "$clone2"
}

@test "io_stats: rotation keeps the file valid and appends after it" {
    printf 'x' > a.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]
    # Grow the file past the 10 MiB threshold with valid-looking records.
    python3 - <<'PY'
import json
with open('.omemfs/io_stats.jsonl', 'a') as f:
    rec = {"ts": "2026-01-01T00:00:00Z", "cmd": "push", "pad": "p" * 200}
    line = json.dumps(rec) + "\n"
    for _ in range((10 * 1024 * 1024) // len(line) + 50):
        f.write(line)
PY
    before="$(stat -c %s .omemfs/io_stats.jsonl)"
    [ "$before" -gt $((10 * 1024 * 1024)) ]

    printf 'y' > b.txt
    run "$OMEMFS" push
    [ "$status" -eq 0 ]

    after="$(stat -c %s .omemfs/io_stats.jsonl)"
    [ "$after" -lt "$before" ]
    # No leftover temp file in .omemfs, and the last line is the new push record.
    [ -z "$(find .omemfs -maxdepth 1 -name '.tmp*' -o -maxdepth 1 -name '.omemfs-tmp-*')" ]
    tail -n 1 .omemfs/io_stats.jsonl | grep -q '"cmd":"push"'
}
