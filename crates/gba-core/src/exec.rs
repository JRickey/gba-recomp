//! Instruction execution: the ARM7TDMI semantics of the `armv4t::Instr`
//! model. This is the project's executable specification — every behavior
//! here is either from the ARM ARM (DDI 0100), the ARM7TDMI TRM, or the hardware reference's
//! "ARM CPU Reference", and the recompiler must match it.

use armv4t::{
    decode_arm, decode_thumb, AluOp, Cond, Instr, MemOffset, MemWidth, Op, Operand2, Shift,
    ShiftKind, LR, PC,
};

use crate::bus::Bus;
use crate::cpu::{Cpu, Exception, FLAG_C, FLAG_N, FLAG_T, FLAG_V, FLAG_Z};

/// Fetch, decode, and execute one instruction. Returns the decoded
/// instruction (callers use it for tracing and loop detection).
pub fn step<B: Bus>(cpu: &mut Cpu, bus: &mut B) -> Instr {
    step_inner(cpu, bus, false, None)
}

/// Like [`step`], but recognized BIOS SWI calls are emulated natively
/// instead of vectoring into a (possibly absent) BIOS image.
pub fn step_hle<B: Bus>(cpu: &mut Cpu, bus: &mut B) -> Instr {
    step_inner(cpu, bus, true, None)
}

/// Like [`step_hle`], memoizing decodes through `cache`.
pub fn step_hle_cached<B: Bus>(cpu: &mut Cpu, bus: &mut B, cache: &mut DecodeCache) -> Instr {
    step_inner(cpu, bus, true, Some(cache))
}

/// Like [`step`] (no BIOS HLE — SWIs vector to the real BIOS at 0x08),
/// memoizing decodes through `cache`.
pub fn step_cached<B: Bus>(cpu: &mut Cpu, bus: &mut B, cache: &mut DecodeCache) -> Instr {
    step_inner(cpu, bus, false, Some(cache))
}

/// Content-keyed decode memoization. An entry hits only when both its
/// key (instruction address | thumb bit) and its raw encoding match the
/// words just fetched, and decoding is a pure function of exactly those
/// inputs — so a hit is always identical to a fresh decode, and
/// self-modifying code simply misses and re-decodes. The fetch itself
/// still goes through the bus on every step.
pub struct DecodeCache {
    entries: Box<[CacheEntry]>,
}

#[derive(Clone, Copy)]
struct CacheEntry {
    key: u32,
    raw: u32,
    instr: Instr,
}

const CACHE_SLOTS: usize = 1 << 15;

impl Default for DecodeCache {
    fn default() -> Self {
        // An unused-slot sentinel isn't needed: a hit requires the key to
        // match `addr | thumb`, and no fetch address reaches 0xFFFF_FFFF.
        let empty = CacheEntry {
            key: u32::MAX,
            raw: 0,
            instr: decode_arm(0, 0),
        };
        DecodeCache {
            entries: vec![empty; CACHE_SLOTS].into_boxed_slice(),
        }
    }
}

impl DecodeCache {
    #[inline]
    fn get_thumb(&mut self, addr: u32, raw: u16) -> Instr {
        let e = &mut self.entries[((addr >> 1) as usize) & (CACHE_SLOTS - 1)];
        let key = addr | 1;
        if e.key == key && e.raw == raw as u32 {
            return e.instr;
        }
        let instr = decode_thumb(raw, addr);
        *e = CacheEntry {
            key,
            raw: raw as u32,
            instr,
        };
        instr
    }

    #[inline]
    fn get_arm(&mut self, addr: u32, raw: u32) -> Instr {
        let e = &mut self.entries[((addr >> 2) as usize) & (CACHE_SLOTS - 1)];
        if e.key == addr && e.raw == raw {
            return e.instr;
        }
        let instr = decode_arm(raw, addr);
        *e = CacheEntry {
            key: addr,
            raw,
            instr,
        };
        instr
    }
}

