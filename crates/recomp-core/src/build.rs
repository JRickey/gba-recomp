//! Drive a recompile end to end: analyze → emit C → compile each unit →
//! link a shared library. Discovering the seeds (interpreter profiling,
//! label files, BIOS sweeps) is the caller's job — this takes the already
//! assembled [`View`] and seed set and produces the native artifact, so
//! the same engine serves the developer CLI, the packager, and the
//! launcher's local-build path.

use std::path::{Path, PathBuf};

use crate::analyze::{self, View};
use crate::emit;

/// Optimization profile for the build. `Fast` is the launcher's "try a
/// game now" local build (latency matters); `Ship` is the optimized
/// artifact gba-pack distributes.
// waiting to implement: build_library hardcodes the current flags until the
// launcher's local-build path needs to diverge from the packager's.
#[allow(dead_code)]
#[derive(Clone, Copy)]
pub enum Profile {
    Fast,
    Ship,
}

/// How the emitted C gets compiled.
pub enum Backend {
    /// Spawn an external compiler binary (`cc`/`clang`/`gcc`/`zig cc`) that
    /// accepts our gcc/clang-style flags (`-O1 -fPIC -c`, `-shared`).
    External(PathBuf),
    /// Compile in-process with the bundled TinyCC in this directory (the
    /// fallback when no system compiler is found). See [`crate::tcc`].
    LibTcc(PathBuf),
}

/// Where the C compiler comes from. Resolve one with [`Compiler::detect`]
/// (system compiler if present, else bundled TinyCC) or pin an explicit
/// external program with [`Compiler::external`].
pub struct Compiler {
    pub backend: Backend,
    /// Cross-compile target triple (`zig cc -target …`).
    // waiting to implement: gba-pack packs host-only today; this drives
    // cross-target emission when that lands.
    #[allow(dead_code)]
    pub target: Option<String>,
    /// Which optimization profile to compile with.
    // waiting to implement: see `Profile`.
    #[allow(dead_code)]
    pub profile: Profile,
}

impl Compiler {
    /// Pin a specific external compiler binary (resolved on PATH at spawn
    /// time if not absolute), shipping profile, host target.
    pub fn external<P: Into<PathBuf>>(program: P) -> Self {
        Compiler {
            backend: Backend::External(program.into()),
            target: None,
            profile: Profile::Ship,
        }
    }

    /// Resolve a compiler the way the launcher's "build to play" path and the
    /// CLI both should: an explicit override, then a system compiler on PATH,
    /// then the bundled TinyCC so a build never requires the user to install
    /// a toolchain.
    ///
    /// Windows note: we probe `clang`/`gcc`/`cc` (which accept our flags and
    /// work standalone) but deliberately NOT MSVC `cl.exe` — it needs a
    /// vcvars environment (INCLUDE/LIB) and uses `/`-style flags, so wiring it
    /// means a flag-translation layer. On a typical Windows box none of
    /// clang/gcc/cc is on PATH, so the bundled TinyCC is what makes "just
    /// build it" work with zero setup. Point `$GBA_RECOMP_CC` at a specific
    /// compiler (e.g. an installed `clang`) to override.
    pub fn detect() -> Self {
        if let Some(p) = std::env::var_os("GBA_RECOMP_CC") {
            return Compiler::external(PathBuf::from(p));
        }
        let force_tcc = std::env::var_os("GBA_RECOMP_FORCE_TCC").is_some();
        if !force_tcc {
            let candidates: &[&str] = if cfg!(windows) {
                &["clang", "gcc", "cc"]
            } else {
                &["cc", "clang", "gcc"]
            };
            for name in candidates {
                if let Some(p) = which(name) {
                    return Compiler::external(p);
                }
            }
        }
        if let Some(dir) = crate::tcc::lib_dir() {
            return Compiler {
                backend: Backend::LibTcc(dir),
                target: None,
                profile: Profile::Fast,
            };
        }
        // Nothing found: keep `cc` so the build surfaces the existing clear
        // "cc: not found" error rather than failing silently.
        Compiler::external("cc")
    }

    /// Human-readable backend name, for status/diagnostic output.
    pub fn describe(&self) -> String {
        match &self.backend {
            Backend::External(p) => format!("system compiler ({})", p.display()),
            Backend::LibTcc(d) => format!("bundled TinyCC ({})", d.display()),
        }
    }
}

