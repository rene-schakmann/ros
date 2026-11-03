//! AArch64 CPU helpers: system registers, barriers, interrupt masking, power.

use core::arch::asm;

/// Read a system register, e.g. `read_sysreg!("CNTFRQ_EL0")`.
///
/// Reading is treated as safe: the registers we read have no side effects,
/// with the documented exception of `ICC_IAR1_EL1` (acknowledges an interrupt),
/// which only the IRQ handler reads.
macro_rules! read_sysreg {
    ($reg:literal) => {{
        let value: u64;
        // SAFETY: reading a system register has no memory-safety implications.
        // (The block may sit inside a caller's `unsafe` block, hence the allow.)
        #[allow(unused_unsafe)]
        unsafe { core::arch::asm!(concat!("mrs {}, ", $reg), out(reg) value, options(nostack)) };
        value
    }};
}

/// Write a system register, e.g. `unsafe { write_sysreg!("VBAR_EL1", addr) }`.
///
/// Expands to an `unsafe` operation: system registers control the MMU, the
/// exception vectors and interrupt routing, so the caller must justify every
/// write in a `// SAFETY:` comment.
macro_rules! write_sysreg {
    ($reg:literal, $value:expr) => {{
        let value: u64 = $value;
        core::arch::asm!(concat!("msr ", $reg, ", {}"), in(reg) value, options(nostack))
    }};
}

pub(crate) use read_sysreg;
pub(crate) use write_sysreg;

/// Exception level we run at (1 = kernel mode).
pub fn current_el() -> u64 {
    (read_sysreg!("CurrentEL") >> 2) & 3
}

/// Sleep until the next interrupt arrives.
pub fn wait_for_interrupt() {
    // SAFETY: `wfi` only pauses the CPU.
    unsafe { asm!("wfi", options(nostack, preserves_flags)) };
}

/// Unmask IRQs (clear the I bit in DAIF).
pub fn enable_irqs() {
    // SAFETY: changing the interrupt mask doesn't touch memory; the handlers
    // were installed before this is called (see `kernel_main`).
    unsafe { asm!("msr daifclr, #2", options(nostack)) };
}

/// Mask IRQs (set the I bit in DAIF).
pub fn disable_irqs() {
    // SAFETY: masking interrupts is always sound.
    unsafe { asm!("msr daifset, #2", options(nostack)) };
}

/// Full system data synchronization barrier: every earlier memory access
/// completes before any later one. Needed before telling a device (via MMIO)
/// to look at memory we just wrote.
pub fn dsb() {
    // SAFETY: a barrier has no side effects besides ordering.
    unsafe { asm!("dsb sy", options(nostack, preserves_flags)) };
}

/// Instruction synchronization barrier: later instructions see the effect of
/// earlier system register writes.
pub fn isb() {
    // SAFETY: a barrier has no side effects besides ordering.
    unsafe { asm!("isb", options(nostack, preserves_flags)) };
}

/// Stop the CPU for good.
pub fn halt() -> ! {
    disable_irqs();
    loop {
        wait_for_interrupt();
    }
}

/// CPU implementer name and part number, decoded from `MIDR_EL1`.
pub fn cpu_name() -> (&'static str, u64) {
    let midr = read_sysreg!("MIDR_EL1");
    let implementer = match (midr >> 24) & 0xff {
        0x41 => "ARM",
        0x61 => "Apple",
        0x51 => "Qualcomm",
        _ => "unknown",
    };
    (implementer, (midr >> 4) & 0xfff)
}

/// Smallest data cache line size in bytes (`CTR_EL0.DminLine`).
fn cache_line() -> u64 {
    4 << ((read_sysreg!("CTR_EL0") >> 16) & 0xf)
}

/// Run the data cache operation `op` on every cache line in `addr..addr+len`.
macro_rules! for_each_line {
    ($op:literal, $addr:expr, $len:expr) => {{
        let line = cache_line();
        let end = $addr + $len as u64;
        let mut a = $addr & !(line - 1);
        while a < end {
            // SAFETY: data cache maintenance by address only moves data
            // between the cache and memory; it never changes its contents.
            unsafe { asm!(concat!("dc ", $op, ", {}"), in(reg) a, options(nostack, preserves_flags)) };
            a += line;
        }
        dsb();
    }};
}

// DMA and caches
// --------------
// On the Raspberry Pi 5 the USB controller (behind the RP1 chip) is not
// cache coherent: it reads and writes RAM directly, past the CPU caches.
// So before a device reads memory the CPU wrote, the CPU must *clean*
// (write back) those cache lines; before the CPU reads memory a device wrote,
// it must *invalidate* its stale copies. Under QEMU and VirtualBox DMA is
// coherent and these operations are cheap no-ops in effect.

/// Write back cached data in `addr..addr+len` to memory, so a device sees it.
pub fn dma_clean(addr: u64, len: usize) {
    for_each_line!("cvac", addr, len);
}

/// Discard cached copies of `addr..addr+len`, so the CPU sees what a device
/// wrote. The range must not hold CPU writes that weren't cleaned yet.
pub fn dma_invalidate(addr: u64, len: usize) {
    for_each_line!("ivac", addr, len);
}

/// Write back and discard cached data in `addr..addr+len`.
pub fn dma_clean_invalidate(addr: u64, len: usize) {
    for_each_line!("civac", addr, len);
}

// PSCI (Power State Coordination Interface): the standard ARM firmware call
// for power management. We trap into the hypervisor with `hvc` (or into secure
// firmware with `smc`), which then powers off or resets the machine.
const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;
const PSCI_SYSTEM_RESET: u64 = 0x8400_0009;

fn psci_call(function: u64, use_hvc: bool) {
    // At EL2 we *are* the hypervisor: `hvc` would trap into our own vectors,
    // so the call must go to the secure firmware (EL3) with `smc`.
    let use_hvc = use_hvc && current_el() < 2;
    // SAFETY: SYSTEM_OFF / SYSTEM_RESET don't return on success; on failure
    // the firmware clobbers at most x0-x3, which are declared as outputs.
    unsafe {
        if use_hvc {
            asm!("hvc #0", inout("x0") function => _, lateout("x1") _, lateout("x2") _,
                 lateout("x3") _, options(nostack));
        } else {
            asm!("smc #0", inout("x0") function => _, lateout("x1") _, lateout("x2") _,
                 lateout("x3") _, options(nostack));
        }
    }
}

/// Ask the firmware/hypervisor to power the machine off.
pub fn power_off(use_hvc: bool) -> ! {
    psci_call(PSCI_SYSTEM_OFF, use_hvc);
    halt()
}

/// Ask the firmware/hypervisor to restart the machine.
pub fn reboot(use_hvc: bool) -> ! {
    psci_call(PSCI_SYSTEM_RESET, use_hvc);
    halt()
}
