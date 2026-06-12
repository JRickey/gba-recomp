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

[runtime]                    # which gba-lib modules ship in the binary
menu = true                  # press-Escape in-game menu (see below)
enhanced-audio = true        # per-channel sinc + soft-clip + engine-HLE shadow
screen-sim = true            # panel simulation (crates/screen)
engine-hle = "auto"          # auto | off | m4a | gax | rdrv  (pin = skip discovery)

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
distributed in this repository. gba-lib ships the toolkit only.

## The in-game menu

A new, optional runtime module (planned: `crates/menu`): press Escape
during play to get an overlay with
- resume / quit,
- audio: enhanced-audio toggle (the crossfaded engine-HLE machinery
  already makes this safe live),
- video: screen model, darken, temporal mode — the av.cfg surface,
- input: rebinding (keyboard + pad, the input-config surface),
- ROM/BIOS: re-run the file selection flow.

It renders into the existing presentation path (the screen-sim wgpu
surface over the play window), so it works identically in `recomp
play` and in packaged binaries; `[runtime] menu = false` builds it
out. The launcher keeps its own first-launch BIOS setup — the menu's
ROM/BIOS flow is the packaged binary's equivalent.

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

## Status

`gba-pack` currently validates the whole description (config schema,
SHA pins, input hashes, label presence) and prints the build plan.
The build steps land in order: translate-with-labels (exists today as
`recomp build` — needs a coverage report), runtime crate emission,
menu module, per-platform link, C-source export with names.
