//! Static recompiler CLI.
//!
//! Current commands exercise the decoder against real ROMs:
//!   dis <rom> [--addr HEX] [--count N] [--thumb]   disassemble from an address
//!   entry-scan <dir>                               decode every ROM's entry point

use std::path::Path;
use std::process::ExitCode;

mod analyze;
mod emit;
mod labels;
mod packfile;

use armv4t::{decode_arm, decode_thumb, Op};
use gba_core::{is_self_loop, Machine, StepEvent};

const ROM_BASE: u32 = 0x0800_0000;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `recomp help [cmd]`, `recomp <cmd> --help`, and bare `--help`/`-h`
    // all land in help; the first non-help token names the topic.
    if args.iter().any(|a| a == "--help" || a == "-h") || args.first().map(String::as_str) == Some("help") {
        let topic = args
            .iter()
            .find(|a| *a != "--help" && *a != "-h" && *a != "help")
            .map(String::as_str);
        print_help(topic);
        return ExitCode::SUCCESS;
    }
    let result = match args.first().map(String::as_str) {
        Some("dis") => cmd_dis(&args[1..]),
        Some("entry-scan") => cmd_entry_scan(&args[1..]),
        Some("run") => cmd_run(&args[1..]),
        Some("frames") => cmd_frames(&args[1..]),
        Some("play") => cmd_play(&args[1..]),
        Some("build") => cmd_build(&args[1..]),
        Some("mp2k-scan") => cmd_mp2k_scan(&args[1..]),
        Some("engine-scan") => cmd_engine_scan(&args[1..]),
        Some("runc") => cmd_runc(&args[1..]),
        Some("verify") => cmd_verify(&args[1..]),
        Some("labels") => cmd_labels(&args[1..]),
        // A packaged binary (manifest beside the executable) plays its
        // pinned title when launched bare — double-click behavior.
        None if packfile::load().is_some() => cmd_play(&[]),
        _ => {
            print_help(None);
            return ExitCode::FAILURE;
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `(name, synopsis, flag lines, description)` per command — the single
/// source for both the overview listing and per-command help.
const HELP: &[(&str, &str, &[&str], &str)] = &[
    ("build", "recomp build <rom> [--ram] [--bios file] [--labels file]", &[
        "--ram    profile a short interpreter run first, translating the RAM-resident \
code and computed-branch targets it discovers (recommended; play's cache builds use it)",
        "--bios FILE    experimental: recompile a real 16 KB BIOS image too (region 0) \
and disable all BIOS HLE; output goes to out/<stem>-bios.dylib",
        "--labels FILE    union an explicit label map (instead of relying on the \
beside-the-image file); used by packaging so a symlinked or renamed image still finds \
its map",
    ], "Translate a ROM image to a native shared library at out/<stem>.dylib. \
Emits C11 in bounded chunks and compiles them with cc. Label files (<rom>.labels \
or the recorder's accumulator) contribute extra entry-point seeds automatically."),
    ("play", "recomp play <rom> [--interp] [--stats] [--status] [--record-labels] [--bios file] [--no-bios]", &[
        "--interp           force the interpreter (skip/ignore native translation)",
        "--stats            print performance readouts (frame time, native vs fallback)",
        "--status           emit machine-readable lifecycle lines on stdout (used by the launcher)",
        "--record-labels    record interpreter-fallback entry points; the next translation \
covers them (accumulates across sessions)",
        "--bios FILE    boot a specific real BIOS image (recompiled; no BIOS HLE)",
        "--no-bios      force BIOS HLE even when an image is installed",
    ], "Windowed play. Boots the real BIOS when one is installed ($GBA_RECOMP_BIOS, \
gba_bios.bin next to the executable, or the user config dir — the launcher's first-launch \
setup installs it); BIOS HLE otherwise. First launch translates the image into the \
per-user cache (one-time, progress printed); later launches load it instantly. Reads \
input.cfg/av.cfg from the shared config directory."),
    ("runc", "recomp runc <rom> [--frames N] [--out img.ppm] [--input file] [--record-labels] [--bios file]", &[
        "--frames N         frames to run (default 600)",
        "--out PATH         write the final frame as a PPM",
        "--input FILE       replay a recorded input script (see play's RECOMP_RECORD_INPUT)",
        "--record-labels    record fallback entry points as labels (headless soak runs)",
        "--bios FILE     run a real-BIOS build (loads out/<stem>-bios.dylib; no BIOS HLE)",
    ], "Run the recompiled output from out/<stem>.dylib headless (build first); \
prints frame hash and native/fallback counts."),
    ("verify", "recomp verify <rom> [--frames N] [--reuse] [--dump prefix] [--input file] [--bios file]", &[
        "--frames N       frames to compare (default 600)",
        "--reuse          skip the rebuild if out/<stem>.dylib exists",
        "--dump PREFIX    write both final frames as PREFIX.interp.ppm / PREFIX.recomp.ppm",
        "--input FILE     replay a recorded input script on both sides (instead of demo taps)",
        "--bios FILE      verify with a real recompiled BIOS on both sides (no BIOS HLE)",
    ], "Differential check: run the interpreter and the recompiled output, compare \
frame hashes, print MATCH or MISMATCH (exit code follows)."),
    ("run", "recomp run <rom> [--max-steps N] [--trace] [--hist] [--bios file]", &[
        "--max-steps N    stop after N interpreter steps",
        "--trace          disassemble every executed instruction to stderr",
        "--hist           print a hot-PC histogram at exit",
        "--bios FILE      execute a real 16 KB BIOS image (boot from the reset vector; no HLE)",
    ], "Headless interpreter run; the conformance-suite driver (a pass parks with r12=0)."),
    ("frames", "recomp frames <rom> [--frames N] [--out img.ppm] [--keys MASK] [--demo] [--input file] [--sav file] [--bios file]", &[
        "--frames N      frames to run (default 600)",
        "--out PATH      write the final frame as a PPM",
        "--keys MASK     hold a KEYINPUT mask for the whole run (hex)",
        "--demo          deterministic demo input: periodic Start, then A taps",
        "--input FILE    replay a recorded input script (see play's RECOMP_RECORD_INPUT)",
        "--sav FILE      preload a save file",
        "--bios FILE     execute a real 16 KB BIOS image (boot from the reset vector; no HLE)",
    ], "Headless boot to frame N on the interpreter; prints the frame hash plus \
boot diagnostics (DISPCNT/PC/SWI and live disassembly at PC)."),
    ("labels", "recomp labels <show|import|export> <rom> [file]", &[
        "show              sources, entry counts, snapshot status, cache key",
        "import FILE       union a shared label file into this image's accumulator \
(v1 lines or v2 TOML, auto-detected)",
        "export [FILE]     write a shareable label file (default <rom>.labels.toml; \
a non-.toml FILE writes v1 lines)",
    ], "Inspect and exchange label files — entry points discovered at runtime or by \
disassembly tooling that feed translation coverage (see BUILDING.md and docs/labels.md)."),
    ("dis", "recomp dis <rom> [--addr HEX] [--count N] [--thumb]", &[
        "--addr HEX    start address (default ROM base)",
        "--count N     instructions to print (default 16)",
        "--thumb       decode as Thumb",
    ], "Disassemble ROM bytes at an address."),
    ("entry-scan", "recomp entry-scan <dir>", &[],
     "Decode the entry branch of every image in a directory — a corpus sanity check."),
    ("mp2k-scan", "recomp mp2k-scan <rom|dir>", &[],
     "Report MP2K/M4A audio-driver detection (signatures, hook addresses) for one image or a directory."),
    ("engine-scan", "recomp engine-scan <rom|dir>", &[],
     "Classify the audio engine (MP2K, GAX lineage, others) for one image or a directory."),
];

/// `recomp help` / `recomp help <cmd>` / `recomp <cmd> --help`.
fn print_help(topic: Option<&str>) {
    if let Some(name) = topic {
        if let Some((_, synopsis, flags, desc)) = HELP.iter().find(|(n, ..)| *n == name) {
            eprintln!("usage: {synopsis}");
            eprintln!();
            eprintln!("{desc}");
            if !flags.is_empty() {
                eprintln!();
                for f in *flags {
                    eprintln!("  {f}");
                }
            }
            return;
        }
        eprintln!("unknown command {name:?}");
        eprintln!();
    }
    eprintln!("usage: recomp <command> [args]");
    eprintln!();
    for (_, synopsis, _, _) in HELP {
        eprintln!("  {synopsis}");
    }
    eprintln!();
    eprintln!("'recomp help <command>' for details; BUILDING.md for the full reference");
}

/// Recorded input script: header line `gba-input v1`, then one
/// `<frame> <hexmask>` line per key-state change (KEYINPUT-style,
/// active-low). Keys hold their value between change points; before the
/// first point everything is released. Recorded by play under
/// RECOMP_RECORD_INPUT=<path>, replayed by frames/runc/verify --input —
/// the bridge from a live repro to a deterministic headless one.
struct InputScript(Vec<(u64, u16)>);

impl InputScript {
    fn load(path: &str) -> Result<InputScript, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        let mut lines = text.lines();
        if lines.next().map(str::trim) != Some("gba-input v1") {
            return Err(format!("{path}: not a gba-input v1 file"));
        }
        let mut points = Vec::new();
        for l in lines {
            let l = l.trim();
            if l.is_empty() {
                continue;
            }
            let (f, k) = l.split_once(' ').ok_or_else(|| format!("{path}: bad line {l:?}"))?;
            points.push((
                f.parse().map_err(|e| format!("{path}: bad frame in {l:?}: {e}"))?,
                u16::from_str_radix(k, 16).map_err(|e| format!("{path}: bad mask in {l:?}: {e}"))?,
            ));
        }
        points.sort();
        Ok(InputScript(points))
    }

    fn keys_at(&self, frame: u64) -> u16 {
        match self.0.partition_point(|&(f, _)| f <= frame) {
            0 => 0x3FF,
            n => self.0[n - 1].1,
        }
    }
}

/// Load and sanity-check a real BIOS image for --bios runs.
fn load_bios_file(path: impl AsRef<Path>) -> Result<Vec<u8>, String> {
    let path = path.as_ref();
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes.len() != input_config::BIOS_SIZE {
        return Err(format!(
            "{}: expected a {}-byte BIOS image, got {} bytes",
            path.display(),
            input_config::BIOS_SIZE,
            bytes.len()
        ));
    }
    Ok(bytes)
}

/// Construct the machine for a run: real-BIOS mode when an image is
/// given (boots from the reset vector, no HLE), HLE boot otherwise.
fn make_machine(rom: Vec<u8>, bios: Option<&[u8]>) -> Machine {
    match bios {
        Some(b) => Machine::new_with_bios(rom, b),
        None => Machine::new(rom),
    }
}

fn parse_hex(s: &str) -> Result<u32, String> {
    let s = s.trim_start_matches("0x").trim_start_matches("0X");
    u32::from_str_radix(s, 16).map_err(|e| format!("bad hex value {s:?}: {e}"))
}

fn cmd_dis(args: &[String]) -> Result<(), String> {
    let mut rom_path = None;
    let mut addr = ROM_BASE;
    let mut count = 16usize;
    let mut thumb = false;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--addr" => addr = parse_hex(it.next().ok_or("--addr needs a value")?)?,
            "--count" => {
                count = it.next().ok_or("--count needs a value")?
                    .parse().map_err(|e| format!("bad count: {e}"))?
            }
            "--thumb" => thumb = true,
            other if rom_path.is_none() => rom_path = Some(other.to_string()),
            other => return Err(format!("unexpected argument {other:?}")),
        }
    }
    let rom_path = rom_path.ok_or("missing ROM path")?;
    let rom = std::fs::read(&rom_path).map_err(|e| format!("{rom_path}: {e}"))?;

    let mut pc = addr;
    for _ in 0..count {
        let off = pc.wrapping_sub(ROM_BASE) as usize;
        if thumb {
            let Some(bytes) = rom.get(off..off + 2) else { break };
            let half = u16::from_le_bytes([bytes[0], bytes[1]]);
            let instr = decode_thumb(half, pc);
            println!("{pc:08x}:     {half:04x}  {}", instr.disasm());
            pc += 2;
        } else {
            let Some(bytes) = rom.get(off..off + 4) else { break };
            let word = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            let instr = decode_arm(word, pc);
            println!("{pc:08x}: {word:08x}  {}", instr.disasm());
            pc += 4;
        }
    }
    Ok(())
}

/// Every licensed cartridge image begins with an ARM branch over the header to the
/// entry point. Decoding it across a whole corpus is a cheap end-to-end
/// smoke test of the ARM decoder against real-world data.
fn cmd_entry_scan(args: &[String]) -> Result<(), String> {
    let dir = args.first().ok_or("missing directory")?;
    let mut total = 0u32;
    let mut ok = 0u32;

    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| format!("{dir}: {e}"))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    entries.sort();

    for path in &entries {
        total += 1;
        match check_entry(path) {
            Ok(line) => {
                ok += 1;
                println!("OK   {line}  {}", file_name(path));
            }
            Err(e) => println!("FAIL {e}  {}", file_name(path)),
        }
    }
    println!("\n{ok}/{total} ROM entry points decoded as a branch");
    if ok == total && total > 0 {
        Ok(())
    } else {
        Err("entry scan had failures".into())
    }
}

/// Run a ROM in the interpreter until it parks in an unconditional
/// self-loop (the idiom every test ROM ends with), then dump CPU state.
fn cmd_run(args: &[String]) -> Result<(), String> {
    let mut rom_path = None;
    let mut max_steps = 200_000_000u64;
    let mut trace = false;
    let mut hist = false;
    let mut bios: Option<Vec<u8>> = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--max-steps" => {
                max_steps = it.next().ok_or("--max-steps needs a value")?
                    .parse().map_err(|e| format!("bad max-steps: {e}"))?
            }
            "--trace" => trace = true,
            "--hist" => hist = true,
            "--bios" => bios = Some(load_bios_file(it.next().ok_or("--bios needs a value")?)?),
            other if rom_path.is_none() => rom_path = Some(other.to_string()),
            other => return Err(format!("unexpected argument {other:?}")),
        }
    }
    let rom_path = rom_path.ok_or("missing ROM path")?;
    let rom = std::fs::read(&rom_path).map_err(|e| format!("{rom_path}: {e}"))?;

    let mut m = make_machine(rom, bios.as_deref());
    let mut counts: std::collections::HashMap<u32, u64> = std::collections::HashMap::new();

    let mut steps = 0u64;
    while steps < max_steps {
        let event = m.step();
        steps += 1;
        if let StepEvent::Instr(instr) = event {
            if trace {
                eprintln!("{:08x} [{}]: {}", instr.addr, m.bus.clock, instr.disasm());
            }
            // Histogram the tail of the run (where the steady state lives).
            if hist && steps > max_steps.saturating_sub(500_000) {
                *counts.entry(instr.addr).or_default() += 1;
            }
            // Parked: unconditional branch to itself, with no way out.
            if is_self_loop(&instr) && !m.bus.irq_pending() {
                break;
            }
        }
    }

    if hist {
        let mut top: Vec<_> = counts.into_iter().collect();
        top.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
        for (addr, n) in top.iter().take(10) {
            println!("hot {addr:08x}: {n}");
        }
    }

    println!("steps: {steps} frames: {}", m.bus.frames);
    for r in 0..16 {
        print!("r{r}={:08x} ", m.cpu.regs[r]);
        if r % 4 == 3 {
            println!();
        }
    }
    println!("cpsr={:08x} mode={:?} thumb={}", m.cpu.cpsr, m.cpu.mode(), m.cpu.thumb());
    {
        use gba_core::Bus as _;
        println!("ewram[0]={:08x}", m.bus.read32(0x0200_0000));
        let dispstat = m.bus.read16(0x0400_0004);
        let biosflags = m.bus.read16(0x0300_7FF8);
        println!("ie={:04x} if={:04x} ime={} dispstat={dispstat:04x} halted={} armed={} biosflags={biosflags:04x}", m.bus.reg_ie, m.bus.reg_if, m.bus.ime, m.bus.halted, m.bus.intr_wait_armed);
    }
    Ok(())
}

