//! Human-readable disassembly of the [`Instr`] model, UAL-flavored.
//!
//! Output is for debugging, test assertions, and eyeball-diffing against
//! other disassemblers — not a contract. Thumb instructions print through
//! their ARM-equivalent semantics (e.g. `push {lr}` prints as
//! `stmdb sp!, {lr}`), which keeps the formatter honest about what the
//! decoder actually produced.

use crate::*;
use std::fmt::Write as _;

const REG_NAMES: [&str; 16] = [
    "r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9", "r10", "r11", "r12", "sp", "lr",
    "pc",
];

fn reg(r: Reg) -> &'static str {
    REG_NAMES[r as usize & 0xF]
}

fn cond_suffix(c: Cond) -> &'static str {
    match c {
        Cond::Eq => "eq",
        Cond::Ne => "ne",
        Cond::Cs => "cs",
        Cond::Cc => "cc",
        Cond::Mi => "mi",
        Cond::Pl => "pl",
        Cond::Vs => "vs",
        Cond::Vc => "vc",
        Cond::Hi => "hi",
        Cond::Ls => "ls",
        Cond::Ge => "ge",
        Cond::Lt => "lt",
        Cond::Gt => "gt",
        Cond::Le => "le",
        Cond::Al => "",
        Cond::Nv => "nv",
    }
}

fn shift_str(shift: Shift) -> String {
    match shift {
        Shift::Imm {
            kind: ShiftKind::Lsl,
            amount: 0,
        } => String::new(),
        Shift::Imm {
            kind: ShiftKind::Ror,
            amount: 0,
        } => ", rrx".into(),
        Shift::Imm { kind, amount } => {
            // lsr/asr #0 encode shifts by 32.
            let amount = if amount == 0 { 32 } else { amount as u32 };
            format!(", {} #{}", kind_str(kind), amount)
        }
        Shift::Reg { kind, rs } => format!(", {} {}", kind_str(kind), reg(rs)),
    }
}

fn kind_str(kind: ShiftKind) -> &'static str {
    match kind {
        ShiftKind::Lsl => "lsl",
        ShiftKind::Lsr => "lsr",
        ShiftKind::Asr => "asr",
        ShiftKind::Ror => "ror",
    }
}

fn op2_str(op2: Operand2) -> String {
    match op2 {
        Operand2::Imm { value, .. } => format!("#0x{value:x}"),
        Operand2::Reg { rm, shift } => format!("{}{}", reg(rm), shift_str(shift)),
    }
}

fn rlist_str(rlist: u16) -> String {
    let mut out = String::from("{");
    let mut first = true;
    for r in 0..16u8 {
        if rlist & (1 << r) != 0 {
            if !first {
                out.push_str(", ");
            }
            out.push_str(reg(r));
            first = false;
        }
    }
    out.push('}');
    out
}

fn psr_str(spsr: bool, fields: u8) -> String {
    let mut s = String::from(if spsr { "spsr_" } else { "cpsr_" });
    for (bit, ch) in [(0, 'c'), (1, 'x'), (2, 's'), (3, 'f')] {
        if fields & (1 << bit) != 0 {
            s.push(ch);
        }
    }
    s
}

