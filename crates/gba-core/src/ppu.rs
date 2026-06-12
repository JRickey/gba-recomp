//! Scanline PPU renderer (hardware reference, "LCD Video Controller").
//!
//! Renders one visible scanline at HBlank into the RGB555 framebuffer:
//! - modes 0-2: text backgrounds (4/8bpp tiles, flips, scrolling) and
//!   affine backgrounds (8bpp, optional wrap, per-line reference stepping);
//! - modes 3-5: bitmap backgrounds (16bpp, 8bpp paged, small paged);
//! - sprites: 1D/2D mapping, 4/8bpp, flips, affine (incl. double-size),
//!   per-pixel priority against backgrounds, semi-transparent mode,
//!   OBJ-window mode;
//! - windows 0/1, OBJ window, outside region;
//! - color effects: alpha blend, brightness up/down, semi-transparent
//!   sprites forcing alpha against second targets;
//! - backdrop (palette entry 0).
//!
//! Mosaic is not yet implemented. Mid-scanline raster effects are out of
//! scope by design (scanline granularity; see docs/architecture.md §6).

use crate::mem::{MemMap, VISIBLE_WIDTH};

const TRANSPARENT: u16 = 0xFFFF;

/// One line of layer output: RGB555 color or TRANSPARENT.
type Line = [u16; VISIBLE_WIDTH];

#[derive(Clone, Copy)]
struct ObjPixel {
    color: u16,
    priority: u8,
    semi: bool,
}

const OBJ_EMPTY: ObjPixel = ObjPixel {
    color: TRANSPARENT,
    priority: 0xFF,
    semi: false,
};

fn io16(mem: &MemMap, off: usize) -> u16 {
    u16::from_le_bytes([mem.io[off], mem.io[off + 1]])
}

fn pal16(mem: &MemMap, index: usize) -> u16 {
    u16::from_le_bytes([mem.palette[index * 2], mem.palette[index * 2 + 1]]) & 0x7FFF
}

pub fn render_scanline(mem: &mut MemMap, line: usize) {
    let dispcnt = io16(mem, 0x00);

    if dispcnt & 0x80 != 0 {
        // Forced blank: white.
        let row = &mut mem.framebuffer[line * VISIBLE_WIDTH..(line + 1) * VISIBLE_WIDTH];
        row.fill(0x7FFF);
        return;
    }

    // Latch / step the affine reference accumulators.
    if line == 0 || mem.aff_dirty[0] {
        mem.aff_ref[0] = sign_extend28(read_io32(mem, 0x28));
        mem.aff_ref[1] = sign_extend28(read_io32(mem, 0x2C));
        mem.aff_dirty[0] = false;
    }
    if line == 0 || mem.aff_dirty[1] {
        mem.aff_ref[2] = sign_extend28(read_io32(mem, 0x38));
        mem.aff_ref[3] = sign_extend28(read_io32(mem, 0x3C));
        mem.aff_dirty[1] = false;
    }

    let mode = dispcnt & 7;
    let mut bg_lines = [[TRANSPARENT; VISIBLE_WIDTH]; 4];
    let mut bg_used = [false; 4];

    match mode {
        0 => {
            for bg in 0..4 {
                if dispcnt & (0x100 << bg) != 0 {
                    render_text_bg(mem, bg, line, &mut bg_lines[bg]);
                    bg_used[bg] = true;
                }
            }
        }
        1 => {
            for bg in 0..2 {
                if dispcnt & (0x100 << bg) != 0 {
                    render_text_bg(mem, bg, line, &mut bg_lines[bg]);
                    bg_used[bg] = true;
                }
            }
            if dispcnt & 0x400 != 0 {
                render_affine_bg(mem, 2, &mut bg_lines[2]);
                bg_used[2] = true;
            }
        }
        2 => {
            for bg in 2..4 {
                if dispcnt & (0x100 << bg) != 0 {
                    render_affine_bg(mem, bg, &mut bg_lines[bg]);
                    bg_used[bg] = true;
                }
            }
        }
        3 | 4 | 5 => {
            if dispcnt & 0x400 != 0 {
                render_bitmap_bg(mem, mode, dispcnt, line, &mut bg_lines[2]);
                bg_used[2] = true;
            }
        }
        _ => {}
    }

    // Sprites.
    let mut obj_line = [OBJ_EMPTY; VISIBLE_WIDTH];
    let mut obj_window = [false; VISIBLE_WIDTH];
    let mut line_has_semi = false;
    if dispcnt & 0x1000 != 0 {
        line_has_semi = render_sprites(mem, dispcnt, line, &mut obj_line, &mut obj_window);
    }

    // Step affine accumulators for the next line (PB/PD).
    mem.aff_ref[0] = mem.aff_ref[0].wrapping_add(io16(mem, 0x22) as i16 as i32);
    mem.aff_ref[1] = mem.aff_ref[1].wrapping_add(io16(mem, 0x26) as i16 as i32);
    mem.aff_ref[2] = mem.aff_ref[2].wrapping_add(io16(mem, 0x32) as i16 as i32);
    mem.aff_ref[3] = mem.aff_ref[3].wrapping_add(io16(mem, 0x36) as i16 as i32);

    compose(
        mem,
        dispcnt,
        line,
        &bg_lines,
        &bg_used,
        &obj_line,
        &obj_window,
        line_has_semi,
    );
}

