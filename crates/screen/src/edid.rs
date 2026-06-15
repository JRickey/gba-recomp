//! Read the connected panel's measured primaries from its EDID (Linux).
//!
//! This is an OS interface, not a guess: the kernel exposes each DRM
//! connector's raw EDID at `/sys/class/drm/<card>-<connector>/edid`, and the
//! base block carries the display's chromaticity coordinates. We use these
//! only for the app-side fallback — when the compositor isn't color-managing,
//! we compress sRGB into *these* primaries so a wide-gamut panel doesn't
//! oversaturate. When the compositor IS managing, it owns the panel's
//! characteristics and we ignore this.

use std::fs;
use std::path::Path;

use crate::color::{gamut_area, Primaries, Xy};

/// Primaries of the connected panel, or `None` if no usable EDID is found.
/// With several displays connected we pick the widest-gamut one — that's the
/// panel most likely to oversaturate, and the case the fallback exists for.
/// (Per-window connector matching is a TODO for multi-monitor accuracy.)
pub fn panel_primaries() -> Option<Primaries> {
    let drm = Path::new("/sys/class/drm");
    let mut best: Option<Primaries> = None;
    for entry in fs::read_dir(drm).ok()?.flatten() {
        let dir = entry.path();
        // Connector dirs look like `card1-DP-3`; skip `card1`, `renderD*` etc.
        if !dir
            .file_name()
            .is_some_and(|n| n.to_string_lossy().contains('-'))
        {
            continue;
        }
        if !matches!(fs::read_to_string(dir.join("status")), Ok(s) if s.trim() == "connected") {
            continue;
        }
        let Ok(edid) = fs::read(dir.join("edid")) else {
            continue;
        };
        let Some(prim) = parse(&edid) else { continue };
        // Keep the widest gamut seen.
        match best {
            Some(b) if gamut_area(&prim) <= gamut_area(&b) => {}
            _ => best = Some(prim),
        }
    }
    best
}

/// Parse the EDID 1.x base block's chromaticity into `Primaries`. Returns
/// `None` on a bad header or implausibly small (degenerate / sRGB-default)
/// gamut — in which case the panel reports nothing wide to correct toward.
fn parse(edid: &[u8]) -> Option<Primaries> {
    if edid.len() < 128 || edid[0..8] != [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00] {
        return None;
    }
    // Chromaticity block (bytes 25..35): 2-bit low parts packed in 25/26,
    // 8-bit high parts in 27..35. Each coordinate = (hi << 2 | lo) / 1024.
    let lo = |byte: usize, shift: u8| ((edid[byte] >> shift) & 0x3) as u16;
    let coord = |hi: usize, byte: usize, shift: u8| {
        (((edid[hi] as u16) << 2) | lo(byte, shift)) as f64 / 1024.0
    };
    let red = Xy {
        x: coord(27, 25, 6),
        y: coord(28, 25, 4),
    };
    let green = Xy {
        x: coord(29, 25, 2),
        y: coord(30, 25, 0),
    };
    let blue = Xy {
        x: coord(31, 26, 6),
        y: coord(32, 26, 4),
    };
    let white = Xy {
        x: coord(33, 26, 2),
        y: coord(34, 26, 0),
    };

    let prim = Primaries {
        red,
        green,
        blue,
        white,
    };
    // Reject obviously bad reads; also treat ~sRGB as "nothing to correct".
    if !plausible(&prim) || gamut_area(&prim) <= gamut_area(&crate::color::SRGB) * 1.02 {
        return None;
    }
    Some(prim)
}

/// Coordinates in (0,1), a non-degenerate triangle, white near the locus.
fn plausible(p: &Primaries) -> bool {
    let ok = |c: &Xy| c.x > 0.0 && c.x < 1.0 && c.y > 0.0 && c.y < 1.0;
    ok(&p.red) && ok(&p.green) && ok(&p.blue) && ok(&p.white) && gamut_area(p) > 0.01
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand-built EDID header + chromaticity block for a P3-ish panel
    /// (matches the dev display) parses to the expected primaries.
    #[test]
    fn parses_wide_gamut_chromaticity() {
        let mut e = vec![0u8; 128];
        e[0..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
        // Encode R(0.686,0.309) G(0.265,0.668) B(0.150,0.058) W(0.3127,0.329).
        let enc = |v: f64| (v * 1024.0).round() as u16;
        let put = |e: &mut [u8], hi: usize, byte: usize, shift: u8, v: u16| {
            e[hi] = (v >> 2) as u8;
            e[byte] |= ((v & 0x3) as u8) << shift;
        };
        put(&mut e, 27, 25, 6, enc(0.686));
        put(&mut e, 28, 25, 4, enc(0.309));
        put(&mut e, 29, 25, 2, enc(0.265));
        put(&mut e, 30, 25, 0, enc(0.668));
        put(&mut e, 31, 26, 6, enc(0.150));
        put(&mut e, 32, 26, 4, enc(0.058));
        put(&mut e, 33, 26, 2, enc(0.3127));
        put(&mut e, 34, 26, 0, enc(0.329));

        let p = parse(&e).expect("should parse + be wider than sRGB");
        assert!((p.red.x - 0.686).abs() < 0.002, "red x {}", p.red.x);
        assert!((p.green.y - 0.668).abs() < 0.002, "green y {}", p.green.y);
        assert!(gamut_area(&p) > gamut_area(&crate::color::SRGB));
    }

    #[test]
    fn rejects_bad_header() {
        assert!(parse(&[0u8; 128]).is_none());
    }
}
