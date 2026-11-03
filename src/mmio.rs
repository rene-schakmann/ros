//! Device register access (MMIO = memory-mapped I/O).
//!
//! Device registers look like memory, but under a hypervisor every access is
//! trapped and emulated. The hypervisor can only decode simple load/store
//! instructions, so these helpers use inline assembly: a plain `ldr`/`str` with
//! no address write-back. `read_volatile`/`write_volatile` give no such
//! guarantee, and VirtualBox stops the VM with `VERR_NEM_IPE_2` when it meets
//! an instruction form it can't emulate.
//!
//! All functions are `unsafe`: writing to an arbitrary physical address can
//! corrupt memory or reprogram hardware, so callers must make sure `addr` is a
//! register (or RAM) they own.

use core::arch::asm;

/// Read one byte.
///
/// # Safety
/// `addr` must be a mapped, readable device register or memory location.
#[inline]
pub unsafe fn read8(addr: u64) -> u8 {
    let v: u32;
    // SAFETY: the caller guarantees `addr` is readable.
    unsafe { asm!("ldrb {0:w}, [{1}]", out(reg) v, in(reg) addr, options(nostack, readonly)) };
    v as u8
}

/// Read a 32-bit value. `addr` must be 4-byte aligned.
///
/// # Safety
/// `addr` must be a mapped, readable, aligned device register or memory location.
#[inline]
pub unsafe fn read32(addr: u64) -> u32 {
    let v: u32;
    // SAFETY: the caller guarantees `addr` is readable and aligned.
    unsafe { asm!("ldr {0:w}, [{1}]", out(reg) v, in(reg) addr, options(nostack, readonly)) };
    v
}

/// Read a 64-bit value. `addr` must be 8-byte aligned.
///
/// # Safety
/// `addr` must be a mapped, readable, aligned device register or memory location.
#[inline]
pub unsafe fn read64(addr: u64) -> u64 {
    let v: u64;
    // SAFETY: the caller guarantees `addr` is readable and aligned.
    unsafe { asm!("ldr {0}, [{1}]", out(reg) v, in(reg) addr, options(nostack, readonly)) };
    v
}

/// Write one byte.
///
/// # Safety
/// `addr` must be a mapped, writable device register or memory location that
/// the caller owns, and writing `value` must not break any invariant.
#[inline]
pub unsafe fn write8(addr: u64, value: u8) {
    // SAFETY: the caller guarantees `addr` is writable and owned by them.
    unsafe { asm!("strb {0:w}, [{1}]", in(reg) value as u32, in(reg) addr, options(nostack)) };
}

/// Write a 32-bit value. `addr` must be 4-byte aligned.
///
/// # Safety
/// Same as [`write8`], and `addr` must be aligned.
#[inline]
pub unsafe fn write32(addr: u64, value: u32) {
    // SAFETY: the caller guarantees `addr` is writable, aligned and owned by them.
    unsafe { asm!("str {0:w}, [{1}]", in(reg) value, in(reg) addr, options(nostack)) };
}

/// Write a 64-bit value as two 32-bit writes, low half first.
///
/// Many devices (e.g. xHCI) accept 64-bit registers this way, and it avoids
/// relying on 64-bit MMIO support in the hypervisor.
///
/// # Safety
/// Same as [`write32`] for both `addr` and `addr + 4`.
#[inline]
pub unsafe fn write64(addr: u64, value: u64) {
    // SAFETY: forwarded from the caller.
    unsafe {
        write32(addr, value as u32);
        write32(addr + 4, (value >> 32) as u32);
    }
}