/// Run a ROM for N frames headless, print a framebuffer hash, and
/// optionally write the final frame as a binary PPM.
fn cmd_frames(args: &[String]) -> Result<(), String> {
    let mut rom_path = None;
    let mut frames = 60u64;
    let mut out: Option<String> = None;
    let mut keys = 0x3FFu16; // active-low: nothing pressed
    let mut demo = false; // verify-style Start/A taps (menus need edges)
    let mut input: Option<InputScript> = None; // recorded-session replay
    let mut sav: Option<String> = None; // load backup media before boot
    let mut bios: Option<Vec<u8>> = None; // real-BIOS execution (no HLE)

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--frames" => {
                frames = it.next().ok_or("--frames needs a value")?
                    .parse().map_err(|e| format!("bad frames: {e}"))?
            }
            "--out" => out = Some(it.next().ok_or("--out needs a value")?.to_string()),
            "--keys" => keys = parse_hex(it.next().ok_or("--keys needs a value")?)? as u16,
            "--demo" => demo = true,
            "--input" => {
                input = Some(InputScript::load(it.next().ok_or("--input needs a value")?)?)
            }
            "--sav" => sav = Some(it.next().ok_or("--sav needs a value")?.to_string()),
            "--bios" => bios = Some(load_bios_file(it.next().ok_or("--bios needs a value")?)?),
            other if rom_path.is_none() => rom_path = Some(other.to_string()),
            other => return Err(format!("unexpected argument {other:?}")),
        }
    }
    let rom_path = rom_path.ok_or("missing ROM path")?;
    let rom = std::fs::read(&rom_path).map_err(|e| format!("{rom_path}: {e}"))?;

    let mut m = make_machine(rom, bios.as_deref());
    m.bus.keys = keys;
    if let Some(p) = &sav {
        let data = std::fs::read(p).map_err(|e| format!("{p}: {e}"))?;
        m.bus.load_save_data(&data);
        eprintln!("loaded {p}");
    }

    // Diagnostic (RECOMP_DUMP_AUDIO=<prefix>): capture the full audio taps
    // headless — <prefix>.mixed.wav / .psg.wav (PCM16 mono @ tap rate)
    // and <prefix>.fifoN.bin ((i8 sample, u32le period) DAC events).
    let dump_audio = std::env::var("RECOMP_DUMP_AUDIO").ok();
    let mut mixed: Vec<i16> = Vec::new();
    let mut psg: Vec<i16> = Vec::new();
    let mut fifo_ev: [Vec<(i8, u32)>; 2] = [Vec::new(), Vec::new()];
    let mut hle: Vec<f32> = Vec::new();
    if dump_audio.is_some() {
        m.bus.tap_channels = true;
    }
    // RECOMP_MP2K=1: arm the HLE shadow mixer headless — the
    // differential self-check then runs under `frames`, which is the
    // autonomous validation path for the shadow (no ears required).
    let mp2k_on = std::env::var_os("RECOMP_MP2K").is_some();
    if mp2k_on {
        m.bus.tap_channels = true;
        eprintln!("hle: {}", arm_audio_hle(&mut m));
    }
    // Diagnostic (RECOMP_COST_FROM=N): from frame N on, attribute charged
    // cycles to the PC that incurred them; dump the histogram at exit.
    let cost_from: Option<u64> = std::env::var("RECOMP_COST_FROM").ok().and_then(|v| v.parse().ok());
    let mut cost: std::collections::HashMap<u32, u64> = std::collections::HashMap::new();
    // Triage (RECOMP_WATCH=hexaddr): print a guest word at every frame
    // where it changes — pairs with the reference harness's REF_WATCH
    // for state-timeline diffs.
    let watch: Option<u32> = std::env::var("RECOMP_WATCH")
        .ok()
        .and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok());
    let mut watch_last: Option<u32> = None;
    for _ in 0..frames {
        if demo {
            m.bus.keys = demo_keys(m.bus.frames);
        }
        if let Some(s) = &input {
            m.bus.keys = s.keys_at(m.bus.frames);
        }
        if let Some(a) = watch {
            use gba_core::Bus as _;
            let v = m.bus.read32(a);
            if watch_last != Some(v) {
                eprintln!("WATCH f={} [{a:08x}]={v:08x}", m.bus.frames);
                watch_last = Some(v);
            }
        }
        if cost_from.is_some_and(|n| m.bus.frames >= n) {
            m.bus.frame_ready = false;
            let mut steps = 0u64;
            while !m.bus.frame_ready && steps < 5_000_000 {
                let pc = m.cpu.regs[15];
                let c0 = m.bus.clock;
                m.step();
                *cost.entry(pc).or_insert(0) += m.bus.clock - c0;
                steps += 1;
            }
        } else {
            m.run_frame(5_000_000);
        }
        if dump_audio.is_some() {
            mixed.extend(m.bus.audio_buf.drain(..));
            psg.extend(m.bus.psg_tap.drain(..));
            for f in 0..2 {
                fifo_ev[f].append(&mut m.bus.fifo_tap[f]);
            }
        }
        if mp2k_on {
            hle.extend(m.bus.hle_tap.drain(..));
        }
    }
    if !cost.is_empty() {
        let mut v: Vec<(u32, u64)> = cost.into_iter().collect();
        v.sort_by_key(|&(_, c)| std::cmp::Reverse(c));
        let total: u64 = v.iter().map(|&(_, c)| c).sum();
        eprintln!("COST total={total} cycles over traced frames; top PCs:");
        for (pc, c) in v.iter().take(40) {
            eprintln!("  COST {pc:08x} {c}");
        }
    }
    if let Some(h) = m.bus.mp2k.as_deref() {
        let (corr, ratio) = h.last_correlation();
        eprintln!(
            "mp2k: hooks={} stale={} bad_waves={} corr={corr:.3} ratio={ratio:.2} gain={:.2} mode={} pauses={} proven={} engaged={} active={}{}",
            h.hooks,
            h.stale_ticks,
            h.bad_waves,
            h.gain(),
            h.count_mode(),
            h.vf.pauses,
            h.vf.proven,
            h.engaged,
            h.active,
            h.vf.reverted
                .as_deref()
                .map(|m| format!(" PAUSED: {m}"))
                .unwrap_or_default()
        );
    }
    if let Some(g) = m.bus.gax.as_deref() {
        let (corr, ratio) = g.last_correlation();
        eprintln!(
            "gax: hooks={} stale={} bad_waves={} corr={corr:.3} ratio={ratio:.2} gain={:.2} pauses={} proven={} engaged={} active={}{}",
            g.hooks,
            g.stale_ticks,
            g.bad_waves,
            g.gain(),
            g.vf.pauses,
            g.vf.proven,
            g.engaged,
            g.active,
            g.vf.reverted
                .as_deref()
                .map(|m| format!(" PAUSED: {m}"))
                .unwrap_or_default()
        );
    }
    if let Some(r) = m.bus.rdrv.as_deref() {
        let (corr, ratio) = r.last_correlation();
        eprintln!(
            "rdrv: hooks={} stale={} bad_waves={} corr={corr:.3} ratio={ratio:.2} gain={:.2} pauses={} proven={} engaged={} active={}{}",
            r.hooks,
            r.stale_ticks,
            r.bad_waves,
            r.gain(),
            r.vf.pauses,
            r.vf.proven,
            r.engaged,
            r.active,
            r.vf.reverted
                .as_deref()
                .map(|m| format!(" PAUSED: {m}"))
                .unwrap_or_default()
        );
    }
    if let Some(prefix) = &dump_audio {
        write_wav(&format!("{prefix}.mixed.wav"), &mixed, gba_core::mem::AUDIO_RATE_HZ, 2)?;
        write_wav(&format!("{prefix}.psg.wav"), &psg, gba_core::mem::AUDIO_RATE_HZ, 2)?;
        if !hle.is_empty() {
            let pcm: Vec<i16> = hle
                .iter()
                .map(|v| (v * 32768.0).clamp(-32768.0, 32767.0) as i16)
                .collect();
            write_wav(&format!("{prefix}.hle.wav"), &pcm, gba_core::mem::AUDIO_RATE_HZ, 2)?;
        }
        for f in 0..2 {
            let mut raw = Vec::with_capacity(fifo_ev[f].len() * 5);
            for &(s, p) in &fifo_ev[f] {
                raw.push(s as u8);
                raw.extend_from_slice(&p.to_le_bytes());
            }
            let path = format!("{prefix}.fifo{f}.bin");
            std::fs::write(&path, raw).map_err(|e| format!("{path}: {e}"))?;
        }
        eprintln!(
            "audio dump: {} mixed stereo frames, fifo events A={} B={}, underruns A={} B={}, refills A={} B={}, pushes A={} B={}",
            mixed.len() / 2,
            fifo_ev[0].len(),
            fifo_ev[1].len(),
            m.bus.fifo_underruns[0],
            m.bus.fifo_underruns[1],
            m.bus.fifo_refills[0],
            m.bus.fifo_refills[1],
            m.bus.fifo_pushes[0],
            m.bus.fifo_pushes[1],
        );
    }

    // Triage: dump RAM snapshots for offline disassembly.
    if let Some(p) = std::env::var_os("RECOMP_DUMP_IWRAM") {
        std::fs::write(&p, &m.bus.iwram).map_err(|e| e.to_string())?;
        eprintln!("dumped iwram to {}", p.to_string_lossy());
    }
    // Triage: dump PPU state (<prefix>.{oam,vram,pal,io}) for offline
    // sprite/tile decoding.
    if let Some(p) = std::env::var_os("RECOMP_DUMP_PPU") {
        let p = p.to_string_lossy();
        std::fs::write(format!("{p}.oam"), &m.bus.oam).map_err(|e| e.to_string())?;
        std::fs::write(format!("{p}.vram"), &m.bus.vram).map_err(|e| e.to_string())?;
        std::fs::write(format!("{p}.pal"), &m.bus.palette).map_err(|e| e.to_string())?;
        std::fs::write(format!("{p}.io"), &m.bus.io).map_err(|e| e.to_string())?;
        eprintln!("dumped ppu state to {p}.*");
    }

    // FNV-1a over the framebuffer for cheap regression hashing.
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &px in &m.bus.framebuffer {
        for b in px.to_le_bytes() {
            hash ^= b as u64;
            hash = hash.wrapping_mul(0x1_0000_01b3);
        }
    }
    {
        use gba_core::Bus as _;
        let dispcnt = m.bus.read16(0x0400_0000);
        println!(
            "frames: {} hash: {hash:016x} dispcnt: {dispcnt:04x} pc: {:08x} mode: {:?} unhandled_swis: {:x}",
            m.bus.frames, m.cpu.regs[15], m.cpu.mode(), m.bus.unhandled_swis
        );
    }

    // Live disassembly around the final PC (reads through the bus, so
    // RAM-resident code is visible) — invaluable for stuck-loop triage.
    {
        use gba_core::Bus as _;
        let pc = m.cpu.regs[15];
        let start = pc.saturating_sub(12);
        for i in 0..8 {
            if m.cpu.thumb() {
                let a = start + i * 2;
                let h = m.bus.read16(a & !1);
                eprintln!("  {a:08x}: {}", decode_thumb(h, a & !1).disasm());
            } else {
                let a = start + i * 4;
                let w = m.bus.read32(a & !3);
                eprintln!("  {a:08x}: {}", decode_arm(w, a & !3).disasm());
            }
        }
    }

    if let Some(path) = out {
        let mut ppm = format!("P6\n240 160\n255\n").into_bytes();
        for &px in &m.bus.framebuffer {
            let r = (px & 31) as u8;
            let g = ((px >> 5) & 31) as u8;
            let b = ((px >> 10) & 31) as u8;
            ppm.extend_from_slice(&[r << 3 | r >> 2, g << 3 | g >> 2, b << 3 | b >> 2]);
        }
        std::fs::write(&path, ppm).map_err(|e| format!("{path}: {e}"))?;
        println!("wrote {path}");
    }
    Ok(())
}

/// Dev triage: report MP2K driver detection over a ROM or a directory
/// of ROMs (symlinks followed) — validates the signature scan against
/// the corpus and reports where each hook would land.
fn cmd_mp2k_scan(args: &[String]) -> Result<(), String> {
    let target = args.first().ok_or("missing ROM or directory")?;
    let path = Path::new(target);
    let mut files: Vec<std::path::PathBuf> = if path.is_dir() {
        std::fs::read_dir(path)
            .map_err(|e| format!("{target}: {e}"))?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "gba"))
            .collect()
    } else {
        vec![path.to_path_buf()]
    };
    files.sort();
    let (mut hits, mut total) = (0u32, 0u32);
    for f in &files {
        let rom = std::fs::read(f).map_err(|e| format!("{}: {e}", f.display()))?;
        total += 1;
        let sigs = gba_core::mp2k::detect(&rom);
        if sigs.is_empty() {
            println!("---- {}", file_name(f));
        } else {
            hits += 1;
            let entries = sigs
                .iter()
                .map(|sig| {
                    format!(
                        "{:#010x} ({})",
                        sig.sound_main_ram & !1,
                        if sig.sound_main_ram & 1 != 0 { "thumb" } else { "arm" },
                    )
                })
                .collect::<Vec<_>>()
                .join(" / ");
            println!(
                "MP2K {} SoundMain@{:#08x} SoundMainRAM={entries}",
                file_name(f),
                sigs[0].sound_main_off,
            );
        }
    }
    println!("\n{hits}/{total} images carry the stock MP2K driver signature");
    Ok(())
}


/// Arm the appropriate engine HLE shadow for this image (enhanced
/// path). Returns a description line for diagnostics.
fn arm_audio_hle(m: &mut Machine) -> String {
    match gba_core::engine::classify(&m.bus.rom) {
        gba_core::engine::Engine::M4a(sigs) => {
            let desc = format!(
                "M4A/MP2K — HLE shadow armed (SoundMainRAM {})",
                sigs.iter()
                    .map(|s| format!("{:#010x}", s.sound_main_ram & !1))
                    .collect::<Vec<_>>()
                    .join("/")
            );
            m.bus.mp2k = Some(Box::new(gba_core::mp2k::Mp2kHle::new(&sigs)));
            desc
        }
        gba_core::engine::Engine::Gax(ver) => {
            if let Some(sig) = gba_core::gax::detect_v1(&m.bus.rom) {
                let desc = "GAX v1 lineage — HLE shadow armed (state block located at runtime)"
                    .to_string();
                m.bus.gax = Some(Box::new(gba_core::gax::GaxHle::new(sig)));
                desc
            } else if ver.is_some() {
                // Banner era (v2/v3): the work block self-identifies at
                // runtime ('GAX3' magic + structural validation).
                m.bus.gax = Some(Box::new(gba_core::gax::GaxHle::new_v3()));
                format!(
                    "GAX {} — HLE shadow armed (work block located at runtime)",
                    ver.as_deref().unwrap_or("?")
                )
            } else {
                "GAX (early, unrecognized revision) — per-channel enhancement active".into()
            }
        }
        gba_core::engine::Engine::Rdiag => {
            if let Some(sig) = gba_core::rdrv::detect(&m.bus.rom) {
                m.bus.rdrv = Some(Box::new(gba_core::rdrv::RdrvHle::new(sig)));
                "in-house (R) — HLE shadow armed (mixer blob located at runtime)".into()
            } else {
                "in-house (R, unrecognized revision) — per-channel enhancement active".into()
            }
        }
        other => format!(
            "{} — per-channel enhancement active (engine HLE not yet available)",
            other.describe()
        ),
    }
}

/// Dev triage: audio-engine classification over a ROM or directory.
fn cmd_engine_scan(args: &[String]) -> Result<(), String> {
    let target = args.first().ok_or("missing ROM or directory")?;
    let path = Path::new(target);
    let mut files: Vec<std::path::PathBuf> = if path.is_dir() {
        std::fs::read_dir(path)
            .map_err(|e| format!("{target}: {e}"))?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "gba"))
            .collect()
    } else {
        vec![path.to_path_buf()]
    };
    files.sort();
    let mut tally: std::collections::BTreeMap<String, u32> = Default::default();
    for f in &files {
        let rom = std::fs::read(f).map_err(|e| format!("{}: {e}", f.display()))?;
        let engine = gba_core::engine::classify(&rom);
        let d = engine.describe();
        *tally.entry(d.split(' ').next().unwrap_or("?").to_string()).or_default() += 1;
        println!("{:24} {}", d, file_name(f));
    }
    println!();
    for (k, v) in tally {
        println!("{v:4}  {k}");
    }
    Ok(())
}

fn file_name(p: &Path) -> &str {
    p.file_name().and_then(|s| s.to_str()).unwrap_or("?")
}

fn check_entry(path: &Path) -> Result<String, String> {
    let mut header = [0u8; 4];
    let rom = std::fs::read(path).map_err(|e| e.to_string())?;
    header.copy_from_slice(rom.get(..4).ok_or("ROM shorter than 4 bytes")?);
    let word = u32::from_le_bytes(header);
    let instr = decode_arm(word, ROM_BASE);
    match instr.op {
        Op::Branch { link: false, target } => Ok(format!("{word:08x} -> {target:08x}")),
        _ => Err(format!("{word:08x} decoded as {:?}", instr.disasm())),
    }
}

