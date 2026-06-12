//! The whole machine: CPU + bus, interrupt delivery, and the HLE of the
//! BIOS interrupt dispatcher.
//!
//! On hardware, an IRQ vectors to BIOS code which saves r0-r3/r12/lr on the
//! IRQ stack, points lr at the BIOS return stub, and jumps through the user
//! handler pointer at 0x03007FFC (ARM state). The handler returns to the
//! stub, which restores and executes `subs pc, lr, #4`. We reproduce that
//! contract exactly — including the stack traffic, which games can observe —
//! with `IRQ_RETURN_ADDR` standing in for the stub: fetching from it in IRQ
//! mode triggers the HLE epilogue.

use armv4t::{Cond, Instr, Op};

use crate::bus::Bus;
use crate::cpu::{Cpu, Mode, FLAG_I, FLAG_T};
use crate::exec;
use crate::mem::MemMap;

/// Address of the BIOS instruction the IRQ handler returns to
/// (the real BIOS dispatcher's post-call address).
pub const IRQ_RETURN_ADDR: u32 = 0x0000_0138;

/// Address holding the user IRQ handler pointer.
const HANDLER_PTR: u32 = 0x0300_7FFC;

#[repr(C)]
pub struct Machine {
    pub cpu: Cpu,
    pub bus: MemMap,
    /// Decode memoization (content-keyed; see `exec::DecodeCache`).
    pub decode_cache: exec::DecodeCache,
}

/// What a single `step` did — callers use it for tracing and loop detection.
pub enum StepEvent {
    /// Executed one instruction.
    Instr(Instr),
    /// Slept (halted / waiting), dispatched an IRQ, or returned from one.
    Idle,
}

impl Machine {
    pub fn new(rom: Vec<u8>) -> Machine {
        Machine {
            cpu: Cpu::new(),
            bus: MemMap::new(rom),
            decode_cache: exec::DecodeCache::default(),
        }
    }

    /// A machine that executes a real BIOS image instead of the HLE:
    /// boots from the reset vector in SVC mode, SWIs vector to 0x08,
    /// IRQs vector to 0x18 — no BIOS HLE anywhere.
    pub fn new_with_bios(rom: Vec<u8>, bios: &[u8]) -> Machine {
        let mut m = Machine::new(rom);
        m.bus.load_bios(bios);
        m.cpu.reset_hard_boot();
        m
    }

    /// Execute one instruction (or service interrupt machinery).
    pub fn step(&mut self) -> StepEvent {
        // Wake from Halt when any enabled interrupt is latched (IME ignored).
        if self.bus.halted {
            if self.bus.irq_line() {
                self.bus.halted = false;
            } else {
                self.bus.skip_to_next_event();
                return StepEvent::Idle;
            }
        }

        // HLE BIOS IRQ return stub (real-BIOS mode executes the actual
        // dispatcher at this address instead).
        if !self.bus.real_bios
            && self.cpu.regs[15] == IRQ_RETURN_ADDR
            && self.cpu.mode() == Mode::Irq
        {
            self.irq_epilogue();
            return StepEvent::Idle;
        }

        if self.bus.irq_pending() && !self.cpu.flag(FLAG_I) {
            if self.bus.real_bios {
                self.enter_irq_real();
            } else {
                self.dispatch_irq();
            }
            return StepEvent::Idle;
        }

        // MP2K HLE: PC landing on SoundMainRAM means the track engine
        // has finalized this tick's channel state — snapshot it for the
        // shadow mixer (read-only; the guest mixer still runs).
        if let Some(h) = self.bus.mp2k.as_deref() {
            let key = self.cpu.regs[15] | self.cpu.thumb() as u32;
            if h.active && h.hook_match(key) {
                self.bus.mp2k_frame_hook(key);
            }
        }
        if let Some(g) = self.bus.gax.as_deref() {
            let key = self.cpu.regs[15] | self.cpu.thumb() as u32;
            if g.hook_match(key) {
                let r0 = self.cpu.regs[0];
                self.bus.gax_frame_hook(key, r0);
            }
        }
        if let Some(rr) = self.bus.rdrv.as_deref() {
            let key = self.cpu.regs[15] | self.cpu.thumb() as u32;
            if rr.hook_match(key) {
                self.bus.rdrv_frame_hook(key);
            }
        }

        let region = (self.cpu.regs[15] >> 24) as usize & 0xF;
        let instr = if self.bus.real_bios {
            exec::step_cached(&mut self.cpu, &mut self.bus, &mut self.decode_cache)
        } else {
            exec::step_hle_cached(&mut self.cpu, &mut self.bus, &mut self.decode_cache)
        };
        self.bus.tick(instr_cost(region, &instr));
        StepEvent::Instr(instr)
    }

