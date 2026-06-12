//! ARMv4T (ARM7TDMI) instruction model and decoder.
//!
//! Both ARM (32-bit) and Thumb (16-bit) instructions decode into the shared
//! semantic [`Instr`] model: Thumb is, with few exceptions, a subset of ARM,
//! so the analyzer, translator, and interpreter all consume one representation.
//! The exceptions (the two halves of a Thumb `BL` pair) get dedicated ops.
//!
//! Decoding takes the instruction's address so that direct branch targets are
//! resolved here (including the +8/+4 pipeline offset). Other PC-relative
//! reads (e.g. `ldr rX, [pc, #imm]`, `add rX, pc, #imm`) keep PC as a plain
//! register operand; consumers apply the pipeline offset via
//! [`Instr::pc_value`] / [`Instr::literal_addr`].
//!
//! Raw shift-encoding conventions are preserved, not normalized:
//! `Shift::Imm { Lsr|Asr, amount: 0 }` means a shift by 32, and
//! `Shift::Imm { Ror, amount: 0 }` means RRX — exactly as in the ARM ARM.

mod arm;
mod disasm;
mod thumb;

pub use arm::decode_arm;
pub use thumb::{decode_thumb, fuse_thumb_bl};

/// Register index, 0..=15.
pub type Reg = u8;

pub const SP: Reg = 13;
pub const LR: Reg = 14;
pub const PC: Reg = 15;

/// Condition codes, in encoding order. `Nv` (0b1111) is never valid on ARMv4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cond {
    Eq,
    Ne,
    Cs,
    Cc,
    Mi,
    Pl,
    Vs,
    Vc,
    Hi,
    Ls,
    Ge,
    Lt,
    Gt,
    Le,
    Al,
    Nv,
}

impl Cond {
    pub fn from_bits(bits: u8) -> Cond {
        use Cond::*;
        [
            Eq, Ne, Cs, Cc, Mi, Pl, Vs, Vc, Hi, Ls, Ge, Lt, Gt, Le, Al, Nv,
        ][bits as usize & 0xF]
    }
}

/// Data-processing opcodes, in ARM encoding order (bits 24-21).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AluOp {
    And,
    Eor,
    Sub,
    Rsb,
    Add,
    Adc,
    Sbc,
    Rsc,
    Tst,
    Teq,
    Cmp,
    Cmn,
    Orr,
    Mov,
    Bic,
    Mvn,
}

impl AluOp {
    pub fn from_bits(bits: u8) -> AluOp {
        use AluOp::*;
        [
            And, Eor, Sub, Rsb, Add, Adc, Sbc, Rsc, Tst, Teq, Cmp, Cmn, Orr, Mov, Bic, Mvn,
        ][bits as usize & 0xF]
    }

    /// Comparison/test ops write flags only; rd is unused.
    pub fn is_compare(self) -> bool {
        matches!(self, AluOp::Tst | AluOp::Teq | AluOp::Cmp | AluOp::Cmn)
    }

    /// MOV/MVN ignore rn.
    pub fn is_unary(self) -> bool {
        matches!(self, AluOp::Mov | AluOp::Mvn)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShiftKind {
    Lsl,
    Lsr,
    Asr,
    Ror,
}

impl ShiftKind {
    pub fn from_bits(bits: u8) -> ShiftKind {
        [
            ShiftKind::Lsl,
            ShiftKind::Lsr,
            ShiftKind::Asr,
            ShiftKind::Ror,
        ][bits as usize & 3]
    }
}

/// A shift applied to a register operand. Raw encoding conventions preserved
/// (see module docs for the amount-0 special cases).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shift {
    Imm { kind: ShiftKind, amount: u8 },
    Reg { kind: ShiftKind, rs: Reg },
}

impl Shift {
    pub const NONE: Shift = Shift::Imm {
        kind: ShiftKind::Lsl,
        amount: 0,
    };
}

/// Second operand of a data-processing instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operand2 {
    /// Immediate, already rotated. `ror` is the rotate amount (0, 2, .., 30);
    /// when `ror != 0` the shifter carry-out is bit 31 of `value`.
    Imm {
        value: u32,
        ror: u8,
    },
    Reg {
        rm: Reg,
        shift: Shift,
    },
}

impl Operand2 {
    pub fn imm(value: u32) -> Operand2 {
        Operand2::Imm { value, ror: 0 }
    }