fn read_io32(mem: &MemMap, off: usize) -> u32 {
    u32::from_le_bytes([
        mem.io[off],
        mem.io[off + 1],
        mem.io[off + 2],
        mem.io[off + 3],
    ])
}

fn sign_extend28(v: u32) -> i32 {
    ((v << 4) as i32) >> 4
}

// ---- text backgrounds ----

fn render_text_bg(mem: &MemMap, bg: usize, line: usize, out: &mut Line) {
    let cnt = io16(mem, 0x08 + bg * 2);
    let char_base = ((cnt >> 2) & 3) as usize * 0x4000;
    let eight_bpp = cnt & 0x80 != 0;
    let screen_base = ((cnt >> 8) & 0x1F) as usize * 0x800;
    let size = (cnt >> 14) & 3;

    let hofs = (io16(mem, 0x10 + bg * 4) & 0x1FF) as usize;
    let vofs = (io16(mem, 0x12 + bg * 4) & 0x1FF) as usize;

    let (w_tiles, h_tiles) = match size {
        0 => (32, 32),
        1 => (64, 32),
        2 => (32, 64),
        _ => (64, 64),
    };

    let y = (line + vofs) & (h_tiles * 8 - 1);
    let ty = y / 8;
    let py = y % 8;

    // Walk the line in tile spans: all pixels within one span share the
    // same map entry, so fetch and decode it once (per-pixel results are
    // identical to the previous per-pixel formulation).
    let mut sx = 0usize;
    while sx < VISIBLE_WIDTH {
        let x = (sx + hofs) & (w_tiles * 8 - 1);
        let tx = x / 8;
        let px = x % 8;
        let span = (8 - px).min(VISIBLE_WIDTH - sx);

        // Screenblock layout: 32x32-tile blocks, left-to-right then down.
        let block = (ty / 32) * (w_tiles / 32) + tx / 32;
        let entry_idx = block * 1024 + (ty % 32) * 32 + (tx % 32);
        let entry_off = screen_base + entry_idx * 2;
        let entry = u16::from_le_bytes([mem.vram[entry_off], mem.vram[entry_off + 1]]);

        let tile = (entry & 0x3FF) as usize;
        let hflip = entry & 0x400 != 0;
        let vflip = entry & 0x800 != 0;
        let fy = if vflip { 7 - py } else { py };

        if eight_bpp {
            let base = char_base + tile * 64 + fy * 8;
            for i in 0..span {
                let fx = if hflip { 7 - (px + i) } else { px + i };
                let addr = base + fx;
                let color_idx = if addr >= 0x1_0000 {
                    0
                } else {
                    mem.vram[addr] as usize
                };
                out[sx + i] = if color_idx == 0 {
                    TRANSPARENT
                } else {
                    pal16(mem, color_idx)
                };
            }
        } else {
            let base = char_base + tile * 32 + fy * 4;
            let pal_base = ((entry >> 12) as usize & 0xF) * 16;
            for i in 0..span {
                let fx = if hflip { 7 - (px + i) } else { px + i };
                let addr = base + fx / 2;
                let color_idx = if addr >= 0x1_0000 {
                    0
                } else {
                    let b = mem.vram[addr];
                    let nib = if fx & 1 == 0 { b & 0xF } else { b >> 4 } as usize;
                    if nib == 0 {
                        0
                    } else {
                        pal_base + nib
                    }
                };
                out[sx + i] = if color_idx == 0 {
                    TRANSPARENT
                } else {
                    pal16(mem, color_idx)
                };
            }
        }
        sx += span;
    }
}

