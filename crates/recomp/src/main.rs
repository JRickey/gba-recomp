//! Static recompiler CLI.
//!
//! Current commands exercise the decoder against real ROMs:
//!   dis <rom> [--addr HEX] [--count N] [--thumb]   disassemble from an address
//!   entry-scan <dir>                               decode every ROM's entry point

use std::path::Path;
use std::process::ExitCode;

mod analyze;
mod emit;

use armv4t::{decode_arm, decode_thumb, Op};
use gba_core::{is_self_loop, Machine, StepEvent};

const ROM_BASE: u32 = 0x0800_0000;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("dis") => cmd_dis(&args[1..]),
        Some("entry-scan") => cmd_entry_scan(&args[1..]),
        Some("run") => cmd_run(&args[1..]),
        Some("frames") => cmd_frames(&args[1..]),
        Some("play") => cmd_play(&args[1..]),
        Some("build") => cmd_build(&args[1..]),
        Some("runc") => cmd_runc(&args[1..]),
        Some("verify") => cmd_verify(&args[1..]),
        _ => {
            eprintln!("usage: recomp dis <rom> [--addr HEX] [--count N] [--thumb]");
            eprintln!("       recomp entry-scan <dir>");
            eprintln!("       recomp run <rom> [--max-steps N] [--trace]");
            eprintln!("       recomp frames <rom> [--frames N] [--out img.ppm] [--keys MASK]");
            eprintln!("       recomp play <rom>");
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

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--max-steps" => {
                max_steps = it.next().ok_or("--max-steps needs a value")?
                    .parse().map_err(|e| format!("bad max-steps: {e}"))?
            }
            "--trace" => trace = true,
            "--hist" => hist = true,
            other if rom_path.is_none() => rom_path = Some(other.to_string()),
            other => return Err(format!("unexpected argument {other:?}")),
        }
    }
    let rom_path = rom_path.ok_or("missing ROM path")?;
    let rom = std::fs::read(&rom_path).map_err(|e| format!("{rom_path}: {e}"))?;

    let mut m = Machine::new(rom);
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

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--frames" => {
                frames = it.next().ok_or("--frames needs a value")?
                    .parse().map_err(|e| format!("bad frames: {e}"))?
            }
            "--out" => out = Some(it.next().ok_or("--out needs a value")?.to_string()),
            "--keys" => keys = parse_hex(it.next().ok_or("--keys needs a value")?)? as u16,
            other if rom_path.is_none() => rom_path = Some(other.to_string()),
            other => return Err(format!("unexpected argument {other:?}")),
        }
    }
    let rom_path = rom_path.ok_or("missing ROM path")?;
    let rom = std::fs::read(&rom_path).map_err(|e| format!("{rom_path}: {e}"))?;

    let mut m = Machine::new(rom);
    m.bus.keys = keys;
    for _ in 0..frames {
        m.run_frame(5_000_000);
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

    let rom_path = args.first().ok_or("missing ROM path")?.clone();
    let rom = std::fs::read(&rom_path).map_err(|e| format!("{rom_path}: {e}"))?;
    let sav_path = format!("{}.sav", rom_path.trim_end_matches(".gba"));

    let mut m = Machine::new(rom);
    if let Ok(sav) = std::fs::read(&sav_path) {
        m.bus.load_save_data(&sav);
        eprintln!("loaded {sav_path}");
    }

    let mut window = Window::new(
        "recomp",
        240 * 3,
        160 * 3,
        WindowOptions { resize: true, ..WindowOptions::default() },
    )
    .map_err(|e| e.to_string())?;
    window.set_target_fps(60);


    // Audio: cpal output stream fed from a shared ring of 32768 Hz mono
    // samples, nearest-neighbor resampled to the device rate.
    let ring = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::<i16>::new()));
    let _stream = {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
        let host = cpal::default_host();
        match host.default_output_device() {
            Some(device) => match device.default_output_config() {
                Ok(cfg) => {
                    let rate = cfg.sample_rate().0 as f64;
                    let channels = cfg.channels() as usize;
                    let step = 32768.0 / rate;
                    let ring2 = ring.clone();
                    let mut frac = 0.0f64;
                    let mut last = 0i16;
                    let stream = device
                        .build_output_stream(
                            &cfg.into(),
                            move |out: &mut [f32], _| {
                                let mut q = ring2.lock().unwrap();
                                for frame in out.chunks_mut(channels) {
                                    frac += step;
                                    while frac >= 1.0 {
                                        frac -= 1.0;
                                        if let Some(s) = q.pop_front() {
                                            last = s;
                                        }
                                    }
                                    let v = last as f32 / 32768.0;
                                    for ch in frame.iter_mut() {
                                        *ch = v;
                                    }
                                }
                            },
                            |e| eprintln!("audio error: {e}"),
                            None,
                        )
                        .ok();
                    if let Some(s) = &stream {
                        let _ = s.play();
                    }
                    stream
                }
                Err(_) => None,
            },
            None => None,
        }
    };

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

    let mut buf = vec![0u32; 240 * 160];
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

        m.run_frame(5_000_000);

        // Hand this frame's audio to the output ring (cap ~250 ms).
        {
            let mut q = ring.lock().unwrap();
            for s in m.bus.audio_buf.drain(..) {
                if q.len() < 8192 {
                    q.push_back(s);
                }
            }
        }

        for (dst, &px) in buf.iter_mut().zip(m.bus.framebuffer.iter()) {
            let r = (px & 31) as u32;
            let g = ((px >> 5) & 31) as u32;
            let b = ((px >> 10) & 31) as u32;
            *dst = ((r << 3 | r >> 2) << 16) | ((g << 3 | g >> 2) << 8) | (b << 3 | b >> 2);
        }
        window.update_with_buffer(&buf, 240, 160).map_err(|e| e.to_string())?;
    }

    if let Some(data) = m.bus.save_data() {
        std::fs::write(&sav_path, data).map_err(|e| format!("{sav_path}: {e}"))?;
        eprintln!("saved {sav_path}");
    }
    Ok(())
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
    for arg in args {
        match arg.as_str() {
            "--ram" => ram = true,
            other if rom_path.is_none() => rom_path = Some(other.to_string()),
            other => return Err(format!("unexpected argument {other:?}")),
        }
    }
    let rom_path = &rom_path.ok_or("missing ROM path")?;
    let rom = std::fs::read(rom_path).map_err(|e| format!("{rom_path}: {e}"))?;

    // Profile-guided RAM discovery: run the interpreter briefly, recording
    // control-transfer targets in EWRAM/IWRAM, then translate the observed
    // RAM-resident code from the end-of-run snapshot (content-guarded at
    // execution time).
    let (seeds, ewram, iwram) = if !ram {
        (Vec::new(), Vec::new(), Vec::new())
    } else {
        let mut m = Machine::new(rom.clone());
        let mut seeds = std::collections::BTreeSet::new();
        let mut prev_end = 0u32;
        let mut steps = 0u64;
        while m.bus.frames < 240 && steps < 60_000_000 {
            steps += 1;
            match m.step() {
                StepEvent::Instr(instr) => {
                    let pc = m.cpu.regs[15];
                    // IWRAM only: EWRAM commonly holds streamed overlays,
                    // which defeat entry guards; that needs write-watch
                    // invalidation before it can be translated safely.
                    if pc != prev_end && pc >> 24 == 3 {
                        seeds.insert(pc | m.cpu.thumb() as u32);
                    }
                    prev_end = instr.addr.wrapping_add(instr.size());
                }
                StepEvent::Idle => {
                    let pc = m.cpu.regs[15];
                    if pc >> 24 == 3 && pc != prev_end {
                        seeds.insert(pc | m.cpu.thumb() as u32);
                    }
                }
            }
        }
        println!("profiled {} RAM entry points over {} frames", seeds.len(), m.bus.frames);
        (seeds.into_iter().collect::<Vec<u32>>(), m.bus.ewram.clone(), m.bus.iwram.clone())
    };

    let _ = &ewram;
    let view = analyze::View {
        rom: &rom,
        ewram: None,
        iwram: if ram { Some(&iwram) } else { None },
    };
    let analysis = analyze::analyze(&view, &seeds);
    let n_instrs: usize = analysis.blocks.iter().map(|b| b.instrs.len()).sum();
    println!("blocks: {} instructions: {n_instrs}", analysis.blocks.len());

    std::fs::create_dir_all("out").map_err(|e| e.to_string())?;
    let stem = Path::new(rom_path)
        .file_stem().and_then(|s| s.to_str()).unwrap_or("game").to_string();
    let lib_path = format!("out/{stem}.dylib");

    // Bounded translation units, compiled one at a time: full-image
    // translations can exceed the source image a hundredfold, and a single
    // huge unit makes cc balloon to many GB (parallel sweeps then exhaust
    // the machine). 16 MB of C keeps each cc invocation modest.
    const MAX_UNIT: usize = 16 << 20;
    let mut objs: Vec<String> = Vec::new();
    let chunks = emit::emit_c_chunked(&analysis, &view, MAX_UNIT, |c| {
        let i = objs.len();
        let c_path = format!("out/{stem}.{i}.c");
        let o_path = format!("out/{stem}.{i}.o");
        std::fs::write(&c_path, c).map_err(|e| e.to_string())?;
        let status = std::process::Command::new("cc")
            .args(["-O1", "-c", "-o", &o_path, &c_path])
            .status()
            .map_err(|e| format!("cc: {e}"))?;
        let _ = std::fs::remove_file(&c_path);
        if !status.success() {
            return Err(format!("cc failed on {c_path}"));
        }
        objs.push(o_path);
        Ok(())
    })?;

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
    println!("wrote {lib_path} ({chunks} units)");
    Ok(())
}

