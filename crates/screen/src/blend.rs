//! Temporal response: the panel's slow pixel transitions, which a whole
//! class of titles exploits (alternating-frame "flicker transparency",
//! half-resolution interlacing, map-jitter anti-aliasing). On a fast modern
//! display those effects strobe at 30 Hz instead of fusing — simulating
//! persistence is a *correctness* feature, not a filter.
//!
//! All state advances once per EMULATED frame, inside the dispatch loop.
//! Blending presented frames instead is the classic parity bug (frame skip
//! or compositor buffering locks onto one phase and the effect turns opaque
//! or invisible); our loop produces every frame regardless of presentation
//! pacing, so feeding it here is immune by construction.
//!
//! Mixing happens on display-encoded (gamma-space) values, matching the
//! reference emulator's interframe blend semantics.

use crate::lut::ColorLut;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ResponseMode {
    /// No temporal processing.
    Off,
    /// 50:50 blend with the previous emulated frame, everywhere. What the
    /// reference emulator ships; sufficient for flicker-transparency.
    Simple,
    /// Blend only pixels that are actually alternating at frame rate
    /// (A,B,A,B over a 6-frame window, within `tolerance`); static content
    /// stays tack sharp. Default.
    Smart,
    /// Exponential persistence: out += (new - out) * (1 - rho) per frame.
    /// The full motion-smear simulation; `rho` is the per-screen carryover.
    Persistence,
}

impl ResponseMode {
    pub const ALL: [ResponseMode; 4] = [
        ResponseMode::Off,
        ResponseMode::Simple,
        ResponseMode::Smart,
        ResponseMode::Persistence,
    ];

    pub fn name(self) -> &'static str {
        match self {
            ResponseMode::Off => "off",
            ResponseMode::Simple => "simple",
            ResponseMode::Smart => "smart",
            ResponseMode::Persistence => "persistence",
        }
    }

    pub fn by_name(name: &str) -> Option<ResponseMode> {
        Self::ALL.into_iter().find(|m| m.name() == name)
    }
}

/// How much of the previous output a Persistence-mode pixel keeps each
/// frame. Qualitative, per the historical record: the reflective family
/// transitions slowest; the backlit revision is quicker but still fuses
/// 30 Hz flicker.
pub fn default_rho(screen: crate::profile::ScreenKind) -> f32 {
    use crate::profile::ScreenKind::*;
    match screen {
        Unlit | Frontlit => 0.42,
        _ => 0.30,
    }
}

const RING: usize = 6;

pub struct Temporal {
    pub mode: ResponseMode,
    /// Per-channel 5-bit tolerance for Smart alternation matching; 0 = exact.
    /// Titles overlay flicker with scrolling, so a little slack helps.
    pub tolerance: u16,
    pub rho: f32,
    pixels: usize,
    /// Raw BGR555 history, most recent at `head`.
    ring: Vec<Vec<u16>>,
    head: usize,
    filled: usize,
    /// Persistence accumulator over display-encoded channels.
    acc: Vec<[f32; 3]>,
}

impl Temporal {
    pub fn new(pixels: usize, mode: ResponseMode, tolerance: u16, rho: f32) -> Self {
        Self {
            mode,
            tolerance,
            rho: rho.clamp(0.0, 0.9),
            pixels,
            ring: (0..RING).map(|_| vec![0u16; pixels]).collect(),
            head: 0,
            filled: 0,
            acc: vec![[0.0; 3]; pixels],
        }
    }

    /// Advance one emulated frame. Call exactly once per produced frame,
    /// with the raw BGR555 framebuffer.
    pub fn push(&mut self, frame: &[u16], lut: &ColorLut) {
        debug_assert_eq!(frame.len(), self.pixels);
        self.head = (self.head + RING - 1) % RING;
        self.ring[self.head][..self.pixels].copy_from_slice(frame);
        self.filled = (self.filled + 1).min(RING);

        if self.mode == ResponseMode::Persistence {
            let keep = if self.filled == 1 { 0.0 } else { self.rho };
            for (a, &px) in self.acc.iter_mut().zip(frame) {
                let e = lut.table[(px & 0x7FFF) as usize];
                for c in 0..3 {
                    a[c] += (e[c] as f32 - a[c]) * (1.0 - keep);
                }
            }
        }
    }

