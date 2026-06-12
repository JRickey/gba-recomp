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
        0x02 => {
            bus.hle_halt();
            true
        }
        0x04 | 0x05 => {
            // IntrWait / VBlankIntrWait (the latter = IntrWait(1, VBlank)).
            let (discard, mask) = if num == 0x05 {
                (true, 1)
            } else {
                (cpu.regs[0] != 0, cpu.regs[1] as u16)
            };
            if !bus.hle_intr_wait(discard, mask) {
                // Halted: rewind so this SWI re-executes after the wake
                // and handler run, mirroring the BIOS halt-recheck loop.
                cpu.branch = Some(cpu.regs[15]);
            }
            true
        }
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
        0x09 => {
            // ArcTan(r0 = tan, 1.14 fixed) -> r0 = angle (-0x4000..0x4000).
            let t = cpu.regs[0] as i16 as f64 / 16384.0;
            cpu.regs[0] = ((t.atan() / std::f64::consts::PI) * 32768.0) as i32 as u32;
            true
        }
        0x0A => {
            // ArcTan2(r0 = x, r1 = y), 1.14 fixed -> r0 = angle 0..0xFFFF.
            let x = cpu.regs[0] as i16 as f64;
            let y = cpu.regs[1] as i16 as f64;
            let angle = y.atan2(x) / (2.0 * std::f64::consts::PI) * 65536.0;
            cpu.regs[0] = (angle as i32 as u32) & 0xFFFF;
            true
        }
        0x01 => {
            register_ram_reset(cpu, bus);
            true
        }
        0x0B => cpu_set(cpu, bus),
        0x0C => cpu_fast_set(cpu, bus),
        0x0D => {
            cpu.regs[0] = 0xBAAE_187F; // GetBiosChecksum
            true
        }
        0x0E => {
            bg_affine_set(cpu, bus);
            true
        }
        0x0F => {
            obj_affine_set(cpu, bus);
            true
        }
        0x10 => {
            bit_unpack(cpu, bus);
            true
        }
        0x11 | 0x12 => {
            lz77_uncomp(cpu, bus);
            true
        }
        0x13 => {
            huff_uncomp(cpu, bus);
            true
        }
        0x14 | 0x15 => {
            rl_uncomp(cpu, bus);
            true
        }
        0x1F => {
            // MidiKey2Freq(wa, key, fine): sample frequency for a MIDI
            // key. freq = wa->freq / 2^((180 - key - fine/256) / 12).
            // Sound-driver builds that call the BIOS for this produce
            // pure noise if it returns garbage — every sample step in
            // the mixer goes wild.
            let base = bus.read32(cpu.regs[0].wrapping_add(4)) as f64;
            let exp = (180.0 - cpu.regs[1] as f64 - cpu.regs[2] as f64 / 256.0) / 12.0;
            cpu.regs[0] = (base / exp.exp2()) as u32;
            true
        }
        _ => false,
    }
}

/// RegisterRamReset (r0 = flags). Clears the selected memory regions and
/// resets I/O to power-on defaults. The last 0x200 bytes of IWRAM (BIOS
/// work area, incl. the IRQ handler pointer) are preserved.
fn register_ram_reset<B: Bus>(cpu: &mut Cpu, bus: &mut B) {
    let flags = cpu.regs[0];
    if flags & 0x01 != 0 {
        for a in (0x0200_0000u32..0x0204_0000).step_by(4) {
            bus.write32(a, 0);
        }
    }
    if flags & 0x02 != 0 {
        for a in (0x0300_0000u32..0x0300_7E00).step_by(4) {
            bus.write32(a, 0);
        }
    }
    if flags & 0x04 != 0 {
        for a in (0x0500_0000u32..0x0500_0400).step_by(4) {
            bus.write32(a, 0);
        }
    }
    if flags & 0x08 != 0 {
        for a in (0x0600_0000u32..0x0601_8000).step_by(4) {
            bus.write32(a, 0);
        }
    }
    if flags & 0x10 != 0 {
        for a in (0x0700_0000u32..0x0700_0400).step_by(4) {
            bus.write32(a, 0);
        }
    }
    if flags & 0x80 != 0 {
        // "Other" registers: display comes up in forced blank.
        bus.write16(0x0400_0000, 0x0080);
    }
}

