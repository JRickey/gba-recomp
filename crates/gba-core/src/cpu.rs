//! ARM7TDMI register file, CPSR/SPSR, and mode banking.

/// CPSR bit positions.
pub const FLAG_N: u32 = 1 << 31;
pub const FLAG_Z: u32 = 1 << 30;
pub const FLAG_C: u32 = 1 << 29;
pub const FLAG_V: u32 = 1 << 28;
pub const FLAG_I: u32 = 1 << 7;
pub const FLAG_F: u32 = 1 << 6;
pub const FLAG_T: u32 = 1 << 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    User = 0x10,
    Fiq = 0x11,
    Irq = 0x12,
    Svc = 0x13,
    Abt = 0x17,
    Und = 0x1B,
    Sys = 0x1F,
}

impl Mode {
    pub fn from_bits(bits: u32) -> Mode {
        match bits & 0x1F {
            0x10 => Mode::User,
            0x11 => Mode::Fiq,
            0x12 => Mode::Irq,
            0x13 => Mode::Svc,
            0x17 => Mode::Abt,
            0x1B => Mode::Und,
            _ => Mode::Sys, // 0x1F; anything else is unpredictable on real HW
        }
    }

    /// Bank index for r13/r14/SPSR storage. User and System share bank 0.
    fn bank(self) -> usize {
        match self {
            Mode::User | Mode::Sys => 0,
            Mode::Fiq => 1,
            Mode::Irq => 2,
            Mode::Svc => 3,
            Mode::Abt => 4,
            Mode::Und => 5,
        }
    }

    pub fn has_spsr(self) -> bool {
        !matches!(self, Mode::User | Mode::Sys)
    }
}

/// Exception vectors (all in BIOS at 0x00000000).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exception {
    Undefined = 0x04,
    Swi = 0x08,
    Irq = 0x18,
}

#[repr(C)]
pub struct Cpu {
    /// Active register view for the current mode. `regs[15]` holds the
    /// *fetch* address of the next instruction between steps; during
    /// execution PC operand reads go through the pipeline-offset helpers
    /// in `exec`.
    pub regs: [u32; 16],
    pub cpsr: u32,
    /// Pending PC write from the executing instruction (raw, unaligned).
    pub branch: Option<u32>,
    /// Banked r13/r14 by bank index (usr/sys, fiq, irq, svc, abt, und).
    bank_r13: [u32; 6],
    bank_r14: [u32; 6],
    spsr: [u32; 6], // index 0 unused (usr/sys have no SPSR)
    /// r8-r12 are banked only for FIQ.
    usr_r8_12: [u32; 5],
    fiq_r8_12: [u32; 5],
}

impl Cpu {
    /// Power-on state as left by the BIOS when jumping to a cartridge:
    /// System mode, IRQs enabled in CPSR, BIOS-conventional stack pointers.
    pub fn new() -> Cpu {
        let mut cpu = Cpu {
            regs: [0; 16],
            cpsr: Mode::Sys as u32,
            branch: None,
            bank_r13: [0; 6],
            bank_r14: [0; 6],
            spsr: [0; 6],
            usr_r8_12: [0; 5],
            fiq_r8_12: [0; 5],
        };
        cpu.bank_r13[Mode::Sys.bank()] = 0x0300_7F00;
        cpu.bank_r13[Mode::Irq.bank()] = 0x0300_7FA0;
        cpu.bank_r13[Mode::Svc.bank()] = 0x0300_7FE0;
        cpu.regs[13] = 0x0300_7F00;
        cpu.regs[15] = 0x0800_0000;
        cpu
    }

    /// Hardware reset state, for booting a real BIOS from the reset
    /// vector: SVC mode with IRQ+FIQ masked, ARM state, PC = 0, every
    /// register and bank zeroed — the BIOS sets up the stacks itself.
    pub fn reset_hard_boot(&mut self) {
        self.regs = [0; 16];
        self.cpsr = Mode::Svc as u32 | FLAG_I | FLAG_F;
        self.branch = None;
        self.bank_r13 = [0; 6];
        self.bank_r14 = [0; 6];
        self.spsr = [0; 6];
        self.usr_r8_12 = [0; 5];
        self.fiq_r8_12 = [0; 5];
    }

    pub fn mode(&self) -> Mode {
        Mode::from_bits(self.cpsr)
    }

