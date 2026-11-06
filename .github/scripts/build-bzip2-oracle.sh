#!/usr/bin/env bash
# Build the pinned LGPL reference executable for codec differential tests.
set -euo pipefail

revision=9128b80e3a471108678e2b3ec3c985861c8d7b0f # 7-Zip 26.04
root="${BZIP2_ORACLE_BUILD_DIR:-$PWD/target/bzip2-oracle}"
mkdir -p "$root"
root="$(cd "$root" && pwd)"
marker=.bzip2-oracle-revision
if [ -e "$root/source" ] || [ -L "$root/source" ]; then
  if [ ! -f "$root/source/$marker" ] || [ "$(< "$root/source/$marker")" != "$revision" ]; then
    printf 'Refusing unverified or mismatched reference source: %s\n' "$root/source" >&2
    exit 1
  fi
else
  staging="$(mktemp -d "$root/source.staging.XXXXXX")"
  trap 'if [ -n "${staging:-}" ]; then rm -rf -- "$staging"; fi' EXIT
  trap 'exit 130' INT
  trap 'exit 143' HUP TERM
  git init --bare --quiet "$root/git"
  git -C "$root/git" fetch --quiet --depth 1 https://github.com/ip7z/7zip.git "$revision"
  test "$(git -C "$root/git" rev-parse 'FETCH_HEAD^{commit}')" = "$revision"
  git -C "$root/git" archive "$revision" | tar -x -C "$staging"
  printf '%s\n' "$revision" > "$staging/$marker"
  # A failed or interrupted archive never becomes a reusable source tree.
  test ! -e "$root/source" && test ! -L "$root/source"
  mv "$staging" "$root/source"
  staging=
fi
bundle="$root/source/CPP/7zip/Bundles/Alone2"
if [ "$(uname -s)" = Darwin ]; then
  make -C "$bundle" -f ../../cmpl_mac_arm64.mak -j2
  oracle="$bundle/b/m_arm64/7zz"
else
  make -C "$bundle" -f makefile.gcc -j2
  oracle="$bundle/_o/7zz"
fi
test -x "$oracle"
if [ -n "${GITHUB_ENV:-}" ]; then
  echo "BZIP2_TURBO_7ZZ=$oracle" >> "$GITHUB_ENV"
fi
printf 'BZip2 reference: %s\n' "$oracle"
