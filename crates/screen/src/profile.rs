//! Screen device profiles: per-hardware-revision models of how the console's
//! panels rendered the 15-bit colors developers authored, plus the display
//! targets we re-render them for.
//!
//! Three physical screen generations shipped, but only two color regimes:
//! the original reflective panel and its frontlit successor share one
//! measured gamut (the frontlight changes brightness/contrast, not the
//! filters); the late backlit revision is a different, near-sRGB panel.
//! Primaries below are from the public-domain colorimeter dataset; the
//! backlit set is recovered from the same dataset's published transform.

use crate::color::{Primaries, Xy, D65};

/// Measured primaries of the original (reflective/frontlit) panel family.
/// Strongly orange-shifted red, desaturated green, blue lifted toward white:
/// the deliberately weak color filters of a reflective LCD. Entirely inside
/// sRGB — reproducing it is a matter of correct mapping, not wide gamut.
pub const PANEL_REFLECTIVE: Primaries = Primaries {
    red: Xy {
        x: 0.4925,
        y: 0.3100,
    },
    green: Xy {
        x: 0.3150,
        y: 0.4825,
    },
    blue: Xy {
        x: 0.1625,
        y: 0.1925,
    },
    white: D65,
};

/// Backlit-revision panel, recovered from the public-domain measured
/// transform (it is near-sRGB; late titles were authored against it).
pub const PANEL_BACKLIT: Primaries = Primaries {
    red: Xy {
        x: 0.6191,
        y: 0.3454,
    },
    green: Xy {
        x: 0.3269,
        y: 0.6003,
    },
    blue: Xy {
        x: 0.1436,
        y: 0.0893,
    },
    white: D65,
};

/// Which physical screen to simulate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScreenKind {
    /// No simulation: 5-bit channels replicated to 8-bit, untouched.
    Raw,
    /// Original unlit reflective panel: dark tone response, washed gamut,
    /// ambient light floor.
    Unlit,
    /// Frontlit revision: same panel/gamut, lit — brighter response, milky
    /// blacks from frontlight scatter.
    Frontlit,
    /// Late backlit revision: near-sRGB gamut, clean blacks.
    Backlit,
    /// The community-canonical gamma-4.0 model (kept for users who know
    /// that exact look; not colorimeter-derived).
    Classic,
}

impl ScreenKind {
    pub const ALL: [ScreenKind; 5] = [
        ScreenKind::Raw,
        ScreenKind::Unlit,
        ScreenKind::Frontlit,
        ScreenKind::Backlit,
        ScreenKind::Classic,
    ];

    /// Config-file token and canonical name.
    pub fn name(self) -> &'static str {
        match self {
            ScreenKind::Raw => "raw",
            ScreenKind::Unlit => "unlit",
            ScreenKind::Frontlit => "frontlit",
            ScreenKind::Backlit => "backlit",
            ScreenKind::Classic => "classic",
        }
    }

    pub fn by_name(name: &str) -> Option<ScreenKind> {
        Self::ALL.into_iter().find(|k| k.name() == name)
    }
}

/// The fully-resolved optical model a LUT is built from.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct PanelModel {
    pub primaries: Primaries,
    /// Pure power-law tone response of the panel (authored intent is
    /// "looks right at this response"). 2.2 for lit panels; reflective
    /// viewing pushes the effective exponent toward 3-4.
    pub gamma: f64,
    /// Linear-light luminance scale applied before the gamut transform
    /// (measured panels run slightly dim vs full-scale).
    pub luminance: f64,
    /// Linear-light black floor: frontlight scatter / ambient reflection.
    /// out = floor + (1 - floor) * out.
    pub black_floor: f64,
}

/// Continuous user knob (0..1): how dark the viewing conditions are for the
/// reflective-family screens. Raises the effective tone-response exponent
/// from the lit measurement (2.2) toward the unlit-viewing extreme (3.8),
/// matching the public dataset's darken semantics.
pub fn effective_gamma(darken: f64) -> f64 {
    2.2 + 1.6 * darken.clamp(0.0, 1.0)
}

impl ScreenKind {
    /// Default pixel-grid strength: the grid IS the physical screen for
    /// the device models; Raw/Classic are pure color modes, default flat.
    pub fn default_grid_strength(self) -> f32 {
        match self {
            ScreenKind::Raw | ScreenKind::Classic => 0.0,
            _ => 1.0,
        }
    }

    /// Default darken value per screen; the config can override it.
    pub fn default_darken(self) -> f64 {
        match self {
            ScreenKind::Unlit => 0.7,
            ScreenKind::Frontlit => 0.15,
            _ => 0.0,
        }
    }

