#!/bin/sh
# Full-corpus differential sweep.
#
#   tools/full_sweep.sh [src] [dst] [frames] [jobs]
#
# Stage 1: extract every zipped image in src (default data/full) to
# dst (default data/full-sha) as <sha256>.gba, deduplicating by content
# hash and skipping BIOS images. Skipped entirely if dst is already
# populated (set SWEEP_REEXTRACT=1 to force).
# Stage 2: run `recomp verify` (interpreter vs recompiled, identical demo
# input) on each extracted image. PASS = framebuffer hashes bit-identical.
# Results append to out/full-sweep.log; images already in the log are
# skipped, so an interrupted sweep resumes where it stopped.
#
# Safety: jobs are niced, the recompiler emits bounded translation units
# (so cc memory stays modest), and a watchdog aborts the whole sweep if
# the kernel reports critical memory pressure.

set -u
src=${1:-data/full}
export SWEEP_DST=${2:-data/full-sha}
export SWEEP_FRAMES=${3:-1200}
jobs=${4:-4}
export SWEEP_BIN=./target/release/recomp
log=out/full-sweep.log

mkdir -p "$SWEEP_DST" out
touch "$log"

if [ -z "$(ls "$SWEEP_DST"/*.gba 2>/dev/null | head -1)" ] || [ "${SWEEP_REEXTRACT:-0}" = 1 ]; then
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
      if command -v sha256sum >/dev/null 2>&1; then
        sha=$(sha256sum "$tmp" | cut -d" " -f1)
      else
        sha=$(shasum -a 256 "$tmp" | cut -d" " -f1)
      fi
      if [ -e "$SWEEP_DST/$sha.gba" ]; then
        rm -f "$tmp"
      else
        mv "$tmp" "$SWEEP_DST/$sha.gba"
      fi
    ' sh
else
  echo "[stage 1] $SWEEP_DST already populated — skipping extraction"
fi
total=$(ls "$SWEEP_DST"/*.gba 2>/dev/null | wc -l | tr -d " ")

# Work list: everything not already verified in a previous (partial) run.
done_shas=$(mktemp)
pending=$(mktemp)
grep -oE '^(PASS|FAIL) [0-9a-f]{8}' "$log" | awk '{print $2}' | sort -u > "$done_shas"
for f in "$SWEEP_DST"/*.gba; do
  sha=$(basename "$f" .gba | cut -c1-8)
  grep -qx "$sha" "$done_shas" || echo "$f"
done > "$pending"
todo=$(wc -l < "$pending" | tr -d " ")
echo "[stage 2] $todo of $total images pending, $SWEEP_FRAMES frames, $jobs jobs"

# Watchdog: abort everything if the kernel reaches critical memory
# pressure, so a runaway can never take the machine down again.
# Darwin reports a pressure level directly; elsewhere derive one from
# /proc/meminfo MemAvailable (<5% critical, <15% warning).
mem_pressure_level() {
  lvl=$(sysctl -n kern.memorystatus_vm_pressure_level 2>/dev/null)
  if [ -n "$lvl" ]; then
    echo "$lvl"
  elif [ -r /proc/meminfo ]; then
    awk '/^MemTotal:/{t=$2} /^MemAvailable:/{a=$2}
         END{p=a*100/t; if(p<5) print 4; else if(p<15) print 2; else print 1}' /proc/meminfo
  else
    echo 1
  fi
}
(
  while :; do
    lvl=$(mem_pressure_level)
    if [ "$lvl" -ge 4 ]; then
      echo "WATCHDOG: critical memory pressure — aborting sweep" >> "$log"
      pkill -f "$SWEEP_BIN verify" 2>/dev/null
      pkill -f "cc -O1 -c -o out/" 2>/dev/null
      exit 1
    elif [ "$lvl" -ge 2 ]; then
      echo "WATCHDOG: memory pressure warning (level $lvl)" >> "$log"
    fi
    sleep 15
  done
) &
watchdog=$!

xargs -P "$jobs" -n 1 sh -c '
  f="$1"
  stem=$(basename "$f" .gba)
  sha=$(printf %.8s "$stem")
  r=$(nice -n 19 "$SWEEP_BIN" verify "$f" --frames "$SWEEP_FRAMES" 2>/dev/null | grep "^verify")
  rm -f "out/$stem.dylib" out/"$stem".*.c out/"$stem".*.o
  case "$r" in
    *" MATCH"*) echo "PASS $sha" ;;
    *)          echo "FAIL $sha: ${r:-no-output}" ;;
  esac
' sh < "$pending" >> "$log"

kill "$watchdog" 2>/dev/null
rm -f "$done_shas" "$pending"

pass=$(grep -c '^PASS' "$log")
fail=$(grep -c '^FAIL' "$log")
echo "full sweep: $pass pass, $fail fail (of $total)"
[ "$fail" -eq 0 ]