/// Interactive play: window + keyboard via minifb, 60 Hz pacing, and
/// .sav persistence next to the image.
///
/// Keys: arrows = D-pad, Z = A, X = B, Enter = Start, RShift = Select,
/// A = L, S = R, Esc = quit.
fn cmd_play(args: &[String]) -> Result<(), String> {
    use minifb::{Key, Window, WindowOptions};

    let mut rom_path = None;
    let mut interp_only = false;
    // Machine-readable lifecycle lines on stdout for a supervising
    // frontend (the launcher): `STATUS building <pct>` / `STATUS playing`.
    let mut status = false;
    // Perf instrumentation is developer tooling: always on in debug
    // builds, opt-in (--stats or GBA_RECOMP_STATS=1) in release — the
    // out-of-box experience stays clean.
    let mut show_stats =
        cfg!(debug_assertions) || std::env::var_os("GBA_RECOMP_STATS").is_some();
    let mut bios_arg: Option<String> = None;
    let mut no_bios = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--interp" => interp_only = true,
            "--status" => status = true,
            "--stats" => show_stats = true,
            "--record-labels" => {
                FALLBACK_COLLECT.store(true, std::sync::atomic::Ordering::Relaxed)
            }
            "--bios" => bios_arg = Some(it.next().ok_or("--bios needs a value")?.to_string()),
            "--no-bios" => no_bios = true,
            other if rom_path.is_none() => rom_path = Some(other.to_string()),
            other => return Err(format!("unexpected argument {other:?}")),
        }
    }
    // Packaged mode: a manifest beside the executable pins this binary
    // to one title. The ROM is resolved by content hash (the package
    // ships none), the BIOS pin is enforced, and the translation loads
    // from the package instead of the build cache.
    let manifest = match packfile::load() {
        Some(Ok(m)) => Some(m),
        Some(Err(e)) => return Err(e),
        None => None,
    };
    let rom_path = match (&rom_path, &manifest) {
        (Some(p), _) => p.clone(),
        (None, Some(m)) => {
            let p = packfile::find_rom(m)?;
            eprintln!("{}: playing {}", m.name, p.display());
            p.to_string_lossy().into_owned()
        }
        (None, None) => return Err("missing ROM path".into()),
    };
    let rom = std::fs::read(&rom_path).map_err(|e| format!("{rom_path}: {e}"))?;
    if let Some(m) = &manifest {
        let sha = rom_sha256(&rom);
        if sha != m.rom_sha256 {
            return Err(format!(
                "this package plays exactly one cartridge image \
(sha256 {}…); {rom_path} hashes to {}…",
                &m.rom_sha256[..16],
                &sha[..16]
            ));
        }
    }
    let sav_path = format!("{}.sav", rom_path.trim_end_matches(".gba"));

    // Product boot path: real BIOS when an image is installed (explicit
    // --bios > the launcher-installed image via the shared resolution),
    // BIOS HLE otherwise. An explicit --bios that fails to load is a
    // hard error; a discovered image that fails is loud but non-fatal —
    // the player still gets a working session.
    let bios: Option<Vec<u8>> = if no_bios {
        eprintln!("boot: BIOS HLE (--no-bios)");
        None
    } else if let Some(p) = &bios_arg {
        let b = load_bios_file(p)?;
        eprintln!("boot: real BIOS ({p})");
        Some(b)
    } else if let Some(p) = input_config::find_bios() {
        match load_bios_file(&p) {
            Ok(b) => {
                eprintln!("boot: real BIOS ({})", p.display());
                Some(b)
            }
            Err(e) => {
                eprintln!("DEGRADED: installed BIOS unusable ({e}); using BIOS HLE");
                None
            }
        }
    } else {
        eprintln!("boot: BIOS HLE (no BIOS image installed)");
        None
    };
    if let Some(b) = &bios {
        use sha2::{Digest, Sha256};
        let sha = Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect::<String>();
        if let Some(m) = &manifest {
            if sha != m.bios_sha256 {
                return Err(format!(
                    "this package requires the pinned BIOS (sha256 {}…); the installed \
image hashes to {}…",
                    &m.bios_sha256[..16],
                    &sha[..16]
                ));
            }
        } else if sha != input_config::BIOS_SHA256 {
            eprintln!("bios: image is not the canonical dump (sha256 {sha}) — trying it as-is");
        }
    } else if let Some(m) = &manifest {
        return Err(format!(
            "this package requires a BIOS image (sha256 {}…) and none is installed.\n\
Place your own dump as gba_bios.bin next to the executable:\n  {}",
            &m.bios_sha256[..16],
            m.dir.display()
        ));
    }

    // Native translation. Packaged: the prebuilt library ships in the
    // package — under interpreter=false (a *full recomp*) failing to
    // load it is fatal, and so is any dispatch miss later. Developer
    // path: per-user cache, built on first launch. The product bar is
    // full speed at full accuracy — interpreter fallback is a defect
    // surface, not a graceful mode: it exists so a translation failure
    // stays loudly diagnosable instead of crashing, and the loop
    // alarms if speed drops below realtime either way.
    let native = if let Some(m) = &manifest {
        if interp_only && !m.interpreter {
            return Err("--interp is not available in a full-recomp package".into());
        }
        if !m.interpreter {
            NO_INTERP.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let lib_path = m.dir.join(&m.translation);
        match load_native(&lib_path).map(|v| {
            eprintln!("native translation: {} blocks (packaged)", v.1.len);
            v
        }) {
            Ok(v) if !interp_only => Some(v),
            Ok(_) => None,
            Err(e) if !m.interpreter => return Err(e),
            Err(e) => {
                eprintln!("DEGRADED: packaged translation unavailable ({e}); interpreter only");
                None
            }
        }
    } else if interp_only {
        None
    } else {
        match ensure_native(&rom_path, &rom, status, bios.as_deref()) {
            Ok(v) => Some(v),
            Err(e) => {
                eprintln!("DEGRADED: native translation unavailable ({e}); interpreter only");
                None
            }
        }
    };

    let mut m = make_machine(rom, bios.as_deref());
    if let Ok(sav) = std::fs::read(&sav_path) {
        m.bus.load_save_data(&sav);
        eprintln!("loaded {sav_path}");
    }

    // Triage (RECOMP_RECORD_INPUT=<path>): record this session's input as
    // a replayable script (frames/runc/verify --input). The save state at
    // session start is snapshotted next to it so the replay boots from
    // identical backup media.
    let mut record: Option<(std::fs::File, u16)> = match std::env::var_os("RECOMP_RECORD_INPUT") {
        None => None,
        Some(p) => {
            use std::io::Write;
            let p = p.to_string_lossy().into_owned();
            if std::path::Path::new(&sav_path).is_file() {
                let _ = std::fs::copy(&sav_path, format!("{p}.sav"));
            }
            let mut f = std::fs::File::create(&p).map_err(|e| format!("{p}: {e}"))?;
            writeln!(f, "gba-input v1").map_err(|e| e.to_string())?;
            eprintln!("recording input to {p}");
            Some((f, 0x3FF))
        }
    };

    let title = Path::new(&rom_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("recomp")
        .to_string();
    let mut window = Window::new(
        &title,
        240 * 3,
        160 * 3,
        WindowOptions { resize: true, ..WindowOptions::default() },
    )
    .map_err(|e| e.to_string())?;
    // Pacing is audio-clock driven (below), not display driven: a 120 Hz
    // or throttled window must not change game speed.
    window.set_target_fps(0);
    if status {
        status_line("playing");
    }


    // Audio. The A/V config selects the output path: hardware-faithful
    // zero-order hold of the mixed 65536 Hz tap, or the premium path
    // (final product name TBD) — each Direct Sound channel interpolated
    // from its own timer-derived rate straight to the device rate, PSG
    // band-limited from its grid, summed in float behind a DC blocker.
    let av = input_config::AvConfig::load();
    m.bus.tap_channels = av.audio_enhanced;
    // MP2K HLE (Stage 3): arm the shadow mixer when the stock driver
    // is present. It engages on the first live driver tick and reverts
    // loudly if its output stops matching the canon FIFO stream.
    if av.audio_enhanced {
        let desc = arm_audio_hle(&mut m);
        eprintln!("audio engine: {desc}");
    }
    let streams = std::sync::Arc::new(std::sync::Mutex::new(AudioStreams::default()));
    let _stream = start_audio(streams.clone(), av.audio_enhanced);

    // Bindings come from the launcher-managed config (defaults match the
    // historical hardcoded map). Keyboard always works; a configured
    // gamepad is OR-ed in on top.
    let icfg = input_config::InputConfig::load();
    let defaults = input_config::InputConfig::default();
    let key_pairs: Vec<(Key, u16)> = input_config::Button::ALL
        .iter()
        .map(|b| {
            let name = &icfg.keys[b.index()];
            let key = minifb_key(name).unwrap_or_else(|| {
                eprintln!("input.cfg: unknown key {name:?} for {} — using default", b.name());
                minifb_key(&defaults.keys[b.index()]).unwrap()
            });
            (key, b.bit())
        })
        .collect();
    let mut pad = match icfg.device {
        input_config::Device::Gamepad => gilrs::Gilrs::new().ok(),
        input_config::Device::Keyboard => None,
    };

    // Screen simulation (see crates/screen): temporal response on the
    // emulated-frame stream, color LUT to the display's colorspace, and a
    // GPU grid/scaling pass on this same window. Settings come from the
    // launcher-managed A/V config; unknown values fall back loudly.
    let screen_kind = screen::ScreenKind::by_name(&av.screen).unwrap_or_else(|| {
        eprintln!("av.cfg: unknown video.screen {:?} — using frontlit", av.screen);
        screen::ScreenKind::Frontlit
    });
    let display_target = screen::DisplayTarget::by_name(&av.display_gamut)
        .unwrap_or_else(|| {
            eprintln!("av.cfg: unknown video.gamut {:?} — using auto", av.display_gamut);
            screen::DisplayTarget::Auto
        });
    let lut = screen::ColorLut::build(&screen::ColorSettings {
        screen: screen_kind,
        darken: input_config::AvConfig::knob(&av.screen_darken)
            .map(f64::from)
            .unwrap_or(f64::NAN),
        target: display_target,
    });
    let response = screen::ResponseMode::by_name(&av.response).unwrap_or_else(|| {
        eprintln!("av.cfg: unknown video.response {:?} — using smart", av.response);
        screen::ResponseMode::Smart
    });
    let mut temporal = screen::Temporal::new(
        240 * 160,
        response,
        0,
        input_config::AvConfig::knob(&av.response_keep)
            .unwrap_or_else(|| screen::blend::default_rho(screen_kind)),
    );
    let grid_params = screen::present::GridParams::with_strength(
        input_config::AvConfig::knob(&av.grid)
            .unwrap_or_else(|| screen_kind.default_grid_strength()),
    );
    // GPU presenter on this window; if unavailable, the minifb CPU blit
    // below still carries the full color + response simulation (no grid).
    let mut presenter =
        match screen::present::Presenter::new(&window, 240, 160, display_target) {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!("video: GPU presenter unavailable ({e}); basic scaling active");
                None
            }
        };

    let mut buf = vec![0u32; 240 * 160];
    let mut rgba = vec![[0u8; 4]; 240 * 160];
    let mut emu_ema_ms = 0.0f64;
    let mut frames_run = 0u64;
    let mut slow_warned = false;
    let mut native_run = 0u64;
    let mut fallback_run = 0u64;
    let mut sav_seen = m.bus.save_data().map(fnv64);
    let mut next_frame = std::time::Instant::now();
    while window.is_open() && !window.is_key_down(Key::Escape) {
        // KEYINPUT is active-low.
        let mut keys = 0x3FFu16;
        for (key, bit) in &key_pairs {
            if window.is_key_down(*key) {
                keys &= !bit;
            }
        }
        if let Some(g) = pad.as_mut() {
            while g.next_event().is_some() {}
            let chosen = g
                .gamepads()
                .find(|(_, gp)| gp.name() == icfg.gamepad_name)
                .or_else(|| g.gamepads().next());
            if let Some((_, gp)) = chosen {
                for b in input_config::Button::ALL {
                    if pad_pressed(&gp, &icfg.pads[b.index()]) {
                        keys &= !b.bit();
                    }
                }
                // left stick doubles as the D-pad
                let (x, y) = (
                    gp.value(gilrs::Axis::LeftStickX),
                    gp.value(gilrs::Axis::LeftStickY),
                );
                if x > 0.5 { keys &= !(1 << 4); }
                if x < -0.5 { keys &= !(1 << 5); }
                if y > 0.5 { keys &= !(1 << 6); }
                if y < -0.5 { keys &= !(1 << 7); }
            }
        }
        m.bus.keys = keys;
        if let Some((f, last)) = &mut record {
            if keys != *last {
                use std::io::Write;
                let _ = writeln!(f, "{} {keys:03x}", m.bus.frames);
                *last = keys;
            }
        }

        // Audio-clock pacing: the device callback consumes at a fixed
        // rate, so run exactly as many frames as the ring needs —
        // production follows consumption, immune to display refresh
        // rate (ProMotion) and window-server throttling. Without an
        // audio device, wall-clock pace at the hardware frame rate.
        let mut ran = 0u32;
        while ran < 4 {
            if _stream.is_some() {
                let fill = {
                    let st = streams.lock().unwrap();
                    if av.audio_enhanced { st.psg.len() / 2 } else { st.mixed.len() / 2 }
                };
                if fill >= RING_TARGET {
                    break;
                }
            } else if ran >= 1 {
                break;
            }
            ran += 1;

            let emu_t0 = std::time::Instant::now();
            match &native {
                Some((_lib, table)) => {
                    let mptr = &mut m as *mut Machine as *mut std::ffi::c_void;
                    let (n, f) = run_frame_native(&mut m, table, mptr, 5_000_000);
                    native_run += n;
                    fallback_run += f;
                }
                None => m.run_frame(5_000_000),
            }
            // Advance the screen's temporal response once per EMULATED
            // frame — never per presented frame, or flicker-transparency
            // titles lock onto one phase (the classic parity bug).
            temporal.push(&m.bus.framebuffer, &lut);
            // Realtime alarm: the frame budget is 16.7 ms. If smoothed
            // emulation cost approaches it, the product promise is broken —
            // say so once, with the number.
            let dt_ms = emu_t0.elapsed().as_secs_f64() * 1e3;
            emu_ema_ms = if frames_run == 0 { dt_ms } else { emu_ema_ms * 0.95 + dt_ms * 0.05 };
            frames_run += 1;
            if !slow_warned && frames_run > 120 && emu_ema_ms > 15.0 {
                slow_warned = true;
                eprintln!(
                    "DEGRADED: emulation averaging {emu_ema_ms:.1} ms/frame \
                     (budget 16.7 ms) — below native speed"
                );
            }

            // Live perf readout in the title, once a second: emulation cost
            // against the 16.7 ms budget and the native-dispatch share.
            // Developer tooling — hidden from the release out-of-box
            // experience unless explicitly requested.
            if show_stats && frames_run % 60 == 0 {
                let total = native_run + fallback_run;
                let share = if total == 0 { 0.0 } else { native_run as f64 * 100.0 / total as f64 };
                window.set_title(&format!(
                    "recomp · {emu_ema_ms:.2} ms/frame ({:.1}x headroom) · native {share:.0}%",
                    16.7 / emu_ema_ms.max(0.01),
                ));
                native_run = 0;
                fallback_run = 0;
            }

            // Audio defect surfacing: ring drops and underrun holds are
            // silent corruption — say so, with numbers, per the product bar.
            if frames_run % 300 == 0 {
                let (d, h, c) = {
                    let mut st = streams.lock().unwrap();
                    let v = (st.drops, st.holds, st.clipped);
                    st.drops = 0;
                    st.holds = 0;
                    st.clipped = 0;
                    v
                };
                if d + h > 0 {
                    eprintln!(
                        "DEGRADED: audio ring dropped {d} / held {h} sample pairs in the last 5 s"
                    );
                }
                // Soft-knee engagement is canon clipping handled better,
                // not a defect — visible only in developer stats.
                if show_stats && c > 0 {
                    eprintln!("audio: soft clip engaged on {c} sample pairs in the last 5 s");
                }
            }

            // Flush backup media periodically when it changed, so an external
            // teardown (launcher exit kills us) can't lose more than ~5 s of
            // save progress.
            if frames_run % 300 == 0 {
                if let Some(data) = m.bus.save_data() {
                    let h = fnv64(data);
                    if sav_seen != Some(h) {
                        if std::fs::write(&sav_path, data).is_ok() {
                            sav_seen = Some(h);
                        }
                    }
                }
            }

            // Hand this frame's audio to the consumer (caps ~250 ms each),
            // refresh the routing snapshot, and count anything dropped.
            {
                use gba_core::Bus as _;
                let cnt_h = m.bus.read16(0x0400_0082);
                let mut st = streams.lock().unwrap();
                st.route = [(cnt_h >> 8 & 3) as u8, (cnt_h >> 12 & 3) as u8];
                st.vol_half = [cnt_h & 4 == 0, cnt_h & 8 == 0];
                if av.audio_enhanced {
                    m.bus.audio_buf.clear();
                    for pair in m.bus.psg_tap.chunks_exact(2) {
                        if st.psg.len() < 65536 {
                            st.psg.push_back(pair[0] as f32 / 32768.0);
                            st.psg.push_back(pair[1] as f32 / 32768.0);
                        } else {
                            st.drops += 1;
                        }
                    }
                    m.bus.psg_tap.clear();
                    // MP2K HLE stream and liveness. A pause is a
                    // diagnostic event (the crossfade keeps it
                    // inaudible) — say so, with the reason, but don't
                    // let a hopeless variant spam the log forever.
                    let live = m.bus.mp2k.as_deref().is_some_and(|h| h.live())
                        || m.bus.gax.as_deref().is_some_and(|g| g.live())
                        || m.bus.rdrv.as_deref().is_some_and(|r| r.live());
                    let pause_msg = if let Some(h) = m.bus.mp2k.as_deref_mut() {
                        h.vf.reverted.take().map(|m| (m, h.vf.pauses))
                    } else if let Some(g) = m.bus.gax.as_deref_mut() {
                        g.vf.reverted.take().map(|m| (m, g.vf.pauses))
                    } else if let Some(r) = m.bus.rdrv.as_deref_mut() {
                        r.vf.reverted.take().map(|m| (m, r.vf.pauses))
                    } else {
                        None
                    };
                    if let Some((msg, pauses)) = pause_msg {
                        if pauses <= 3 {
                            eprintln!(
                                "DEGRADED: engine HLE paused (probing continues): {msg}"
                            );
                        } else if pauses == 4 {
                            eprintln!(
                                "DEGRADED: engine HLE paused again: {msg} (further pauses unlogged)"
                            );
                        }
                    }
                    // Feed the HLE ring only while live — an unproven
                    // stream must neither pile up (drop spam) nor leave
                    // stale audio for the switchover.
                    if live {
                        for pair in m.bus.hle_tap.chunks_exact(2) {
                            if st.hle.len() < 65536 {
                                st.hle.push_back(pair[0]);
                                st.hle.push_back(pair[1]);
                            } else {
                                st.drops += 1;
                            }
                        }
                    } else {
                        st.hle.clear();
                    }
                    m.bus.hle_tap.clear();
                    // Switch over only once the HLE ring is primed; the
                    // callback crossfades both directions, and the
                    // per-channel path stays warm below.
                    if !live {
                        st.hle_on = false;
                    } else if st.hle.len() / 2 >= RING_TARGET / 2 {
                        st.hle_on = true;
                    }
                    for f in 0..2 {
                        let events = std::mem::take(&mut m.bus.fifo_tap[f]);
                        for e in events {
                            if st.fifo[f].len() < 32768 {
                                st.fifo[f].push_back(e);
                            } else {
                                st.drops += 1;
                            }
                        }
                    }
                } else {
                    for pair in m.bus.audio_buf.chunks_exact(2) {
                        if st.mixed.len() < 65536 {
                            st.mixed.push_back(pair[0]);
                            st.mixed.push_back(pair[1]);
                        } else {
                            st.drops += 1;
                        }
                    }
                    m.bus.audio_buf.clear();
                }
            }
        }

        if ran == 0 {
            // Ring is full: let the device drain a little, pump events.
            std::thread::sleep(std::time::Duration::from_millis(2));
            window.update();
            continue;
        }
        if _stream.is_none() {
            next_frame += std::time::Duration::from_nanos(16_742_706); // 59.7275 Hz
            let now = std::time::Instant::now();
            if next_frame > now {
                std::thread::sleep(next_frame - now);
            } else {
                next_frame = now;
            }
        }

        // Resolve the visible image (temporal response + color LUT) and
        // present: GPU grid/scaling pass when available, otherwise the
        // plain minifb blit (still fully color-corrected).
        temporal.compose(&lut, &mut rgba);
        if let Some(p) = presenter.as_mut() {
            let phys = p.physical_size(window.get_size());
            match p.present(&rgba, phys, &grid_params) {
                Ok(()) => {
                    window.update();
                    continue;
                }
                Err(e) => {
                    eprintln!("video: GPU presenter failed ({e}); basic scaling active");
                    presenter = None;
                }
            }
        }
        for (dst, px) in buf.iter_mut().zip(rgba.iter()) {
            *dst = (px[0] as u32) << 16 | (px[1] as u32) << 8 | px[2] as u32;
        }
        window.update_with_buffer(&buf, 240, 160).map_err(|e| e.to_string())?;
    }

    if let Some(data) = m.bus.save_data() {
        std::fs::write(&sav_path, data).map_err(|e| format!("{sav_path}: {e}"))?;
        eprintln!("saved {sav_path}");
    }
    print_fallback_census();
    if FALLBACK_COLLECT.load(std::sync::atomic::Ordering::Relaxed) {
        record_labels(m.bus.rom.len(), &rom_sha256(&m.bus.rom))?;
    }
    Ok(())
}

