//! gba-pack — turn a mapped image into a distributable recompiled
//! game binary. See docs/packaging.md for the design; this binary is
//! the validation/planning surface of it (the build steps land next).
//!
//! The product invariant the packager exists to uphold: a package
//! carries NO image or BIOS bytes. It pins both by SHA-256 and the
//! produced binary refuses to run until the end user supplies files
//! that hash to exactly those values.

mod config;

use config::{PackConfig, Platform};
use sha2::{Digest, Sha256};
use std::path::Path;

const USAGE: &str = "usage: gba-pack <pack.toml> [--rom FILE] [--bios FILE]
  --rom FILE     the packager's own image dump; verified against image.rom-sha256
  --bios FILE    the packager's own BIOS dump; verified against image.bios-sha256

Validates the package description and (with --rom/--bios) the inputs,
then prints the build plan. The build itself is not wired yet.";

fn sha256_file(path: &str) -> Result<String, String> {
    let data = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    Ok(Sha256::digest(&data).iter().map(|b| format!("{b:02x}")).collect())
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut cfg_path = None;
    let mut rom = None;
    let mut bios = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--rom" => rom = Some(it.next().ok_or("--rom needs a value")?.clone()),
            "--bios" => bios = Some(it.next().ok_or("--bios needs a value")?.clone()),
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            other if cfg_path.is_none() => cfg_path = Some(other.to_string()),
            other => return Err(format!("unexpected argument {other:?}\n{USAGE}")),
        }
    }
    let cfg_path = cfg_path.ok_or(USAGE)?;
    let cfg = PackConfig::load(Path::new(&cfg_path))?;

    println!("package  {} v{}", cfg.package.name, cfg.package.version);
    println!(
        "targets  {}",
        cfg.package
            .platforms
            .iter()
            .map(|p| match p {
                Platform::Macos => "macos",
                Platform::Windows => "windows",
                Platform::Linux => "linux",
            })
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!("image    rom  {}…", &cfg.image.rom_sha256[..16]);
    println!("         bios {}…", &cfg.image.bios_sha256[..16]);

    // The packager's own dumps, when offered, must hash to the pins —
    // a package built from the wrong revision is broken at birth.
    if let Some(p) = &rom {
        let got = sha256_file(p)?;
        if got != cfg.image.rom_sha256 {
            return Err(format!("{p}: sha256 {got} does not match image.rom-sha256"));
        }
        println!("rom      {p} (verified)");
    }
    if let Some(p) = &bios {
        let got = sha256_file(p)?;
        if got != cfg.image.bios_sha256 {
            return Err(format!("{p}: sha256 {got} does not match image.bios-sha256"));
        }
        println!("bios     {p} (verified)");
    }

    match &cfg.labels.file {
        Some(f) if f.is_file() => println!("labels   {}", f.display()),
        Some(f) => return Err(format!("labels.file not found: {}", f.display())),
        None => println!(
            "labels   none — coverage will be profile+static only; a mapper-grade \
label set is strongly recommended for a packaged release"
        ),
    }

    println!(
        "runtime  menu={} enhanced-audio={} screen-sim={} engine-hle={:?}",
        cfg.runtime.menu, cfg.runtime.enhanced_audio, cfg.runtime.screen_sim, cfg.runtime.engine_hle
    );
    println!(
        "output   binary={} c-source={}",
        cfg.output.binary, cfg.output.c_source
    );
    println!();
    println!("plan validated. build pipeline not wired yet — next steps:");
    println!("  1. translate (recomp build with the label set, full-coverage report)");
    println!("  2. emit runtime crate with pinned SHAs + selected modules");
    println!("  3. per-platform link{}", if cfg.output.c_source { " + C source tree export" } else { "" });
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("gba-pack: {e}");
        std::process::exit(1);
    }
}
