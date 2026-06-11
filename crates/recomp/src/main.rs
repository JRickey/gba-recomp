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
            eprintln!("       recomp play <rom> [--interp] [--stats] [--status]");
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
    for arg in args {
        match arg.as_str() {
            "--interp" => interp_only = true,
            "--status" => status = true,
            "--stats" => show_stats = true,
            other if rom_path.is_none() => rom_path = Some(other.to_string()),
            other => return Err(format!("unexpected argument {other:?}")),
        }
    }
    let rom_path = rom_path.ok_or("missing ROM path")?;
    let rom = std::fs::read(&rom_path).map_err(|e| format!("{rom_path}: {e}"))?;
    let sav_path = format!("{}.sav", rom_path.trim_end_matches(".gba"));

    // Native translation: load from the per-user cache, building it on
    // first launch. The product bar is full speed at full accuracy — the
    // interpreter fallback below is a defect surface, not a graceful
    // mode: it exists so a translation failure stays loudly diagnosable
    // instead of crashing, and the loop alarms if speed drops below
    // realtime either way.
    let native = if interp_only {
        None
    } else {
        match ensure_native(&rom_path, &rom, status) {
            Ok(v) => Some(v),
            Err(e) => {
                eprintln!("DEGRADED: native translation unavailable ({e}); interpreter only");
                None
            }
        }
    };

    let mut m = Machine::new(rom);
    if let Ok(sav) = std::fs::read(&sav_path) {
        m.bus.load_save_data(&sav);
        eprintln!("loaded {sav_path}");
    }

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
    window.set_target_fps(60);
    if status {
        status_line("playing");
    }


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
    let mut emu_ema_ms = 0.0f64;
    let mut frames_run = 0u64;
    let mut slow_warned = false;
    let mut native_run = 0u64;
    let mut fallback_run = 0u64;
    let mut sav_seen = m.bus.save_data().map(fnv64);
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

/// One machine-readable lifecycle line on stdout, for the launcher
/// supervising a `play --status` child. Flushed immediately: the reader
/// is a pipe, and a buffered line is a frozen progress bar.
fn status_line(s: &str) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "STATUS {s}");
    let _ = out.flush();
}

