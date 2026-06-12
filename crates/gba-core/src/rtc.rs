//! Cartridge GPIO port and the S-3511A Real-Time Clock.
//!
//! Some cartridges wire a 4-bit general-purpose I/O port into the ROM bus at
//! 0x080000C4..0x080000C9 (hardware reference, "GBA Cart I/O Port (GPIO)"):
//!
//!   | 0x080000C4 | Data      | bits 0-3 = pin levels                       |
//!   | 0x080000C6 | Direction | bits 0-3: 1 = GBA drives the pin (output)  |
//!   | 0x080000C8 | Control   | bit0: 1 = port is readable (else reads ROM)|
//!
//! The only GPIO device we model is the S-3511A clock, the chip behind the
//! battery-backed "system clock" in calendar/day-night games. Its three
//! pins map onto the data port:
//!
//!   bit0 = SCK (serial clock)   bit1 = SIO (bidirectional data)   bit2 = CS
//!
//! A transaction runs while CS is high: the host clocks one command byte in,
//! then clocks the register's data bytes in (write) or out (read), MSB- or
//! LSB-first as auto-detected from the command's fixed `0110` nibble.
//!
//! Command byte: `0110` (bits 7-4) | register (bits 3-1) | R/W (bit0, 1=read).
//! Registers used by clock games:
//!   0 = reset   1 = control/status (1B)   2 = date+time (7B)   3 = time (3B)
//!
//! Date+time is BCD: year(00-99=2000-2099), month, day, day-of-week(0-6),
//! hour, minute, second. On the GBA the hour's bit7 is the PM flag and
//! control bit6 selects 24-hour mode.
//!
//! We don't keep a battery: reads are answered from the *host* clock
//! ([`crate::hostclock`]). A game that *sets* the clock is honoured for the
//! session via a seconds offset, so the in-game clock screen behaves while
//! real time keeps advancing. Detection is by the Seiko SDK library
//! signature left in the ROM, so cartridges without the chip are untouched
//! (their 0xC4 region holds ordinary ROM code).

use crate::hostclock::{self, Civil};

/// Seiko RTC library signature ("Seiko Instruments Inc. RTC, V###") embedded
/// in the ROM by the SDK. A reliable, revision-independent presence test.
const SIGNATURE: &[u8] = b"SIIRTC_V";

/// True if the ROM links the Seiko RTC library — i.e. has the clock chip.
/// `RECOMP_RTC_OFF` forces this off (escape hatch / A-B diagnostics).
pub fn detect(rom: &[u8]) -> bool {
    if std::env::var_os("RECOMP_RTC_OFF").is_some() {
        return false;
    }
    rom.windows(SIGNATURE.len()).any(|w| w == SIGNATURE)
}

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Idle,
    Command,
    Read,
    Write,
}

pub struct Rtc {
    // --- GPIO port registers (low nibble significant) ---
    /// Last value written to the data port (read back on output pins).
    data: u8,
    /// Direction: bit set = GBA drives that pin.
    direction: u8,
    /// Control bit0: port readable.
    read_enable: bool,

    // --- serial line state ---
    sck: bool,
    cs: bool,
    /// Level the chip drives onto SIO (seen on input pins during a read).
    sio_out: bool,

    // --- transaction state machine ---
    phase: Phase,
    /// Command-byte accumulator (first bit received lands in bit7).
    cmd_acc: u8,
    /// Bits consumed in the current byte.
    nbits: u8,
    /// Index of the current data byte within `buffer`.
    byte_idx: usize,
    /// Number of data bytes for the selected register.
    buflen: usize,
    /// Data bytes being shifted out (read) or in (write).
    buffer: [u8; 7],
    reg: u8,
    lsb_first: bool,

    /// Status/control register (bit6 = 24-hour mode, bit7 = power-lost).
    control: u8,
    /// Seconds added to host time, set when the game writes the clock.
    offset: i64,

    /// Deterministic override (`RECOMP_RTC_EPOCH`): a fixed point on the
    /// `hostclock` linear timeline used instead of the live host clock, so
    /// differential `verify`/sweep runs are reproducible. Unset in play.
    fixed: Option<i64>,
    trace: bool,
}

impl Rtc {
    pub fn new() -> Rtc {
        Rtc {
            data: 0,
            direction: 0,
            read_enable: false,
            sck: false,
            cs: false,
            sio_out: false,
            phase: Phase::Idle,
            cmd_acc: 0,
            nbits: 0,
            byte_idx: 0,
            buflen: 0,
            buffer: [0; 7],
            reg: 0,
            lsb_first: false,
            // Power OK (bit7=0): trust the host clock immediately, no
            // "set the time" prompt. 24-hour mode left for the game to set.
            control: 0,
            offset: 0,
            fixed: std::env::var("RECOMP_RTC_EPOCH")
                .ok()
                .and_then(|s| s.trim().parse().ok()),
            trace: std::env::var_os("RECOMP_TRACE_RTC").is_some(),
        }
    }

