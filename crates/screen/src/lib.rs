//! Screen simulation: reproduce, per hardware revision, what the console's
//! panel did to the colors developers authored — and target the result at
//! the colorspace the user's display actually shows.
//!
//! Pipeline (order matches the reference ecosystem):
//!
//! ```text
//! raw BGR555 frames -> [temporal response] -> [color LUT] -> [grid + scale]
//!      (emulation)        (blend.rs)           (lut.rs)      (GPU, present.rs)
//! ```
//!
//! Frame hashing, verify and sweeps stay defined on the raw BGR555 frames;
//! everything here is present-time only and cannot affect parity tooling.

pub mod blend;
pub mod color;
/// Resolve the effective display target from OS facts (panel EDID +
/// compositor color-management state): declare sRGB when the OS manages it,
/// else compress into the panel's primaries ourselves.
#[cfg(feature = "gpu")]
pub mod display;
/// Read the connected panel's measured primaries from its EDID (Linux),
/// for the unmanaged color-management fallback.
#[cfg(all(feature = "gpu", target_os = "linux"))]
mod edid;
pub mod lut;
#[cfg(feature = "gpu")]
pub mod present;
pub mod profile;
/// Wayland color-management probe (Linux): on its own isolated connection,
/// reads what color volume the compositor believes the output has, so
/// `display` can tell a color-managing compositor from a passthrough one.
#[cfg(all(feature = "gpu", target_os = "linux"))]
mod wayland_cm;

pub use blend::{ResponseMode, Temporal};
pub use lut::{ColorLut, ColorSettings};
pub use profile::{DisplayTarget, ScreenKind};
