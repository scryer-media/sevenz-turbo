#!/usr/bin/env bash
# Install the pinned 7-Zip release for the 7-Zip interop tests, so they
# compare with one release on every runner instead of whichever 7-Zip the
# runner image carries. The files are the official release assets, each
# checked against its SHA-256 before use.
set -euo pipefail

release=26.03
base="https://github.com/ip7z/7zip/releases/download/$release"
root="${SEVENZ_7ZIP_DIR:-$PWD/target/7zip}"
mkdir -p "$root"
root="$(cd "$root" && pwd)"

# fetch NAME SHA256: download a release asset into "$root" and verify it.
fetch() {
  curl --fail --silent --show-error --location --retry 3 -o "$root/$1" "$base/$1"
  if command -v sha256sum >/dev/null; then
    echo "$2  $root/$1" | sha256sum --check --quiet -
  else
    echo "$2  $root/$1" | shasum -a 256 --check --quiet -
  fi
}

bin="$root/bin"
mkdir -p "$bin"
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)
    fetch 7z2603-linux-x64.tar.xz dc99eff5008f1ab79bd7084c68513701547a808a89502bf4133683535ab3c695
    tar -xJf "$root/7z2603-linux-x64.tar.xz" -C "$bin" 7zz
    tool=7zz
    ;;
  Linux-aarch64)
    fetch 7z2603-linux-arm64.tar.xz 2389ba20e4d8295e8709c20b6263b69bd1ec4972fe38a04ad7a1badbf595b996
    tar -xJf "$root/7z2603-linux-arm64.tar.xz" -C "$bin" 7zz
    tool=7zz
    ;;
  Darwin-*)
    fetch 7z2603-mac.tar.xz 5ca87677072c59f5602e5c49baa27d4694bacd2259b4e507f0094249d4281480
    tar -xJf "$root/7z2603-mac.tar.xz" -C "$bin" 7zz
    tool=7zz
    ;;
  MINGW*-x86_64 | MSYS*-x86_64)
    # The installer is a 7z archive behind a stub; 7zr unpacks 7z.exe and
    # 7z.dll from it without running it. The tests run `7zz`, the name the
    # other platforms' releases use, so 7z.exe is also there under it.
    fetch 7zr.exe ad4c82fadcbdf93c03b4fc440f300509c7d60c5c2f4d183e35d9d70d6957037d
    fetch 7z2603-x64.exe 0859c524b8a63551848f0c246abddcb1d0b7b656b0fbfe879f8d85e61a9e6edd
    (cd "$bin" && "$root/7zr.exe" x -y -bso0 -bsp0 "$root/7z2603-x64.exe" 7z.exe 7z.dll)
    cp "$bin/7z.exe" "$bin/7zz.exe"
    tool=7zz
    ;;
  *)
    echo "no pinned 7-Zip for $(uname -s)-$(uname -m)" >&2
    exit 1
    ;;
esac

banner="$("$bin/$tool" | sed -n 2p)"
case "$banner" in
  *" $release "*) ;;
  *)
    echo "expected 7-Zip $release, got: $banner" >&2
    exit 1
    ;;
esac
if [ -n "${GITHUB_PATH:-}" ]; then
  if command -v cygpath >/dev/null; then
    cygpath -w "$bin" >> "$GITHUB_PATH"
  else
    echo "$bin" >> "$GITHUB_PATH"
  fi
  echo "SEVENZ_TURBO_7ZZ_REQUIRED=1" >> "$GITHUB_ENV"
fi
printf '%s\n7-Zip: %s\n' "$banner" "$bin/$tool"
