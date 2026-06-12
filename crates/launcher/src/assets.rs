//! Procedurally generated assets. The launcher ships no image files;
//! everything (including the window icon) is computed at startup.
//!
//! The canonical mark is rendered by [`render_icon_rgba`] at any
//! resolution — it is the launcher's window icon AND the source the
//! packager (`gba-pack`) derives platform icon formats from, so the
//! product reads as one identity everywhere. Keeping it a renderer
//! rather than a bundled PNG preserves the "zero image assets" ethos.

use eframe::egui;

/// Render the canonical app icon into a freshly allocated `size`×`size`
/// RGBA8 buffer (premultiplied alpha is *not* applied — straight RGBA,
/// as both egui's `IconData` and PNG want it).
///
/// The mark: a glossy indigo→violet rounded-square shell (the Y2K
/// translucent-plastic console), a brushed-chrome ring, and a silver
/// D-pad cross with a cyan center jewel. Antialiased by 2×2 supersampling
/// so it stays crisp from a 16-px favicon up to a 1024-px master.
pub fn render_icon_rgba(size: usize) -> Vec<u8> {
    let mut rgba = vec![0u8; size * size * 4];
    // 2×2 supersample: four sub-samples per pixel, averaged. Cheap, and
    // the alternative (relying on the SDF rim falloff alone) leaves the
    // chrome ring and cross edges visibly stairstepped at small sizes.
    const SS: usize = 2;
    let inv = 1.0 / size as f32;
    for py in 0..size {
        for px in 0..size {
            let mut acc = [0.0f32; 4];
            for sy in 0..SS {
                for sx in 0..SS {
                    let fx = (px as f32 + (sx as f32 + 0.5) / SS as f32) * inv;
                    let fy = (py as f32 + (sy as f32 + 0.5) / SS as f32) * inv;
                    // sample plane in [-1, 1]
                    let x = fx * 2.0 - 1.0;
                    let y = fy * 2.0 - 1.0;
                    let [r, g, b, a] = sample(x, y);
                    acc[0] += r;
                    acc[1] += g;
                    acc[2] += b;
                    acc[3] += a;
                }
            }
            let n = (SS * SS) as f32;
            let i = (py * size + px) * 4;
            rgba[i] = (acc[0] / n).clamp(0.0, 255.0) as u8;
            rgba[i + 1] = (acc[1] / n).clamp(0.0, 255.0) as u8;
            rgba[i + 2] = (acc[2] / n).clamp(0.0, 255.0) as u8;
            rgba[i + 3] = (acc[3] / n).clamp(0.0, 255.0) as u8;
        }
    }
    rgba
}

/// Shade one point of the icon plane (coordinates in [-1, 1]); returns
/// straight RGBA in 0..=255, alpha 0 outside the shell. Pulled out of the
/// pixel loop so the supersampler can call it per sub-sample.
fn sample(x: f32, y: f32) -> [f32; 4] {
    // signed distance to a rounded square centered at origin
    let sd_round_rect = |half: f32, r: f32| -> f32 {
        let qx = x.abs() - (half - r);
        let qy = y.abs() - (half - r);
        let ox = qx.max(0.0);
        let oy = qy.max(0.0);
        (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - r
    };

    let d = sd_round_rect(0.92, 0.34);
    if d > 0.0 {
        return [0.0, 0.0, 0.0, 0.0]; // transparent outside the shell
    }
    // antialiased rim; the falloff scale is in shell-half units, so it is
    // resolution independent (the supersampler refines it further)
    let edge = (-d * 22.0).clamp(0.0, 1.0);

    // indigo plastic with vertical light falloff (shell -> glossy violet)
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

    // cyan center jewel — the electric accent that ties it to the wordmark
    let cr = (x * x + y * y).sqrt();
    if cr < 0.12 {
        let m = (1.0 - cr / 0.12).powf(0.6);
        // CYAN (0x7d, 0xe8, 0xff) with a hot white core
        r += (0x7d as f32 - r) * m + (255.0 - 0x7d as f32) * m * m * 0.7;
        g += (0xe8 as f32 - g) * m + (255.0 - 0xe8 as f32) * m * m * 0.7;
        b += (0xff as f32 - b) * m;
    }

    [r, g, b, edge * 255.0]
}

/// 64×64 window icon for egui's `ViewportBuilder::with_icon`.
pub fn app_icon() -> egui::IconData {
    const S: u32 = 64;
    egui::IconData {
        rgba: render_icon_rgba(S as usize),
        width: S,
        height: S,
    }
}

/// Encode an 8-bit RGBA buffer as a PNG (truecolor + alpha). Used by the
/// `gba-icon` generator to write the committed master; small enough to keep
/// here rather than pull a full image stack into the tool.
pub fn encode_png_rgba(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    use flate2::{write::ZlibEncoder, Compression};
    use std::io::Write;

    assert_eq!(
        rgba.len(),
        (width * height * 4) as usize,
        "rgba size mismatch"
    );
    // Filter byte 0 (None) per scanline, then zlib-compress the lot.
    let mut raw = Vec::with_capacity((width * height * 4 + height) as usize);
    let stride = (width * 4) as usize;
    for row in rgba.chunks_exact(stride) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut z = ZlibEncoder::new(Vec::new(), Compression::best());
    z.write_all(&raw).unwrap();
    let idat = z.finish().unwrap();

    let mut out = Vec::with_capacity(idat.len() + 64);
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    let chunk = |out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let crc = crc32(&out[start..]);
        out.extend_from_slice(&crc.to_be_bytes());
    };
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit, color type 6 (RGBA)
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &idat);
    chunk(&mut out, b"IEND", &[]);
    out
}

/// PNG/zlib CRC-32 (IEEE), computed without a lookup table — the icon
/// generator runs once and the payloads are tiny.
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xffff_ffff;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}
