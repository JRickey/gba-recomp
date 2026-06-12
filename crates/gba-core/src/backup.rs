//! Cartridge backup media: SRAM, Flash 64K/128K, EEPROM 512B/8K.
//!
//! The type is detected by scanning the image for the SDK library ID
//! strings every licensed game links in (`EEPROM_V`, `SRAM_V`, `FLASH_V`,
//! `FLASH512_V`, `FLASH1M_V`).
//!
//! Flash (hardware reference, "Cart Backup Flash ROM"): command sequences
//! written to 0x0E005555/0x0E002AAA — enter/exit ID mode, erase chip or
//! 4 KB sector, write byte, switch 64 KB bank (128K parts). We report a
//! Macronix ID (0xC2/0x1C for 64K, 0xC2/0x09 for 128K).
//!
//! EEPROM (hardware reference, "Cart Backup EEPROM"): a serial bitstream
//! clocked one bit per 16-bit access through the 0x0D region by DMA3.
//! Requests: `11 addr stop` = read (responds 4 dummy + 64 data bits),
//! `10 addr data64 stop` = write. The address width (6 vs 14 bits) is how
//! 512-byte and 8-KB parts differ; we infer it from the request length.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupKind {
    None,
    Sram,
    Flash64,
    Flash128,
    Eeprom,
}

/// Scan an image for the SDK backup ID strings.
pub fn detect(rom: &[u8]) -> BackupKind {
    let find = |needle: &[u8]| rom.windows(needle.len()).any(|w| w == needle);
    // Order matters: FLASH1M before FLASH (prefix), specific before generic.
    if find(b"FLASH1M_V") {
        BackupKind::Flash128
    } else if find(b"FLASH512_V") || find(b"FLASH_V") {
        BackupKind::Flash64
    } else if find(b"EEPROM_V") {
        BackupKind::Eeprom
    } else if find(b"SRAM_V") || find(b"SRAM_F_V") {
        BackupKind::Sram
    } else {
        BackupKind::None
    }
}

// ---- Flash ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlashState {
    Ready,
    Cmd1,  // saw AA @ 5555
    Cmd2,  // saw 55 @ 2AAA
    Erase, // 0x80 armed: expect AA, 55, then 0x10/0x30
    EraseCmd1,
    EraseCmd2,
    Write, // next byte write goes to the array
    Bank,  // next write to 0x0E000000 selects the bank
}

pub struct Flash {
    pub data: Vec<u8>,
    state: FlashState,
    id_mode: bool,
    bank: usize,
    two_banks: bool,
}

impl Flash {
    pub fn new(two_banks: bool) -> Flash {
        Flash {
            data: vec![0xFF; if two_banks { 0x2_0000 } else { 0x1_0000 }],
            state: FlashState::Ready,
            id_mode: false,
            bank: 0,
            two_banks,
        }
    }

    pub fn read(&self, addr: u32) -> u8 {
        let off = (addr & 0xFFFF) as usize;
        if self.id_mode {
            return match off {
                0 => 0xC2, // Macronix
                1 => {
                    if self.two_banks {
                        0x09
                    } else {
                        0x1C
                    }
                }
                _ => 0xFF,
            };
        }
        self.data[self.bank * 0x1_0000 + off]
    }

    pub fn write(&mut self, addr: u32, value: u8) {
        let off = addr & 0xFFFF;
        use FlashState::*;
        match self.state {
            Write => {
                // Flash writes only clear bits.
                let i = self.bank * 0x1_0000 + off as usize;
                self.data[i] &= value;
                self.state = Ready;
                return;
            }
            Bank => {
                if off == 0 && self.two_banks {
                    self.bank = (value & 1) as usize;
                }
                self.state = Ready;
                return;
            }
            _ => {}
        }
        match (self.state, off, value) {
            (Ready, 0x5555, 0xAA) => self.state = Cmd1,
            (Cmd1, 0x2AAA, 0x55) => self.state = Cmd2,
            (Cmd2, 0x5555, 0x90) => {
                self.id_mode = true;
                self.state = Ready;
            }
            (Cmd2, 0x5555, 0xF0) => {
                self.id_mode = false;
                self.state = Ready;
            }
            (Cmd2, 0x5555, 0x80) => self.state = Erase,
            (Cmd2, 0x5555, 0xA0) => self.state = Write,
            (Cmd2, 0x5555, 0xB0) => self.state = Bank,
            (Erase, 0x5555, 0xAA) => self.state = EraseCmd1,
            (EraseCmd1, 0x2AAA, 0x55) => self.state = EraseCmd2,
            (EraseCmd2, 0x5555, 0x10) => {
                self.data.fill(0xFF);
                self.state = Ready;
            }
            (EraseCmd2, _, 0x30) => {
                // 4 KB sector erase at the written address.
                let base = self.bank * 0x1_0000 + (off as usize & 0xF000);
                self.data[base..base + 0x1000].fill(0xFF);
                self.state = Ready;
            }
            _ => self.state = Ready,
        }
    }
}

// ---- EEPROM ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EepromMode {
    Idle,
    Reading { addr: usize, bit: u32 },
}

pub struct Eeprom {
    pub data: Vec<u8>,
    /// Bits accumulated from writes since the last completed request.
    in_bits: Vec<bool>,
    mode: EepromMode,
    /// 14-bit addressing (8 KB part)? Inferred from request length.
    wide: bool,
}

impl Eeprom {
    pub fn new() -> Eeprom {
        Eeprom {
            data: vec![0xFF; 0x2000],
            in_bits: Vec::new(),
            mode: EepromMode::Idle,
            wide: false,
        }
    }

    pub fn write_bit(&mut self, value: u16) {
        self.in_bits.push(value & 1 != 0);
        if self.in_bits.len() > 81 {
            self.in_bits.clear(); // desynced; resynchronize
        }
    }

