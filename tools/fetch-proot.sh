#!/usr/bin/env bash
# Populate a jniLibs tree with proot and everything it needs to actually start.
#
#   usage: tools/fetch-proot.sh <jniLibs-root> [abi ...]     (default: both ABIs)
#
# Shared by tools/exec-probe and by the app itself, because getting this subtly
# wrong costs a debugging session: proot that starts and immediately dies with
# "CANNOT LINK EXECUTABLE" is indistinguishable, at a glance, from the SELinux
# restriction the project is built around.
#
# What lands in <jniLibs-root>/<abi>/:
#   libproot.so          proot itself
#   libproot-loader.so   the loader ELF proot maps guest binaries with
#   libtalloc.so         renamed from libtalloc.so.2 (see below)
#   libandroid-shmem.so
#
# Two Android rules drive the shape of this:
#   1. Only files named lib*.so are unpacked from an APK, so libtalloc.so.2
#      would never reach the device. Renaming the file is not enough - proot's
#      DT_NEEDED and libtalloc's own SONAME still name the old string, so both
#      are patched.
#   2. The native library directory is the one place an app may execute from at
#      any targetSdkVersion. Everything executable has to live here.
set -euo pipefail

out="${1:?usage: fetch-proot.sh <jniLibs-root> [abi ...]}"
shift || true
abis=("$@")
[ ${#abis[@]} -eq 0 ] && abis=(arm64-v8a x86_64)

need() { command -v "$1" >/dev/null || { echo "missing required tool: $1" >&2; exit 1; }; }
need curl; need tar; need ar; need python3

pool="https://packages.termux.dev/apt/termux-main/pool/main"
work="$(mktemp -d)"; trap 'rm -rf "$work"' EXIT

# Shorten a NUL-terminated string inside an ELF, in place.
#
# Entries in .dynstr are NUL-separated, so writing a shorter string and padding
# the remainder with NULs leaves an unreferenced empty string - harmless.
# patchelf would do this too, but is not worth a hard dependency.
elf_rename() {
  python3 - "$1" "$2" "$3" <<'PY'
import sys
path, old, new = sys.argv[1], sys.argv[2].encode(), sys.argv[3].encode()
assert len(new) <= len(old)
data = bytearray(open(path, "rb").read())
needle = old + b"\x00"
n = data.count(needle)
if n == 0: sys.exit(0)
if n != 1: sys.exit(f"refusing: {old.decode()} occurs {n}x in {path}")
i = data.index(needle)
data[i:i+len(needle)] = new + b"\x00" * (len(needle) - len(new))
open(path, "wb").write(data)
PY
}

fetch() { # <pool-subdir> <package> <termux-arch> -> extracts to $work/<pkg>-<arch>/root
  local sub="$1" pkg="$2" arch="$3" deb dest
  deb="$(curl -fsSL "$pool/$sub/$pkg/" | grep -oE "${pkg}_[^\"<]*_${arch}\.deb" | sort -uV | tail -1)"
  [ -n "$deb" ] || { echo "no $pkg for $arch" >&2; return 1; }
  dest="$work/$pkg-$arch"; rm -rf "$dest"; mkdir -p "$dest/root"
  curl -fsSL "$pool/$sub/$pkg/$deb" -o "$dest/pkg.deb"
  ( cd "$dest" && ar x pkg.deb )
  tar -xf "$(ls "$dest"/data.tar.* | head -1)" -C "$dest/root"
  echo "$deb"
}

for abi in "${abis[@]}"; do
  case "$abi" in
    arm64-v8a) arch=aarch64 ;;
    x86_64)    arch=x86_64 ;;
    *) echo "unsupported abi: $abi" >&2; exit 1 ;;
  esac
  dir="$out/$abi"; mkdir -p "$dir"
  echo "=== $abi ==="

  deb="$(fetch p proot "$arch")";                   echo "    $deb"
  src="$work/proot-$arch/root"
  install -m 0755 "$(find "$src" -type f -name proot | head -1)" "$dir/libproot.so"
  loader="$(find "$src" -type f -name loader | head -1)"
  if [ -n "$loader" ]; then
    install -m 0755 "$loader" "$dir/libproot-loader.so"
  else
    echo "    WARNING: no proot loader found; proot cannot map guest binaries" >&2
  fi

  deb="$(fetch libt libtalloc "$arch")";            echo "    $deb"
  install -m 0755 "$(find "$work/libtalloc-$arch/root" -name 'libtalloc.so.2*' -type f | head -1)" \
    "$dir/libtalloc.so"

  deb="$(fetch liba libandroid-shmem "$arch")";     echo "    $deb"
  install -m 0755 "$(find "$work/libandroid-shmem-$arch/root" -name 'libandroid-shmem.so' -type f | head -1)" \
    "$dir/libandroid-shmem.so"

  elf_rename "$dir/libproot.so"  "libtalloc.so.2" "libtalloc.so"
  elf_rename "$dir/libtalloc.so" "libtalloc.so.2" "libtalloc.so"
  echo "    -> $(ls "$dir" | tr '\n' ' ')"
done