/// One machine-readable lifecycle line on stdout, for the launcher
/// supervising a `play --status` child. Flushed immediately: the reader
/// is a pipe, and a buffered line is a frozen progress bar.
fn status_line(s: &str) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "STATUS {s}");
    let _ = out.flush();
}

/// Translation cache revision. Bump on ANY change that can invalidate a
/// cached translation: emitter output, runtime ABI, or emulation
/// semantics — `--ram` builds bake interpreter-profiled state (seeds +
/// RAM snapshots) into the translation, so HLE/core fixes count too.
/// Rev history: 1 = initial; 2 = HuffUnComp HLE, whole-block RAM
/// guards, conditional-BL link fix; 3 = MidiKey2Freq HLE + sound
/// FIFO/DMA timing fixes (profiled state shifts); 4 = SWI ends blocks
/// (halt ordering vs following instructions — scanline-effect timing).
const TRANSLATION_REV: u32 = 5;

/// Locate (or build) the cached native translation for this image.
/// Cache key is the ROM's SHA-256 under the `TRANSLATION_REV` directory,
/// so stale natives can never load; superseded revision dirs are swept.
/// With `status`, build progress is reported as `STATUS building <pct>`.
fn ensure_native(
    rom_path: &str,
    rom: &[u8],
    status: bool,
    bios: Option<&[u8]>,
) -> Result<(libloading::Library, BlockTable), String> {
    let sha = rom_sha256(rom);
    let base = dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("gba-recomp");
    let dir = base.join(format!("t{TRANSLATION_REV}"));
    // Sweep superseded revision directories — they can only hold stale
    // translations no current binary will ever load.
    if let Ok(entries) = std::fs::read_dir(&base) {
        for e in entries.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('t')
                && name[1..].chars().all(|c| c.is_ascii_digit())
                && *name != *format!("t{TRANSLATION_REV}")
            {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    // Experimental EWRAM translations get their own cache entry so they
    // can never be loaded by (or shadow) a normal run.
    let mut suffix = if std::env::var_os("RECOMP_EWRAM").is_some() {
        "-e".to_string()
    } else {
        String::new()
    };
    // Real-BIOS translations bake region-0 code and different boot/SWI
    // semantics into the dylib, so they key on the BIOS content too —
    // swapping the installed BIOS rebuilds rather than loading stale
    // natives, and HLE/real artifacts can never shadow each other.
    if let Some(b) = bios {
        let bsha = rom_sha256(b);
        suffix.push_str(&format!("-b{}", &bsha[..8]));
    }
    // The label set (and the IWRAM snapshot backing it) participates
    // in the key: grown coverage retranslates on the next launch.
    let lbl = labels::load_all(rom_path, &sha, rom.len());
    let lsuffix = if lbl.is_empty() {
        String::new()
    } else {
        let blob = labels::Blob::load(&labels::blob_path(&sha));
        let d = lbl.digest(blob.as_ref());
        format!("-l{:08x}", d as u32 ^ (d >> 32) as u32)
    };
    let lib_path = dir.join(format!("{sha}{suffix}{lsuffix}.dylib"));
    let lib_str = lib_path.to_str().ok_or("non-UTF8 cache path")?;
    if !lib_path.is_file() {
        // Superseded translations of this image (older label sets) can
        // never be loaded again — sweep them before building anew.
        // Same suffix family only, so normal and experimental entries
        // never evict each other.
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let stale = name
                    .strip_prefix(&sha)
                    .and_then(|r| r.strip_prefix(suffix.as_str()))
                    .is_some_and(|r| r.starts_with('.') || r.starts_with("-l"));
                if stale {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        eprintln!("first launch: translating image (one-time)...");
        if status {
            status_line("building 0");
        }
        build_dylib(rom_path, true, bios, None, lib_str, &mut |pct, msg| {
            if status {
                status_line(&format!("building {pct} {msg}"));
            } else {
                term_progress(pct, msg);
            }
        })?;
        if !status {
            eprintln!();
        }
    }
    let v = load_native(&lib_path)?;
    eprintln!("native translation: {} blocks", v.1.len);
    Ok(v)
}

/// Load a translation library and its block table from an explicit
/// path (the cache, an out/ artifact, or a package).
fn load_native(lib_path: &Path) -> Result<(libloading::Library, BlockTable), String> {
    let lib = unsafe { libloading::Library::new(lib_path) }
        .map_err(|e| format!("{}: {e}", lib_path.display()))?;
    let table = BlockTable::load(&lib)?;
    Ok((lib, table))
}

/// Streams shared between the emulation loop (producer) and the cpal
/// callback (consumer). Faithful mode uses only `mixed`; enhanced mode
/// uses the per-channel taps. `mixed` and `psg` are interleaved stereo.
#[derive(Default)]
struct AudioStreams {
    mixed: std::collections::VecDeque<i16>,
    psg: std::collections::VecDeque<f32>,
    fifo: [std::collections::VecDeque<(i8, u32)>; 2],
    /// MP2K HLE shadow-mixer stereo (65536 Hz grid, hardware-rail
    /// units like `psg`). When `hle_on`, the callback substitutes this
    /// for the FIFO A/B channels; reverting mid-session falls straight
    /// back to the per-channel interpolators.
    hle: std::collections::VecDeque<f32>,
    hle_on: bool,
    /// SOUNDCNT_H routing snapshot for the enhanced path, refreshed
    /// each frame by the producer: bit 0 = right side, bit 1 = left.
    route: [u8; 2],
    /// SOUNDCNT_H 50%-volume flags per Direct Sound channel.
    vol_half: [bool; 2],
    /// Producer-side ring drops and consumer-side underrun holds since
    /// the last report. Silent audio defects are still defects — the
    /// play loop surfaces nonzero deltas as DEGRADED.
    drops: u64,
    holds: u64,
    /// Sample pairs that crossed the hardware rail (soft-knee engaged)
    /// since the last report. Expected behavior on hot mixes, not a
    /// defect — surfaced as a stats line, never DEGRADED.
    clipped: u64,
}

/// The premium path's consumer state: one interpolator per Direct Sound
/// channel at its own rate, PSG through the stereo grid resampler, and
/// a DC blocker per side.
struct Enhanced {
    psg: SincResampler,
    fifo: [FifoInterp; 2],
    /// MP2K HLE stream resampler — crossfaded against `fifo` as the
    /// shadow mixer proves/pauses.
    hle: SincResampler,
    /// Crossfade position (0 = per-channel path, 1 = HLE) and its
    /// per-sample slew (~40 ms time constant).
    xfade: f32,
    xfade_k: f32,
    dc: [DcBlock; 2],
}

/// Build the cpal output stream over the shared streams. `enhanced`
/// selects the premium output path; off reproduces the original
/// zero-order-hold behavior exactly.
fn start_audio(
    streams: std::sync::Arc<std::sync::Mutex<AudioStreams>>,
    enhanced: bool,
) -> Option<cpal::Stream> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    let device = cpal::default_host().default_output_device()?;
    let cfg = device.default_output_config().ok()?;
    let rate = cfg.sample_rate().0 as f64;
    let channels = cfg.channels() as usize;
    let src = gba_core::mem::AUDIO_RATE_HZ as f64;

    let mut eng = if enhanced {
        Some(Enhanced {
            psg: SincResampler::new(src, rate),
            fifo: [FifoInterp::new(rate), FifoInterp::new(rate)],
            hle: SincResampler::new(src, rate),
            xfade: 0.0,
            xfade_k: (1.0 / (0.040 * rate)) as f32,
            dc: [DcBlock::new(rate), DcBlock::new(rate)],
        })
    } else {
        None
    };
    let step = src / rate;
    let mut frac = 0.0f64;
    let mut last = (0i16, 0i16);
    // The hardware output is AC-coupled: DC at the DAC never reaches
    // the speaker. Model it on the faithful path too, or PSG duty
    // offsets thump on every note edge.
    let mut faithful_dc = [DcBlock::new(rate), DcBlock::new(rate)];
    let stream = device
        .build_output_stream(
            &cfg.into(),
            move |out: &mut [f32], _| {
                let mut st = streams.lock().unwrap();
                for frame in out.chunks_mut(channels) {
                    let (l, r) = match eng.as_mut() {
                        Some(e) => {
                            let (pl, pr) = e.psg.next(&mut st.psg);
                            let mut l = pl;
                            let mut r = pr;
                            // BOTH Direct Sound paths stay warm — the
                            // per-channel interpolators and the MP2K
                            // shadow stream — and a short equal-power-
                            // ish crossfade moves between them, so an
                            // HLE engage or pause is never an audible
                            // seam (both render the same music).
                            let a = e.fifo[0].next(&mut st.fifo[0])
                                * if st.vol_half[0] { 0.5 } else { 1.0 };
                            let b = e.fifo[1].next(&mut st.fifo[1])
                                * if st.vol_half[1] { 0.5 } else { 1.0 };
                            let (hl, hr) = e.hle.next(&mut st.hle);
                            let hr = hr * if st.vol_half[0] { 0.5 } else { 1.0 };
                            let hl = hl * if st.vol_half[1] { 0.5 } else { 1.0 };
                            let target = if st.hle_on { 1.0 } else { 0.0 };
                            e.xfade += (target - e.xfade) * e.xfade_k;
                            let xf = e.xfade;
                            let mut fl = 0.0;
                            let mut fr = 0.0;
                            if st.route[0] & 2 != 0 {
                                fl += a;
                            }
                            if st.route[0] & 1 != 0 {
                                fr += a;
                            }
                            if st.route[1] & 2 != 0 {
                                fl += b;
                            }
                            if st.route[1] & 1 != 0 {
                                fr += b;
                            }
                            let mut gl = 0.0;
                            let mut gr = 0.0;
                            if st.route[0] & 2 != 0 || st.route[1] & 2 != 0 {
                                gl = hl;
                            }
                            if st.route[0] & 1 != 0 || st.route[1] & 1 != 0 {
                                gr = hr;
                            }
                            l += fl * (1.0 - xf) + gl * xf;
                            r += fr * (1.0 - xf) + gr * xf;
                            // Stage 2: where the hardware would hard-clip
                            // the over-rail sum, saturate it softly
                            // instead — before the DC block, matching the
                            // hardware order (rail precedes the output
                            // coupling). Below the rail this is exact
                            // identity, so unclipped material is
                            // bit-identical to the pre-Stage-2 path.
                            if l.abs() > RAIL || r.abs() > RAIL {
                                st.clipped += 1;
                            }
                            (e.dc[0].next(soft_clip(l)), e.dc[1].next(soft_clip(r)))
                        }
                        None => {
                            // The emulated DAC runs on the 59.73 Hz frame
                            // grid while the window paces 60 — steer the
                            // faithful path too, or the ring overflows
                            // into a burst-drop buzz at the cap.
                            let pairs = (st.mixed.len() / 2) as f64;
                            let err = (pairs - RING_TARGET as f64) / RING_TARGET as f64;
                            frac += step * (1.0 + 0.02 * err.clamp(-1.0, 1.0));
                            while frac >= 1.0 {
                                frac -= 1.0;
                                match (st.mixed.pop_front(), st.mixed.pop_front()) {
                                    (Some(l), Some(r)) => last = (l, r),
                                    _ => st.holds += 1,
                                }
                            }
                            (
                                faithful_dc[0].next(last.0 as f32 / 32768.0),
                                faithful_dc[1].next(last.1 as f32 / 32768.0),
                            )
                        }
                    };
                    // Calibrate the hardware rail (±0x200 units = ±0.5
                    // here) toward device full scale. Both paths share
                    // the gain so the A/V toggle stays level-matched;
                    // it leaves ~2 dB above the rail for the enhanced
                    // path's soft knee. The clamp is a safety net for
                    // DC-block overshoot — faithful content was already
                    // rail-clipped at the mix (the canon hard clip).
                    let l = (l * OUT_GAIN).clamp(-1.0, 1.0);
                    let r = (r * OUT_GAIN).clamp(-1.0, 1.0);
                    match frame {
                        [m] => *m = (l + r) * 0.5,
                        [fl, fr, rest @ ..] => {
                            *fl = l;
                            *fr = r;
                            for ch in rest {
                                *ch = (l + r) * 0.5;
                            }
                        }
                        [] => {}
                    }
                }
            },
            |e| eprintln!("audio error: {e}"),
            None,
        )
        .ok()?;
    let _ = stream.play();
    Some(stream)
}

const SINC_TAPS: usize = 24;
const SINC_PHASES: usize = 128;
/// Ring fill the rate control steers toward (~62 ms at the 65536 Hz tap).
const RING_TARGET: usize = 4096;
/// Hardware clip rail in the float mix domain (±0x200 units = ±0.5).
const RAIL: f32 = 0.5;
/// Output gain calibrating the rail toward device full scale. 1.6 puts
/// the rail ~2 dB under full scale — the headroom the enhanced path's
/// soft knee lands over-rail peaks in.
const OUT_GAIN: f32 = 1.6;

/// Stage-2 soft-knee saturator (enhanced path only): exact identity
/// through the hardware rail, then a tanh knee compressing the over-rail
/// region (the six-channel sum can reach 3x the rail) into the headroom
/// above it, asymptoting at full scale after OUT_GAIN. C1 at the knee.
/// Faithful mode keeps the hard rail — clipping is sometimes the
/// intended texture (the research note's "GBA speaker" mono examples).
fn soft_clip(x: f32) -> f32 {
    let a = x.abs();
    if a <= RAIL {
        return x;
    }
    let h = 1.0 / OUT_GAIN - RAIL; // headroom above the rail
    (RAIL + h * ((a - RAIL) / h).tanh()).copysign(x)
}

/// Polyphase interpolation table: (SINC_PHASES + 1) rows of SINC_TAPS
/// coefficients, Hann-windowed sinc at cutoff `fc` (cycles per source
/// sample), each row normalized to unity DC gain.
fn build_sinc_table(fc: f64) -> Vec<f32> {
    use std::f64::consts::PI;
    let center = (SINC_TAPS / 2 - 1) as f64;
    let half = SINC_TAPS as f64 / 2.0;
    let mut table = Vec::with_capacity((SINC_PHASES + 1) * SINC_TAPS);
    for p in 0..=SINC_PHASES {
        let phi = p as f64 / SINC_PHASES as f64;
        let mut row = [0.0f64; SINC_TAPS];
        let mut sum = 0.0;
        for (k, c) in row.iter_mut().enumerate() {
            let t = k as f64 - center - phi;
            let sinc = if t.abs() < 1e-9 {
                2.0 * fc
            } else {
                (2.0 * PI * fc * t).sin() / (PI * t)
            };
            let win = if t.abs() < half { 0.5 * (1.0 + (PI * t / half).cos()) } else { 0.0 };
            *c = sinc * win;
            sum += *c;
        }
        table.extend(row.iter().map(|c| (c / sum) as f32));
    }
    table
}

/// One-pole DC blocker, fc ~ 10 Hz: y[n] = x[n] - x[n-1] + r*y[n-1].
/// Kills bias offsets, held-DAC ledges, and underrun pops.
struct DcBlock {
    r: f32,
    x1: f32,
    y1: f32,
}

impl DcBlock {
    fn new(out_hz: f64) -> DcBlock {
        DcBlock { r: 1.0 - (2.0 * std::f64::consts::PI * 10.0 / out_hz) as f32, x1: 0.0, y1: 0.0 }
    }

    fn next(&mut self, x: f32) -> f32 {
        let y = x - self.x1 + self.r * self.y1;
        self.x1 = x;
        self.y1 = y;
        y
    }
}

/// Fixed-ratio polyphase windowed-sinc resampler over an interleaved
/// stereo queue, with gentle (max ±2%) consumption-rate control against
/// the queue fill. Band-limits to the narrower of the two Nyquists, so
/// downsampling to a 44.1/48 kHz device does not fold images the way
/// the zero-order-hold path does. Used for the PSG grid stream.
struct SincResampler {
    table: Vec<f32>,
    /// Last SINC_TAPS source samples per side, oldest first.
    hist: [[f32; SINC_TAPS]; 2],
    ratio: f64,
    frac: f64,
}

impl SincResampler {
    fn new(src_hz: f64, out_hz: f64) -> SincResampler {
        // 10% under the narrower Nyquist leaves the window transition
        // band room.
        SincResampler {
            table: build_sinc_table(0.45 * (out_hz / src_hz).min(1.0)),
            hist: [[0.0; SINC_TAPS]; 2],
            ratio: src_hz / out_hz,
            frac: 0.0,
        }
    }

    fn next(&mut self, q: &mut std::collections::VecDeque<f32>) -> (f32, f32) {
        // Steer consumption toward the target fill; an underrun repeats
        // the newest pair (a one-sample ZOH the DC blocker smooths).
        let pairs = (q.len() / 2) as f64;
        let err = (pairs - RING_TARGET as f64) / RING_TARGET as f64;
        self.frac += self.ratio * (1.0 + 0.02 * err.clamp(-1.0, 1.0));
        while self.frac >= 1.0 {
            self.frac -= 1.0;
            let (l, r) = match (q.pop_front(), q.pop_front()) {
                (Some(l), Some(r)) => (l, r),
                _ => (self.hist[0][SINC_TAPS - 1], self.hist[1][SINC_TAPS - 1]),
            };
            for (h, s) in self.hist.iter_mut().zip([l, r]) {
                h.copy_within(1.., 0);
                h[SINC_TAPS - 1] = s;
            }
        }
        let row = &self.table[(self.frac * SINC_PHASES as f64) as usize * SINC_TAPS..][..SINC_TAPS];
        (
            row.iter().zip(self.hist[0].iter()).map(|(c, s)| c * s).sum(),
            row.iter().zip(self.hist[1].iter()).map(|(c, s)| c * s).sum(),
        )
    }
}

/// Renders one Direct Sound channel as band-limited steps (the blip-buf
/// idea): each DAC transition deposits a windowed-sinc impulse — the
/// step's derivative — into a small future ring at its exact fractional
/// output position, and a leaky integrator reconstructs the waveform.
/// Edges land jitter-free at the channel's own timer rate (which may
/// change at any event), and crucially the DAC staircase spectrum — the
/// ZOH images above the mix rate's Nyquist — survives up to the device
/// Nyquist. That brightness is part of the hardware's canon sound;
/// smooth sample interpolation removes it and audibly dulls transients
/// (reference-trace comparison on a square-timbre chime). Channels
/// driven above the device rate degrade gracefully: overlapping
/// deposits sum to a proper low-pass.
struct FifoInterp {
    /// Windowed-sinc impulse table (rows sum to 1 = unit step).
    table: Vec<f32>,
    /// Future deposits, ring-indexed by output sample.
    fut: [f32; SINC_TAPS * 2],
    pos: usize,
    /// Leaky integrator state. The tiny leak only bounds drift; real
    /// output coupling is the shared DC blocker downstream.
    y: f32,
    /// Current DAC level, for edge deltas.
    level: f32,
    /// Master cycles until the next DAC event is due.
    t_next: f64,
    /// Current source sample spacing in master cycles.
    period: f64,
    /// Master cycles per output sample.
    out_step: f64,
}

impl FifoInterp {
    fn new(out_hz: f64) -> FifoInterp {
        FifoInterp {
            table: build_sinc_table(0.45),
            fut: [0.0; SINC_TAPS * 2],
            pos: 0,
            y: 0.0,
            level: 0.0,
            t_next: 512.0,
            period: 512.0, // placeholder until the first DAC event
            out_step: (1u64 << 24) as f64 / out_hz,
        }
    }

    fn next(&mut self, q: &mut std::collections::VecDeque<(i8, u32)>) -> f32 {
        // Steer toward ~62 ms queued at the channel's current rate.
        let target = ((1u64 << 24) as f64 / self.period / 16.0).max(64.0);
        let err = (q.len() as f64 - target) / target;
        let stepc = self.out_step * (1.0 + 0.02 * err.clamp(-1.0, 1.0));

        self.t_next -= stepc;
        while self.t_next <= 0.0 {
            // The event fires inside this output sample, at fraction phi
            // of the sample period.
            let phi = (1.0 + self.t_next / stepc).clamp(0.0, 1.0);
            let new_level = match q.pop_front() {
                Some((s, p)) => {
                    self.period = (p as f64).max(1.0);
                    s as f32 * (64.0 / 32768.0)
                }
                None => self.level, // DAC holds on underrun
            };
            let delta = new_level - self.level;
            self.level = new_level;
            if delta != 0.0 {
                let row = &self.table
                    [(phi * SINC_PHASES as f64) as usize * SINC_TAPS..][..SINC_TAPS];
                for (k, c) in row.iter().enumerate() {
                    self.fut[(self.pos + k) % (SINC_TAPS * 2)] += delta * c;
                }
            }
            self.t_next += self.period;
        }

        self.y = self.y * 0.9999 + self.fut[self.pos];
        self.fut[self.pos] = 0.0;
        self.pos = (self.pos + 1) % (SINC_TAPS * 2);
        self.y
    }
}

/// Canonical key name (see input-config) to minifb key.
fn minifb_key(name: &str) -> Option<minifb::Key> {
    use minifb::Key::*;
    Some(match name {
        "A" => A, "B" => B, "C" => C, "D" => D, "E" => E, "F" => F, "G" => G,
        "H" => H, "I" => I, "J" => J, "K" => K, "L" => L, "M" => M, "N" => N,
        "O" => O, "P" => P, "Q" => Q, "R" => R, "S" => S, "T" => T, "U" => U,
        "V" => V, "W" => W, "X" => X, "Y" => Y, "Z" => Z,
        "0" => Key0, "1" => Key1, "2" => Key2, "3" => Key3, "4" => Key4,
        "5" => Key5, "6" => Key6, "7" => Key7, "8" => Key8, "9" => Key9,
        "Up" => Up, "Down" => Down, "Left" => Left, "Right" => Right,
        "Enter" => Enter, "Space" => Space, "Tab" => Tab,
        "Backspace" => Backspace,
        "LeftShift" => LeftShift, "RightShift" => RightShift,
        _ => return None,
    })
}

/// Gilrs button name (the launcher stores `{:?}` of the button) to state.
fn pad_pressed(gp: &gilrs::Gamepad, name: &str) -> bool {
    use gilrs::Button::*;
    let btn = match name {
        "South" => South, "East" => East, "North" => North, "West" => West,
        "C" => C, "Z" => Z,
        "LeftTrigger" => LeftTrigger, "LeftTrigger2" => LeftTrigger2,
        "RightTrigger" => RightTrigger, "RightTrigger2" => RightTrigger2,
        "Select" => Select, "Start" => Start, "Mode" => Mode,
        "LeftThumb" => LeftThumb, "RightThumb" => RightThumb,
        "DPadUp" => DPadUp, "DPadDown" => DPadDown,
        "DPadLeft" => DPadLeft, "DPadRight" => DPadRight,
        _ => return false,
    };
    gp.is_pressed(btn)
}

/// Statically recompile a ROM: analyze, emit C, compile to a shared
/// library with the system C compiler.
fn cmd_build(args: &[String]) -> Result<(), String> {
    let mut rom_path = None;
    let mut ram = false;
    let mut bios: Option<Vec<u8>> = None;
    let mut explicit_labels: Option<String> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--ram" => ram = true,
            "--bios" => bios = Some(load_bios_file(it.next().ok_or("--bios needs a value")?)?),
            "--labels" => {
                explicit_labels = Some(it.next().ok_or("--labels needs a value")?.to_string())
            }
            other if rom_path.is_none() => rom_path = Some(other.to_string()),
            other => return Err(format!("unexpected argument {other:?}")),
        }
    }
    let rom_path = &rom_path.ok_or("missing ROM path")?;
    std::fs::create_dir_all("out").map_err(|e| e.to_string())?;
    let stem = Path::new(rom_path)
        .file_stem().and_then(|s| s.to_str()).unwrap_or("game").to_string();
    // Real-BIOS builds get their own artifact: the translation bakes in
    // different boot/SWI semantics, so it must never shadow an HLE build.
    let suffix = if bios.is_some() { "-bios" } else { "" };
    let r = build_dylib(
        rom_path,
        ram,
        bios.as_deref(),
        explicit_labels.as_deref(),
        &format!("out/{stem}{suffix}.dylib"),
        &mut term_progress,
    );
    eprintln!();
    r
}

