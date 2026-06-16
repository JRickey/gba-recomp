//! Sidecar label files: code entry points discovered at runtime or by
//! external disassembly, fed back into the analyzer so translation
//! coverage doesn't depend on what a single profiling boot reached.
//!
//! Two on-disk formats, detected by content (never by extension):
//!
//! **v2 — TOML, the interchange format.** What disassembly tooling
//! (Ghidra exports, automated mappers) emits and what `labels export`
//! writes. Addresses are TOML integers (hex literals encouraged) or
//! `"0x…"` strings; `name`/`end` are optional enrichment the
//! recompiler preserves (names feed C-source export; `end` is the
//! exclusive function end):
//!
//! ```toml
//! format = "gba-labels"
//! version = 2
//!
//! [image]
//! sha256 = "<64 hex>"
//!
//! [[functions]]
//! name = "AgbMain"        # optional
//! address = 0x0800_01c8
//! end = 0x0800_0220       # optional, exclusive
//! mode = "thumb"          # "arm" | "thumb" (or "a" | "t")
//! ```
//!
//! **v1 — line format, the runtime recorder's accumulator.** Minimal,
//! union-mergeable, addresses only (the recorder has no names to say):
//!
//! ```text
//! gba-labels v1
//! rom-sha256 <64 hex>
//! rom <hexaddr> a|t
//! iwram <hexaddr> a|t ...
//! ewram <hexaddr> a|t ...
//! ```
//!
//! Both are content-free with respect to the image (addresses and
//! hashes only; never image bytes). The memory region is derived from
//! the address (`0x08..=0x0D` rom, `0x03` iwram, `0x02` ewram —
//! reserved). `rom` records become analyzer seeds. They are hints, not
//! trusted input: the translation derives from the actual ROM bytes,
//! so a wrong label can at worst translate code that is never jumped
//! to. `iwram` records translate when a local content snapshot backs
//! them; `ewram` records are reserved (counted and skipped).
//!
//! Lookup order: `<rom>.labels.toml`, then `<rom>.labels` next to the
//! image (portable, shareable), then the recorder's v1 accumulator and
//! the import accumulator (`<config>/gba-recomp/labels/<sha256>
//! .labels[.toml]`). All are loaded and unioned.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

