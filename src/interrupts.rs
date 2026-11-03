//! Interrupts: the GICv3 interrupt controller and the ARM generic timer.
//!
//! Devices signal the GIC ("Generic Interrupt Controller"); the GIC decides
//! which interrupt goes to the CPU; the CPU jumps to our IRQ vector. We use it
//! for a 100 Hz timer tick, which also wakes the CPU from `wfi` so the shell
//! can poll the keyboard without burning CPU time.
//!
//! The GIC has three parts:
//! - the *distributor* (GICD, one per system) routes shared interrupts,
//! - one *redistributor* (GICR) per CPU handles private interrupts (PPIs),
//!   such as the timer,
//! - the *CPU interface* is accessed through `ICC_*` system registers.
//!
//! Reference: Arm GICv3/v4 Architecture Specification (IHI 0069).

use crate::arch::{self, read_sysreg, write_sysreg};
use crate::mmio::{read32, read64, write8, write32};
use core::sync::atomic::{AtomicU64, Ordering};

/// Timer interrupts per second.
pub const TICK_HZ: u64 = 100;

static TICKS: AtomicU64 = AtomicU64::new(0);
static TIMER_IRQ: AtomicU64 = AtomicU64::new(27);
static INTERVAL: AtomicU64 = AtomicU64::new(0);
static BOOT_COUNT: AtomicU64 = AtomicU64::new(0);

// Distributor registers (global, shared by all CPUs).
const GICD_CTLR: u64 = 0x0000;
const GICD_CTLR_ENABLE_GRP1: u32 = 1 << 1;
const GICD_CTLR_ARE: u32 = 1 << 4; // affinity routing enable
const GICD_CTLR_RWP: u32 = 1 << 31; // register write pending

// Redistributor registers (one frame per CPU): control frame + SGI/PPI frame.
const GICR_TYPER: u64 = 0x0008;
const GICR_TYPER_LAST: u64 = 1 << 4;
const GICR_WAKER: u64 = 0x0014;
const GICR_WAKER_PROCESSOR_SLEEP: u32 = 1 << 1;
const GICR_WAKER_CHILDREN_ASLEEP: u32 = 1 << 2;
const GICR_FRAME_SIZE: u64 = 0x20000;
const GICR_SGI: u64 = 0x10000;
const GICR_IGROUPR0: u64 = GICR_SGI + 0x0080;
const GICR_ISENABLER0: u64 = GICR_SGI + 0x0100;
const GICR_IPRIORITYR: u64 = GICR_SGI + 0x0400;

/// Interrupt IDs 1020-1023 are special ("spurious": nothing to handle).
const SPURIOUS_INTID: u64 = 1020;

/// Find the redistributor frame belonging to this CPU (matched by affinity).
///
/// # Safety
/// `gicr` must be the redistributor base reported by ACPI.
unsafe fn find_redistributor(gicr: u64) -> u64 {
    let mpidr = read_sysreg!("MPIDR_EL1");
    // GICR_TYPER[63:32] holds Aff3.Aff2.Aff1.Aff0; MPIDR has Aff3 at bit 32.
    let affinity = (mpidr & 0xff_ffff) | (((mpidr >> 32) & 0xff) << 24);
    let mut frame = gicr;
    loop {
        // SAFETY: frames are contiguous from `gicr` until the one marked Last.
        let typer = unsafe { read64(frame + GICR_TYPER) };
        if typer >> 32 == affinity || typer & GICR_TYPER_LAST != 0 {
            return frame; // matching CPU, or the last frame
        }
        frame += GICR_FRAME_SIZE;
    }
}

