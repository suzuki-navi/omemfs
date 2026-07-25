#!/bin/bash
set -euo pipefail

# Creates the release tag from Cargo.toml's version, so the tag string is
# never typed by hand and can never drift from the version actually being
# released. See scripts/PUBLISH.md for the full release flow.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

version="$(grep -m1 '^version = ' "$REPO_DIR/Cargo.toml" | sed -E 's/version = "(.*)"/\1/')"
tag="v${version}"

if [ -z "$version" ]; then
  echo "Failed to read version from Cargo.toml" >&2
  exit 1
fi

if git -C "$REPO_DIR" rev-parse "$tag" >/dev/null 2>&1; then
  echo "Tag ${tag} already exists." >&2
  exit 1
fi

git -C "$REPO_DIR" tag -a "$tag" -m "Release ${tag}"

echo "Created tag ${tag}."
echo "Push it with:"
echo "  git -C \"$REPO_DIR\" push --tags"
