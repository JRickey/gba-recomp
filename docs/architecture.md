# Static Recompiler — Architecture Brief

> Target: the GBA — a handheld console built around a 16.78 MHz ARM7TDMI
> (ARMv4T), 240×160 LCD, tile/bitmap PPU, 4 PSG + 2 PCM-FIFO audio channels,
> 4-channel DMA, flat cartridge address space, no OS — games bang hardware
> registers directly. Test data is referenced by SHA-256 only.

## Thesis

No public static recompiler exists for the GBA — the niche is open.
Prior art brackets the problem: a well-known static-recomp toolchain for a
1996-era 3D console proved the translate-everything + runtime-address-lookup
model on real commercial games, and a historical dynarec emulator for this
very platform proved that ARM/Thumb translation with crude timing reaches
~80% library compatibility on a 333 MHz MIPS handheld. We combine the two
and close the remaining gap with cheap AOT-time cycle accounting and a
fallback interpreter. A modern desktop has ~1000× the target's compute; we
spend that headroom on accuracy and simplicity, not speed tricks.

## Pain points → solutions

| # | Pain point | Why it's hard | Solution |
|---|---|---|---|
| 1 | **Indirect branches / function pointers** (`bx rN`, `pop {pc}`, `mov pc, rX`, RAM-held callbacks — pervasive: common engines dispatch frame tasks through RAM-held function-pointer tables, e.g. `sha256:a9dec84d…`) | Static CFG recovery is undecidable; this killed classic SBT attempts | **Don't solve it statically.** Translate *every* decodable instruction; compile indirect transfers to a runtime lookup `guest_addr → native fn ptr`. Direct calls/branches still become direct native calls. Lookup table is mode-aware (bit 0 = Thumb, per BX semantics) |
| 2 | **ARM↔Thumb interworking** | Two ISAs, state switches at runtime via target bit 0 | Lift both ISAs into one common IR; entry-point table keys on (address, T-bit). ~61% Thumb / ~39% ARM executed in practice, ARM concentrated in fast-RAM hot code |
| 3 | **Code/data interleaving** (literal pools and jump tables live *in text*, unlike x86/MIPS) | Linear sweep is wrong by construction | Recursive traversal from entry points; every `ldr rX,[pc,#n]` marks data (and often yields a new code pointer); jump-table idioms of the era's two compiler families are enumerable patterns with statically visible bounds (`cmp rN,#max` precedes the dispatch). Translate-everything makes misclassification non-fatal: junk that's never executed just bloats output |
| 4 | **Code copied to fast RAM at boot** (near-universal: crt0 copies hot sections; the standard audio mixer runs from IWRAM in most of the library) | The "static" image isn't where it executes | The bytes come *from the cartridge image*, so they're visible AOT: detect copy sources (crt0 patterns, BIOS-copy/DMA with ROM source), translate those images too, register them at their runtime addresses. Content-hash so re-copies of identical code are no-ops |
| 5 | **True self-modifying / streamed code** (minority of titles: some re-DMA code into IWRAM every frame; some stream overlay code through RAM; one anti-emulation line uses pipeline-visible SMC) | Can't be translated ahead of time at all | **Fallback interpreter tier** for RAM regions whose contents don't match any known translation. Keeps the whole product JIT-free. The interpreter doubles as the differential-testing oracle |
| 6 | **Timing** (games poll VCOUNT, race the beam, FIFO audio driven by timer+DMA cadence; no OS layer to hide behind) | Native code runs ~∞× too fast; per-instruction cycle callbacks are heavyweight | **Cycle-count accuracy, not cycle accuracy** (operations take the right number of cycles without true interleaving — known sufficient for effectively the whole commercial library). AOT-sum cycles per basic block (region-aware: the code's home region and waitstates are known statically; the near-universal commercial waitstate config can be assumed), decrement a downcount, check the event scheduler at block edges. MMIO accesses are sync points: lazily run PPU/timers/DMA up to "now" on any IO access. Scanline-granular PPU rendering; the 1232-cycle/line, 228-line grid is exact |
| 7 | **Interrupts** | BIOS-mediated dispatch through a RAM vector (`0x03007FFC`), IRQ-mode banking | Check IF/IE/IME at block edges (delivery delayed a few instructions — empirically fine). Model only the modes games use: System/User + IRQ + SVC banked R13/R14 + SPSR + CPSR I/T bits. FIQ/aborts: unused on this platform, omit |
| 8 | **BIOS** (proprietary blob; games call ~40 SWIs and even read BIOS open-bus values as fingerprints) | Can't ship it | HLE SWIs 0x00–0x18 (Div, CpuSet, LZ77, IntrWait… — well-trodden); sound SWIs are nearly unused by commercial games (they link their own driver). Honor the dispatch contract (handler pointer at 0x03007FFC, IntrWait flags at 0x03007FF8, post-SWI open-bus values). Optionally support a user-supplied or open-source replacement BIOS run through the interpreter for the accuracy tail |
| 9 | **Memory access cost** (every guest load/store needs decode) | Fastmem (mmap+SIGSEGV) is unsafe-heavy and unbackpatchable in AOT C | **Software address decode**: the map is tiny and fixed (region = bits 24–27); page-pointer table + MMIO escape. Statically specialize literal/PC-relative addresses (very common) into direct array/MMIO accesses with zero dispatch. Implement quirks: rotated misaligned reads, VRAM 8-bit write rules, mirrors, open bus, shared DMA latch |
| 10 | **Saves** | Four backup-media families + in-cart RTC/sensors, no header flag | AOT detection by scanning the image for SDK library ID strings — done at recompile time, baked into the output. Implement SRAM, Flash 64/128K (command state machine), EEPROM 512B/8K (DMA serial protocol), GPIO RTC |
| 11 | **Link cable over network** | Serial multiplayer is synchronous, master-clocked, ms-cadence handshakes — naive networked link breaks | Every peer runs *all* N recompiled instances locally (our output is deterministic by construction); the link cable is emulated locally and perfectly; only controller inputs cross the network — rollback or delay-based lockstep. Recompiled-native speed is what makes re-simulating N cores affordable. Hard day-one requirements this imposes: **determinism + fast full savestates** |
| 12 | **Anti-emulator tricks** (one re-release line probes the wrong backup type, executes from VRAM and mirror addresses, uses pipeline SMC) | Deliberate hostility | Interpreter tier + accurate open-bus/mirror handling covers them; explicitly out of scope for v1 polish — small, known title list |

## Architecture

```
┌─────────────── recompiler tool (Rust) ───────────────┐
│ image → custom ARMv4T/Thumb decoder → analysis        │
│ (entry discovery, literal pools, jump tables,        │
│  RAM-copy detection, save-type detection,            │
│  per-block cycle sums) → IR → emit C11               │
└──────────────────────┬───────────────────────────────┘
                       ▼
        generated C (sharded, parallel-compiled by
        clang/gcc/msvc; user builds locally — output
        is a derivative of the input, never distributed)
                       ▼
┌─────────────── runtime library (Rust, C ABI) ────────┐
│ scheduler/events · software PPU (scanline) · APU     │
│ (PSG + FIFO mixing) · DMA/timers/IRQ · memory map    │
│ · backup media · fallback ARMv4T interpreter ·       │
│ savestates · netplay (rollback / lockstep)           │
│ frontend: winit + wgpu + cpal + gilrs (desktop)      │
└──────────────────────────────────────────────────────┘
```

**Language decision: Rust tool + Rust runtime (C ABI) + emitted C11.**
Rust where we hand-write code (analysis tooling, runtime, netcode — where its
safety pays). Emitted code is C because (a) it's the proven path at this
workload in prior static-recomp projects; (b) rustc has documented pathologies
on huge machine-generated files (>30 min/>32 GB RAM on single generated files,
both codegen backends; a 32 MB image → potentially 10⁵+ functions is squarely
in the danger zone); (c) end users can build recompiled games with any C
compiler, no Rust toolchain; (d) C output keeps the door open to platforms
where JIT is prohibited — there, AOT translation is the *only* full-speed
path, our strongest strategic differentiator. We write our own ARMv4T decoder
(small ISA: ~50 ARM + 19 Thumb formats; general-purpose disassembler libraries
over-accept ARMv7 encodings) and cross-check it against external decoders and
interpreter oracles.

**IR designed for per-flag liveness from day one**: NZCV as separate defs so
whole-function dead-flag elimination deletes the majority of flag computations
(Thumb sets flags on almost every ALU op; almost none are read). Bonus: on
AArch64 hosts guest NZCV maps ~1:1 to host flags including the NOT-borrow
carry convention.

## Presentation & screen simulation

The `screen` crate reproduces, per hardware revision, what the console's
panels did to the colors developers authored (raw output on a modern
monitor is famously oversaturated — games were tuned for the panel):

```
raw BGR555 frames ─→ temporal response ─→ color LUT ─→ grid + scale ─→ present
   (emulation)        (per EMULATED        (32768-entry  (GPU pass,
                       frame: flicker       BGR555→RGBA8, band-limited
                       fusing/persistence)  display-encoded) apertures)
```

- **Color** is first-principles colorimetry: measured panel primaries
  (public-domain colorimeter dataset; derivation cross-checked in tests
  against the dataset's published transform) → XYZ → the display's
  colorspace. Two panel gamuts exist (reflective/frontlit family vs the
  near-sRGB late backlit revision); a continuous darken knob spans the
  unlit↔lit tone response (2.2→3.8 pure power law).
- **Display targeting**: on macOS the surface's layer is colorspace-tagged
  so the compositor color-matches per monitor (wide-gamut laptop panels
  included); elsewhere sRGB is the SDR assumption with a manual wide-gamut
  override. The panel gamuts sit entirely inside sRGB — accuracy is about
  correct mapping, not wide gamut.
- **Temporal response** advances on the emulated-frame stream, never
  presented frames (frame-skip parity is the classic bug); flicker-based
  transparency in many titles makes this a correctness feature.
- **Grid** is a clean-room band-limited implementation (analytic aperture
  integration per output pixel): moiré-free at any window scale, BGR
  subpixel stripe order, fades out below ~2× scale.
- **Pacing**: presentation never paces emulation (audio clock owns speed);
  the GPU surface presents without vsync blocking. Frame hashes / verify /
  sweeps stay defined on raw BGR555 — the pipeline is present-time only.
- Fallback: if no GPU surface is available, the CPU blit path keeps the
  full color + response simulation (no grid) and says so on stderr.

## Platform & distribution

- macOS/Windows/Linux: tool + runtime distributed (binaries via CI); user
  supplies the cartridge image; output builds locally.
- Android: per-game self-contained APK for sideloading (store policy forbids
  loading downloaded native code); cargo-ndk + Gradle. Mind the 16 KB-page-size
  linker requirement.
- iOS (future): static recompilation is the only full-speed path on stock
  devices (no JIT); the stack (wgpu/Metal, cpal/CoreAudio, winit) carries over.

## Licensing decision

**MIT OR Apache-2.0 dual is achievable and recommended.** The full crate stack
is permissive (winit Apache-2.0; wgpu/pixels/object MIT/Apache; rollback
netcode MIT). GPL emulators are read-for-concepts only — document behaviors
from hardware references and test ROMs, never transcribe. Ship
`THIRD_PARTY_LICENSES` via cargo-about. The tool/runtime contain no
proprietary code; recompiled output is a derivative of the input image and is
never distributed.

## Validation strategy

1. **Conformance ROMs**: external CPU/memory/timing test suites — run via the
   interpreter first, then recompiled.
2. **Differential testing**: lockstep recompiled-vs-interpreter execution
   traces; mature external emulators as oracles.
3. **Corpus runs**: `data/baseline` (a small set including known hard cases,
   referenced by SHA-256) for depth; the wider test data under `data/full`
   for breadth — measure decode coverage, translation success, boot-to-title,
   frame-hash stability.
4. **Compiler ground truth**: compile known C with the era's SDK compiler;
   assert the analyzer recovers functions/tables/pools exactly.

## Proposed milestones

1. **M0 — Decoder + interpreter core**: ARMv4T decoder (tables shared by
   analyzer/translator/interpreter), memory map, pass external CPU suites in
   interpreter mode. This *is* the fallback tier, the oracle, and the
   semantics spec. ✅
2. **M1 — Runtime hardware**: scheduler, scanline PPU, timers/DMA/IRQ, keypad,
   audio (PSG+FIFO), saves; a test image boots interpreted, on screen, with
   sound.
3. **M2 — Recompiler MVP**: analysis + C emission for Thumb-in-ROM with
   runtime lookup for indirects; mixed mode (recompiled ROM code, interpreted
   RAM code); first commercial title boots recompiled.
4. **M3 — Full static path**: ARM lifting, RAM-copy translation, cycle sums,
   idle-loop→sleep transformation, flag liveness; corpus-wide compatibility
   runs.
5. **M4 — Product**: savestates/determinism audit, netplay link cable, Android
   APK pipeline, polish.