/// Set up the GIC and start the timer. Interrupts stay masked until
/// [`arch::enable_irqs`] is called.
///
/// # Safety
/// `gicd` and `gicr` must be the GICv3 distributor and redistributor bases
/// from ACPI, the exception vectors must be installed, and this must be
/// called once, on the boot CPU.
pub unsafe fn init(gicd: u64, gicr: u64, timer_irq: u32) {
    TIMER_IRQ.store(timer_irq as u64, Ordering::Relaxed);

    // SAFETY: `gicd`/`gicr` are the GIC register frames (caller guarantees),
    // and all offsets below are architected GICv3 registers.
    unsafe {
        // Distributor: enable affinity routing and group 1 interrupts.
        let ctlr = read32(gicd + GICD_CTLR);
        write32(gicd + GICD_CTLR, ctlr | GICD_CTLR_ARE | GICD_CTLR_ENABLE_GRP1);
        while read32(gicd + GICD_CTLR) & GICD_CTLR_RWP != 0 {}

        // Redistributor: wake it up, then enable the timer's interrupt (a PPI,
        // i.e. private to this CPU) in group 1 with middle priority.
        let rd = find_redistributor(gicr);
        write32(rd + GICR_WAKER, read32(rd + GICR_WAKER) & !GICR_WAKER_PROCESSOR_SLEEP);
        while read32(rd + GICR_WAKER) & GICR_WAKER_CHILDREN_ASLEEP != 0 {}

        let bit = 1u32 << timer_irq;
        write32(rd + GICR_IGROUPR0, read32(rd + GICR_IGROUPR0) | bit);
        write8(rd + GICR_IPRIORITYR + timer_irq as u64, 0x80);
        write32(rd + GICR_ISENABLER0, bit);
    }

    // SAFETY: programming the CPU interface and the virtual timer only affects
    // interrupt delivery, which is still masked (DAIF.I) at this point.
    unsafe {
        // CPU interface: enable system register access, allow all priorities,
        // enable group 1.
        write_sysreg!("S3_0_C12_C12_5", read_sysreg!("S3_0_C12_C12_5") | 1); // ICC_SRE_EL1.SRE
        arch::isb();
        write_sysreg!("S3_0_C4_C6_0", 0xff); // ICC_PMR_EL1: priority mask
        write_sysreg!("S3_0_C12_C12_3", 0); // ICC_BPR1_EL1: no priority grouping
        write_sysreg!("S3_0_C12_C12_4", read_sysreg!("S3_0_C12_C12_4") & !2); // ICC_CTLR_EL1: EOI also deactivates
        write_sysreg!("S3_0_C12_C12_7", 1); // ICC_IGRPEN1_EL1: enable group 1
        arch::isb();

        // Generic timer: counts at CNTFRQ Hz. Fires when the count reaches CVAL.
        let interval = timer_frequency() / TICK_HZ;
        INTERVAL.store(interval, Ordering::Relaxed);
        let now = counter();
        BOOT_COUNT.store(now, Ordering::Relaxed);
        write_sysreg!("CNTV_CVAL_EL0", now + interval);
        write_sysreg!("CNTV_CTL_EL0", 1); // enable, not masked
    }
}

/// IRQ handler, called from the exception vector with IRQs masked.
pub fn handle_irq() {
    let intid = read_sysreg!("S3_0_C12_C12_0"); // ICC_IAR1_EL1: acknowledge
    if intid >= SPURIOUS_INTID {
        return; // spurious: nothing pending any more
    }
    if intid == TIMER_IRQ.load(Ordering::Relaxed) {
        TICKS.fetch_add(1, Ordering::Relaxed);
        // Schedule the next tick relative to the last one, so ticks don't drift.
        let next = read_sysreg!("CNTV_CVAL_EL0") + INTERVAL.load(Ordering::Relaxed);
        // SAFETY: re-arming our own timer.
        unsafe { write_sysreg!("CNTV_CVAL_EL0", next.max(counter())) };
    }
    // SAFETY: signals end of interrupt for the ID we just acknowledged.
    unsafe { write_sysreg!("S3_0_C12_C12_1", intid) }; // ICC_EOIR1_EL1
}

/// Number of timer interrupts handled since boot.
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Frequency of the system counter in Hz.
pub fn timer_frequency() -> u64 {
    read_sysreg!("CNTFRQ_EL0")
}

/// Current value of the virtual system counter.
pub fn counter() -> u64 {
    arch::isb(); // don't let the read happen early (out of order)
    read_sysreg!("CNTVCT_EL0")
}

/// Convert counter ticks to milliseconds without overflowing
/// (`ticks * 1000` would overflow after ~9 days at 24 MHz).
fn ticks_to_ms(ticks: u64, frequency: u64) -> u64 {
    (ticks as u128 * 1000 / frequency.max(1) as u128) as u64
}

/// Convert milliseconds to counter ticks.
fn ms_to_ticks(ms: u64, frequency: u64) -> u64 {
    (ms as u128 * frequency as u128 / 1000) as u64
}

/// Milliseconds since the kernel started the timer.
pub fn uptime_ms() -> u64 {
    ticks_to_ms(counter().wrapping_sub(BOOT_COUNT.load(Ordering::Relaxed)), timer_frequency())
}

/// Counter value `ms` milliseconds from now, for timeouts.
pub fn deadline(ms: u64) -> u64 {
    counter().saturating_add(ms_to_ticks(ms, timer_frequency()))
}

/// Busy-wait. Works even with interrupts masked (reads the counter directly).
pub fn delay_ms(ms: u64) {
    let end = deadline(ms);
    while counter() < end {
        core::hint::spin_loop();
    }
}

/// Wait until `done()` returns true, or give up after `ms` milliseconds.
/// Returns whether `done()` became true.
pub fn wait_until(ms: u64, mut done: impl FnMut() -> bool) -> bool {
    let end = deadline(ms);
    while counter() < end {
        if done() {
            return true;
        }
        core::hint::spin_loop();
    }
    done()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_ticks_without_overflow() {
        let freq = 24_000_000;
        assert_eq!(ticks_to_ms(freq, freq), 1000);
        // 30 days of uptime: `ticks * 1000` would overflow u64.
        let month = freq * 60 * 60 * 24 * 30;
        assert_eq!(ticks_to_ms(month, freq), 30 * 24 * 60 * 60 * 1000);
        assert_eq!(ms_to_ticks(1500, freq), 36_000_000);
        assert_eq!(ticks_to_ms(5, 0), 5000); // no division by zero
    }

    #[test]
    fn wait_until_returns_early_and_times_out() {
        assert!(wait_until(1000, || true));
        assert!(!wait_until(1, || false));
    }
}
