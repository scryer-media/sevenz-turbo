#!/usr/bin/env bash
# Print every feature of the crate except `bzip2-permissive`, comma-separated,
# for `cargo ... --features "$(bash .github/scripts/all-features.sh)"`.
# `bzip2-permissive` excludes `bzip2`, so `--all-features` is a compile error;
# this list stands in for it. With `--check`, also verify that the docs.rs
# feature list in Cargo.toml is this same list.
set -euo pipefail

manifest="$(cd "$(dirname "$0")/../.." && pwd)/Cargo.toml"

features() {
  awk '/^\[features\]/ { on = 1; next }
       /^\[/ { on = 0 }
       on && /^[a-z0-9_-]+ *=/ { sub(/ *=.*/, ""); if ($0 != "bzip2-permissive") print }' "$manifest"
}

if [ "${1:-}" = "--check" ]; then
  docs_rs="$(awk '/^\[package\.metadata\.docs\.rs\]/ { on = 1; next }
                  /^\[/ { on = 0 }
                  on' "$manifest" | grep -o '"[a-z0-9_-]*"' | tr -d '"' | sort)"
  if [ "$docs_rs" != "$(features | sort)" ]; then
    echo "[package.metadata.docs.rs] features must be every feature but bzip2-permissive:" >&2
    diff <(features | sort) <(printf '%s\n' "$docs_rs") >&2 || true
    exit 1
  fi
  exit 0
fi

features | paste -sd, -