// ---- affine backgrounds ----

fn render_affine_bg(mem: &MemMap, bg: usize, out: &mut Line) {
    let cnt = io16(mem, 0x08 + bg * 2);
    let char_base = ((cnt >> 2) & 3) as usize * 0x4000;
    let screen_base = ((cnt >> 8) & 0x1F) as usize * 0x800;
    let wrap = cnt & 0x2000 != 0;
    let size_tiles = 16usize << ((cnt >> 14) & 3); // 16,32,64,128
    let size_px = (size_tiles * 8) as i32;

    let (pa, pc, ref_base) = if bg == 2 {
        (
            io16(mem, 0x20) as i16 as i32,
            io16(mem, 0x24) as i16 as i32,
            0,
        )
    } else {
        (
            io16(mem, 0x30) as i16 as i32,
            io16(mem, 0x34) as i16 as i32,
            2,
        )
    };
    let mut fx = mem.aff_ref[ref_base];
    let mut fy = mem.aff_ref[ref_base + 1];

    for out_px in out.iter_mut() {
        let mut tx = fx >> 8;
        let mut ty = fy >> 8;
        fx = fx.wrapping_add(pa);
        fy = fy.wrapping_add(pc);

        if wrap {
            tx = tx.rem_euclid(size_px);
            ty = ty.rem_euclid(size_px);
        } else if tx < 0 || ty < 0 || tx >= size_px || ty >= size_px {
            *out_px = TRANSPARENT;
            continue;
        }
        let (tx, ty) = (tx as usize, ty as usize);

        let entry_off = screen_base + (ty / 8) * size_tiles + tx / 8;
        let tile = mem.vram[entry_off] as usize;
        let addr = char_base + tile * 64 + (ty % 8) * 8 + tx % 8;
        let color_idx = if addr < 0x1_0000 {
            mem.vram[addr] as usize
        } else {
            0
        };
        *out_px = if color_idx == 0 {
            TRANSPARENT
        } else {
            pal16(mem, color_idx)
        };
    }
}

// ---- bitmap backgrounds ----

fn render_bitmap_bg(mem: &MemMap, mode: u16, dispcnt: u16, line: usize, out: &mut Line) {
    let page = if dispcnt & 0x10 != 0 { 0xA000usize } else { 0 };
    match mode {
        3 => {
            for (x, out_px) in out.iter_mut().enumerate() {
                let off = (line * VISIBLE_WIDTH + x) * 2;
                let c = u16::from_le_bytes([mem.vram[off], mem.vram[off + 1]]) & 0x7FFF;
                *out_px = c;
            }
        }
        4 => {
            for (x, out_px) in out.iter_mut().enumerate() {
                let idx = mem.vram[page + line * VISIBLE_WIDTH + x] as usize;
                *out_px = if idx == 0 {
                    TRANSPARENT
                } else {
                    pal16(mem, idx)
                };
            }
        }
        _ => {
            // Mode 5: 160x128 16bpp, paged.
            for (x, out_px) in out.iter_mut().enumerate() {
                if x < 160 && line < 128 {
                    let off = page + (line * 160 + x) * 2;
                    let c = u16::from_le_bytes([mem.vram[off], mem.vram[off + 1]]) & 0x7FFF;
                    *out_px = c;
                } else {
                    *out_px = TRANSPARENT;
                }
            }
        }
    }
}

// ---- sprites ----

const OBJ_SIZES: [[(i32, i32); 4]; 3] = [
    [(8, 8), (16, 16), (32, 32), (64, 64)], // square
    [(16, 8), (32, 8), (32, 16), (64, 32)], // horizontal
    [(8, 16), (8, 32), (16, 32), (32, 64)], // vertical
];

