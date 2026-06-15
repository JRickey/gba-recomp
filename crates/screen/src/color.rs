//! CIE colorimetry: the math that turns measured panel chromaticities into
//! an exact panel-to-display transform for whatever gamut the user's screen
//! actually has. All math in f64; this runs only at LUT-build time.
//!
//! The pipeline is standard colorimetry (xyY -> XYZ, primaries+white ->
//! RGB/XYZ matrix, Bradford chromatic adaptation), not borrowed shader
//! code — the constants it consumes are published colorimeter measurements.

/// CIE 1931 xy chromaticity.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Xy {
    pub x: f64,
    pub y: f64,
}

/// A set of RGB primaries plus white point, defining an additive colorspace.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Primaries {
    pub red: Xy,
    pub green: Xy,
    pub blue: Xy,
    pub white: Xy,
}

pub const D65: Xy = Xy {
    x: 0.3127,
    y: 0.3290,
};

/// sRGB / Rec.709 primaries, D65.
pub const SRGB: Primaries = Primaries {
    red: Xy { x: 0.64, y: 0.33 },
    green: Xy { x: 0.30, y: 0.60 },
    blue: Xy { x: 0.15, y: 0.06 },
    white: D65,
};

/// Display P3 (DCI primaries, D65 white, sRGB transfer).
pub const DISPLAY_P3: Primaries = Primaries {
    red: Xy { x: 0.680, y: 0.320 },
    green: Xy { x: 0.265, y: 0.690 },
    blue: Xy { x: 0.150, y: 0.060 },
    white: D65,
};

/// Area of the primary triangle in the CIE xy plane — a cheap, monotone
/// proxy for gamut size (sRGB ≈ 0.112, Display P3 ≈ 0.152). Used to tell a
/// wide-gamut panel from an sRGB one without a full volume computation.
pub fn gamut_area(p: &Primaries) -> f64 {
    let (r, g, b) = (p.red, p.green, p.blue);
    0.5 * ((g.x - r.x) * (b.y - r.y) - (b.x - r.x) * (g.y - r.y)).abs()
}

/// Column-major-free, plain row-major 3x3 over f64.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Mat3(pub [[f64; 3]; 3]);

impl Mat3 {
    pub const IDENTITY: Mat3 = Mat3([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]);

    pub fn mul(&self, rhs: &Mat3) -> Mat3 {
        let mut out = [[0.0; 3]; 3];
        for (i, row) in out.iter_mut().enumerate() {
            for (j, v) in row.iter_mut().enumerate() {
                *v = (0..3).map(|k| self.0[i][k] * rhs.0[k][j]).sum();
            }
        }
        Mat3(out)
    }

    pub fn apply(&self, v: [f64; 3]) -> [f64; 3] {
        [0, 1, 2].map(|i| (0..3).map(|k| self.0[i][k] * v[k]).sum())
    }

    pub fn inverse(&self) -> Mat3 {
        let m = &self.0;
        let cof = |r: usize, c: usize| {
            let (r1, r2) = ((r + 1) % 3, (r + 2) % 3);
            let (c1, c2) = ((c + 1) % 3, (c + 2) % 3);
            m[r1][c1] * m[r2][c2] - m[r1][c2] * m[r2][c1]
        };
        let det = m[0][0] * cof(0, 0) + m[0][1] * cof(0, 1) + m[0][2] * cof(0, 2);
        // Transposed cofactors / det. Colorspace matrices are far from
        // singular; a degenerate input is a programming error.
        assert!(det.abs() > 1e-12, "singular colorspace matrix");
        let mut out = [[0.0; 3]; 3];
        for (i, row) in out.iter_mut().enumerate() {
            for (j, v) in row.iter_mut().enumerate() {
                *v = cof(j, i) / det;
            }
        }
        Mat3(out)
    }
}

/// xy -> XYZ with Y = 1.
pub fn xy_to_xyz(c: Xy) -> [f64; 3] {
    [c.x / c.y, 1.0, (1.0 - c.x - c.y) / c.y]
}

