//! Memory bus abstraction.
//!
//! The bus deals in *aligned* accesses only — the CPU-side rotation rules
//! for misaligned loads (rotated reads, LDRSH degradation) are instruction
//! semantics and live in `exec`, which masks addresses before calling here.

pub trait Bus {
    fn read8(&mut self, addr: u32) -> u8;
    fn write8(&mut self, addr: u32, value: u8);

    fn read16(&mut self, addr: u32) -> u16 {
        let lo = self.read8(addr) as u16;
        let hi = self.read8(addr.wrapping_add(1)) as u16;
        lo | (hi << 8)
    }

    fn read32(&mut self, addr: u32) -> u32 {
        let lo = self.read16(addr) as u32;
        let hi = self.read16(addr.wrapping_add(2)) as u32;
        lo | (hi << 16)
    }

    fn write16(&mut self, addr: u32, value: u16) {
        self.write8(addr, value as u8);
        self.write8(addr.wrapping_add(1), (value >> 8) as u8);
    }

    fn write32(&mut self, addr: u32, value: u32) {
        self.write16(addr, value as u16);
        self.write16(addr.wrapping_add(2), (value >> 16) as u16);
    }

    // HLE hooks for BIOS calls whose effect lives in the machine, not the
    // CPU. Default no-ops keep simple test buses working; `MemMap`
    // implements them.

    /// SWI 0x02 Halt: sleep until any enabled interrupt is latched.
    fn hle_halt(&mut self) {}

    /// SWI 0x04/0x05 IntrWait: sleep until the user IRQ handler sets
    /// `mask` bits at 0x03007FF8. Returns true if the bus models this.
    fn hle_intr_wait(&mut self, _discard: bool, _mask: u16) -> bool {
        false
    }

    /// Called for a SWI the HLE layer doesn't implement. Returning true
    /// suppresses the exception (sensible when no BIOS image is loaded —
    /// vectoring into zeroed memory is strictly worse than a no-op).
    fn note_unhandled_swi(&mut self, _num: u32) -> bool {
        false
    }
}
