# Packaging: from mapped image to distributable recomp

`gba-pack` (crates/pack) turns one cartridge image into a portable,
platform-native game binary — a *recomp* in the N64-recomp sense: the
code is statically recompiled, everything else stays the user's own.

## The invariant

A package distributes **no proprietary data**. It pins the cartridge
image and the BIOS by SHA-256, and the produced binary **must not
work** until the end user supplies files hashing to exactly those
values (in-app first-run flow: pick ROM → pick BIOS → verify → play;
paths persist, hashes re-verify on every launch). This is not a
courtesy dialog — gameplay is unreachable without both. The image is
required at runtime regardless: translation covers code, but all data
reads come from the user's image through the bus, and the real BIOS
boots the same way `play --bios` does today.

## Inputs

1. **The image + BIOS** — the packager's own dumps, hash-verified
   against the pins before anything builds.
2. **A label map** — `gba-labels` TOML (docs/labels.md), normally
   produced by the gba-mapper workflow (the automated overnight
   disassembler, a sibling repository/submodule). The map is what
   makes a *packaged* recomp different from `recomp play`'s cache
   build: full function coverage up front instead of
   profile-plus-fallback. The packager reports coverage loudly; an
   unmapped package is a DEGRADED package.
3. **`pack.toml`** — the package description (below).

## pack.toml

```toml
[package]
name = "my-recomp"
version = "0.1.0"
platforms = ["macos", "windows", "linux"]   # the default trio

[image]
rom-sha256  = "<64 hex>"    # required — the gate
bios-sha256 = "<64 hex>"    # required — the gate

[labels]
file = "game.labels.toml"

[runtime]                    # which gba-recomp modules ship in the binary
menu = true                  # press-Escape in-game menu (see below)
enhanced-audio = true        # per-channel sinc + soft-clip + engine-HLE shadow
screen-sim = true            # panel simulation (crates/screen)
engine-hle = "auto"          # auto | off | m4a | gax | rdrv  (pin = skip discovery)
interpreter = true           # false = FULL RECOMP: no interpreter ships; a
                             # dispatch miss halts loudly, and the build
                             # refuses to package unless a soak run executed
                             # zero interpreter-fallback steps

[build]                      # build-time only — nothing here ships
compiler = "clang"           # optional: pin the C compiler used to translate.
                             # Default: a system cc/clang/gcc if present, else
                             # the bundled TinyCC. Pin a real optimizing
                             # compiler here for a shipped package. Any
                             # cc-style program/path; works on all platforms.

[output]
binary = true
c-source = false             # also emit the recompiled C tree with the
                             # label set's function names applied
                             # (sub_<addr> where unnamed)
```

Everything under `[runtime]` is borrowing from this repository: the
packaged binary links the same crates `play` uses (gba-core, screen,
input-config, the dispatch loop). Flexibility is the point — a recomp
author takes the modules they want and nothing else. The three
planned reference implementations will exercise exactly this surface,
and they live in **their own repositories**: nothing produced by the
packager — binaries, translated code, C trees — is ever bundled or
distributed in this repository. gba-recomp ships the toolkit only.

## The in-game menu

A new, optional runtime module (planned: `crates/menu`): press Escape
during play to get an overlay with
- resume / quit,
- audio: enhanced-audio toggle (the crossfaded engine-HLE machinery
  already makes this safe live),
- video: screen model, darken, temporal mode — the av.cfg surface,
- input: rebinding (keyboard + pad, the input-config surface),
- ROM/BIOS: re-run the file selection flow.

It renders into the existing presentation path (composited into the
frame buffer before present, so it works under both the wgpu presenter
and the CPU blit), identically in `recomp play` and in packaged
binaries; `[runtime] menu = false` gates it out (Escape then quits as
before). Audio/video changes apply **live** — screen model, darken,
response, grid, and the enhanced-audio toggle all take effect without
a relaunch (enhanced audio crossfades over ~40 ms; both paths stay
warm). Only the output gamut is restart-required (baked into the GPU
presenter) and is labeled so. The pad opener is the guide/Mode button;
keyboard Escape always works.

## Outputs

- **Binary** (default): per selected platform, a self-contained
  executable embedding the translated code, the selected runtime
  modules, and the two SHA pins. Cross-platform builds are
  cross-compilation problems and may initially require building on
  each target (CI recipe to follow with the reference
  implementations).
- **C source tree** (`c-source = true`): the emitted translation
  units, with functions named from the label map. This is the
  inspection/hacking surface. Like the binary, it is *derived from
  the image*; each recomp's own repository decides what it
  distributes — this repository never carries either.

## Full recomp (`interpreter = false`)

Interpreter fallback is bit-exact, so it is never a *correctness*
risk — but a "full recomp" should mean what it says. With the flag
off, three teeth engage:

1. **Build gate**: `gba-pack build` soak-runs the translation
   (default 3600 frames, `--soak` to change) and refuses to package
   unless **zero** interpreter steps executed. If the first soak finds
   gaps (typically computed jump-table targets, invisible to any
   static map), it records them as labels, rebuilds once, and re-runs
   the gate.
2. **Runtime trap**: the packaged binary halts loudly on a dispatch
   miss, printing the missing address as a label the packager can add
   — never a silent fidelity downgrade.
3. **No escape hatch**: `--interp` is rejected by full-recomp
   packages.

The honest caveat: a soak proves the *covered paths*, not all paths.
A player reaching genuinely unvisited code (different route, save
state) trips the trap. Packagers should soak with recorded input
covering real play, and ship updates as labels grow. Exhaustive
in-function seeding from a complete mapper boundary set (translate
every address inside every mapped function) is the planned hard
guarantee.

## Status

`gba-pack` validates (plan mode) and **builds** (`gba-pack build`):
translate with labels → soak gate → assemble `<out>/<name>/` holding
the runtime binary, `translation.<dylib|so|dll>` (host platform), the `recomp.pack.toml`
manifest (pins + runtime options, no image content), a README for the
end user, and `src-c/` when `c-source = true`. The runtime binary is
`recomp` itself: a manifest beside the executable switches it to
packaged behavior (bare launch plays the pinned title; ROM resolved
by content hash next to the exe, BIOS pin enforced, translation
loaded from the package). Still to land: the menu module,
cross-platform packaging (host-only today), engine-hle pin
enforcement at runtime, label-named functions in the C export.