/// The build pipeline behind `cmd_build`, parameterized on the output
/// path so the play runtime can target its own translation cache.
/// Intermediates land next to `lib_path` as `<stem>.<i>.{c,o}`.
/// `progress` receives a whole-build percentage (monotonic, 0..=100);
/// phase weights are rough but the dominant compile phase is exact
/// (blocks compiled / total blocks).
fn build_dylib(
    rom_path: &str,
    ram: bool,
    bios: Option<&[u8]>,
    explicit_labels: Option<&str>,
    lib_path: &str,
    progress: &mut dyn FnMut(u8, &str),
) -> Result<(), String> {
    let rom = std::fs::read(rom_path).map_err(|e| format!("{rom_path}: {e}"))?;

    // Phase budget, weighted by image size: bigger images spend nearly
    // all their build inside the C compiler, so the profiling slice of
    // the bar shrinks as the image grows (the bar should track wall
    // time, not phase count).
    let rom_mb = ((rom.len() >> 20) as u64).max(1);
    let prof_end = (64 / rom_mb).clamp(4, 20) as u8;

    // Profile-guided RAM discovery: run the interpreter briefly, recording
    // control-transfer targets in EWRAM/IWRAM, then translate the observed
    // RAM-resident code from the end-of-run snapshot (content-guarded at
    // execution time).
    // RECOMP_EWRAM=1 (experimental): seed and translate EWRAM-resident
    // code too, under the same whole-block content guards as IWRAM.
    // Default-off pending the resident-vs-streamed-overlay measurements
    // for the labels design.
    let ewram_xlat = std::env::var_os("RECOMP_EWRAM").is_some();
    let (seeds, ewram, iwram) = if !ram {
        (Vec::new(), Vec::new(), Vec::new())
    } else {
        // Profile under the same boot semantics the output will run with:
        // a real-BIOS build profiles a real-BIOS run.
        let mut m = make_machine(rom.clone(), bios);
        let mut seeds = std::collections::BTreeSet::new();
        let mut prev_end = 0u32;
        let mut steps = 0u64;
        let mut last_pct = 0u8;
        let mut last_frame = u64::MAX;
        // Profile until the title has actually made sound, not a fixed
        // boot window: silent-boot titles install their IWRAM mixer
        // early but first execute it when music starts (often behind a
        // menu), which a 240-frame profile never sees. Demo input
        // drives the menus; engagement = the first nonzero audio
        // output sample (FIFO init-priming is silent zeros, so pushes
        // alone are a false signal), then a margin seeds the mixer's
        // hot paths. RAM-seed quiescence extends further if discovery
        // is still live; caps bound titles that never make sound.
        const PROFILE_MIN_FRAMES: u64 = 240;
        const PROFILE_MAX_FRAMES: u64 = 1800;
        const PROFILE_AUDIO_MARGIN: u64 = 120;
        let mut audio_from: Option<u64> = None;
        let mut audio_base: Option<i16> = None;
        // The window counts frames of *game* execution: under a real
        // BIOS the boot animation runs ~230 frames before the cart
        // entry is ever reached, which would otherwise consume the
        // whole window (measured: 0 entry points profiled). The boot
        // chime is also not the soundtrack — audio engagement only
        // counts after cart handoff.
        let mut cart_frame0: Option<u64> = None;
        loop {
            if cart_frame0.is_none() && m.cpu.regs[15] >= 0x0800_0000 {
                cart_frame0 = Some(m.bus.frames);
            }
            let game_frames = cart_frame0.map_or(0, |f0| m.bus.frames - f0);
            let done = cart_frame0.is_some()
                && (game_frames >= PROFILE_MAX_FRAMES
                    || audio_from.is_some_and(|f0| {
                        game_frames >= (f0 + PROFILE_AUDIO_MARGIN).max(PROFILE_MIN_FRAMES)
                    }));
            if done || steps >= 200_000_000 {
                break;
            }
            steps += 1;
            if m.bus.frames != last_frame {
                last_frame = m.bus.frames;
                m.bus.keys = demo_keys(game_frames);
                if cart_frame0.is_none() {
                    // Still in the BIOS logo: discard chime output so it
                    // can't trip the engagement detector.
                    m.bus.audio_buf.clear();
                    audio_base = None;
                } else if audio_from.is_none() && !m.bus.audio_buf.is_empty() {
                    // Engagement = output CHANGING, not merely nonzero
                    // (a constant bias level is still silence). The
                    // buffer must be drained: production self-caps
                    // when nothing consumes it.
                    let base = *audio_base.get_or_insert(m.bus.audio_buf[0]);
                    if m.bus.audio_buf.iter().any(|&s| s != base) {
                        audio_from = Some(game_frames);
                    }
                    m.bus.audio_buf.clear();
                }
                // The profiling slice of the bar; the endpoint moves
                // until audio engages, so estimate it (the report only
                // ever advances). The label says what we're waiting on.
                let est_end = match audio_from {
                    None => PROFILE_MAX_FRAMES,
                    Some(f0) => {
                        PROFILE_MAX_FRAMES.min((f0 + PROFILE_AUDIO_MARGIN).max(PROFILE_MIN_FRAMES))
                    }
                };
                let pct =
                    (game_frames * prof_end as u64 / est_end.max(1)).min(prof_end as u64) as u8;
                if pct > last_pct {
                    last_pct = pct;
                    let msg = match (audio_from, game_frames) {
                        (None, f) if f < 90 => "powering on the cartridge\u{2026}",
                        (None, _) => "running the intro, listening for the soundtrack\u{2026}",
                        (Some(_), _) => "soundtrack heard \u{2014} studying how it plays\u{2026}",
                    };
                    progress(pct, msg);
                }
            }
            // Seed observed control-transfer targets in IWRAM and ROM.
            // ROM targets recover code static traversal can't reach
            // (computed branches, handlers installed by RAM code) and
            // need no guard — ROM is immutable. IWRAM blocks are
            // content-guarded at execution time. EWRAM stays excluded
            // by default: it commonly holds streamed overlays, where
            // entry guards detect the swap but the hash-then-interpret
            // cycle is pure overhead; RECOMP_EWRAM opts in (resident
            // EWRAM engines, where the guard always passes).
            // Real-BIOS builds also seed observed BIOS entries: the
            // vectors + pointer sweep cover the static reachables, but
            // indirect returns (IntrWait re-entry, handler exits) are
            // only visible dynamically — and the BIOS is as immutable
            // as ROM, so they need no guard either.
            let seedable = |pc: u32| {
                pc >> 24 == 3
                    || (ewram_xlat && pc >> 24 == 2)
                    || (0x08..=0x0D).contains(&(pc >> 24))
                    || (bios.is_some() && pc < 0x4000)
            };
            let end = match m.step() {
                StepEvent::Instr(instr) => Some(instr.addr.wrapping_add(instr.size())),
                StepEvent::Idle => None,
            };
            let pc = m.cpu.regs[15];
            if pc != prev_end && seedable(pc) {
                seeds.insert(pc | m.cpu.thumb() as u32);
            }
            if let Some(e) = end {
                prev_end = e;
            }
        }
        println!("profiled {} RAM entry points over {} frames", seeds.len(), m.bus.frames);
        progress(
            prof_end,
            &format!("test drive done \u{2014} {} live code paths spotted", seeds.len()),
        );
        (seeds.into_iter().collect::<Vec<u32>>(), m.bus.ewram.clone(), m.bus.iwram.clone())
    };

    // Label-file seeds: runtime-discovered entry points (recorded
    // play/runc sessions, community files) join the profile seeds.
    // ROM entries are immutable and unguarded; IWRAM entries translate
    // from the recorder's local content snapshot, overlaid onto the
    // profile snapshot, and run behind the whole-block content guards
    // like every RAM-resident block.
    let sha = rom_sha256(&rom);
    let mut seeds = seeds;
    let mut iwram = iwram;
    let lbl = labels::load_all_with(rom_path, &sha, rom.len(), explicit_labels);
    if !lbl.is_empty() {
        let mut set: std::collections::BTreeSet<u32> = seeds.iter().copied().collect();
        let before = set.len();
        set.extend(&lbl.rom);
        // RECOMP_EXHAUSTIVE (full-recomp packaging): a soak proves the
        // covered paths, not all paths — computed jump-table targets
        // inside a function appear only when play reaches them. With
        // complete boundaries (`end` records from a mapper-grade set),
        // every decode point inside every mapped function becomes a
        // seed, so any computed target the game can ever take has a
        // native block. Pool words inside ranges translate to junk
        // blocks nothing dispatches to — size, not correctness.
        if std::env::var_os("RECOMP_EXHAUSTIVE").is_some() {
            let mut dense = 0usize;
            for (&key, &end) in &lbl.ends {
                if !(0x08..=0x0D).contains(&(key >> 24)) {
                    continue;
                }
                let step = if key & 1 != 0 { 2 } else { 4 };
                let thumb = key & 1;
                let mut a = key & !1;
                while a + step <= end {
                    set.insert(a | thumb);
                    a += step;
                }
                dense += 1;
            }
            println!(
                "labels: exhaustive in-function seeding over {dense} bounded functions"
            );
        }
        let rom_added = set.len() - before;
        let mut iw_added = 0usize;
        let mut iw_unbacked = 0usize;
        if !lbl.iwram.is_empty() {
            match labels::Blob::load(&labels::blob_path(&sha)) {
                Some(blob) => {
                    if iwram.is_empty() {
                        iwram = vec![0; labels::IWRAM_LEN];
                    }
                    for i in 0..labels::IWRAM_LEN {
                        if blob.mask[i] != 0 {
                            iwram[i] = blob.img[i];
                        }
                    }
                    for &key in &lbl.iwram {
                        if blob.valid_at(key) && set.insert(key) {
                            iw_added += 1;
                        }
                    }
                }
                None => iw_unbacked = lbl.iwram.len(),
            }
        }
        seeds = set.into_iter().collect();
        println!("labels: +{rom_added} rom, +{iw_added} iwram seeds");
        if iw_unbacked != 0 {
            println!(
                "labels: {iw_unbacked} iwram entries lack a local snapshot — \
run play/runc --record-labels to capture one"
            );
        }
    }

    // Real-BIOS translation seeds: the exception vectors, plus a sweep of
    // every aligned word in the image that looks like an in-BIOS code
    // pointer — the SWI dispatcher reaches its handlers through an
    // address table (an indexed load, not a PC-relative literal), which
    // recursive traversal alone cannot follow. Junk seeds are harmless:
    // unreached blocks just bloat the output (translate-everything).
    if let Some(b) = bios {
        seeds.extend([0x00u32, 0x08, 0x18]);
        let mut swept = 0usize;
        for off in (0..b.len() - 3).step_by(4) {
            let v = u32::from_le_bytes(b[off..off + 4].try_into().unwrap());
            let t = v & !1;
            if v != 0 && t < 0x4000 && (v & 1 == 1 || v & 3 == 0) {
                seeds.push(if v & 1 == 1 { t | 1 } else { t });
                swept += 1;
            }
        }
        println!("bios: seeded 3 vectors + {swept} swept pointer words");
    }

    let view = analyze::View {
        rom: &rom,
        ewram: if ram && ewram_xlat { Some(&ewram) } else { None },
        iwram: if !iwram.is_empty() { Some(&iwram) } else { None },
        bios,
    };
    progress(prof_end + 1, "charting every reachable code path\u{2026}");
    let analysis = analyze::analyze(&view, &seeds);
    let n_instrs: usize = analysis.blocks.iter().map(|b| b.instrs.len()).sum();
    println!("blocks: {} instructions: {n_instrs}", analysis.blocks.len());
    progress(
        prof_end + 2,
        &format!("map drawn: {} code blocks to translate", analysis.blocks.len()),
    );

    let prefix = lib_path.strip_suffix(".dylib").unwrap_or(lib_path);

    // Bounded translation units, compiled one at a time: full-image
    // translations can exceed the source image a hundredfold, and a single
    // huge unit makes cc balloon to many GB (parallel sweeps then exhaust
    // the machine). 16 MB of C keeps each cc invocation modest.
    const MAX_UNIT: usize = 16 << 20;
    let total_blocks = analysis.blocks.len().max(1);
    // Unit-count estimate up front (measured ~150 bytes of C per
    // instruction), so the compile labels can say "part 2 of ~9".
    let est_units = (n_instrs as u64 * 150 / MAX_UNIT as u64 + 1).max(1);
    let span_start = prof_end + 2;
    let span = 96 - span_start as u64;
    let mut objs: Vec<String> = Vec::new();
    let mut prev_done = 0usize;
    let chunks = emit::emit_c_chunked(&analysis, &view, MAX_UNIT, |c, blocks_done| {
        let i = objs.len();
        let p0 = span_start + (prev_done as u64 * span / total_blocks as u64) as u8;
        let p1 = span_start + (blocks_done as u64 * span / total_blocks as u64) as u8;
        prev_done = blocks_done;
        let c_path = format!("{prefix}.{i}.c");
        let o_path = format!("{prefix}.{i}.o");
        std::fs::write(&c_path, c).map_err(|e| e.to_string())?;
        // Compiling dominates the build, so the bar keeps moving while
        // cc works: poll the child and ease through this unit's span
        // against a size-based time estimate (measured ~0.7 MB of C
        // per second), never claiming the unit done early.
        let mut child = std::process::Command::new("cc")
            .args(["-O1", "-c", "-o", &o_path, &c_path])
            .spawn()
            .map_err(|e| format!("cc: {e}"))?;
        let est_secs = (c.len() as f64 / 700_000.0).max(1.0);
        let t0 = std::time::Instant::now();
        let mut spin = 0usize;
        let status = loop {
            match child.try_wait().map_err(|e| format!("cc: {e}"))? {
                Some(st) => break st,
                None => {
                    let frac = (t0.elapsed().as_secs_f64() / est_secs).min(0.95);
                    let pct = p0 + ((p1.saturating_sub(p0)) as f64 * frac) as u8;
                    let dots = ["\u{2026}", "\u{2026}\u{2026}", "\u{2026}\u{2026}\u{2026}"][spin % 3];
                    spin += 1;
                    progress(
                        pct,
                        &format!(
                            "forging native code \u{2014} part {} of ~{}{dots}",
                            i + 1,
                            est_units.max(i as u64 + 1)
                        ),
                    );
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
            }
        };
        // Triage: keep the emitted C around for inspection.
        if std::env::var_os("RECOMP_KEEP_C").is_none() {
            let _ = std::fs::remove_file(&c_path);
        }
        if !status.success() {
            return Err(format!("cc failed on {c_path}"));
        }
        objs.push(o_path);
        progress(p1, &format!("{blocks_done} of {total_blocks} blocks translated"));
        Ok(())
    })?;

    progress(97, "linking it all together\u{2026}");
    let status = std::process::Command::new("cc")
        .arg("-shared")
        .arg("-o")
        .arg(&lib_path)
        .args(&objs)
        .status()
        .map_err(|e| format!("cc: {e}"))?;
    for o in &objs {
        let _ = std::fs::remove_file(o);
    }
    if !status.success() {
        return Err("link failed".into());
    }
    progress(100, "ready to play");
    println!("wrote {lib_path} ({chunks} units)");
    Ok(())
}

type BlockFn = extern "C" fn(*const gba_core::capi::RtApi, *mut std::ffi::c_void) -> u32;

/// Direct-indexed block lookup. Keys are `guest address | thumb bit`;
/// nearly all live in the ROM window (and IWRAM for --ram builds), so
/// those get dense tables — lookup is a subtract, compare, and load —
/// with a HashMap holding any stragglers.
struct BlockTable {
    rom: Vec<Option<BlockFn>>,
    iwram: Vec<Option<BlockFn>>,
    /// Dense EWRAM table, allocated only when an EWRAM block exists
    /// (experimental RECOMP_EWRAM builds).
    ewram: Vec<Option<BlockFn>>,
    /// Dense BIOS table (region 0), allocated only for --bios builds.
    bios: Vec<Option<BlockFn>>,
    other: std::collections::HashMap<u32, BlockFn>,
    len: usize,
}

const IWRAM_BASE: u32 = 0x0300_0000;
const EWRAM_BASE: u32 = 0x0200_0000;

impl BlockTable {
    fn load(lib: &libloading::Library) -> Result<BlockTable, String> {
        use gba_core::capi::RcgBlock;
        let blocks: &[RcgBlock] = unsafe {
            let blocks: libloading::Symbol<*const RcgBlock> =
                lib.get(b"rcg_blocks").map_err(|e| e.to_string())?;
            let count: libloading::Symbol<*const u64> =
                lib.get(b"rcg_block_count").map_err(|e| e.to_string())?;
            std::slice::from_raw_parts(*blocks, **count as usize)
        };
        let mut rom_max = 0usize;
        for b in blocks {
            let r = b.key.wrapping_sub(ROM_BASE) as usize;
            if r < 0x0200_0000 {
                rom_max = rom_max.max(r + 1);
            }
        }
        let any_ewram = blocks
            .iter()
            .any(|b| (b.key.wrapping_sub(EWRAM_BASE) as usize) < 0x4_0000);
        let any_bios = blocks.iter().any(|b| b.key < 0x4000);
        let mut t = BlockTable {
            rom: vec![None; rom_max],
            iwram: vec![None; 0x8000],
            ewram: vec![None; if any_ewram { 0x4_0000 } else { 0 }],
            bios: vec![None; if any_bios { 0x4000 } else { 0 }],
            other: std::collections::HashMap::new(),
            len: blocks.len(),
        };
        // Diagnostic: RECOMP_IWRAM_MAX=<hex> drops IWRAM natives above
        // the threshold at load time (cheap block bisection, no rebuild).
        let iwram_max = std::env::var("RECOMP_IWRAM_MAX")
            .ok()
            .and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok());
        for b in blocks {
            let r = b.key.wrapping_sub(ROM_BASE) as usize;
            let w = b.key.wrapping_sub(IWRAM_BASE) as usize;
            if (b.key as usize) < t.bios.len() {
                t.bios[b.key as usize] = Some(b.func);
            } else if r < t.rom.len() {
                t.rom[r] = Some(b.func);
            } else if w < t.iwram.len() {
                if let Some(mx) = iwram_max {
                    if b.key & !1 > mx {
                        continue;
                    }
                }
                t.iwram[w] = Some(b.func);
            } else {
                let e = b.key.wrapping_sub(EWRAM_BASE) as usize;
                if e < t.ewram.len() {
                    t.ewram[e] = Some(b.func);
                } else {
                    t.other.insert(b.key, b.func);
                }
            }
        }
        Ok(t)
    }

    #[inline(always)]
    fn get(&self, key: u32) -> Option<BlockFn> {
        let r = key.wrapping_sub(ROM_BASE) as usize;
        if r < self.rom.len() {
            return self.rom[r];
        }
        let w = key.wrapping_sub(IWRAM_BASE) as usize;
        if w < self.iwram.len() {
            return self.iwram[w];
        }
        let e = key.wrapping_sub(EWRAM_BASE) as usize;
        if e < self.ewram.len() {
            return self.ewram[e];
        }
        if (key as usize) < self.bios.len() {
            return self.bios[key as usize];
        }
        if self.other.is_empty() {
            None
        } else {
            self.other.get(&key).copied()
        }
    }
}

