//! Memory map and memory-mapped hardware (hardware reference, "Memory Map"
//! and I/O register chapters).
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
//! 8-bit write quirks: byte writes to palette and BG VRAM are duplicated
//! into the halfword; byte writes to OBJ VRAM and OAM are ignored.
//!
//! The bus also owns the master clock and the memory-mapped peripherals:
//! video timing events, interrupt latches (IE/IF/IME), the four timers,
//! and the four DMA channels. The `Machine` wrapper drives `tick()` and
//! handles CPU-side interrupt entry.

use crate::bus::Bus;

const DISPSTAT: u32 = 0x0400_0004;
const VCOUNT: u32 = 0x0400_0006;
const KEYINPUT: u32 = 0x0400_0130;
const REG_IE: u32 = 0x0400_0200;
const REG_IF: u32 = 0x0400_0202;
const REG_IME: u32 = 0x0400_0208;
const HALTCNT: u32 = 0x0400_0301;

/// Interrupt source bits in IE/IF.
pub mod irq {
    pub const VBLANK: u16 = 1 << 0;
    pub const HBLANK: u16 = 1 << 1;
    pub const VCOUNT: u16 = 1 << 2;
    pub const TIMER0: u16 = 1 << 3;
    pub const DMA0: u16 = 1 << 8;
    pub const KEYPAD: u16 = 1 << 12;
}

/// Video timing grid (hardware reference, "LCD Dimensions and Timings").
pub const CYCLES_PER_SCANLINE: u64 = 1232;
pub const SCANLINES_PER_FRAME: u64 = 228;
pub const VISIBLE_SCANLINES: u64 = 160;
pub const VISIBLE_WIDTH: usize = 240;
/// HBlank flag asserts at cycle ~1006 of the scanline, not at 960
/// (hardware-measured; external timing suites test this).
pub const HBLANK_FLAG_CYCLE: u64 = 1006;

#[derive(Default, Clone, Copy)]
struct Timer {
    reload: u16,
    control: u8,
    /// Counter value at `last_sync`.
    count: u16,
    /// Clock at which `count` was correct (prescaler-aligned).
    last_sync: u64,
}

impl Timer {
    fn enabled(&self) -> bool {
        self.control & 0x80 != 0
    }
    fn cascade(&self) -> bool {
        self.control & 0x04 != 0
    }
    fn irq_enabled(&self) -> bool {
        self.control & 0x40 != 0
    }
    fn prescaler_shift(&self) -> u32 {
        [0, 6, 8, 10][(self.control & 3) as usize]
    }
}

#[derive(Default, Clone, Copy)]
struct Dma {
    src: u32,
    dst: u32,
    count: u16,
    control: u16,
    /// Internal latches (set when the channel is enabled).
    cur_src: u32,
    cur_dst: u32,
}

impl Dma {
    fn enabled(&self) -> bool {
        self.control & 0x8000 != 0
    }
    fn timing(&self) -> u16 {
        (self.control >> 12) & 3
    }
    fn repeat(&self) -> bool {
        self.control & 0x0200 != 0
    }
    fn word(&self) -> bool {
        self.control & 0x0400 != 0
    }
    fn irq_enabled(&self) -> bool {
        self.control & 0x4000 != 0
    }
}

/// Video event phase within the current scanline.
#[derive(Clone, Copy, PartialEq)]
enum VPhase {
    Hblank,
    LineEnd,
}

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

    /// Master cycle counter.
    pub clock: u64,

    // Interrupt state.
    pub reg_ie: u16,
    pub reg_if: u16,
    pub ime: bool,
    pub halted: bool,
    /// HLE IntrWait state: mask being waited on at 0x03007FF8.
    pub intr_wait: Option<u16>,

    /// KEYINPUT value (active-low; 0x3FF = nothing pressed).
    pub keys: u16,

    timers: [Timer; 4],
    dma: [Dma; 4],

    // Video event tracking.
    vline_start: u64,
    vphase: VPhase,
    /// Set when a frame completes (start of VBlank); cleared by the runner.
    pub frame_ready: bool,
    pub frames: u64,

    /// 240×160 RGB555 output, written per scanline by the PPU.
    pub framebuffer: Vec<u16>,
    next_event: u64,
}

