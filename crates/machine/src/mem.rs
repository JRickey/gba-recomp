//! Memory map (hardware reference, "Memory Map").
//!
//! | Region  | Address    | Size   | Mirroring                      |
//! |---------|------------|--------|--------------------------------|
//! | BIOS    | 0x00000000 | 16 KB  | none                           |
//! | EWRAM   | 0x02000000 | 256 KB | every 256 KB through 0x02FFFFFF|
//! | IWRAM   | 0x03000000 | 32 KB  | every 32 KB through 0x03FFFFFF |
//! | I/O     | 0x04000000 | 1 KB   | (0x04000800 quirk unmodeled)   |
//! | Palette | 0x05000000 | 1 KB   | every 1 KB                     |
//! | VRAM    | 0x06000000 | 96 KB  | 64K+32K, 32K doubled, every 128K|
//! | OAM     | 0x07000000 | 1 KB   | every 1 KB                     |
//! | ROM     | 0x08000000 | ≤32 MB | ×3 waitstate images 08/0A/0C   |
//! | SRAM    | 0x0E000000 | 64 KB  | mirrored through 0x0FFFFFFF    |
//!
//! 8-bit write quirks (hardware reference, "Writing 8bit Data to Video Memory"):
//! byte writes to palette and BG VRAM are duplicated into the halfword;
//! byte writes to OBJ VRAM and OAM are ignored.
//!
//! Open bus and BIOS-region protection are approximated for now (return 0 /
//! ROM out-of-bounds pattern); the precise last-fetch behavior arrives with
//! the prefetch model. I/O registers are currently backed by plain bytes
//! with KEYINPUT defaulting to "no keys pressed" — real peripherals land in
//! later milestones.

use crate::bus::Bus;

const DISPSTAT: u32 = 0x0400_0004;
const VCOUNT: u32 = 0x0400_0006;
const KEYINPUT: u32 = 0x0400_0130;

/// Video timing grid (hardware reference, "LCD Dimensions and Timings").
pub const CYCLES_PER_SCANLINE: u64 = 1232;
pub const SCANLINES_PER_FRAME: u64 = 228;
pub const VISIBLE_SCANLINES: u64 = 160;
/// HBlank flag asserts at cycle ~1006 of the scanline, not at 960
/// (hardware-measured; external timing suites test this).
pub const HBLANK_FLAG_CYCLE: u64 = 1006;

pub struct MemMap {
    pub bios: Vec<u8>,
    pub ewram: Vec<u8>,
    pub iwram: Vec<u8>,
    pub io: Vec<u8>,
    pub palette: Vec<u8>,
    pub vram: Vec<u8>,
    pub oam: Vec<u8>,
    pub rom: Vec<u8>,
    pub sram: Vec<u8>,
    /// Master cycle counter driving derived video timing. A real event
    /// scheduler replaces this in M1; the grid constants stay.
    pub clock: u64,
}

impl MemMap {
    pub fn new(rom: Vec<u8>) -> MemMap {
        let mut mem = MemMap {
            bios: vec![0; 0x4000],
            ewram: vec![0; 0x4_0000],
            iwram: vec![0; 0x8000],
            io: vec![0; 0x400],
            palette: vec![0; 0x400],
            vram: vec![0; 0x1_8000],
            oam: vec![0; 0x400],
            rom,
            sram: vec![0xFF; 0x1_0000],
            clock: 0,
        };
        // KEYINPUT: active-low, all released.
        mem.io[(KEYINPUT & 0x3FF) as usize] = 0xFF;
        mem.io[(KEYINPUT & 0x3FF) as usize + 1] = 0x03;
        mem
    }

    /// Step the master clock.
    pub fn tick(&mut self, cycles: u64) {
        self.clock += cycles;
    }

    fn scanline(&self) -> u64 {
        (self.clock / CYCLES_PER_SCANLINE) % SCANLINES_PER_FRAME
    }

