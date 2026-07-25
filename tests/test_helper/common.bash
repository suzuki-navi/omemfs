# tests/test_helper/common.bash
# Common helpers for omemfs CLI tests

OMEMFS="${OMEMFS:-omemfs}"

setup_test_dir() {
    TEST_DIR="$(mktemp -d)"
    cd "$TEST_DIR"
}

teardown_test_dir() {
    if [[ -n "$TEST_DIR" && -d "$TEST_DIR" ]]; then
        rm -rf "$TEST_DIR"
    fi
}

setup_local_remote() {
    REMOTE_DIR="$(mktemp -d)"
}

teardown_local_remote() {
    if [[ -n "$REMOTE_DIR" && -d "$REMOTE_DIR" ]]; then
        rm -rf "$REMOTE_DIR"
    fi
}

# Clone from an empty local remote and cd into the result.
setup_repo() {
    setup_test_dir
    setup_local_remote
    run "$OMEMFS" clone --new --url "$REMOTE_DIR" .
    [ "$status" -eq 0 ]
}

# Check whether an object file exists in the local cache (relative to cwd).
object_exists() {
    object_exists_in "." "$1"
}

# Check whether an object file exists in the local cache of the repo at
# directory $1 (adaptive-depth sharding: objects/<2>/<2>/<2>/<rest>).
object_exists_in() {
    local dir="$1"
    local hash="$2"
    local p1="${hash:0:2}"
    local p2="${hash:2:2}"
    local p3="${hash:4:2}"
    local rest="${hash:6}"
    [ -f "${dir}/.omemfs/objects/${p1}/${p2}/${p3}/${rest}" ]
}

# Read the clone_root hash.
get_clone_root() {
    cat .omemfs/clone_root
}

# Read the remote root hash from INDEX_ROOT via omemfs cat.
get_remote_root() {
    "$OMEMFS" cat index-root 2>/dev/null \
        | grep '"remote_root"' \
        | grep -oE '[0-9a-f]{64}'
}

# Skip the calling test unless OMEMFS_SLOW_TESTS is set. Used to gate
# very slow large-file tests (hundreds of MiB) out of the default run.
# Opt in with: OMEMFS_SLOW_TESTS=1 ./run_tests.sh
require_slow_tests() {
    if [ -z "${OMEMFS_SLOW_TESTS:-}" ]; then
        skip "slow test; set OMEMFS_SLOW_TESTS=1 to run"
    fi
}

# ---------------------------------------------------------------------------
# Cloud-backend integration helpers (env-gated; skipped by default)
# ---------------------------------------------------------------------------
#
# Cloud integration tests are skipped unless their endpoint/credentials env
# vars are set (see design/13_cloud_backends.md, "Testing strategy"). They
# bootstrap a repo by writing `.omemfs/config` directly with the full backend
# block — a custom `endpoint` is required to target an emulator (MinIO /
# storage-testbench) and is NOT expressible via a plain `s3://` / `gs://` URL,
# so config injection is the documented non-interactive path. A second clone is
# then obtained through a connection string (`config export` → `clone --url`),
# exercising the real read-from-cloud path.

# Write a repository config at `$1/.omemfs/config` from the JSON in `$2`, plus
# an empty clone_root so the directory is a valid (empty) repo ready to push.
seed_repo_config() {
    local dir="$1" config_json="$2"
    mkdir -p "$dir/.omemfs"
    printf '%s' "$config_json" > "$dir/.omemfs/config"
    printf '' > "$dir/.omemfs/clone_root"
}

# Export an omemfs_repo_ connection string for the repo in `$1`.
repo_connection_string() {
    (cd "$1" && "$OMEMFS" config export 2>/dev/null) \
        | grep -oE 'omemfs_repo_[A-Za-z0-9]+'
}

# Run a push from inside repo dir `$1`, asserting success.
push_repo() {
    run bash -c "cd '$1' && '$OMEMFS' push"
    [ "$status" -eq 0 ]
}

# A per-test unique prefix so concurrent/repeated runs against a shared bucket
# never collide. Uses BATS_TEST_NUMBER (stable within a file run).
unique_prefix() {
    echo "omemfs-it/${BATS_TEST_NUMBER:-0}-$$"
}