impl MemMap {
    pub fn new(rom: Vec<u8>) -> MemMap {
        MemMap {
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
            reg_ie: 0,
            reg_if: 0,
            ime: false,
            halted: false,
            intr_wait: None,
            keys: 0x3FF,
            timers: [Timer::default(); 4],
            dma: [Dma::default(); 4],
            vline_start: 0,
            vphase: VPhase::Hblank,
            frame_ready: false,
            frames: 0,
            framebuffer: vec![0; VISIBLE_WIDTH * VISIBLE_SCANLINES as usize],
            next_event: HBLANK_FLAG_CYCLE,
        }
    }

    /// Current scanline (0..=227).
    pub fn scanline(&self) -> u64 {
        (self.vline_start / CYCLES_PER_SCANLINE) % SCANLINES_PER_FRAME
    }

    /// True when an enabled interrupt is latched (wakes Halt regardless of IME).
    pub fn irq_line(&self) -> bool {
        self.reg_ie & self.reg_if != 0
    }

    /// True when the CPU should take the IRQ exception.
    pub fn irq_pending(&self) -> bool {
        self.ime && self.irq_line()
    }

    /// Advance the master clock and process any due events.
    pub fn tick(&mut self, cycles: u64) {
        self.clock += cycles;
        while self.clock >= self.next_event {
            self.process_events();
        }
    }

    /// Jump the clock to the next event (used while halted/waiting).
    pub fn skip_to_next_event(&mut self) {
        self.clock = self.clock.max(self.next_event);
        while self.clock >= self.next_event {
            self.process_events();
        }
    }

    fn video_due(&self) -> u64 {
        match self.vphase {
            VPhase::Hblank => self.vline_start + HBLANK_FLAG_CYCLE,
            VPhase::LineEnd => self.vline_start + CYCLES_PER_SCANLINE,
        }
    }

    fn process_events(&mut self) {
        // Timers first (their overflow time may precede the video event).
        self.advance_timers();
        if self.clock >= self.video_due() {
            self.process_video_event();
        }
        self.recompute_next_event();
    }

    fn recompute_next_event(&mut self) {
        let mut next = self.video_due();
        for i in 0..4 {
            if let Some(t) = self.timer_overflow_time(i) {
                next = next.min(t);
            }
        }
        self.next_event = next;
    }

    fn process_video_event(&mut self) {
        match self.vphase {
            VPhase::Hblank => {
                let line = self.scanline();
                if line < VISIBLE_SCANLINES {
                    crate::ppu::render_scanline(self, line as usize);
                    self.trigger_dma(2); // HBlank timing (visible lines only)
                }
                if self.io[(DISPSTAT & 0x3FF) as usize] & 0x10 != 0 {
                    self.raise_irq(irq::HBLANK);
                }
                self.vphase = VPhase::LineEnd;
            }
            VPhase::LineEnd => {
                self.vline_start += CYCLES_PER_SCANLINE;
                self.vphase = VPhase::Hblank;
                let line = self.scanline();
                let dispstat = self.io[(DISPSTAT & 0x3FF) as usize];
                let vcount_setting = self.io[(DISPSTAT & 0x3FF) as usize + 1];
                if line == vcount_setting as u64 && dispstat & 0x20 != 0 {
                    self.raise_irq(irq::VCOUNT);
                }
                if line == VISIBLE_SCANLINES {
                    self.frame_ready = true;
                    self.frames += 1;
                    if dispstat & 0x08 != 0 {
                        self.raise_irq(irq::VBLANK);
                    }
                    self.trigger_dma(1); // VBlank timing
                }
            }
        }
    }

    pub fn raise_irq(&mut self, bits: u16) {
        self.reg_if |= bits;
    }

    // ---- timers ----

    /// Clock at which timer `i` will next overflow (non-cascade only).
    fn timer_overflow_time(&self, i: usize) -> Option<u64> {
        let t = &self.timers[i];
        if !t.enabled() || t.cascade() {
            return None;
        }
        let remaining = (0x1_0000 - t.count as u64) << t.prescaler_shift();
        Some(t.last_sync + remaining)
    }