fn step_inner<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    hle: bool,
    cache: Option<&mut DecodeCache>,
) -> Instr {
    let pc = cpu.regs[PC as usize];
    bus.note_fetch(pc | cpu.thumb() as u32);
    let instr = match cache {
        Some(c) => {
            if cpu.thumb() {
                c.get_thumb(pc & !1, bus.read16(pc & !1))
            } else {
                c.get_arm(pc & !3, bus.read32(pc & !3))
            }
        }
        None => {
            if cpu.thumb() {
                decode_thumb(bus.read16(pc & !1), pc & !1)
            } else {
                decode_arm(bus.read32(pc & !3), pc & !3)
            }
        }
    };

    if cond_passed(cpu, instr.cond) {
        let handled = if hle {
            if let Op::Swi { imm } = instr.op {
                let num = if instr.thumb { imm } else { imm >> 16 };
                let handled = crate::hle::bios_call(cpu, bus, num) || bus.note_unhandled_swi(num);
                if handled {
                    bus.note_swi_returned();
                }
                handled
            } else {
                false
            }
        } else {
            false
        };
        if !handled {
            exec(cpu, bus, &instr);
        }
    }

    match cpu.branch.take() {
        Some(target) => {
            let mask = if cpu.thumb() { !1 } else { !3 };
            cpu.regs[PC as usize] = target & mask;
        }
        None => cpu.regs[PC as usize] = instr.addr.wrapping_add(instr.size()),
    }
    instr
}

pub fn cond_passed(cpu: &Cpu, cond: Cond) -> bool {
    let n = cpu.flag(FLAG_N);
    let z = cpu.flag(FLAG_Z);
    let c = cpu.flag(FLAG_C);
    let v = cpu.flag(FLAG_V);
    match cond {
        Cond::Eq => z,
        Cond::Ne => !z,
        Cond::Cs => c,
        Cond::Cc => !c,
        Cond::Mi => n,
        Cond::Pl => !n,
        Cond::Vs => v,
        Cond::Vc => !v,
        Cond::Hi => c && !z,
        Cond::Ls => !c || z,
        Cond::Ge => n == v,
        Cond::Lt => n != v,
        Cond::Gt => !z && n == v,
        Cond::Le => z || n != v,
        Cond::Al => true,
        Cond::Nv => false,
    }
}

/// Read a register as an instruction operand: PC reads with the pipeline
/// offset. `pc_extra` adds the ARM7TDMI +12 cases (register-specified
/// shifts, STR/STM of PC).
fn read_reg(cpu: &Cpu, instr: &Instr, r: u8, pc_extra: u32) -> u32 {
    if r == PC {
        instr.pc_value().wrapping_add(pc_extra)
    } else {
        cpu.regs[r as usize]
    }
}

fn write_reg(cpu: &mut Cpu, r: u8, value: u32) {
    if r == PC {
        cpu.branch = Some(value);
    } else {
        cpu.regs[r as usize] = value;
    }
}