/// Locate (or build) the cached native translation for this image.
/// Cache key is the ROM's SHA-256 under a translation-format revision
/// directory, so a future emitter change can't load stale natives.
/// With `status`, build progress is reported as `STATUS building <pct>`.
fn ensure_native(
    rom_path: &str,
    rom: &[u8],
    status: bool,
) -> Result<(libloading::Library, BlockTable), String> {
    use sha2::{Digest, Sha256};
    let sha = Sha256::digest(rom)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let dir = dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("gba-recomp")
        .join("t1");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let lib_path = dir.join(format!("{sha}.dylib"));
    let lib_str = lib_path.to_str().ok_or("non-UTF8 cache path")?;
    if !lib_path.is_file() {
        eprintln!("first launch: translating image (one-time)...");
        if status {
            status_line("building 0");
        }
        build_dylib(rom_path, true, lib_str, &mut |pct| {
            if status {
                status_line(&format!("building {pct}"));
            }
        })?;
    }
    let lib = unsafe { libloading::Library::new(&lib_path) }
        .map_err(|e| format!("{}: {e}", lib_path.display()))?;
    let table = BlockTable::load(&lib)?;
    eprintln!("native translation: {} blocks", table.len);
    Ok((lib, table))
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
    std::fs::create_dir_all("out").map_err(|e| e.to_string())?;
    let stem = Path::new(rom_path)
        .file_stem().and_then(|s| s.to_str()).unwrap_or("game").to_string();
    build_dylib(rom_path, ram, &format!("out/{stem}.dylib"), &mut |_| {})
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
    lib_path: &str,
    progress: &mut dyn FnMut(u8),
) -> Result<(), String> {
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
        let mut last_pct = 0u8;
        while m.bus.frames < 240 && steps < 60_000_000 {
            steps += 1;
            // Profiling occupies the first 20% of the reported build.
            let pct = (m.bus.frames * 20 / 240) as u8;
            if pct != last_pct {
                last_pct = pct;
                progress(pct);
            }
            // Seed observed control-transfer targets in IWRAM and ROM.
            // ROM targets recover code static traversal can't reach
            // (computed branches, handlers installed by RAM code) and
            // need no guard — ROM is immutable. IWRAM blocks are
            // content-guarded at execution time. EWRAM stays excluded:
            // it commonly holds streamed overlays, which defeat entry
            // guards; that needs write-watch invalidation first.
            let seedable = |pc: u32| pc >> 24 == 3 || (0x08..=0x0D).contains(&(pc >> 24));
            match m.step() {
                StepEvent::Instr(instr) => {
                    let pc = m.cpu.regs[15];
                    if pc != prev_end && seedable(pc) {
                        seeds.insert(pc | m.cpu.thumb() as u32);
                    }
                    prev_end = instr.addr.wrapping_add(instr.size());
                }
                StepEvent::Idle => {
                    let pc = m.cpu.regs[15];
                    if pc != prev_end && seedable(pc) {
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
    progress(22);

    let prefix = lib_path.strip_suffix(".dylib").unwrap_or(lib_path);

    // Bounded translation units, compiled one at a time: full-image
    // translations can exceed the source image a hundredfold, and a single
    // huge unit makes cc balloon to many GB (parallel sweeps then exhaust
    // the machine). 16 MB of C keeps each cc invocation modest.
    const MAX_UNIT: usize = 16 << 20;
    let total_blocks = analysis.blocks.len().max(1);
    let mut objs: Vec<String> = Vec::new();
    let chunks = emit::emit_c_chunked(&analysis, &view, MAX_UNIT, |c, blocks_done| {
        let i = objs.len();
        let c_path = format!("{prefix}.{i}.c");
        let o_path = format!("{prefix}.{i}.o");
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
        // Compiling dominates the build: it spans 22..=96 of the report.
        progress(22 + (blocks_done * 74 / total_blocks) as u8);
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
    progress(100);
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
    other: std::collections::HashMap<u32, BlockFn>,
    len: usize,
}

const IWRAM_BASE: u32 = 0x0300_0000;

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
        let mut t = BlockTable {
            rom: vec![None; rom_max],
            iwram: vec![None; 0x8000],
            other: std::collections::HashMap::new(),
            len: blocks.len(),
        };
        for b in blocks {
            let r = b.key.wrapping_sub(ROM_BASE) as usize;
            let w = b.key.wrapping_sub(IWRAM_BASE) as usize;
            if r < t.rom.len() {
                t.rom[r] = Some(b.func);
            } else if w < t.iwram.len() {
                t.iwram[w] = Some(b.func);
            } else {
                t.other.insert(b.key, b.func);
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
    while !m.bus.frame_ready && steps < max_steps {
        steps += 1;
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
        match table.get(key) {
            Some(f) => {
                f(&RT_API, mptr);
                native += 1;
            }
            None => {
                m.step();
                fallback += 1;
            }
        }
    }
    (native, fallback)
}

/// Per-frame stall guard for the native loop: generous beyond any real
/// frame (a frame is ~280K cycles) while still bounding a wedged run.
const FRAME_STEP_GUARD: u64 = 200_000_000;

/// Run a recompiled image: translated blocks where available, interpreter
/// fallback elsewhere, interrupt machinery at block boundaries.
fn cmd_runc(args: &[String]) -> Result<(), String> {

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
    let table = BlockTable::load(&lib)?;
    println!("loaded {} translated blocks", table.len);

    let mut m = Machine::new(rom);
    let mptr = &mut m as *mut Machine as *mut std::ffi::c_void;

    let mut native_blocks = 0u64;
    let mut fallback_steps = 0u64;
    while m.bus.frames < frames {
        let before = m.bus.frames;
        let (n, f) = run_frame_native(&mut m, &table, mptr, FRAME_STEP_GUARD);
        native_blocks += n;
        fallback_steps += f;
        if m.bus.frames == before {
            return Err("step guard exceeded".into());
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
    let mut reuse = false;
    let mut dump: Option<String> = None;
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
            other if rom_path.is_none() => rom_path = Some(other.to_string()),
            other => return Err(format!("unexpected argument {other:?}")),
        }
    }
    let rom_path = rom_path.ok_or("missing ROM path")?;

    let stem = Path::new(&rom_path)
        .file_stem().and_then(|s| s.to_str()).unwrap_or("game").to_string();
    if !(reuse && Path::new(&format!("out/{stem}.dylib")).is_file()) {
        cmd_build(&[rom_path.clone()])?;
    }
    let interp = run_hash(&rom_path, frames, false,
        dump.as_ref().map(|p| format!("{p}.interp.ppm")))?;
    let recomp = run_hash(&rom_path, frames, true,
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
    dump: Option<String>,
) -> Result<u64, String> {
    let rom = std::fs::read(rom_path).map_err(|e| format!("{rom_path}: {e}"))?;
    let mut m = Machine::new(rom);
    if !recompiled {
        for _ in 0..frames {
            m.bus.keys = demo_keys(m.bus.frames);
            m.run_frame(5_000_000);
        }
        dump_frame(&m, dump)?;
        return Ok(fb_hash(&m));
    }
    let stem = Path::new(rom_path)
        .file_stem().and_then(|s| s.to_str()).unwrap_or("game").to_string();
    let lib_path = format!("out/{stem}.dylib");
    let lib = unsafe { libloading::Library::new(&lib_path) }
        .map_err(|e| format!("{lib_path}: {e}"))?;
    let table = BlockTable::load(&lib)?;
    let mptr = &mut m as *mut Machine as *mut std::ffi::c_void;
    while m.bus.frames < frames {
        let before = m.bus.frames;
        m.bus.keys = demo_keys(before);
        run_frame_native(&mut m, &table, mptr, FRAME_STEP_GUARD);
        if m.bus.frames == before {
            return Err("step guard exceeded".into());
        }
    }
    dump_frame(&m, dump)?;
    Ok(fb_hash(&m))
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
