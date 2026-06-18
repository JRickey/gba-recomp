//! Bundled-TinyCC compile backend — the fallback when no system C compiler
//! is on PATH, so a build never requires the end user to install a
//! toolchain. We load `libtcc` at *runtime* with `libloading` and drive its
//! documented C ABI to compile the emitted C straight to a shared library
//! in-process (no temp objects, no spawned process).
//!
//! Dynamic loading is deliberate: libtcc is LGPL-2.1, and loading it as a
//! replaceable shared object (never statically linked) keeps that copyleft
//! component at arm's length. The compiler's *output* — the translated
//! library — is not a derivative of the compiler, so it stays clean.

use std::ffi::{c_char, c_int, c_void, CString};
use std::path::{Path, PathBuf};

/// `tcc_set_output_type` value for "dynamic library" (libtcc.h).
const TCC_OUTPUT_DLL: c_int = 4;

/// Resolve the directory holding `libtcc.{dylib,so,dll}`, `libtcc1.a`, and an
/// `include/` (with our self-contained `stdint.h` shim — TinyCC bundles
/// `stddef.h`/`stdarg.h` but not `stdint.h`, which the emitted C needs).
/// Order: explicit `$GBA_RECOMP_TCC_LIB`, then a `tcc/` dir beside the
/// running executable (where release packaging stages it).
pub fn lib_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("GBA_RECOMP_TCC_LIB") {
        let p = PathBuf::from(d);
        if p.is_dir() {
            return Some(p);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let cand = exe.parent()?.join("tcc");
    if cand.is_dir() {
        return Some(cand);
    }
    macos_bundle_resource_tcc(&exe).filter(|p| p.is_dir())
}

#[cfg(target_os = "macos")]
fn macos_bundle_resource_tcc(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    if macos.file_name()? != "MacOS" {
        return None;
    }
    let contents = macos.parent()?;
    if contents.file_name()? != "Contents" {
        return None;
    }
    Some(contents.join("Resources").join("tcc"))
}

#[cfg(not(target_os = "macos"))]
fn macos_bundle_resource_tcc(_exe: &Path) -> Option<PathBuf> {
    None
}

fn shared_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "libtcc.dll"
    } else if cfg!(target_os = "macos") {
        "libtcc.dylib"
    } else {
        "libtcc.so"
    }
}

/// Compile every C file in `c_files` into one shared library at `out`, using
/// the libtcc in `lib_dir`. Equivalent to `tcc -shared a.c b.c -o out`, but
/// in-process. Errors carry libtcc's own diagnostics (it also prints them to
/// stderr via its default error handler).
pub fn compile_to_dll(lib_dir: &Path, c_files: &[String], out: &Path) -> Result<(), String> {
    let so = lib_dir.join(shared_name());
    let runtime_lib_dir = runtime_lib_dir(lib_dir);
    let cpath = |p: &Path| -> Result<CString, String> {
        CString::new(p.to_string_lossy().as_bytes()).map_err(|_| "NUL byte in path".to_string())
    };

    // SAFETY: we load a known C library and call its documented, stable C
    // ABI. The function pointers are valid for as long as `lib` stays loaded,
    // which is the whole of this block.
    unsafe {
        let lib = libloading::Library::new(&so)
            .map_err(|e| format!("load libtcc at {}: {e}", so.display()))?;

        let tcc_new = *lib
            .get::<unsafe extern "C" fn() -> *mut c_void>(b"tcc_new\0")
            .map_err(|e| format!("libtcc tcc_new: {e}"))?;
        let tcc_delete = *lib
            .get::<unsafe extern "C" fn(*mut c_void)>(b"tcc_delete\0")
            .map_err(|e| format!("libtcc tcc_delete: {e}"))?;
        let tcc_set_lib_path = *lib
            .get::<unsafe extern "C" fn(*mut c_void, *const c_char)>(b"tcc_set_lib_path\0")
            .map_err(|e| format!("libtcc tcc_set_lib_path: {e}"))?;
        let tcc_add_sysinclude_path = *lib
            .get::<unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int>(
                b"tcc_add_sysinclude_path\0",
            )
            .map_err(|e| format!("libtcc tcc_add_sysinclude_path: {e}"))?;
        let tcc_set_output_type = *lib
            .get::<unsafe extern "C" fn(*mut c_void, c_int) -> c_int>(b"tcc_set_output_type\0")
            .map_err(|e| format!("libtcc tcc_set_output_type: {e}"))?;
        let tcc_add_file = *lib
            .get::<unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int>(b"tcc_add_file\0")
            .map_err(|e| format!("libtcc tcc_add_file: {e}"))?;
        let tcc_output_file = *lib
            .get::<unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int>(b"tcc_output_file\0")
            .map_err(|e| format!("libtcc tcc_output_file: {e}"))?;

        let s = tcc_new();
        if s.is_null() {
            return Err("tcc_new returned null".into());
        }
        // Compile inside a closure so a single `tcc_delete` runs on every exit.
        let run = || -> Result<(), String> {
            tcc_set_lib_path(s, cpath(&runtime_lib_dir)?.as_ptr()); // finds libtcc1.a
            let inc = lib_dir.join("include");
            if inc.is_dir() {
                tcc_add_sysinclude_path(s, cpath(&inc)?.as_ptr()); // our stdint.h shim
            }
            if tcc_set_output_type(s, TCC_OUTPUT_DLL) != 0 {
                return Err("tcc_set_output_type(DLL) failed".into());
            }
            for f in c_files {
                let fc = CString::new(f.as_bytes()).map_err(|_| "NUL in path".to_string())?;
                if tcc_add_file(s, fc.as_ptr()) != 0 {
                    return Err(format!("tinycc failed to compile {f}"));
                }
            }
            if tcc_output_file(s, cpath(out)?.as_ptr()) != 0 {
                return Err(format!("tinycc failed to write {}", out.display()));
            }
            Ok(())
        };
        let r = run();
        tcc_delete(s);
        r
    }
}

fn runtime_lib_dir(lib_dir: &Path) -> PathBuf {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    let arch = Some("arm64");
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    let arch = Some("x86_64");
    #[cfg(not(all(
        target_os = "macos",
        any(target_arch = "aarch64", target_arch = "x86_64")
    )))]
    let arch: Option<&str> = None;

    if let Some(arch) = arch {
        let dir = lib_dir.join(arch);
        if dir.join("libtcc1.a").is_file() {
            return dir;
        }
    }
    lib_dir.to_path_buf()
}