fn exec<B: Bus>(cpu: &mut Cpu, bus: &mut B, instr: &Instr) {
    match instr.op {
        Op::Alu { op, s, rd, rn, op2 } => exec_alu(cpu, instr, op, s, rd, rn, op2),
        Op::Mul {
            acc,
            s,
            rd,
            rn,
            rs,
            rm,
        } => {
            let mut result = cpu.regs[rm as usize].wrapping_mul(cpu.regs[rs as usize]);
            if acc {
                result = result.wrapping_add(cpu.regs[rn as usize]);
            }
            write_reg(cpu, rd, result);
            if s {
                // C is UNPREDICTABLE after multiplies on ARMv4; left unchanged.
                cpu.set_flag(FLAG_N, result & (1 << 31) != 0);
                cpu.set_flag(FLAG_Z, result == 0);
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
            let a = cpu.regs[rm as usize];
            let b = cpu.regs[rs as usize];
            let mut result = if signed {
                (a as i32 as i64).wrapping_mul(b as i32 as i64) as u64
            } else {
                (a as u64).wrapping_mul(b as u64)
            };
            if acc {
                let acc_val =
                    ((cpu.regs[rd_hi as usize] as u64) << 32) | cpu.regs[rd_lo as usize] as u64;
                result = result.wrapping_add(acc_val);
            }
            cpu.regs[rd_lo as usize] = result as u32;
            cpu.regs[rd_hi as usize] = (result >> 32) as u32;
            if s {
                cpu.set_flag(FLAG_N, result & (1 << 63) != 0);
                cpu.set_flag(FLAG_Z, result == 0);
            }
        }
        Op::Swap { byte, rd, rm, rn } => {
            let addr = cpu.regs[rn as usize];
            let value = if byte {
                let v = bus.read8(addr) as u32;
                bus.write8(addr, cpu.regs[rm as usize] as u8);
                v
            } else {
                let v = bus.read32(addr & !3).rotate_right((addr & 3) * 8);
                bus.write32(addr & !3, cpu.regs[rm as usize]);
                v
            };
            write_reg(cpu, rd, value);
        }
        Op::Bx { rm } => {
            let target = read_reg(cpu, instr, rm, 0);
            cpu.set_thumb(target & 1 != 0);
            cpu.branch = Some(target);
        }
        Op::Branch { link, target } => {
            if link {
                cpu.regs[LR as usize] = instr.addr.wrapping_add(4);
            }
            cpu.branch = Some(target);
        }
        Op::ThumbBlHigh { lr_partial } => {
            cpu.regs[LR as usize] = lr_partial;
        }
        Op::ThumbBlLow { off } => {
            let target = cpu.regs[LR as usize].wrapping_add((off as u32) << 1);
            cpu.regs[LR as usize] = instr.addr.wrapping_add(2) | 1;
            cpu.branch = Some(target);
        }
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
        } => exec_mem(
            cpu, bus, instr, load, width, signed, rd, rn, offset, pre, up, writeback,
        ),
        Op::BlockMem {
            load,
            rn,
            rlist,
            pre,
            up,
            s_bit,
            writeback,
        } => exec_block(cpu, bus, instr, load, rn, rlist, pre, up, s_bit, writeback),
        Op::Mrs { spsr, rd } => {
            let value = if spsr { cpu.spsr() } else { cpu.cpsr };
            write_reg(cpu, rd, value);
        }
        Op::MsrReg { spsr, fields, rm } => exec_msr(cpu, spsr, fields, read_reg(cpu, instr, rm, 0)),
        Op::MsrImm {
            spsr,
            fields,
            value,
        } => exec_msr(cpu, spsr, fields, value),
        Op::Swi { .. } => {
            cpu.enter_exception(Exception::Swi, instr.addr.wrapping_add(instr.size()));
        }
        Op::Undefined { .. } => {
            cpu.enter_exception(Exception::Undefined, instr.addr.wrapping_add(instr.size()));
        }
    }
}

/// Barrel-shifter result: (value, carry_out).
fn eval_op2(cpu: &Cpu, instr: &Instr, op2: Operand2) -> (u32, bool) {
    let c_in = cpu.flag(FLAG_C);
    match op2 {
        Operand2::Imm { value, ror } => {
            let carry = if ror != 0 {
                value & (1 << 31) != 0
            } else {
                c_in
            };
            (value, carry)
        }
        Operand2::Reg { rm, shift } => match shift {
            Shift::Imm { kind, amount } => {
                let rm_val = read_reg(cpu, instr, rm, 0);
                shift_imm(rm_val, kind, amount, c_in)
            }
            Shift::Reg { kind, rs } => {
                // Register-specified shift: PC operands read as +12 (ARM).
                let rm_val = read_reg(cpu, instr, rm, 4);
                let amount = cpu.regs[rs as usize] & 0xFF;
                shift_reg(rm_val, kind, amount, c_in)
            }
        },
    }
}

