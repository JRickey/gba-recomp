//! High-level emulation of BIOS SWI calls (hardware reference, "BIOS Functions").
//!
//! Games rarely use the BIOS sound driver, but the arithmetic and memory
//! SWIs are pervasive. Register/flag side effects matter: games observe
//! clobbers. Functions not yet handled return `false` and take the real
//! SWI exception path (which requires a loaded BIOS image).

use crate::bus::Bus;
use crate::cpu::Cpu;

/// Attempt to handle BIOS call `num`. Returns true if fully handled.
pub fn bios_call<B: Bus>(cpu: &mut Cpu, bus: &mut B, num: u32) -> bool {
    match num {
        0x06 => {
            div(cpu, cpu.regs[0] as i32, cpu.regs[1] as i32);
            true
        }
        0x07 => {
            // DivArm: operands swapped relative to Div.
            div(cpu, cpu.regs[1] as i32, cpu.regs[0] as i32);
            true
        }
        0x08 => {
            cpu.regs[0] = (cpu.regs[0] as f64).sqrt() as u32;
            true
        }
        0x0B => cpu_set(cpu, bus),
        0x0C => cpu_fast_set(cpu, bus),
        _ => false,
    }
}

fn div(cpu: &mut Cpu, number: i32, denom: i32) {
    if denom == 0 {
        // Hardware BIOS returns garbage without hanging; the common
        // observed result is number/±1 sign artifacts. Keep it
        // quotient saturates to ±1 patterns. Keep it simple and defined.
        let q = if number < 0 { -1 } else { 1 };
        cpu.regs[0] = q as u32;
        cpu.regs[1] = number as u32;
        cpu.regs[3] = 1;
        return;
    }
    let q = number.wrapping_div(denom);
    let r = number.wrapping_rem(denom);
    cpu.regs[0] = q as u32;
    cpu.regs[1] = r as u32;
    cpu.regs[3] = q.unsigned_abs();
}

/// CpuSet: r0=src, r1=dst, r2 = count[0:20] | fill[24] | word[26].
fn cpu_set<B: Bus>(cpu: &mut Cpu, bus: &mut B) -> bool {
    let src = cpu.regs[0];
    let dst = cpu.regs[1];
    let control = cpu.regs[2];
    let count = control & 0x1F_FFFF;
    let fill = control & (1 << 24) != 0;
    let word = control & (1 << 26) != 0;

    if word {
        let src = src & !3;
        let dst = dst & !3;
        let fill_val = if fill { bus.read32(src) } else { 0 };
        for i in 0..count {
            let v = if fill { fill_val } else { bus.read32(src + i * 4) };
            bus.write32(dst + i * 4, v);
        }
    } else {
        let src = src & !1;
        let dst = dst & !1;
        let fill_val = if fill { bus.read16(src) } else { 0 };
        for i in 0..count {
            let v = if fill { fill_val } else { bus.read16(src + i * 2) };
            bus.write16(dst + i * 2, v);
        }
    }
    true
}

/// CpuFastSet: word transfers in chunks of 8; count rounded up to 8.
fn cpu_fast_set<B: Bus>(cpu: &mut Cpu, bus: &mut B) -> bool {
    let src = cpu.regs[0] & !3;
    let dst = cpu.regs[1] & !3;
    let control = cpu.regs[2];
    let count = (control & 0x1F_FFFF).div_ceil(8) * 8;
    let fill = control & (1 << 24) != 0;

    let fill_val = if fill { bus.read32(src) } else { 0 };
    for i in 0..count {
        let v = if fill { fill_val } else { bus.read32(src + i * 4) };
        bus.write32(dst + i * 4, v);
    }
    true
}
