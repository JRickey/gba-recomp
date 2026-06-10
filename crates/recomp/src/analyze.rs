//! Static analysis: basic-block discovery over ROM code.
//!
//! Recursive traversal from the cartridge entry point, in both ISAs.
//! Direct branches contribute both edges; BL contributes the call target
//! and the fall-through; BX/computed-PC/`pop {pc}` end a block with no
//! static successor (the runtime lookup table covers them). Literal pools
//! loaded via `ldr rX, [pc, #imm]` are scanned for ROM code pointers
//! (bit 0 = Thumb), which seeds function pointers, interworking thunks,
//! and the IRQ handler — translate-everything makes a wrong guess
//! harmless: unreached blocks just bloat the output.
//!
//! Misclassification can't corrupt execution for one more reason: the
//! emitted v0 blocks verify the PC after every instruction and bail to
//! the dispatcher on any divergence.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use armv4t::{decode_arm, decode_thumb, Cond, Instr, Op, PC};

pub const ROM_BASE: u32 = 0x0800_0000;

/// A discovered basic block (ROM only).
#[derive(Debug, Clone)]
pub struct Block {
    pub addr: u32,
    pub thumb: bool,
    /// Decoded instructions, in order.
    pub instrs: Vec<Instr>,
}

impl Block {
    pub fn key(&self) -> u32 {
        self.addr | self.thumb as u32
    }
}

pub struct Analysis {
    pub blocks: Vec<Block>,
}

/// True when `addr` lies inside the ROM image.
fn in_rom(rom: &[u8], addr: u32) -> bool {
    let off = addr.wrapping_sub(ROM_BASE);
    (addr >> 24) == 0x08 && (off as usize) < rom.len()
}

fn read16(rom: &[u8], addr: u32) -> u16 {
    let o = (addr - ROM_BASE) as usize;
    u16::from_le_bytes([rom[o], rom[o + 1]])
}

fn read32(rom: &[u8], addr: u32) -> u32 {
    let o = (addr - ROM_BASE) as usize;
    u32::from_le_bytes([rom[o], rom[o + 1], rom[o + 2], rom[o + 3]])
}

/// Static successors of an instruction, plus whether execution can fall
/// through to the next instruction.
fn successors(instr: &Instr, thumb: bool) -> (Vec<u32>, bool) {
    let cond_can_fall = instr.cond != Cond::Al;
    match instr.op {
        Op::Branch { link, target } => {
            let key = target | thumb as u32;
            // BL falls through (it returns); conditional B falls through.
            (vec![key], link || cond_can_fall)
        }
        Op::ThumbBlLow { .. } => (vec![], true), // fused below at block level
        Op::Bx { .. } => (vec![], cond_can_fall),
        Op::Swi { .. } => (vec![], true),
        Op::Undefined { .. } => (vec![], false),
        Op::Alu { rd, .. } if rd == PC => (vec![], cond_can_fall),
        Op::Mem { load: true, rd, .. } if rd == PC => (vec![], cond_can_fall),
        Op::BlockMem { load: true, rlist, .. } if rlist & (1 << PC) != 0 => {
            (vec![], cond_can_fall)
        }
        _ => (vec![], true),
    }
}

/// True if the instruction always diverts control flow (ends a block).
fn ends_block(instr: &Instr) -> bool {
    matches!(
        instr.op,
        Op::Branch { .. }
            | Op::Bx { .. }
            | Op::ThumbBlLow { .. }
            | Op::Undefined { .. }
    ) || matches!(instr.op, Op::Alu { rd, .. } if rd == PC)
        || matches!(instr.op, Op::Mem { load: true, rd, .. } if rd == PC)
        || matches!(instr.op, Op::BlockMem { load: true, rlist, .. } if rlist & (1 << PC) != 0)
}

pub fn analyze(rom: &[u8]) -> Analysis {
    let mut queue: VecDeque<u32> = VecDeque::new();
    let mut seen: BTreeSet<u32> = BTreeSet::new();
    let mut blocks: BTreeMap<u32, Block> = BTreeMap::new();

    // Entry: the header branch at 0x08000000 (ARM).
    queue.push_back(ROM_BASE);

    while let Some(key) = queue.pop_front() {
        if !seen.insert(key) {
            continue;
        }
        let thumb = key & 1 != 0;
        let start = key & !1;
        if !in_rom(rom, start) || !in_rom(rom, start + 3) {
            continue;
        }

        let mut instrs = Vec::new();
        let mut addr = start;
        let mut prev: Option<Instr> = None;
        loop {
            if !in_rom(rom, addr) || !in_rom(rom, addr + 3) || instrs.len() >= 4096 {
                break;
            }
            let instr = if thumb {
                decode_thumb(read16(rom, addr), addr)
            } else {
                decode_arm(read32(rom, addr), addr)
            };

            // Harvest literal pools: word literals that look like ROM code
            // pointers seed new entries (bit 0 selects the ISA).
            if let Some(lit) = instr.literal_addr() {
                if in_rom(rom, lit) && lit + 3 >= lit && in_rom(rom, lit + 3) {
                    let v = read32(rom, lit);
                    let t = v & !1;
                    if in_rom(rom, t) && t & 1 == 0 && (v & 1 == 1 || v & 3 == 0) {
                        queue.push_back(if v & 1 == 1 { t | 1 } else { t });
                    }
                }
            }

            // Fused Thumb BL: enqueue the call target.
            if let Op::ThumbBlLow { .. } = instr.op {
                if let Some(p) = &prev {
                    if let Some((target, _)) = armv4t::fuse_thumb_bl(p, &instr) {
                        if in_rom(rom, target & !1) {
                            queue.push_back((target & !1) | 1);
                        }
                        // The pair returns: continue after the BL.
                        queue.push_back((addr + 2) | 1);
                    }
                }
            }

            let (succs, falls) = successors(&instr, thumb);
            for s in succs {
                if in_rom(rom, s & !1) {
                    queue.push_back(s);
                }
            }

            let end = ends_block(&instr);
            addr += instr.size();
            prev = Some(instr.clone());
            instrs.push(instr);

            if end {
                break;
            }
            if !falls {
                break;
            }
        }
        // Fall-through continuation for conditional block endings.
        if let Some(last) = instrs.last() {
            let (_, falls) = successors(last, thumb);
            if falls && in_rom(rom, addr) {
                queue.push_back(addr | thumb as u32);
            }
        }

        if !instrs.is_empty() {
            blocks.insert(key, Block { addr: start, thumb, instrs });
        }
    }

    Analysis { blocks: blocks.into_values().collect() }
}