/// BgAffineSet: r0 = src array, r1 = dst array, r2 = count.
/// Src (20 bytes): ox.8, oy.8 (i32 19.8), cx, cy (i16), sx, sy (i16 8.8),
/// theta (u16, 0..0xFFFF circle). Dst (16 bytes): pa..pd, dx, dy.
fn bg_affine_set<B: Bus>(cpu: &mut Cpu, bus: &mut B) {
    let mut src = cpu.regs[0];
    let mut dst = cpu.regs[1];
    for _ in 0..cpu.regs[2] {
        let ox = bus.read32(src) as i32 as f64 / 256.0;
        let oy = bus.read32(src + 4) as i32 as f64 / 256.0;
        let cx = bus.read16(src + 8) as i16 as f64;
        let cy = bus.read16(src + 10) as i16 as f64;
        let sx = bus.read16(src + 12) as i16 as f64 / 256.0;
        let sy = bus.read16(src + 14) as i16 as f64 / 256.0;
        let theta = bus.read16(src + 16) as f64 / 65536.0 * 2.0 * std::f64::consts::PI;
        src += 20;

        let (sin, cos) = theta.sin_cos();
        let pa = sx * cos;
        let pb = -sx * sin;
        let pc = sy * sin;
        let pd = sy * cos;

        bus.write16(dst, (pa * 256.0) as i32 as u16);
        bus.write16(dst + 2, (pb * 256.0) as i32 as u16);
        bus.write16(dst + 4, (pc * 256.0) as i32 as u16);
        bus.write16(dst + 6, (pd * 256.0) as i32 as u16);
        let dx = ox - (pa * cx + pb * cy);
        let dy = oy - (pc * cx + pd * cy);
        bus.write32(dst + 8, (dx * 256.0) as i32 as u32);
        bus.write32(dst + 12, (dy * 256.0) as i32 as u32);
        dst += 16;
    }
}

/// ObjAffineSet: r0 = src, r1 = dst, r2 = count, r3 = dst stride
/// (2 = packed, 8 = OAM layout). Src (8 bytes): sx, sy (8.8), theta, pad.
fn obj_affine_set<B: Bus>(cpu: &mut Cpu, bus: &mut B) {
    let mut src = cpu.regs[0];
    let mut dst = cpu.regs[1];
    let stride = cpu.regs[3];
    for _ in 0..cpu.regs[2] {
        let sx = bus.read16(src) as i16 as f64 / 256.0;
        let sy = bus.read16(src + 2) as i16 as f64 / 256.0;
        let theta = bus.read16(src + 4) as f64 / 65536.0 * 2.0 * std::f64::consts::PI;
        src += 8;

        let (sin, cos) = theta.sin_cos();
        bus.write16(dst, (sx * cos * 256.0) as i32 as u16);
        bus.write16(dst + stride, (-sx * sin * 256.0) as i32 as u16);
        bus.write16(dst + stride * 2, (sy * sin * 256.0) as i32 as u16);
        bus.write16(dst + stride * 3, (sy * cos * 256.0) as i32 as u16);
        dst += stride * 4;
    }
}

/// BitUnPack: r0 = src, r1 = dst (word-aligned), r2 = info ptr:
/// u16 src length (bytes), u8 src width, u8 dst width, u32 offset
/// (bit 31: also add offset to zero units).
fn bit_unpack<B: Bus>(cpu: &mut Cpu, bus: &mut B) {
    let src = cpu.regs[0];
    let dst = cpu.regs[1];
    let info = cpu.regs[2];
    let len = bus.read16(info) as u32;
    let src_width = bus.read8(info + 2) as u32;
    let dst_width = bus.read8(info + 3) as u32;
    let offset_field = bus.read32(info + 4);
    let offset = offset_field & 0x7FFF_FFFF;
    let zero_flag = offset_field & 0x8000_0000 != 0;

    if src_width == 0 || dst_width == 0 || dst_width > 32 {
        return;
    }
    let mut out_acc: u32 = 0;
    let mut out_bits: u32 = 0;
    let mut out_addr = dst;
    let src_mask = (1u32 << src_width) - 1;

    for i in 0..len {
        let byte = bus.read8(src + i) as u32;
        let mut bit = 0;
        while bit < 8 {
            let mut unit = (byte >> bit) & src_mask;
            if unit != 0 || zero_flag {
                unit = unit.wrapping_add(offset);
            }
            out_acc |= unit << out_bits;
            out_bits += dst_width;
            if out_bits >= 32 {
                bus.write32(out_addr, out_acc);
                out_addr += 4;
                out_acc = 0;
                out_bits = 0;
            }
            bit += src_width;
        }
    }
    if out_bits > 0 {
        bus.write32(out_addr, out_acc);
    }
}