/// Immediate-amount shifts, including the amount-0 special encodings:
/// LSR #0 / ASR #0 mean a shift by 32; ROR #0 means RRX.
fn shift_imm(value: u32, kind: ShiftKind, amount: u8, c_in: bool) -> (u32, bool) {
    match kind {
        ShiftKind::Lsl => {
            if amount == 0 {
                (value, c_in)
            } else {
                (value << amount, value & (1 << (32 - amount)) != 0)
            }
        }
        ShiftKind::Lsr => {
            if amount == 0 {
                (0, value & (1 << 31) != 0)
            } else {
                (value >> amount, value & (1 << (amount - 1)) != 0)
            }
        }
        ShiftKind::Asr => {
            if amount == 0 {
                let sign = value & (1 << 31) != 0;
                (if sign { 0xFFFF_FFFF } else { 0 }, sign)
            } else {
                (
                    (value as i32 >> amount) as u32,
                    value & (1 << (amount - 1)) != 0,
                )
            }
        }
        ShiftKind::Ror => {
            if amount == 0 {
                // RRX: rotate right through carry by one.
                let out = ((c_in as u32) << 31) | (value >> 1);
                (out, value & 1 != 0)
            } else {
                (
                    value.rotate_right(amount as u32),
                    value & (1 << (amount - 1)) != 0,
                )
            }
        }
    }
}

/// Register-amount shifts (bottom byte of rs); amounts of 0, 32, and >32
/// have distinct documented results.
fn shift_reg(value: u32, kind: ShiftKind, amount: u32, c_in: bool) -> (u32, bool) {
    if amount == 0 {
        return (value, c_in);
    }
    match kind {
        ShiftKind::Lsl => match amount {
            1..=31 => (value << amount, value & (1 << (32 - amount)) != 0),
            32 => (0, value & 1 != 0),
            _ => (0, false),
        },
        ShiftKind::Lsr => match amount {
            1..=31 => (value >> amount, value & (1 << (amount - 1)) != 0),
            32 => (0, value & (1 << 31) != 0),
            _ => (0, false),
        },
        ShiftKind::Asr => match amount {
            1..=31 => (
                (value as i32 >> amount) as u32,
                value & (1 << (amount - 1)) != 0,
            ),
            _ => {
                let sign = value & (1 << 31) != 0;
                (if sign { 0xFFFF_FFFF } else { 0 }, sign)
            }
        },
        ShiftKind::Ror => {
            let eff = amount & 31;
            if eff == 0 {
                (value, value & (1 << 31) != 0)
            } else {
                (value.rotate_right(eff), value & (1 << (eff - 1)) != 0)
            }
        }
    }
}

fn exec_alu(cpu: &mut Cpu, instr: &Instr, op: AluOp, s: bool, rd: u8, rn: u8, op2: Operand2) {
    // With a register-specified shift, *all* PC operand reads see +12.
    let pc_extra = if matches!(
        op2,
        Operand2::Reg {
            shift: Shift::Reg { .. },
            ..
        }
    ) {
        4
    } else {
        0
    };
    let (b, shifter_c) = eval_op2(cpu, instr, op2);
    let a = read_reg(cpu, instr, rn, pc_extra);
    let c_in = cpu.flag(FLAG_C) as u32;

    enum Out {
        Logic(u32),
        Arith {
            result: u32,
            carry: bool,
            overflow: bool,
        },
    }
    use Out::*;

    let add = |x: u32, y: u32, carry_in: u32| {
        let wide = x as u64 + y as u64 + carry_in as u64;
        let result = wide as u32;
        Arith {
            result,
            carry: wide > u32::MAX as u64,
            overflow: (!(x ^ y) & (x ^ result)) & (1 << 31) != 0,
        }
    };
    // sub computes x - y - (1 - carry_in); carry out = NOT borrow.
    let sub = |x: u32, y: u32, carry_in: u32| add(x, !y, carry_in);

    let out = match op {
        AluOp::And | AluOp::Tst => Logic(a & b),
        AluOp::Eor | AluOp::Teq => Logic(a ^ b),
        AluOp::Orr => Logic(a | b),
        AluOp::Bic => Logic(a & !b),
        AluOp::Mov => Logic(b),
        AluOp::Mvn => Logic(!b),
        AluOp::Add => add(a, b, 0),
        AluOp::Adc => add(a, b, c_in),
        AluOp::Sub | AluOp::Cmp => sub(a, b, 1),
        AluOp::Sbc => sub(a, b, c_in),
        AluOp::Rsb => sub(b, a, 1),
        AluOp::Rsc => sub(b, a, c_in),
        AluOp::Cmn => add(a, b, 0),
    };

    let result = match out {
        Logic(r) => r,
        Arith { result, .. } => result,
    };

    if s {
        if rd == PC {
            // S-suffixed rd=PC: CPSR := SPSR. This includes compares —
            // the historical TSTP/TEQP/CMPP/CMNP encodings, which real
            // games and test ROMs exercise (no flags are set from the
            // result; the mode/flag restore IS the effect).
            let spsr = cpu.spsr();
            cpu.set_cpsr(spsr);
        } else {
            cpu.set_flag(FLAG_N, result & (1 << 31) != 0);
            cpu.set_flag(FLAG_Z, result == 0);
            match out {
                Logic(_) => cpu.set_flag(FLAG_C, shifter_c),
                Arith {
                    carry, overflow, ..
                } => {
                    cpu.set_flag(FLAG_C, carry);
                    cpu.set_flag(FLAG_V, overflow);
                }
            }
        }
    }

    if !op.is_compare() {
        write_reg(cpu, rd, result);
    }
}

