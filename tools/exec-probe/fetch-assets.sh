#!/usr/bin/env bash
# Fetch the binaries the probe needs, for every ABI we might test on.
#
# Nothing here is committed - third-party artefacts, pinned at fetch time and
# recorded in assets/MANIFEST.txt.
#
#   1. An Alpine minirootfs   -> the musl guest binaries to test against.
#   2. Termux's prebuilt proot -> avoids an NDK build of proot just to answer a
#                                 yes/no question.
#   3. proot's shared libraries -> proot is NOT static. It needs libtalloc and
#                                  libandroid-shmem, and without them it dies with
#                                  "CANNOT LINK EXECUTABLE", which looks exactly
#                                  like the SELinux restriction we are measuring.
#
# Both ABIs are fetched. arm64-v8a is the real target; x86_64 lets the probe run
# natively on an emulator - and neither question it asks (does SELinux block
# app-data exec, can bionic's linker load a musl binary) is architecture-specific.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
assets="$here/AndroidApp/app/src/main/assets"
jnilibs_root="$here/AndroidApp/app/src/main/jniLibs"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

need() { command -v "$1" >/dev/null || { echo "missing required tool: $1" >&2; exit 1; }; }
need curl; need tar; need ar; need python3

mkdir -p "$assets"
manifest="$assets/MANIFEST.txt"
: > "$manifest"

alpine_branch="v3.21"
termux_pool="https://packages.termux.dev/apt/termux-main/pool/main"

# Replace a NUL-terminated string inside an ELF in place with a shorter one.
#
# Android's packager only extracts native libraries named lib*.so, so a file
# called libtalloc.so.2 never reaches the device. Renaming the file is not
# enough - the DT_NEEDED entry inside proot, and libtalloc's own SONAME, still
# say "libtalloc.so.2". Shortening the string in .dynstr and NUL-padding the
# remainder is safe: entries are NUL-separated, so the leftover bytes simply
# become an unreferenced empty string. (patchelf would do this too, but is not
# a dependency worth requiring here.)
elf_rename_string() {
  python3 - "$1" "$2" "$3" <<'PY'
import sys
path, old, new = sys.argv[1], sys.argv[2].encode(), sys.argv[3].encode()
assert len(new) <= len(old), "replacement must not be longer"
data = bytearray(open(path, "rb").read())
needle = old + b"\x00"
n = data.count(needle)
if n == 0:
    print(f"    note: {old.decode()} not present in {path}")
    sys.exit(0)
if n != 1:
    sys.exit(f"    refusing: {old.decode()} occurs {n} times in {path}")
i = data.index(needle)
data[i:i + len(needle)] = new + b"\x00" * (len(needle) - len(new))
open(path, "wb").write(data)
print(f"    patched {old.decode()} -> {new.decode()} in {path.split('/')[-1]}")
PY
}

# fetch_termux <pool-subdir> <package> <abi> -> extracts into $work/<pkg>-<abi>/
fetch_termux() {
  local sub="$1" pkg="$2" abi="$3"
  local deb
  deb="$(curl -fsSL "$termux_pool/$sub/$pkg/" \
    | grep -oE "${pkg}_[^\"<]*_${abi}\.deb" | sort -uV | tail -1)"
  [ -n "$deb" ] || { echo "no $pkg package for $abi" >&2; return 1; }
  local dest="$work/$pkg-$abi"
  rm -rf "$dest"; mkdir -p "$dest/root"
  curl -fsSL "$termux_pool/$sub/$pkg/$deb" -o "$dest/pkg.deb"
  ( cd "$dest" && ar x pkg.deb )
  tar -xf "$(ls "$dest"/data.tar.* | head -1)" -C "$dest/root"
  echo "$deb"
}

# android abi : alpine arch : termux arch
for triple in "arm64-v8a:aarch64:aarch64" "x86_64:x86_64:x86_64"; do
  abi="${triple%%:*}"; rest="${triple#*:}"
  alpine_arch="${rest%%:*}"; termux_arch="${rest##*:}"
  jnilibs="$jnilibs_root/$abi"
  mkdir -p "$jnilibs"
  echo "=============== $abi ==============="

  # ------------------------------------------------------------- alpine ----
  alpine_base="https://dl-cdn.alpinelinux.org/alpine/${alpine_branch}/releases/${alpine_arch}"
  alpine_file="$(curl -fsSL "$alpine_base/" \
    | grep -oE "alpine-minirootfs-[0-9.]+-${alpine_arch}\.tar\.gz" | sort -uV | tail -1)"
  [ -n "$alpine_file" ] || { echo "no Alpine minirootfs for $alpine_arch" >&2; exit 1; }
  echo "==> $alpine_file"
  curl -fsSL "$alpine_base/$alpine_file" -o "$work/alpine-${abi}.tar.gz"
  {
    echo "[$abi] alpine: $alpine_base/$alpine_file"
    echo "[$abi] alpine sha256: $(sha256sum "$work/alpine-${abi}.tar.gz" | cut -d' ' -f1)"
  } >> "$manifest"

  # Store as a PLAIN tar: AAPT2 silently gunzips any asset ending in .gz and
  # renames it, so a .tar.gz asset arrives as a .tar the app is not looking for -
  # which surfaces as a mysteriously SKIPPED check rather than an error.
  gunzip -c "$work/alpine-${abi}.tar.gz" > "$assets/alpine-minirootfs-${abi}.tar"

  # -------------------------------------------------------------- proot ----
  # Delegated: the app will need exactly the same libraries, and two copies of
  # this logic would drift.
  "$here/../fetch-proot.sh" "$jnilibs_root" "$abi" | sed 's/^/  /'
  echo "[$abi] proot + libs: via tools/fetch-proot.sh" >> "$manifest"

  echo "    jniLibs/$abi: $(ls "$jnilibs" | tr '\n' ' ')"
done

echo
echo "manifest:"; sed 's/^/    /' "$manifest"
