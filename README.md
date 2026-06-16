# GBA-Recomp

A rust based, static recompiling toolkit and runtime for the GBA. Translates cartridge ROM images ahead of time into native executables. The runtime library provides the hardware model (PPU, APU, DMA/timers/IRQ, saves, input, RTC). The launcher lets you pick a BIOS and game, and play.

## Author's Notes

The GBA was my childhood system. It was the first gaming console I ever owned. Because the DS came after and had backwards compatibility, I kept playing some GBA games for a long time. The nature of the console, it being designed as a portable SNES, also sparked my love for retro gaming. I played many SNES classics for the first time on their GBA versions.

This is a new kind of recomp project, built to be the absolute best way to play GBA on modern platforms. Everything is provided out of the box: video, audio, full controller support, cross-platform support, a full user-friendly launcher and optional CLI to package games into full native executables. I have taken a lot of care to give the absolute best GBA experience and have provided a few enhancements, detailed in [Audio](#Audio) and [Video](#Video). The final state of this project is that it will work with every game ever released, 0 interpreter.

How is that possible? Surely it falls back to the interpreter sometimes? You can't resolve all code branches at load? The game will work for about 5 minutes then crash? 

Well, I am currently disassembling and mapping every game. Yes, every game, full disassembly. Every function, its bounds, its callers and callees. Importantly, this is just a map, not a decompilation or even interpretation. Functions retain their address offset names. But this lets the recompilation engine have a 100% clear view, every function that needs recompilation, every function that loads into IWRAM. This enables 0 interpreter. Once the map is complete, I'll be even removing the interpreter from this repository. 

I'm using a combination of Ghidra, Qwen 3.6 Coder running locally on my machine, Codex running hourly supervision checks, and a bunch of custom harnesses and tools to fully automate the process, running 24/7. Each rom image's map is verified against reassembling the image for 100% accuracy in a task of this magnitude. Everything will be double and triple checked, there will not be a single function missing. After all, I'm only paying for electricity for my GPU.

As of now, I have 169,778 functions out of an estimated 4.1 million mapped in 4 days of 24/7 runtime. This lives in `gamedb.sqlite`. It currently has one game fully mapped in it, another project of mine, can you guess which? This file is currently licensed CC0, public domain and will continue to be licensed public domain until and beyond completion. I hope that people use it not just for recompiling their favorite games in this repository, but as a basis for matching decompilation as well.

This project is an AI-assisted project. I believe that software should be open source, free, and abundant. If something does not exist, I take that as a challenge to make it exist, and I will use any tools available to do that. Copyright, licenses, and attribution must be respected as a legal requirement and as principle for sustainable open source development. For details on AI-assisted development regarding copyright, licensing, and clean-rooms, see [Legal](#legal).

## Design at a glance

- **Boundaries, not guesswork.** Each game's complete function map — every
  function's address, length, and ARM/Thumb mode, keyed by ROM SHA-256 —
  ships as `gamedb.sqlite` (Work in progress) (CC0). The recompiler seeds from those boundaries,
  so with a full map every reachable block is translated up front: a 100%
  native binary, zero interpreter.
- **`recomp-core` is the engine.** Analyze → emit C11 → compile → link, as a
  standalone library shared by the developer CLI, the packager, and the
  launcher. The emitted C calls the Rust hardware model through a C ABI,
  builds with any C compiler (no just-in-time compilation), and its bounded
  translation units compile in parallel across cores.
- **Translate everything; resolve the indirect branches at runtime** via a
  guest-address → native-function table (mode-aware).
- **RAM-resident code is captured, not interpreted.** Games copy hot code
  (IRQ handlers, sound mixers) into IWRAM at boot; a short interpreter
  profile at build time observes and translates it from the runtime snapshot.
  The interpreter then remains only as a stability net for self-modifying
  code and as the differential-testing oracle — never the intended runtime
  path.
- **Two surfaces, one engine.** End users play from the launcher: pick a
  cartridge, it hashes your image, looks the boundaries up in the database,
  runs the profile, and recompiles a full native library into your install
  dir — cached by content, instant on later launches. Developers use the
  `recomp` CLI and `gba-pack` to package one game into a standalone, portable
  executable, optionally emitting the recompiled C for modding.
- **Cycle-count accuracy**: per-block cycle sums computed at recompile time,
  event scheduler checked at block edges, MMIO accesses as catch-up sync points.
- **Netplay**: all instances simulated on every peer, only controller inputs
  cross the network (rollback/lockstep) (Work in progress)

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
- **Correct on every display, out of the box.** Simulated colors carry their
  colorspace, so a color-managing compositor lands them accurately on the actual
  panel — ordinary sRGB, wide-gamut, or HDR — instead of stretching them into
  oversaturation. Where a platform doesn't color-manage by default (notably some
  Wayland setups driving wide-gamut panels), a gated fallback detects the panel
  and corrects the output itself, while leaving ordinary sRGB panels untouched —
  so neither case needs any configuration. All of this is present-time only:
  frame hashing, `verify`, and the differential sweeps stay defined on the raw
  frames and cannot be affected by it.

## Workspace

| Crate | Purpose |
|---|---|
| `crates/armv4t` | ARMv4T/Thumb instruction model + decoder (shared by analyzer, translator, interpreter) |
| `crates/gba-core` | GBA machine model: ARM7TDMI interpreter, memory map, hardware |
| `crates/recomp-core` | Static recompiler engine: analyze, emit C, parallel compile/link, pluggable label sourcing — the library behind the CLI, packager, and launcher |
| `crates/gamedb` | Reader for `gamedb.sqlite`: function boundaries by ROM SHA-256, seeding a full recompile |
| `crates/recomp` | Recompiler CLI + play runtime over `recomp-core`: `build` (emit + cc), `runc`/`verify` (recompiled execution, differential checks), `play` (windowed play), `frames`/`run`/`dis` (headless tools) |
| `crates/pack` | `gba-pack`: package one mapped game into a standalone, distributable executable (pins ROM/BIOS by hash; ships no game data) |
| `crates/input-config` | Shared input bindings: device choice + button maps, written by the launcher, read by `play` |
| `crates/screen` | Screen simulation: per-revision panel color, temporal response, GPU pixel-grid present path |
| `crates/launcher` | `gba-launcher` frontend: cartridge selection/launch, input rebinding, A/V settings — procedural theme, no bundled assets |

## Building

```sh
cargo build --release
./target/release/recomp play path/to/your.gba    # translate (first launch) and play
./target/release/gba-launcher                     # graphical frontend
```

You need stable Rust and a C compiler on `PATH` as `cc` — translation
emits C11 and compiles it locally. See **[BUILDING.md](BUILDING.md)** for
platform-specific dependencies (macOS / Linux / Windows / Android), build
profiles and parallelism, the full CLI command reference, the translation
cache, and how to run the tests.

## Legal

**No proprietary content ships here.** This repository contains no first-party
code, BIOS, ROM data, or assets from the console's manufacturer or any game
publisher. You supply your own legally obtained ROM image and BIOS; nothing in
this toolkit obtains them for you.

**No distribution of recompiled output** This repository does not contain any 
recompiled output. Users build recompiled output themselves from their own legally
obtained ROM Image and BIOS.

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

**Vendored data.** One third-party data file is checked in: a community
game-controller mapping database (zlib-licensed), bundled so the launcher and
play runtime recognize the broadest set of controllers. It is recorded, with
source, version, and license, in [THIRD-PARTY.md](THIRD-PARTY.md).

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
