//! The color pipeline, baked: a 32768-entry table mapping every raw BGR555
//! framebuffer value to display-ready RGBA8. 128 KB, L2-resident, rebuilt in
//! well under a millisecond on any settings change; per-frame cost is one
//! lookup per source pixel. Frame hashing / verify / sweeps stay defined on
//! the raw BGR555 frames — this table is present-time only.

use crate::color::{rgb_to_rgb, srgb_oetf, Mat3, Primaries};
use crate::profile::ScreenKind;

/// BGR555 -> RGBA8 (alpha 255). Index with the raw 15-bit framebuffer value.
pub struct ColorLut {
    pub table: Box<[[u8; 4]; 32768]>,
}

/// Settings a LUT is built from.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ColorSettings {
    pub screen: ScreenKind,
    /// Reflective-family viewing-darkness knob, 0..1. NaN = screen default.
    pub darken: f64,
    /// Primaries to encode the output for (resolved by the caller from the
    /// configured [`DisplayTarget`](crate::profile::DisplayTarget) plus OS
    /// detection — sRGB, Display P3, or the measured panel gamut).
    pub target: Primaries,
}

impl Default for ColorSettings {
    fn default() -> Self {
        // The library was authored against the reflective panel family; the
        // frontlit model is the default "as designed, comfortably visible"
        // view. Raw remains one click away.
        Self {
            screen: ScreenKind::Frontlit,
            darken: f64::NAN,
            target: crate::color::SRGB,
        }
    }
}

impl ColorSettings {
    pub fn darken_or_default(&self) -> f64 {
        if self.darken.is_nan() {
            self.screen.default_darken()
        } else {
            self.darken.clamp(0.0, 1.0)
        }
    }
}

#[inline]
fn unpack555(px: u16) -> [f64; 3] {
    [
        (px & 31) as f64 / 31.0,
        ((px >> 5) & 31) as f64 / 31.0,
        ((px >> 10) & 31) as f64 / 31.0,
    ]
}

#[inline]
fn quantize(v: f64) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

impl ColorLut {
    pub fn build(settings: &ColorSettings) -> ColorLut {
        let mut table: Box<[[u8; 4]; 32768]> =
            vec![[0u8; 4]; 32768].into_boxed_slice().try_into().unwrap();

        match settings.screen {
            ScreenKind::Raw => {
                for (px, entry) in table.iter_mut().enumerate() {
                    // Plain 5->8 bit replication, no color model.
                    let r = (px & 31) as u8;
                    let g = ((px >> 5) & 31) as u8;
                    let b = ((px >> 10) & 31) as u8;
                    *entry = [r << 3 | r >> 2, g << 3 | g >> 2, b << 3 | b >> 2, 255];
                }
            }
            ScreenKind::Classic => {
                for (px, entry) in table.iter_mut().enumerate() {
                    *entry = classic_entry(px as u16);
                }
            }
            screen => {
                let model = screen.model(settings.darken_or_default()).unwrap();
                let to_display: Mat3 = rgb_to_rgb(&model.primaries, &settings.target);
                for (px, entry) in table.iter_mut().enumerate() {
                    let c = unpack555(px as u16);
                    // Panel tone response (authored intent), pure power law.
                    let lin = c.map(|v| v.powf(model.gamma));
                    // Measured luminance scale, then the gamut transform.
                    let lin = lin.map(|v| (v * model.luminance).clamp(0.0, 1.0));
                    let mut out = to_display.apply(lin);
                    // The reflective panel sits outside the display gamut on
                    // a thin negative lobe; clip, then apply the optical
                    // black floor in linear light.
                    for v in &mut out {
                        let clipped = v.clamp(0.0, 1.0);
                        let lifted = model.black_floor + (1.0 - model.black_floor) * clipped;
                        // Encode with the display transfer the surface is
                        // actually tagged/assumed as (piecewise sRGB — both
                        // sRGB and Display P3 use it).
                        *v = srgb_oetf(lifted);
                    }
                    *entry = [quantize(out[0]), quantize(out[1]), quantize(out[2]), 255];
                }
            }
        }
        ColorLut { table }
    }

    /// Map one raw frame into an RGBA8 buffer (no temporal processing).
    pub fn map_frame(&self, src: &[u16], dst: &mut [[u8; 4]]) {
        for (d, &s) in dst.iter_mut().zip(src) {
            *d = self.table[(s & 0x7FFF) as usize];
        }
    }
}

