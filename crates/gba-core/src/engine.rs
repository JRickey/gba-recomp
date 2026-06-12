//! Audio-engine classification: which sound middleware an image links.
//!
//! Most commercial images use one of a handful of audio engines, and
//! linked library regions are bit-stable across titles — so byte
//! signatures classify reliably. Classification drives which HLE shadow can
//! arm (M4A today, GAX next) and is surfaced as a diagnostic either
//! way.

use crate::mp2k::{self, Mp2kSig};

#[derive(Debug, Clone, PartialEq)]
pub enum Engine {
    /// SDK MusicPlayer2000 family — HLE shadow available.
    M4a(Vec<Mp2kSig>),
    /// GAX lineage; version from the banner when present (v2/v3),
    /// `None` for the bannerless v1-era builds.
    Gax(Option<String>),
    /// Krawall XM/S3M tracker middleware.
    Krawall,
    /// A UK studio's in-house driver (ships "AUDIO ERROR" strings).
    Rdiag,
    /// MusyX (string-tagged).
    MusyX,
    Unknown,
}

impl Engine {
    pub fn describe(&self) -> String {
        match self {
            Engine::M4a(sigs) => format!("M4A/MP2K ({} entry)", sigs.len()),
            Engine::Gax(Some(v)) => format!("GAX {v}"),
            Engine::Gax(None) => "GAX (early, bannerless)".into(),
            Engine::Krawall => "Krawall".into(),
            Engine::Rdiag => "in-house (R)".into(),
            Engine::MusyX => "MusyX".into(),
            Engine::Unknown => "unknown".into(),
        }
    }
}

/// First-byte-skip substring search (memchr + verify) — the needles
/// are long enough that this is effectively linear. Shared by the
/// engine-HLE detectors (gax, rdrv).
pub(crate) fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    let first = needle[0];
    let mut i = from;
    while i + needle.len() <= hay.len() {
        match hay[i..].iter().position(|&b| b == first) {
            Some(off) => {
                i += off;
                if i + needle.len() > hay.len() {
                    return None;
                }
                if &hay[i..i + needle.len()] == needle {
                    return Some(i);
                }
                i += 1;
            }
            None => return None,
        }
    }
    None
}

/// GAX v1-era library-function prefixes (minted from a bit-matched
/// reference image; thumb code, position-independent over these bytes).
const GAX1_FUNCS: [&[u8]; 2] = [
    &[
        0x70, 0xB5, 0x05, 0x1C, 0x0E, 0x1C, 0x30, 0x68, 0x03, 0x21, 0x08, 0x40, 0x00, 0x28, 0x00,
        0xD0, 0xB4, 0xE0, 0x70, 0x68, 0x08, 0x40, 0x00, 0x28, 0x00, 0xD0, 0xAF, 0xE0, 0xB0, 0x68,
        0x08, 0x40,
    ],
    &[
        0xF0, 0xB5, 0x57, 0x46, 0x4E, 0x46, 0x45, 0x46, 0xE0, 0xB4, 0x81, 0xB0, 0x05, 0x1C, 0x89,
        0x46, 0x17, 0x1C, 0x98, 0x46, 0x19, 0x49, 0x0E, 0x68, 0x88, 0x22, 0x52, 0x00, 0xB0, 0x18,
        0x02, 0x68,
    ],
];

/// GAX driver constant-table prefixes (note-ratio, inverse-pitch, PSG
/// period, wave-volume LUTs). Individually near-generic tracker math;
/// the 4-way conjunction marks the lineage even in recompiled builds
/// whose function bodies differ.
const GAX_TABLES: [&[u8]; 4] = [
    &[
        0x00, 0x10, 0xF3, 0x10, 0xF5, 0x11, 0x06, 0x13, 0x28, 0x14, 0x5B, 0x15, 0xA0, 0x16, 0xF9,
        0x17,
    ],
    &[
        0x00, 0x10, 0x1A, 0x0F, 0x41, 0x0E, 0x74, 0x0D, 0xB2, 0x0C, 0xFC, 0x0B, 0x50, 0x0B, 0xAD,
        0x0A,
    ],
    &[0x7B, 0x07, 0x82, 0x07, 0x89, 0x07, 0x90, 0x07],
    &[
        0x00, 0x60, 0x60, 0x60, 0x40, 0x40, 0x40, 0x40, 0x80, 0x80, 0x80, 0x80, 0x20, 0x20, 0x20,
        0x20,
    ],
];

/// Classify the audio engine an image links.
pub fn classify(rom: &[u8]) -> Engine {
    // M4A first: it has the strongest signature and an HLE shadow.
    let sigs = mp2k::detect(rom);
    if !sigs.is_empty() {
        return Engine::M4a(sigs);
    }

    // GAX banner (v2/v3): version digits follow the engine name.
    if let Some(i) = find(rom, b"GAX Sound Engine", 0) {
        let tail = &rom[(i + 16).min(rom.len())..(i + 28).min(rom.len())];
        let ver: String = tail
            .iter()
            .map(|&b| b as char)
            .skip_while(|c| *c == ' ' || *c == 'v')
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        return Engine::Gax(if ver.is_empty() { None } else { Some(ver) });
    }
    // Bannerless GAX lineage: all four driver LUTs present.
    if GAX_TABLES.iter().all(|t| find(rom, t, 0).is_some()) {
        let _v1 = GAX1_FUNCS.iter().any(|f| find(rom, f, 0).is_some());
        return Engine::Gax(None);
    }

    // Krawall: CVS keyword qualified by library strings nearby.
    let mut j = find(rom, b"$Id: ", 0);
    while let Some(i) = j {
        let end = (i + 160).min(rom.len());
        let blob = &rom[i..end];
        if find(blob, b"Krawall", 0).is_some()
            || find(blob, b"version.h", 0).is_some()
            || find(blob, b"player.c,v", 0).is_some()
        {
            return Engine::Krawall;
        }
        j = find(rom, b"$Id: ", i + 5);
    }

    // One in-house driver ships its diagnostic strings verbatim.
    let mut hits = 0;
    let mut k = find(rom, b"AUDIO ERROR, ", 0);
    while let Some(i) = k {
        hits += 1;
        if hits >= 2 {
            return Engine::Rdiag;
        }
        k = find(rom, b"AUDIO ERROR, ", i + 13);
    }

    if find(rom, b"MusyX", 0).is_some() {
        return Engine::MusyX;
    }
    Engine::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_synthetic_markers() {
        let mut rom = vec![0u8; 0x4000];
        assert_eq!(classify(&rom), Engine::Unknown);

        rom[0x100..0x100 + 24].copy_from_slice(b"GAX Sound Engine v3.05A ");
        assert_eq!(classify(&rom), Engine::Gax(Some("3.05".into())));

        let mut rom2 = vec![0u8; 0x4000];
        rom2[0x200..0x200 + 29].copy_from_slice(b"$Id: Krawall something here  ");
        assert_eq!(classify(&rom2), Engine::Krawall);

        let mut rom3 = vec![0u8; 0x4000];
        rom3[0x80..0x8D].copy_from_slice(b"AUDIO ERROR, ");
        rom3[0x200..0x20D].copy_from_slice(b"AUDIO ERROR, ");
        assert_eq!(classify(&rom3), Engine::Rdiag);

        // Four-table conjunction (bannerless lineage).
        let mut rom4 = vec![0u8; 0x4000];
        let mut off = 0x300;
        for t in GAX_TABLES {
            rom4[off..off + t.len()].copy_from_slice(t);
            off += 0x100;
        }
        assert_eq!(classify(&rom4), Engine::Gax(None));
    }
}
