#!/usr/bin/env bash
# Build a self-contained, dynamically-loadable TinyCC and stage it as a
# `tcc/` directory that the recompiler resolves beside its own binary
# (recomp_core::tcc::lib_dir). This is what lets a local "build to play"
# work on a machine with no system C compiler installed — recomp falls back
# to this bundled TinyCC.
#
# libtcc is LGPL-2.1. We build it as a SHARED library and load it at runtime
# (libloading), never linking it statically, so the copyleft stays contained
# and the compiler's output is unaffected. The shipped license notice lives
# in THIRD-PARTY.md.
#
# Usage:  scripts/build-tcc.sh <out-dir>      # creates <out-dir>/tcc/
# Pin the source with TCC_REF (commit/tag); default tracks a known-good rev.
# Cross-slice macOS builds can set TCC_CPU, TCC_CC, TCC_EXTRA_CFLAGS,
# TCC_EXTRA_LDFLAGS, TCC_SYSINCLUDEPATHS, TCC_LIBPATHS, and TCC_CRTPREFIX;
# these are passed to TinyCC's configure as separate args.
# Unix only (macOS, Linux). Windows builds libtcc.dll via TinyCC's own win32
# build — see the note at the bottom; package-release.sh stages a prebuilt
# tcc/ via $TCC_DIR on every platform, so this script is the Unix producer.
set -euo pipefail

OUT="${1:?usage: build-tcc.sh <out-dir>}"
TCC_REPO="${TCC_REPO:-https://github.com/TinyCC/tinycc}"
# Pin for reproducibility (TinyCC 0.9.28rc, verified to build a working
# arm64/x86_64 shared libtcc and compile our emitted C bit-identically).
TCC_REF="${TCC_REF:-a338258d309c888bde96b2d1f206299231a54ddf}"

case "$(uname -s)" in
  Darwin) SHARED="libtcc.dylib" ;;
  Linux)  SHARED="libtcc.so" ;;
  *) echo "build-tcc.sh: unsupported OS $(uname -s); Windows uses the win32 build (see header)" >&2; exit 2 ;;
esac

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

echo "build-tcc: cloning TinyCC @ ${TCC_REF}"
git clone "$TCC_REPO" "$WORK/src" >/dev/null 2>&1
git -C "$WORK/src" checkout -q "$TCC_REF"

echo "build-tcc: configure --disable-static && make"
( cd "$WORK/src"
  args=(--prefix="$WORK/stage" --disable-static)
  [ -n "${TCC_CPU:-}" ] && args+=(--cpu="$TCC_CPU")
  [ -n "${TCC_CC:-}" ] && args+=(--cc="$TCC_CC")
  [ -n "${TCC_EXTRA_CFLAGS:-}" ] && args+=(--extra-cflags="$TCC_EXTRA_CFLAGS")
  [ -n "${TCC_EXTRA_LDFLAGS:-}" ] && args+=(--extra-ldflags="$TCC_EXTRA_LDFLAGS")
  if [ -n "${TCC_SYSINCLUDEPATHS:-}" ]; then
    args+=(--sysincludepaths="$WORK/src/include:$TCC_SYSINCLUDEPATHS")
  fi
  [ -n "${TCC_LIBPATHS:-}" ] && args+=(--libpaths="$TCC_LIBPATHS")
  [ -n "${TCC_CRTPREFIX:-}" ] && args+=(--crtprefix="$TCC_CRTPREFIX")
  ./configure "${args[@]}" >/dev/null
  make -j"$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)" >/dev/null
  make install >/dev/null )

# Flatten the installed prefix into the layout tcc::lib_dir() expects:
#   tcc/<SHARED>   tcc/libtcc1.a   tcc/include/stdint.h
DEST="$OUT/tcc"
rm -rf "$DEST"; mkdir -p "$DEST/include"
# The shared lib may be versioned on Linux (libtcc.so.1.0); normalize the name.
cp "$(ls "$WORK"/stage/lib/${SHARED}* 2>/dev/null | head -1)" "$DEST/$SHARED"
cp "$WORK"/stage/lib/tcc/libtcc1.a "$DEST/"
# LGPL-2.1 requires the library's license to travel with the binary.
cp "$WORK"/src/COPYING "$DEST/COPYING" 2>/dev/null || true

# TinyCC bundles stddef.h/stdarg.h but NOT stdint.h, which the emitted C
# needs. Ship a minimal shim so the toolchain is fully self-contained — no
# system SDK/headers required on the end user's machine.
cat > "$DEST/include/stdint.h" <<'SHIM'
#ifndef _RECOMP_STDINT_H
#define _RECOMP_STDINT_H
typedef unsigned char      uint8_t;   typedef signed char    int8_t;
typedef unsigned short     uint16_t;  typedef short          int16_t;
typedef unsigned int       uint32_t;  typedef int            int32_t;
typedef unsigned long long uint64_t;  typedef long long      int64_t;
typedef unsigned long      uintptr_t; typedef long           intptr_t;
#endif
SHIM

echo "build-tcc: staged ->"
ls -la "$DEST"
echo "build-tcc: done ($DEST)"

# --- Windows note --------------------------------------------------------
# On Windows, build libtcc.dll with TinyCC's own win32 build (e.g. under
# MSYS2/mingw: `cd win32 && ../configure && make`), then assemble the same
# tcc/ layout: tcc/libtcc.dll, tcc/libtcc1.a, tcc/include/. TinyCC's win32
# headers DO include stdint.h, so the shim above is optional there. Set
# $TCC_DIR to that tcc/ when invoking package-release.sh.