/// Returns true if any semi-transparent sprite pixel was written
/// (conservative: a later overwrite by an opaque sprite still counts).
fn render_sprites(
    mem: &MemMap,
    dispcnt: u16,
    line: usize,
    out: &mut [ObjPixel; VISIBLE_WIDTH],
    window: &mut [bool; VISIBLE_WIDTH],
) -> bool {
    let mut any_semi = false;
    let one_dim = dispcnt & 0x40 != 0;
    let bitmap_mode = (dispcnt & 7) >= 3;
    let line = line as i32;

    for i in 0..128 {
        let a0 = u16::from_le_bytes([mem.oam[i * 8], mem.oam[i * 8 + 1]]);
        let a1 = u16::from_le_bytes([mem.oam[i * 8 + 2], mem.oam[i * 8 + 3]]);
        let a2 = u16::from_le_bytes([mem.oam[i * 8 + 4], mem.oam[i * 8 + 5]]);

        let affine = a0 & 0x100 != 0;
        if !affine && a0 & 0x200 != 0 {
            continue; // disabled
        }
        let shape = ((a0 >> 14) & 3) as usize;
        if shape == 3 {
            continue; // prohibited
        }
        let size_idx = ((a1 >> 14) & 3) as usize;
        let (w, h) = OBJ_SIZES[shape][size_idx];
        let double = affine && a0 & 0x200 != 0;
        let (bw, bh) = if double { (w * 2, h * 2) } else { (w, h) };

        let mut y = (a0 & 0xFF) as i32;
        if y >= 160 {
            y -= 256;
        }
        let mut x = (a1 & 0x1FF) as i32;
        if x >= 240 {
            x -= 512;
        }
        if line < y || line >= y + bh {
            continue;
        }

        let mode = (a0 >> 10) & 3;
        let eight_bpp = a0 & 0x2000 != 0;
        let tile_base = (a2 & 0x3FF) as usize;
        let priority = ((a2 >> 10) & 3) as u8;
        let palette = ((a2 >> 12) & 0xF) as usize;

        // Affine parameters.
        let (pa, pb, pc, pd) = if affine {
            let g = ((a1 >> 9) & 0x1F) as usize * 32;
            let p = |off: usize| {
                u16::from_le_bytes([mem.oam[g + off], mem.oam[g + off + 1]]) as i16 as i32
            };
            (p(6), p(14), p(22), p(30))
        } else {
            (0x100, 0, 0, 0x100)
        };

        let row_tiles = if one_dim {
            (w / 8) * if eight_bpp { 2 } else { 1 }
        } else {
            32
        };

        let ly = line - y;
        for bx in 0..bw {
            let sx = x + bx;
            if !(0..VISIBLE_WIDTH as i32).contains(&sx) {
                continue;
            }
            let sx_usize = sx as usize;

            // Texture coordinates.
            let (u, v) = if affine {
                let dx = bx - bw / 2;
                let dy = ly - bh / 2;
                let u = ((pa * dx + pb * dy) >> 8) + w / 2;
                let v = ((pc * dx + pd * dy) >> 8) + h / 2;
                if u < 0 || v < 0 || u >= w || v >= h {
                    continue;
                }
                (u, v)
            } else {
                let mut u = bx;
                let mut v = ly;
                // OBJ attr1 flips: bit 12 horizontal, bit 13 vertical
                // (unlike BG map entries, where flips are bits 10/11).
                if a1 & 0x1000 != 0 {
                    u = w - 1 - u;
                }
                if a1 & 0x2000 != 0 {
                    v = h - 1 - v;
                }
                (u, v)
            };

            let (tu, pu) = (u / 8, u % 8);
            let (tv, pv) = (v / 8, v % 8);
            let tile_step = if eight_bpp { 2 } else { 1 };
            let tile = tile_base + tv as usize * row_tiles as usize + tu as usize * tile_step;
            // In bitmap modes the first 512 tiles overlap the bitmap.
            if bitmap_mode && tile < 512 {
                continue;
            }
            let addr = 0x1_0000
                + if eight_bpp {
                    (tile / 2) * 64 + pv as usize * 8 + pu as usize
                } else {
                    tile * 32 + pv as usize * 4 + pu as usize / 2
                };
            if addr >= 0x1_8000 {
                continue;
            }
            let color_idx = if eight_bpp {
                mem.vram[addr] as usize
            } else {
                let b = mem.vram[addr];
                let nib = if pu & 1 == 0 { b & 0xF } else { b >> 4 } as usize;
                if nib == 0 {
                    0
                } else {
                    palette * 16 + nib
                }
            };
            if color_idx == 0 {
                continue;
            }
            let color = pal16(mem, 256 + color_idx);

            if mode == 2 {
                window[sx_usize] = true;
            } else if out[sx_usize].priority == 0xFF || priority < out[sx_usize].priority {
                out[sx_usize] = ObjPixel {
                    color,
                    priority,
                    semi: mode == 1,
                };
                any_semi |= mode == 1;
            }
        }
    }
    any_semi
}