    /// Requests arrive as a single DMA; the driver calls this at the end
    /// of the transfer. Lengths identify the shape unambiguously:
    /// read = 2 + (6|14) + 1, write = 2 + (6|14) + 64 + 1.
    pub fn flush(&mut self) {
        let n = self.in_bits.len();
        if n < 2 {
            self.in_bits.clear();
            return;
        }
        let is_read = self.in_bits[0] && self.in_bits[1];
        let is_write = self.in_bits[0] && !self.in_bits[1];
        if is_read && (n == 9 || n == 17) {
            self.wide = n == 17;
            let addr = self.parse_addr(2, n - 3);
            self.mode = EepromMode::Reading { addr, bit: 0 };
        } else if is_write && (n == 73 || n == 81) {
            self.wide = n == 81;
            let addr_bits = n - 67;
            let addr = self.parse_addr(2, addr_bits);
            for i in 0..64 {
                let bit = self.in_bits[2 + addr_bits + i];
                let byte = addr + i / 8;
                if byte < self.data.len() {
                    let mask = 0x80 >> (i % 8);
                    if bit {
                        self.data[byte] |= mask;
                    } else {
                        self.data[byte] &= !mask;
                    }
                }
            }
        }
        self.in_bits.clear();
    }

    pub fn has_pending_bits(&self) -> bool {
        !self.in_bits.is_empty()
    }

    fn parse_addr(&self, start: usize, len: usize) -> usize {
        let mut addr = 0usize;
        for i in 0..len {
            addr = (addr << 1) | self.in_bits[start + i] as usize;
        }
        // Only 10 bits are significant on the 8 KB part; unit = 8 bytes.
        (addr & 0x3FF) * 8
    }

    pub fn read_bit(&mut self) -> u16 {
        // A CPU-driven request (no DMA boundary) completes when the game
        // switches to reading.
        if self.has_pending_bits() {
            self.flush();
        }
        match self.mode {
            EepromMode::Idle => 1, // ready
            EepromMode::Reading { addr, ref mut bit } => {
                let b = *bit;
                *bit += 1;
                if b < 4 {
                    0 // dummy bits
                } else {
                    let i = (b - 4) as usize;
                    let byte = addr + i / 8;
                    let value = if byte < self.data.len() {
                        (self.data[byte] >> (7 - i % 8)) & 1
                    } else {
                        1
                    };
                    if b >= 67 {
                        self.mode = EepromMode::Idle;
                    }
                    value as u16
                }
            }
        }
    }
}

impl Default for Eeprom {
    fn default() -> Self {
        Eeprom::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_types() {
        assert_eq!(detect(b"xxFLASH1M_Vyy"), BackupKind::Flash128);
        assert_eq!(detect(b"xxFLASH_V12"), BackupKind::Flash64);
        assert_eq!(detect(b"..EEPROM_V11.."), BackupKind::Eeprom);
        assert_eq!(detect(b"..SRAM_V100"), BackupKind::Sram);
        assert_eq!(detect(b"nothing"), BackupKind::None);
    }

    #[test]
    fn flash_id_mode() {
        let mut f = Flash::new(true);
        f.write(0x0E00_5555, 0xAA);
        f.write(0x0E00_2AAA, 0x55);
        f.write(0x0E00_5555, 0x90);
        assert_eq!(f.read(0x0E00_0000), 0xC2);
        assert_eq!(f.read(0x0E00_0001), 0x09);
        f.write(0x0E00_5555, 0xAA);
        f.write(0x0E00_2AAA, 0x55);
        f.write(0x0E00_5555, 0xF0);
        assert_eq!(f.read(0x0E00_0000), 0xFF);
    }

    #[test]
    fn flash_write_and_sector_erase() {
        let mut f = Flash::new(false);
        let cmd = |f: &mut Flash, b| {
            f.write(0x0E00_5555, 0xAA);
            f.write(0x0E00_2AAA, 0x55);
            f.write(0x0E00_5555, b);
        };
        cmd(&mut f, 0xA0);
        f.write(0x0E00_1234, 0x5A);
        assert_eq!(f.read(0x0E00_1234), 0x5A);
        cmd(&mut f, 0x80);
        f.write(0x0E00_5555, 0xAA);
        f.write(0x0E00_2AAA, 0x55);
        f.write(0x0E00_1000, 0x30); // erase the 0x1000 sector
        assert_eq!(f.read(0x0E00_1234), 0xFF);
    }

    #[test]
    fn eeprom_write_then_read() {
        let mut e = Eeprom::new();
        // Write request, 14-bit address 5: 10 + addr + 64 data bits + stop.
        let mut bits: Vec<u16> = vec![1, 0];
        for i in (0..14).rev() {
            bits.push((5 >> i) & 1);
        }
        // Data: byte pattern 0xA5 repeated.
        for _ in 0..8 {
            for i in (0..8).rev() {
                bits.push((0xA5u16 >> i) & 1);
            }
        }
        bits.push(0); // stop
        for b in bits {
            e.write_bit(b);
        }
        // Idle read returns ready.
        assert_eq!(e.read_bit(), 1);

        // Read request for the same address.
        let mut bits: Vec<u16> = vec![1, 1];
        for i in (0..14).rev() {
            bits.push((5 >> i) & 1);
        }
        bits.push(0);
        for b in bits {
            e.write_bit(b);
        }
        // 4 dummy bits then data.
        for _ in 0..4 {
            assert_eq!(e.read_bit(), 0);
        }
        let mut byte = 0u8;
        for _ in 0..8 {
            byte = (byte << 1) | e.read_bit() as u8;
        }
        assert_eq!(byte, 0xA5);
    }
}