    fn frame(&self, age: usize) -> &[u16] {
        &self.ring[(self.head + age) % RING]
    }

    /// Resolve the current visible image into RGBA8.
    pub fn compose(&self, lut: &ColorLut, out: &mut [[u8; 4]]) {
        debug_assert_eq!(out.len(), self.pixels);
        match self.mode {
            ResponseMode::Off => lut.map_frame(self.frame(0), out),
            _ if self.filled < 2 => lut.map_frame(self.frame(0), out),
            ResponseMode::Simple => {
                let (cur, prev) = (self.frame(0), self.frame(1));
                for i in 0..self.pixels {
                    out[i] = mix(
                        lut.table[(cur[i] & 0x7FFF) as usize],
                        lut.table[(prev[i] & 0x7FFF) as usize],
                    );
                }
            }
            ResponseMode::Smart => {
                let have_window = self.filled >= RING;
                let f: [&[u16]; RING] = [0, 1, 2, 3, 4, 5].map(|age| self.frame(age));
                for i in 0..self.pixels {
                    let cur = lut.table[(f[0][i] & 0x7FFF) as usize];
                    out[i] = if have_window && self.alternating(&f, i) {
                        mix(cur, lut.table[(f[1][i] & 0x7FFF) as usize])
                    } else {
                        cur
                    };
                }
            }
            ResponseMode::Persistence => {
                for (o, a) in out.iter_mut().zip(&self.acc) {
                    *o = [
                        (a[0] + 0.5) as u8,
                        (a[1] + 0.5) as u8,
                        (a[2] + 0.5) as u8,
                        255,
                    ];
                }
            }
        }
    }

    /// True when pixel `i` is flickering at exactly frame rate: even-phase
    /// frames agree, odd-phase frames agree, and the two phases differ.
    #[inline]
    fn alternating(&self, f: &[&[u16]; RING], i: usize) -> bool {
        let t = self.tolerance;
        close555(f[0][i], f[2][i], t)
            && close555(f[2][i], f[4][i], t)
            && close555(f[1][i], f[3][i], t)
            && close555(f[3][i], f[5][i], t)
            && !close555(f[0][i], f[1][i], t)
    }
}

/// Per-channel comparison of two BGR555 pixels within a 5-bit tolerance.
#[inline]
fn close555(a: u16, b: u16, tol: u16) -> bool {
    if tol == 0 {
        return a == b;
    }
    ((a & 31).abs_diff(b & 31) <= tol)
        && ((a >> 5 & 31).abs_diff(b >> 5 & 31) <= tol)
        && ((a >> 10 & 31).abs_diff(b >> 10 & 31) <= tol)
}