    fn advance_timers(&mut self) {
        for i in 0..4 {
            let t = self.timers[i];
            if !t.enabled() || t.cascade() {
                continue;
            }
            let shift = t.prescaler_shift();
            let elapsed = (self.clock.saturating_sub(t.last_sync)) >> shift;
            if elapsed == 0 {
                continue;
            }
            let total = t.count as u64 + elapsed;
            self.timers[i].last_sync += elapsed << shift;
            if total < 0x1_0000 {
                self.timers[i].count = total as u16;
            } else {
                let period = (0x1_0000 - t.reload as u64).max(1);
                let past = total - 0x1_0000;
                let overflows = 1 + past / period;
                self.timers[i].count = (t.reload as u64 + past % period) as u16;
                self.timer_overflowed(i, overflows);
            }
        }
    }

    fn timer_overflowed(&mut self, i: usize, times: u64) {
        if self.timers[i].irq_enabled() {
            self.raise_irq(irq::TIMER0 << i);
        }
        // Cascade into the next timer.
        if i < 3 && self.timers[i + 1].enabled() && self.timers[i + 1].cascade() {
            for _ in 0..times {
                let next = &mut self.timers[i + 1];
                let (count, wrapped) = next.count.overflowing_add(1);
                next.count = if wrapped { next.reload } else { count };
                if wrapped {
                    self.timer_overflowed(i + 1, 1);
                }
            }
        }
    }

    fn timer_read_count(&mut self, i: usize) -> u16 {
        self.advance_timers();
        self.timers[i].count
    }

    fn timer_write_control(&mut self, i: usize, value: u8) {
        self.advance_timers();
        let was_enabled = self.timers[i].enabled();
        self.timers[i].control = value;
        if !was_enabled && self.timers[i].enabled() {
            // Enabling reloads the counter.
            self.timers[i].count = self.timers[i].reload;
            self.timers[i].last_sync = self.clock;
        }
        self.recompute_next_event();
    }

    // ---- DMA ----

    /// Fire all enabled channels with the given start timing
    /// (0 = immediate, 1 = vblank, 2 = hblank, 3 = special).
    fn trigger_dma(&mut self, timing: u16) {
        for i in 0..4 {
            if self.dma[i].enabled() && self.dma[i].timing() == timing {
                self.run_dma(i);
            }
        }
    }

    fn run_dma(&mut self, i: usize) {
        let ch = self.dma[i];
        let max: u32 = if i == 3 { 0x1_0000 } else { 0x4000 };
        let count = if ch.count == 0 { max } else { ch.count as u32 };
        let unit: u32 = if ch.word() { 4 } else { 2 };

        let dst_adj = (ch.control >> 5) & 3;
        let src_adj = (ch.control >> 7) & 3;

        let mut src = ch.cur_src & if ch.word() { !3 } else { !1 };
        let mut dst = ch.cur_dst & if ch.word() { !3 } else { !1 };

        for _ in 0..count {
            if ch.word() {
                let v = self.read32(src);
                self.write32(dst, v);
            } else {
                let v = self.read16(src);
                self.write16(dst, v);
            }
            src = match src_adj {
                0 => src.wrapping_add(unit),
                1 => src.wrapping_sub(unit),
                _ => src, // 2 = fixed (3 is prohibited)
            };
            dst = match dst_adj {
                0 | 3 => dst.wrapping_add(unit),
                1 => dst.wrapping_sub(unit),
                _ => dst,
            };
        }

        self.dma[i].cur_src = src;
        // Rough cycle cost: 2 cycles per unit transferred plus setup.
        self.clock += 2 * count as u64 + 4;

        if ch.repeat() && ch.timing() != 0 {
            // Repeat: count reloads; destination reloads in mode 3.
            if dst_adj == 3 {
                self.dma[i].cur_dst = ch.cur_dst;
            } else {
                self.dma[i].cur_dst = dst;
            }
        } else {
            self.dma[i].cur_dst = dst;
            self.dma[i].control &= !0x8000; // channel disables itself
            self.io[0xBB + i * 12] = (self.dma[i].control >> 8) as u8;
        }

        if ch.irq_enabled() {
            self.raise_irq(irq::DMA0 << i);
        }
    }