    pub fn reg(rm: Reg) -> Operand2 {
        Operand2::Reg {
            rm,
            shift: Shift::NONE,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemWidth {
    Byte,
    Half,
    Word,
}

/// Offset of a single load/store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemOffset {
    Imm(u16),
    Reg { rm: Reg, shift: Shift },
}

/// The semantic operation, shared between ARM and Thumb encodings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Data processing. For compare/test ops `rd` is 0 and meaningless; for
    /// unary ops `rn` is 0 and meaningless.
    Alu {
        op: AluOp,
        s: bool,
        rd: Reg,
        rn: Reg,
        op2: Operand2,
    },
    /// MUL / MLA: rd = rm * rs (+ rn if `acc`).
    Mul {
        acc: bool,
        s: bool,
        rd: Reg,
        rn: Reg,
        rs: Reg,
        rm: Reg,
    },
    /// UMULL/UMLAL/SMULL/SMLAL: rd_hi:rd_lo = rm * rs (+ rd_hi:rd_lo if `acc`).
    MulLong {
        signed: bool,
        acc: bool,
        s: bool,
        rd_hi: Reg,
        rd_lo: Reg,
        rs: Reg,
        rm: Reg,
    },
    /// SWP/SWPB: rd = [rn]; [rn] = rm (atomic, with rotated read).
    Swap {
        byte: bool,
        rd: Reg,
        rm: Reg,
        rn: Reg,
    },
    /// BX rm: branch to rm & !1, switch to Thumb if rm bit 0 set.
    Bx { rm: Reg },
    /// B/BL with the target fully resolved (pipeline offset applied).
    Branch { link: bool, target: u32 },
    /// First half of a Thumb BL pair: LR := lr_partial (already includes
    /// pc+4 and the sign-extended high offset shifted left 12).
    ThumbBlHigh { lr_partial: u32 },
    /// Second half of a Thumb BL pair: target = LR + (off << 1);
    /// LR := (addr of this half + 2) | 1.
    ThumbBlLow { off: u16 },
    /// Single load/store. `signed` only for loads (LDRSB/LDRSH).
    Mem {
        load: bool,
        width: MemWidth,
        signed: bool,
        rd: Reg,
        rn: Reg,
        offset: MemOffset,
        pre: bool,
        up: bool,
        writeback: bool,
    },
    /// LDM/STM. `s_bit`: user-bank transfer or SPSR restore (with PC in list).
    BlockMem {
        load: bool,
        rn: Reg,
        rlist: u16,
        pre: bool,
        up: bool,
        s_bit: bool,
        writeback: bool,
    },
    /// MRS: rd := CPSR/SPSR.
    Mrs { spsr: bool, rd: Reg },
    /// MSR from register. `fields` = bits 19-16 of the encoding (c,x,s,f).
    MsrReg { spsr: bool, fields: u8, rm: Reg },
    /// MSR from immediate.
    MsrImm { spsr: bool, fields: u8, value: u32 },
    /// SWI: software interrupt. In ARM the comment is 24 bits; in Thumb 8.
    /// BIOS call number is `imm >> 16` in ARM, `imm` in Thumb.
    Swi { imm: u32 },
    /// Anything that does not decode on ARMv4T (incl. coprocessor space).
    Undefined { raw: u32 },
}

/// A decoded instruction: address, condition, operation, and source ISA.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Instr {
    pub addr: u32,
    pub cond: Cond,
    pub op: Op,
    pub thumb: bool,
}

impl Instr {
    /// Size in bytes of the encoding (2 for Thumb, 4 for ARM).
    pub fn size(&self) -> u32 {
        if self.thumb {
            2
        } else {
            4
        }
    }

    /// The value read when this instruction reads PC as an operand:
    /// instruction address + 8 (ARM) or + 4 (Thumb).
    ///
    /// ARM7TDMI special case NOT covered here: ARM-mode register-specified
    /// shifts and STR/STM of PC read PC+12; consumers needing that must
    /// handle it explicitly.
    pub fn pc_value(&self) -> u32 {
        self.addr.wrapping_add(if self.thumb { 4 } else { 8 })
    }

    /// If this is a PC-relative literal load (`ldr rd, [pc, #imm]`), the
    /// absolute address of the literal — marks 4 (or 2/1) bytes of data in
    /// the text section, and frequently a code pointer.
    pub fn literal_addr(&self) -> Option<u32> {
        match self.op {
            Op::Mem {
                load: true,
                rn: PC,
                offset: MemOffset::Imm(imm),
                pre: true,
                up,
                ..
            } => {
                // In Thumb, PC is additionally word-aligned for literal loads.
                let base = if self.thumb {
                    self.pc_value() & !3
                } else {
                    self.pc_value()
                };
                Some(if up {
                    base.wrapping_add(imm as u32)
                } else {
                    base.wrapping_sub(imm as u32)
                })
            }
            _ => None,
        }
    }
}

#[inline]
pub(crate) fn bit(w: u32, n: u32) -> bool {
    (w >> n) & 1 != 0
}

#[inline]
pub(crate) fn bits(w: u32, lo: u32, hi: u32) -> u32 {
    (w >> lo) & ((1 << (hi - lo + 1)) - 1)
}
