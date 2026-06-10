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
#[repr(C)]
pub struct RtApi {
    pub interp_one: extern "C" fn(*mut core::ffi::c_void) -> u32,
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
    let instr = exec::step_hle(&mut m.cpu, &mut m.bus);
    m.bus.tick(crate::machine::instr_cost(region, &instr));
    m.cpu.regs[15] | m.cpu.thumb() as u32
}

pub const RT_API: RtApi = RtApi { interp_one: rt_interp_one };
