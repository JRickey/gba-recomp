# GBA-Recomp

A rust based, static recompiling toolkit and runtime for the GBA. Translates cartridge ROM images ahead of time into native executables. The runtime library provides the hardware model (PPU, APU, DMA/timers/IRQ, saves, input, RTC).

## Author's Notes

The GBA was my childhood system. It was the first gaming console I ever owned. Because the DS came after and had backwards compatibility, I kept playing some GBA games for a long time. The nature of the console, it being designed as a portable SNES, also sparked my love for retro gaming. I played many SNES classics for the first time on their GBA versions.

This project is a love letter to the system and to the community. The goal is to provide the most faithful, accurate, performant experience for the GBA, then to take it beyond the stock experience, in ways only a static recompilation could. I cannot ensure 0% interpreter fallback for all titles. This project aims to be a solid out of the box experience, with rich libraries and functionality for those who wish to make title specific static recompilations from this toolkit.

This project is in early development. The interpreter tier is complete, the static recompiler pipeline works end to end, each match bit for bit across the test corpus. Full testing has not been completed.

This project is an AI-assisted project. I believe that software should be open source, free, and abundant. If something does not exist, I take that as a challenge to make it exist, and I will use any tools available to do that. Copyright, licenses, and attribution must be respected as a legal requirement and as principle for sustainable open source development. For details on AI-assisted development regarding copyright, licensing, and clean-rooms, see [Legal](#legal).

## Design at a glance

- **Translate everything; resolve the indirect branches at runtime** via a
  guest-address → native-function table (ARM/Thumb mode-aware).
- **Rust tool + Rust runtime (C ABI) + emitted C11** — generated code compiles
  with any C compiler; no just-in-time compilation.
- **Fallback interpreter** for RAM-resident/self-modifying code; doubles as the
  differential-testing oracle. Kept for stability, not as intended runtime
  behavior. Interpreter fallback is meant to be kept to a minimum within
  reasonable development limits.
- **Cycle-count accuracy**: per-block cycle sums computed at recompile time,
  event scheduler checked at block edges, MMIO accesses as catch-up sync points.
- **Netplay**: all instances simulated on every peer, only controller inputs
  cross the network (rollback/lockstep).

## Enhanced Features

Two features included provide an enhanced out of box experience for this
toolkit and runtime. They are what justify static recompilation over emulation
and already cover a wide net of the console's library. Both are opt-in; with
them off, output is exactly what the hardware produced. The provenance of
everything below is covered once, in [Legal](#legal).

### Audio

- **Faithful by default.** The stock path reproduces the hardware mix
  sample-for-sample — the original Direct Sound FIFO and PSG chain, zero-order
  held at its true rate. It is bit-exact and serves as the differential oracle
  for everything else.
- **Enhanced output path** (opt-in): band-limited (windowed-sinc) resampling, a
  DC blocker, and a soft-clip knee. The same notes the hardware played, without
  the console speaker's aliasing and rail clipping — tuned for modern speakers
  and headphones.
- **High-level engine mixing** — the static-recompilation differentiator. The
  audio middleware a title links (the SDK MP2K/M4A family, the GAX lineage, and
  others) is identified by byte signature; a *shadow mixer* then re-renders that
  engine's own per-voice state in floating point, free of the hardware mixer's
  8-bit requantization and low mix-rate ceiling — which is what handicaps the
  GBA's audio today.
- **Proven before it is heard.** The shadow runs alongside the real mixer and is
  continuously cross-checked against the hardware's canon stream. It substitutes
  only after a fully passing window, auto-calibrates its gain, and reverts loudly
  (printing `DEGRADED` to log) the instant it stops matching — never a silent
  guess. Like the interpreter, this is not desirable and intended behavior, it
  exists as a signal for improvement.

### Video

- **Screen simulation.** Per-revision panel color, reproducing what each physical
  screen did to the 15-bit colors developers authored: the launch reflective
  panel, its frontlit successor, and the late near-sRGB backlit revision — each
  built from measured display colorimetry — plus a *classic* gamma-4.0 model for
  the look a decade of emulators standardized on. Raw, untouched output is always
  available.
- **Temporal response.** Optional reproduction of LCD pixel persistence, which
  restores the transparency and shading effects many titles draw by flickering at
  30 Hz — effects a perfect, instantaneous modern display would otherwise break.
- **Pixel grid.** An analytic, scale-aware subpixel grid (BGR stripes) rendered on
  the GPU (wgpu).
- **Correct on modern displays.** The output is colorspace-tagged so simulated
  colors land accurately on wide-gamut and HDR panels instead of being stretched
  into oversaturation. All of this is present-time only: frame hashing, `verify`,
  and the differential sweeps stay defined on the raw frames and cannot be
  affected by it.

## Workspace

| Crate | Purpose |
|---|---|
| `crates/armv4t` | ARMv4T/Thumb instruction model + decoder (shared by analyzer, translator, interpreter) |
| `crates/gba-core` | GBA machine model: ARM7TDMI interpreter, memory map, hardware |
| `crates/recomp` | Recompiler CLI: `build` (emit + cc), `runc`/`verify` (recompiled execution, differential checks), `play` (windowed play), `frames`/`run`/`dis` (headless tools) |
| `crates/input-config` | Shared input bindings: device choice + button maps, written by the launcher, read by `play` |
| `crates/screen` | Screen simulation: per-revision panel color, temporal response, GPU pixel-grid present path |
| `crates/launcher` | `gba-launcher` frontend: cartridge selection/launch, input rebinding, A/V settings — procedural theme, no bundled assets |

## Legal

**No proprietary content ships here.** This repository contains no first-party
code, BIOS, ROM data, or assets from the console's manufacturer or any game
publisher. You supply your own legally obtained ROM image and BIOS; nothing in
this toolkit obtains them for you.

**Recompiled output is not ours to distribute, nor yours to redistribute.** A
statically recompiled executable is a derivative work of the input ROM image.
This repository never contains or distributes recompiled output — you build it
locally from your own image. Distributing that output is legally akin to
distributing the original ROM image.

**Clean-room throughout.** No code is copied or ported from any other project.
Where another implementation is consulted, it is read for *facts* only —
documented hardware behavior, register maps, data-structure layouts, byte
signatures — never for code. GPL- and MPL-licensed emulators are treated strictly
as such fact references; nothing is derived from them at the code level. In any
case the recompiler and the hardware model are structurally unrelated to any
emulator, because this is ahead-of-time translation, not emulation.

**Audio-engine high-level emulation.** The optional shadow mixers reproduce the
*behavior* of third-party audio middleware embedded in commercial titles (the
MP2K/M4A SDK driver, the GAX lineage, and others). They are written from
independently catalogued facts about those drivers — public reverse-engineering
notes, SDK documentation, structure offsets, and byte signatures — and contain
none of the drivers' code. The detection signatures are short byte patterns used
to recognize a driver, not reproductions of it.

**Video screen simulation.** The panel color models are computed by standard CIE
colorimetry from published, public-domain colorimeter measurements of the
original screens; the math is my own, not a ported shader. The *classic* model
reproduces a long-public, public-domain gamma-4.0 formulation by arithmetic. The
pixel-grid shader is original. No GPL- or MPL-licensed shader code is used.

**Reference materials** are secondary sources that catalogue facts about the
device, its behavior, and the third-party audio libraries some titles embed. No
leaked, confidential, or otherwise proprietary documents or source from the
platform's manufacturer or any other company were used in this project. This
Legal section is the single, repository-wide statement of provenance; the source
files carry no per-file disclaimers.

**Frontend assets** are generated procedurally by our own code; no third-party
artwork is bundled. The GUI toolkit ships its own open-licensed default fonts.

**Dependencies** are all permissively licensed (MIT / Apache-2.0 / ISC) —
egui/eframe, winit, wgpu, raw-window-handle, rfd, gilrs, minifb, cpal,
libloading, dirs, sha2, and (on Apple targets) the objc2 family. There are no
copyleft dependencies. Each dependency's license is bundled with release
distributions.

## License

This project is dual-licensed under either of

- **MIT** ([LICENSE-MIT](LICENSE-MIT)) or
- **Apache License, Version 2.0** ([LICENSE-APACHE](LICENSE-APACHE))

at your option — the standard permissive arrangement of the Rust ecosystem.
Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project shall be dual-licensed as above, without any
additional terms or conditions.
