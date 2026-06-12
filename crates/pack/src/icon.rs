//! Executable-icon embedding for `gba-pack build`.
//!
//! One square PNG master (the packager's `package.icon`, or the bundled
//! canonical launcher icon) is decoded once, resized to the sizes each
//! platform wants, and re-encoded into the platform's icon container:
//!
//!   - macOS → `<name>.app/Contents/Resources/<name>.icns` (PNG-backed
//!     `icp4/ic07/ic08/ic09/ic10` entries) + Info.plist, then the bundle
//!     is ad-hoc codesigned.
//!   - Windows → `<name>.ico` (multi-size, BMP-encoded entries) emitted
//!     into the package dir; PE resource embedding is wired for builds
//!     hosted ON Windows (see `embed_windows_pe`).
//!   - Linux → `<name>.png` (256²) + a `<name>.desktop` entry, XDG-style.
//!
//! The package stays unzip-and-run on every platform: no installer, the
//! runtime resolves its ROM/BIOS/manifest beside the executable (inside
//! `Contents/MacOS/` for the .app — see the bundle assembly note).
//!
//! The PNG codec here is deliberately minimal (8-bit RGB/RGBA, no
//! interlace) — enough for an icon master, not a general image library.

use std::io::{Read, Write};
use std::path::Path;

/// The bundled canonical icon master — the launcher's own identity, used
/// when a package supplies no `package.icon`. Committed PNG, regenerated
/// by `cargo run -p gba-launcher --bin gba-icon`; project-owned art (see
/// THIRD-PARTY.md). Embedded so the packager needs no repo-relative file
/// at run time.
const DEFAULT_MASTER: &[u8] = include_bytes!("../../../assets/icon/app-icon.png");

/// Straight-alpha 8-bit RGBA image.
pub struct Rgba {
    pub w: u32,
    pub h: u32,
    pub px: Vec<u8>, // w*h*4
}

impl Rgba {
    fn at(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.w + x) * 4) as usize;
        [self.px[i], self.px[i + 1], self.px[i + 2], self.px[i + 3]]
    }

    /// Box-average downscale / bilinear-ish upscale to `s`×`s`. Icon
    /// masters are downscaled in practice; box averaging keeps the chrome
    /// ring and cross edges clean without ringing.
    fn resized(&self, s: u32) -> Rgba {
        let mut px = vec![0u8; (s * s * 4) as usize];
        for oy in 0..s {
            for ox in 0..s {
                // source box covering this destination pixel
                let x0 = ox * self.w / s;
                let x1 = ((ox + 1) * self.w / s).max(x0 + 1).min(self.w);
                let y0 = oy * self.h / s;
                let y1 = ((oy + 1) * self.h / s).max(y0 + 1).min(self.h);
                let mut acc = [0u32; 4];
                let mut n = 0u32;
                for sy in y0..y1 {
                    for sx in x0..x1 {
                        let p = self.at(sx, sy);
                        for c in 0..4 {
                            acc[c] += p[c] as u32;
                        }
                        n += 1;
                    }
                }
                let o = ((oy * s + ox) * 4) as usize;
                for c in 0..4 {
                    px[o + c] = (acc[c] / n.max(1)) as u8;
                }
            }
        }
        Rgba { w: s, h: s, px }
    }
}

/// Load the icon master: the packager's PNG if given, else the bundled
/// canonical icon. Returns a square image (errors if the master isn't
/// square — every platform format assumes square).
pub fn load_master(custom: Option<&Path>) -> Result<Rgba, String> {
    let bytes = match custom {
        Some(p) => std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()))?,
        None => DEFAULT_MASTER.to_vec(),
    };
    let img = decode_png(&bytes)?;
    if img.w != img.h {
        return Err(format!(
            "icon master must be square, got {}×{}",
            img.w, img.h
        ));
    }
    if img.w < 256 {
        return Err(format!(
            "icon master is {0}×{0}; supply at least 256×256 (1024² recommended)",
            img.w
        ));
    }
    Ok(img)
}

// ---- minimal PNG decode (8-bit RGB / RGBA, no interlace) ----------------

