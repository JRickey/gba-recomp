#!/usr/bin/env bash
# Windows producer for the bundled TinyCC fallback compiler — the counterpart
# to scripts/build-tcc.sh (which is the macOS/Linux producer; that script is
# left untouched). Builds a self-contained, dynamically-loadable libtcc and
# stages it as a `tcc/` directory the recompiler resolves beside its own
# binary (recomp_core::tcc::lib_dir), so a local "build to play" works on a
# machine with no system C compiler installed.
#
# Run inside an MSYS2 *mingw64* shell (uname -s = MINGW64_NT-...): it needs
# the mingw-w64 gcc toolchain + make. On a GitHub runner that is the
# `msys2/setup-msys2` action with `install: mingw-w64-x86_64-gcc make git`.
#
# Why a separate script (not a branch in build-tcc.sh): the Windows install
# layout is genuinely different from Unix. TinyCC on Windows expects, relative
# to the lib path passed to `tcc_set_lib_path`, a `lib/` holding libtcc1.a and
# the `*.def` import descriptors, and an `include/` holding the full header
# set (its stdint.h pulls in _mingw.h, so a flattened single-header staging
# does NOT work here). We therefore preserve TinyCC's native layout verbatim
# instead of flattening it the way the Unix script does.
#
# libtcc is LGPL-2.1. We build it as a SHARED library (libtcc.dll) and load it
# at runtime (libloading), never linking it statically, so the copyleft stays
# contained and the compiler's output is unaffected. The shipped license
# notice lives in THIRD-PARTY.md; the full text travels as tcc/COPYING.
#
# Usage:  scripts/build-tcc-windows.sh <out-dir>      # creates <out-dir>/tcc/
# Pin the source with TCC_REF (commit/tag); default matches build-tcc.sh.
set -euo pipefail

OUT="${1:?usage: build-tcc-windows.sh <out-dir>}"
TCC_REPO="${TCC_REPO:-https://github.com/TinyCC/tinycc}"
# Same pin as scripts/build-tcc.sh so all platforms ship the same TinyCC.
TCC_REF="${TCC_REF:-a338258d309c888bde96b2d1f206299231a54ddf}"

case "$(uname -s)" in
  MINGW64*|MSYS*|MINGW*) : ;;
  *) echo "build-tcc-windows.sh: run inside an MSYS2 mingw64 shell (uname is '$(uname -s)'); use scripts/build-tcc.sh on macOS/Linux" >&2; exit 2 ;;
esac

command -v gcc  >/dev/null || { echo "build-tcc-windows.sh: gcc not found — install mingw-w64-x86_64-gcc" >&2; exit 2; }
command -v make >/dev/null || { echo "build-tcc-windows.sh: make not found — install make" >&2; exit 2; }

# Everything (clone, build temps, install prefix) lives under one work dir on
# a writable drive. TinyCC's build shells out to gcc, which writes temp files
# via the Windows temp dir; if TMP/TEMP are unset they resolve to C:\Windows\
# (not writable) and the build fails. Pin them to a writable path in Windows
# (drive-letter) form — mingw gcc does not understand /tmp-style POSIX paths.
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/tmp"
WIN_TMP="$(cygpath -w "$WORK/tmp")"
export TMP="$WIN_TMP" TEMP="$WIN_TMP" TMPDIR="$WIN_TMP"

echo "build-tcc-windows: cloning TinyCC @ ${TCC_REF}"
git clone "$TCC_REPO" "$WORK/src" >/dev/null 2>&1
git -C "$WORK/src" checkout -q "$TCC_REF"

echo "build-tcc-windows: configure (auto-detects WIN32 under mingw) && make"
( cd "$WORK/src"
  ./configure --prefix="$WORK/stage" >/dev/null
  make -j"$(nproc 2>/dev/null || echo 4)" >/dev/null
  make install >/dev/null )

# Stage TinyCC's native Windows layout verbatim:
#   tcc/libtcc.dll              the compiler, loaded at runtime
#   tcc/lib/{libtcc1.a,*.def}   runtime helpers + Windows import descriptors
#   tcc/include/                full headers (stdint.h -> _mingw.h chain)
DEST="$OUT/tcc"
rm -rf "$DEST"; mkdir -p "$DEST"
cp "$WORK/stage/libtcc.dll" "$DEST/libtcc.dll"
cp -R "$WORK/stage/lib"     "$DEST/lib"
cp -R "$WORK/stage/include" "$DEST/include"
# LGPL-2.1 requires the library's license to travel with the binary.
cp "$WORK/src/COPYING" "$DEST/COPYING" 2>/dev/null || true

# Sanity: the three pieces the runtime relies on must be present.
test -f "$DEST/libtcc.dll"        || { echo "build-tcc-windows: libtcc.dll missing" >&2; exit 1; }
test -f "$DEST/lib/libtcc1.a"     || { echo "build-tcc-windows: lib/libtcc1.a missing" >&2; exit 1; }
test -f "$DEST/include/stdint.h"  || { echo "build-tcc-windows: include/stdint.h missing" >&2; exit 1; }

echo "build-tcc-windows: staged ->"
ls -la "$DEST"
echo "build-tcc-windows: done ($DEST)"
