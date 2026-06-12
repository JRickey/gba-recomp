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
       gba-pack build <pack.toml> --rom FILE --bios FILE [--out DIR]
                      [--recomp PATH] [--soak FRAMES]

Plan mode validates the package description and (with --rom/--bios) the
inputs, then prints the build plan.

Build mode produces the distributable package directory: translates the
image (recomp build --ram, labels auto-loaded), soak-tests the result,
and assembles <out>/<name>/ — the runtime binary, the translation, and
the recomp.pack.toml manifest pinning ROM and BIOS by SHA-256. With
[runtime] interpreter = false the build refuses to ship unless the soak
ran ZERO interpreter-fallback steps (recording and rebuilding once to
close coverage gaps automatically).";

fn sha256_file(path: &str) -> Result<String, String> {
    let data = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    Ok(Sha256::digest(&data).iter().map(|b| format!("{b:02x}")).collect())
}

/// Resolve the recomp binary: explicit flag, env, sibling, PATH.
/// Resolved paths are canonicalized — recomp runs from a working
/// directory, so a relative path would silently stop resolving.
fn find_recomp(explicit: Option<&str>) -> Result<std::path::PathBuf, String> {
    if let Some(p) = explicit {
        return std::fs::canonicalize(p).map_err(|e| format!("{p}: {e}"));
    }
    if let Some(p) = std::env::var_os("GBA_RECOMP_BIN") {
        if let Ok(p) = std::fs::canonicalize(&p) {
            return Ok(p);
        }
    }
    if let Ok(me) = std::env::current_exe() {
        let sib = me.with_file_name("recomp");
        if sib.is_file() {
            return Ok(sib);
        }
    }
    Ok(std::path::PathBuf::from("recomp")) // hope for PATH; spawn errors say so
}

