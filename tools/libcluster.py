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


# Engine recipes. Engine/middleware names
# are not game titles; fine to match and report.

# GAX v1 bannerless family: 32-byte library-function prefixes minted
# from the bit-matched decomp, plus a 4-way constant-table conjunction
# (16-byte prefixes of the driver LUTs) that survives recompiled
# builds where function bodies differ.
GAX1_FUNCS = [
    bytes.fromhex("70B5051C0E1C306803210840002800D0B4E070680840002800D0AFE0B0680840"),
    bytes.fromhex("F0B557464E464546E0B481B0051C8946171C98461949066888225200B0180268"),
]
GAX1_TABLES = [
    bytes.fromhex("0010F310F511061328145B15A016F917"),  # note ratios 2^(n/12)
    bytes.fromhex("00101A0F410E740DB20CFC0B500BAD0A"),  # inverse pitch
    bytes.fromhex("7B07820789079007"),                  # PSG period LUT
    bytes.fromhex("00606060404040408080808020202020"),  # wave volume LUT
]


def annotate(rom: bytes) -> list[str]:
    tags = []
    if m4a_detect(rom):
        tags.append("M4A")
    # GAX v2/v3: version banner.
    i = rom.find(b"GAX Sound Engine")
    if i >= 0:
        import re
        m = re.match(rb"[ v]*(\d\.\d+)", rom[i + 16 : i + 26])
        tags.append(f"GAX{m.group(1).decode() if m else ''}")
    elif sum(t in rom for t in GAX1_TABLES) == 4:
        # Bannerless GAX lineage: all four driver LUTs present.
        tags.append("GAX1" if any(f in rom for f in GAX1_FUNCS) else "GAXlin")
    # Krawall: CVS keyword qualified by library strings.
    j = rom.find(b"$Id: ")
    while j >= 0:
        end = rom.find(b"\x00", j, j + 160)
        blob = rom[j : end if end > 0 else j + 160]
        if b"Krawall" in blob or b"version.h" in blob or b"player.c,v" in blob:
            tags.append("Krawall")
            break
        j = rom.find(b"$Id: ", j + 5)
    # Rare in-house driver ships its diagnostics.
    if rom.count(b"AUDIO ERROR, ") >= 2:
        tags.append("Rare")
    if b"MusyX" in rom:
        tags.append("MusyX")
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
