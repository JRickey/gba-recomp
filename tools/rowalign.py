#!/usr/bin/env python3
"""For each differing scanline, find the (dy, dx) shift that best maps
our row onto the reference. Reveals one-line-late or off-by-N-pixel
application of per-scanline register effects.

Usage: rowalign.py ours.ppm ref.ppm [first_row last_row]
"""
import sys


def load(path):
    with open(path, "rb") as f:
        data = f.read()
    parts = data.split(b"\n", 3)
    w, h = map(int, parts[1].split())
    px = parts[3][: w * h * 3]
    # quantize to 5-bit per channel, pack rows as lists of tuples
    rows = []
    for y in range(h):
        base = y * w * 3
        rows.append(
            [
                (px[i] >> 3, px[i + 1] >> 3, px[i + 2] >> 3)
                for i in range(base, base + w * 3, 3)
            ]
        )
    return w, h, rows


def rowdiff(a, b, dx):
    # compare a[x] vs b[x+dx] over the overlap
    w = len(a)
    n = 0
    tot = 0
    for x in range(max(0, -dx), min(w, w - dx)):
        tot += 1
        if a[x] != b[x + dx]:
            n += 1
    return n, tot


def main():
    ours_p, ref_p = sys.argv[1], sys.argv[2]
    y0 = int(sys.argv[3]) if len(sys.argv) > 3 else 0
    y1 = int(sys.argv[4]) if len(sys.argv) > 4 else 159
    w, h, ours = load(ours_p)
    _, _, ref = load(ref_p)
    for y in range(y0, y1 + 1):
        base, tot = rowdiff(ours[y], ref[y], 0)
        if base == 0:
            continue
        best = (base, 0, 0)
        for dy in (-2, -1, 0, 1, 2):
            yy = y + dy
            if not (0 <= yy < h):
                continue
            for dx in range(-12, 13):
                n, _ = rowdiff(ours[y], ref[yy], dx)
                if n < best[0]:
                    best = (n, dy, dx)
        print(
            f"row {y:3d}: diff {base:3d}  best dy={best[1]:+d} dx={best[2]:+d} -> residual {best[0]}"
        )


if __name__ == "__main__":
    main()
