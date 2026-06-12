//! Thumb (16-bit) instruction decoding for ARMv4T.
//!
//! Each of the 19 Thumb formats maps onto the shared [`Op`] model — every
//! Thumb instruction except the BL halves has an exact ARM equivalent.
//! Notable Thumb-isms preserved by the mapping:
//! - most ALU operations set flags unconditionally (`s: true`);
//! - format 5 hi-register ADD/MOV/CMP do not set flags (except CMP);
//! - `neg rd, rs` becomes `rsbs rd, rs, #0`;
//! - shifts-by-register become `movs rd, rd, <kind> rs`;
//! - push/pop become `stmdb sp!` / `ldmia sp!`.

use crate::*;

/// Decode one Thumb instruction located at `addr`.
pub fn decode_thumb(h: u16, addr: u32) -> Instr {
    let (cond, op) = decode_op(h as u32, addr);
    Instr {
        addr,
        cond,
        op,
        thumb: true,
    }
}

fn decode_op(h: u32, addr: u32) -> (Cond, Op) {
    let al = Cond::Al;
    match bits(h, 13, 15) {
        0b000 => {
            if bits(h, 11, 12) == 0b11 {
                // Format 2: add/sub rd, rs, rn|#imm3
                let imm = bit(h, 10);
                let op = if bit(h, 9) { AluOp::Sub } else { AluOp::Add };
                let val = bits(h, 6, 8);
                let op2 = if imm {
                    Operand2::imm(val)
                } else {
                    Operand2::reg(val as Reg)
                };
                (
                    al,
                    Op::Alu {
                        op,
                        s: true,
                        rd: (h & 7) as Reg,
                        rn: bits(h, 3, 5) as Reg,
                        op2,
                    },
                )
            } else {
                // Format 1: lsl/lsr/asr rd, rs, #imm5 -> movs rd, rs, <kind> #imm5
                // (amount 0 for lsr/asr means 32, same raw convention as ARM.)
                let kind = ShiftKind::from_bits(bits(h, 11, 12) as u8);
                (
                    al,
                    Op::Alu {
                        op: AluOp::Mov,
                        s: true,
                        rd: (h & 7) as Reg,
                        rn: 0,
                        op2: Operand2::Reg {
                            rm: bits(h, 3, 5) as Reg,
                            shift: Shift::Imm {
                                kind,
                                amount: bits(h, 6, 10) as u8,
                            },
                        },
                    },
                )
            }
        }
        0b001 => {
            // Format 3: mov/cmp/add/sub rd, #imm8
            let rd = bits(h, 8, 10) as Reg;
            let imm = Operand2::imm(h & 0xFF);
            let (op, rn) = match bits(h, 11, 12) {
                0b00 => (AluOp::Mov, 0),
                0b01 => (AluOp::Cmp, rd),
                0b10 => (AluOp::Add, rd),
                _ => (AluOp::Sub, rd),
            };
            (
                al,
                Op::Alu {
                    op,
                    s: true,
                    rd,
                    rn,
                    op2: imm,
                },
            )
        }
        0b010 => decode_group_010(h, addr),
        0b011 => {
            // Format 9: ldr/str{b} rd, [rb, #imm5]
            let byte = bit(h, 12);
            let imm5 = bits(h, 6, 10);
            (
                al,
                Op::Mem {
                    load: bit(h, 11),
                    width: if byte { MemWidth::Byte } else { MemWidth::Word },
                    signed: false,
                    rd: (h & 7) as Reg,
                    rn: bits(h, 3, 5) as Reg,
                    offset: MemOffset::Imm((if byte { imm5 } else { imm5 * 4 }) as u16),
                    pre: true,
                    up: true,
                    writeback: false,
                },
            )
        }
        0b100 => {
            if bit(h, 12) {
                // Format 11: ldr/str rd, [sp, #imm8*4]
                (
                    al,
                    Op::Mem {
                        load: bit(h, 11),
                        width: MemWidth::Word,
                        signed: false,
                        rd: bits(h, 8, 10) as Reg,
                        rn: SP,
                        offset: MemOffset::Imm(((h & 0xFF) * 4) as u16),
                        pre: true,
                        up: true,
                        writeback: false,
                    },
                )
            } else {
                // Format 10: ldrh/strh rd, [rb, #imm5*2]
                (
                    al,
                    Op::Mem {
                        load: bit(h, 11),
                        width: MemWidth::Half,
                        signed: false,
                        rd: (h & 7) as Reg,
                        rn: bits(h, 3, 5) as Reg,
                        offset: MemOffset::Imm((bits(h, 6, 10) * 2) as u16),
                        pre: true,
                        up: true,
                        writeback: false,
                    },
                )
            }
        }
        0b101 => {
            if bit(h, 12) {
                decode_group_misc(h)
            } else {
                // Format 12: add rd, pc|sp, #imm8*4 (adr / add-from-sp).
                // ADR uses (PC & !3) as the base; since the decoder knows
                // the address, the alignment is folded into the immediate
                // (wraps when PC ≡ 2 mod 4 and imm is 0 — consumers add
                // with wrapping semantics).
                let rn = if bit(h, 11) { SP } else { PC };
                let mut imm = (h & 0xFF) * 4;
                if rn == PC {
                    imm = imm.wrapping_sub(addr & 2);
                }
                (
                    al,
                    Op::Alu {
                        op: AluOp::Add,
                        s: false,
                        rd: bits(h, 8, 10) as Reg,
                        rn,
                        op2: Operand2::imm(imm),
                    },
                )
            }
        }
        0b110 => {
            if bit(h, 12) {
                // Format 16/17: conditional branch or SWI.
                let cond_bits = bits(h, 8, 11) as u8;
                match cond_bits {
                    0xF => (al, Op::Swi { imm: h & 0xFF }),
                    0xE => (al, Op::Undefined { raw: h }),
                    _ => {
                        let off = ((h & 0xFF) as i8 as i32) << 1;
                        (
                            Cond::from_bits(cond_bits),
                            Op::Branch {
                                link: false,
                                target: addr.wrapping_add(4).wrapping_add(off as u32),
                            },
                        )
                    }
                }
            } else {
                // Format 15: stmia/ldmia rb!, {rlist}
                (
                    al,
                    Op::BlockMem {
                        load: bit(h, 11),
                        rn: bits(h, 8, 10) as Reg,
                        rlist: (h & 0xFF) as u16,
                        pre: false,
                        up: true,
                        s_bit: false,
                        writeback: true,
                    },
                )
            }
        }
        0b111 => match bits(h, 11, 12) {
            0b00 => {
                // Format 18: unconditional branch.
                let off = ((bits(h, 0, 10) << 21) as i32 >> 21) << 1;
                (
                    al,
                    Op::Branch {
                        link: false,
                        target: addr.wrapping_add(4).wrapping_add(off as u32),
                    },
                )
            }
            0b10 => {
                // Format 19, first half: lr := pc + (signext(off11) << 12).
                let off = ((bits(h, 0, 10) << 21) as i32 >> 21) << 12;
                (
                    al,
                    Op::ThumbBlHigh {
                        lr_partial: addr.wrapping_add(4).wrapping_add(off as u32),
                    },
                )
            }
            0b11 => (
                al,
                Op::ThumbBlLow {
                    off: bits(h, 0, 10) as u16,
                },
            ),
            // 0b01 is the BLX suffix on ARMv5T; undefined on v4T.
            _ => (al, Op::Undefined { raw: h }),
        },
        _ => unreachable!(),
    }
}

