# shellcheck shell=bash
# The pinned 7-Zip source tree, fetched once into "$1/source". Sourced by
# build-sdk-oracle.sh and build-7zz.sh, so both build the same revision.

sevenzip_revision=0766b733fe3e06dd2a7f9a3cfbf2108ac73abd17 # 7-Zip 26.03

fetch_7zip_source() {
  local root="$1"
  if [ ! -d "$root/source" ]; then
    git init --bare --quiet "$root/git"
    git -C "$root/git" fetch --quiet --depth 1 https://github.com/ip7z/7zip.git "$sevenzip_revision"
    test "$(git -C "$root/git" rev-parse FETCH_HEAD)" = "$sevenzip_revision"
    mkdir "$root/source"
    git -C "$root/git" archive "$sevenzip_revision" | tar -x -C "$root/source"
  fi
}