/// Minimal `which`: first match for `name` (with platform exe suffixes) in a
/// `PATH` directory. Avoids a dependency for a five-line search.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: &[&str] = if cfg!(windows) {
        &["", ".exe", ".bat", ".cmd"]
    } else {
        &[""]
    };
    for dir in std::env::split_paths(&path) {
        for ext in exts {
            let cand = dir.join(format!("{name}{ext}"));
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// What a completed build translated. Returned for in-process callers
/// that want the counts without parsing stdout; `build_library` also emits
/// the `blocks:`/`wrote` lines the packager parses, at their original
/// points (a later step routes those through a caller sink so the
/// in-process launcher stays stdout-silent).
pub struct BuildReport {
    pub blocks: usize,
    pub instrs: usize,
    pub units: usize,
}

/// Analyze `seeds` over `view`, translate every reachable block to C, and
/// compile+link a shared library at `out`. Intermediates land next to
/// `out` as `<stem>.<i>.{c,o}`. `start_pct` is where this build's slice of
/// the progress bar begins (the caller's discovery phase consumed
/// `0..start_pct`); the dominant compile phase is exact (blocks compiled /
/// total blocks).
///
/// `progress` is a monotonic 0..=100 sink — the CLI renders a terminal
/// bar, the launcher can pass a closure capturing its egui state.
pub fn build_library(
    view: &View,
    seeds: &[u32],
    cc: &Compiler,
    out: &Path,
    start_pct: u8,
    progress: &mut dyn FnMut(u8, &str),
) -> Result<BuildReport, String> {
    progress(start_pct + 1, "charting every reachable code path\u{2026}");
    let analysis = analyze::analyze(view, seeds);
    let n_instrs: usize = analysis.blocks.iter().map(|b| b.instrs.len()).sum();
    println!("blocks: {} instructions: {n_instrs}", analysis.blocks.len());
    progress(
        start_pct + 2,
        &format!(
            "map drawn: {} code blocks to translate",
            analysis.blocks.len()
        ),
    );

    // Intermediates share the artifact's stem: `out/game.so` → `out/game`.
    let prefix = out.with_extension("");
    let prefix = prefix.display().to_string();

    // 16 MB translation units: full-image translations can exceed the
    // source image a hundredfold, and a single huge unit makes cc balloon
    // to many GB. Bounded units keep each cc invocation modest AND let the
    // units compile in parallel.
    const MAX_UNIT: usize = 16 << 20;
    let span_start = start_pct + 2;

    // Phase 1 — emit every unit to disk. Streaming keeps peak memory at one
    // unit; the heavy compile is deferred so it can fan out across cores.
    // -fPIC (below) is required: these objects link into a shared library
    // (`cc -shared`) that play dlopen()s. Without it gcc's small code model
    // emits 32-bit absolute relocations (R_X86_64_32S) for jump tables and
    // constant pools, which the linker rejects for a shared object.
    let mut units: Vec<(String, String)> = Vec::new(); // (c_path, o_path)
    emit::emit_c_chunked(&analysis, view, MAX_UNIT, |c, _blocks_done| {
        let i = units.len();
        let c_path = format!("{prefix}.{i}.c");
        let o_path = format!("{prefix}.{i}.o");
        std::fs::write(&c_path, c).map_err(|e| e.to_string())?;
        units.push((c_path, o_path));
        progress(
            span_start,
            &format!("translating to C \u{2014} {} units emitted\u{2026}", i + 1),
        );
        Ok(())
    })?;
    let unit_count = units.len();
    let keep_c = std::env::var_os("RECOMP_KEEP_C").is_some();

    // Phase 2 — compile. Two backends: an external compiler spawned per unit
    // (parallel, then a `-shared` link), or the bundled libtcc which compiles
    // every unit straight to the shared library in-process.
    let prog = match &cc.backend {
        Backend::LibTcc(dir) => {
            progress(
                span_start + 1,
                "forging native code with bundled TinyCC\u{2026}",
            );
            let c_paths: Vec<String> = units.iter().map(|(c, _)| c.clone()).collect();
            crate::tcc::compile_to_dll(dir, &c_paths, out)?;
            if !keep_c {
                for (c, _) in &units {
                    let _ = std::fs::remove_file(c);
                }
            }
            progress(100, "ready to play");
            println!("wrote {} ({unit_count} units, tinycc)", out.display());
            return Ok(BuildReport {
                blocks: analysis.blocks.len(),
                instrs: n_instrs,
                units: unit_count,
            });
        }
        Backend::External(prog) => prog,
    };

    // External-compiler path: one job per core keeps every core busy without
    // blowing memory (each 16 MB unit's cc stays modest); RECOMP_JOBS
    // overrides. Workers pull units from a shared counter; the main thread
    // drives the bar from the completion count.
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
    let jobs = std::env::var("RECOMP_JOBS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
        })
        .min(unit_count.max(1));
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let abort = AtomicBool::new(false);
    let err: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    let units = &units;

    std::thread::scope(|s| {
        for _ in 0..jobs {
            s.spawn(|| {
                while !abort.load(Relaxed) {
                    let i = next.fetch_add(1, Relaxed);
                    if i >= unit_count {
                        break;
                    }
                    let (c_path, o_path) = &units[i];
                    match std::process::Command::new(prog)
                        .args(["-O1", "-fPIC", "-c", "-o", o_path, c_path])
                        .status()
                    {
                        Ok(st) if st.success() => {
                            if !keep_c {
                                let _ = std::fs::remove_file(c_path);
                            }
                            done.fetch_add(1, Relaxed);
                        }
                        Ok(_) => {
                            *err.lock().unwrap() = Some(format!("cc failed on {c_path}"));
                            abort.store(true, Relaxed);
                        }
                        Err(e) => {
                            *err.lock().unwrap() = Some(format!("cc: {e}"));
                            abort.store(true, Relaxed);
                        }
                    }
                }
            });
        }
        // Compile owns span [span_start+1, 96], advancing as whole units land.
        let lo = (span_start + 1) as u64;
        loop {
            let d = done.load(Relaxed) as u64;
            let pct = (lo + d * (96 - lo) / unit_count.max(1) as u64) as u8;
            progress(
                pct,
                &format!("forging native code on {jobs} cores \u{2014} {d}/{unit_count} units"),
            );
            if d as usize >= unit_count || abort.load(Relaxed) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    });
    if let Some(e) = err.into_inner().unwrap() {
        return Err(e);
    }

    progress(97, "linking it all together\u{2026}");
    let status = std::process::Command::new(prog)
        .arg("-shared")
        .arg("-o")
        .arg(out)
        .args(units.iter().map(|(_, o)| o))
        .status()
        .map_err(|e| format!("cc: {e}"))?;
    for (_, o) in units {
        let _ = std::fs::remove_file(o);
    }
    if !status.success() {
        return Err("link failed".into());
    }
    progress(100, "ready to play");
    println!("wrote {} ({unit_count} units)", out.display());
    Ok(BuildReport {
        blocks: analysis.blocks.len(),
        instrs: n_instrs,
        units: unit_count,
    })
}
