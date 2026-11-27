#!/usr/bin/env bash
# Build the public-domain C archive decoder used by tests/sdk_functional.rs.
set -euo pipefail

root="${SEVENZ_SDK_BUILD_DIR:-$PWD/target/sdk-oracle}"
mkdir -p "$root"
root="$(cd "$root" && pwd)"
# shellcheck source=.github/scripts/7zip-source.sh
source "$(dirname "$0")/7zip-source.sh"
fetch_7zip_source "$root"
make -C "$root/source/C/Util/7z" -f makefile.gcc CC=cc -j2
oracle="$root/source/C/Util/7z/_o/7zdec"
test -x "$oracle"
if [ -n "${GITHUB_ENV:-}" ]; then
  echo "SEVENZ_TURBO_SDK_7ZDEC=$oracle" >> "$GITHUB_ENV"
  echo "SEVENZ_TURBO_SDK_REQUIRED=1" >> "$GITHUB_ENV"
fi
printf 'SDK archive decoder: %s\n' "$oracle"