    /// Byte read from a GPIO register (offset already masked to 0xC4..0xC9).
    /// When the port is in write-only mode the bus returns ROM data, which is
    /// zero across this header region for every clock cartridge.
    pub fn read(&self, off: u32) -> u8 {
        if !self.read_enable {
            return 0;
        }
        match off {
            0xC4 => self.read_data(),
            0xC6 => self.direction,
            0xC8 => 1,
            _ => 0, // high bytes of each halfword register read as 0
        }
    }

    /// Byte write to a GPIO register (offset masked to 0xC4..0xC9).
    pub fn write(&mut self, off: u32, value: u8) {
        match off {
            0xC4 => self.write_data(value),
            0xC6 => self.direction = value & 0x0F,
            0xC8 => self.read_enable = value & 1 != 0,
            _ => {}
        }
    }

    /// Compose the data-port read: output pins read back the written level,
    /// input pins read the device. Only SIO (bit1) is device-driven.
    fn read_data(&self) -> u8 {
        let mut v = 0u8;
        for bit in 0..4u8 {
            let level = if self.direction & (1 << bit) != 0 {
                (self.data >> bit) & 1
            } else if bit == 1 {
                self.sio_out as u8
            } else {
                0
            };
            v |= level << bit;
        }
        v
    }

    fn write_data(&mut self, value: u8) {
        self.data = value & 0x0F;
        let new_sck = self.data & 0b001 != 0;
        let new_cs = self.data & 0b100 != 0;
        let sio_in = self.data & 0b010 != 0;

        if !self.cs && new_cs {
            // CS rising edge: begin a transaction.
            self.phase = Phase::Command;
            self.cmd_acc = 0;
            self.nbits = 0;
        } else if self.cs && !new_cs {
            self.phase = Phase::Idle;
        }

        // Bits move on the rising edge of SCK while selected.
        if new_cs && !self.sck && new_sck {
            self.clock_bit(sio_in);
        }

        self.sck = new_sck;
        self.cs = new_cs;
    }

    fn clock_bit(&mut self, sio_in: bool) {
        match self.phase {
            Phase::Command => {
                self.cmd_acc = (self.cmd_acc << 1) | sio_in as u8;
                self.nbits += 1;
                if self.nbits == 8 {
                    self.decode_command();
                }
            }
            Phase::Write => {
                if self.byte_idx < self.buflen {
                    let pos = self.bit_pos();
                    if sio_in {
                        self.buffer[self.byte_idx] |= 1 << pos;
                    } else {
                        self.buffer[self.byte_idx] &= !(1 << pos);
                    }
                    self.advance_byte(true);
                }
            }
            Phase::Read => {
                if self.byte_idx < self.buflen {
                    let pos = self.bit_pos();
                    self.sio_out = (self.buffer[self.byte_idx] >> pos) & 1 != 0;
                    self.advance_byte(false);
                }
            }
            Phase::Idle => {}
        }
    }

    /// Bit position within the current data byte for this clock, honouring
    /// the auto-detected transfer order.
    fn bit_pos(&self) -> u8 {
        if self.lsb_first {
            self.nbits
        } else {
            7 - self.nbits
        }
    }

    fn advance_byte(&mut self, committing: bool) {
        self.nbits += 1;
        if self.nbits == 8 {
            self.nbits = 0;
            self.byte_idx += 1;
            if committing && self.byte_idx == self.buflen {
                self.commit_write();
            }
        }
    }

    fn decode_command(&mut self) {
        // The fixed `0110` nibble tells us the bit order: as received it sits
        // in the high nibble for MSB-first transfers, in the (reversed) high
        // nibble for LSB-first ones.
        let (cmd, lsb) = if self.cmd_acc >> 4 == 0b0110 {
            (self.cmd_acc, false)
        } else {
            (self.cmd_acc.reverse_bits(), true)
        };
        self.lsb_first = lsb;
        self.reg = (cmd >> 1) & 0x07;
        let read = cmd & 1 != 0;
        self.nbits = 0;
        self.byte_idx = 0;
        self.buffer = [0; 7];
        self.buflen = match self.reg {
            1 => 1, // control / status
            2 => 7, // date + time
            3 => 3, // time
            _ => 0, // reset (0) and unmodelled registers
        };

        if self.reg == 0 {
            self.reset_clock();
        }
        if read {
            match self.reg {
                1 => self.buffer[0] = self.control,
                2 => self.load_datetime(),
                3 => self.load_time(),
                _ => {}
            }
            self.phase = Phase::Read;
        } else {
            self.phase = Phase::Write;
        }

        if self.trace {
            eprintln!(
                "rtc: cmd reg={} {} len={} {}",
                self.reg,
                if read { "read" } else { "write" },
                self.buflen,
                if lsb { "lsb" } else { "msb" }
            );
        }
    }