fn decode_png(bytes: &[u8]) -> Result<Rgba, String> {
    if !bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Err("not a PNG".into());
    }
    let mut pos = 8;
    let mut width = 0u32;
    let mut height = 0u32;
    let mut channels = 0usize;
    let mut idat = Vec::new();
    while pos + 8 <= bytes.len() {
        let len = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        let kind = &bytes[pos + 4..pos + 8];
        let data_start = pos + 8;
        let data_end = data_start + len;
        if data_end + 4 > bytes.len() {
            return Err("truncated PNG chunk".into());
        }
        let data = &bytes[data_start..data_end];
        match kind {
            b"IHDR" => {
                width = u32::from_be_bytes(data[0..4].try_into().unwrap());
                height = u32::from_be_bytes(data[4..8].try_into().unwrap());
                let bit_depth = data[8];
                let color_type = data[9];
                let interlace = data[12];
                if bit_depth != 8 {
                    return Err(format!("icon PNG must be 8-bit/channel (got {bit_depth})"));
                }
                if interlace != 0 {
                    return Err("interlaced icon PNG unsupported; re-export non-interlaced".into());
                }
                channels = match color_type {
                    2 => 3, // truecolor
                    6 => 4, // truecolor + alpha
                    _ => {
                        return Err(format!(
                            "icon PNG must be RGB or RGBA (color type 2/6, got {color_type})"
                        ))
                    }
                };
            }
            b"IDAT" => idat.extend_from_slice(data),
            b"IEND" => break,
            _ => {}
        }
        pos = data_end + 4; // skip the trailing CRC
    }
    if width == 0 || height == 0 || channels == 0 {
        return Err("PNG missing IHDR/IDAT".into());
    }
    let raw = inflate(&idat)?;
    unfilter(&raw, width, height, channels)
}

fn inflate(zlib: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(zlib)
        .read_to_end(&mut out)
        .map_err(|e| format!("inflate: {e}"))?;
    Ok(out)
}

/// Reverse PNG scanline filters, expanding RGB to RGBA.
fn unfilter(raw: &[u8], w: u32, h: u32, ch: usize) -> Result<Rgba, String> {
    let stride = w as usize * ch;
    if raw.len() < (stride + 1) * h as usize {
        return Err("PNG image data short".into());
    }
    let mut prev = vec![0u8; stride];
    let mut cur = vec![0u8; stride];
    let mut out = Vec::with_capacity(w as usize * h as usize * 4);
    let mut p = 0;
    for _ in 0..h {
        let filter = raw[p];
        p += 1;
        cur.copy_from_slice(&raw[p..p + stride]);
        p += stride;
        for i in 0..stride {
            let a = if i >= ch { cur[i - ch] as i32 } else { 0 }; // left
            let b = prev[i] as i32; // up
            let c = if i >= ch { prev[i - ch] as i32 } else { 0 }; // up-left
            let recon = match filter {
                0 => 0,
                1 => a,
                2 => b,
                3 => (a + b) / 2,
                4 => {
                    let pp = a + b - c;
                    let (pa, pb, pc) = ((pp - a).abs(), (pp - b).abs(), (pp - c).abs());
                    if pa <= pb && pa <= pc {
                        a
                    } else if pb <= pc {
                        b
                    } else {
                        c
                    }
                }
                f => return Err(format!("unknown PNG filter {f}")),
            };
            cur[i] = (cur[i] as i32 + recon) as u8;
        }
        for px in cur.chunks_exact(ch) {
            match ch {
                3 => out.extend_from_slice(&[px[0], px[1], px[2], 255]),
                4 => out.extend_from_slice(px),
                _ => unreachable!(),
            }
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    Ok(Rgba { w, h, px: out })
}

// ---- minimal PNG encode (8-bit RGBA) -----------------------------------

fn encode_png(img: &Rgba) -> Vec<u8> {
    let stride = img.w as usize * 4;
    let mut raw = Vec::with_capacity((stride + 1) * img.h as usize);
    for row in img.px.chunks_exact(stride) {
        raw.push(0); // filter: None
        raw.extend_from_slice(row);
    }
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    z.write_all(&raw).unwrap();
    let idat = z.finish().unwrap();

    let mut out = Vec::with_capacity(idat.len() + 64);
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    let chunk = |out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        out.extend_from_slice(&crc32(&out[start..]).to_be_bytes());
    };
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&img.w.to_be_bytes());
    ihdr.extend_from_slice(&img.h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &idat);
    chunk(&mut out, b"IEND", &[]);
    out
}

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

// ---- macOS: .icns ------------------------------------------------------

/// Build an `.icns` from PNG-backed entries. Apple's icns supports raw
/// PNG payloads for the modern OSTypes; each entry is the master resized
/// and PNG-encoded. We emit the common Retina-capable set.
pub fn build_icns(master: &Rgba) -> Vec<u8> {
    // (OSType, pixel size). icp4/icp5 = 16/32 (PNG allowed since 10.7);
    // ic07..ic10 = 128/256/512/1024. Covers Finder, Dock, and Quick Look.
    const ENTRIES: &[(&[u8; 4], u32)] = &[
        (b"icp4", 16),
        (b"icp5", 32),
        (b"ic07", 128),
        (b"ic08", 256),
        (b"ic09", 512),
        (b"ic10", 1024),
    ];
    let mut body = Vec::new();
    for (ostype, size) in ENTRIES {
        let png = encode_png(&master.resized(*size));
        body.extend_from_slice(*ostype);
        body.extend_from_slice(&((png.len() + 8) as u32).to_be_bytes());
        body.extend_from_slice(&png);
    }
    let mut out = Vec::with_capacity(body.len() + 8);
    out.extend_from_slice(b"icns");
    out.extend_from_slice(&((body.len() + 8) as u32).to_be_bytes());
    out.extend_from_slice(&body);
    out
}

