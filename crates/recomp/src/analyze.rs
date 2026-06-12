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

/// Composite code view: ROM plus optional profiled RAM snapshots and an
/// optional real BIOS image (region 0, fixed like ROM).
pub struct View<'a> {
    pub rom: &'a [u8],
    pub ewram: Option<&'a [u8]>,
    pub iwram: Option<&'a [u8]>,
    pub bios: Option<&'a [u8]>,
}

impl View<'_> {
    fn slice(&self, addr: u32) -> Option<(&[u8], usize)> {
        match addr >> 24 {
            0x08 => {
                let off = (addr & 0x01FF_FFFF) as usize;
                (off < self.rom.len()).then_some((self.rom, off))
            }
            0x02 => self.ewram.map(|m| (m, (addr & 0x3_FFFF) as usize)),
            0x03 => self.iwram.map(|m| (m, (addr & 0x7FFF) as usize)),
            0x00 if addr < 0x4000 => self.bios.map(|m| (m, addr as usize)),
            _ => None,
        }
    }

    fn valid(&self, addr: u32) -> bool {
        matches!(self.slice(addr), Some((m, o)) if o + 3 < m.len() || o < m.len())
    }

    pub fn read16(&self, addr: u32) -> u16 {
        let (m, o) = self.slice(addr).unwrap();
        u16::from_le_bytes([m[o], m[o + 1]])
    }

    pub fn read32(&self, addr: u32) -> u32 {
        let (m, o) = self.slice(addr).unwrap();
        u32::from_le_bytes([m[o], m[o + 1], m[o + 2], m[o + 3]])
    }

    /// A contiguous byte range, if it lies fully inside one region.
    pub fn bytes(&self, addr: u32, len: usize) -> Option<&[u8]> {
        let (m, o) = self.slice(addr)?;
        m.get(o..o + len)
    }
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
        Op::BlockMem {
            load: true, rlist, ..
        } if rlist & (1 << PC) != 0 => (vec![], cond_can_fall),
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
            // SWI is a control transfer: Halt-class calls flip machine
            // sleep state that must gate execution of the NEXT
            // instruction (the dispatch loop owns halt/IRQ interleaving).
            // Emitting it mid-block reorders the following instructions
            // ahead of the halt — a spin-on-flag loop after `swi Halt`
            // then reads its flag before sleeping and the trailing halt
            // eats real time after wake (3d624dba intro scanline
            // effects landed 125 lines late).
            | Op::Swi { .. }
    ) || matches!(instr.op, Op::Alu { rd, .. } if rd == PC)
        || matches!(instr.op, Op::Mem { load: true, rd, .. } if rd == PC)
        || matches!(instr.op, Op::BlockMem { load: true, rlist, .. } if rlist & (1 << PC) != 0)
}

pub fn analyze(view: &View, seeds: &[u32]) -> Analysis {
    let mut queue: VecDeque<u32> = VecDeque::new();
    let mut seen: BTreeSet<u32> = BTreeSet::new();
    let mut blocks: BTreeMap<u32, Block> = BTreeMap::new();
    let seed_set: BTreeSet<u32> = seeds.iter().copied().collect();

    // RAM holds data that decodes as plausible instructions, so speculative
    // discovery there explodes combinatorially (gigabytes of dead C). RAM
    // translation stays anchored to *observed* execution: only profiled
    // entries start RAM blocks, and edges out of RAM may target ROM or
    // other profiled entries only.
    // Edges into ROM are always fine. Edges into RAM are fine when they
    // come from RAM code we already trust (anchored chains from observed
    // execution); ROM-side discoveries may only target RAM at profiled
    // entries — that is where snapshot-data-as-code explodes.
    let may_enqueue = |target_key: u32, from_ram: bool, seed_set: &BTreeSet<u32>| -> bool {
        let region = (target_key & !1) >> 24;
        // ROM and BIOS are fixed images: speculative discovery is safe
        // (junk blocks are never dispatched). View::valid gates BIOS
        // targets on an image actually being present.
        if region == 8 || region == 0 {
            return true;
        }
        from_ram || seed_set.contains(&target_key)
    };

    // Entry: the header branch at 0x08000000 (ARM), plus profiled seeds.
    queue.push_back(ROM_BASE);
    queue.extend(seeds.iter().copied());

    while let Some(key) = queue.pop_front() {
        if !seen.insert(key) {
            continue;
        }
        let thumb = key & 1 != 0;
        let start = key & !1;
        if !view.valid(start) || !view.valid(start + 3) {
            continue;
        }

        let from_ram = !matches!(start >> 24, 0x08 | 0x00);
        let max_len = if from_ram { 512 } else { 4096 };
        let mut instrs = Vec::new();
        let mut addr = start;
        let mut prev: Option<Instr> = None;
        loop {
            if !view.valid(addr) || !view.valid(addr + 3) || instrs.len() >= max_len {
                break;
            }
            let instr = if thumb {
                decode_thumb(view.read16(addr), addr)
            } else {
                decode_arm(view.read32(addr), addr)
            };

            // Harvest literal pools: word literals that look like ROM code
            // pointers seed new entries (bit 0 selects the ISA).
            if !from_ram {
                if let Some(lit) = instr.literal_addr() {
                    if view.valid(lit) && view.valid(lit + 3) {
                        let v = view.read32(lit);
                        let t = v & !1;
                        // Harvested pointers may target ROM, or RAM code we
                        // actually observed executing.
                        if view.valid(t) && t & 1 == 0 && (v & 1 == 1 || v & 3 == 0) {
                            let key = if v & 1 == 1 { t | 1 } else { t };
                            if may_enqueue(key, false, &seed_set) {
                                queue.push_back(key);
                            }
                        }
                    }
                }
            }

            // Fused Thumb BL: enqueue the call target.
            if let Op::ThumbBlLow { .. } = instr.op {
                if let Some(p) = &prev {
                    if let Some((target, _)) = armv4t::fuse_thumb_bl(p, &instr) {
                        let tkey = (target & !1) | 1;
                        if view.valid(target & !1) && may_enqueue(tkey, from_ram, &seed_set) {
                            queue.push_back(tkey);
                        }
                        // The pair returns: continue after the BL.
                        if may_enqueue((addr + 2) | 1, from_ram, &seed_set) {
                            queue.push_back((addr + 2) | 1);
                        }
                    }
                }
            }

            let (succs, falls) = successors(&instr, thumb);
            for s in succs {
                if view.valid(s & !1) && may_enqueue(s, from_ram, &seed_set) {
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
            if falls && view.valid(addr) && may_enqueue(addr | thumb as u32, from_ram, &seed_set) {
                queue.push_back(addr | thumb as u32);
            }
        }

        if !instrs.is_empty() {
            blocks.insert(
                key,
                Block {
                    addr: start,
                    thumb,
                    instrs,
                },
            );
        }
    }

    Analysis {
        blocks: blocks.into_values().collect(),
    }
}