    fn dma_write_control_hi(&mut self, i: usize, value: u8) {
        let was_enabled = self.dma[i].enabled();
        self.dma[i].control = (self.dma[i].control & 0x00FF) | ((value as u16) << 8);
        if !was_enabled && self.dma[i].enabled() {
            // Latch internal registers on enable.
            self.dma[i].cur_src = self.dma[i].src;
            self.dma[i].cur_dst = self.dma[i].dst;
            if self.dma[i].timing() == 0 {
                self.run_dma(i);
            }
        }
    }

    /// Route an I/O-register byte write to the owning peripheral.
    fn io_write8(&mut self, off: u32, value: u8) {
        match 0x0400_0000 + off {
            REG_IE => self.reg_ie = (self.reg_ie & 0xFF00) | value as u16,
            x if x == REG_IE + 1 => {
                self.reg_ie = (self.reg_ie & 0x00FF) | ((value as u16) << 8)
            }
            // IF: write-1-to-clear.
            REG_IF => self.reg_if &= !(value as u16),
            x if x == REG_IF + 1 => self.reg_if &= !((value as u16) << 8),
            REG_IME => self.ime = value & 1 != 0,
            HALTCNT => self.halted = true,
            // Timers: reload low/high, control.
            x if (0x0400_0100..0x0400_0110).contains(&x) => {
                self.io[off as usize] = value;
                let i = ((x - 0x0400_0100) / 4) as usize;
                match (x - 0x0400_0100) % 4 {
                    0 => self.timers[i].reload = (self.timers[i].reload & 0xFF00) | value as u16,
                    1 => {
                        self.timers[i].reload =
                            (self.timers[i].reload & 0x00FF) | ((value as u16) << 8)
                    }
                    2 => self.timer_write_control(i, value),
                    _ => {}
                }
            }
            // DMA registers. The raw store happens first: an immediate
            // transfer may clear the enable bit again from run_dma.
            x if (0x0400_00B0..0x0400_00E0).contains(&x) => {
                self.io[off as usize] = value;
                let rel = (x - 0x0400_00B0) as usize;
                let i = rel / 12;
                let field = rel % 12;
                match field {
                    0..=3 => {
                        let shift = field * 8;
                        self.dma[i].src =
                            (self.dma[i].src & !(0xFF << shift)) | ((value as u32) << shift);
                    }
                    4..=7 => {
                        let shift = (field - 4) * 8;
                        self.dma[i].dst =
                            (self.dma[i].dst & !(0xFF << shift)) | ((value as u32) << shift);
                    }
                    8 => self.dma[i].count = (self.dma[i].count & 0xFF00) | value as u16,
                    9 => self.dma[i].count = (self.dma[i].count & 0x00FF) | ((value as u16) << 8),
                    10 => self.dma[i].control = (self.dma[i].control & 0xFF00) | value as u16,
                    11 => self.dma_write_control_hi(i, value),
                    _ => unreachable!(),
                }
            }
            _ => self.io[off as usize] = value,
        }
    }

    fn io_read8(&mut self, off: u32) -> u8 {
        match 0x0400_0000 + off {
            DISPSTAT => self.dispstat_low(),
            VCOUNT => ((self.clock / CYCLES_PER_SCANLINE) % SCANLINES_PER_FRAME) as u8,
            KEYINPUT => self.keys as u8,
            x if x == KEYINPUT + 1 => (self.keys >> 8) as u8,
            REG_IE => self.reg_ie as u8,
            x if x == REG_IE + 1 => (self.reg_ie >> 8) as u8,
            REG_IF => self.reg_if as u8,
            x if x == REG_IF + 1 => (self.reg_if >> 8) as u8,
            REG_IME => self.ime as u8,
            x if (0x0400_0100..0x0400_0110).contains(&x) && (x % 4 < 2) => {
                let i = ((x - 0x0400_0100) / 4) as usize;
                let count = self.timer_read_count(i);
                if x % 4 == 0 {
                    count as u8
                } else {
                    (count >> 8) as u8
                }
            }
            _ => self.io[off as usize],
        }
    }

