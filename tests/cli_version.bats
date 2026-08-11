#!/usr/bin/env bats

load test_helper/common

@test "--version: prints the program name and package version" {
    run "$OMEMFS" --version

    [ "$status" -eq 0 ]
    [[ "$output" =~ ^omemfs[[:space:]][0-9]+\.[0-9]+\.[0-9]+$ ]]
}
