#!/usr/bin/env python3
"""Cluster ROMs by bit-identical shared code/data blobs.

Games link the same middleware (audio engines above all), and linked
library regions are frequently bit-identical across images. Method:

  1. hash 64-byte windows at 16-byte stride per image (low-entropy
     windows skipped: padding/fill);
  2. windows present in >= half the corpus are "background" (compiler
     runtime / SDK boilerplate) and are excluded;
  3. pairwise shared-window counts over the remainder form a
     similarity graph; connected components above a threshold are
     library-sharing clusters;
  4. clusters are annotated with known engine signatures (M4A pool
     shape, engine version strings) — unannotated clusters are
     candidates for manual identification.

Usage: libcluster.py <rom-dir> [--names name-map.txt] [--edge N]
"""
import argparse
import os
import struct
import sys

import numpy as np

WIN = 64
STRIDE = 16


def window_hashes(rom: bytes) -> np.ndarray:
    """Deduplicated 64-bit-ish hashes of content windows."""
    a = np.frombuffer(rom, dtype=np.uint8)
    n = (len(a) - WIN) // STRIDE + 1
    if n <= 0:
        return np.empty(0, dtype=np.uint64)
    idx = (np.arange(n)[:, None] * STRIDE + np.arange(WIN)[None, :])
    w = a[idx].astype(np.uint64)
    # Entropy filter: padding and fill have (near-)constant bytes.
    spread = w.max(axis=1) - w.min(axis=1)
    keep = spread > 8
    w = w[keep]
    # Polynomial mix per window (vectorized FNV-flavored).
    h = np.full(w.shape[0], 0xCBF29CE484222325, dtype=np.uint64)
    for k in range(WIN):
        h = (h ^ w[:, k]) * np.uint64(0x100000001B3)
    return np.unique(h)


def m4a_detect(rom: bytes) -> bool:
    """MP2K literal-pool shape (see gba-core mp2k.rs)."""
    needle = struct.pack("<II", 0x03007FF0, 0x68736D53)
    pos = 0
    while True:
        i = rom.find(needle, pos)
        if i < 0:
            return False
        if i % 4 == 0 and i + 20 <= len(rom):
            ptr, vc, pb = struct.unpack_from("<3I", rom, i + 8)
            if (ptr >> 24) in (2, 3) and vc == 0x04000006 and pb == 0x350:
                return True
        pos = i + 2


# Cheap engine strings (engine names are not game titles; fine to match).
STRINGS = {
    "GAX": [b"GAX Sound Engine"],
    "Krawall": [b"Krawall", b"$Id: kramWorker"],
    "MusyX": [b"MusyX"],
    "GHX": [b"GHX Sound Engine"],
}


def annotate(rom: bytes) -> list[str]:
    tags = []
    if m4a_detect(rom):
        tags.append("M4A")
    for name, needles in STRINGS.items():
        if any(rom.find(n) >= 0 for n in needles):
            tags.append(name)
    return tags


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("dir")
    ap.add_argument("--names", default=None)
    ap.add_argument("--edge", type=int, default=64,
                    help="min shared non-background windows for an edge")
    args = ap.parse_args()

    names = {}
    if args.names and os.path.exists(args.names):
        for ln in open(args.names):
            parts = ln.split(None, 1)
            if len(parts) == 2:
                names[parts[0]] = parts[1].strip()

    paths = sorted(
        os.path.join(args.dir, f)
        for f in os.listdir(args.dir)
        if f.endswith(".gba")
    )
    roms = []
    sets = []
    tags = []
    for p in paths:
        rom = open(p, "rb").read()
        roms.append(os.path.basename(p)[:-4])
        sets.append(window_hashes(rom))
        tags.append(annotate(rom))
        print(f"  hashed {os.path.basename(p)[:16]} "
              f"({len(sets[-1])} windows) {tags[-1]}", file=sys.stderr)

    # Background = windows in >= half the corpus (compiler runtime, SDK).
    allh = np.concatenate(sets)
    uniq, counts = np.unique(allh, return_counts=True)
    background = uniq[counts >= max(2, len(sets) // 2)]
    print(f"background windows excluded: {len(background)}", file=sys.stderr)
    sets = [np.setdiff1d(s, background, assume_unique=True) for s in sets]

    # Group windows by the EXACT set of images containing them: a large
    # family of windows with one identical image-set is one shared
    # library blob. No transitive chaining — version variants show up
    # as overlapping groups instead of merging.
    n = len(sets)
    hashes = np.concatenate(sets)
    owners = np.concatenate(
        [np.full(len(s), i, dtype=np.int32) for i, s in enumerate(sets)]
    )
    order = np.argsort(hashes, kind="stable")
    hashes = hashes[order]
    owners = owners[order]
    starts = np.flatnonzero(np.r_[True, hashes[1:] != hashes[:-1]])
    ends = np.r_[starts[1:], len(hashes)]
    groups: dict[bytes, int] = {}
    for a, b in zip(starts, ends):
        if b - a < 2:
            continue  # window unique to one image
        mask = np.zeros(n, dtype=bool)
        mask[owners[a:b]] = True
        groups[mask.tobytes()] = groups.get(mask.tobytes(), 0) + 1

    def label(i):
        nm = names.get(roms[i], roms[i][:16])[:40]
        tg = ",".join(tags[i]) or "?"
        return f"    [{tg:12s}] {nm}"

    MIN_WINDOWS = 192  # ~3 KB of bit-identical content
    big = sorted(
        ((cnt, m) for m, cnt in groups.items() if cnt >= MIN_WINDOWS),
        reverse=True,
    )
    for cnt, mbytes in big:
        mask = np.frombuffer(mbytes, dtype=bool)
        members = np.flatnonzero(mask)
        print(f"\nSHARED BLOB ~{cnt * STRIDE // 1024} KB across {len(members)} images:")
        for i in members:
            print(label(i))
    return 0


if __name__ == "__main__":
    sys.exit(main())