/// Run a recompiled image: translated blocks where available, interpreter
/// fallback elsewhere, interrupt machinery at block boundaries.
fn cmd_runc(args: &[String]) -> Result<(), String> {
    use gba_core::capi::{RcgBlock, RtApi, RT_API};

    let mut rom_path = None;
    let mut frames = 600u64;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--frames" => {
                frames = it.next().ok_or("--frames needs a value")?
                    .parse().map_err(|e| format!("bad frames: {e}"))?
            }
            other if rom_path.is_none() => rom_path = Some(other.to_string()),
            other => return Err(format!("unexpected argument {other:?}")),
        }
    }
    let rom_path = rom_path.ok_or("missing ROM path")?;
    let rom = std::fs::read(&rom_path).map_err(|e| format!("{rom_path}: {e}"))?;
    let stem = Path::new(&rom_path)
        .file_stem().and_then(|s| s.to_str()).unwrap_or("game").to_string();
    let lib_path = format!("out/{stem}.dylib");

    let lib = unsafe { libloading::Library::new(&lib_path) }
        .map_err(|e| format!("{lib_path}: {e}"))?;
    let table: std::collections::HashMap<u32, extern "C" fn(*const RtApi, *mut std::ffi::c_void) -> u32> = unsafe {
        let blocks: libloading::Symbol<*const RcgBlock> =
            lib.get(b"rcg_blocks").map_err(|e| e.to_string())?;
        let count: libloading::Symbol<*const u64> =
            lib.get(b"rcg_block_count").map_err(|e| e.to_string())?;
        let n = **count as usize;
        std::slice::from_raw_parts(*blocks, n)
            .iter()
            .map(|b| (b.key, b.func))
            .collect()
    };
    println!("loaded {} translated blocks", table.len());

    let mut m = Machine::new(rom);
    let mptr = &mut m as *mut Machine as *mut std::ffi::c_void;

    let mut native_blocks = 0u64;
    let mut fallback_steps = 0u64;
    let mut guard = 0u64;
    while m.bus.frames < frames {
        guard += 1;
        if guard > 4_000_000_000 {
            return Err("step guard exceeded".into());
        }
        // Interrupt machinery and sleep states go through Machine::step.
        if m.bus.halted
            || (m.cpu.regs[15] == gba_core::machine::IRQ_RETURN_ADDR
                && m.cpu.mode() == gba_core::Mode::Irq)
            || (m.bus.irq_pending() && !m.cpu.flag(gba_core::cpu::FLAG_I))
        {
            m.step();
            continue;
        }
        let key = m.cpu.regs[15] | m.cpu.thumb() as u32;
        match table.get(&key) {
            Some(f) => {
                f(&RT_API, mptr);
                native_blocks += 1;
            }
            None => {
                m.step();
                fallback_steps += 1;
            }
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
    Ok(())
}

/// Differential verification: run N frames interpreted and N frames
/// recompiled (building if needed) and compare framebuffer hashes.
fn cmd_verify(args: &[String]) -> Result<(), String> {
    let mut rom_path = None;
    let mut frames = 600u64;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--frames" => {
                frames = it.next().ok_or("--frames needs a value")?
                    .parse().map_err(|e| format!("bad frames: {e}"))?
            }
            other if rom_path.is_none() => rom_path = Some(other.to_string()),
            other => return Err(format!("unexpected argument {other:?}")),
        }
    }
    let rom_path = rom_path.ok_or("missing ROM path")?;

    let interp = run_hash(&rom_path, frames, false)?;
    cmd_build(&[rom_path.clone()])?;
    let recomp = run_hash(&rom_path, frames, true)?;
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

