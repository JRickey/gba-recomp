//! PSG channels 1-4 (hardware reference, "Sound Channel 1-4").
//!
//! State advances lazily at the audio sample grid (65536 Hz), which is
//! comfortably finer than envelope (64 Hz), sweep (128 Hz), and length
//! (256 Hz) timers, and fine enough for pitch (square top frequency
//! ~131 kHz folds; audible range reproduces correctly).
//!
//! Register map (offsets into I/O space):
//!   ch1: 0x60 sweep, 0x62 duty/len/env, 0x64 freq/ctl
//!   ch2:             0x68 duty/len/env, 0x6C freq/ctl
//!   ch3: 0x70 enable, 0x72 len/vol, 0x74 freq/ctl, wave RAM 0x90-0x9F
//!   ch4:             0x78 len/env,  0x7C poly/ctl
//!   master: 0x80 SOUNDCNT_L, 0x82 SOUNDCNT_H (PSG volume), 0x84 X.

const DUTY: [[i16; 8]; 4] = [
    [-8, -8, -8, -8, -8, -8, -8, 8],
    [8, -8, -8, -8, -8, -8, -8, 8],
    [8, -8, -8, -8, -8, 8, 8, 8],
    [-8, 8, 8, 8, 8, 8, 8, -8],
];

#[derive(Default, Clone, Copy)]
struct Envelope {
    volume: u8,
    increase: bool,
    period: u8,
    timer: u32,
}

impl Envelope {
    fn load(&mut self, reg: u8) {
        self.volume = reg >> 4;
        self.increase = reg & 0x08 != 0;
        self.period = reg & 7;
        self.timer = 0;
    }
    /// Advance by `frames` 64ths of a second.
    fn step(&mut self, frames: u32) {
        if self.period == 0 {
            return;
        }
        self.timer += frames;
        while self.timer >= self.period as u32 {
            self.timer -= self.period as u32;
            if self.increase && self.volume < 15 {
                self.volume += 1;
            } else if !self.increase && self.volume > 0 {
                self.volume -= 1;
            }
        }
    }
}

#[derive(Default, Clone, Copy)]
struct Square {
    on: bool,
    duty: usize,
    phase: usize,
    /// Fractional phase accumulator in source cycles.
    acc: u32,
    freq: u32,
    env: Envelope,
    length: u32,
    use_length: bool,
    // Channel 1 sweep.
    sweep_shift: u8,
    sweep_decrease: bool,
    sweep_period: u8,
    sweep_timer: u32,
}

impl Square {
    /// Cycles (16.78 MHz) per duty step: period/8 of 131072/(2048-f) Hz.
    fn cycles_per_step(&self) -> u32 {
        16 * (2048 - self.freq.min(2047))
    }

    fn sample(&mut self, cycles: u32, frames64: u32) -> i16 {
        if !self.on {
            return 0;
        }
        if self.use_length {
            if self.length == 0 {
                self.on = false;
                return 0;
            }
            // length ticks at 256 Hz = 4 per 64 Hz frame
            self.length = self.length.saturating_sub(frames64 * 4);
        }
        self.env.step(frames64);
        if self.sweep_period > 0 && self.sweep_shift > 0 {
            // Sweep clock is 128 Hz: 2 ticks per 64 Hz frame, and the
            // period is in 128 Hz units.
            self.sweep_timer += frames64 * 2;
            while self.sweep_timer >= self.sweep_period as u32 {
                self.sweep_timer -= self.sweep_period as u32;
                let d = self.freq >> self.sweep_shift;
                self.freq = if self.sweep_decrease {
                    self.freq.saturating_sub(d)
                } else {
                    (self.freq + d).min(2047)
                };
                if self.freq >= 2047 {
                    self.on = false;
                }
            }
        }
        // Area-average the duty waveform over the sample window instead
        // of point-sampling it: edges land with sub-sample precision, so
        // a high square stays clean harmonics instead of gaining the
        // edge-jitter sidebands point sampling produces ("fizzy" PSG).
        let per = self.cycles_per_step();
        let mut area: i32 = 0;
        let mut rem = cycles;
        while rem > 0 {
            let take = per.saturating_sub(self.acc).min(rem);
            area += DUTY[self.duty][self.phase] as i32 * take as i32;
            self.acc += take;
            rem -= take;
            if self.acc >= per {
                self.acc = 0;
                self.phase = (self.phase + 1) & 7;
            }
        }
        (area * self.env.volume as i32 / cycles as i32) as i16
    }
}

