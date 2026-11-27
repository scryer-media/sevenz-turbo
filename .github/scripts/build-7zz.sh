#!/usr/bin/env bash
# Build `7zz` from the pinned 7-Zip source for the 7-Zip interop tests, so
# they compare with one release on every runner instead of whichever 7-Zip
# the runner image carries.
set -euo pipefail

root="${SEVENZ_7ZZ_BUILD_DIR:-$PWD/target/7zz}"
mkdir -p "$root"
root="$(cd "$root" && pwd)"
# shellcheck source=.github/scripts/7zip-source.sh
source "$(dirname "$0")/7zip-source.sh"
fetch_7zip_source "$root"

bundle="$root/source/CPP/7zip/Bundles/Alone2"
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) makefile=../../cmpl_mac_arm64.mak output=b/m_arm64 ;;
  Darwin-x86_64) makefile=../../cmpl_mac_x64.mak output=b/m_x64 ;;
  *) makefile=../../cmpl_gcc.mak output=b/g ;;
esac
make -C "$bundle" -f "$makefile" -j4 >/dev/null
bin="$root/bin"
mkdir -p "$bin"
cp "$bundle/$output/7zz" "$bin/7zz"
"$bin/7zz" i >/dev/null
if [ -n "${GITHUB_PATH:-}" ]; then
  echo "$bin" >> "$GITHUB_PATH"
  echo "SEVENZ_TURBO_7ZZ_REQUIRED=1" >> "$GITHUB_ENV"
fi
"$bin/7zz" | sed -n 2p
printf '7zz: %s\n' "$bin/7zz"