/// Linear RGB (in `p`) -> CIE XYZ. Columns are the primaries' XYZ, scaled so
/// that RGB (1,1,1) lands exactly on the white point at Y = 1.
pub fn rgb_to_xyz(p: &Primaries) -> Mat3 {
    let r = xy_to_xyz(p.red);
    let g = xy_to_xyz(p.green);
    let b = xy_to_xyz(p.blue);
    let m = Mat3([[r[0], g[0], b[0]], [r[1], g[1], b[1]], [r[2], g[2], b[2]]]);
    let s = m.inverse().apply(xy_to_xyz(p.white));
    let mut out = m.0;
    for row in &mut out {
        for (v, sj) in row.iter_mut().zip(s) {
            *v *= sj;
        }
    }
    Mat3(out)
}

/// Bradford chromatic adaptation: XYZ relative to white `from` -> XYZ
/// relative to white `to`. Used when a panel's white point is not the
/// display's (e.g. simulating an unlit reflective panel under a warm
/// ambient illuminant).
pub fn bradford_adaptation(from: Xy, to: Xy) -> Mat3 {
    const BRADFORD: Mat3 = Mat3([
        [0.8951, 0.2664, -0.1614],
        [-0.7502, 1.7135, 0.0367],
        [0.0389, -0.0685, 1.0296],
    ]);
    let src = BRADFORD.apply(xy_to_xyz(from));
    let dst = BRADFORD.apply(xy_to_xyz(to));
    let scale = Mat3([
        [dst[0] / src[0], 0.0, 0.0],
        [0.0, dst[1] / src[1], 0.0],
        [0.0, 0.0, dst[2] / src[2]],
    ]);
    BRADFORD.inverse().mul(&scale).mul(&BRADFORD)
}

/// Linear-light transform from one RGB space to another, adapting white
/// points when they differ.
pub fn rgb_to_rgb(src: &Primaries, dst: &Primaries) -> Mat3 {
    let to_xyz = rgb_to_xyz(src);
    let from_xyz = rgb_to_xyz(dst).inverse();
    if src.white == dst.white {
        from_xyz.mul(&to_xyz)
    } else {
        from_xyz
            .mul(&bradford_adaptation(src.white, dst.white))
            .mul(&to_xyz)
    }
}

/// Piecewise sRGB opto-electronic transfer (linear -> encoded). This is what
/// a colorspace-tagged sRGB surface is decoded with by the OS compositor —
/// distinct from the pure power laws the panel models use on the input side.
pub fn srgb_oetf(v: f64) -> f64 {
    if v <= 0.0031308 {
        12.92 * v
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(a: f64, b: f64, tol: f64, what: &str) {
        assert!((a - b).abs() <= tol, "{what}: {a} vs {b}");
    }

    #[test]
    fn srgb_matrix_matches_published_values() {
        // IEC 61966-2-1 reference matrix, 4 decimal places.
        let m = rgb_to_xyz(&SRGB).0;
        let expect = [
            [0.4124, 0.3576, 0.1805],
            [0.2126, 0.7152, 0.0722],
            [0.0193, 0.1192, 0.9505],
        ];
        for i in 0..3 {
            for j in 0..3 {
                assert_close(m[i][j], expect[i][j], 5e-4, "srgb[{i}][{j}]");
            }
        }
    }

    #[test]
    fn inverse_roundtrips() {
        let m = rgb_to_xyz(&DISPLAY_P3);
        let id = m.mul(&m.inverse()).0;
        for i in 0..3 {
            for j in 0..3 {
                assert_close(id[i][j], Mat3::IDENTITY.0[i][j], 1e-12, "id");
            }
        }
    }

    #[test]
    fn bradford_identity_when_whites_match() {
        let m = bradford_adaptation(D65, D65).0;
        for i in 0..3 {
            for j in 0..3 {
                assert_close(m[i][j], Mat3::IDENTITY.0[i][j], 1e-12, "bradford");
            }
        }
    }

    #[test]
    fn white_maps_to_white() {
        // Any same-white RGB->RGB transform must carry (1,1,1) to (1,1,1).
        let m = rgb_to_rgb(&DISPLAY_P3, &SRGB);
        let w = m.apply([1.0, 1.0, 1.0]);
        for c in w {
            assert_close(c, 1.0, 1e-9, "white");
        }
    }

    #[test]
    fn srgb_oetf_endpoints_and_continuity() {
        assert_eq!(srgb_oetf(0.0), 0.0);
        assert_close(srgb_oetf(1.0), 1.0, 1e-12, "oetf(1)");
        let lo = srgb_oetf(0.0031308 - 1e-9);
        let hi = srgb_oetf(0.0031308 + 1e-9);
        assert_close(lo, hi, 1e-6, "oetf knee");
    }
}
