//! Sidecar label files: code entry points discovered at runtime, fed
//! back into the analyzer so translation coverage doesn't depend on
//! what a single profiling boot happened to reach.
//!
//! Format — text, line-based, union-mergeable, content-free (addresses
//! and hashes only; never image bytes):
//!
//!     gba-labels v1
//!     rom-sha256 <64 hex>
//!     rom <hexaddr> a|t
//!     iwram <hexaddr> a|t ...
//!     ewram <hexaddr> a|t ...
//!
//! `rom` records become analyzer seeds. They are hints, not trusted
//! input: the translation derives from the actual ROM bytes, so a wrong
//! label can at worst translate code that is never jumped to. `iwram`/
//! `ewram` records are reserved for content-guarded RAM translation
//! (recorded now so files don't need regenerating when that lands);
//! the build counts and skips them.
//!
//! Lookup order: `<rom>.labels` next to the image (portable, shareable),
//! then `<config>/gba-recomp/labels/<sha256>.labels` (the recorder's
//! accumulator). Both are loaded and unioned.

use std::collections::BTreeSet;
use std::path::PathBuf;

#[derive(Default)]
pub struct Labels {
    /// ROM entry points, `guest address | thumb bit`.
    pub rom: BTreeSet<u32>,
    /// IWRAM entry points, `guest address | thumb bit`. Translatable
    /// when a local content snapshot exists (see [`Blob`]); the record
    /// itself stays portable presence data.
    pub iwram: BTreeSet<u32>,
    /// Reserved (ewram) record lines, preserved verbatim so a rewrite
    /// never drops forward-compatibility data.
    pub reserved: Vec<String>,
    /// Malformed or out-of-range lines encountered while loading.
    pub skipped: usize,
}

/// Local IWRAM content snapshot backing the image's `iwram` labels:
/// the 32 KB image plus a per-byte validity mask, accumulated by the
/// recorder at the moment each new IWRAM entry point is discovered.
/// Machine-local (it contains image-derived bytes) — never shared, and
/// never part of the portable label file.
pub struct Blob {
    pub img: Vec<u8>,
    pub mask: Vec<u8>,
}

const BLOB_MAGIC: &[u8] = b"gba-iwram v1\n";
pub const IWRAM_LEN: usize = 0x8000;

impl Blob {
    pub fn new() -> Blob {
        Blob { img: vec![0; IWRAM_LEN], mask: vec![0; IWRAM_LEN] }
    }

    pub fn load(path: &std::path::Path) -> Option<Blob> {
        let data = std::fs::read(path).ok()?;
        let body = data.strip_prefix(BLOB_MAGIC)?;
        if body.len() != 2 * IWRAM_LEN {
            return None;
        }
        Some(Blob { img: body[..IWRAM_LEN].to_vec(), mask: body[IWRAM_LEN..].to_vec() })
    }

    pub fn save(&self, path: &std::path::Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let mut data = Vec::with_capacity(BLOB_MAGIC.len() + 2 * IWRAM_LEN);
        data.extend_from_slice(BLOB_MAGIC);
        data.extend_from_slice(&self.img);
        data.extend_from_slice(&self.mask);
        std::fs::write(path, data).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn valid_at(&self, key: u32) -> bool {
        self.mask[(key & !1) as usize & (IWRAM_LEN - 1)] != 0
    }

    pub fn valid_bytes(&self) -> usize {
        self.mask.iter().filter(|&&m| m != 0).count()
    }
}

/// The blob path for an image (next to its label accumulator).
pub fn blob_path(sha: &str) -> PathBuf {
    config_path(sha).with_extension("iwram")
}

/// Sanity cap: far above any real title, far below a runaway recorder.
const MAX_LABELS: usize = 65536;

impl Labels {
    /// Parse one file, validating ROM records against the image size.
    /// A wrong-sha file is an error (labels are per-image); malformed
    /// lines are counted and skipped, never fatal.
    pub fn load(path: &std::path::Path, sha: &str, rom_len: usize) -> Result<Labels, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut lines = text.lines();
        if lines.next().map(str::trim) != Some("gba-labels v1") {
            return Err(format!("{}: not a gba-labels v1 file", path.display()));
        }
        match lines.next().map(str::trim).and_then(|l| l.strip_prefix("rom-sha256 ")) {
            Some(s) if s == sha => {}
            Some(s) => {
                return Err(format!(
                    "{}: labels are for image {}…, not {}…",
                    path.display(),
                    &s[..8.min(s.len())],
                    &sha[..8]
                ))
            }
            None => return Err(format!("{}: missing rom-sha256 header", path.display())),
        }
        let mut out = Labels::default();
        for line in lines {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut f = line.split_whitespace();
            let (region, addr, mode) = (f.next(), f.next(), f.next());
            let parsed = (|| {
                let addr = u32::from_str_radix(addr?, 16).ok()?;
                let thumb = match mode? {
                    "a" => 0u32,
                    "t" => 1,
                    _ => return None,
                };
                Some((region?, addr, thumb))
            })();
            match parsed {
                Some(("rom", addr, thumb))
                    if (0x08..=0x0D).contains(&(addr >> 24))
                        && ((addr & 0x01FF_FFFF) as usize) < rom_len
                        && addr & 1 == 0
                        && out.rom.len() < MAX_LABELS =>
                {
                    out.rom.insert(addr | thumb);
                }
                Some(("iwram", addr, thumb))
                    if addr >> 24 == 3 && addr & 1 == 0 && out.iwram.len() < MAX_LABELS =>
                {
                    out.iwram.insert(addr | thumb);
                }
                Some(("ewram", ..)) => out.reserved.push(line.to_string()),
                _ => out.skipped += 1,
            }
        }
        Ok(out)
    }

