# Labels: runtime-discovered translation coverage

Static analysis plus a profiling boot cannot reach everything a game
will ever execute: computed-branch targets, handlers installed at
runtime, and RAM-resident engines that only run deep into a session.
Whenever the runtime falls back to the interpreter, it knows exactly
which entry point the translation was missing. Labels close that loop:
the runtime records the misses, and the next translation covers them.

## Lifecycle

```
play with --record-labels          (or runc, for headless soak runs)
        │  fallback entry points accumulate, union-merged per image
        ▼
<config>/gba-recomp/labels/<sha256>.labels      (+ <sha256>.iwram)
        │  loaded automatically by every build of that image
        ▼
next launch: cache key changed → one-time retranslation → coverage
```

No manual step beyond the flag: the label set's digest is part of the
play translation-cache key, so a grown set retranslates automatically.
Sessions accumulate — the file only ever gains entries.

## File format

Plain text, line-based, designed to be union-merged and diffed:

```
gba-labels v1
rom-sha256 <64 hex digits>
rom 080f3a50 t
iwram 03000198 a
ewram 02000420 a
```

- The header pins the file to one image by content hash; a file for a
  different image is refused.
- `rom <hexaddr> a|t` — a ROM entry point (`a` = ARM, `t` = Thumb).
  These become analyzer seeds. They are *hints, not trusted input*: the
  translation derives from the image's own bytes, so a wrong or
  malicious label can at worst translate code nothing ever jumps to.
- `iwram <hexaddr> a|t` — an entry point in RAM-resident code. The
  address is portable; the *content* it refers to is not part of the
  file (see below).
- `ewram ...` — reserved. Recorded for forward compatibility, counted
  and skipped by today's build.
- Unknown or malformed lines are skipped, never fatal. Files merge by
  set union, so concatenating knowledge from many sessions or many
  people is safe.

Label files contain addresses and hashes only — **never bytes from the
image**. They are safe to publish and share.

## RAM-resident code and the local snapshot

Translating an `iwram` entry needs the code bytes at that address,
which belong to the image and therefore cannot travel in the label
file. Instead, the recorder captures a machine-local snapshot the
moment each new IWRAM entry point is discovered (that code is
certainly live right then — an end-of-session snapshot could miss an
overlay that was swapped out). It is stored next to the accumulator as
`<sha256>.iwram` (32 KB image + per-byte validity mask) and overlaid
onto the profiling snapshot at build time.

Execution stays correct regardless: every RAM-translated block runs
behind a whole-block content guard, so if the game later rewrites that
memory, the stale translation rejects itself and the interpreter takes
over for exactly those entries.

If you import a shared label file with `iwram` entries, they activate
after one local `--record-labels` session captures their content.

## Tooling

```sh
recomp labels show   game.gba                 # counts, snapshot, cache key
recomp labels import game.gba shared.labels   # union into the accumulator
recomp labels export game.gba [out.labels]    # shareable file (addresses only)
```

`export` is how you publish: the output is the merged set for that
image, named by convention `<rom>.labels`, which any copy of the
toolkit picks up automatically when it sits next to the image.

## What still falls back, by design

- `ewram` entries: recorded but not translated. EWRAM code is rare,
  measured correct-but-not-faster under guards, and waits on
  write-watch invalidation or block chaining to become a win.
- RAM code whose content guard fails (a swapped overlay): correctness
  first — those entries interpret until a future multi-overlay scheme
  keys several guarded translations to one address.
- A handful of runtime-generated stubs whose bytes exist nowhere
  stable. These are typically a negligible share of execution.

The fallback census (`RECOMP_TRACE_FALLBACK=1` on play/runc) is the
measurement tool behind all of this: per-region distinct-entry and hit
counts, hottest entries first.
