#!/usr/bin/env bash
# Build the public-domain C archive decoder used by tests/sdk_functional.rs.
set -euo pipefail

revision=0766b733fe3e06dd2a7f9a3cfbf2108ac73abd17 # 7-Zip 26.03
root="${SEVENZ_SDK_BUILD_DIR:-$PWD/target/sdk-oracle}"
mkdir -p "$root"
root="$(cd "$root" && pwd)"
if [ ! -d "$root/source" ]; then
  git init --bare --quiet "$root/git"
  git -C "$root/git" fetch --quiet --depth 1 https://github.com/ip7z/7zip.git "$revision"
  test "$(git -C "$root/git" rev-parse FETCH_HEAD)" = "$revision"
  mkdir "$root/source"
  git -C "$root/git" archive "$revision" | tar -x -C "$root/source"
fi
make -C "$root/source/C/Util/7z" -f makefile.gcc CC=cc -j2
oracle="$root/source/C/Util/7z/_o/7zdec"
test -x "$oracle"
if [ -n "${GITHUB_ENV:-}" ]; then
  echo "SEVENZ_TURBO_SDK_7ZDEC=$oracle" >> "$GITHUB_ENV"
  echo "SEVENZ_TURBO_SDK_REQUIRED=1" >> "$GITHUB_ENV"
fi
printf 'SDK archive decoder: %s\n' "$oracle"