// ---- composition ----

/// Layer ids for blend targeting: 0-3 BGs, 4 OBJ, 5 backdrop.
#[allow(clippy::too_many_arguments)]
fn compose(
    mem: &mut MemMap,
    dispcnt: u16,
    line: usize,
    bg_lines: &[Line; 4],
    bg_used: &[bool; 4],
    obj_line: &[ObjPixel; VISIBLE_WIDTH],
    obj_window: &[bool; VISIBLE_WIDTH],
    line_has_semi: bool,
) {
    let backdrop = pal16(mem, 0);

    // Background priorities (compose ties: lower BG number wins; sort
    // keys are unique, so unstable sort matches the old stable sort).
    let mut bg_prio_buf = [(0u8, 0usize); 4];
    let mut nbg = 0;
    for bg in 0..4 {
        if bg_used[bg] {
            bg_prio_buf[nbg] = ((io16(mem, 0x08 + bg * 2) & 3) as u8, bg);
            nbg += 1;
        }
    }
    bg_prio_buf[..nbg].sort_unstable();
    let bg_prio = &bg_prio_buf[..nbg];

    let win0_on = dispcnt & 0x2000 != 0;
    let win1_on = dispcnt & 0x4000 != 0;
    let objwin_on = dispcnt & 0x8000 != 0;
    let any_window = win0_on || win1_on || objwin_on;

    let winin = io16(mem, 0x48);
    let winout = io16(mem, 0x4A);

    let win0h = io16(mem, 0x40);
    let win1h = io16(mem, 0x42);
    let win0v = io16(mem, 0x44);
    let win1v = io16(mem, 0x46);

    let in_win0_y = win0_on && in_window_range((win0v >> 8) as u8, win0v as u8, line as u8);
    let in_win1_y = win1_on && in_window_range((win1v >> 8) as u8, win1v as u8, line as u8);

    let bldcnt = io16(mem, 0x50);
    let blend_mode = (bldcnt >> 6) & 3;
    let bldalpha = io16(mem, 0x52);
    let eva = (bldalpha & 0x1F).min(16) as u32;
    let evb = ((bldalpha >> 8) & 0x1F).min(16) as u32;
    let evy = (io16(mem, 0x54) & 0x1F).min(16) as u32;

    let row = line * VISIBLE_WIDTH;

    // Fast path: no color effects and no semi-transparent sprite pixels
    // on this line, so the second-layer search is dead code — the top
    // layer alone decides each pixel: the first opaque BG in priority
    // order whose window control bit allows it, beaten by the sprite
    // when its priority is <= that BG's (sprites rank above BGs of
    // equal priority).
    if blend_mode == 0 && !line_has_semi {
        for x in 0..VISIBLE_WIDTH {
            let control: u16 = if !any_window {
                0x3F
            } else if in_win0_y && in_window_range((win0h >> 8) as u8, win0h as u8, x as u8) {
                winin & 0x3F
            } else if in_win1_y && in_window_range((win1h >> 8) as u8, win1h as u8, x as u8) {
                (winin >> 8) & 0x3F
            } else if objwin_on && obj_window[x] {
                (winout >> 8) & 0x3F
            } else {
                winout & 0x3F
            };

            let mut color = backdrop;
            let mut top_prio = 0xFFu8;
            for &(bp, bg) in bg_prio {
                if control & (1 << bg) != 0 {
                    let c = bg_lines[bg][x];
                    if c != TRANSPARENT {
                        color = c;
                        top_prio = bp;
                        break;
                    }
                }
            }
            let obj = &obj_line[x];
            if obj.priority != 0xFF && control & 0x10 != 0 && obj.priority <= top_prio {
                color = obj.color;
            }
            mem.framebuffer[row + x] = color;
        }
        return;
    }

    for x in 0..VISIBLE_WIDTH {
        // Window control for this pixel: bits 0-3 BGs, 4 OBJ, 5 effects.
        let control: u16 = if !any_window {
            0x3F
        } else if in_win0_y && in_window_range((win0h >> 8) as u8, win0h as u8, x as u8) {
            winin & 0x3F
        } else if in_win1_y && in_window_range((win1h >> 8) as u8, win1h as u8, x as u8) {
            (winin >> 8) & 0x3F
        } else if objwin_on && obj_window[x] {
            (winout >> 8) & 0x3F
        } else {
            winout & 0x3F
        };

        // Find the top two visible layers (OBJ ranks above BGs of equal
        // priority; BGs tie-break by index via the pre-sorted list).
        let obj = &obj_line[x];
        let obj_visible = obj.priority != 0xFF && control & 0x10 != 0;

        let mut first: Option<(u16, usize)> = None;
        let mut second: Option<(u16, usize)> = None;
        'outer: for p in 0..4u8 {
            if obj_visible && obj.priority == p {
                if first.is_none() {
                    first = Some((obj.color, 4));
                } else {
                    second = Some((obj.color, 4));
                    break 'outer;
                }
            }
            for &(bp, bg) in bg_prio {
                if bp == p && control & (1 << bg) != 0 {
                    let c = bg_lines[bg][x];
                    if c != TRANSPARENT {
                        if first.is_none() {
                            first = Some((c, bg));
                        } else {
                            second = Some((c, bg));
                            break 'outer;
                        }
                    }
                }
            }
        }
        let (fc, fl) = first.unwrap_or((backdrop, 5));
        let (sc, sl) = second.unwrap_or((backdrop, 5));

        let effects_allowed = control & 0x20 != 0;
        let semi = obj_visible && fl == 4 && obj.semi;

        let color = if semi && bldcnt & (0x100 << sl) != 0 {
            // Semi-transparent sprite forces alpha against 2nd targets.
            alpha_blend(fc, sc, eva, evb)
        } else if effects_allowed && blend_mode != 0 && bldcnt & (1 << fl) != 0 {
            match blend_mode {
                1 if bldcnt & (0x100 << sl) != 0 => alpha_blend(fc, sc, eva, evb),
                2 => brighten(fc, evy),
                3 => darken(fc, evy),
                _ => fc,
            }
        } else {
            fc
        };

        mem.framebuffer[row + x] = color;
    }
}

