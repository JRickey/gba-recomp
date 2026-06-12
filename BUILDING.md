# Building from source

The workspace builds two user-facing binaries:

- **`recomp`** — the recompiler CLI and play runtime (`crates/recomp`)
- **`gba-launcher`** — the graphical frontend (`crates/launcher`)

plus the library crates they share (`armv4t`, `gba-core`, `input-config`,
`screen`).

## Prerequisites

1. **Rust** (stable, edition 2021) via [rustup](https://rustup.rs):

   ```sh
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   ```

2. **A C compiler on `PATH` as `cc`.** This is a *runtime* requirement, not
   just a build one: the recompiler emits C11 and shells out to `cc` to
   compile each translation unit and link the per-title shared library.
   Without it, `recomp build` fails and `recomp play` degrades to the
   interpreter (loudly).

   - macOS: Xcode Command Line Tools — `xcode-select --install`
   - Linux: `gcc` or `clang` (`build-essential` on Debian/Ubuntu)

3. **Platform libraries** — see [Platform notes](#platform-notes) below.

## Quick start

```sh
git clone <this repository>
cd gba-lib
cargo build --release
```

Binaries land in `target/release/`:

```sh
./target/release/recomp play path/to/your.gba   # translate (first launch) and play
./target/release/gba-launcher                    # graphical frontend
```

You supply your own legally obtained ROM image; nothing here obtains one
for you (see the Legal section of the README).

## Build profiles and parallelism

- **`cargo build`** (debug) — fast iteration. Emulation is markedly slower
  than release but fine for development and tests.
- **`cargo build --release`** — what you should benchmark and play on. The
  release profile uses fat LTO with `codegen-units = 1`, so the *final
  link step is single-threaded and slow by design*; that's the intended
  trade for runtime speed. Symbols are kept (`debug = true`) so profilers
  and backtraces work on release builds.

Useful knobs:

```sh
cargo build -j N                  # cap rustc parallelism (defaults to all cores)
cargo build --release -p recomp   # skip the GUI stack when you only need the CLI
cargo run -p recomp --release -- play rom.gba   # build + run in one step
```

Note `-j` does not parallelize the fat-LTO link or the emitted-C compile
during translation; those are inherently serial per title.

### Translating many titles at once

Each `recomp build` emits chunked ~16 MB C translation units and compiles
them one `cc` at a time, but a single 4 MB image can still expand to
hundreds of MB of C. If you script builds across a set of images
(sweeps), **keep it at ≤ 4 concurrent jobs** on a 16 GB machine —
parallel `cc` invocations on large images will exhaust memory. Emitted C
is compiled `-O1`; `-O2` was measured at ~2% runtime gain for 3× the
compile time and rejected.

## Running the crates

### `recomp` — the CLI

| Command | Purpose |
|---|---|
| `recomp build <rom> [--ram] [--bios file]` | Translate to a native shared library in `out/`. `--ram` adds a short profiling run that discovers RAM-resident code and computed-branch targets, then bakes them in (recommended; this is what `play` uses). `--bios` also recompiles a real 16 KB BIOS image (region 0, no BIOS HLE) into `out/<stem>-bios.dylib`. |
| `recomp play <rom> [--interp] [--stats] [--status] [--bios file] [--no-bios]` | Windowed play. Boots the real BIOS when an image is installed (see below), BIOS HLE otherwise. First launch auto-translates into the cache (one-time, progress printed); `--interp` forces the interpreter; `--stats` prints perf readouts; `--status` emits machine-readable lifecycle lines (used by the launcher). |
| `recomp runc <rom> [--frames N] [--out img.ppm] [--bios file]` | Headless run of the recompiled output from `out/`. |
| `recomp verify <rom> [--frames N] [--reuse] [--dump prefix] [--bios file]` | Differential check: interpreter vs recompiled frame hash; prints `MATCH`/`MISMATCH`. `--reuse` skips the rebuild, `--dump` writes both final frames, `--bios` verifies under a real recompiled BIOS on both sides. |
| `recomp run <rom> [--max-steps N] [--trace] [--hist] [--bios file]` | Headless interpreter run; `--hist` prints a hot-PC histogram. |
| `recomp frames <rom> [--frames N] [--out img.ppm] [--keys MASK] [--demo] [--sav file] [--bios file]` | Headless boot to frame N; prints frame hash and boot diagnostics. `--demo` taps Start/A periodically; `--sav` preloads a save. |
| `recomp dis <rom> [--addr HEX] [--count N] [--thumb]` | Disassemble at an address. |
| `recomp entry-scan <dir>` | Validate entry decoding across a directory of images. |
| `recomp mp2k-scan <rom\|dir>`, `recomp engine-scan <rom\|dir>` | Audio-engine detection reports (developer tools for the HLE shadow mixers). |

View dumped frames with any PPM-aware tool; on macOS:
`sips -s format png out/x.ppm --out out/x.png`.

**Translation cache.** `play` keeps per-title translations under
`<cache_dir>/gba-recomp/t<REV>/<sha256>.dylib` (e.g.
`~/Library/Caches/gba-recomp` on macOS, `~/.cache/gba-recomp` on Linux),
keyed by the ROM's SHA-256. Bumping `TRANSLATION_REV` (any emitter or ABI
change) invalidates and sweeps old revisions automatically; deleting the
directory just costs a one-time retranslation.

**Labels: reducing interpreter fallback.** Static analysis plus a short
profiling boot can't reach code that only executes deep into a game
(computed-branch targets, handlers installed at runtime). Whenever the
runtime falls back to the interpreter it knows exactly which entry point
was missing — play or runc with `--record-labels` persists those as a
*label file* and the next translation covers them:

```sh
recomp play game.gba --record-labels     # just play; labels accumulate
# next launch rebuilds automatically (the label set is part of the
# cache key) — fallback at the places you visited is gone
```

Label files are keyed by the image's SHA-256, hold only addresses and
names (never image content), and union-merge — so they accumulate
across sessions and can be shared: a file named `<rom>.labels.toml`
(the TOML interchange format disassembly tooling emits — see
docs/labels.md) or `<rom>.labels` (the recorder's line format) next to
the image is picked up automatically alongside the recorder's own
accumulator in `<config_dir>/gba-recomp/labels/`. ROM entries are pure
hints (the translation derives from the image itself, so a wrong label
is harmless). `iwram` entries cover RAM-resident code: the recorder
captures a machine-local content snapshot the moment each entry is
discovered, the build translates from it, and the standard whole-block
content guards keep execution correct if the game later swaps that
memory (the snapshot stays on your machine — shared label files carry
addresses only, so a recipient records their own snapshot by playing).
`ewram` records are reserved and currently skipped.

**BIOS image (real-BIOS boot).** `play` boots the user's real BIOS dump
when one is installed, recompiling it alongside the cartridge (real-BIOS
translations carry a `-b<bios-sha prefix>` cache suffix so they never
shadow HLE builds). Resolution order: `$GBA_RECOMP_BIOS`, then
`gba_bios.bin` next to the executable (portable release layout), then
`<config_dir>/gba-recomp/gba_bios.bin`. The launcher's first-launch setup
collects the dump and installs it (release directory preferred, config
directory when that's read-only). No image installed = BIOS HLE, exactly
the previous behavior; `--no-bios` forces HLE.

### `gba-launcher` — the frontend

```sh
cargo run -p gba-launcher --release
```

The launcher spawns `recomp play` for the selected cartridge. It resolves
the `recomp` binary from `$GBA_RECOMP_BIN`, then next to the launcher
executable, then `$PATH` — set `GBA_RECOMP_BIN=target/release/recomp`
when running both from the source tree. Input bindings and A/V settings
are shared with `play` via `<config_dir>/gba-recomp/` (`input.cfg`,
`av.cfg`). See `docs/launcher.md`.

## Tests

```sh
cargo test            # workspace unit + integration tests, no external data needed
```

Conformance against external test suites and game images is data-driven
and intentionally not in the repository — you supply your own copies
under the gitignored `data/` directory. The conventions:

- CPU/memory conformance suites: `recomp run data/suites/<suite>/*.gba`
  — a pass parks with `r12 = 0` (or `ewram[0] = 0`, depending on suite
  convention).
- Differential sweeps: `recomp verify` per image; goldens are frame
  hashes (see `out/golden-hashes-600f.txt` pattern).

## Platform notes

### macOS (primary development platform)

Xcode Command Line Tools are the only system requirement. The screen
simulation tags its Metal layer with the correct colorspace (sRGB/P3) via
`objc2`; no extra setup. Both Apple Silicon and Intel work.

### Linux

Works headless and windowed; the differential sweeps run on a Linux box.
Build-time package needs, Debian/Ubuntu names:

```sh
sudo apt install build-essential pkg-config libasound2-dev \
    libx11-dev libxkbcommon-dev
```

- `libasound2-dev` — ALSA, for audio output (cpal)
- X11/xkbcommon headers — for the play window (minifb) and launcher (eframe/winit)
- GPU presentation (wgpu) uses Vulkan at runtime: install your distro's
  Vulkan drivers (e.g. `mesa-vulkan-drivers`); without them the screen
  simulation falls back gracefully
- File dialogs use the XDG desktop portal (no GTK dependency)

Note: cached translations keep the `.dylib` file name on all platforms;
on Linux they are ordinary ELF shared objects — the extension is
cosmetic.

### Windows

Builds are expected to work for the interpreter, launcher, and play
paths (all dependencies support Windows) but are **untested on
hardware**. Native translation additionally requires a POSIX-style `cc`
on `PATH` (LLVM/clang or MSYS2/MinGW-w64); the MSVC `cl.exe` driver is
not supported. Treat Windows as experimental; reports welcome.

### Android

Unverified scaffolding only — the launcher builds as a `cdylib` through
the NativeActivity glue:

```sh
cargo build -p gba-launcher --target aarch64-linux-android \
    --features android-native-activity
```

See `docs/launcher.md` for what's missing (SAF picker shim).

## Developer diagnostics

Performance and triage readouts are deliberately absent from release UX;
opt in explicitly:

- `recomp play --stats` — frame-time / native-vs-fallback readouts
- `RECOMP_KEEP_C=1 recomp build ...` — keep the emitted C next to the
  output for inspection
- `recomp play --interp` — force the interpreter (e.g. to isolate a
  translation bug; a correct title behaves identically, just slower)

Anything printed as `DEGRADED` is a defect surface (interpreter
fallback, audio ring drops, slow frames) — it is intentionally loud.
