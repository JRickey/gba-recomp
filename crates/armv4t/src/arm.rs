//! ARM (32-bit) instruction decoding for ARMv4T.
//!
//! Decode order matters: several encodings live inside others' don't-care
//! space. BX overlaps TEQ-without-S; MUL/MULL/SWP and the halfword transfers
//! all live in the data-processing class with bits 7 and 4 set; MRS/MSR
//! occupy the compare-ops-without-S hole. Patterns are tested most-specific
//! first, mirroring the ARM7TDMI datasheet's instruction-set map.

use crate::*;

/// Decode one ARM instruction located at `addr`.
pub fn decode_arm(w: u32, addr: u32) -> Instr {
    let cond = Cond::from_bits((w >> 28) as u8);
    let op = if cond == Cond::Nv {
        // 0b1111 condition space is UNPREDICTABLE on ARMv4 (no BLX/PLD yet).
        Op::Undefined { raw: w }
    } else {
        decode_op(w, addr)
    };
    Instr {
        addr,
        cond,
        op,
        thumb: false,
    }
}

fn decode_op(w: u32, addr: u32) -> Op {
    match bits(w, 25, 27) {
        0b000 => decode_class_000(w),
        0b001 => decode_dp_or_psr(w, /*imm_class=*/ true),
        0b010 => decode_single_transfer(w, /*reg_offset=*/ false),
        0b011 => {
            if bit(w, 4) {
                // Architecturally the undefined-instruction space.
                Op::Undefined { raw: w }
            } else {
                decode_single_transfer(w, /*reg_offset=*/ true)
            }
        }
        0b100 => Op::BlockMem {
            load: bit(w, 20),
            rn: bits(w, 16, 19) as Reg,
            rlist: (w & 0xFFFF) as u16,
            pre: bit(w, 24),
            up: bit(w, 23),
            s_bit: bit(w, 22),
            writeback: bit(w, 21),
        },
        0b101 => {
            let off = ((bits(w, 0, 23) << 8) as i32 >> 8) << 2;
            Op::Branch {
                link: bit(w, 24),
                target: addr.wrapping_add(8).wrapping_add(off as u32),
            }
        }
        // 0b110: LDC/STC, 0b111 without bit 24: CDP/MCR/MRC — the target console has no
        // coprocessors; all take the undefined-instruction trap.
        0b110 => Op::Undefined { raw: w },
        0b111 => {
            if bit(w, 24) {
                Op::Swi {
                    imm: bits(w, 0, 23),
                }
            } else {
                Op::Undefined { raw: w }
            }
        }
        _ => unreachable!(),
    }
}

/// Class 000: data processing (register operand), BX, multiplies, SWP,
/// halfword/signed transfers, MRS/MSR-from-register.
fn decode_class_000(w: u32) -> Op {
    if w & 0x0FFF_FFF0 == 0x012F_FF10 {
        return Op::Bx {
            rm: bits(w, 0, 3) as Reg,
        };
    }
    if w & 0x0FC0_00F0 == 0x0000_0090 {
        return Op::Mul {
            acc: bit(w, 21),
            s: bit(w, 20),
            rd: bits(w, 16, 19) as Reg,
            rn: bits(w, 12, 15) as Reg,
            rs: bits(w, 8, 11) as Reg,
            rm: bits(w, 0, 3) as Reg,
        };
    }
    if w & 0x0F80_00F0 == 0x0080_0090 {
        return Op::MulLong {
            signed: bit(w, 22),
            acc: bit(w, 21),
            s: bit(w, 20),
            rd_hi: bits(w, 16, 19) as Reg,
            rd_lo: bits(w, 12, 15) as Reg,
            rs: bits(w, 8, 11) as Reg,
            rm: bits(w, 0, 3) as Reg,
        };
    }
    if w & 0x0FB0_0FF0 == 0x0100_0090 {
        return Op::Swap {
            byte: bit(w, 22),
            rd: bits(w, 12, 15) as Reg,
            rm: bits(w, 0, 3) as Reg,
            rn: bits(w, 16, 19) as Reg,
        };
    }
    if bit(w, 7) && bit(w, 4) {
        // Halfword / signed transfers (bits 6-5 = SH; 00 here would be a
        // multiply/SWP pattern, all consumed above).
        return decode_half_transfer(w);
    }
    decode_dp_or_psr(w, /*imm_class=*/ false)
}