#[derive(Default)]
pub struct Labels {
    /// ROM entry points, `guest address | thumb bit`.
    pub rom: BTreeSet<u32>,
    /// IWRAM entry points, `guest address | thumb bit`. Translatable
    /// when a local content snapshot exists (see [`Blob`]); the record
    /// itself stays portable presence data.
    pub iwram: BTreeSet<u32>,
    /// Function names from v2 files, keyed like the entry sets.
    /// Enrichment only: never part of the translation cache digest
    /// (renames must not force retranslation), consumed by C-source
    /// export and diagnostics.
    pub names: BTreeMap<u32, String>,
    /// Exclusive function end addresses from v2 files, same keying.
    /// Enrichment only, like `names`.
    pub ends: BTreeMap<u32, u32>,
    /// Reserved (ewram) record lines in v1 form, preserved verbatim so
    /// a rewrite never drops forward-compatibility data.
    pub reserved: Vec<String>,
    /// Malformed or out-of-range entries encountered while loading.
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
        Blob {
            img: vec![0; IWRAM_LEN],
            mask: vec![0; IWRAM_LEN],
        }
    }

    pub fn load(path: &std::path::Path) -> Option<Blob> {
        let data = std::fs::read(path).ok()?;
        let body = data.strip_prefix(BLOB_MAGIC)?;
        if body.len() != 2 * IWRAM_LEN {
            return None;
        }
        Some(Blob {
            img: body[..IWRAM_LEN].to_vec(),
            mask: body[IWRAM_LEN..].to_vec(),
        })
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
    /// The format is detected from content: a `gba-labels v1` first
    /// line parses as v1, anything else as v2 TOML. A wrong-sha file
    /// is an error (labels are per-image); malformed entries are
    /// counted and skipped, never fatal.
    pub fn load(path: &std::path::Path, sha: &str, rom_len: usize) -> Result<Labels, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if text.lines().next().map(str::trim) == Some("gba-labels v1") {
            Self::load_v1(&text, path, sha, rom_len)
        } else {
            Self::load_toml(&text, path, sha, rom_len)
        }
    }

    fn load_v1(
        text: &str,
        path: &std::path::Path,
        sha: &str,
        rom_len: usize,
    ) -> Result<Labels, String> {
        let mut lines = text.lines();
        lines.next(); // the validated magic line
        match lines
            .next()
            .map(str::trim)
            .and_then(|l| l.strip_prefix("rom-sha256 "))
        {
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

    fn load_toml(
        text: &str,
        path: &std::path::Path,
        sha: &str,
        rom_len: usize,
    ) -> Result<Labels, String> {
        let doc: toml::Value = text.parse().map_err(|e| {
            format!(
                "{}: not a gba-labels file (no v1 magic; TOML: {e})",
                path.display()
            )
        })?;
        let t = doc
            .as_table()
            .filter(|t| t.get("format").and_then(toml::Value::as_str) == Some("gba-labels"))
            .ok_or_else(|| format!("{}: not a gba-labels file", path.display()))?;
        match t.get("version").and_then(toml::Value::as_integer) {
            Some(2) => {}
            v => {
                return Err(format!(
                    "{}: unsupported gba-labels version {}",
                    path.display(),
                    v.map_or("missing".into(), |v| v.to_string())
                ))
            }
        }
        let fsha = t
            .get("image")
            .and_then(|i| i.get("sha256"))
            .and_then(toml::Value::as_str)
            .ok_or_else(|| format!("{}: missing image.sha256", path.display()))?;
        if !fsha.eq_ignore_ascii_case(sha) {
            return Err(format!(
                "{}: labels are for image {}…, not {}…",
                path.display(),
                &fsha[..8.min(fsha.len())],
                &sha[..8]
            ));
        }
        let mut out = Labels::default();
        let empty = Vec::new();
        let fns = t
            .get("functions")
            .and_then(toml::Value::as_array)
            .unwrap_or(&empty);
        for f in fns {
            let addr = f.get("address").and_then(addr_value);
            let thumb = f
                .get("mode")
                .and_then(toml::Value::as_str)
                .and_then(|m| match m {
                    "arm" | "a" => Some(0u32),
                    "thumb" | "t" => Some(1),
                    _ => None,
                });
            let (Some(addr), Some(thumb)) = (addr, thumb) else {
                out.skipped += 1;
                continue;
            };
            let end = f.get("end").and_then(addr_value);
            let name = f.get("name").and_then(toml::Value::as_str);
            if !out.push_function(addr, thumb, end, name, rom_len) {
                out.skipped += 1;
            }
        }
        Ok(out)
    }

    /// Apply one v2 function record — address + `thumb` bit (0/1), plus an
    /// optional exclusive `end` and `name` — the unit both a TOML map and a
    /// gamedb row carry. Region, range, even-address and cap rules are
    /// identical across sources, so DB-driven and file-driven seeds match
    /// exactly. Returns false when the record is out of range or in an
    /// unmapped region (the caller counts it as skipped); ewram records are
    /// preserved as `reserved` and return true.
    pub fn push_function(
        &mut self,
        addr: u32,
        thumb: u32,
        end: Option<u32>,
        name: Option<&str>,
        rom_len: usize,
    ) -> bool {
        let key = addr | thumb;
        let accepted = match addr >> 24 {
            0x08..=0x0D
                if ((addr & 0x01FF_FFFF) as usize) < rom_len
                    && addr & 1 == 0
                    && self.rom.len() < MAX_LABELS =>
            {
                self.rom.insert(key);
                true
            }
            0x03 if addr & 1 == 0 && self.iwram.len() < MAX_LABELS => {
                self.iwram.insert(key);
                true
            }
            0x02 if addr & 1 == 0 => {
                // Reserved records keep their v1 spelling so the
                // accumulator round-trips them; enrichment fields
                // are dropped with the rest of ewram support.
                self.reserved.push(format!(
                    "ewram {addr:08x} {}",
                    if thumb != 0 { "t" } else { "a" }
                ));
                return true;
            }
            _ => false,
        };
        if !accepted {
            return false;
        }
        if let Some(name) = name {
            if !name.is_empty() {
                self.names.insert(key, name.to_string());
            }
        }
        if let Some(end) = end {
            if end > addr {
                self.ends.insert(key, end);
            }
        }
        true
    }

    /// Union another set into this one. Enrichment merges first-wins:
    /// the earlier-loaded source (the file beside the image) outranks
    /// later accumulators on name/end disagreement.
    pub fn merge(&mut self, other: Labels) {
        self.rom.extend(other.rom);
        self.iwram.extend(other.iwram);
        for (k, v) in other.names {
            self.names.entry(k).or_insert(v);
        }
        for (k, v) in other.ends {
            self.ends.entry(k).or_insert(v);
        }
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
            let _ = writeln!(
                s,
                "rom {:08x} {}",
                key & !1,
                if key & 1 != 0 { "t" } else { "a" }
            );
        }
        for &key in &self.iwram {
            let _ = writeln!(
                s,
                "iwram {:08x} {}",
                key & !1,
                if key & 1 != 0 { "t" } else { "a" }
            );
        }
        for l in &self.reserved {
            let _ = writeln!(s, "{l}");
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        std::fs::write(path, s).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Write the v2 TOML form (the interchange format): all entries,
    /// names and ends preserved, addresses as hex literals.
    pub fn save_toml(&self, path: &std::path::Path, sha: &str) -> Result<(), String> {
        use std::fmt::Write;
        let mut s = String::with_capacity(64 + (self.rom.len() + self.iwram.len()) * 48);
        let _ = writeln!(s, "format = \"gba-labels\"");
        let _ = writeln!(s, "version = 2");
        let _ = writeln!(s, "\n[image]");
        let _ = writeln!(s, "sha256 = \"{sha}\"");
        let emit = |s: &mut String, key: u32| {
            let _ = writeln!(s, "\n[[functions]]");
            if let Some(n) = self.names.get(&key) {
                let _ = writeln!(
                    s,
                    "name = \"{}\"",
                    n.replace('\\', "\\\\").replace('"', "\\\"")
                );
            }
            let _ = writeln!(s, "address = 0x{:08x}", key & !1);
            if let Some(&e) = self.ends.get(&key) {
                let _ = writeln!(s, "end = 0x{e:08x}");
            }
            let _ = writeln!(
                s,
                "mode = \"{}\"",
                if key & 1 != 0 { "thumb" } else { "arm" }
            );
        };
        for &key in self.rom.iter().chain(&self.iwram) {
            emit(&mut s, key);
        }
        for l in &self.reserved {
            let mut f = l.split_whitespace().skip(1);
            if let (Some(addr), Some(mode)) = (f.next(), f.next()) {
                let _ = writeln!(s, "\n[[functions]]");
                let _ = writeln!(s, "address = 0x{addr}");
                let _ = writeln!(
                    s,
                    "mode = \"{}\"",
                    if mode == "t" { "thumb" } else { "arm" }
                );
            }
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

/// A TOML address: an integer (hex literals encouraged) or a hex
/// string with optional `0x` prefix — Ghidra CSV exports say
/// `08001234`, hand-written files say `0x08001234`; accept both.
fn addr_value(v: &toml::Value) -> Option<u32> {
    match v {
        toml::Value::Integer(i) => u32::try_from(*i).ok(),
        toml::Value::String(s) => {
            let s = s.trim();
            let s = s
                .strip_prefix("0x")
                .or_else(|| s.strip_prefix("0X"))
                .unwrap_or(s);
            u32::from_str_radix(&s.replace('_', ""), 16).ok()
        }
        _ => None,
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

/// The import accumulator for v2 files: imported TOML merges here so
/// its names/ends survive (the v1 recorder accumulator can't carry
/// them).
pub fn config_toml_path(sha: &str) -> PathBuf {
    config_path(sha).with_extension("labels.toml")
}

/// Load and union every label source for an image. Errors in one file
/// (wrong sha, bad header) are reported and that file ignored.
pub fn load_all(rom_path: &str, sha: &str, rom_len: usize) -> Labels {
    load_all_with(rom_path, sha, rom_len, None)
}

/// Like [`load_all`], plus an explicit label file unioned first. The
/// explicit path is how packaging passes a map deterministically —
/// filename-adjacent discovery breaks when the image is a symlink or
/// renamed, so a packaged build never relies on it.
pub fn load_all_with(rom_path: &str, sha: &str, rom_len: usize, explicit: Option<&str>) -> Labels {
    let mut out = Labels::default();
    let stem = rom_path.trim_end_matches(".gba");
    let beside_toml = PathBuf::from(format!("{stem}.labels.toml"));
    let beside = PathBuf::from(format!("{stem}.labels"));
    let explicit = explicit.map(PathBuf::from);
    let mut sources: Vec<PathBuf> = Vec::new();
    if let Some(p) = explicit {
        sources.push(p);
    }
    sources.extend([beside_toml, beside, config_path(sha), config_toml_path(sha)]);
    // Same file reachable two ways (explicit == beside) loads once.
    sources.dedup_by(|a, b| a.canonicalize().ok() == b.canonicalize().ok() && a.exists());
    for p in sources {
        if !p.is_file() {
            continue;
        }
        match Labels::load(&p, sha, rom_len) {
            Ok(l) => {
                eprintln!(
                    "labels: {} rom + {} iwram entries from {}{}{}",
                    l.rom.len(),
                    l.iwram.len(),
                    p.display(),
                    if l.names.is_empty() {
                        String::new()
                    } else {
                        format!(" ({} named)", l.names.len())
                    },
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

/// Where a build's entry-point labels come from, keyed by ROM sha256.
/// Callers fold the returned [`Labels`] into analyzer seeds; whether they
/// originate from files beside the image, the shipped gamedb, or a
/// recorder is the source's concern. This is the seam that lets the
/// launcher's gamedb-driven build and the CLI's file-driven build share
/// one engine.
pub trait LabelSource {
    fn labels(&self, sha: &str, rom_len: usize) -> Result<Labels, String>;
}

/// The file-backed source: today's behavior — the union of
/// `<rom>.labels.toml`/`.labels` beside the image, the config-dir
/// accumulators, and an optional explicit map (see [`load_all_with`]).
pub struct FileLabels<'a> {
    pub rom_path: &'a str,
    pub explicit: Option<&'a str>,
}

impl LabelSource for FileLabels<'_> {
    fn labels(&self, sha: &str, rom_len: usize) -> Result<Labels, String> {
        Ok(load_all_with(self.rom_path, sha, rom_len, self.explicit))
    }
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
    fn toml_roundtrip_detection_and_enrichment() {
        let sha = "ab".repeat(32);
        let dir = std::env::temp_dir().join("gba-labels-toml-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.labels.toml");
        // Hand-written input: hex-int, underscore, and string addresses,
        // short mode spellings, names and ends, an ewram record.
        std::fs::write(
            &p,
            format!(
                r#"
format = "gba-labels"
version = 2

[image]
sha256 = "{sha}"

[[functions]]
name = "AgbMain"
address = 0x0800_1234
end = "0x08001268"
mode = "thumb"

[[functions]]
address = "08002000"
mode = "a"

[[functions]]
address = 0x02000420
mode = "arm"

[[functions]]
address = 0x09FFFFFE   # beyond rom_len -> skipped
mode = "arm"
"#
            ),
        )
        .unwrap();
        let l = Labels::load(&p, &sha, 0x10000).unwrap();
        assert_eq!(l.rom.len(), 2);
        assert!(l.rom.contains(&0x0800_1235)); // thumb bit folded in
        assert!(l.rom.contains(&0x0800_2000));
        assert_eq!(
            l.names.get(&0x0800_1235).map(String::as_str),
            Some("AgbMain")
        );
        assert_eq!(l.ends.get(&0x0800_1235), Some(&0x0800_1268));
        assert_eq!(l.reserved, vec!["ewram 02000420 a".to_string()]);
        assert_eq!(l.skipped, 1);
        // Round-trip through save_toml preserves everything.
        let p2 = dir.join("rt.labels.toml");
        l.save_toml(&p2, &sha).unwrap();
        let back = Labels::load(&p2, &sha, 0x10000).unwrap();
        assert_eq!(back.rom, l.rom);
        assert_eq!(back.names, l.names);
        assert_eq!(back.ends, l.ends);
        assert_eq!(back.reserved, l.reserved);
        // Wrong sha refuses; digest ignores enrichment (rename must
        // not retranslate).
        assert!(Labels::load(&p, &"cd".repeat(32), 0x10000).is_err());
        let mut stripped = Labels::default();
        stripped.rom = l.rom.clone();
        assert_eq!(stripped.digest(None), l.digest(None));
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