#[derive(Default)]
pub struct Apu {
    sq: [Square; 2],
    // Wave channel.
    wave_on: bool,
    wave_pos: usize,
    wave_acc: u32,
    wave_freq: u32,
    wave_shift: u8, // volume shift (0 = mute)
    /// Bit 5 of SOUND3CNT_L: 64-sample mode (both banks back-to-back).
    wave_dim64: bool,
    // Noise channel.
    noise_on: bool,
    noise_lfsr: u16,
    noise_acc: u32,
    noise_env: Envelope,
    noise_div: u32,
    noise_width7: bool,
    /// 64 Hz frame-sequencer accumulator (cycles).
    frame_acc: u32,
}

impl Apu {
    /// Handle a PSG register byte write.
    pub fn write8(&mut self, io: &[u8], off: usize, value: u8) {
        match off {
            0x60 => {
                self.sq[0].sweep_shift = value & 7;
                self.sq[0].sweep_decrease = value & 8 != 0;
                self.sq[0].sweep_period = (value >> 4) & 7;
            }
            0x62 | 0x68 => {
                let i = (off == 0x68) as usize;
                self.sq[i].duty = ((value >> 6) & 3) as usize;
                self.sq[i].length = 64 - (value & 63) as u32;
            }
            0x63 | 0x69 => {
                let i = (off == 0x69) as usize;
                self.sq[i].env.load(value);
                if self.sq[i].env.volume == 0 && !self.sq[i].env.increase {
                    self.sq[i].on = false; // DAC off
                }
            }
            0x64 | 0x6C => {
                let i = (off == 0x6C) as usize;
                self.sq[i].freq = (self.sq[i].freq & 0x700) | value as u32;
            }
            0x65 | 0x6D => {
                let i = (off == 0x6D) as usize;
                self.sq[i].freq = (self.sq[i].freq & 0xFF) | (((value & 7) as u32) << 8);
                self.sq[i].use_length = value & 0x40 != 0;
                if value & 0x80 != 0 {
                    // Trigger.
                    self.sq[i].on = true;
                    self.sq[i].phase = 0;
                    self.sq[i].env.load(io[0x63 + i * 6]);
                    if self.sq[i].length == 0 {
                        self.sq[i].length = 64;
                    }
                    if self.sq[i].env.volume == 0 && !self.sq[i].env.increase {
                        self.sq[i].on = false;
                    }
                }
            }
            0x70 => {
                // Bit 6 (bank select) is handled bus-side: the visible
                // window and the playing-bank shadow swap on a flip, so
                // `wave_playing` always holds the playing bank's bytes.
                self.wave_dim64 = value & 0x20 != 0;
                if value & 0x80 == 0 {
                    self.wave_on = false;
                }
            }
            0x73 => {
                self.wave_shift = match (value >> 5) & 3 {
                    0 => 0,
                    1 => 1,
                    2 => 2,
                    _ => 3,
                };
                if value & 0x80 != 0 {
                    self.wave_shift = 4; // 75% mode marker
                }
            }
            0x74 => self.wave_freq = (self.wave_freq & 0x700) | value as u32,
            0x75 => {
                self.wave_freq = (self.wave_freq & 0xFF) | (((value & 7) as u32) << 8);
                if value & 0x80 != 0 && io[0x70] & 0x80 != 0 {
                    self.wave_on = true;
                    self.wave_pos = 0;
                }
            }
            0x79 => self.noise_env.load(value),
            0x7C => {
                self.noise_width7 = value & 8 != 0;
                let r = (value & 7) as u32;
                let s = (value >> 4) as u32;
                let divisor = if r == 0 { 8 } else { 16 * r };
                self.noise_div = (divisor << s).max(8) * 4; // in 16.78MHz cycles
            }
            0x7D => {
                if value & 0x80 != 0 {
                    self.noise_on = true;
                    self.noise_lfsr = 0x7FFF;
                    self.noise_env.load(io[0x79]);
                    if self.noise_env.volume == 0 && !self.noise_env.increase {
                        self.noise_on = false;
                    }
                }
            }
            _ => {}
        }
    }