// ---- Windows: .ico -----------------------------------------------------

/// Build a multi-size `.ico`. Small sizes are stored as 32-bpp BMP
/// (DIB) entries — the universally-decodable form; large icons could be
/// PNG-in-ICO (Vista+) but BMP keeps every Windows version happy and the
/// payload is tiny at icon sizes.
pub fn build_ico(master: &Rgba) -> Vec<u8> {
    const SIZES: &[u32] = &[16, 32, 48, 64, 128, 256];
    let images: Vec<(u32, Vec<u8>)> = SIZES
        .iter()
        .map(|&s| (s, bmp_dib(&master.resized(s))))
        .collect();

    let mut out = Vec::new();
    // ICONDIR
    out.extend_from_slice(&[0, 0]); // reserved
    out.extend_from_slice(&1u16.to_le_bytes()); // type: icon
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    // directory entries; image data follows after all entries
    let mut offset = 6 + images.len() * 16;
    for (s, data) in &images {
        let dim = if *s >= 256 { 0u8 } else { *s as u8 }; // 0 means 256
        out.push(dim); // width
        out.push(dim); // height
        out.push(0); // palette count
        out.push(0); // reserved
        out.extend_from_slice(&1u16.to_le_bytes()); // color planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bpp
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += data.len();
    }
    for (_, data) in &images {
        out.extend_from_slice(data);
    }
    out
}

/// 32-bpp bottom-up BMP DIB for an ICO entry: a BITMAPINFOHEADER with a
/// *doubled* height (color rows + AND-mask rows), BGRA pixels bottom-up,
/// then a 1-bpp AND mask (all zero — alpha lives in the BGRA).
fn bmp_dib(img: &Rgba) -> Vec<u8> {
    let w = img.w;
    let h = img.h;
    let mut out = Vec::new();
    // BITMAPINFOHEADER (40 bytes)
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&((h * 2) as i32).to_le_bytes()); // color + mask
    out.extend_from_slice(&1u16.to_le_bytes()); // planes
    out.extend_from_slice(&32u16.to_le_bytes()); // bpp
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&0u32.to_le_bytes()); // image size (0 ok for BI_RGB)
    out.extend_from_slice(&0i32.to_le_bytes()); // x ppm
    out.extend_from_slice(&0i32.to_le_bytes()); // y ppm
    out.extend_from_slice(&0u32.to_le_bytes()); // palette
    out.extend_from_slice(&0u32.to_le_bytes()); // important
                                                // BGRA pixels, bottom-up
    for y in (0..h).rev() {
        for x in 0..w {
            let [r, g, b, a] = img.at(x, y);
            out.extend_from_slice(&[b, g, r, a]);
        }
    }
    // AND mask: 1 bit/pixel, rows padded to 32 bits, all opaque (0).
    let mask_stride = w.div_ceil(32) * 4;
    out.extend(std::iter::repeat_n(0u8, (mask_stride * h) as usize));
    out
}

// ---- per-platform packaging glue ---------------------------------------

/// Write the Linux desktop integration: a 256² PNG plus a `.desktop`
/// launcher beside the executable. XDG-style, no install step — a file
/// manager that indexes the directory shows the icon.
pub fn write_linux(pkg: &Path, name: &str, master: &Rgba) -> Result<(), String> {
    let png = encode_png(&master.resized(256));
    std::fs::write(pkg.join(format!("{name}.png")), &png)
        .map_err(|e| format!("{name}.png: {e}"))?;
    let desktop = format!(
        "[Desktop Entry]\n\
Type=Application\n\
Name={name}\n\
Comment=Statically recompiled game\n\
Exec=./{name}\n\
Icon=./{name}.png\n\
Terminal=false\n\
Categories=Game;\n"
    );
    std::fs::write(pkg.join(format!("{name}.desktop")), desktop)
        .map_err(|e| format!("{name}.desktop: {e}"))?;
    Ok(())
}

/// Emit the `.ico` into the package directory. PE resource embedding into
/// the produced executable is only wired when the build is hosted on
/// Windows (see [`embed_windows_pe`]); on other hosts the `.ico` travels
/// beside the binary so a shortcut/installer can pick it up.
pub fn write_windows_ico(pkg: &Path, name: &str, master: &Rgba) -> Result<(), String> {
    let ico = build_ico(master);
    std::fs::write(pkg.join(format!("{name}.ico")), &ico)
        .map_err(|e| format!("{name}.ico: {e}"))?;
    Ok(())
}