    /// DISPSTAT bits 0-2 derived from the clock; bits 3+ from the backing
    /// store (IRQ enables, VCount setting).
    fn dispstat_low(&self) -> u8 {
        let line = (self.clock / CYCLES_PER_SCANLINE) % SCANLINES_PER_FRAME;
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
    /// 16-bit pattern (hardware reference: cartridge bus with nothing
    /// driving it).
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
            0x04 => {
                let off = addr & 0x00FF_FFFF;
                if off < 0x400 {
                    self.io_read8(off)
                } else {
                    0
                }
            }
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
                    self.io_write8(off, value);
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

    fn hle_halt(&mut self) {
        self.halted = true;
    }

    fn hle_intr_wait(&mut self, discard: bool, mask: u16) -> bool {
        if discard {
            let flags = self.read16(0x0300_7FF8);
            self.write16(0x0300_7FF8, flags & !mask);
        }
        // The BIOS routine forces IME on.
        self.ime = true;
        self.intr_wait = Some(mask);
        true
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
        assert_eq!(mem.read16(0x0800_0010), 0x0008);
        assert_eq!(mem.read16(0x0800_0012), 0x0009);
    }

    #[test]
    fn keyinput_defaults_released() {
        let mut mem = MemMap::new(vec![]);
        assert_eq!(mem.read16(0x0400_0130), 0x03FF);
    }

    #[test]
    fn if_write_one_to_clear() {
        let mut mem = MemMap::new(vec![]);
        mem.raise_irq(irq::VBLANK | irq::TIMER0);
        mem.write16(REG_IF, irq::VBLANK);
        assert_eq!(mem.reg_if, irq::TIMER0);
    }

    #[test]
    fn timer_overflow_raises_irq() {
        let mut mem = MemMap::new(vec![]);
        // Timer 0: reload 0xFFF0, prescaler 1, IRQ enabled, start.
        mem.write16(0x0400_0100, 0xFFF0);
        mem.write8(0x0400_0102, 0xC0);
        mem.tick(15);
        assert_eq!(mem.reg_if & irq::TIMER0, 0);
        mem.tick(2);
        assert_ne!(mem.reg_if & irq::TIMER0, 0);
        // Counter wrapped to reload and keeps counting.
        assert!(mem.read16(0x0400_0100) >= 0xFFF0);
    }

    #[test]
    fn timer_cascade() {
        let mut mem = MemMap::new(vec![]);
        // Timer 0 overflows every 16 cycles; timer 1 cascades with IRQ.
        mem.write16(0x0400_0100, 0xFFF0);
        mem.write16(0x0400_0104, 0xFFFF); // timer 1: overflow on first tick
        mem.write8(0x0400_0106, 0xC4); // enable + cascade + irq
        mem.write8(0x0400_0102, 0x80); // enable timer 0
        mem.tick(20);
        assert_ne!(mem.reg_if & (irq::TIMER0 << 1), 0);
    }

    #[test]
    fn immediate_dma_copies() {
        let mut mem = MemMap::new(vec![]);
        mem.write32(0x0200_0000, 0x1122_3344);
        mem.write32(0x0200_0004, 0x5566_7788);
        // DMA3: src EWRAM, dst IWRAM, 2 words, immediate.
        mem.write32(0x0400_00D4, 0x0200_0000);
        mem.write32(0x0400_00D8, 0x0300_0000);
        mem.write16(0x0400_00DC, 2);
        mem.write16(0x0400_00DE, 0x8400); // enable | 32-bit
        assert_eq!(mem.read32(0x0300_0000), 0x1122_3344);
        assert_eq!(mem.read32(0x0300_0004), 0x5566_7788);
        // Channel disabled itself.
        assert_eq!(mem.read8(0x0400_00DF) & 0x80, 0);
    }

    #[test]
    fn vblank_event_sets_flag_and_irq() {
        let mut mem = MemMap::new(vec![]);
        mem.write8(0x0400_0004, 0x08); // DISPSTAT: vblank IRQ enable
        mem.tick(CYCLES_PER_SCANLINE * VISIBLE_SCANLINES + 1);
        assert!(mem.frame_ready);
        assert_ne!(mem.reg_if & irq::VBLANK, 0);
        assert_eq!(mem.read8(0x0400_0004) & 1, 1); // vblank flag visible
    }
}