/// Run recomp with args in `cwd`, streaming nothing; returns stdout.
fn recomp_run(
    bin: &Path,
    cwd: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
) -> Result<String, String> {
    let mut c = std::process::Command::new(bin);
    c.args(args).current_dir(cwd);
    for (k, v) in envs {
        c.env(k, v);
    }
    let out = c.output().map_err(|e| format!("{}: {e}", bin.display()))?;
    if !out.status.success() {
        return Err(format!(
            "recomp {} failed:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    // Relay the child's report lines — the packager should see what
    // the translation actually did (label counts, block counts,
    // seeding mode), not just our phase banners.
    for line in stdout.lines() {
        let line = line.trim_end();
        if line.starts_with("labels:") || line.starts_with("blocks:")
            || line.starts_with("bios:") || line.starts_with("profiled ")
            || line.contains("fallback_steps")
        {
            println!("      | {line}");
        }
    }
    Ok(stdout)
}

/// Parse `fallback_steps: N` from runc output.
fn fallback_steps(runc_out: &str) -> Result<u64, String> {
    runc_out
        .lines()
        .find_map(|l| l.split("fallback_steps: ").nth(1))
        .and_then(|s| s.trim().parse().ok())
        .ok_or_else(|| "runc output had no fallback_steps line".into())
}

fn cmd_build(args: &[String]) -> Result<(), String> {
    let mut cfg_path = None;
    let mut rom = None;
    let mut bios = None;
    let mut out_dir = std::path::PathBuf::from("pack-out");
    let mut recomp_flag = None;
    let mut soak = 3600u64;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--rom" => rom = Some(it.next().ok_or("--rom needs a value")?.clone()),
            "--bios" => bios = Some(it.next().ok_or("--bios needs a value")?.clone()),
            "--out" => out_dir = it.next().ok_or("--out needs a value")?.into(),
            "--recomp" => recomp_flag = Some(it.next().ok_or("--recomp needs a value")?.clone()),
            "--soak" => {
                soak = it.next().ok_or("--soak needs a value")?
                    .parse().map_err(|e| format!("bad soak: {e}"))?
            }
            other if cfg_path.is_none() => cfg_path = Some(other.to_string()),
            other => return Err(format!("unexpected argument {other:?}\n{USAGE}")),
        }
    }
    let cfg = PackConfig::load(Path::new(&cfg_path.ok_or(USAGE)?))?;
    let rom = rom.ok_or("build needs --rom (the packager's own dump)")?;
    let bios = bios.ok_or("build needs --bios (the packager's own dump)")?;
    let rom = std::fs::canonicalize(&rom).map_err(|e| format!("{rom}: {e}"))?;
    let bios = std::fs::canonicalize(&bios).map_err(|e| format!("{bios}: {e}"))?;
    for (p, want, what) in [
        (&rom, &cfg.image.rom_sha256, "image.rom-sha256"),
        (&bios, &cfg.image.bios_sha256, "image.bios-sha256"),
    ] {
        let got = sha256_file(p.to_str().ok_or("non-UTF8 path")?)?;
        if got != *want {
            return Err(format!("{}: sha256 {got} does not match {what}", p.display()));
        }
    }
    if let Some(f) = &cfg.labels.file {
        if !f.is_file() {
            return Err(format!("labels.file not found: {}", f.display()));
        }
    }
    let host = if cfg!(target_os = "macos") {
        Platform::Macos
    } else if cfg!(target_os = "windows") {
        Platform::Windows
    } else {
        Platform::Linux
    };
    for p in &cfg.package.platforms {
        if *p != host {
            eprintln!("note: {p:?} requested but cross-packaging isn't wired; building host only");
        }
    }

    let recomp = find_recomp(recomp_flag.as_deref())?;
    let work = out_dir.join("work");
    std::fs::create_dir_all(&work).map_err(|e| format!("{}: {e}", work.display()))?;
    let stem = rom.file_stem().and_then(|s| s.to_str()).ok_or("bad ROM name")?.to_string();
    // The package is BIOS-coherent by construction: the runtime boots
    // the end user's real BIOS (pin-enforced), so the translation is
    // built and soaked against the same image — an HLE translation
    // would have no native blocks for region 0 and a full recomp
    // would trap at the reset vector.
    let dylib = work.join(format!("out/{stem}-bios.dylib"));
    let rom_str = rom.to_str().ok_or("non-UTF8 rom path")?;
    let bios_str = bios.to_str().ok_or("non-UTF8 bios path")?;
    // Full recomp builds seed every decode point inside every bounded
    // function (the mapper's `end` records): computed jump-table
    // targets the soak never reaches still get native blocks. The
    // soak gate stays as the proof, this makes it pass for paths
    // nobody drove.
    let mut build_envs: Vec<(&str, &str)> = Vec::new();
    if cfg.output.c_source {
        build_envs.push(("RECOMP_KEEP_C", "1"));
    }
    if !cfg.runtime.interpreter {
        build_envs.push(("RECOMP_EXHAUSTIVE", "1"));
    }
    let keep_c: &[(&str, &str)] = &build_envs;
    // Pass the label map EXPLICITLY rather than relying on the
    // recompiler's beside-the-image discovery: the build runs against
    // the canonicalized (symlink-resolved) image path, so a map named
    // for the symlink would never be found. A full recomp's exhaustive
    // in-function seeding depends on the map's `end` records, so a
    // silently-missed map would ship a soak-only package.
    let labels_abs = match &cfg.labels.file {
        Some(f) => Some(
            std::fs::canonicalize(f)
                .map_err(|e| format!("{}: {e}", f.display()))?
                .to_str()
                .ok_or("non-UTF8 labels path")?
                .to_string(),
        ),
        None => {
            if !cfg.runtime.interpreter {
                return Err(
                    "[runtime] interpreter = false requires a [labels] file — a full \
recomp needs the function map's boundaries for exhaustive coverage"
                        .into(),
                );
            }
            None
        }
    };
    let mut build_args: Vec<&str> = vec!["build", rom_str, "--ram", "--bios", bios_str];
    if let Some(l) = &labels_abs {
        build_args.push("--labels");
        build_args.push(l);
    }

    println!("[1/3] translating (recomp build --ram --bios; labels auto-load beside the image)...");
    if !build_envs.is_empty() {
        println!("      build env: {}", build_envs.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(" "));
    }
    recomp_run(&recomp, &work, &build_args, keep_c)?;
    if !dylib.is_file() {
        return Err(format!("translation did not produce {}", dylib.display()));
    }

    println!("[2/3] soak: {soak} frames with fallback tracing...");
    let frames = soak.to_string();
    let soak_args = ["runc", rom_str, "--frames", &frames, "--bios", bios_str];
    let mut steps =
        fallback_steps(&recomp_run(&recomp, &work, &soak_args, &[("RECOMP_TRACE_FALLBACK", "1")])?)?;
    println!("      fallback_steps: {steps}");
    if !cfg.runtime.interpreter && steps > 0 {
        println!("      full recomp requires zero — recording the gaps and rebuilding once...");
        recomp_run(
            &recomp, &work,
            &["runc", rom_str, "--frames", &frames, "--bios", bios_str, "--record-labels"],
            &[],
        )?;
        recomp_run(&recomp, &work, &build_args, keep_c)?;
        steps = fallback_steps(
            &recomp_run(&recomp, &work, &soak_args, &[("RECOMP_TRACE_FALLBACK", "1")])?,
        )?;
        println!("      fallback_steps after rebuild: {steps}");
        if steps > 0 {
            return Err(format!(
                "full recomp gate failed: {steps} interpreter steps remain after a \
record-and-rebuild pass. Extend coverage (longer/recorded soak input, more labels) \
or set [runtime] interpreter = true.",
            ));
        }
    }

    println!("[3/3] assembling package...");
    let pkg = out_dir.join(&cfg.package.name);
    std::fs::create_dir_all(&pkg).map_err(|e| format!("{}: {e}", pkg.display()))?;
    let exe = pkg.join(&cfg.package.name);
    std::fs::copy(&recomp, &exe).map_err(|e| format!("{}: {e}", exe.display()))?;
    std::fs::copy(&dylib, pkg.join("translation.dylib")).map_err(|e| e.to_string())?;
    std::fs::write(
        pkg.join("recomp.pack.toml"),
        format!(
            "# Generated by gba-pack — pins only, no image content.\n\
[package]\nname = \"{}\"\n\n[image]\nrom-sha256 = \"{}\"\nbios-sha256 = \"{}\"\n\n\
[runtime]\ninterpreter = {}\n\n[translation]\nfile = \"translation.dylib\"\n",
            cfg.package.name, cfg.image.rom_sha256, cfg.image.bios_sha256,
            cfg.runtime.interpreter,
        ),
    )
    .map_err(|e| e.to_string())?;
    std::fs::write(
        pkg.join("README.txt"),
        format!(
            "{} — a statically recompiled game.\n\n\
This package contains NO game or BIOS data. To play, place your own dumps\n\
next to the executable:\n\n  1. your cartridge image (.gba), sha256 {}\n\
  2. your BIOS image as gba_bios.bin, sha256 {}\n\nThen run ./{}\n",
            cfg.package.name, cfg.image.rom_sha256, cfg.image.bios_sha256, cfg.package.name,
        ),
    )
    .map_err(|e| e.to_string())?;
    if cfg.output.c_source {
        let src = pkg.join("src-c");
        std::fs::create_dir_all(&src).map_err(|e| e.to_string())?;
        let mut n = 0;
        if let Ok(entries) = std::fs::read_dir(work.join("out")) {
            for e in entries.flatten() {
                if e.path().extension().is_some_and(|x| x == "c") {
                    std::fs::copy(e.path(), src.join(e.file_name()))
                        .map_err(|e| e.to_string())?;
                    n += 1;
                }
            }
        }
        println!("      C source: {n} translation units -> {}", src.display());
    }
    println!();
    println!("package ready: {}", pkg.display());
    println!(
        "  {} + translation.dylib + recomp.pack.toml (interpreter = {})",
        cfg.package.name, cfg.runtime.interpreter
    );
    println!("  end user supplies: ROM {}…, BIOS {}…",
        &cfg.image.rom_sha256[..12], &cfg.image.bios_sha256[..12]);
    Ok(())
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("build") {
        return cmd_build(&args[1..]);
    }
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
