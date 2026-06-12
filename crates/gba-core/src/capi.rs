//! C ABI for recompiled code (docs/architecture.md: "runtime library
//! (Rust, C ABI)").
//!
//! Generated C calls back into the runtime through a function-pointer
//! table (`RtApi`) — no dynamic symbol resolution needed in either
//! direction. The machine is passed as an opaque pointer.
//!
//! v0 contract: `interp_one` executes exactly one guest instruction at
//! the current PC (with BIOS HLE and cycle accounting) and returns the
//! next fetch address with the Thumb bit folded into bit 0 — the same
//! key format the block lookup table uses. Interrupt delivery and the
//! IRQ-return stub stay in the host loop at block boundaries.

use crate::exec;
use crate::machine::Machine;

/// Function table handed to generated code.
///
/// Layout contract with generated C: `Machine` and `Cpu` are `repr(C)`
/// with `cpu` first and `regs[16]` / `cpsr` as the leading CPU fields, so
/// `(uint32_t*)m` points at r0..r15 and `((uint32_t*)m)[16]` is CPSR.
#[repr(C)]
pub struct RtApi {
    pub interp_one: extern "C" fn(*mut core::ffi::c_void) -> u32,
    pub read8: extern "C" fn(*mut core::ffi::c_void, u32) -> u32,
    pub read16: extern "C" fn(*mut core::ffi::c_void, u32) -> u32,
    pub read32: extern "C" fn(*mut core::ffi::c_void, u32) -> u32,
    pub write8: extern "C" fn(*mut core::ffi::c_void, u32, u32),
    pub write16: extern "C" fn(*mut core::ffi::c_void, u32, u32),
    pub write32: extern "C" fn(*mut core::ffi::c_void, u32, u32),
    pub tick: extern "C" fn(*mut core::ffi::c_void, u32),
    /// Content guard for RAM-translated blocks: returns 1 iff the FNV-1a
    /// hash of `len` guest bytes at `addr` equals `expect`. One call per
    /// block entry; hashes the IWRAM slice directly on the fast path.
    pub guard: extern "C" fn(*mut core::ffi::c_void, u32, u32, u64) -> u32,
    /// Chain gate: returns 1 when chained native code must return to
    /// the dispatch loop — a frame completed, the CPU halted, or an
    /// interrupt became deliverable. Checked on chained back-edges so
    /// native loops cannot starve IRQ delivery or frame pacing.
    pub chain_gate: extern "C" fn(*mut core::ffi::c_void) -> u32,
}

const _: () = {
    assert!(std::mem::offset_of!(Machine, cpu) == 0);
    assert!(std::mem::offset_of!(crate::cpu::Cpu, regs) == 0);
    assert!(std::mem::offset_of!(crate::cpu::Cpu, cpsr) == 64);
};

macro_rules! mach {
    ($m:ident) => {
        unsafe { &mut *($m as *mut Machine) }
    };
}

extern "C" fn rt_chain_gate(m: *mut core::ffi::c_void) -> u32 {
    let mach = mach!(m);
    (mach.bus.frame_ready
        || mach.bus.halted
        || (mach.bus.irq_pending() && mach.cpu.cpsr & crate::cpu::FLAG_I == 0)) as u32
}

extern "C" fn rt_read8(m: *mut core::ffi::c_void, a: u32) -> u32 {
    crate::Bus::read8(&mut mach!(m).bus, a) as u32
}
extern "C" fn rt_read16(m: *mut core::ffi::c_void, a: u32) -> u32 {
    crate::Bus::read16(&mut mach!(m).bus, a) as u32
}
extern "C" fn rt_read32(m: *mut core::ffi::c_void, a: u32) -> u32 {
    crate::Bus::read32(&mut mach!(m).bus, a)
}
extern "C" fn rt_write8(m: *mut core::ffi::c_void, a: u32, v: u32) {
    crate::Bus::write8(&mut mach!(m).bus, a, v as u8)
}
extern "C" fn rt_write16(m: *mut core::ffi::c_void, a: u32, v: u32) {
    crate::Bus::write16(&mut mach!(m).bus, a, v as u16)
}
extern "C" fn rt_write32(m: *mut core::ffi::c_void, a: u32, v: u32) {
    crate::Bus::write32(&mut mach!(m).bus, a, v)
}
extern "C" fn rt_tick(m: *mut core::ffi::c_void, cycles: u32) {
    mach!(m).bus.tick(cycles as u64)
}

extern "C" fn rt_guard(m: *mut core::ffi::c_void, addr: u32, len: u32, expect: u64) -> u32 {
    let bus = &mut mach!(m).bus;
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let off = (addr & 0x7FFF) as usize;
    let eoff = (addr & 0x3_FFFF) as usize;
    if addr >> 24 == 3 && off + len as usize <= bus.iwram.len() {
        for &b in &bus.iwram[off..off + len as usize] {
            h ^= b as u64;
            h = h.wrapping_mul(0x1_0000_01b3);
        }
    } else if addr >> 24 == 2 && eoff + len as usize <= bus.ewram.len() {
        for &b in &bus.ewram[eoff..eoff + len as usize] {
            h ^= b as u64;
            h = h.wrapping_mul(0x1_0000_01b3);
        }
    } else {
        for i in 0..len {
            h ^= crate::Bus::read8(bus, addr.wrapping_add(i)) as u64;
            h = h.wrapping_mul(0x1_0000_01b3);
        }
    }
    (h == expect) as u32
}

/// A translated block: `key` is the guest address with bit 0 = Thumb.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RcgBlock {
    pub key: u32,
    pub func: extern "C" fn(*const RtApi, *mut core::ffi::c_void) -> u32,
}

pub extern "C" fn rt_interp_one(m: *mut core::ffi::c_void) -> u32 {
    let m = unsafe { &mut *(m as *mut Machine) };
    let region = (m.cpu.regs[15] >> 24) as usize & 0xF;
    let instr = if m.bus.real_bios {
        exec::step_cached(&mut m.cpu, &mut m.bus, &mut m.decode_cache)
    } else {
        exec::step_hle_cached(&mut m.cpu, &mut m.bus, &mut m.decode_cache)
    };
    m.bus.tick(crate::machine::instr_cost(region, &instr));
    m.cpu.regs[15] | m.cpu.thumb() as u32
}

pub const RT_API: RtApi = RtApi {
    interp_one: rt_interp_one,
    read8: rt_read8,
    read16: rt_read16,
    read32: rt_read32,
    write8: rt_write8,
    write16: rt_write16,
    write32: rt_write32,
    tick: rt_tick,
    guard: rt_guard,
    chain_gate: rt_chain_gate,
};
