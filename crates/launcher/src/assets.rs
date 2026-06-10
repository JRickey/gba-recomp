//! Procedurally generated assets. The launcher ships no image files;
//! everything (including the window icon) is computed at startup.

use eframe::egui;

/// 64x64 window icon: glossy indigo rounded square, chrome ring, and a
/// silver D-pad cross — drawn pixel-by-pixel.
pub fn app_icon() -> egui::IconData {
    const S: usize = 64;
    let mut rgba = vec![0u8; S * S * 4];

    // signed distance to a rounded square centered at origin
    let sd_round_rect = |x: f32, y: f32, half: f32, r: f32| -> f32 {
        let qx = x.abs() - (half - r);
        let qy = y.abs() - (half - r);
        let ox = qx.max(0.0);
        let oy = qy.max(0.0);
        (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - r
    };

    for py in 0..S {
        for px in 0..S {
            // pixel center in [-1, 1]
            let x = (px as f32 + 0.5) / S as f32 * 2.0 - 1.0;
            let y = (py as f32 + 0.5) / S as f32 * 2.0 - 1.0;

            let d = sd_round_rect(x, y, 0.92, 0.34);
            if d > 0.0 {
                continue; // transparent outside the shell
            }
            let edge = (-d * 14.0).min(1.0); // antialiased rim

            // indigo plastic with vertical light falloff
            let t = (y + 1.0) / 2.0;
            let mut r = 0x3f as f32 + (0x6f - 0x3f) as f32 * (1.0 - t);
            let mut g = 0x44 as f32 + (0x66 - 0x44) as f32 * (1.0 - t);
            let mut b = 0x9e as f32 + (0xdd - 0x9e) as f32 * (1.0 - t);

            // top gloss band
            if y < -0.15 {
                let gl = ((-0.15 - y) / 0.85) * 0.35;
                r += (255.0 - r) * gl;
                g += (255.0 - g) * gl;
                b += (255.0 - b) * gl;
            }

            // chrome ring
            let rad = (x * x + y * y).sqrt();
            let ring = ((rad - 0.62).abs() - 0.05).max(0.0);
            if ring < 0.04 {
                let m = 1.0 - ring / 0.04;
                let shade = 200.0 - y * 50.0;
                r += (shade - r) * m;
                g += (shade - g) * m;
                b += (shade + 12.0 - b) * m;
            }

            // silver D-pad cross
            let arm = 0.16;
            let len = 0.46;
            if (x.abs() < arm && y.abs() < len) || (y.abs() < arm && x.abs() < len) {
                let shade = 225.0 - y * 35.0;
                r = shade;
                g = shade + 3.0;
                b = shade + 14.0;
                // cross bevel shadow on lower edge
                if y > len - 0.08 || (y > arm - 0.08 && x.abs() >= arm) {
                    r -= 50.0;
                    g -= 50.0;
                    b -= 40.0;
                }
            }

            let i = (py * S + px) * 4;
            rgba[i] = r.clamp(0.0, 255.0) as u8;
            rgba[i + 1] = g.clamp(0.0, 255.0) as u8;
            rgba[i + 2] = b.clamp(0.0, 255.0) as u8;
            rgba[i + 3] = (edge * 255.0) as u8;
        }
    }

    egui::IconData { rgba, width: S as u32, height: S as u32 }
}
