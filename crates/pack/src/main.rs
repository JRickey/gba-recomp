//! gba-pack — turn a mapped image into a distributable recompiled
//! game binary. See docs/packaging.md for the design; this binary is
//! the validation/planning surface of it (the build steps land next).
//!
//! The product invariant the packager exists to uphold: a package
//! carries NO image or BIOS bytes. It pins both by SHA-256 and the
//! produced binary refuses to run until the end user supplies files
//! that hash to exactly those values.

mod config;
mod icon;

use config::{EngineHle, PackConfig, Platform};
use sha2::{Digest, Sha256};
use std::path::Path;

/// Dynamic-library extension for translations. Host-platform only:
/// gba-pack packs for the host (documented limit), and recomp emits
/// the translation with the host's native extension.
const LIB_EXT: &str = if cfg!(windows) {
    "dll"
} else if cfg!(target_os = "macos") {
    "dylib"
} else {
    "so"
};

const USAGE: &str = "usage: gba-pack <pack.toml> [--rom FILE] [--bios FILE]
       gba-pack build <pack.toml> --rom FILE --bios FILE [--out DIR]
                      [--recomp PATH] [--soak FRAMES] [--no-soak]

Plan mode validates the package description and (with --rom/--bios) the
inputs, then prints the build plan.

Build mode produces the distributable package directory: translates the
image (recomp build --ram, labels auto-loaded), soak-tests the result,
and assembles <out>/<name>/ — the runtime binary, the translation, and
the recomp.pack.toml manifest pinning ROM and BIOS by SHA-256. With
[runtime] interpreter = false the build refuses to ship unless the soak
ran ZERO interpreter-fallback steps (recording and rebuilding once to
close coverage gaps automatically).

--no-soak skips the soak gate (the long fallback-traced validation run)
when you trust the map's coverage. --ram profiling still runs — it is the
only way to capture RAM-resident (IWRAM) code, which a static ROM map
cannot address.";

