#!/bin/sh
# Full-corpus differential sweep.
#
#   tools/full_sweep.sh [src] [dst] [frames] [jobs]
#
# Stage 1: extract every zipped image in src (default data/full) to
# dst (default data/full-sha) as <sha256>.gba, deduplicating by content
# hash and skipping BIOS images.
# Stage 2: run `recomp verify` (interpreter vs recompiled, identical demo
# input) on each extracted image. PASS = framebuffer hashes bit-identical.
# Per-image build artifacts (out/<sha>.c, out/<sha>.dylib) are removed as
# each image finishes. Results aggregate to out/full-sweep.log.

set -u
src=${1:-data/full}
export SWEEP_DST=${2:-data/full-sha}
export SWEEP_FRAMES=${3:-1200}
jobs=${4:-$(sysctl -n hw.ncpu 2>/dev/null || nproc || echo 8)}
export SWEEP_BIN=./target/release/recomp

mkdir -p "$SWEEP_DST" out

echo "[stage 1] extracting images from $src -> $SWEEP_DST"
find "$src"/ -name '*.zip' ! -name '\[BIOS\]*' -print0 |
  xargs -0 -P "$jobs" -n 1 sh -c '
    zip="$1"
    entry=$(zipinfo -1 "$zip" 2>/dev/null | grep -i "\.gba$" | head -1)
    [ -n "$entry" ] || exit 0
    tmp=$(mktemp "$SWEEP_DST/.extract.XXXXXX") || exit 0
    if ! unzip -p "$zip" "$entry" > "$tmp" 2>/dev/null; then
      rm -f "$tmp"; exit 0
    fi
    sha=$(shasum -a 256 "$tmp" | cut -d" " -f1)
    if [ -e "$SWEEP_DST/$sha.gba" ]; then
      rm -f "$tmp"
    else
      mv "$tmp" "$SWEEP_DST/$sha.gba"
    fi
  ' sh
total=$(ls "$SWEEP_DST"/*.gba 2>/dev/null | wc -l | tr -d " ")
echo "[stage 1] done: $total unique images"

echo "[stage 2] differential verify, $SWEEP_FRAMES frames, $jobs jobs"
: > out/full-sweep.log
ls "$SWEEP_DST"/*.gba | xargs -P "$jobs" -n 1 sh -c '
  f="$1"
  stem=$(basename "$f" .gba)
  sha=$(printf %.8s "$stem")
  r=$("$SWEEP_BIN" verify "$f" --frames "$SWEEP_FRAMES" 2>/dev/null | grep "^verify")
  rm -f "out/$stem.c" "out/$stem.dylib"
  case "$r" in
    *" MATCH"*) echo "PASS $sha" ;;
    *)          echo "FAIL $sha: ${r:-no-output}" ;;
  esac
' sh >> out/full-sweep.log

pass=$(grep -c '^PASS' out/full-sweep.log)
fail=$(grep -c '^FAIL' out/full-sweep.log)
echo "full sweep: $pass pass, $fail fail (of $total)"
[ "$fail" -eq 0 ]