/// Drive the machine to the next completed frame using translated blocks
/// where available and the interpreter elsewhere, with interrupt
/// machinery serviced at block boundaries — the same execution sequence
/// as the original whole-run dispatch loop, bounded per frame. Returns
/// (native blocks run, fallback steps); the frame is incomplete only if
/// `max_steps` was exhausted (the caller's stall guard).
fn run_frame_native(
    m: &mut Machine,
    table: &BlockTable,
    mptr: *mut std::ffi::c_void,
    max_steps: u64,
) -> (u64, u64) {
    use gba_core::capi::RT_API;
    m.bus.frame_ready = false;
    let mut native = 0u64;
    let mut fallback = 0u64;
    let mut steps = 0u64;
    // Fallback-entry detection state (RECOMP_TRACE_FALLBACK): end address
    // of the previous fallback instruction; MAX = last step wasn't a
    // straight-line fallback continuation.
    let mut fb_prev_end = u32::MAX;
    while !m.bus.frame_ready && steps < max_steps {
        steps += 1;
        // Interrupt machinery and sleep states go through Machine::step.
        // (The IRQ-return-stub check is HLE-only: under a real BIOS that
        // address is ordinary BIOS code.)
        if m.bus.halted
            || (!m.bus.real_bios
                && m.cpu.regs[15] == gba_core::machine::IRQ_RETURN_ADDR
                && m.cpu.mode() == gba_core::Mode::Irq)
            || (m.bus.irq_pending() && !m.cpu.flag(gba_core::cpu::FLAG_I))
        {
            m.step();
            continue;
        }
        let key = m.cpu.regs[15] | m.cpu.thumb() as u32;
        // Real-BIOS mode: native blocks bypass the interpreter's fetch
        // hook, so maintain the executing-in-BIOS flag at dispatch
        // (region-0 reads return real bytes only while it is set).
        if m.bus.real_bios {
            m.bus.pc_in_bios = key < 0x4000;
        }
        // MP2K HLE hook at block granularity (SoundMainRAM is a call
        // target, so its entry is always a dispatch point). The
        // interpreter fallback re-checks inside step(); the hook is
        // idempotent within a tick.
        if let Some(h) = m.bus.mp2k.as_deref() {
            if h.active && h.hook_match(key) {
                m.bus.mp2k_frame_hook(key);
            }
        }
        if let Some(g) = m.bus.gax.as_deref() {
            if g.hook_match(key) {
                let r0 = m.cpu.regs[0];
                m.bus.gax_frame_hook(key, r0);
            }
        }
        if let Some(rr) = m.bus.rdrv.as_deref() {
            if rr.hook_match(key) {
                m.bus.rdrv_frame_hook(key);
            }
        }
        match table.get(key) {
            Some(f) => {
                // Diagnostic: census of executed IWRAM natives.
                if std::env::var_os("RECOMP_TRACE_IWRAM").is_some() && key >> 24 == 3 {
                    IWRAM_HITS.with(|h| *h.borrow_mut().entry(key).or_insert(0u64) += 1);
                }
                f(&RT_API, mptr);
                native += 1;
                fb_prev_end = u32::MAX;
            }
            None => {
                if NO_INTERP.load(std::sync::atomic::Ordering::Relaxed) {
                    eprintln!(
                        "DEFECT: full recomp missing native code at {:08x} ({}) — this \
code path is not covered by the package's translation. Report this address to the \
packager (it becomes a label; the rebuilt package covers it).",
                        key & !1,
                        if key & 1 != 0 { "thumb" } else { "arm" },
                    );
                    std::process::exit(2);
                }
                // Diagnostic (RECOMP_TRACE_FALLBACK): census of fallback
                // *entries* — the dispatch keys that would be labels.
                // Straight-line continuation inside a fallback run is not
                // an entry; the same discontinuity filter as the build
                // profiler applies.
                if trace_fallback() {
                    if key & !1 != fb_prev_end {
                        let new = FALLBACK_ENTRIES.with(|h| {
                            let mut h = h.borrow_mut();
                            let len0 = h.len();
                            *h.entry(key).or_insert(0u64) += 1;
                            h.len() != len0
                        });
                        if new
                            && key >> 24 == 3
                            && FALLBACK_COLLECT.load(std::sync::atomic::Ordering::Relaxed)
                        {
                            capture_iwram(&m.bus.iwram, key);
                        }
                    }
                    fb_prev_end = match m.step() {
                        StepEvent::Instr(i) => i.addr.wrapping_add(i.size()),
                        StepEvent::Idle => u32::MAX,
                    };
                } else {
                    m.step();
                }
                fallback += 1;
            }
        }
    }
    (native, fallback)
}