    /// Snapshot host time (with the session offset) into the 7-byte buffer.
    fn load_datetime(&mut self) {
        let c = self.now();
        let h24 = self.control & 0x40 != 0;
        self.buffer[0] = bcd((c.year.rem_euclid(100)) as u8);
        self.buffer[1] = bcd(c.month);
        self.buffer[2] = bcd(c.day);
        self.buffer[3] = c.dow & 0x07;
        self.buffer[4] = encode_hour(c.hour, h24);
        self.buffer[5] = bcd(c.min);
        self.buffer[6] = bcd(c.sec);
        if self.trace {
            eprintln!(
                "rtc: report {:04}-{:02}-{:02} {:02}:{:02}:{:02} (dow {})",
                c.year, c.month, c.day, c.hour, c.min, c.sec, c.dow
            );
        }
    }

    fn load_time(&mut self) {
        let c = self.now();
        let h24 = self.control & 0x40 != 0;
        self.buffer[0] = encode_hour(c.hour, h24);
        self.buffer[1] = bcd(c.min);
        self.buffer[2] = bcd(c.sec);
    }

    fn commit_write(&mut self) {
        match self.reg {
            1 => self.control = self.buffer[0],
            2 => {
                let h24 = self.control & 0x40 != 0;
                let set = Civil {
                    year: 2000 + unbcd(self.buffer[0]) as i32,
                    month: unbcd(self.buffer[1]).clamp(1, 12),
                    day: unbcd(self.buffer[2]).clamp(1, 31),
                    dow: self.buffer[3] & 0x07,
                    hour: decode_hour(self.buffer[4], h24),
                    min: unbcd(self.buffer[5]).min(59),
                    sec: unbcd(self.buffer[6]).min(59),
                };
                self.set_offset_from(set);
            }
            3 => {
                // Only h/m/s given: keep the current date, replace the time.
                let h24 = self.control & 0x40 != 0;
                let mut set = self.now();
                set.hour = decode_hour(self.buffer[0], h24);
                set.min = unbcd(self.buffer[1]).min(59);
                set.sec = unbcd(self.buffer[2]).min(59);
                self.set_offset_from(set);
            }
            _ => {}
        }
    }

    /// The clock base: the deterministic override if set, else host time.
    fn base_now(&self) -> Civil {
        match self.fixed {
            Some(s) => hostclock::from_linear(s),
            None => hostclock::now_local(),
        }
    }

    /// Clock base plus the session offset.
    fn now(&self) -> Civil {
        let base = self.base_now();
        if self.offset == 0 {
            base
        } else {
            hostclock::from_linear(hostclock::to_linear(&base) + self.offset)
        }
    }

    /// Make `now()` report `target` from this moment on.
    fn set_offset_from(&mut self, target: Civil) {
        let host = self.base_now();
        self.offset = hostclock::to_linear(&target) - hostclock::to_linear(&host);
        if self.trace {
            eprintln!(
                "rtc: clock set to {:04}-{:02}-{:02} {:02}:{:02}:{:02} (offset {}s)",
                target.year,
                target.month,
                target.day,
                target.hour,
                target.min,
                target.sec,
                self.offset
            );
        }
    }

    fn reset_clock(&mut self) {
        self.control = 0;
        self.offset = 0;
    }
}

impl Default for Rtc {
    fn default() -> Self {
        Rtc::new()
    }
}

fn bcd(v: u8) -> u8 {
    ((v / 10) << 4) | (v % 10)
}

fn unbcd(b: u8) -> u8 {
    (b >> 4) * 10 + (b & 0x0F)
}

/// Encode an hour (0-23) into the RTC hour byte: BCD low bits plus the PM
/// flag in bit7. In 12-hour mode the low bits are 1-12.
fn encode_hour(hour: u8, h24: bool) -> u8 {
    let pm = (hour >= 12) as u8;
    let h = if h24 {
        hour
    } else {
        let h = hour % 12;
        if h == 0 {
            12
        } else {
            h
        }
    };
    bcd(h) | (pm << 7)
}