/// Window coordinate test: x1 = left/top (inclusive), x2 = right/bottom
/// (exclusive); x1 > x2 wraps.
fn in_window_range(x1: u8, x2: u8, v: u8) -> bool {
    if x1 <= x2 {
        (x1..x2).contains(&v)
    } else {
        v >= x1 || v < x2
    }
}

// Color-effect arithmetic models the hardware blend unit, not GBATEK's
// fractional notation: per-channel multiply-accumulate rounds half-up
// (+8 >> 4; the darken subtrahend rounds half-down, +7, so the result
// still rounds half-up), and green passes through a 6-bit lane (the
// real hardware LSB of that lane is PRAM bit 15, which we mask at
// palette fetch — see pal16).

fn alpha_blend(a: u16, b: u16, eva: u32, evb: u32) -> u16 {
    let (ar, ag, ab) = split6(a);
    let (br, bg, bb) = split6(b);
    let r = ((ar * eva + br * evb + 8) >> 4).min(31);
    let g = ((ag * eva + bg * evb + 8) >> 4).min(63);
    let b = ((ab * eva + bb * evb + 8) >> 4).min(31);
    join6(r, g, b)
}

fn brighten(c: u16, evy: u32) -> u16 {
    let (r, g, b) = split6(c);
    join6(
        r + (((31 - r) * evy + 8) >> 4),
        g + (((63 - g) * evy + 8) >> 4),
        b + (((31 - b) * evy + 8) >> 4),
    )
}