/// LZ77UnComp (WRAM/VRAM variants; we produce identical output).
/// Header u32 at src: bits 8-31 = decompressed size. Stream: flag byte
/// (MSB first); bit clear = literal byte, bit set = back-reference
/// (disp = 12 bits + 1, len = 4 bits + 3).
fn lz77_uncomp<B: Bus>(cpu: &mut Cpu, bus: &mut B) {
    let mut src = cpu.regs[0];
    let dst = cpu.regs[1];
    let size = (bus.read32(src) >> 8) as usize;
    src += 4;

    let mut out: Vec<u8> = Vec::with_capacity(size);
    while out.len() < size {
        let flags = bus.read8(src);
        src += 1;
        for bit in (0..8).rev() {
            if out.len() >= size {
                break;
            }
            if flags & (1 << bit) == 0 {
                out.push(bus.read8(src));
                src += 1;
            } else {
                let b0 = bus.read8(src) as usize;
                let b1 = bus.read8(src + 1) as usize;
                src += 2;
                let len = (b0 >> 4) + 3;
                let disp = ((b0 & 0xF) << 8 | b1) + 1;
                for _ in 0..len {
                    if out.len() >= size {
                        break;
                    }
                    let v = if disp <= out.len() {
                        out[out.len() - disp]
                    } else {
                        0
                    };
                    out.push(v);
                }
            }
        }
    }
    write_out(bus, dst, &out);
}

/// HuffUnComp. Header u32 at src: bits 0-3 = data unit size in bits
/// (4 or 8), bits 8-31 = decompressed size in bytes. src+4: tree size
/// byte n (bitstream starts at src+4 + (n+1)*2), src+5: tree table,
/// root first. Non-leaf node: bits 0-5 = offset (child0 at
/// (addr AND NOT 1) + offset*2 + 2, child1 right after), bit 7 = child0
/// is a leaf, bit 6 = child1 is a leaf; a leaf byte is the data unit.
/// Bitstream: u32 words consumed MSB-first (0 = child0, 1 = child1).
/// Units pack into output bytes LSB-first.
fn huff_uncomp<B: Bus>(cpu: &mut Cpu, bus: &mut B) {
    let src = cpu.regs[0];
    let dst = cpu.regs[1];
    let header = bus.read32(src);
    let unit_bits = (header & 0xF).clamp(1, 8);
    let size = (header >> 8) as usize;
    let tree_size = bus.read8(src + 4) as u32;
    let root = src + 5;
    let mut stream = src + 4 + (tree_size + 1) * 2;

    let mut word = 0u32;
    let mut bits_left = 0u32;
    let mut byte = 0u8;
    let mut shift = 0u32;
    let mut out: Vec<u8> = Vec::with_capacity(size);
    while out.len() < size {
        // Walk the tree one bit at a time until a leaf yields a unit.
        let mut node = root;
        let unit = loop {
            if bits_left == 0 {
                word = bus.read32(stream);
                stream += 4;
                bits_left = 32;
            }
            let b = word >> 31;
            word <<= 1;
            bits_left -= 1;

            let n = bus.read8(node);
            let child = (node & !1) + 2 + (n as u32 & 0x3F) * 2 + b;
            let leaf = n & if b == 0 { 0x80 } else { 0x40 } != 0;
            if leaf {
                break bus.read8(child);
            }
            node = child;
        };
        byte |= unit << shift;
        shift += unit_bits;
        if shift >= 8 {
            out.push(byte);
            byte = 0;
            shift = 0;
        }
    }
    write_out(bus, dst, &out);
}