/// Inverse of [`encode_hour`], yielding a 0-23 hour.
fn decode_hour(byte: u8, h24: bool) -> u8 {
    let pm = byte & 0x80 != 0;
    let h = unbcd(byte & 0x3F);
    if h24 {
        h.min(23)
    } else {
        let h = h % 12;
        if pm {
            h + 12
        } else {
            h
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_signature() {
        let mut rom = vec![0u8; 4096];
        rom[2048..2048 + SIGNATURE.len()].copy_from_slice(SIGNATURE);
        assert!(detect(&rom));
        assert!(!detect(&vec![0u8; 4096]));
    }

    #[test]
    fn bcd_roundtrips() {
        for v in 0..=99u8 {
            assert_eq!(unbcd(bcd(v)), v);
        }
    }

    #[test]
    fn hour_pm_flag() {
        // 24-hour mode: 13:00 -> BCD 0x13 with PM bit.
        assert_eq!(encode_hour(13, true), 0x13 | 0x80);
        assert_eq!(decode_hour(0x13 | 0x80, true), 13);
        // 12-hour mode: 13:00 -> 1 PM.
        assert_eq!(encode_hour(13, false), 0x01 | 0x80);
        assert_eq!(decode_hour(0x01 | 0x80, false), 13);
        // Midnight in 12-hour mode reads back as hour 12 AM.
        assert_eq!(encode_hour(0, false), 0x12);
    }

    /// Drive the serial pins the way the SDK library does and read the date
    /// back out, MSB-first. Proves the command decode, bit clocking and BCD
    /// packing line up end to end.
    #[test]
    fn datetime_read_over_serial() {
        const SCK: u8 = 0b001;
        const SIO: u8 = 0b010;
        const CS: u8 = 0b100;

        let mut rtc = Rtc::new();
        // Enable readback, set direction: SCK, SIO, CS as outputs initially.
        rtc.write(0xC8, 1);
        rtc.write(0xC6, 0b0111);
        // Raise CS to open a transaction.
        rtc.write(0xC4, CS);

        // Clock in the datetime-read command 0x65 (0110_0101), MSB-first.
        let send_bit = |rtc: &mut Rtc, bit: u8| {
            let s = if bit != 0 { SIO } else { 0 };
            rtc.write(0xC4, CS | s); // SCK low
            rtc.write(0xC4, CS | s | SCK); // SCK high -> sampled
        };
        for i in (0..8).rev() {
            send_bit(&mut rtc, (0x65 >> i) & 1);
        }

        // Switch SIO to input and clock out 7 bytes.
        rtc.write(0xC6, 0b0101);
        let mut bytes = [0u8; 7];
        for byte in bytes.iter_mut() {
            let mut v = 0u8;
            for _ in 0..8 {
                rtc.write(0xC4, CS); // SCK low
                rtc.write(0xC4, CS | SCK); // SCK high -> chip presents bit
                let sio = (rtc.read(0xC4) >> 1) & 1;
                v = (v << 1) | sio; // MSB-first
            }
            *byte = v;
        }
        rtc.write(0xC4, 0); // drop CS

        let now = hostclock::now_local();
        assert_eq!(bytes[0], bcd((now.year.rem_euclid(100)) as u8));
        assert_eq!(bytes[1], bcd(now.month));
        assert_eq!(bytes[2], bcd(now.day));
        assert_eq!(unbcd(bytes[5]), now.min);
    }

    /// Writing the clock, then reading it back, returns what was set.
    #[test]
    fn set_then_read_clock() {
        const SCK: u8 = 0b001;
        const SIO: u8 = 0b010;
        const CS: u8 = 0b100;
        let mut rtc = Rtc::new();
        rtc.write(0xC8, 1);
        rtc.write(0xC6, 0b0111);
        rtc.write(0xC4, CS);

        let send_byte = |rtc: &mut Rtc, val: u8| {
            for i in (0..8).rev() {
                let s = if (val >> i) & 1 != 0 { SIO } else { 0 };
                rtc.write(0xC4, CS | s);
                rtc.write(0xC4, CS | s | SCK);
            }
        };
        // Command 0x64 = datetime write, then 7 BCD bytes for 2030-12-25
        // 08:09:10, Wednesday(3), 24-hour mode already implied by hour byte.
        send_byte(&mut rtc, 0x64);
        let want = [bcd(30), bcd(12), bcd(25), 3, bcd(8), bcd(9), bcd(10)];
        for b in want {
            send_byte(&mut rtc, b);
        }
        rtc.write(0xC4, 0);

        // It tracks real time from the set point; the date must read back
        // identically (seconds may have advanced, so don't check those).
        assert_eq!(rtc.now().year, 2030);
        assert_eq!(rtc.now().month, 12);
        assert_eq!(rtc.now().day, 25);
        assert_eq!(rtc.now().hour, 8);
    }
}
