//! Scanline PPU renderer.
//!
//! Stub for now: fills each line with the backdrop color (palette entry 0)
//! so mode/bitmap rendering can land in a focused follow-up. The call site
//! (the HBlank event in `mem.rs`) and the framebuffer contract are final.

use crate::mem::{MemMap, VISIBLE_WIDTH};

pub fn render_scanline(mem: &mut MemMap, line: usize) {
    let backdrop = u16::from_le_bytes([mem.palette[0], mem.palette[1]]);
    let row = &mut mem.framebuffer[line * VISIBLE_WIDTH..(line + 1) * VISIBLE_WIDTH];
    row.fill(backdrop);
}
