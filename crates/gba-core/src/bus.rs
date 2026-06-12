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

    /// SWI 0x04/0x05 IntrWait, one iteration of the BIOS loop: handle the
    /// (first-call-only) discard, then check the flags at 0x03007FF8.
    /// Returns true when the wait is satisfied (matched flags consumed) —
    /// the caller proceeds past the SWI. Returns false when the bus has
    /// halted the CPU instead — the caller must rewind so the SWI
    /// re-executes after wake + handler, exactly like the BIOS loop.
    fn hle_intr_wait(&mut self, _discard: bool, _mask: u16) -> bool {
        true
    }

    /// Called for a SWI the HLE layer doesn't implement. Returning true
    /// suppresses the exception (sensible when no BIOS image is loaded —
    /// vectoring into zeroed memory is strictly worse than a no-op).
    fn note_unhandled_swi(&mut self, _num: u32) -> bool {
        false
    }

    /// Called after an HLE'd SWI completes — steps the BIOS
    /// read-protection value on buses that model it.
    fn note_swi_returned(&mut self) {}

    /// Called with the fetch address before every instruction fetch.
    /// Buses executing a real BIOS image use it to track whether the
    /// CPU is inside the BIOS (region-0 reads return real bytes only
    /// then) and to keep the read-protection value at the last-fetched
    /// BIOS opcode, exactly like hardware.
    fn note_fetch(&mut self, _pc: u32) {}
}