/// Per-frame stall guard for the native loop: generous beyond any real
/// frame (a frame is ~280K cycles) while still bounding a wedged run.
const FRAME_STEP_GUARD: u64 = 200_000_000;

thread_local! {
    /// Diagnostic (RECOMP_TRACE_IWRAM): executed-IWRAM-native census.
    static IWRAM_HITS: std::cell::RefCell<std::collections::HashMap<u32, u64>> =
        std::cell::RefCell::new(std::collections::HashMap::new());

    /// Diagnostic (RECOMP_TRACE_FALLBACK): fallback-entry census —
    /// dispatch keys (addr | thumb) that missed the block table, with
    /// hit counts. Entries only, not straight-line continuations.
    static FALLBACK_ENTRIES: std::cell::RefCell<std::collections::HashMap<u32, u64>> =
        std::cell::RefCell::new(std::collections::HashMap::new());

    /// --record-labels: IWRAM content accumulated at the moment each
    /// new IWRAM entry point is discovered (the code is certainly live
    /// right then; an end-of-session snapshot could miss swapped
    /// overlays). Saved as the image's local snapshot blob.
    static IWRAM_CAP: std::cell::RefCell<Option<Box<labels::Blob>>> =
        std::cell::RefCell::new(None);
}

/// Fold the live IWRAM into the capture accumulator: first capture
/// fills everything; later new-entry events force-refresh a window
/// around the new entry (its code is live NOW; elsewhere, first-seen
/// content stands and the runtime guards reject anything stale).
fn capture_iwram(iwram: &[u8], entry: u32) {
    IWRAM_CAP.with(|c| {
        let mut c = c.borrow_mut();
        let blob = c.get_or_insert_with(|| Box::new(labels::Blob::new()));
        let n = iwram.len().min(labels::IWRAM_LEN);
        for i in 0..n {
            if blob.mask[i] == 0 {
                blob.img[i] = iwram[i];
                blob.mask[i] = 1;
            }
        }
        let at = (entry & !1) as usize & (labels::IWRAM_LEN - 1);
        let (lo, hi) = (at.saturating_sub(64), (at + 2048).min(n));
        blob.img[lo..hi].copy_from_slice(&iwram[lo..hi]);
        blob.mask[lo..hi].fill(1);
    });
}

/// Census collection runs under RECOMP_TRACE_FALLBACK (prints at exit)
/// or --record-labels (persists entries as a label file).
static FALLBACK_COLLECT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Full-recomp mode (packaged with `interpreter = false`): a dispatch
/// miss halts loudly instead of interpreting. The package was gated on
/// a zero-fallback soak at build time, so reaching this is a coverage
/// defect the packager needs to hear about, never something to play
/// through silently at degraded fidelity.
static NO_INTERP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn trace_fallback() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("RECOMP_TRACE_FALLBACK").is_some())
        || FALLBACK_COLLECT.load(std::sync::atomic::Ordering::Relaxed)
}

/// Persist this run's fallback census as labels: ROM entries become
/// analyzer seeds on the next build; IWRAM/EWRAM entries are recorded
/// as reserved presence records (not yet translatable). Union-merges
/// into the per-image accumulator under the config dir.
fn record_labels(rom_len: usize, sha: &str) -> Result<(), String> {
    let mut new = labels::Labels::default();
    FALLBACK_ENTRIES.with(|h| {
        for (&key, _) in h.borrow().iter() {
            match key >> 24 {
                0x08..=0x0D if ((key & !1 & 0x01FF_FFFF) as usize) < rom_len => {
                    new.rom.insert(key);
                }
                0x03 => {
                    new.iwram.insert(key);
                }
                0x02 => new.reserved.push(format!(
                    "ewram {:08x} {}",
                    key & !1,
                    if key & 1 != 0 { "t" } else { "a" }
                )),
                _ => {}
            }
        }
    });
    let path = labels::config_path(sha);
    let mut all = match path.is_file() {
        true => labels::Labels::load(&path, sha, rom_len)?,
        false => labels::Labels::default(),
    };
    let (rom0, iw0) = (all.rom.len(), all.iwram.len());
    all.merge(new);
    all.save(&path, sha)?;
    // Persist the IWRAM content captured at entry discovery: this
    // session's bytes override prior sessions where captured (they
    // reflect the overlays actually seen; guards reject the rest).
    IWRAM_CAP.with(|c| -> Result<(), String> {
        let Some(cap) = c.borrow_mut().take() else { return Ok(()) };
        let bp = labels::blob_path(sha);
        let mut blob = labels::Blob::load(&bp).unwrap_or_else(labels::Blob::new);
        for i in 0..labels::IWRAM_LEN {
            if cap.mask[i] != 0 {
                blob.img[i] = cap.img[i];
                blob.mask[i] = 1;
            }
        }
        blob.save(&bp)?;
        eprintln!("labels: iwram snapshot updated ({} bytes valid)", blob.valid_bytes());
        Ok(())
    })?;
    eprintln!(
        "labels: +{} rom, +{} iwram entries recorded ({} rom, {} iwram total) -> {}",
        all.rom.len() - rom0,
        all.iwram.len() - iw0,
        all.rom.len(),
        all.iwram.len(),
        path.display()
    );
    if all.rom.len() > rom0 || all.iwram.len() > iw0 {
        eprintln!("labels: the next translation rebuild covers the new entries");
    }
    Ok(())
}

/// Label-file tooling: inspect, import a shared file, export for
/// sharing. The local IWRAM snapshot is shown but never exported.
fn cmd_labels(args: &[String]) -> Result<(), String> {
    const USAGE: &str = "usage: recomp labels <show|import|export> <rom> [file]";
    let sub = args.first().ok_or(USAGE)?.as_str();
    let rom_path = args.get(1).ok_or(USAGE)?;
    let rom = std::fs::read(rom_path).map_err(|e| format!("{rom_path}: {e}"))?;
    let sha = rom_sha256(&rom);
    match sub {
        "show" => {
            let all = labels::load_all(rom_path, &sha, rom.len());
            println!("image {sha}");
            println!(
                "labels: {} rom, {} iwram, {} named, {} reserved (ewram)",
                all.rom.len(),
                all.iwram.len(),
                all.names.len(),
                all.reserved.len()
            );
            match labels::Blob::load(&labels::blob_path(&sha)) {
                Some(b) => println!(
                    "iwram snapshot: {} bytes valid ({})",
                    b.valid_bytes(),
                    labels::blob_path(&sha).display()
                ),
                None => println!("iwram snapshot: none (run play/runc --record-labels)"),
            }
            if !all.is_empty() {
                let blob = labels::Blob::load(&labels::blob_path(&sha));
                let d = all.digest(blob.as_ref());
                println!("cache key suffix: -l{:08x}", d as u32 ^ (d >> 32) as u32);
            }
            Ok(())
        }
        "import" => {
            let file = args.get(2).ok_or("import needs a label file")?;
            let incoming = labels::Labels::load(std::path::Path::new(file), &sha, rom.len())?;
            // Enriched (named) sets go to the TOML accumulator — the v1
            // file can't carry names; address-only sets keep using it.
            let enriched = !incoming.names.is_empty() || !incoming.ends.is_empty();
            let path = if enriched {
                labels::config_toml_path(&sha)
            } else {
                labels::config_path(&sha)
            };
            let mut all = match path.is_file() {
                true => labels::Labels::load(&path, &sha, rom.len())?,
                false => labels::Labels::default(),
            };
            let (r0, i0) = (all.rom.len(), all.iwram.len());
            all.merge(incoming);
            if enriched {
                all.save_toml(&path, &sha)?;
            } else {
                all.save(&path, &sha)?;
            }
            println!(
                "imported: +{} rom, +{} iwram ({} rom, {} iwram total) -> {}",
                all.rom.len() - r0,
                all.iwram.len() - i0,
                all.rom.len(),
                all.iwram.len(),
                path.display()
            );
            if all.iwram.len() > 0 && labels::Blob::load(&labels::blob_path(&sha)).is_none() {
                println!(
                    "note: iwram entries activate after a local --record-labels session \
captures their content"
                );
            }
            Ok(())
        }
        "export" => {
            let default = format!("{}.labels.toml", rom_path.trim_end_matches(".gba"));
            let out = args.get(2).cloned().unwrap_or(default);
            let all = labels::load_all(rom_path, &sha, rom.len());
            if all.is_empty() && all.reserved.is_empty() {
                return Err("nothing to export — record some labels first".into());
            }
            // The extension picks the format: .toml = v2 interchange
            // (names/ends preserved), anything else = v1 lines.
            if out.ends_with(".toml") {
                all.save_toml(std::path::Path::new(&out), &sha)?;
            } else {
                all.save(std::path::Path::new(&out), &sha)?;
            }
            println!(
                "exported {} rom + {} iwram entries ({} named) -> {out} (addresses and \
names only; the local iwram snapshot is never exported)",
                all.rom.len(),
                all.iwram.len(),
                all.names.len()
            );
            Ok(())
        }
        other => Err(format!("unknown labels subcommand {other:?}; {USAGE}")),
    }
}

/// Single-line terminal build progress: bar, percent, phase label.
fn term_progress(pct: u8, msg: &str) {
    use std::io::Write;
    let filled = pct as usize * 28 / 100;
    eprint!(
        "\r\x1b[K  [{}{}] {pct:>3}%  {msg}",
        "\u{25a0}".repeat(filled),
        "\u{00b7}".repeat(28 - filled)
    );
    let _ = std::io::stderr().flush();
}

/// SHA-256 of the image, hex — the cross-tool image identity.
fn rom_sha256(rom: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(rom).iter().map(|b| format!("{b:02x}")).collect()
}

/// Dump the fallback-entry census: per-region totals plus the hottest
/// entries. Resident code shows as few entries with huge counts;
/// streamed overlays as many entries with small counts.
fn print_fallback_census() {
    if !trace_fallback() {
        return;
    }
    let region_name = |key: u32| match key >> 24 {
        0 => "bios",
        2 => "ewram",
        3 => "iwram",
        8..=0xD => "rom",
        _ => "other",
    };
    FALLBACK_ENTRIES.with(|h| {
        let h = h.borrow();
        let mut per: std::collections::BTreeMap<&str, (u64, u64)> = Default::default();
        for (&k, &n) in h.iter() {
            let e = per.entry(region_name(k)).or_insert((0, 0));
            e.0 += 1;
            e.1 += n;
        }
        eprintln!("FALLBACK census: {} distinct entries", h.len());
        for (r, (distinct, hits)) in &per {
            eprintln!("  {r:>5}: {distinct} entries, {hits} hits");
        }
        let mut v: Vec<(u32, u64)> = h.iter().map(|(&k, &n)| (k, n)).collect();
        v.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
        for (k, n) in v.iter().take(25) {
            eprintln!(
                "  {} {:08x} {} {n}",
                region_name(*k),
                k & !1,
                if k & 1 != 0 { "t" } else { "a" }
            );
        }
    });
}