    /// DISPSTAT bits 0-2 derived from the clock; bits 3+ from the backing
    /// store (IRQ enables, VCount setting).
    fn dispstat_low(&self) -> u8 {
        let line = self.scanline();
        let dot_cycle = self.clock % CYCLES_PER_SCANLINE;
        let stored = self.io[(DISPSTAT & 0x3FF) as usize];
        let mut value = stored & !0x07;
        // VBlank flag: lines 160..=226 (not 227).
        if (VISIBLE_SCANLINES..SCANLINES_PER_FRAME - 1).contains(&line) {
            value |= 1;
        }
        if dot_cycle >= HBLANK_FLAG_CYCLE {
            value |= 2;
        }
        let vcount_setting = self.io[(DISPSTAT & 0x3FF) as usize + 1];
        if line == vcount_setting as u64 {
            value |= 4;
        }
        value
    }

    /// VRAM's 96 KB maps into a 128 KB window as 64K + 32K with the 32K
    /// block appearing twice.
    fn vram_index(addr: u32) -> usize {
        let mut off = (addr & 0x1_FFFF) as usize;
        if off >= 0x1_8000 {
            off -= 0x8000;
        }
        off
    }

    fn rom_index(addr: u32) -> usize {
        (addr & 0x01FF_FFFF) as usize
    }

    /// Reads past the cartridge's physical end return an address-derived
    /// 16-bit pattern (hardware reference: cartridge bus with nothing driving it).
    fn rom_open_bus(addr: u32) -> u8 {
        let half = (addr >> 1) & 0xFFFF;
        if addr & 1 == 0 {
            half as u8
        } else {
            (half >> 8) as u8
        }
    }
}

impl Bus for MemMap {
    fn read8(&mut self, addr: u32) -> u8 {
        match addr >> 24 {
            0x00 => {
                if addr < 0x4000 {
                    self.bios[addr as usize]
                } else {
                    0 // open bus (unmodeled)
                }
            }
            0x02 => self.ewram[(addr & 0x3_FFFF) as usize],
            0x03 => self.iwram[(addr & 0x7FFF) as usize],
            0x04 => match addr {
                DISPSTAT => self.dispstat_low(),
                VCOUNT => self.scanline() as u8,
                _ => {
                    let off = addr & 0x00FF_FFFF;
                    if off < 0x400 {
                        self.io[off as usize]
                    } else {
                        0
                    }
                }
            },
            0x05 => self.palette[(addr & 0x3FF) as usize],
            0x06 => self.vram[Self::vram_index(addr)],
            0x07 => self.oam[(addr & 0x3FF) as usize],
            0x08..=0x0D => {
                let idx = Self::rom_index(addr);
                if idx < self.rom.len() {
                    self.rom[idx]
                } else {
                    Self::rom_open_bus(addr)
                }
            }
            0x0E | 0x0F => self.sram[(addr & 0xFFFF) as usize],
            _ => 0, // open bus (unmodeled)
        }
    }

    fn write8(&mut self, addr: u32, value: u8) {
        match addr >> 24 {
            0x02 => self.ewram[(addr & 0x3_FFFF) as usize] = value,
            0x03 => self.iwram[(addr & 0x7FFF) as usize] = value,
            0x04 => {
                let off = addr & 0x00FF_FFFF;
                if off < 0x400 {
                    self.io[off as usize] = value;
                }
            }
            0x05 => {
                // Byte writes duplicate into the aligned halfword.
                let base = (addr & 0x3FE) as usize;
                self.palette[base] = value;
                self.palette[base + 1] = value;
            }
            0x06 => {
                // Byte writes: duplicated in BG VRAM, ignored in OBJ VRAM.
                // (BG/OBJ boundary at 0x10000 for bitmap-mode simplicity;
                // mode-dependent boundary refines this later.)
                let idx = Self::vram_index(addr & !1);
                if idx < 0x1_0000 {
                    self.vram[idx] = value;
                    self.vram[idx + 1] = value;
                }
            }
            0x07 => {} // byte writes to OAM are ignored
            0x0E | 0x0F => self.sram[(addr & 0xFFFF) as usize] = value,
            _ => {} // BIOS/ROM writes: backup-media commands, modeled later
        }
    }