impl Instr {
    /// Disassemble to a UAL-flavored string.
    pub fn disasm(&self) -> String {
        let c = cond_suffix(self.cond);
        match self.op {
            Op::Alu { op, s, rd, rn, op2 } => {
                let mnem = format!("{:?}", op).to_lowercase();
                let s_suffix = if s && !op.is_compare() { "s" } else { "" };
                if op.is_compare() {
                    format!("{mnem}{c} {}, {}", reg(rn), op2_str(op2))
                } else if op.is_unary() {
                    format!("{mnem}{c}{s_suffix} {}, {}", reg(rd), op2_str(op2))
                } else {
                    format!(
                        "{mnem}{c}{s_suffix} {}, {}, {}",
                        reg(rd),
                        reg(rn),
                        op2_str(op2)
                    )
                }
            }
            Op::Mul {
                acc,
                s,
                rd,
                rn,
                rs,
                rm,
            } => {
                let s_suffix = if s { "s" } else { "" };
                if acc {
                    format!(
                        "mla{c}{s_suffix} {}, {}, {}, {}",
                        reg(rd),
                        reg(rm),
                        reg(rs),
                        reg(rn)
                    )
                } else {
                    format!("mul{c}{s_suffix} {}, {}, {}", reg(rd), reg(rm), reg(rs))
                }
            }
            Op::MulLong {
                signed,
                acc,
                s,
                rd_hi,
                rd_lo,
                rs,
                rm,
            } => {
                let mnem = match (signed, acc) {
                    (false, false) => "umull",
                    (false, true) => "umlal",
                    (true, false) => "smull",
                    (true, true) => "smlal",
                };
                let s_suffix = if s { "s" } else { "" };
                format!(
                    "{mnem}{c}{s_suffix} {}, {}, {}, {}",
                    reg(rd_lo),
                    reg(rd_hi),
                    reg(rm),
                    reg(rs)
                )
            }
            Op::Swap { byte, rd, rm, rn } => {
                let b = if byte { "b" } else { "" };
                format!("swp{c}{b} {}, {}, [{}]", reg(rd), reg(rm), reg(rn))
            }
            Op::Bx { rm } => format!("bx{c} {}", reg(rm)),
            Op::Branch { link, target } => {
                let l = if link { "l" } else { "" };
                format!("b{l}{c} 0x{target:08x}")
            }
            Op::ThumbBlHigh { lr_partial } => format!("bl.hi lr=0x{lr_partial:08x}"),
            Op::ThumbBlLow { off } => format!("bl.lo off=0x{off:x}"),
            Op::Mem {
                load,
                width,
                signed,
                rd,
                rn,
                offset,
                pre,
                up,
                writeback,
            } => {
                let mnem = match (load, width, signed) {
                    (true, MemWidth::Word, _) => "ldr",
                    (true, MemWidth::Half, false) => "ldrh",
                    (true, MemWidth::Half, true) => "ldrsh",
                    (true, MemWidth::Byte, false) => "ldrb",
                    (true, MemWidth::Byte, true) => "ldrsb",
                    (false, MemWidth::Word, _) => "str",
                    (false, MemWidth::Half, _) => "strh",
                    (false, MemWidth::Byte, _) => "strb",
                };
                let sign = if up { "" } else { "-" };
                let off = match offset {
                    MemOffset::Imm(0) => String::new(),
                    MemOffset::Imm(imm) => format!(", #{sign}0x{imm:x}"),
                    MemOffset::Reg { rm, shift } => {
                        format!(", {sign}{}{}", reg(rm), shift_str(shift))
                    }
                };
                let mut out = format!("{mnem}{c} {}, ", reg(rd));
                if pre {
                    let wb = if writeback { "!" } else { "" };
                    let _ = write!(out, "[{}{off}]{wb}", reg(rn));
                } else {
                    let _ = write!(out, "[{}]{off}", reg(rn));
                }
                // Annotate resolved literal pool addresses.
                if let Some(lit) = self.literal_addr() {
                    let _ = write!(out, " ; =[0x{lit:08x}]");
                }
                out
            }
            Op::BlockMem {
                load,
                rn,
                rlist,
                pre,
                up,
                s_bit,
                writeback,
            } => {
                let mnem = if load { "ldm" } else { "stm" };
                let mode = match (pre, up) {
                    (false, true) => "ia",
                    (true, true) => "ib",
                    (false, false) => "da",
                    (true, false) => "db",
                };
                let wb = if writeback { "!" } else { "" };
                let user = if s_bit { "^" } else { "" };
                format!(
                    "{mnem}{mode}{c} {}{wb}, {}{user}",
                    reg(rn),
                    rlist_str(rlist)
                )
            }
            Op::Mrs { spsr, rd } => {
                format!("mrs{c} {}, {}", reg(rd), if spsr { "spsr" } else { "cpsr" })
            }
            Op::MsrReg { spsr, fields, rm } => {
                format!("msr{c} {}, {}", psr_str(spsr, fields), reg(rm))
            }
            Op::MsrImm {
                spsr,
                fields,
                value,
            } => {
                format!("msr{c} {}, #0x{value:x}", psr_str(spsr, fields))
            }
            Op::Swi { imm } => format!("swi{c} 0x{imm:x}"),
            Op::Undefined { raw } => format!("undefined ; raw=0x{raw:08x}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{decode_arm, decode_thumb};

    #[test]
    fn arm_formatting() {
        let cases: &[(u32, &str)] = &[
            (0xE3A0_0000, "mov r0, #0x0"),
            (0xE12F_FF1E, "bx lr"),
            (0xE92D_4010, "stmdb sp!, {r4, lr}"),
            (0xE8BD_8010, "ldmia sp!, {r4, pc}"),
            (0xEA00_002E, "b 0x080000c0"),
            (0xE59F_1018, "ldr r1, [pc, #0x18] ; =[0x08000020]"),
            (0xE001_0392, "mul r1, r2, r3"),
            (0xE10F_0000, "mrs r0, cpsr"),
            (0xE129_F000, "msr cpsr_cf, r0"),
            (0x1359_0000, "cmpne r9, #0x0"),
            (0xE1A0_1102, "mov r1, r2, lsl #2"),
        ];
        for &(word, expected) in cases {
            assert_eq!(
                decode_arm(word, 0x0800_0000).disasm(),
                expected,
                "word {word:08X}"
            );
        }
    }

    #[test]
    fn thumb_formatting() {
        let cases: &[(u16, &str)] = &[
            (0x4770, "bx lr"),
            (0xB500, "stmdb sp!, {lr}"),
            (0xBD10, "ldmia sp!, {r4, pc}"),
            (0x2000, "movs r0, #0x0"),
            (0x1889, "adds r1, r1, r2"),
            (0xD0FE, "beq 0x08000000"),
            (0xDF06, "swi 0x6"),
            (0x4904, "ldr r1, [pc, #0x10] ; =[0x08000014]"),
        ];
        for &(half, expected) in cases {
            assert_eq!(
                decode_thumb(half, 0x0800_0000).disasm(),
                expected,
                "half {half:04X}"
            );
        }
    }
}