fn sha256_file(path: &str) -> Result<String, String> {
    let data = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    Ok(Sha256::digest(&data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
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
        if line.starts_with("labels:")
            || line.starts_with("blocks:")
            || line.starts_with("bios:")
            || line.starts_with("profiled ")
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
    // A deeper default soak than a quick smoke test: the gate proves
    // covered paths, so a shallow window (a single boot) lets deep-play
    // code slip through. 7200 frames (~2 min of play) exercises menus
    // and the first real gameplay; --soak raises it further.
    let mut soak = 7200u64;
    // --no-soak skips the soak *gate* (the long fallback-traced validation
    // run), trusting the map's coverage. It still runs --ram profiling:
    // that pass discovers RAM-resident (IWRAM) code — copied from ROM at
    // boot and executed at 0x03xxxxxx — which a static ROM map cannot
    // address, so it is NOT optional. Skipping it leaves a full recomp that
    // halts the moment the game enters its IWRAM code.
    let mut no_soak = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--rom" => rom = Some(it.next().ok_or("--rom needs a value")?.clone()),
            "--bios" => bios = Some(it.next().ok_or("--bios needs a value")?.clone()),
            "--out" => out_dir = it.next().ok_or("--out needs a value")?.into(),
            "--recomp" => recomp_flag = Some(it.next().ok_or("--recomp needs a value")?.clone()),
            "--no-soak" => no_soak = true,
            "--soak" => {
                soak = it
                    .next()
                    .ok_or("--soak needs a value")?
                    .parse()
                    .map_err(|e| format!("bad soak: {e}"))?
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
            return Err(format!(
                "{}: sha256 {got} does not match {what}",
                p.display()
            ));
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
    let stem = rom
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or("bad ROM name")?
        .to_string();
    // The package is BIOS-coherent by construction: the runtime boots
    // the end user's real BIOS (pin-enforced), so the translation is
    // built and soaked against the same image — an HLE translation
    // would have no native blocks for region 0 and a full recomp
    // would trap at the reset vector.
    let translation = work.join(format!("out/{stem}-bios.{LIB_EXT}"));
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
    // A packager can pin a real compiler (e.g. clang) instead of the
    // recompiler's default cc->TinyCC fallback; recomp honors $GBA_RECOMP_CC
    // first in its detection order.
    if let Some(c) = cfg.build.compiler.as_deref() {
        build_envs.push(("GBA_RECOMP_CC", c));
        println!("      compiler: {c} (pinned via [build] compiler)");
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
    // --ram is always on: it profiles an interpreter run to translate
    // RAM-resident code the static map can't address (see --no-soak above).
    let mut build_args: Vec<&str> = vec!["build", rom_str, "--ram", "--bios", bios_str];
    if let Some(l) = &labels_abs {
        build_args.push("--labels");
        build_args.push(l);
    }

    println!("[1/3] translating (recomp build --ram --bios; exhaustive seeding from the map)...");
    if !build_envs.is_empty() {
        println!(
            "      build env: {}",
            build_envs
                .iter()
                .map(|(k, _)| *k)
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
    recomp_run(&recomp, &work, &build_args, keep_c)?;
    if !translation.is_file() {
        return Err(format!(
            "translation did not produce {}",
            translation.display()
        ));
    }

    if no_soak {
        println!("[2/3] soak skipped (--no-soak): trusting the map's exhaustive coverage");
    } else {
        println!("[2/3] soak: {soak} frames with fallback tracing...");
        let frames = soak.to_string();
        let soak_args = ["runc", rom_str, "--frames", &frames, "--bios", bios_str];
        let mut steps = fallback_steps(&recomp_run(
            &recomp,
            &work,
            &soak_args,
            &[("RECOMP_TRACE_FALLBACK", "1")],
        )?)?;
        println!("      fallback_steps: {steps}");
        if !cfg.runtime.interpreter && steps > 0 {
            println!("      full recomp requires zero — recording the gaps and rebuilding once...");
            recomp_run(
                &recomp,
                &work,
                &[
                    "runc",
                    rom_str,
                    "--frames",
                    &frames,
                    "--bios",
                    bios_str,
                    "--record-labels",
                ],
                &[],
            )?;
            recomp_run(&recomp, &work, &build_args, keep_c)?;
            steps = fallback_steps(&recomp_run(
                &recomp,
                &work,
                &soak_args,
                &[("RECOMP_TRACE_FALLBACK", "1")],
            )?)?;
            println!("      fallback_steps after rebuild: {steps}");
            if steps > 0 {
                return Err(format!(
                    "full recomp gate failed: {steps} interpreter steps remain after a \
record-and-rebuild pass. Extend coverage (longer/recorded soak input, more labels) \
or set [runtime] interpreter = true.",
                ));
            }
        }
    }

    println!("[3/3] assembling package...");
    // The icon master: the packager's PNG, or the bundled canonical icon.
    let master = icon::load_master(cfg.package.icon.as_deref())?;
    if cfg.package.icon.is_none() {
        println!("      icon: using the canonical default (no package.icon given)");
    } else {
        println!(
            "      icon: {}×{} master from package.icon",
            master.w, master.h
        );
    }

    let pkg = out_dir.join(&cfg.package.name);
    std::fs::create_dir_all(&pkg).map_err(|e| format!("{}: {e}", pkg.display()))?;
    let name = &cfg.package.name;

    // The runtime resolves its manifest/translation/ROM/BIOS beside the
    // executable (recomp packfile.rs: current_exe -> exe_dir). On macOS the
    // binary lives in <name>.app/Contents/MacOS, so the package data must
    // sit there; on Windows/Linux it sits in the package root next to the
    // binary. `data_dir` is wherever the executable ends up.
    let (exe, data_dir): (std::path::PathBuf, std::path::PathBuf) = match host {
        Platform::Macos => {
            let app = pkg.join(format!("{name}.app"));
            let macos = app.join("Contents/MacOS");
            let res = app.join("Contents/Resources");
            std::fs::create_dir_all(&macos).map_err(|e| format!("{}: {e}", macos.display()))?;
            std::fs::create_dir_all(&res).map_err(|e| format!("{}: {e}", res.display()))?;
            // .icns + Info.plist
            std::fs::write(res.join(format!("{name}.icns")), icon::build_icns(&master))
                .map_err(|e| format!("{name}.icns: {e}"))?;
            std::fs::write(app.join("Contents/Info.plist"), info_plist(name))
                .map_err(|e| format!("Info.plist: {e}"))?;
            (macos.join(name), macos)
        }
        _ => (pkg.join(name), pkg.clone()),
    };

    std::fs::copy(&recomp, &exe).map_err(|e| format!("{}: {e}", exe.display()))?;
    let translation_name = format!("translation.{LIB_EXT}");
    std::fs::copy(&translation, data_dir.join(&translation_name)).map_err(|e| e.to_string())?;
    // The engine-HLE pin travels only when it pins something — `auto`
    // is the runtime's own default discovery.
    let engine_pin = match cfg.runtime.engine_hle {
        EngineHle::Auto => String::new(),
        e => format!("engine-hle = \"{}\"\n", e.as_str()),
    };
    std::fs::write(
        data_dir.join("recomp.pack.toml"),
        format!(
            "# Generated by gba-pack — pins only, no image content.\n\
[package]\nname = \"{}\"\n\n[image]\nrom-sha256 = \"{}\"\nbios-sha256 = \"{}\"\n\n\
[runtime]\ninterpreter = {}\nmenu = {}\n{}\n[translation]\nfile = \"{}\"\n",
            name,
            cfg.image.rom_sha256,
            cfg.image.bios_sha256,
            cfg.runtime.interpreter,
            cfg.runtime.menu,
            engine_pin,
            translation_name,
        ),
    )
    .map_err(|e| e.to_string())?;

    // Per-platform icon embedding + desktop integration, and a top-level
    // README that explains where the end user drops their own dumps.
    let readme = match host {
        // Inside an .app the binary is buried; the user drops dumps into
        // the bundle's MacOS dir (or the shared config dir), so the
        // top-level README spells out the path.
        Platform::Macos => format!(
            "{name} — a statically recompiled game (macOS .app bundle).\n\n\
This package contains NO game or BIOS data. To play, place your own dumps\n\
inside the bundle next to the executable, or in your config directory:\n\n\
  {name}.app/Contents/MacOS/   (or ~/Library/Application Support/gba-recomp/)\n\n\
  1. your cartridge image (.gba),  sha256 {rom}\n\
  2. your BIOS image as gba_bios.bin, sha256 {bios}\n\n\
Then open {name}.app. The bundle is ad-hoc signed; on first launch macOS may\n\
ask you to confirm in System Settings > Privacy & Security.\n",
            name = name,
            rom = cfg.image.rom_sha256,
            bios = cfg.image.bios_sha256,
        ),
        _ => format!(
            "{name} — a statically recompiled game.\n\n\
This package contains NO game or BIOS data. To play, place your own dumps\n\
next to the executable:\n\n  1. your cartridge image (.gba), sha256 {rom}\n\
  2. your BIOS image as gba_bios.bin, sha256 {bios}\n\nThen run ./{name}\n",
            name = name,
            rom = cfg.image.rom_sha256,
            bios = cfg.image.bios_sha256,
        ),
    };
    std::fs::write(pkg.join("README.txt"), readme).map_err(|e| e.to_string())?;

    // The project's dual license travels with every package so the end
    // user receives the terms the runtime ships under. Embedded (not
    // read from the repo) so packaging works from any working directory.
    std::fs::write(
        pkg.join("LICENSE-MIT"),
        include_str!("../../../LICENSE-MIT"),
    )
    .map_err(|e| e.to_string())?;
    std::fs::write(
        pkg.join("LICENSE-APACHE"),
        include_str!("../../../LICENSE-APACHE"),
    )
    .map_err(|e| e.to_string())?;

    match host {
        Platform::Macos => {
            // Ad-hoc codesign so arm64 will run an unsigned bundle without
            // Gatekeeper killing it (`codesign -dv` then proves the sig).
            let app = pkg.join(format!("{name}.app"));
            let st = std::process::Command::new("codesign")
                .args(["--force", "--deep", "--sign", "-"])
                .arg(&app)
                .status()
                .map_err(|e| format!("codesign: {e}"))?;
            if !st.success() {
                return Err("codesign of the .app failed".into());
            }
            println!("      macOS: {name}.app built, ad-hoc signed");
        }
        Platform::Windows => {
            icon::write_windows_ico(&pkg, name, &master)?;
            // PE resource embedding is host-gated; the side-car .ico always
            // travels with the package (see icon::embed_windows_pe).
            #[cfg(target_os = "windows")]
            icon::embed_windows_pe(&exe, &master)?;
            println!("      windows: {name}.ico emitted (PE embed wired on Windows hosts)");
        }
        Platform::Linux => {
            icon::write_linux(&pkg, name, &master)?;
            println!("      linux: {name}.png + {name}.desktop emitted");
        }
    }

    if cfg.output.c_source {
        let src = pkg.join("src-c");
        std::fs::create_dir_all(&src).map_err(|e| e.to_string())?;
        let mut n = 0;
        if let Ok(entries) = std::fs::read_dir(work.join("out")) {
            for e in entries.flatten() {
                if e.path().extension().is_some_and(|x| x == "c") {
                    std::fs::copy(e.path(), src.join(e.file_name())).map_err(|e| e.to_string())?;
                    n += 1;
                }
            }
        }
        println!("      C source: {n} translation units -> {}", src.display());
    }
    println!();
    println!("package ready: {}", pkg.display());
    println!(
        "  {} + translation.{} + recomp.pack.toml (interpreter = {})",
        name, LIB_EXT, cfg.runtime.interpreter
    );
    println!(
        "  end user supplies: ROM {}…, BIOS {}…",
        &cfg.image.rom_sha256[..12],
        &cfg.image.bios_sha256[..12]
    );
    Ok(())
}

/// Minimal Info.plist for the macOS .app: enough for Finder/Dock to show
/// the icon and Launch Services to run the bundle. The identifier is
/// derived from the package name; CFBundleIconFile points at the .icns.
fn info_plist(name: &str) -> String {
    // CFBundleIdentifier wants a reverse-DNS-ish string; keep it free of
    // any vendor/title text and just namespace under the product.
    let ident_stem: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
\"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n<dict>\n\
  <key>CFBundleName</key>            <string>{name}</string>\n\
  <key>CFBundleDisplayName</key>     <string>{name}</string>\n\
  <key>CFBundleExecutable</key>      <string>{name}</string>\n\
  <key>CFBundleIconFile</key>        <string>{name}</string>\n\
  <key>CFBundleIdentifier</key>      <string>com.gba-recomp.{ident_stem}</string>\n\
  <key>CFBundlePackageType</key>     <string>APPL</string>\n\
  <key>CFBundleVersion</key>         <string>1</string>\n\
  <key>CFBundleShortVersionString</key> <string>1.0</string>\n\
  <key>LSMinimumSystemVersion</key>  <string>10.13</string>\n\
  <key>NSHighResolutionCapable</key> <true/>\n\
</dict>\n</plist>\n"
    )
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
            return Err(format!(
                "{p}: sha256 {got} does not match image.bios-sha256"
            ));
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
        cfg.runtime.menu,
        cfg.runtime.enhanced_audio,
        cfg.runtime.screen_sim,
        cfg.runtime.engine_hle
    );
    println!(
        "output   binary={} c-source={}",
        cfg.output.binary, cfg.output.c_source
    );
    println!();
    println!("plan validated. build pipeline not wired yet — next steps:");
    println!("  1. translate (recomp build with the label set, full-coverage report)");
    println!("  2. emit runtime crate with pinned SHAs + selected modules");
    println!(
        "  3. per-platform link{}",
        if cfg.output.c_source {
            " + C source tree export"
        } else {
            ""
        }
    );
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("gba-pack: {e}");
        std::process::exit(1);
    }
}