fn run_hash(rom_path: &str, frames: u64, recompiled: bool) -> Result<u64, String> {
    use gba_core::capi::{RcgBlock, RtApi, RT_API};
    let rom = std::fs::read(rom_path).map_err(|e| format!("{rom_path}: {e}"))?;
    let mut m = Machine::new(rom);
    if !recompiled {
        for _ in 0..frames {
            m.bus.keys = demo_keys(m.bus.frames);
            m.run_frame(5_000_000);
        }
        return Ok(fb_hash(&m));
    }
    let stem = Path::new(rom_path)
        .file_stem().and_then(|s| s.to_str()).unwrap_or("game").to_string();
    let lib_path = format!("out/{stem}.dylib");
    let lib = unsafe { libloading::Library::new(&lib_path) }
        .map_err(|e| format!("{lib_path}: {e}"))?;
    let table: std::collections::HashMap<u32, extern "C" fn(*const RtApi, *mut std::ffi::c_void) -> u32> = unsafe {
        let blocks: libloading::Symbol<*const RcgBlock> =
            lib.get(b"rcg_blocks").map_err(|e| e.to_string())?;
        let count: libloading::Symbol<*const u64> =
            lib.get(b"rcg_block_count").map_err(|e| e.to_string())?;
        std::slice::from_raw_parts(*blocks, **count as usize)
            .iter().map(|b| (b.key, b.func)).collect()
    };
    let mptr = &mut m as *mut Machine as *mut std::ffi::c_void;
    let mut guard = 0u64;
    let mut keyed_frame = u64::MAX;
    while m.bus.frames < frames {
        guard += 1;
        if guard > 4_000_000_000 {
            return Err("step guard exceeded".into());
        }
        if keyed_frame != m.bus.frames {
            keyed_frame = m.bus.frames;
            m.bus.keys = demo_keys(keyed_frame);
        }
        if m.bus.halted
            || (m.cpu.regs[15] == gba_core::machine::IRQ_RETURN_ADDR
                && m.cpu.mode() == gba_core::Mode::Irq)
            || (m.bus.irq_pending() && !m.cpu.flag(gba_core::cpu::FLAG_I))
        {
            m.step();
            continue;
        }
        let key = m.cpu.regs[15] | m.cpu.thumb() as u32;
        match table.get(&key) {
            Some(f) => {
                f(&RT_API, mptr);
            }
            None => {
                m.step();
            }
        }
    }
    Ok(fb_hash(&m))
}
