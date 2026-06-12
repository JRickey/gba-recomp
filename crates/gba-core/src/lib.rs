//! GBA machine model.
//!
//! The interpreter executes the `armv4t::Instr` model directly. It serves
//! three roles in the project (see docs/architecture.md):
//! 1. the executable specification of ARM7TDMI semantics,
//! 2. the differential-testing oracle for the recompiler,
//! 3. the runtime fallback tier for RAM-resident/self-modifying code.
//!
//! Correctness over speed: this is the reference implementation. The
//! recompiler's output is what has to be fast.

pub mod apu;
pub mod backup;
pub mod bus;
pub mod capi;
pub mod cpu;
pub mod engine;
pub mod exec;
pub mod gax;
pub mod hle;
pub mod hostclock;
pub mod machine;
pub mod mem;
pub mod mp2k;
pub mod ppu;
pub mod rdrv;
pub mod rtc;
pub mod shadow;

pub use bus::Bus;
pub use cpu::{Cpu, Mode};
pub use machine::{instr_cost as machine_instr_cost, is_self_loop, Machine, StepEvent};
pub use mem::MemMap;