    /// Resolve to the optical model. `darken` only affects the
    /// reflective-family screens (the lit backlit panel has no ambient knob).
    pub fn model(self, darken: f64) -> Option<PanelModel> {
        match self {
            ScreenKind::Raw | ScreenKind::Classic => None,
            ScreenKind::Unlit => Some(PanelModel {
                primaries: PANEL_REFLECTIVE,
                gamma: effective_gamma(darken),
                luminance: 0.91,
                black_floor: 0.055,
            }),
            ScreenKind::Frontlit => Some(PanelModel {
                primaries: PANEL_REFLECTIVE,
                gamma: effective_gamma(darken),
                luminance: 0.91,
                black_floor: 0.030,
            }),
            ScreenKind::Backlit => Some(PanelModel {
                primaries: PANEL_BACKLIT,
                gamma: 2.2,
                luminance: 0.935,
                black_floor: 0.002,
            }),
        }
    }
}

/// What colorspace the bytes we hand the OS are interpreted in.
///
/// `Auto` resolves per platform: on macOS we tag the surface's layer as sRGB
/// and the compositor color-matches to the actual panel (P3 laptop screens
/// included) — detection is the OS's job and it does it per-monitor. On
/// platforms without compositor color management we assume the SDR standard
/// (sRGB); `DisplayP3` is the manual override for an unmanaged wide-gamut
/// monitor, where we target the panel's primaries ourselves.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DisplayTarget {
    Auto,
    Srgb,
    DisplayP3,
}

impl DisplayTarget {
    pub const ALL: [DisplayTarget; 3] = [
        DisplayTarget::Auto,
        DisplayTarget::Srgb,
        DisplayTarget::DisplayP3,
    ];

    pub fn name(self) -> &'static str {
        match self {
            DisplayTarget::Auto => "auto",
            DisplayTarget::Srgb => "srgb",
            DisplayTarget::DisplayP3 => "display-p3",
        }
    }

    pub fn by_name(name: &str) -> Option<DisplayTarget> {
        Self::ALL.into_iter().find(|t| t.name() == name)
    }

    /// The primaries the LUT should encode for, for an explicit target.
    /// `Auto`/`Srgb` = sRGB (the platform assumption, or a compositor that
    /// color-manages our untagged-sRGB output). The auto-detected wide-gamut
    /// fallback does not go through here — see [`crate::display::resolve`],
    /// which returns measured panel primaries directly.
    pub fn primaries(self) -> Primaries {
        match self {
            DisplayTarget::Auto | DisplayTarget::Srgb => crate::color::SRGB,
            DisplayTarget::DisplayP3 => crate::color::DISPLAY_P3,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_roundtrip() {
        for k in ScreenKind::ALL {
            assert_eq!(ScreenKind::by_name(k.name()), Some(k));
        }
        for t in DisplayTarget::ALL {
            assert_eq!(DisplayTarget::by_name(t.name()), Some(t));
        }
    }

    #[test]
    fn darken_gamma_range() {
        assert_eq!(effective_gamma(0.0), 2.2);
        assert!((effective_gamma(1.0) - 3.8).abs() < 1e-12);
        assert_eq!(effective_gamma(7.0), effective_gamma(1.0)); // clamped
    }

    #[test]
    fn reflective_panel_to_srgb_matches_reference_transform() {
        // The public-domain dataset also publishes the hand-shipped
        // panel->sRGB matrix. Deriving it from the measured primaries via
        // standard colorimetry must land on the same transform — this is
        // the clean-room cross-check for the whole color core.
        let m = crate::color::rgb_to_rgb(&PANEL_REFLECTIVE, &crate::color::SRGB).0;
        let reference = [
            [0.905, 0.195, -0.10],
            [0.10, 0.65, 0.25],
            [0.1575, 0.1425, 0.70],
        ];
        for i in 0..3 {
            for j in 0..3 {
                assert!(
                    (m[i][j] - reference[i][j]).abs() < 0.006,
                    "[{i}][{j}]: derived {} vs reference {}",
                    m[i][j],
                    reference[i][j]
                );
            }
        }
    }

    #[test]
    fn backlit_panel_is_near_srgb() {
        let m = crate::color::rgb_to_rgb(&PANEL_BACKLIT, &crate::color::SRGB).0;
        for (i, row) in m.iter().enumerate() {
            for (j, v) in row.iter().enumerate() {
                let id = if i == j { 1.0 } else { 0.0 };
                assert!((v - id).abs() < 0.12, "[{i}][{j}] = {v}");
            }
        }
    }
}