/// The community-canonical gamma-4.0 model, reproduced exactly as published
/// (a linear-light channel mix at LCD gamma 4.0, re-encoded at 2.2 with a
/// 255/280 normalization; constants are from the original author's public
/// write-up). Output is
/// already display-encoded; it defined "correct" for a decade of accuracy-
/// focused emulation and is kept bit-faithful rather than re-derived.
fn classic_entry(px: u16) -> [u8; 4] {
    let [r, g, b] = unpack555(px);
    let lr = r.powf(4.0);
    let lg = g.powf(4.0);
    let lb = b.powf(4.0);
    let mix = |a: f64, bq: f64, c: f64| {
        let v: f64 = (a * lb + bq * lg + c * lr) / 255.0;
        (v.powf(1.0 / 2.2) * (255.0 / 280.0)).clamp(0.0, 1.0)
    };
    [
        quantize(mix(0.0, 50.0, 255.0)),
        quantize(mix(30.0, 230.0, 10.0)),
        quantize(mix(220.0, 10.0, 50.0)),
        255,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(screen: ScreenKind) -> ColorSettings {
        ColorSettings {
            screen,
            ..ColorSettings::default()
        }
    }

    #[test]
    fn raw_replicates_5bit() {
        let lut = ColorLut::build(&settings(ScreenKind::Raw));
        assert_eq!(lut.table[0], [0, 0, 0, 255]);
        assert_eq!(lut.table[0x7FFF], [255, 255, 255, 255]);
        // Pure red, mid value: r=16 -> 10000 -> 10000100 = 132.
        assert_eq!(lut.table[16], [132, 0, 0, 255]);
    }

    #[test]
    fn channel_order_is_bgr555() {
        // Red lives in the LOW 5 bits of the framebuffer word.
        for screen in [
            ScreenKind::Frontlit,
            ScreenKind::Backlit,
            ScreenKind::Classic,
        ] {
            let lut = ColorLut::build(&settings(screen));
            let red = lut.table[0x001F];
            let blue = lut.table[0x7C00];
            assert!(red[0] > red[2], "{screen:?} red entry {red:?}");
            assert!(blue[2] > blue[0], "{screen:?} blue entry {blue:?}");
        }
    }

    #[test]
    fn white_stays_neutral() {
        // Full white must stay neutral (equal channels) through every
        // colorimetric model: the gamut transforms are white-preserving by
        // construction. Classic is exempt — that model's red row is
        // deliberately overdriven and its white is intentionally warm.
        for screen in ScreenKind::ALL
            .into_iter()
            .filter(|s| *s != ScreenKind::Classic)
        {
            let lut = ColorLut::build(&settings(screen));
            let [r, g, b, _] = lut.table[0x7FFF];
            assert!(
                r.abs_diff(g) <= 1 && g.abs_diff(b) <= 1,
                "{screen:?}: white = {:?}",
                lut.table[0x7FFF]
            );
        }
    }

    #[test]
    fn monotone_per_channel() {
        // Brighter input never gets darker output on the same channel.
        for screen in ScreenKind::ALL {
            let lut = ColorLut::build(&settings(screen));
            for v in 0..31u16 {
                assert!(
                    lut.table[(v + 1) as usize][0] >= lut.table[v as usize][0],
                    "{screen:?} red not monotone at {v}"
                );
            }
        }
    }

    #[test]
    fn reflective_family_desaturates_red() {
        // The defining visible behavior: saturated red shifts orange (gains
        // green) and loses purity vs raw.
        let lut = ColorLut::build(&settings(ScreenKind::Frontlit));
        let [r, g, _b, _] = lut.table[0x001F];
        assert!(g > 60, "red should bleed toward orange, got g={g}");
        assert!(r < 250, "red should lose purity, got r={r}");
    }

    #[test]
    fn backlit_blacks_darker_than_frontlit() {
        let back = ColorLut::build(&settings(ScreenKind::Backlit));
        let front = ColorLut::build(&settings(ScreenKind::Frontlit));
        assert!(back.table[0][0] < front.table[0][0]);
    }

    #[test]
    fn classic_matches_published_formula_spot_values() {
        // Independent recomputation of two table entries straight from the
        // published math, full precision.
        // White: row sums 305/270/280 over 255 re-encoded at 1/2.2, scaled
        // 255/280 — none of the channels actually clip at white, so white
        // comes out warm: (1.19608^(1/2.2))*0.910714 = 0.98793 -> 252, etc.
        assert_eq!(classic_entry(0x7FFF), [252, 238, 242, 255]);
        let darkish = classic_entry(0x0210); // r=16, g=16, b=0
        let lr: f64 = (16.0 / 31.0f64).powf(4.0);
        let want_r = ((50.0 * lr + 255.0 * lr) / 255.0).powf(1.0 / 2.2) * (255.0 / 280.0) * 255.0;
        assert_eq!(darkish[0], (want_r + 0.5) as u8);
    }

    #[test]
    fn map_frame_applies_table() {
        let lut = ColorLut::build(&settings(ScreenKind::Raw));
        let src = [0x7FFFu16, 0, 16];
        let mut dst = [[0u8; 4]; 3];
        lut.map_frame(&src, &mut dst);
        assert_eq!(dst[0], [255, 255, 255, 255]);
        assert_eq!(dst[2], [132, 0, 0, 255]);
    }
}