#[allow(clippy::too_many_arguments)]
fn exec_mem<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    instr: &Instr,
    load: bool,
    width: MemWidth,
    signed: bool,
    rd: u8,
    rn: u8,
    offset: MemOffset,
    pre: bool,
    up: bool,
    writeback: bool,
) {
    let mut base = read_reg(cpu, instr, rn, 0);
    if instr.thumb && rn == PC {
        // Thumb PC-relative loads use (PC & !3) as the base (LDR literal).
        base &= !3;
    }
    let off = match offset {
        MemOffset::Imm(imm) => imm as u32,
        MemOffset::Reg { rm, shift } => match shift {
            Shift::Imm { kind, amount } => {
                shift_imm(cpu.regs[rm as usize], kind, amount, cpu.flag(FLAG_C)).0
            }
            // Register-shifted offsets don't exist in any v4T encoding.
            Shift::Reg { .. } => unreachable!("reg-shifted mem offset"),
        },
    };
    let offset_base = if up {
        base.wrapping_add(off)
    } else {
        base.wrapping_sub(off)
    };
    let addr = if pre { offset_base } else { base };
    // Post-indexed transfers always write back; pre-indexed only with W.
    let do_writeback = !pre || writeback;

    if load {
        let value = match (width, signed) {
            (MemWidth::Word, _) => bus.read32(addr & !3).rotate_right((addr & 3) * 8),
            (MemWidth::Half, false) => (bus.read16(addr & !1) as u32).rotate_right((addr & 1) * 8),
            (MemWidth::Half, true) => {
                // LDRSH at an odd address degrades to LDRSB.
                if addr & 1 != 0 {
                    bus.read8(addr) as i8 as i32 as u32
                } else {
                    bus.read16(addr) as i16 as i32 as u32
                }
            }
            (MemWidth::Byte, false) => bus.read8(addr) as u32,
            (MemWidth::Byte, true) => bus.read8(addr) as i8 as i32 as u32,
        };
        if do_writeback && rn != rd {
            // Loaded value wins over writeback when rd == rn.
            write_reg(cpu, rn, offset_base);
        }
        write_reg(cpu, rd, value);
    } else {
        // STR of PC stores PC+12 (pipeline + store stage on ARM7TDMI).
        let value = read_reg(cpu, instr, rd, 4);
        match width {
            MemWidth::Word => bus.write32(addr & !3, value),
            MemWidth::Half => bus.write16(addr & !1, value as u16),
            MemWidth::Byte => bus.write8(addr, value as u8),
        }
        if do_writeback {
            write_reg(cpu, rn, offset_base);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn exec_block<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    instr: &Instr,
    load: bool,
    rn: u8,
    rlist: u16,
    pre: bool,
    up: bool,
    s_bit: bool,
    writeback: bool,
) {
    let base = cpu.regs[rn as usize];

    // Empty register list: transfer PC only, but step the base by 0x40
    // (as if 16 registers were transferred).
    let (list, count): (u16, u32) = if rlist == 0 {
        (1 << PC, 16)
    } else {
        (rlist, rlist.count_ones())
    };
    let total = count * 4;

    // Lowest address transferred, and the final base value. Registers are
    // always transferred lowest-register-at-lowest-address.
    let (mut addr, wb_val) = match (up, pre) {
        (true, false) => (base, base.wrapping_add(total)), // IA
        (true, true) => (base.wrapping_add(4), base.wrapping_add(total)), // IB
        (false, false) => (
            base.wrapping_sub(total).wrapping_add(4),
            base.wrapping_sub(total),
        ), // DA
        (false, true) => (base.wrapping_sub(total), base.wrapping_sub(total)), // DB
    };

    // User-bank transfer: S bit set and (store, or load without PC).
    let user_bank = s_bit && !(load && rlist & (1 << PC) != 0);
    let rn_in_list = rlist & (1 << rn) != 0;
    let lowest = rlist.trailing_zeros() as u8;

    if load {
        // Base writeback happens before the loads; a loaded rn then wins.
        if writeback && !rn_in_list {
            cpu.regs[rn as usize] = wb_val;
        }
        for r in 0..16u8 {
            if list & (1 << r) == 0 {
                continue;
            }
            let value = bus.read32(addr & !3);
            if user_bank {
                cpu.set_user_reg(r, value);
            } else if r == PC {
                cpu.branch = Some(value);
                if s_bit {
                    // LDM with PC and S: CPSR := SPSR (exception return).
                    let spsr = cpu.spsr();
                    cpu.set_cpsr(spsr);
                }
            } else {
                cpu.regs[r as usize] = value;
            }
            addr = addr.wrapping_add(4);
        }
    } else {
        for r in 0..16u8 {
            if list & (1 << r) == 0 {
                continue;
            }
            let value = if r == PC {
                instr.pc_value().wrapping_add(4) // STM stores PC+12
            } else if r == rn {
                // Storing the base: old value if rn is the first register
                // in the list, written-back value otherwise.
                if r == lowest {
                    base
                } else {
                    wb_val
                }
            } else if user_bank {
                cpu.user_reg(r)
            } else {
                cpu.regs[r as usize]
            };
            bus.write32(addr & !3, value);
            addr = addr.wrapping_add(4);
        }
        if writeback {
            cpu.regs[rn as usize] = wb_val;
        }
    }
}

fn exec_msr(cpu: &mut Cpu, spsr: bool, fields: u8, value: u32) {
    let mut mask = 0u32;
    if fields & 1 != 0 {
        mask |= 0x0000_00FF; // c: control
    }
    if fields & 2 != 0 {
        mask |= 0x0000_FF00; // x
    }
    if fields & 4 != 0 {
        mask |= 0x00FF_0000; // s
    }
    if fields & 8 != 0 {
        mask |= 0xFF00_0000; // f: flags
    }
    if spsr {
        let new = (cpu.spsr() & !mask) | (value & mask);
        cpu.set_spsr(new);
    } else {
        if cpu.mode() == crate::cpu::Mode::User {
            mask &= 0xFF00_0000; // user mode cannot touch control bits
        }
        // The T bit cannot be changed by MSR on ARM7TDMI (unpredictable);
        // preserve it so a buggy game write doesn't desync the decoder.
        let mask = mask & !FLAG_T;
        let new = (cpu.cpsr & !mask) | (value & mask);
        cpu.set_cpsr(new);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu::FLAG_I;

    /// Flat 64 KB RAM bus for semantics tests; code is placed at 0.
    struct FlatBus {
        ram: Vec<u8>,
    }

    impl FlatBus {
        fn new() -> FlatBus {
            FlatBus {
                ram: vec![0; 0x10000],
            }
        }
    }

    impl Bus for FlatBus {
        fn read8(&mut self, addr: u32) -> u8 {
            self.ram[(addr as usize) & 0xFFFF]
        }
        fn write8(&mut self, addr: u32, value: u8) {
            self.ram[(addr as usize) & 0xFFFF] = value;
        }
    }

    fn run_arm(words: &[u32]) -> (Cpu, FlatBus) {
        let mut cpu = Cpu::new();
        cpu.regs[15] = 0;
        cpu.regs[13] = 0x8000;
        let mut bus = FlatBus::new();
        for (i, w) in words.iter().enumerate() {
            let b = w.to_le_bytes();
            bus.ram[i * 4..i * 4 + 4].copy_from_slice(&b);
        }
        for _ in 0..words.len() {
            step(&mut cpu, &mut bus);
        }
        (cpu, bus)
    }

    #[test]
    fn add_sets_carry_and_overflow() {
        // mov r0, #0xFF000000; adds r0, r0, r0
        let (cpu, _) = run_arm(&[0xE3A0_04FF, 0xE090_0000]);
        assert_eq!(cpu.regs[0], 0xFE00_0000);
        assert!(cpu.flag(FLAG_C));
        assert!(!cpu.flag(FLAG_Z));
        assert!(cpu.flag(FLAG_N));
        // 0x7FFFFFFF + 0x7FFFFFFF overflows.
        // mov r0, #0x80000000; sub r0, r0, #1 -> 0x7FFFFFFF; adds r1, r0, r0
        let (cpu, _) = run_arm(&[0xE3A0_0102, 0xE240_0001, 0xE090_1000]);
        assert!(cpu.flag(FLAG_V));
        assert!(!cpu.flag(FLAG_C));
    }

    #[test]
    fn sub_carry_is_not_borrow() {
        // mov r0, #5; subs r1, r0, #3 -> C=1 (no borrow)
        let (cpu, _) = run_arm(&[0xE3A0_0005, 0xE250_1003]);
        assert_eq!(cpu.regs[1], 2);
        assert!(cpu.flag(FLAG_C));
        // mov r0, #3; subs r1, r0, #5 -> C=0 (borrow), N=1
        let (cpu, _) = run_arm(&[0xE3A0_0003, 0xE250_1005]);
        assert_eq!(cpu.regs[1], 0xFFFF_FFFE);
        assert!(!cpu.flag(FLAG_C));
        assert!(cpu.flag(FLAG_N));
    }

    #[test]
    fn logical_carry_from_shifter() {
        // mov r0, #3; movs r1, r0, lsr #1 -> r1=1, C=1 (bit 0 shifted out)
        let (cpu, _) = run_arm(&[0xE3A0_0003, 0xE1B0_10A0]);
        assert_eq!(cpu.regs[1], 1);
        assert!(cpu.flag(FLAG_C));
    }

    #[test]
    fn rotated_misaligned_load() {
        // str a word, then ldr from +1: value rotates right by 8.
        // mov r0, #0x100; ldr r1, [r0, #1] with stored 0xAABBCCDD
        let mut cpu = Cpu::new();
        cpu.regs[15] = 0;
        let mut bus = FlatBus::new();
        bus.ram[0x100..0x104].copy_from_slice(&0xAABB_CCDDu32.to_le_bytes());
        // ldr r1, [r0, #1]
        for (i, w) in [0xE3A0_0C01u32, 0xE590_1001].iter().enumerate() {
            bus.ram[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        step(&mut cpu, &mut bus);
        step(&mut cpu, &mut bus);
        assert_eq!(cpu.regs[1], 0xDDAA_BBCC); // ror 8
    }

    #[test]
    fn ldm_stm_round_trip() {
        // mov r0, #1; mov r1, #2; stmdb sp!, {r0, r1}; mov r0, #0; mov r1, #0;
        // ldmia sp!, {r0, r1}
        let (cpu, _) = run_arm(&[
            0xE3A0_0001,
            0xE3A0_1002,
            0xE92D_0003,
            0xE3A0_0000,
            0xE3A0_1000,
            0xE8BD_0003,
        ]);
        assert_eq!(cpu.regs[0], 1);
        assert_eq!(cpu.regs[1], 2);
        assert_eq!(cpu.regs[13], 0x8000); // sp restored
    }

    #[test]
    fn stm_stores_old_base_when_first_in_list() {
        // mov r0, #0x100; stmia r0!, {r0, r1}
        let (cpu, mut bus) = run_arm(&[0xE3A0_0C01, 0xE8A0_0003]);
        assert_eq!(bus.read32(0x100), 0x100); // old base stored
        assert_eq!(cpu.regs[0], 0x108); // writeback applied
    }

    #[test]
    fn swi_enters_svc_with_banked_lr() {
        let (cpu, _) = run_arm(&[0xEF00_0000]); // swi 0
        assert_eq!(cpu.mode(), crate::cpu::Mode::Svc);
        assert_eq!(cpu.regs[14], 4); // return address
        assert_eq!(cpu.regs[15], 0x08); // at the vector
        assert!(cpu.flag(FLAG_I)); // IRQs masked
        assert_eq!(cpu.spsr() & 0x1F, crate::cpu::Mode::Sys as u32);
    }

    #[test]
    fn bx_switches_to_thumb_and_back() {
        // mov r0, #0x101; bx r0 -> thumb at 0x100
        let mut cpu = Cpu::new();
        cpu.regs[15] = 0;
        let mut bus = FlatBus::new();
        for (i, w) in [0xE3A0_0E10u32, 0xE380_0001, 0xE12F_FF10]
            .iter()
            .enumerate()
        {
            bus.ram[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        // At 0x100 (thumb): bx lr equivalent... use: mov r0, #7 (0x2007), then bx r2.
        bus.ram[0x100..0x102].copy_from_slice(&0x2007u16.to_le_bytes());
        for _ in 0..4 {
            step(&mut cpu, &mut bus);
        }
        assert!(cpu.thumb());
        assert_eq!(cpu.regs[0], 7);
        assert_eq!(cpu.regs[15], 0x102);
    }

    #[test]
    fn thumb_bl_pair() {
        // Thumb at 0: bl +0x40 -> F000 F820, then the target returns nothing;
        // check lr and pc after both halves.
        let mut cpu = Cpu::new();
        cpu.regs[15] = 0;
        cpu.set_thumb(true);
        let mut bus = FlatBus::new();
        bus.ram[0..2].copy_from_slice(&0xF000u16.to_le_bytes());
        bus.ram[2..4].copy_from_slice(&0xF820u16.to_le_bytes());
        step(&mut cpu, &mut bus);
        step(&mut cpu, &mut bus);
        assert_eq!(cpu.regs[15], 0x44);
        assert_eq!(cpu.regs[14], 0x5); // return address | thumb bit
    }

    #[test]
    fn conditional_skips() {
        // movs r0, #0 (Z=1); movne r1, #5 must not execute.
        let (cpu, _) = run_arm(&[0xE3B0_0000, 0x13A0_1005]);
        assert_eq!(cpu.regs[1], 0);
    }

    #[test]
    fn msr_mode_switch_banks_sp() {
        // Start in System (sp=0x8000 from harness). Switch to IRQ mode:
        // mov r0, #0xD2 (IRQ|I|F); msr cpsr_c, r0; mov sp, #0x40
        // then back to system: mov r0, #0xDF; msr cpsr_c, r0
        let (cpu, _) = run_arm(&[
            0xE3A0_00D2,
            0xE129_F000,
            0xE3A0_D040,
            0xE3A0_00DF,
            0xE129_F000,
        ]);
        assert_eq!(cpu.regs[13], 0x8000); // back to system sp
        assert_eq!(cpu.mode(), crate::cpu::Mode::Sys);
    }
}