    /// Real-BIOS IRQ delivery: take the exception exactly as hardware
    /// does and let the BIOS dispatcher at 0x18 run as guest code.
    fn enter_irq_real(&mut self) {
        {
            static TRACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            if *TRACE.get_or_init(|| std::env::var_os("RECOMP_TRACE_IOW").is_some()) {
                eprintln!(
                    "IRQDISPATCH f={} line={} if={:04x} (real bios)",
                    self.bus.frames,
                    self.bus.scanline(),
                    self.bus.reg_if
                );
            }
        }
        // lr_irq such that `subs pc, lr, #4` resumes at the interrupted
        // instruction (regs[15] is the next fetch address).
        let return_lr = self.cpu.regs[15].wrapping_add(4);
        self.cpu
            .enter_exception(crate::cpu::Exception::Irq, return_lr);
        let target = self.cpu.branch.take().unwrap_or(0x18);
        self.cpu.regs[15] = target & !3;
        self.bus.tick(3); // exception entry (2S + 1N)
    }

    /// Run until the next completed frame (or a step budget runs out).
    pub fn run_frame(&mut self, max_steps: u64) {
        self.bus.frame_ready = false;
        let mut steps = 0;
        while !self.bus.frame_ready && steps < max_steps {
            self.step();
            steps += 1;
        }
    }

    /// HLE of the BIOS IRQ dispatcher prologue.
    fn dispatch_irq(&mut self) {
        // Diagnostic (RECOMP_TRACE_IOW): IRQ dispatch timing vs raise.
        {
            static TRACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            if *TRACE.get_or_init(|| std::env::var_os("RECOMP_TRACE_IOW").is_some()) {
                eprintln!(
                    "IRQDISPATCH f={} line={} if={:04x}",
                    self.bus.frames,
                    self.bus.scanline(),
                    self.bus.reg_if
                );
            }
        }
        let cpu = &mut self.cpu;
        // lr_irq such that `subs pc, lr, #4` resumes at the interrupted
        // instruction (regs[15] is the next fetch address).
        let return_lr = cpu.regs[15].wrapping_add(4);

        let old_cpsr = cpu.cpsr;
        let new_cpsr = (old_cpsr & !0x1F & !FLAG_T) | Mode::Irq as u32 | FLAG_I;
        cpu.set_cpsr(new_cpsr);
        cpu.set_spsr(old_cpsr);

        // BIOS prologue: stmfd sp!, {r0-r3, r12, lr}
        let sp = cpu.regs[13].wrapping_sub(24);
        cpu.regs[13] = sp;
        for (i, value) in [
            cpu.regs[0],
            cpu.regs[1],
            cpu.regs[2],
            cpu.regs[3],
            cpu.regs[12],
            return_lr,
        ]
        .into_iter()
        .enumerate()
        {
            self.bus.write32(sp.wrapping_add(4 * i as u32), value);
        }

        // mov r0, #0x04000000 ; the handler observably receives this.
        self.cpu.regs[0] = 0x0400_0000;
        self.cpu.regs[14] = IRQ_RETURN_ADDR;
        self.bus.bios_open = 0xE25E_F004; // BIOS protection: during IRQ
        let handler = self.bus.read32(HANDLER_PTR);
        self.cpu.regs[15] = handler & !3;
        self.bus.tick(24); // rough dispatch cost
    }

    /// HLE of the BIOS IRQ dispatcher epilogue:
    /// ldmfd sp!, {r0-r3, r12, lr}; subs pc, lr, #4.
    fn irq_epilogue(&mut self) {
        let sp = self.cpu.regs[13];
        let mut vals = [0u32; 6];
        for (i, v) in vals.iter_mut().enumerate() {
            *v = self.bus.read32(sp.wrapping_add(4 * i as u32));
        }
        let cpu = &mut self.cpu;
        cpu.regs[13] = sp.wrapping_add(24);
        cpu.regs[0] = vals[0];
        cpu.regs[1] = vals[1];
        cpu.regs[2] = vals[2];
        cpu.regs[3] = vals[3];
        cpu.regs[12] = vals[4];
        let target = vals[5].wrapping_sub(4);

        let spsr = cpu.spsr();
        cpu.set_cpsr(spsr);
        let mask = if cpu.thumb() { !1u32 } else { !3u32 };
        cpu.regs[15] = target & mask;
        self.bus.bios_open = 0xE55E_C002; // BIOS protection: after IRQ
        self.bus.tick(16);
    }
}