    pub fn thumb(&self) -> bool {
        self.cpsr & FLAG_T != 0
    }

    pub fn set_thumb(&mut self, thumb: bool) {
        if thumb {
            self.cpsr |= FLAG_T;
        } else {
            self.cpsr &= !FLAG_T;
        }
    }

    pub fn flag(&self, mask: u32) -> bool {
        self.cpsr & mask != 0
    }

    pub fn set_flag(&mut self, mask: u32, value: bool) {
        if value {
            self.cpsr |= mask;
        } else {
            self.cpsr &= !mask;
        }
    }

    /// Write CPSR wholesale, performing register re-banking on mode change.
    pub fn set_cpsr(&mut self, value: u32) {
        let old = self.mode();
        let new = Mode::from_bits(value);
        if old != new {
            self.swap_banks(old, new);
        }
        self.cpsr = value;
    }

    fn swap_banks(&mut self, old: Mode, new: Mode) {
        // Save the active view into the old mode's banks.
        self.bank_r13[old.bank()] = self.regs[13];
        self.bank_r14[old.bank()] = self.regs[14];
        if old == Mode::Fiq {
            self.fiq_r8_12.copy_from_slice(&self.regs[8..13]);
        } else {
            self.usr_r8_12.copy_from_slice(&self.regs[8..13]);
        }
        // Load the new mode's banks into the active view.
        self.regs[13] = self.bank_r13[new.bank()];
        self.regs[14] = self.bank_r14[new.bank()];
        if new == Mode::Fiq {
            self.regs[8..13].copy_from_slice(&self.fiq_r8_12);
        } else {
            self.regs[8..13].copy_from_slice(&self.usr_r8_12);
        }
    }

    pub fn spsr(&self) -> u32 {
        let mode = self.mode();
        if mode.has_spsr() {
            self.spsr[mode.bank()]
        } else {
            // Reading SPSR in User/System is unpredictable; return CPSR.
            self.cpsr
        }
    }

    pub fn set_spsr(&mut self, value: u32) {
        let mode = self.mode();
        if mode.has_spsr() {
            self.spsr[mode.bank()] = value;
        }
    }

    /// Read a register from the User bank regardless of current mode
    /// (for LDM/STM with the S bit).
    pub fn user_reg(&self, r: u8) -> u32 {
        let r = r as usize;
        match r {
            8..=12 if self.mode() == Mode::Fiq => self.usr_r8_12[r - 8],
            13 => {
                if self.mode().bank() == 0 {
                    self.regs[13]
                } else {
                    self.bank_r13[0]
                }
            }
            14 => {
                if self.mode().bank() == 0 {
                    self.regs[14]
                } else {
                    self.bank_r14[0]
                }
            }
            _ => self.regs[r],
        }
    }

    pub fn set_user_reg(&mut self, r: u8, value: u32) {
        let r = r as usize;
        match r {
            8..=12 if self.mode() == Mode::Fiq => self.usr_r8_12[r - 8] = value,
            13 => {
                if self.mode().bank() == 0 {
                    self.regs[13] = value;
                } else {
                    self.bank_r13[0] = value;
                }
            }
            14 => {
                if self.mode().bank() == 0 {
                    self.regs[14] = value;
                } else {
                    self.bank_r14[0] = value;
                }
            }
            15 => {} // never a user-bank target in LDM^ semantics we model
            _ => self.regs[r] = value,
        }
    }

    /// Take an exception: bank SPSR/LR, switch mode, mask IRQs, clear Thumb,
    /// and jump to the vector. `return_addr` is what the handler's return
    /// idiom expects in LR (already exception-specific).
    pub fn enter_exception(&mut self, exc: Exception, return_addr: u32) {
        let target_mode = match exc {
            Exception::Undefined => Mode::Und,
            Exception::Swi => Mode::Svc,
            Exception::Irq => Mode::Irq,
        };
        let old_cpsr = self.cpsr;
        let new_cpsr = (self.cpsr & !0x1F & !FLAG_T) | target_mode as u32 | FLAG_I;
        self.set_cpsr(new_cpsr);
        self.spsr[target_mode.bank()] = old_cpsr;
        self.regs[14] = return_addr;
        self.branch = Some(exc as u32);
    }
}

impl Default for Cpu {
    fn default() -> Self {
        Cpu::new()
    }
}