/// RLUnComp: flag byte bit 7 set = run (len = low 7 + 3, one byte),
/// clear = literals (len = low 7 + 1).
fn rl_uncomp<B: Bus>(cpu: &mut Cpu, bus: &mut B) {
    let mut src = cpu.regs[0];
    let dst = cpu.regs[1];
    let size = (bus.read32(src) >> 8) as usize;
    src += 4;

    let mut out: Vec<u8> = Vec::with_capacity(size);
    while out.len() < size {
        let flag = bus.read8(src) as usize;
        src += 1;
        if flag & 0x80 != 0 {
            let len = (flag & 0x7F) + 3;
            let v = bus.read8(src);
            src += 1;
            for _ in 0..len.min(size - out.len()) {
                out.push(v);
            }
        } else {
            let len = (flag & 0x7F) + 1;
            for _ in 0..len.min(size - out.len()) {
                out.push(bus.read8(src));
                src += 1;
            }
        }
    }
    write_out(bus, dst, &out);
}

/// Write decompressed output as halfwords (works for both the WRAM and
/// VRAM SWI variants; VRAM ignores byte writes, so halfword stores are
/// the safe common denominator).
fn write_out<B: Bus>(bus: &mut B, dst: u32, data: &[u8]) {
    let mut i = 0;
    while i + 1 < data.len() {
        bus.write16(dst + i as u32, u16::from_le_bytes([data[i], data[i + 1]]));
        i += 2;
    }
    if i < data.len() {
        bus.write16(dst + i as u32, data[i] as u16);
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
            let v = if fill {
                fill_val
            } else {
                bus.read32(src + i * 4)
            };
            bus.write32(dst + i * 4, v);
        }
    } else {
        let src = src & !1;
        let dst = dst & !1;
        let fill_val = if fill { bus.read16(src) } else { 0 };
        for i in 0..count {
            let v = if fill {
                fill_val
            } else {
                bus.read16(src + i * 2)
            };
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
        let v = if fill {
            fill_val
        } else {
            bus.read32(src + i * 4)
        };
        bus.write32(dst + i * 4, v);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::MemMap;

    /// Two-symbol tree: root at src+5 with both children leaves
    /// (offset 0), leaf0 at src+6, leaf1 at src+7, bitstream at src+8.
    fn huff_fixture(bus: &mut MemMap, src: u32, header: u32, l0: u8, l1: u8, stream: u32) {
        bus.write32(src, header);
        bus.write8(src + 4, 1); // tree size byte: stream at src+4+(1+1)*2
        bus.write8(src + 5, 0xC0);
        bus.write8(src + 6, l0);
        bus.write8(src + 7, l1);
        bus.write32(src + 8, stream);
    }

    #[test]
    fn huff_uncomp_8bit() {
        let mut bus = MemMap::new(vec![]);
        let (src, dst) = (0x0200_0000u32, 0x0200_0100u32);
        // 4 bytes decompressed, 8-bit units; bits 0,0,1,0 -> A A B A
        huff_fixture(&mut bus, src, (4 << 8) | 0x28, 0x41, 0x42, 0b0010 << 28);
        let mut cpu = Cpu::new();
        cpu.regs[0] = src;
        cpu.regs[1] = dst;
        assert!(bios_call(&mut cpu, &mut bus, 0x13));
        assert_eq!(bus.read32(dst), 0x4142_4141);
    }

    #[test]
    fn huff_uncomp_4bit_packs_low_nibble_first() {
        let mut bus = MemMap::new(vec![]);
        let (src, dst) = (0x0200_0000u32, 0x0200_0100u32);
        // 2 bytes decompressed, 4-bit units; bits 0,1,1,0 -> 1,2,2,1
        huff_fixture(&mut bus, src, (2 << 8) | 0x24, 0x1, 0x2, 0b0110 << 28);
        let mut cpu = Cpu::new();
        cpu.regs[0] = src;
        cpu.regs[1] = dst;
        assert!(bios_call(&mut cpu, &mut bus, 0x13));
        assert_eq!(bus.read16(dst), 0x1221);
    }
}