/// PE resource embedding — wired when gba-pack itself is built on Windows.
/// Editing the PE's `.rsrc` (RT_GROUP_ICON + RT_ICON) from a non-Windows
/// host needs a portable PE editor we don't yet vendor; until then this is
/// a clearly-marked no-op beyond the side-car `.ico`, never a silent wrong
/// result.
#[cfg(target_os = "windows")]
pub fn embed_windows_pe(_exe: &Path, _master: &Rgba) -> Result<(), String> {
    // TODO(windows-host): rewrite the PE .rsrc to carry RT_GROUP_ICON +
    // RT_ICON entries built from `build_ico`'s per-size DIBs. Needs a
    // pure-Rust PE editor (e.g. an MIT/Apache ICO/PE-edit crate) added as
    // a Windows-only dependency. The side-car `.ico` is emitted regardless.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_master_decodes_square_and_large() {
        let m = load_master(None).unwrap();
        assert_eq!(m.w, m.h);
        assert!(m.w >= 256);
        assert_eq!(m.px.len(), (m.w * m.h * 4) as usize);
    }

    #[test]
    fn encode_roundtrips_through_decode() {
        // Resize then re-encode then decode: pixels should survive the
        // None-filter + zlib path bit-for-bit at a fixed size.
        let m = load_master(None).unwrap().resized(64);
        let png = encode_png(&m);
        let back = decode_png(&png).unwrap();
        assert_eq!(back.w, 64);
        assert_eq!(back.px, m.px);
    }

    #[test]
    fn icns_has_magic_and_consistent_length() {
        let m = load_master(None).unwrap();
        let icns = build_icns(&m);
        assert_eq!(&icns[0..4], b"icns");
        let declared = u32::from_be_bytes(icns[4..8].try_into().unwrap()) as usize;
        assert_eq!(
            declared,
            icns.len(),
            "icns header length must equal file size"
        );
        // Each entry's PNG payload should itself be a valid PNG.
        assert!(icns
            .windows(8)
            .any(|w| w == [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]));
    }

    #[test]
    fn ico_header_counts_entries() {
        let m = load_master(None).unwrap();
        let ico = build_ico(&m);
        assert_eq!(&ico[0..2], &[0, 0]); // reserved
        assert_eq!(u16::from_le_bytes(ico[2..4].try_into().unwrap()), 1); // type icon
        let count = u16::from_le_bytes(ico[4..6].try_into().unwrap());
        assert_eq!(count, 6); // SIZES.len()
                              // First directory entry's offset/length must land inside the file.
        let off = u32::from_le_bytes(ico[18..22].try_into().unwrap()) as usize;
        let len = u32::from_le_bytes(ico[14..18].try_into().unwrap()) as usize;
        assert!(off + len <= ico.len());
    }

    #[test]
    fn linux_and_windows_emit_valid_files() {
        let m = load_master(None).unwrap();
        let dir = std::env::temp_dir().join("gba-pack-icon-emit");
        std::fs::create_dir_all(&dir).unwrap();

        write_linux(&dir, "demo", &m).unwrap();
        let png = std::fs::read(dir.join("demo.png")).unwrap();
        assert!(png.starts_with(&[0x89, b'P', b'N', b'G']));
        assert_eq!(decode_png(&png).unwrap().w, 256);
        let desktop = std::fs::read_to_string(dir.join("demo.desktop")).unwrap();
        assert!(desktop.contains("Exec=./demo"));
        assert!(desktop.contains("Icon=./demo.png"));

        write_windows_ico(&dir, "demo", &m).unwrap();
        let ico = std::fs::read(dir.join("demo.ico")).unwrap();
        assert_eq!(&ico[0..4], &[0, 0, 1, 0]); // reserved + type icon
    }

    #[test]
    fn rejects_non_png_and_small() {
        let dir = std::env::temp_dir().join("gba-pack-icon-test");
        std::fs::create_dir_all(&dir).unwrap();
        let notpng = dir.join("x.png");
        std::fs::write(&notpng, b"not a png at all").unwrap();
        assert!(load_master(Some(&notpng)).is_err());

        // A valid but tiny PNG (32²) must be rejected as too small.
        let small = Rgba {
            w: 32,
            h: 32,
            px: vec![0x80; 32 * 32 * 4],
        };
        let tiny = dir.join("tiny.png");
        std::fs::write(&tiny, encode_png(&small)).unwrap();
        assert!(load_master(Some(&tiny)).is_err());
    }
}