/// Top bits 010: formats 4 (ALU), 5 (hi-reg/BX), 6 (PC-relative load),
/// 7/8 (register-offset loads/stores).
fn decode_group_010(h: u32, _addr: u32) -> (Cond, Op) {
    let al = Cond::Al;
    if bits(h, 10, 12) == 0b000 {
        // Format 4: ALU operations rd, rs (all set flags).
        let rd = (h & 7) as Reg;
        let rs = bits(h, 3, 5) as Reg;
        let alu = |op: AluOp, rn: Reg, op2: Operand2| Op::Alu {
            op,
            s: true,
            rd,
            rn,
            op2,
        };
        let shift = |kind: ShiftKind| {
            alu(
                AluOp::Mov,
                0,
                Operand2::Reg {
                    rm: rd,
                    shift: Shift::Reg { kind, rs },
                },
            )
        };
        let op = match bits(h, 6, 9) {
            0x0 => alu(AluOp::And, rd, Operand2::reg(rs)),
            0x1 => alu(AluOp::Eor, rd, Operand2::reg(rs)),
            0x2 => shift(ShiftKind::Lsl),
            0x3 => shift(ShiftKind::Lsr),
            0x4 => shift(ShiftKind::Asr),
            0x5 => alu(AluOp::Adc, rd, Operand2::reg(rs)),
            0x6 => alu(AluOp::Sbc, rd, Operand2::reg(rs)),
            0x7 => shift(ShiftKind::Ror),
            0x8 => alu(AluOp::Tst, rd, Operand2::reg(rs)),
            0x9 => alu(AluOp::Rsb, rs, Operand2::imm(0)), // neg rd, rs
            0xA => alu(AluOp::Cmp, rd, Operand2::reg(rs)),
            0xB => alu(AluOp::Cmn, rd, Operand2::reg(rs)),
            0xC => alu(AluOp::Orr, rd, Operand2::reg(rs)),
            0xD => Op::Mul {
                acc: false,
                s: true,
                rd,
                rn: 0,
                rs,
                rm: rd,
            },
            0xE => alu(AluOp::Bic, rd, Operand2::reg(rs)),
            _ => alu(AluOp::Mvn, 0, Operand2::reg(rs)),
        };
        return (al, op);
    }
    if bits(h, 10, 12) == 0b001 {
        // Format 5: hi-register add/cmp/mov, and BX.
        let rd = ((h & 7) | (bits(h, 7, 7) << 3)) as Reg;
        let rs = bits(h, 3, 6) as Reg; // h2 is bit 6, folded in directly
        let op = match bits(h, 8, 9) {
            0b00 => Op::Alu {
                op: AluOp::Add,
                s: false,
                rd,
                rn: rd,
                op2: Operand2::reg(rs),
            },
            0b01 => Op::Alu {
                op: AluOp::Cmp,
                s: true,
                rd: 0,
                rn: rd,
                op2: Operand2::reg(rs),
            },
            0b10 => Op::Alu {
                op: AluOp::Mov,
                s: false,
                rd,
                rn: 0,
                op2: Operand2::reg(rs),
            },
            _ => Op::Bx { rm: rs },
        };
        return (al, op);
    }
    if bits(h, 11, 12) == 0b01 {
        // Format 6: ldr rd, [pc, #imm8*4]
        return (
            al,
            Op::Mem {
                load: true,
                width: MemWidth::Word,
                signed: false,
                rd: bits(h, 8, 10) as Reg,
                rn: PC,
                offset: MemOffset::Imm(((h & 0xFF) * 4) as u16),
                pre: true,
                up: true,
                writeback: false,
            },
        );
    }
    // Formats 7/8: register-offset transfers.
    let rd = (h & 7) as Reg;
    let rn = bits(h, 3, 5) as Reg;
    let offset = MemOffset::Reg {
        rm: bits(h, 6, 8) as Reg,
        shift: Shift::NONE,
    };
    let mem = |load, width, signed| Op::Mem {
        load,
        width,
        signed,
        rd,
        rn,
        offset,
        pre: true,
        up: true,
        writeback: false,
    };
    let op = if bit(h, 9) {
        // Format 8: strh/ldrh/ldsb/ldsh by (bit11=H, bit10=S)... encoding: bits 11,10 = H,S
        match (bit(h, 11), bit(h, 10)) {
            (false, false) => mem(false, MemWidth::Half, false), // strh
            (false, true) => mem(true, MemWidth::Byte, true),    // ldsb
            (true, false) => mem(true, MemWidth::Half, false),   // ldrh
            (true, true) => mem(true, MemWidth::Half, true),     // ldsh
        }
    } else {
        // Format 7: str/ldr{b} rd, [rb, ro]
        let width = if bit(h, 10) {
            MemWidth::Byte
        } else {
            MemWidth::Word
        };
        mem(bit(h, 11), width, false)
    };
    (al, op)
}

