#!/usr/bin/env bash
# Stage and archive the two release bundles for one target:
#
#   gba-recomp-cli      the static-recompiler toolkit (recomp + gba-pack)
#                       for building and packaging recomps
#   gba-recomp-runtime  the player (gba-launcher + the recomp it spawns)
#                       for end users who just want to play
#
# `recomp` ships in both because it is both the build CLI and the play
# runtime. Every bundle carries the dual license and the third-party
# notice. Portable: unzip and run, no install step.
#
# Runs under bash on all three CI runners (macOS, Linux, and Windows via
# git-bash). Binaries must already be built under $BINDIR — and on macOS
# already lipo'd to a universal binary and ad-hoc signed (arm64 refuses
# to run unsigned). Env in: VERSION, TARGET, BINDIR, OUTDIR, EXE,
# RUNNER_OS.
set -euo pipefail

: "${VERSION:?}" "${TARGET:?}" "${BINDIR:?}" "${OUTDIR:?}"
EXE="${EXE:-}"

mkdir -p "$OUTDIR" stage

carry_licenses() { cp LICENSE-MIT LICENSE-APACHE THIRD-PARTY.md "$1"/; }

# $1 = bundle short name, rest = binaries to include. Echoes the staged dir.
stage_bundle() {
  local short="$1"; shift
  local dir="stage/${short}-${VERSION}-${TARGET}"
  rm -rf "$dir"; mkdir -p "$dir"
  for b in "$@"; do cp "$BINDIR/${b}${EXE}" "$dir"/; done
  carry_licenses "$dir"
  printf '%s' "$dir"
}

# $1 = staged dir
archive() {
  local dir="$1" base
  base="$(basename "$dir")"
  if [ "${RUNNER_OS:-}" = "Windows" ]; then
    (cd stage && 7z a -tzip -bso0 "$(pwd)/../$OUTDIR/${base}.zip" "${base}") >/dev/null
    echo "${OUTDIR}/${base}.zip"
  else
    tar -C stage -czf "${OUTDIR}/${base}.tar.gz" "${base}"
    echo "${OUTDIR}/${base}.tar.gz"
  fi
}

# --- CLI toolkit -----------------------------------------------------
cli="$(stage_bundle gba-recomp-cli recomp gba-pack)"
cp README.md BUILDING.md "$cli"/
mkdir -p "$cli/docs"
cp docs/labels.md docs/packaging.md "$cli/docs"/
cat > "$cli/README-CLI.txt" <<EOF
gba-recomp CLI toolkit ${VERSION} (${TARGET})

  recomp     static recompiler + play runtime (build / runc / verify /
             labels / play). See BUILDING.md and docs/.
  gba-pack   package a mapped image into a distributable recomp.
             See docs/packaging.md.

Licensed MIT OR Apache-2.0 (LICENSE-MIT / LICENSE-APACHE). Third-party
components are listed in THIRD-PARTY.md. This bundle contains no game or
BIOS data; those are supplied by you at runtime and verified by hash.
EOF

# --- Player runtime --------------------------------------------------
rt="$(stage_bundle gba-recomp-runtime gba-launcher recomp)"
cat > "$rt/README-RUNTIME.txt" <<EOF
gba-recomp player runtime ${VERSION} (${TARGET})

  gba-launcher   the player UI (cartridge select, input, A/V). It spawns
                 'recomp' (kept beside it) to play.
  recomp         the play runtime.

Run gba-launcher${EXE}. Keep the two files together.

macOS: the binaries are ad-hoc signed (required for Apple Silicon) but
not notarized, so a download is quarantined. If macOS refuses to open
them, clear the quarantine flag once:

    xattr -dr com.apple.quarantine .

Licensed MIT OR Apache-2.0 (LICENSE-MIT / LICENSE-APACHE). Third-party
components (including the bundled controller database) are listed in
THIRD-PARTY.md. No game or BIOS data is included.
EOF

echo "staged:"
archive "$cli"
archive "$rt"