fn decode_half_transfer(w: u32) -> Op {
    let load = bit(w, 20);
    let (width, signed) = match bits(w, 5, 6) {
        0b01 => (MemWidth::Half, false),
        0b10 => (MemWidth::Byte, true),
        0b11 => (MemWidth::Half, true),
        // 0b00 unreachable: that bit pattern decodes as multiply/SWP.
        _ => return Op::Undefined { raw: w },
    };
    if !load && signed {
        // "Store signed" encodings are LDRD/STRD on ARMv5E; undefined on v4.
        return Op::Undefined { raw: w };
    }
    let offset = if bit(w, 22) {
        MemOffset::Imm((bits(w, 8, 11) << 4 | bits(w, 0, 3)) as u16)
    } else {
        MemOffset::Reg {
            rm: bits(w, 0, 3) as Reg,
            shift: Shift::NONE,
        }
    };
    Op::Mem {
        load,
        width,
        signed,
        rd: bits(w, 12, 15) as Reg,
        rn: bits(w, 16, 19) as Reg,
        offset,
        pre: bit(w, 24),
        up: bit(w, 23),
        writeback: bit(w, 21),
    }
}

/// Data processing, or the PSR-transfer instructions hiding in the
/// compare-ops-without-S encoding hole (opcodes 8..=11 with S=0).
fn decode_dp_or_psr(w: u32, imm_class: bool) -> Op {
    let opcode = bits(w, 21, 24) as u8;
    let s = bit(w, 20);

    if !s && (8..=11).contains(&opcode) {
        let spsr = bit(w, 22);
        return match (opcode & 1 != 0, imm_class) {
            // MRS: opcode 8 (CPSR) / 10 (SPSR); register class only.
            (false, false) if w & 0x0FBF_0FFF == 0x010F_0000 => Op::Mrs {
                spsr,
                rd: bits(w, 12, 15) as Reg,
            },
            // MSR from register: opcode 9 / 11.
            (true, false) if w & 0x0FB0_FFF0 == 0x0120_F000 => Op::MsrReg {
                spsr,
                fields: bits(w, 16, 19) as u8,
                rm: bits(w, 0, 3) as Reg,
            },
            // MSR from immediate.
            (true, true) if w & 0x0FB0_F000 == 0x0320_F000 => {
                let ror = (bits(w, 8, 11) * 2) as u8;
                Op::MsrImm {
                    spsr,
                    fields: bits(w, 16, 19) as u8,
                    value: bits(w, 0, 7).rotate_right(ror as u32),
                }
            }
            _ => Op::Undefined { raw: w },
        };
    }

    let op2 = if imm_class {
        let ror = (bits(w, 8, 11) * 2) as u8;
        Operand2::Imm {
            value: bits(w, 0, 7).rotate_right(ror as u32),
            ror,
        }
    } else {
        let rm = bits(w, 0, 3) as Reg;
        let shift = if bit(w, 4) {
            // Register-specified shift (bit 7 is guaranteed 0 here; bit 7 set
            // was consumed by the multiply/halfword patterns).
            Shift::Reg {
                kind: ShiftKind::from_bits(bits(w, 5, 6) as u8),
                rs: bits(w, 8, 11) as Reg,
            }
        } else {
            Shift::Imm {
                kind: ShiftKind::from_bits(bits(w, 5, 6) as u8),
                amount: bits(w, 7, 11) as u8,
            }
        };
        Operand2::Reg { rm, shift }
    };

    Op::Alu {
        op: AluOp::from_bits(opcode),
        s,
        rd: bits(w, 12, 15) as Reg,
        rn: bits(w, 16, 19) as Reg,
        op2,
    }
}