/// Run a recompiled image: translated blocks where available, interpreter
/// fallback elsewhere, interrupt machinery at block boundaries.
fn cmd_runc(args: &[String]) -> Result<(), String> {

    let mut rom_path = None;
    let mut frames = 600u64;
    let mut out: Option<String> = None;
    let mut input: Option<InputScript> = None;
    let mut bios: Option<Vec<u8>> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--frames" => {
                frames = it.next().ok_or("--frames needs a value")?
                    .parse().map_err(|e| format!("bad frames: {e}"))?
            }
            "--out" => out = Some(it.next().ok_or("--out needs a value")?.to_string()),
            "--input" => {
                input = Some(InputScript::load(it.next().ok_or("--input needs a value")?)?)
            }
            "--record-labels" => {
                FALLBACK_COLLECT.store(true, std::sync::atomic::Ordering::Relaxed)
            }
            "--bios" => bios = Some(load_bios_file(it.next().ok_or("--bios needs a value")?)?),
            other if rom_path.is_none() => rom_path = Some(other.to_string()),
            other => return Err(format!("unexpected argument {other:?}")),
        }
    }
    let rom_path = rom_path.ok_or("missing ROM path")?;
    let rom = std::fs::read(&rom_path).map_err(|e| format!("{rom_path}: {e}"))?;
    let stem = Path::new(&rom_path)
        .file_stem().and_then(|s| s.to_str()).unwrap_or("game").to_string();
    let suffix = if bios.is_some() { "-bios" } else { "" };
    let lib_path = format!("out/{stem}{suffix}.dylib");

    let lib = unsafe { libloading::Library::new(&lib_path) }
        .map_err(|e| format!("{lib_path}: {e}"))?;
    let table = BlockTable::load(&lib)?;
    println!("loaded {} translated blocks", table.len);

    let mut m = make_machine(rom, bios.as_deref());
    // RECOMP_MP2K=1: arm the HLE shadow under native dispatch — the
    // hook-at-block-boundary path play uses, validated headless here.
    if std::env::var_os("RECOMP_MP2K").is_some() {
        m.bus.tap_channels = true;
        eprintln!("hle: {}", arm_audio_hle(&mut m));
    }
    let mptr = &mut m as *mut Machine as *mut std::ffi::c_void;

    let mut native_blocks = 0u64;
    let mut fallback_steps = 0u64;
    // Diagnostic (RECOMP_COST_FROM=N): from frame N on, attribute charged
    // cycles to the dispatch key (block entry or interp PC) that incurred
    // them; dump the histogram at exit.
    let cost_from: Option<u64> = std::env::var("RECOMP_COST_FROM").ok().and_then(|v| v.parse().ok());
    let mut cost: std::collections::HashMap<u32, u64> = std::collections::HashMap::new();
    while m.bus.frames < frames {
        let before = m.bus.frames;
        if let Some(s) = &input {
            m.bus.keys = s.keys_at(before);
        }
        if cost_from.is_some_and(|n| m.bus.frames >= n) {
            use gba_core::capi::RT_API;
            m.bus.frame_ready = false;
            let mut steps = 0u64;
            while !m.bus.frame_ready && steps < FRAME_STEP_GUARD {
                steps += 1;
                let pc = m.cpu.regs[15];
                let c0 = m.bus.clock;
                if m.bus.halted
                    || (!m.bus.real_bios
                        && m.cpu.regs[15] == gba_core::machine::IRQ_RETURN_ADDR
                        && m.cpu.mode() == gba_core::Mode::Irq)
                    || (m.bus.irq_pending() && !m.cpu.flag(gba_core::cpu::FLAG_I))
                {
                    m.step();
                    *cost.entry(pc).or_insert(0) += m.bus.clock - c0;
                    continue;
                }
                let key = m.cpu.regs[15] | m.cpu.thumb() as u32;
                if m.bus.real_bios {
                    m.bus.pc_in_bios = key < 0x4000;
                }
                match table.get(key) {
                    Some(f) => {
                        f(&RT_API, mptr);
                        native_blocks += 1;
                    }
                    None => {
                        m.step();
                        fallback_steps += 1;
                    }
                }
                *cost.entry(pc).or_insert(0) += m.bus.clock - c0;
            }
        } else {
            let (n, f) = run_frame_native(&mut m, &table, mptr, FRAME_STEP_GUARD);
            native_blocks += n;
            fallback_steps += f;
        }
        if m.bus.frames == before {
            return Err("step guard exceeded".into());
        }
    }
    if !cost.is_empty() {
        let mut v: Vec<(u32, u64)> = cost.into_iter().collect();
        v.sort_by_key(|&(_, c)| std::cmp::Reverse(c));
        let total: u64 = v.iter().map(|&(_, c)| c).sum();
        eprintln!("COST total={total} cycles over traced frames; top PCs:");
        for (pc, c) in v.iter().take(40) {
            eprintln!("  COST {pc:08x} {c}");
        }
    }

    // FNV-1a framebuffer hash — must match the interpreter's `frames` run.
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &px in &m.bus.framebuffer {
        for b in px.to_le_bytes() {
            hash ^= b as u64;
            hash = hash.wrapping_mul(0x1_0000_01b3);
        }
    }
    println!(
        "frames: {} hash: {hash:016x} native_blocks: {native_blocks} fallback_steps: {fallback_steps}",
        m.bus.frames
    );
    if let Some(h) = m.bus.mp2k.as_deref() {
        let (corr, ratio) = h.last_correlation();
        eprintln!(
            "mp2k: hooks={} stale={} bad_waves={} corr={corr:.3} ratio={ratio:.2} gain={:.2} mode={} pauses={} proven={} engaged={} active={}{}",
            h.hooks,
            h.stale_ticks,
            h.bad_waves,
            h.gain(),
            h.count_mode(),
            h.vf.pauses,
            h.vf.proven,
            h.engaged,
            h.active,
            h.vf.reverted
                .as_deref()
                .map(|m| format!(" PAUSED: {m}"))
                .unwrap_or_default()
        );
    }
    if let Some(g) = m.bus.gax.as_deref() {
        let (corr, ratio) = g.last_correlation();
        eprintln!(
            "gax: hooks={} stale={} bad_waves={} corr={corr:.3} ratio={ratio:.2} gain={:.2} pauses={} proven={} engaged={} active={}{}",
            g.hooks,
            g.stale_ticks,
            g.bad_waves,
            g.gain(),
            g.vf.pauses,
            g.vf.proven,
            g.engaged,
            g.active,
            g.vf.reverted
                .as_deref()
                .map(|m| format!(" PAUSED: {m}"))
                .unwrap_or_default()
        );
    }
    if let Some(r) = m.bus.rdrv.as_deref() {
        let (corr, ratio) = r.last_correlation();
        eprintln!(
            "rdrv: hooks={} stale={} bad_waves={} corr={corr:.3} ratio={ratio:.2} gain={:.2} pauses={} proven={} engaged={} active={}{}",
            r.hooks,
            r.stale_ticks,
            r.bad_waves,
            r.gain(),
            r.vf.pauses,
            r.vf.proven,
            r.engaged,
            r.active,
            r.vf.reverted
                .as_deref()
                .map(|m| format!(" PAUSED: {m}"))
                .unwrap_or_default()
        );
    }
    if std::env::var_os("RECOMP_TRACE_IWRAM").is_some() {
        IWRAM_HITS.with(|h| {
            let mut v: Vec<_> = h.borrow().iter().map(|(&k, &n)| (k, n)).collect();
            v.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
            for (k, n) in v.iter().take(30) {
                println!("iwram native {k:08x}: {n}");
            }
            println!("iwram natives executed: {}", v.len());
        });
    }
    print_fallback_census();
    if FALLBACK_COLLECT.load(std::sync::atomic::Ordering::Relaxed) {
        record_labels(m.bus.rom.len(), &rom_sha256(&m.bus.rom))?;
    }
    dump_frame(&m, out)?;
    Ok(())
}

/// Differential verification: run N frames interpreted and N frames
/// recompiled (building if needed) and compare framebuffer hashes.
fn cmd_verify(args: &[String]) -> Result<(), String> {
    let mut rom_path = None;
    let mut frames = 600u64;
    let mut reuse = false;
    let mut dump: Option<String> = None;
    let mut input: Option<String> = None;
    let mut bios_path: Option<String> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--frames" => {
                frames = it.next().ok_or("--frames needs a value")?
                    .parse().map_err(|e| format!("bad frames: {e}"))?
            }
            // Triage helpers: --reuse skips the rebuild when out/<stem>.dylib
            // already exists; --dump <prefix> writes both final frames as
            // <prefix>.interp.ppm / <prefix>.recomp.ppm.
            "--reuse" => reuse = true,
            "--dump" => dump = Some(it.next().ok_or("--dump needs a value")?.to_string()),
            "--input" => {
                input = Some(it.next().ok_or("--input needs a value")?.to_string())
            }
            "--bios" => {
                bios_path = Some(it.next().ok_or("--bios needs a value")?.to_string())
            }
            other if rom_path.is_none() => rom_path = Some(other.to_string()),
            other => return Err(format!("unexpected argument {other:?}")),
        }
    }
    let rom_path = rom_path.ok_or("missing ROM path")?;
    let bios = bios_path.as_deref().map(load_bios_file).transpose()?;

    let stem = Path::new(&rom_path)
        .file_stem().and_then(|s| s.to_str()).unwrap_or("game").to_string();
    let suffix = if bios.is_some() { "-bios" } else { "" };
    if !(reuse && Path::new(&format!("out/{stem}{suffix}.dylib")).is_file()) {
        let mut build_args = vec![rom_path.clone()];
        if let Some(p) = &bios_path {
            build_args.extend(["--bios".to_string(), p.clone()]);
        }
        cmd_build(&build_args)?;
    }
    let script = input.as_deref().map(InputScript::load).transpose()?;
    let interp = run_hash(&rom_path, frames, false, script.as_ref(), bios.as_deref(),
        dump.as_ref().map(|p| format!("{p}.interp.ppm")))?;
    let recomp = run_hash(&rom_path, frames, true, script.as_ref(), bios.as_deref(),
        dump.as_ref().map(|p| format!("{p}.recomp.ppm")))?;
    let verdict = if interp == recomp { "MATCH" } else { "MISMATCH" };
    println!("verify {verdict} interp={interp:016x} recomp={recomp:016x}");
    if interp == recomp { Ok(()) } else { Err("differential mismatch".into()) }
}


/// Deterministic demo input for differential runs: taps Start, then A,
/// periodically — enough to leave title screens and menus in most games.
fn demo_keys(frame: u64) -> u16 {
    let mut keys = 0x3FFu16;
    let phase = frame % 180;
    if frame >= 240 {
        if phase < 10 {
            keys &= !(1 << 3); // Start
        } else if (60..70).contains(&phase) {
            keys &= !(1 << 0); // A
        }
    }
    keys
}

/// FNV-1a, used for cheap change detection on backup media.
fn fnv64(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x1_0000_01b3);
    }
    h
}

fn fb_hash(m: &Machine) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &px in &m.bus.framebuffer {
        for b in px.to_le_bytes() {
            hash ^= b as u64;
            hash = hash.wrapping_mul(0x1_0000_01b3);
        }
    }
    hash
}

fn run_hash(
    rom_path: &str,
    frames: u64,
    recompiled: bool,
    input: Option<&InputScript>,
    bios: Option<&[u8]>,
    dump: Option<String>,
) -> Result<u64, String> {
    let rom = std::fs::read(rom_path).map_err(|e| format!("{rom_path}: {e}"))?;
    let mut m = make_machine(rom, bios);
    let keys_at = |frame: u64| match input {
        Some(s) => s.keys_at(frame),
        None => demo_keys(frame),
    };
    if !recompiled {
        for _ in 0..frames {
            m.bus.keys = keys_at(m.bus.frames);
            m.run_frame(5_000_000);
        }
        dump_frame(&m, dump)?;
        return Ok(fb_hash(&m));
    }
    let stem = Path::new(rom_path)
        .file_stem().and_then(|s| s.to_str()).unwrap_or("game").to_string();
    let suffix = if bios.is_some() { "-bios" } else { "" };
    let lib_path = format!("out/{stem}{suffix}.dylib");
    let lib = unsafe { libloading::Library::new(&lib_path) }
        .map_err(|e| format!("{lib_path}: {e}"))?;
    let table = BlockTable::load(&lib)?;
    let mptr = &mut m as *mut Machine as *mut std::ffi::c_void;
    while m.bus.frames < frames {
        let before = m.bus.frames;
        m.bus.keys = keys_at(before);
        run_frame_native(&mut m, &table, mptr, FRAME_STEP_GUARD);
        if m.bus.frames == before {
            return Err("step guard exceeded".into());
        }
    }
    dump_frame(&m, dump)?;
    Ok(fb_hash(&m))
}

/// Minimal PCM16LE WAV writer for audio triage dumps.
fn write_wav(path: &str, samples: &[i16], rate: u32, channels: u16) -> Result<(), String> {
    let data_len = (samples.len() * 2) as u32;
    let block = channels as u32 * 2;
    let mut w = Vec::with_capacity(44 + samples.len() * 2);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data_len).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&channels.to_le_bytes());
    w.extend_from_slice(&rate.to_le_bytes());
    w.extend_from_slice(&(rate * block).to_le_bytes());
    w.extend_from_slice(&(block as u16).to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        w.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, w).map_err(|e| format!("{path}: {e}"))
}

fn dump_frame(m: &Machine, path: Option<String>) -> Result<(), String> {
    let Some(path) = path else { return Ok(()) };
    let mut ppm = b"P6\n240 160\n255\n".to_vec();
    for &px in &m.bus.framebuffer {
        let r = (px & 31) as u8;
        let g = ((px >> 5) & 31) as u8;
        let b = ((px >> 10) & 31) as u8;
        ppm.extend_from_slice(&[r << 3 | r >> 2, g << 3 | g >> 2, b << 3 | b >> 2]);
    }
    std::fs::write(&path, ppm).map_err(|e| format!("{path}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sinc_rows_have_unity_dc_gain() {
        let r = SincResampler::new(65536.0, 48000.0);
        for p in 0..=SINC_PHASES {
            let sum: f32 = r.table[p * SINC_TAPS..][..SINC_TAPS].iter().sum();
            assert!((sum - 1.0).abs() < 1e-4, "phase {p}: sum {sum}");
        }
    }

    #[test]
    fn sinc_preserves_audible_sine() {
        // 1 kHz at the 65536 Hz tap, resampled to 48 kHz: RMS must come
        // through within a few percent.
        let mut r = SincResampler::new(65536.0, 48000.0);
        let mut q = std::collections::VecDeque::new();
        for n in 0..65536 {
            let s = (2.0 * std::f64::consts::PI * 1000.0 * n as f64 / 65536.0).sin();
            q.push_back(s as f32 * 0.5);
            q.push_back(s as f32 * 0.5);
        }
        let out: Vec<f32> = (0..40000).map(|_| { let (l, r2) = r.next(&mut q); (l + r2) * 0.5 }).collect();
        let rms = (out[2000..38000].iter().map(|v| v * v).sum::<f32>() / 36000.0).sqrt();
        let expect = 0.5 / 2f32.sqrt(); // amplitude 0.5 sine
        assert!(
            (rms - expect).abs() / expect < 0.03,
            "rms {rms} vs {expect}"
        );
    }

    /// Fill a FIFO event queue with a sine at the given timer period.
    fn fifo_sine(period: u32, hz: f64, n: usize) -> std::collections::VecDeque<(i8, u32)> {
        let src_hz = (1u64 << 24) as f64 / period as f64;
        (0..n)
            .map(|i| {
                let s = (2.0 * std::f64::consts::PI * hz * i as f64 / src_hz).sin();
                ((s * 100.0) as i8, period)
            })
            .collect()
    }

    #[test]
    fn fifo_interp_preserves_sine_at_mp2k_rate() {
        // 1 kHz at 13379 Hz (period 1254) — the MP2K default mix rate —
        // interpolated to 48 kHz: RMS within a few percent of the 8-bit
        // source amplitude.
        let mut f = FifoInterp::new(48000.0);
        let mut q = fifo_sine(1254, 1000.0, 40000);
        let out: Vec<f32> = (0..96000).map(|_| f.next(&mut q)).collect();
        let rms = (out[2000..90000].iter().map(|v| v * v).sum::<f32>() / 88000.0).sqrt();
        let expect = (100.0 * 64.0 / 32768.0) / 2f32.sqrt();
        assert!((rms - expect).abs() / expect < 0.05, "rms {rms} vs {expect}");
    }

    #[test]
    fn fifo_interp_follows_rate_change() {
        // Retune mid-stream 13379 Hz -> 32768 Hz; output stays bounded
        // and alive on both sides of the change.
        let mut f = FifoInterp::new(48000.0);
        let mut q = fifo_sine(1254, 1000.0, 4000);
        q.extend(fifo_sine(512, 1000.0, 8000));
        let out: Vec<f32> = (0..40000).map(|_| f.next(&mut q)).collect();
        assert!(out.iter().all(|v| v.abs() < 0.5));
        let tail_rms = (out[30000..40000].iter().map(|v| v * v).sum::<f32>() / 10000.0).sqrt();
        assert!(tail_rms > 0.05, "went silent after rate change: {tail_rms}");
    }

    #[test]
    fn fifo_interp_holds_above_device_rate() {
        // 65536 Hz channel into a 48 kHz device: the structural fallback
        // is sample-and-hold; output must stay bounded.
        let mut f = FifoInterp::new(48000.0);
        let mut q = fifo_sine(256, 1000.0, 16384);
        let out: Vec<f32> = (0..8000).map(|_| f.next(&mut q)).collect();
        assert!(out.iter().all(|v| v.abs() < 0.5));
    }

    #[test]
    fn soft_clip_identity_below_rail_knee_above() {
        // Bit-exact identity through the rail: unclipped material must
        // be untouched (the Stage-2 fidelity guarantee).
        for i in -1000..=1000i32 {
            let x = i as f32 * (RAIL / 1000.0);
            assert_eq!(soft_clip(x), x);
        }
        // Above the rail: monotonic, odd-symmetric, and bounded so that
        // post-OUT_GAIN output never exceeds device full scale.
        let mut prev = soft_clip(RAIL);
        for i in 1..=300 {
            let x = RAIL + i as f32 * 0.005;
            let y = soft_clip(x);
            assert!(y >= prev, "not monotonic at {x}");
            assert!(y * OUT_GAIN <= 1.0 + 1e-4, "exceeds full scale at {x}: {y}");
            assert_eq!(soft_clip(-x), -y);
            prev = y;
        }
        // C1 at the knee: slope just above the rail stays ~1.
        let d = (soft_clip(RAIL + 1e-3) - soft_clip(RAIL - 1e-3)) / 2e-3;
        assert!((d - 1.0).abs() < 0.02, "knee slope {d}");
        // The hardware's worst case (3x rail) lands just under full scale.
        assert!(soft_clip(3.0 * RAIL) * OUT_GAIN > 0.99);
    }
}