    /// Produce one (left, right) PSG sample pair for `cycles` elapsed
    /// master-clock cycles, honoring SOUNDCNT_L per-channel L/R enables
    /// and master volumes, and the SOUNDCNT_H PSG ratio. `wave_playing`
    /// is the playing wave-RAM bank (the one NOT visible at 0x90; the
    /// bus keeps it in a shadow); the visible window in `io` supplies
    /// the second half in 64-sample mode.
    pub fn sample(&mut self, io: &[u8], wave_playing: &[u8; 16], cycles: u32) -> (i16, i16) {
        // 64 Hz frame count since last sample (almost always 0 or 1).
        self.frame_acc += cycles;
        let frames64 = self.frame_acc / 262144;
        self.frame_acc %= 262144;

        // Per-channel outputs (state always advances, even when a
        // channel is routed to neither side).
        let mut ch: [i32; 4] = [0; 4];
        ch[0] = self.sq[0].sample(cycles, frames64) as i32;
        ch[1] = self.sq[1].sample(cycles, frames64) as i32;

        if self.wave_on && self.wave_shift != 0 {
            // 2097152/(2048-f) Hz, 32 samples per period; area-averaged
            // over the window like the squares. Samples 0-31 come from
            // the playing bank; in 64-sample mode 32-63 continue into
            // the other (window-visible) bank.
            let per = (8 * (2048 - self.wave_freq.min(2047))).max(8);
            let span = if self.wave_dim64 { 64 } else { 32 };
            let mut area: i32 = 0;
            let mut rem = cycles;
            while rem > 0 {
                let take = per.saturating_sub(self.wave_acc).min(rem);
                let pos = self.wave_pos % span;
                let byte = if pos < 32 {
                    wave_playing[pos / 2]
                } else {
                    io[0x90 + pos / 2 - 16]
                };
                let nib = if pos & 1 == 0 { byte >> 4 } else { byte & 0xF } as i32;
                area += (nib - 8) * 16 * take as i32;
                self.wave_acc += take;
                rem -= take;
                if self.wave_acc >= per {
                    self.wave_acc = 0;
                    self.wave_pos = (pos + 1) % span;
                }
            }
            let centered = area / cycles as i32;
            ch[2] = match self.wave_shift {
                1 => centered,
                2 => centered / 2,
                3 => centered / 4,
                _ => centered * 3 / 4,
            };
        }

        if self.noise_on {
            self.noise_env.step(frames64);
            // Area-averaging doubles as the band-limiter for high LFSR
            // clock rates: above-Nyquist noise comes out properly
            // low-passed instead of aliasing down.
            let per = self.noise_div.max(8);
            let mut area: i32 = 0;
            let mut rem = cycles;
            while rem > 0 {
                let take = per.saturating_sub(self.noise_acc).min(rem);
                let lvl = if self.noise_lfsr & 1 == 0 { 8 } else { -8 };
                area += lvl * take as i32;
                self.noise_acc += take;
                rem -= take;
                if self.noise_acc >= per {
                    self.noise_acc = 0;
                    let bit = (self.noise_lfsr ^ (self.noise_lfsr >> 1)) & 1;
                    self.noise_lfsr >>= 1;
                    self.noise_lfsr |= bit << 14;
                    if self.noise_width7 {
                        self.noise_lfsr = (self.noise_lfsr & !0x40) | (bit << 6);
                    }
                }
            }
            ch[3] = area / cycles as i32 * self.noise_env.volume as i32;
        }

        // SOUNDCNT_L: per-channel enables (bits 8-11 right, 12-15 left)
        // and master volumes 0-7 (bits 0-2 right, 4-6 left).
        let cnt_l = u16::from_le_bytes([io[0x80], io[0x81]]);
        let mut right: i32 = 0;
        let mut left: i32 = 0;
        for (i, &c) in ch.iter().enumerate() {
            if cnt_l & (1 << (8 + i)) != 0 {
                right += c;
            }
            if cnt_l & (1 << (12 + i)) != 0 {
                left += c;
            }
        }
        right = right * ((cnt_l & 7) as i32 + 1) / 8;
        left = left * (((cnt_l >> 4) & 7) as i32 + 1) / 8;

        // SOUNDCNT_H PSG master volume: 25/50/100%.
        let vol_shift = match io[0x82] & 3 {
            0 => 2,
            1 => 1,
            _ => 0,
        };
        (
            ((left << 4) >> vol_shift) as i16,
            ((right << 4) >> vol_shift) as i16,
        )
    }