# --- S3 (MinIO) ------------------------------------------------------------
# Gate: OMEMFS_S3_TEST_ENDPOINT (e.g. http://localhost:9000).
# Optional: OMEMFS_S3_TEST_BUCKET (default "omemfs-test"),
#   OMEMFS_S3_TEST_ACCESS_KEY / _SECRET_KEY (default minioadmin/minioadmin),
#   OMEMFS_S3_TEST_REGION (default us-east-1).
require_s3() {
    if [ -z "${OMEMFS_S3_TEST_ENDPOINT:-}" ]; then
        skip "S3 integration test; set OMEMFS_S3_TEST_ENDPOINT (MinIO) to run"
    fi
    S3_BUCKET="${OMEMFS_S3_TEST_BUCKET:-omemfs-test}"
    S3_AK="${OMEMFS_S3_TEST_ACCESS_KEY:-minioadmin}"
    S3_SK="${OMEMFS_S3_TEST_SECRET_KEY:-minioadmin}"
    S3_REGION="${OMEMFS_S3_TEST_REGION:-us-east-1}"
    S3_PREFIX="$(unique_prefix)"
}

# Emit an S3 origin config JSON for the given prefix.
s3_config_json() {
    local prefix="$1"
    cat <<EOF
{
  "version": "2.0",
  "remotes": {
    "origin": {
      "type": "s3",
      "bucket": "${S3_BUCKET}",
      "region": "${S3_REGION}",
      "prefix": "${prefix}",
      "access_key_id": "${S3_AK}",
      "secret_access_key": "${S3_SK}",
      "endpoint": "${OMEMFS_S3_TEST_ENDPOINT}",
      "force_path_style": true
    }
  }
}
EOF
}

# --- GCS (storage-testbench) ----------------------------------------------
# Gate: OMEMFS_GCS_TEST_ENDPOINT (e.g. http://localhost:9000).
# Optional: OMEMFS_GCS_TEST_BUCKET (default "omemfs-test").
require_gcs() {
    if [ -z "${OMEMFS_GCS_TEST_ENDPOINT:-}" ]; then
        skip "GCS integration test; set OMEMFS_GCS_TEST_ENDPOINT (storage-testbench) to run"
    fi
    GCS_BUCKET="${OMEMFS_GCS_TEST_BUCKET:-omemfs-test}"
    GCS_PREFIX="$(unique_prefix)"
}

# Emit a GCS origin config JSON for the given prefix. No credentials => the
# adapter uses anonymous access (storage-testbench accepts it).
gcs_config_json() {
    local prefix="$1"
    cat <<EOF
{
  "version": "2.0",
  "remotes": {
    "origin": {
      "type": "gcs",
      "bucket": "${GCS_BUCKET}",
      "prefix": "${prefix}",
      "endpoint": "${OMEMFS_GCS_TEST_ENDPOINT}"
    }
  }
}
EOF
}

# --- Azure (real account only) --------------------------------------------
# Gate: all of OMEMFS_AZURE_TEST_ACCOUNT / _CONTAINER / _TENANT_ID /
#   _CLIENT_ID / _CLIENT_SECRET. No emulator (Entra-ID-only; see design/13).
require_azure() {
    if [ -z "${OMEMFS_AZURE_TEST_ACCOUNT:-}" ] \
        || [ -z "${OMEMFS_AZURE_TEST_CONTAINER:-}" ] \
        || [ -z "${OMEMFS_AZURE_TEST_TENANT_ID:-}" ] \
        || [ -z "${OMEMFS_AZURE_TEST_CLIENT_ID:-}" ] \
        || [ -z "${OMEMFS_AZURE_TEST_CLIENT_SECRET:-}" ]; then
        skip "Azure integration test; set OMEMFS_AZURE_TEST_* (real account) to run"
    fi
    AZ_PREFIX="$(unique_prefix)"
}

# Emit an Azure origin config JSON for the given prefix.
azure_config_json() {
    local prefix="$1"
    local endpoint_line=""
    if [ -n "${OMEMFS_AZURE_TEST_ENDPOINT:-}" ]; then
        endpoint_line="\"endpoint\": \"${OMEMFS_AZURE_TEST_ENDPOINT}\","
    fi
    cat <<EOF
{
  "version": "2.0",
  "remotes": {
    "origin": {
      "type": "azure",
      "account": "${OMEMFS_AZURE_TEST_ACCOUNT}",
      "container": "${OMEMFS_AZURE_TEST_CONTAINER}",
      "prefix": "${prefix}",
      "tenant_id": "${OMEMFS_AZURE_TEST_TENANT_ID}",
      "client_id": "${OMEMFS_AZURE_TEST_CLIENT_ID}",
      "client_secret": "${OMEMFS_AZURE_TEST_CLIENT_SECRET}",
      ${endpoint_line}
      "encryption": null
    }
  }
}
EOF
}