/// Top bits 1011: formats 13 (sp adjust) and 14 (push/pop).
fn decode_group_misc(h: u32) -> (Cond, Op) {
    let al = Cond::Al;
    if bits(h, 8, 11) == 0b0000 {
        // Format 13: add sp, #±imm7*4
        let op = if bit(h, 7) { AluOp::Sub } else { AluOp::Add };
        return (
            al,
            Op::Alu {
                op,
                s: false,
                rd: SP,
                rn: SP,
                op2: Operand2::imm(bits(h, 0, 6) * 4),
            },
        );
    }
    if bits(h, 9, 10) == 0b10 {
        // Format 14: push/pop {rlist} (+lr / +pc with the R bit).
        let load = bit(h, 11);
        let r = bit(h, 8);
        let mut rlist = (h & 0xFF) as u16;
        if r {
            rlist |= 1 << (if load { PC } else { LR });
        }
        return (
            al,
            Op::BlockMem {
                load,
                rn: SP,
                rlist,
                pre: !load, // push = stmdb (pre-decrement), pop = ldmia (post-increment)
                up: load,
                s_bit: false,
                writeback: true,
            },
        );
    }
    (al, Op::Undefined { raw: h })
}

/// Fuse a Thumb BL pair into (target, return_address_with_thumb_bit).
/// `high` must be the `ThumbBlHigh` op and `low` the following `ThumbBlLow`.
pub fn fuse_thumb_bl(high: &Instr, low: &Instr) -> Option<(u32, u32)> {
    match (high.op, low.op) {
        (Op::ThumbBlHigh { lr_partial }, Op::ThumbBlLow { off }) => {
            let target = lr_partial.wrapping_add((off as u32) << 1);
            let ret = (low.addr.wrapping_add(2)) | 1;
            Some((target, ret))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(h: u16) -> Op {
        decode_thumb(h, 0x0800_0000).op
    }

    #[test]
    fn bx_lr() {
        // 0x4770 = bx lr
        assert_eq!(op(0x4770), Op::Bx { rm: LR });
    }

    #[test]
    fn push_pop() {
        // 0xB500 = push {lr}
        assert_eq!(
            op(0xB500),
            Op::BlockMem {
                load: false,
                rn: SP,
                rlist: 1 << LR,
                pre: true,
                up: false,
                s_bit: false,
                writeback: true,
            }
        );
        // 0xBD10 = pop {r4, pc}
        assert_eq!(
            op(0xBD10),
            Op::BlockMem {
                load: true,
                rn: SP,
                rlist: (1 << PC) | (1 << 4),
                pre: false,
                up: true,
                s_bit: false,
                writeback: true,
            }
        );
    }

    #[test]
    fn mov_imm() {
        // 0x2000 = movs r0, #0
        assert_eq!(
            op(0x2000),
            Op::Alu {
                op: AluOp::Mov,
                s: true,
                rd: 0,
                rn: 0,
                op2: Operand2::imm(0)
            }
        );
        // 0x28FF = cmp r0, #0xFF
        assert_eq!(
            op(0x28FF),
            Op::Alu {
                op: AluOp::Cmp,
                s: true,
                rd: 0,
                rn: 0,
                op2: Operand2::imm(0xFF)
            }
        );
    }

    #[test]
    fn shift_imm() {
        // 0x0089 = lsls r1, r1, #2
        assert_eq!(
            op(0x0089),
            Op::Alu {
                op: AluOp::Mov,
                s: true,
                rd: 1,
                rn: 0,
                op2: Operand2::Reg {
                    rm: 1,
                    shift: Shift::Imm {
                        kind: ShiftKind::Lsl,
                        amount: 2
                    },
                },
            }
        );
    }

    #[test]
    fn add_sub_three_reg() {
        // 0x1889 = adds r1, r1, r2
        assert_eq!(
            op(0x1889),
            Op::Alu {
                op: AluOp::Add,
                s: true,
                rd: 1,
                rn: 1,
                op2: Operand2::reg(2)
            }
        );
        // 0x1E40 = subs r0, r0, #1
        assert_eq!(
            op(0x1E40),
            Op::Alu {
                op: AluOp::Sub,
                s: true,
                rd: 0,
                rn: 0,
                op2: Operand2::imm(1)
            }
        );
    }

    #[test]
    fn alu_format4() {
        // 0x4008 = ands r0, r1
        assert_eq!(
            op(0x4008),
            Op::Alu {
                op: AluOp::And,
                s: true,
                rd: 0,
                rn: 0,
                op2: Operand2::reg(1)
            }
        );
        // 0x4248 = negs r0, r1 -> rsbs r0, r1, #0
        assert_eq!(
            op(0x4248),
            Op::Alu {
                op: AluOp::Rsb,
                s: true,
                rd: 0,
                rn: 1,
                op2: Operand2::imm(0)
            }
        );
        // 0x4348 = muls r0, r1: rd = rd * rs
        assert_eq!(
            op(0x4348),
            Op::Mul {
                acc: false,
                s: true,
                rd: 0,
                rn: 0,
                rs: 1,
                rm: 0
            }
        );
        // 0x41C8 = rors r0, r1
        assert_eq!(
            op(0x41C8),
            Op::Alu {
                op: AluOp::Mov,
                s: true,
                rd: 0,
                rn: 0,
                op2: Operand2::Reg {
                    rm: 0,
                    shift: Shift::Reg {
                        kind: ShiftKind::Ror,
                        rs: 1
                    },
                },
            }
        );
    }

    #[test]
    fn hi_reg_ops() {
        // 0x4448 = add r0, r9
        assert_eq!(
            op(0x4448),
            Op::Alu {
                op: AluOp::Add,
                s: false,
                rd: 0,
                rn: 0,
                op2: Operand2::reg(9)
            }
        );
        // 0x4685 = mov sp, r0
        assert_eq!(
            op(0x4685),
            Op::Alu {
                op: AluOp::Mov,
                s: false,
                rd: SP,
                rn: 0,
                op2: Operand2::reg(0)
            }
        );
        // 0x45C8 = cmp r8, r9
        assert_eq!(
            op(0x45C8),
            Op::Alu {
                op: AluOp::Cmp,
                s: true,
                rd: 0,
                rn: 8,
                op2: Operand2::reg(9)
            }
        );
    }

    #[test]
    fn pc_relative_load() {
        // 0x4904 = ldr r1, [pc, #16]
        let i = decode_thumb(0x4904, 0x0800_0002);
        assert!(matches!(
            i.op,
            Op::Mem {
                load: true,
                width: MemWidth::Word,
                rd: 1,
                rn: PC,
                offset: MemOffset::Imm(16),
                ..
            }
        ));
        // Literal address: (addr + 4 word-aligned) + 16 = 0x08000004 + 16.
        assert_eq!(i.literal_addr(), Some(0x0800_0014));
    }

    #[test]
    fn loads_stores() {
        // 0x6800 = ldr r0, [r0, #0]
        assert!(matches!(
            op(0x6800),
            Op::Mem {
                load: true,
                width: MemWidth::Word,
                signed: false,
                rd: 0,
                rn: 0,
                offset: MemOffset::Imm(0),
                ..
            }
        ));
        // 0x6849 = ldr r1, [r1, #4]
        assert!(matches!(
            op(0x6849),
            Op::Mem {
                load: true,
                width: MemWidth::Word,
                rd: 1,
                rn: 1,
                offset: MemOffset::Imm(4),
                ..
            }
        ));
        // 0x8800 = ldrh r0, [r0, #0]
        assert!(matches!(
            op(0x8800),
            Op::Mem {
                load: true,
                width: MemWidth::Half,
                signed: false,
                ..
            }
        ));
        // 0x5650 = ldsb r0, [r2, r1]
        assert!(matches!(
            op(0x5650),
            Op::Mem {
                load: true,
                width: MemWidth::Byte,
                signed: true,
                rd: 0,
                rn: 2,
                offset: MemOffset::Reg { rm: 1, .. },
                ..
            }
        ));
        // 0x9001 = str r0, [sp, #4]
        assert!(matches!(
            op(0x9001),
            Op::Mem {
                load: false,
                width: MemWidth::Word,
                rd: 0,
                rn: SP,
                offset: MemOffset::Imm(4),
                ..
            }
        ));
    }

    #[test]
    fn sp_adjust() {
        // 0xB082 = sub sp, #8
        assert_eq!(
            op(0xB082),
            Op::Alu {
                op: AluOp::Sub,
                s: false,
                rd: SP,
                rn: SP,
                op2: Operand2::imm(8)
            }
        );
        // 0xB002 = add sp, #8
        assert_eq!(
            op(0xB002),
            Op::Alu {
                op: AluOp::Add,
                s: false,
                rd: SP,
                rn: SP,
                op2: Operand2::imm(8)
            }
        );
    }

    #[test]
    fn adr() {
        // 0xA000 = add r0, pc, #0
        assert_eq!(
            op(0xA000),
            Op::Alu {
                op: AluOp::Add,
                s: false,
                rd: 0,
                rn: PC,
                op2: Operand2::imm(0)
            }
        );
        // 0xA801 = add r0, sp, #4
        assert_eq!(
            op(0xA801),
            Op::Alu {
                op: AluOp::Add,
                s: false,
                rd: 0,
                rn: SP,
                op2: Operand2::imm(4)
            }
        );
    }

    #[test]
    fn branches() {
        // 0xE7FE = b . (infinite loop)
        assert_eq!(
            op(0xE7FE),
            Op::Branch {
                link: false,
                target: 0x0800_0000
            }
        );
        // 0xD0FE = beq -4 .. cond Eq, target = addr+4-4 = addr
        let i = decode_thumb(0xD0FE, 0x0800_0000);
        assert_eq!(i.cond, Cond::Eq);
        assert_eq!(
            i.op,
            Op::Branch {
                link: false,
                target: 0x0800_0000
            }
        );
        // 0xDE00: cond 0b1110 in format 16 is undefined.
        assert!(matches!(op(0xDE00), Op::Undefined { .. }));
    }

    #[test]
    fn swi() {
        // 0xDF06 = swi 6 (Div)
        assert_eq!(op(0xDF06), Op::Swi { imm: 6 });
    }

    #[test]
    fn bl_pair_fuses() {
        // bl with zero offset: 0xF000, 0xF800 at 0x08000000.
        let hi = decode_thumb(0xF000, 0x0800_0000);
        let lo = decode_thumb(0xF800, 0x0800_0002);
        assert_eq!(
            hi.op,
            Op::ThumbBlHigh {
                lr_partial: 0x0800_0004
            }
        );
        assert_eq!(lo.op, Op::ThumbBlLow { off: 0 });
        assert_eq!(fuse_thumb_bl(&hi, &lo), Some((0x0800_0004, 0x0800_0005)));

        // Backwards bl: target = pc+4 - 0x100.
        // high: off11 = 0x7FF (sign-ext -1 << 12 = -0x1000)
        // low: off = (0x1000 - 0x100) >> 1 = 0x780
        let hi = decode_thumb(0xF7FF, 0x0800_0200);
        let lo = decode_thumb(0xFF80, 0x0800_0202);
        assert_eq!(fuse_thumb_bl(&hi, &lo), Some((0x0800_0104, 0x0800_0205)));
    }

    #[test]
    fn blx_suffix_undefined_on_v4t() {
        // 0xE800-range second-half BLX is ARMv5T; must decode Undefined.
        assert!(matches!(op(0xE800), Op::Undefined { .. }));
    }

    #[test]
    fn stmia_ldmia() {
        // 0xC103 = stmia r1!, {r0, r1}
        assert_eq!(
            op(0xC103),
            Op::BlockMem {
                load: false,
                rn: 1,
                rlist: 0b11,
                pre: false,
                up: true,
                s_bit: false,
                writeback: true,
            }
        );
    }
}