    /// SOUNDCNT_X bits 0-3: per-channel active flags (channel triggered
    /// and not yet expired). Length expiry lands lazily at the sample
    /// grid, which is finer than anything that polls these bits.
    pub fn active(&self) -> u8 {
        (self.sq[0].on as u8)
            | (self.sq[1].on as u8) << 1
            | (self.wave_on as u8) << 2
            | (self.noise_on as u8) << 3
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Route channel 3 alone at full volume: each output sample is
    /// (nibble - 8) * 256, so the played nibble stream reads back.
    fn wave_setup(io: &mut [u8], apu: &mut Apu, ctl70: u8) {
        io[0x80] = 0x77; // master volume 7 both sides
        io[0x81] = 0x44; // channel 3 to both sides
        io[0x82] = 0x02; // PSG ratio 100%
        io[0x70] = ctl70;
        apu.write8(io, 0x70, ctl70);
        io[0x73] = 0x20; // wave volume 100%
        apu.write8(io, 0x73, 0x20);
        io[0x74] = 0xFF;
        apu.write8(io, 0x74, 0xFF);
        // Trigger with freq 0x7FF: 8 cycles per wave step.
        io[0x75] = 0x87;
        apu.write8(io, 0x75, 0x87);
    }

    fn next_nibble(apu: &mut Apu, io: &[u8], playing: &[u8; 16]) -> u8 {
        (apu.sample(io, playing, 8).0 / 256 + 8) as u8
    }

    #[test]
    fn wave_single_bank_plays_playing_bank_only() {
        let mut io = vec![0u8; 0x400];
        let mut apu = Apu::default();
        let playing: [u8; 16] = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67, 0x89, 0xAB,
            0xCD, 0xEF,
        ];
        io[0x90..0xA0].fill(0x40); // idle bank: must never be heard
        wave_setup(&mut io, &mut apu, 0x80);
        let got: Vec<u8> = (0..33)
            .map(|_| next_nibble(&mut apu, &io, &playing))
            .collect();
        let want: Vec<u8> = (0..33)
            .map(|i| {
                let b = playing[(i % 32) / 2];
                if i % 2 == 0 {
                    b >> 4
                } else {
                    b & 0xF
                }
            })
            .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn wave_64_sample_mode_spans_both_banks() {
        let mut io = vec![0u8; 0x400];
        let mut apu = Apu::default();
        let playing: [u8; 16] = [0x12; 16];
        io[0x90..0xA0].fill(0x40);
        wave_setup(&mut io, &mut apu, 0xA0); // bit 5: 64-sample mode
        let got: Vec<u8> = (0..65)
            .map(|_| next_nibble(&mut apu, &io, &playing))
            .collect();
        let want: Vec<u8> = (0..65)
            .map(|i| match (i % 64 < 32, i % 2 == 0) {
                (true, true) => 1,
                (true, false) => 2,
                (false, true) => 4,
                (false, false) => 0,
            })
            .collect();
        assert_eq!(got, want);
    }
}