fn decode_single_transfer(w: u32, reg_offset: bool) -> Op {
    let offset = if reg_offset {
        MemOffset::Reg {
            rm: bits(w, 0, 3) as Reg,
            shift: Shift::Imm {
                kind: ShiftKind::from_bits(bits(w, 5, 6) as u8),
                amount: bits(w, 7, 11) as u8,
            },
        }
    } else {
        MemOffset::Imm(bits(w, 0, 11) as u16)
    };
    Op::Mem {
        load: bit(w, 20),
        width: if bit(w, 22) {
            MemWidth::Byte
        } else {
            MemWidth::Word
        },
        signed: false,
        rd: bits(w, 12, 15) as Reg,
        rn: bits(w, 16, 19) as Reg,
        offset,
        pre: bit(w, 24),
        up: bit(w, 23),
        writeback: bit(w, 21),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(w: u32) -> Op {
        decode_arm(w, 0x0800_0000).op
    }

    #[test]
    fn mov_imm() {
        // mov r0, #0
        assert_eq!(
            op(0xE3A0_0000),
            Op::Alu {
                op: AluOp::Mov,
                s: false,
                rd: 0,
                rn: 0,
                op2: Operand2::imm(0)
            }
        );
    }

    #[test]
    fn imm_rotation() {
        // mov r0, #0x80000000 (imm8=2, ror=2*1... encoding: 0xE3A00102 = mov r0, #0x80000000)
        match op(0xE3A0_0102) {
            Op::Alu {
                op: AluOp::Mov,
                op2: Operand2::Imm { value, ror },
                ..
            } => {
                assert_eq!(value, 0x8000_0000);
                assert_eq!(ror, 2);
            }
            other => panic!("decoded {other:?}"),
        }
    }

    #[test]
    fn bx_lr() {
        assert_eq!(op(0xE12F_FF1E), Op::Bx { rm: LR });
    }

    #[test]
    fn bx_beats_teq() {
        // TEQ-without-S space must not swallow BX.
        assert!(matches!(op(0xE12F_FF11), Op::Bx { rm: 1 }));
    }

    #[test]
    fn push_pop() {
        // push {r4, lr} == stmdb sp!, {r4, lr}
        assert_eq!(
            op(0xE92D_4010),
            Op::BlockMem {
                load: false,
                rn: SP,
                rlist: 0x4010,
                pre: true,
                up: false,
                s_bit: false,
                writeback: true,
            }
        );
        // pop {r4, pc} == ldmia sp!, {r4, pc}
        assert_eq!(
            op(0xE8BD_8010),
            Op::BlockMem {
                load: true,
                rn: SP,
                rlist: 0x8010,
                pre: false,
                up: true,
                s_bit: false,
                writeback: true,
            }
        );
    }

    #[test]
    fn branch_target_resolution() {
        // Typical cartridge header entry: b 0x080000C0 encoded at 0x08000000.
        assert_eq!(
            op(0xEA00_002E),
            Op::Branch {
                link: false,
                target: 0x0800_00C0
            }
        );
        // Backwards branch: b . (infinite loop) = 0xEAFFFFFE.
        assert_eq!(
            op(0xEAFF_FFFE),
            Op::Branch {
                link: false,
                target: 0x0800_0000
            }
        );
        // bl with the link bit.
        assert!(matches!(
            op(0xEB00_002E),
            Op::Branch {
                link: true,
                target: 0x0800_00C0
            }
        ));
    }

    #[test]
    fn ldr_literal() {
        // ldr r1, [pc, #0x18]
        let i = decode_arm(0xE59F_1018, 0x0800_0000);
        assert!(matches!(
            i.op,
            Op::Mem {
                load: true,
                width: MemWidth::Word,
                signed: false,
                rd: 1,
                rn: PC,
                offset: MemOffset::Imm(0x18),
                pre: true,
                up: true,
                writeback: false,
            }
        ));
        // Literal address = addr + 8 + 0x18.
        assert_eq!(i.literal_addr(), Some(0x0800_0020));
    }

    #[test]
    fn mul() {
        // mul r1, r2, r3
        assert_eq!(
            op(0xE001_0392),
            Op::Mul {
                acc: false,
                s: false,
                rd: 1,
                rn: 0,
                rs: 3,
                rm: 2
            }
        );
    }

    #[test]
    fn umull() {
        // umull r0, r1, r2, r3: rd_lo=r0, rd_hi=r1, rm=r2, rs=r3
        assert_eq!(
            op(0xE081_0392),
            Op::MulLong {
                signed: false,
                acc: false,
                s: false,
                rd_hi: 1,
                rd_lo: 0,
                rs: 3,
                rm: 2
            }
        );
    }

    #[test]
    fn psr_transfers() {
        // mrs r0, cpsr
        assert_eq!(op(0xE10F_0000), Op::Mrs { spsr: false, rd: 0 });
        // msr cpsr_fc, r0 (fields f|c = 0b1001)
        assert_eq!(
            op(0xE129_F000),
            Op::MsrReg {
                spsr: false,
                fields: 0b1001,
                rm: 0
            }
        );
        // msr spsr_fsxc, r0
        assert_eq!(
            op(0xE16F_F000),
            Op::MsrReg {
                spsr: true,
                fields: 0b1111,
                rm: 0
            }
        );
    }

    #[test]
    fn halfword_transfers() {
        // strh r0, [r1] = 0xE1C100B0
        assert_eq!(
            op(0xE1C1_00B0),
            Op::Mem {
                load: false,
                width: MemWidth::Half,
                signed: false,
                rd: 0,
                rn: 1,
                offset: MemOffset::Imm(0),
                pre: true,
                up: true,
                writeback: false,
            }
        );
        // ldrsh r2, [r3, #4] = 0xE1D320F4
        assert_eq!(
            op(0xE1D3_20F4),
            Op::Mem {
                load: true,
                width: MemWidth::Half,
                signed: true,
                rd: 2,
                rn: 3,
                offset: MemOffset::Imm(4),
                pre: true,
                up: true,
                writeback: false,
            }
        );
    }

    #[test]
    fn swi() {
        // swi 0x060000 — BIOS division call from ARM state.
        assert_eq!(op(0xEF06_0000), Op::Swi { imm: 0x06_0000 });
    }

    #[test]
    fn swp() {
        // swp r0, r1, [r2]
        assert_eq!(
            op(0xE102_0091),
            Op::Swap {
                byte: false,
                rd: 0,
                rm: 1,
                rn: 2
            }
        );
    }

    #[test]
    fn conditions() {
        // movne r0, #0
        assert_eq!(decode_arm(0x13A0_0000, 0).cond, Cond::Ne);
        // 0b1111 condition is undefined on v4.
        assert!(matches!(
            decode_arm(0xF3A0_0000, 0).op,
            Op::Undefined { .. }
        ));
    }

    #[test]
    fn coprocessor_space_undefined() {
        // mcr p15 (exists on ARM9, not on ARM7TDMI): 0xEE010F10
        assert!(matches!(op(0xEE01_0F10), Op::Undefined { .. }));
    }

    #[test]
    fn register_shifted_operand() {
        // add r0, r1, r2, lsl r3
        assert_eq!(
            op(0xE081_0312),
            Op::Alu {
                op: AluOp::Add,
                s: false,
                rd: 0,
                rn: 1,
                op2: Operand2::Reg {
                    rm: 2,
                    shift: Shift::Reg {
                        kind: ShiftKind::Lsl,
                        rs: 3
                    },
                },
            }
        );
    }
}