    fn write16(&mut self, addr: u32, value: u16) {
        // Bypass the byte-write quirks for naturally sized accesses.
        match addr >> 24 {
            0x05 => {
                let base = (addr & 0x3FE) as usize;
                self.palette[base..base + 2].copy_from_slice(&value.to_le_bytes());
            }
            0x06 => {
                let idx = Self::vram_index(addr & !1);
                self.vram[idx..idx + 2].copy_from_slice(&value.to_le_bytes());
            }
            0x07 => {
                let base = (addr & 0x3FE) as usize;
                self.oam[base..base + 2].copy_from_slice(&value.to_le_bytes());
            }
            _ => {
                self.write8(addr, value as u8);
                self.write8(addr.wrapping_add(1), (value >> 8) as u8);
            }
        }
    }

    fn write32(&mut self, addr: u32, value: u32) {
        self.write16(addr & !1, value as u16);
        self.write16((addr & !1).wrapping_add(2), (value >> 16) as u16);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirrors() {
        let mut mem = MemMap::new(vec![0xAB; 0x100]);
        mem.write32(0x0200_0000, 0xDEAD_BEEF);
        assert_eq!(mem.read32(0x0204_0000), 0xDEAD_BEEF); // EWRAM mirror
        assert_eq!(mem.read32(0x02FC_0000), 0xDEAD_BEEF);
        mem.write32(0x0300_0000, 0x1234_5678);
        assert_eq!(mem.read32(0x0300_8000), 0x1234_5678); // IWRAM mirror
        assert_eq!(mem.read32(0x03FF_8000), 0x1234_5678);
        // ROM mirrors across waitstate regions.
        assert_eq!(mem.read8(0x0800_0000), 0xAB);
        assert_eq!(mem.read8(0x0A00_0000), 0xAB);
        assert_eq!(mem.read8(0x0C00_0000), 0xAB);
    }

    #[test]
    fn vram_mirror_layout() {
        let mut mem = MemMap::new(vec![]);
        mem.write16(0x0601_0000, 0x1111);
        // 32K block is doubled: 0x18000 maps back to 0x10000.
        assert_eq!(mem.read16(0x0601_8000), 0x1111);
        // And the whole 128K window mirrors.
        assert_eq!(mem.read16(0x0603_0000), 0x1111);
    }

    #[test]
    fn byte_write_quirks() {
        let mut mem = MemMap::new(vec![]);
        mem.write8(0x0500_0001, 0x42); // palette: duplicated into halfword
        assert_eq!(mem.read16(0x0500_0000), 0x4242);
        mem.write8(0x0700_0000, 0x42); // OAM: ignored
        assert_eq!(mem.read16(0x0700_0000), 0);
        mem.write8(0x0600_0000, 0x37); // BG VRAM: duplicated
        assert_eq!(mem.read16(0x0600_0000), 0x3737);
        mem.write8(0x0601_0001, 0x99); // OBJ VRAM: ignored
        assert_eq!(mem.read16(0x0601_0000), 0);
    }

    #[test]
    fn rom_out_of_bounds_pattern() {
        let mut mem = MemMap::new(vec![0; 4]);
        // Reads past ROM end: (addr/2) & 0xFFFF, little-endian.
        assert_eq!(mem.read16(0x0800_0010), 0x0008);
        assert_eq!(mem.read16(0x0800_0012), 0x0009);
    }

    #[test]
    fn keyinput_defaults_released() {
        let mut mem = MemMap::new(vec![]);
        assert_eq!(mem.read16(0x0400_0130), 0x03FF);
    }
}
