#!/bin/sh
# Parallel differential-verification sweep over a directory of test images.
#
#   tools/sweep.sh [dir] [frames] [jobs]
#
# Each image runs `recomp verify` (interpreter vs recompiled, identical
# demo input) in its own process; results aggregate to stdout and
# out/sweep.log. PASS = framebuffer hashes bit-identical.

set -u
dir=${1:-data/baseline}
export SWEEP_FRAMES=${2:-1200}
jobs=${3:-$(sysctl -n hw.ncpu 2>/dev/null || nproc || echo 8)}
export SWEEP_BIN=./target/release/recomp

mkdir -p out
: > out/sweep.log

ls "$dir"/*.gba | xargs -P "$jobs" -n 1 sh -c '
  f="$1"
  sha=$(basename "$f" .gba | cut -c1-8)
  r=$("$SWEEP_BIN" verify "$f" --frames "$SWEEP_FRAMES" 2>/dev/null | grep "^verify")
  case "$r" in
    *" MATCH"*) echo "PASS $sha" ;;
    *)          echo "FAIL $sha: ${r:-no-output}" ;;
  esac
' sh | tee out/sweep.log

pass=$(grep -c '^PASS' out/sweep.log)
fail=$(grep -c '^FAIL' out/sweep.log)
echo "sweep: $pass pass, $fail fail"
[ "$fail" -eq 0 ]
