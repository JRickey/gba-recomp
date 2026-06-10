//! Static recompiler CLI.
//!
//! Current commands exercise the decoder against real ROMs:
//!   dis <rom> [--addr HEX] [--count N] [--thumb]   disassemble from an address
//!   entry-scan <dir>                               decode every ROM's entry point

use std::path::Path;
use std::process::ExitCode;

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
        _ => {
            eprintln!("usage: recomp dis <rom> [--addr HEX] [--count N] [--thumb]");
            eprintln!("       recomp entry-scan <dir>");
            eprintln!("       recomp run <rom> [--max-steps N] [--trace]");
            eprintln!("       recomp frames <rom> [--frames N] [--out img.ppm] [--keys MASK]");
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
                eprintln!("{:08x}: {}", instr.addr, instr.disasm());
            }
            // Histogram the tail of the run (where the steady state lives).
            if hist && steps > max_steps.saturating_sub(500_000) {
                *counts.entry(instr.addr).or_default() += 1;
            }
            // Parked: unconditional branch to itself, with no way out.
            if is_self_loop(&instr) && !m.bus.irq_pending() && m.bus.intr_wait.is_none() {
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
        println!("ie={:04x} if={:04x} ime={} dispstat={dispstat:04x} halted={} intr_wait={:?} biosflags={biosflags:04x}", m.bus.reg_ie, m.bus.reg_if, m.bus.ime, m.bus.halted, m.bus.intr_wait);
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
