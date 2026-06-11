#!/usr/bin/env python3
"""Diff two 240x160 PPM frames in the 5-bit color domain.

Usage: ppmdiff.py a.ppm b.ppm [--rows]
Prints total differing pixels; with --rows, a per-scanline histogram.
Exit code 0 either way (it's a measurement, not a test).
"""
import sys


def load(path):
    with open(path, "rb") as f:
        data = f.read()
    # P6\n240 160\n255\n
    parts = data.split(b"\n", 3)
    assert parts[0] == b"P6", path
    w, h = map(int, parts[1].split())
    px = parts[3][: w * h * 3]
    return w, h, px


def main():
    a_path, b_path = sys.argv[1], sys.argv[2]
    rows = "--rows" in sys.argv
    w, h, a = load(a_path)
    w2, h2, b = load(b_path)
    assert (w, h) == (w2, h2)
    diff_total = 0
    row_counts = [0] * h
    for y in range(h):
        base = y * w * 3
        n = 0
        for i in range(base, base + w * 3, 3):
            if (
                a[i] >> 3 != b[i] >> 3
                or a[i + 1] >> 3 != b[i + 1] >> 3
                or a[i + 2] >> 3 != b[i + 2] >> 3
            ):
                n += 1
        row_counts[y] = n
        diff_total += n
    print(f"{diff_total} differing pixels / {w*h}")
    if rows and diff_total:
        for y, n in enumerate(row_counts):
            if n:
                print(f"  row {y:3d}: {n}")


if __name__ == "__main__":
    main()
