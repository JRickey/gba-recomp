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
pub mod lut;
#[cfg(feature = "gpu")]
pub mod present;
pub mod profile;

pub use blend::{ResponseMode, Temporal};
pub use lut::{ColorLut, ColorSettings};
pub use profile::{DisplayTarget, ScreenKind};