fn darken(c: u16, evy: u32) -> u16 {
    let (r, g, b) = split6(c);
    join6(
        r - ((r * evy + 7) >> 4),
        g - ((g * evy + 7) >> 4),
        b - ((b * evy + 7) >> 4),
    )
}

/// Channels as the blend unit sees them: 5-bit red/blue, 6-bit green
/// lane (5-bit value doubled; hardware ORs in PRAM bit 15 as the LSB,
/// which we strip at fetch).
fn split6(c: u16) -> (u32, u32, u32) {
    (
        (c & 31) as u32,
        (((c >> 5) & 31) as u32) << 1,
        ((c >> 10) & 31) as u32,
    )
}

fn join6(r: u32, g: u32, b: u32) -> u16 {
    (r | ((g >> 1) << 5) | (b << 10)) as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::Bus;

    #[test]
    fn mode3_pixel() {
        let mut mem = MemMap::new(vec![]);
        mem.write16(0x0400_0000, 0x0403); // mode 3, BG2 on
        mem.write16(0x0600_0000 + 2 * (5 + 7 * 240), 0x7C1F); // magenta at (5,7)
        render_scanline(&mut mem, 7);
        assert_eq!(mem.framebuffer[7 * 240 + 5], 0x7C1F);
        assert_eq!(mem.framebuffer[7 * 240 + 6], 0x0000);
    }

    #[test]
    fn mode0_text_tile() {
        let mut mem = MemMap::new(vec![]);
        mem.write16(0x0400_0000, 0x0100); // mode 0, BG0 on
                                          // BG0: char base 0, screen base block 31 (0xF800), 4bpp, 32x32.
        mem.write16(0x0400_0008, 31 << 8);
        // Palette 1, color 3 = red-ish.
        mem.write16(0x0500_0000 + (16 + 3) * 2, 0x001F);
        // Tile 1: all pixels = color 3.
        for row in 0..8 {
            mem.write32(0x0600_0000 + 32 + row * 4, 0x3333_3333);
        }
        // Map entry (0,0): tile 1, palette 1.
        mem.write16(0x0600_F800, (1 << 12) | 1);
        render_scanline(&mut mem, 0);
        assert_eq!(mem.framebuffer[0], 0x001F);
        // (8,0) is tile (1,0): map entry 0 -> tile 0 -> transparent -> backdrop.
        assert_eq!(mem.framebuffer[8], pal16(&mem, 0));
    }

    #[test]
    fn sprite_over_backdrop() {
        let mut mem = MemMap::new(vec![]);
        mem.write16(0x0400_0000, 0x1000); // mode 0, OBJ on
                                          // OBJ palette color 1 = green.
        mem.write16(0x0500_0200 + 2, 0x03E0);
        // Tile 1 in OBJ charblock: all color 1 (4bpp).
        for row in 0..8 {
            mem.write32(0x0601_0000 + 32 + row * 4, 0x1111_1111);
        }
        // Sprite 0: 8x8 at (10, 20), tile 1.
        mem.write16(0x0700_0000, 20); // attr0: y=20
        mem.write16(0x0700_0002, 10); // attr1: x=10
        mem.write16(0x0700_0004, 1); // attr2: tile 1
        render_scanline(&mut mem, 22);
        assert_eq!(mem.framebuffer[22 * 240 + 10], 0x03E0);
        assert_eq!(mem.framebuffer[22 * 240 + 18], 0x0000); // outside sprite
    }

    #[test]
    fn sprite_flips_use_obj_attr1_bits() {
        // Tile with one green pixel at texel (0, 0); the rest transparent.
        // Flips relocate the lit screen pixel: hflip = attr1 bit 12,
        // vflip = bit 13 (NOT the BG-entry bits 10/11).
        let mut mem = MemMap::new(vec![]);
        mem.write16(0x0400_0000, 0x1000); // mode 0, OBJ on
        mem.write16(0x0500_0200 + 2, 0x03E0);
        mem.write32(0x0601_0000 + 32, 0x0000_0001); // tile 1 row 0: texel 0 = color 1
        mem.write16(0x0700_0000, 20); // attr0: y=20
        mem.write16(0x0700_0002, 10); // attr1: x=10
        mem.write16(0x0700_0004, 1); // attr2: tile 1
        let lit = |mem: &mut MemMap, line: usize, x: usize| {
            render_scanline(mem, line);
            mem.framebuffer[line * 240 + x] == 0x03E0
        };
        assert!(lit(&mut mem, 20, 10)); // unflipped: top-left
        mem.write16(0x0700_0002, 10 | 0x1000); // hflip
        assert!(lit(&mut mem, 20, 17));
        mem.write16(0x0700_0002, 10 | 0x2000); // vflip
        assert!(lit(&mut mem, 27, 10));
        mem.write16(0x0700_0002, 10 | 0x3000); // both
        assert!(lit(&mut mem, 27, 17));
        // BG-entry flip bits must do nothing on sprites.
        mem.write16(0x0700_0002, 10 | 0x0C00);
        assert!(lit(&mut mem, 20, 10));
    }

    #[test]
    fn brightness_math() {
        assert_eq!(brighten(0x0000, 16), 0x7FFF);
        assert_eq!(darken(0x7FFF, 16), 0x0000);
        assert_eq!(brighten(0x0000, 0), 0x0000);
    }

    // Hardware blend-unit identities:
    // round-half-up MAC, 6-bit green lane.

    #[test]
    fn blend_identity_and_extremes() {
        // eva=16/evb=0 passes every color through exactly, including
        // the green lane round-trip.
        for c in [0x0000u16, 0x7FFF, 0x7C1F, 0x03E0, 0x1234, 0x5A5A] {
            assert_eq!(alpha_blend(c, 0x7FFF ^ c, 16, 0), c);
            assert_eq!(alpha_blend(0x7FFF ^ c, c, 0, 16), c);
        }
        // Saturation: white + white at full coefficients clamps.
        assert_eq!(alpha_blend(0x7FFF, 0x7FFF, 16, 16), 0x7FFF);
        // evy=0 is a no-op for both brightness directions.
        for c in [0x7FFF, 0x1234, 0x5A5A] {
            assert_eq!(brighten(c, 0), c);
            assert_eq!(darken(c, 0), c);
        }
    }

    #[test]
    fn blend_rounds_half_up() {
        // 50/50 of red 15 and 16: true value 15.5, hardware rounds up.
        // (15*8 + 16*8 + 8) >> 4 = 16.
        assert_eq!(alpha_blend(15, 16, 8, 8) & 31, 16);
        // Red 0 with 1 at evb=8: 0.5 rounds up to 1 (truncation gave 0).
        assert_eq!(alpha_blend(0, 1, 8, 8) & 31, 1);
        // Green's 6-bit lane rounds up only at fraction >= 12/16:
        // greens 15/16 at 8/8 -> S=124 in g6 (30*8+32*8=496; +8 >>4 =
        // 31; >>1 = 15): the half-step stays DOWN on green.
        assert_eq!((alpha_blend(15 << 5, 16 << 5, 8, 8) >> 5) & 31, 15);
        // ...but green 15/eva=4, 16/evb=12 (true 15.75) rounds up.
        assert_eq!((alpha_blend(15 << 5, 16 << 5, 4, 12) >> 5) & 31, 16);
    }

    #[test]
    fn brightness_rounds() {
        // Brighten red 0 by 1/16: (31*1 + 8) >> 4 = 2 (truncation: 1).
        assert_eq!(brighten(0, 1) & 31, 2);
        // Darken red 31 by 1/16: 31 - ((31 + 7) >> 4) = 29.
        assert_eq!(darken(31, 1) & 31, 29);
        // Green lane: brighten green 0 by 1/16 -> g6 = (63+8)>>4 = 4,
        // back to 5-bit = 2.
        assert_eq!((brighten(0, 1) >> 5) & 31, 2);
    }
}
