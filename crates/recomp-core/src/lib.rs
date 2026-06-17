//! Static recompiler engine: turn GBA machine code into C and (later)
//! compile it to a native library. Pure compute — no windowing, audio, or
//! input — so it can be linked by the `recomp` developer CLI, the packager,
//! and the launcher's local-build path alike.

pub mod analyze;
pub mod build;
pub mod emit;
pub mod labels;
pub mod tcc;

pub use build::{build_library, Backend, BuildReport, Compiler, Profile};
pub use labels::{FileLabels, LabelSource};

/// FNV-1a 64-bit hash. Content-addresses emitted constant pools (see
/// `emit`) and machine-local IWRAM/save snapshots in the callers.
pub fn fnv64(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x1_0000_01b3);
    }
    h
}