/// Rough per-instruction cycle cost by the region PC executes from.
/// Refined per-access costs come with the recompiler's static cycle sums;
/// this keeps interpreter-mode video/timer/audio pacing in the right
/// ballpark (commercial waitstate config assumed).
pub fn instr_cost(region: usize, instr: &Instr) -> u64 {
    let fetch: u64 = match region {
        0x0 | 0x3 | 0x7 => 1, // BIOS, IWRAM, OAM: 32-bit
        0x2 => {
            if instr.thumb {
                3
            } else {
                6
            }
        } // EWRAM: 16-bit, 2 waits
        0x5 | 0x6 => {
            if instr.thumb {
                1
            } else {
                2
            }
        } // palette/VRAM
        0x8..=0xD => {
            if instr.thumb {
                2
            } else {
                4
            }
        } // ROM 3/1 + prefetch
        _ => 1,
    };
    let op_extra: u64 = match instr.op {
        Op::Mem { .. } | Op::Swap { .. } => 2,
        Op::BlockMem { rlist, .. } => 1 + rlist.count_ones() as u64,
        Op::Mul { .. } | Op::MulLong { .. } => 2,
        Op::Branch { .. } | Op::Bx { .. } => 2,
        _ => 0,
    };
    fetch + op_extra
}

/// True if this instruction is an unconditional branch-to-self (the idle
/// idiom every test ROM parks in).
pub fn is_self_loop(instr: &Instr) -> bool {
    instr.cond == Cond::Al
        && matches!(instr.op, Op::Branch { link: false, target } if target == instr.addr)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assemble a tiny IRQ round-trip:
    /// main:  enable VBlank IRQ (DISPSTAT bit 3, IE, IME), then loop reading
    ///        a counter the handler increments.
    /// handler (ARM, in IWRAM): increment [0x03000100], acknowledge IF,
    ///        set BIOS IntrWait flags, bx lr.
    #[test]
    fn vblank_irq_round_trip() {
        // Handler at 0x03000000 (ARM):
        //   ldr r1, =0x03000100 ; ldr r2,[r1] ; add r2,#1 ; str r2,[r1]
        //   ldr r1, =0x04000202 ; mov r2,#1 ; strh r2,[r1]   (ack IF)
        //   bx lr
        let handler: &[u32] = &[
            0xE59F_1018, // ldr r1, [pc, #0x18] -> literal 0x03000100
            0xE591_2000, // ldr r2, [r1]
            0xE282_2001, // add r2, r2, #1
            0xE581_2000, // str r2, [r1]
            0xE59F_100C, // ldr r1, [pc, #0xC] -> literal 0x04000202
            0xE3A0_2001, // mov r2, #1
            0xE1C1_20B0, // strh r2, [r1]
            0xE12F_FF1E, // bx lr
            0x0300_0100, // literal pool
            0x0400_0202,
        ];

        // Main program at 0x08000000 (ARM):
        //   set handler ptr, DISPSTAT vblank-irq-enable, IE=1, IME=1
        //   then: b . (we step manually and watch the counter)
        let main: &[u32] = &[
            0xE3A0_0303, // mov r0, #0x0C000000 -> nope; build addr differently
        ];
        let _ = main;

        let mut m = Machine::new(vec![0; 64]);
        // Install handler code in IWRAM.
        for (i, w) in handler.iter().enumerate() {
            m.bus.write32(0x0300_0000 + 4 * i as u32, *w);
        }
        // Handler pointer.
        m.bus.write32(0x0300_7FFC, 0x0300_0000);
        // DISPSTAT: VBlank IRQ enable; IE: VBlank; IME on.
        m.bus.write8(0x0400_0004, 0x08);
        m.bus.write16(0x0400_0200, 1);
        m.bus.write16(0x0400_0208, 1);
        // Park the CPU on a self-loop in ROM.
        m.bus.rom[0..4].copy_from_slice(&0xEAFF_FFFEu32.to_le_bytes());

        // Run for two frames' worth of steps.
        for _ in 0..200_000 {
            m.step();
            if m.bus.read32(0x0300_0100) >= 2 {
                break;
            }
        }
        assert!(m.bus.read32(0x0300_0100) >= 2, "handler ran on vblank");
        // Let the in-flight handler return, then check we're back at the
        // self-loop in system mode.
        for _ in 0..32 {
            m.step();
        }
        assert_eq!(m.cpu.mode(), Mode::Sys);
        assert_eq!(m.cpu.regs[15], 0x0800_0000);
    }

    /// Real-BIOS mode: boot from the reset vector, hand off to the cart,
    /// vector a SWI into actual BIOS code (which must read its own
    /// bytes), return via `movs pc, lr`, and leave the read-protection
    /// value at the last BIOS opcode fetched.
    #[test]
    fn real_bios_boot_and_swi_round_trip() {
        let mut bios = vec![0u8; 0x4000];
        let put = |b: &mut [u8], addr: usize, w: u32| {
            b[addr..addr + 4].copy_from_slice(&w.to_le_bytes());
        };
        put(&mut bios, 0x00, 0xEA00_000E); // reset: b 0x40
        put(&mut bios, 0x08, 0xEA00_003C); // swi vector: b 0x100
                                           // boot code: ldr r0, =0x08000000 ; bx r0
        put(&mut bios, 0x40, 0xE59F_0000);
        put(&mut bios, 0x44, 0xE12F_FF10);
        put(&mut bios, 0x48, 0x0800_0000);
        // swi handler: ldr r1, =0x03000000 ; mov r2, #0x77 ;
        //              str r2, [r1] ; movs pc, lr
        put(&mut bios, 0x100, 0xE59F_1008);
        put(&mut bios, 0x104, 0xE3A0_2077);
        put(&mut bios, 0x108, 0xE581_2000);
        put(&mut bios, 0x10C, 0xE1B0_F00E);
        put(&mut bios, 0x110, 0x0300_0000);
        // What the pipeline has prefetched (+8) while the `movs pc, lr`
        // return executes — the post-SWI protection value.
        put(&mut bios, 0x114, 0xCAFE_F00D);

        let mut rom = vec![0u8; 16];
        rom[0..4].copy_from_slice(&0xEF00_0000u32.to_le_bytes()); // swi 0
        rom[4..8].copy_from_slice(&0xEAFF_FFFEu32.to_le_bytes()); // b .

        let mut m = Machine::new_with_bios(rom, &bios);
        assert_eq!(m.cpu.regs[15], 0);
        assert_eq!(m.cpu.mode(), Mode::Svc);

        for _ in 0..32 {
            m.step();
        }
        // The SWI handler ran from real BIOS bytes...
        assert_eq!(m.bus.read32(0x0300_0000), 0x77);
        // ...control returned to the cart (parked on `b .`)...
        assert_eq!(m.cpu.regs[15], 0x0800_0004);
        // ...and from outside the BIOS, region-0 reads return the last
        // BIOS prefetch: +8 past the `movs pc, lr` return at 0x10C.
        assert_eq!(m.bus.read32(0x0), 0xCAFE_F00D);
    }

    #[test]
    fn halt_wakes_on_irq() {
        let mut m = Machine::new(vec![0; 64]);
        // bx lr handler that acks all IF bits.
        let handler: &[u32] = &[
            0xE59F_1008, // ldr r1, [pc, #8] -> 0x04000202
            0xE3E0_2000, // mvn r2, #0  (r2 = 0xFFFFFFFF)
            0xE1C1_20B0, // strh r2, [r1]
            0xE12F_FF1E, // bx lr
            0x0400_0202,
        ];
        for (i, w) in handler.iter().enumerate() {
            m.bus.write32(0x0300_0000 + 4 * i as u32, *w);
        }
        m.bus.write32(0x0300_7FFC, 0x0300_0000);
        m.bus.write8(0x0400_0004, 0x08); // vblank irq enable
        m.bus.write16(0x0400_0200, 1);
        m.bus.write16(0x0400_0208, 1);
        m.bus.rom[0..4].copy_from_slice(&0xEAFF_FFFEu32.to_le_bytes());
        m.bus.halted = true;

        let before = m.bus.clock;
        let mut woke = false;
        for _ in 0..10_000 {
            m.step();
            if !m.bus.halted && m.cpu.regs[15] == 0x0800_0000 && m.cpu.mode() == Mode::Sys {
                woke = true;
                break;
            }
        }
        assert!(woke, "halt must wake via IRQ and return to main");
        // The sleep must have skipped time forward, not burned steps.
        assert!(m.bus.clock - before >= crate::mem::CYCLES_PER_SCANLINE * 100);
    }
}