/// 50:50 mix of display-encoded values, round-half-up.
#[inline]
fn mix(a: [u8; 4], b: [u8; 4]) -> [u8; 4] {
    [
        ((a[0] as u16 + b[0] as u16 + 1) >> 1) as u8,
        ((a[1] as u16 + b[1] as u16 + 1) >> 1) as u8,
        ((a[2] as u16 + b[2] as u16 + 1) >> 1) as u8,
        255,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lut::{ColorLut, ColorSettings};
    use crate::profile::ScreenKind;

    fn raw_lut() -> ColorLut {
        ColorLut::build(&ColorSettings {
            screen: ScreenKind::Raw,
            ..ColorSettings::default()
        })
    }

    const WHITE: u16 = 0x7FFF;
    const BLACK: u16 = 0;

    fn run(mode: ResponseMode, frames: &[Vec<u16>], lut: &ColorLut) -> Vec<[u8; 4]> {
        let mut t = Temporal::new(frames[0].len(), mode, 0, 0.5);
        for f in frames {
            t.push(f, lut);
        }
        let mut out = vec![[0u8; 4]; frames[0].len()];
        t.compose(lut, &mut out);
        out
    }

    #[test]
    fn simple_halves_alternation() {
        let lut = raw_lut();
        let frames: Vec<Vec<u16>> = (0..6)
            .map(|i| vec![if i % 2 == 0 { WHITE } else { BLACK }])
            .collect();
        let out = run(ResponseMode::Simple, &frames, &lut);
        assert_eq!(out[0][0], 128); // (255 + 0 + 1) >> 1
    }

    #[test]
    fn smart_blends_flicker_keeps_static_sharp() {
        let lut = raw_lut();
        // pixel 0 alternates white/black; pixel 1 is constant white;
        // pixel 2 cycles 3 values (irregular — must NOT blend).
        let third = [WHITE, BLACK, 0x03E0u16];
        let frames: Vec<Vec<u16>> = (0..6)
            .map(|i| vec![if i % 2 == 0 { WHITE } else { BLACK }, WHITE, third[i % 3]])
            .collect();
        let out = run(ResponseMode::Smart, &frames, &lut);
        assert_eq!(out[0][0], 128, "alternating pixel blends");
        assert_eq!(out[1][0], 255, "static pixel untouched");
        assert_eq!(
            out[2],
            lut.table[third[(6 - 1) % 3] as usize],
            "irregular pixel untouched"
        );
    }

    #[test]
    fn smart_waits_for_full_window() {
        let lut = raw_lut();
        let frames: Vec<Vec<u16>> = (0..3)
            .map(|i| vec![if i % 2 == 0 { WHITE } else { BLACK }])
            .collect();
        let out = run(ResponseMode::Smart, &frames, &lut);
        // Only 3 frames seen: passes through current frame unblended.
        assert_eq!(out[0][0], 255);
    }

    #[test]
    fn smart_tolerance_admits_near_match() {
        let lut = raw_lut();
        let a = WHITE;
        let a_jitter = 0x7FFE; // red 30 vs 31
        let frames = vec![
            vec![a],
            vec![BLACK],
            vec![a_jitter],
            vec![BLACK],
            vec![a],
            vec![BLACK],
        ];
        // exact: jitter breaks the even-phase chain
        let mut t = Temporal::new(1, ResponseMode::Smart, 0, 0.5);
        for f in frames.iter().rev() {
            t.push(f, &lut);
        }
        let mut out = [[0u8; 4]; 1];
        t.compose(&lut, &mut out);
        assert_eq!(out[0][0], 255);
        // tolerance 1: blends
        let mut t = Temporal::new(1, ResponseMode::Smart, 1, 0.5);
        for f in frames.iter().rev() {
            t.push(f, &lut);
        }
        t.compose(&lut, &mut out);
        assert_eq!(out[0][0], 128);
    }

    #[test]
    fn persistence_decays_exponentially() {
        let lut = raw_lut();
        let mut t = Temporal::new(1, ResponseMode::Persistence, 0, 0.5);
        t.push(&[WHITE], &lut); // first frame seeds acc fully
        t.push(&[BLACK], &lut); // 255 * 0.5
        t.push(&[BLACK], &lut); // 255 * 0.25
        let mut out = [[0u8; 4]; 1];
        t.compose(&lut, &mut out);
        assert_eq!(out[0][0], 64); // 63.75 + .5
    }

    #[test]
    fn off_is_pass_through() {
        let lut = raw_lut();
        let frames: Vec<Vec<u16>> = (0..6)
            .map(|i| vec![if i % 2 == 0 { WHITE } else { BLACK }])
            .collect();
        let out = run(ResponseMode::Off, &frames, &lut);
        assert_eq!(out[0], lut.table[BLACK as usize]);
    }

    #[test]
    fn mode_names_roundtrip() {
        for m in ResponseMode::ALL {
            assert_eq!(ResponseMode::by_name(m.name()), Some(m));
        }
    }
}