    /// Union another set into this one.
    pub fn merge(&mut self, other: Labels) {
        self.rom.extend(other.rom);
        self.iwram.extend(other.iwram);
        for l in other.reserved {
            if !self.reserved.contains(&l) {
                self.reserved.push(l);
            }
        }
        self.skipped += other.skipped;
    }

    pub fn save(&self, path: &std::path::Path, sha: &str) -> Result<(), String> {
        use std::fmt::Write;
        let mut s = String::with_capacity(32 + self.rom.len() * 12);
        let _ = writeln!(s, "gba-labels v1");
        let _ = writeln!(s, "rom-sha256 {sha}");
        for &key in &self.rom {
            let _ = writeln!(s, "rom {:08x} {}", key & !1, if key & 1 != 0 { "t" } else { "a" });
        }
        for &key in &self.iwram {
            let _ = writeln!(s, "iwram {:08x} {}", key & !1, if key & 1 != 0 { "t" } else { "a" });
        }
        for l in &self.reserved {
            let _ = writeln!(s, "{l}");
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        std::fs::write(path, s).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Stable content digest — participates in the translation cache
    /// key so a grown label set retranslates automatically. Includes
    /// the local IWRAM snapshot when one backs the iwram records,
    /// since translated output depends on its bytes.
    pub fn digest(&self, blob: Option<&Blob>) -> u64 {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        let mut eat = |b: u8| {
            h ^= b as u64;
            h = h.wrapping_mul(0x1_0000_01b3);
        };
        for &key in self.rom.iter().chain(&self.iwram) {
            key.to_le_bytes().into_iter().for_each(&mut eat);
        }
        if let Some(b) = blob {
            b.img.iter().copied().for_each(&mut eat);
            b.mask.iter().copied().for_each(&mut eat);
        }
        h
    }

    pub fn is_empty(&self) -> bool {
        self.rom.is_empty() && self.iwram.is_empty()
    }
}

/// The recorder's accumulator path for an image.
pub fn config_path(sha: &str) -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("gba-recomp")
        .join("labels")
        .join(format!("{sha}.labels"))
}

/// Load and union every label source for an image. Errors in one file
/// (wrong sha, bad header) are reported and that file ignored.
pub fn load_all(rom_path: &str, sha: &str, rom_len: usize) -> Labels {
    let mut out = Labels::default();
    let beside = PathBuf::from(format!("{}.labels", rom_path.trim_end_matches(".gba")));
    for p in [beside, config_path(sha)] {
        if !p.is_file() {
            continue;
        }
        match Labels::load(&p, sha, rom_len) {
            Ok(l) => {
                eprintln!(
                    "labels: {} rom + {} iwram entries from {}{}",
                    l.rom.len(),
                    l.iwram.len(),
                    p.display(),
                    if l.reserved.is_empty() {
                        String::new()
                    } else {
                        format!(" (+{} reserved records)", l.reserved.len())
                    }
                );
                out.merge(l);
            }
            Err(e) => eprintln!("labels: ignoring {e}"),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_validate_and_merge() {
        let sha = "ab".repeat(32);
        let dir = std::env::temp_dir().join("gba-labels-test");
        let p = dir.join("t.labels");
        let mut l = Labels::default();
        l.rom.insert(0x0800_1234);
        l.rom.insert(0x0800_2001); // thumb
        l.iwram.insert(0x0300_1091); // thumb
        l.reserved.push("ewram 02000420 a".to_string());
        l.save(&p, &sha).unwrap();
        let back = Labels::load(&p, &sha, 0x10000).unwrap();
        assert_eq!(back.rom, l.rom);
        assert_eq!(back.iwram, l.iwram);
        assert_eq!(back.reserved, l.reserved);
        assert_eq!(back.skipped, 0);
        // Out-of-range and malformed lines skip; wrong sha refuses.
        std::fs::write(
            &p,
            format!("gba-labels v1\nrom-sha256 {sha}\nrom 08ffffff a\nrom zz a\nbogus\n"),
        )
        .unwrap();
        let bad = Labels::load(&p, &sha, 0x10000).unwrap();
        assert!(bad.rom.is_empty());
        assert_eq!(bad.skipped, 3);
        assert!(Labels::load(&p, &"cd".repeat(32), 0x10000).is_err());
        // Merge unions and dedups.
        let mut a = Labels::default();
        a.rom.insert(0x0800_0010);
        let mut b = Labels::default();
        b.rom.insert(0x0800_0010);
        b.rom.insert(0x0800_0021);
        let d0 = a.digest(None);
        a.merge(b);
        assert_eq!(a.rom.len(), 2);
        assert_ne!(a.digest(None), d0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn blob_roundtrip_and_digest() {
        let dir = std::env::temp_dir().join("gba-labels-blob-test");
        let p = dir.join("t.iwram");
        let mut blob = Blob::new();
        blob.img[0x1090] = 0xAB;
        blob.mask[0x1090] = 1;
        blob.save(&p).unwrap();
        let back = Blob::load(&p).unwrap();
        assert_eq!(back.img[0x1090], 0xAB);
        assert!(back.valid_at(0x0300_1091));
        assert!(!back.valid_at(0x0300_1093));
        assert_eq!(back.valid_bytes(), 1);
        let l = Labels::default();
        assert_ne!(l.digest(Some(&back)), l.digest(None));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
