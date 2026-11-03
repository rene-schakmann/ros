//! Interrupts: the GIC interrupt controller and the ARM generic timer.
//!
//! Devices signal the GIC ("Generic Interrupt Controller"); the GIC decides
//! which interrupt goes to the CPU; the CPU jumps to our IRQ vector. We use it
//! for a 100 Hz timer tick, which also wakes the CPU from `wfi` so the shell
//! can poll the keyboard without burning CPU time.
//!
//! Two GIC generations are supported, as reported by the ACPI MADT:
//!
//! - **GICv3** (QEMU, VirtualBox): a *distributor* (GICD) for shared
//!   interrupts, one *redistributor* (GICR) per CPU for private interrupts
//!   (PPIs) such as the timer, and a CPU interface in `ICC_*` system registers.
//! - **GICv2** (Raspberry Pi 5's GIC-400): a distributor that also handles
//!   PPIs, and a memory-mapped CPU interface (GICC).
//!
//! References: Arm GICv3/v4 Architecture Specification (IHI 0069),
//! Arm GICv2 Architecture Specification (IHI 0048).

use crate::arch::{self, read_sysreg, write_sysreg};
use crate::mmio::{read32, read64, write8, write32};
use core::sync::atomic::{AtomicU64, Ordering};

/// Timer interrupts per second.
pub const TICK_HZ: u64 = 100;

static TICKS: AtomicU64 = AtomicU64::new(0);
static TIMER_IRQ: AtomicU64 = AtomicU64::new(27);
static INTERVAL: AtomicU64 = AtomicU64::new(0);
static BOOT_COUNT: AtomicU64 = AtomicU64::new(0);
/// GICv2 CPU interface base; 0 means the GICv3 system register interface.
static GICC_BASE: AtomicU64 = AtomicU64::new(0);

/// Which interrupt controller to drive, with its register bases from ACPI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gic {
    /// GICv2: distributor and memory-mapped CPU interface.
    V2 {
        /// Distributor base.
        gicd: u64,
        /// CPU interface base.
        gicc: u64,
    },
    /// GICv3: distributor and redistributors.
    V3 {
        /// Distributor base.
        gicd: u64,
        /// First redistributor frame.
        gicr: u64,
    },
}

// Distributor registers (global, shared by all CPUs).
const GICD_CTLR: u64 = 0x0000;
const GICD_CTLR_ENABLE: u32 = 1 << 0; // GICv2: enable (group 0 / non-secure group 1)
const GICD_CTLR_ENABLE_GRP1: u32 = 1 << 1;
const GICD_CTLR_ARE: u32 = 1 << 4; // GICv3: affinity routing enable
const GICD_CTLR_RWP: u32 = 1 << 31; // GICv3: register write pending
const GICD_ISENABLER0: u64 = 0x0100; // GICv2: banked per CPU for SGIs/PPIs
const GICD_IPRIORITYR: u64 = 0x0400;

// GICv2 CPU interface registers.
const GICC_CTLR: u64 = 0x0000;
const GICC_CTLR_ENABLE: u32 = 1 << 0;
const GICC_PMR: u64 = 0x0004;
const GICC_BPR: u64 = 0x0008;
const GICC_IAR: u64 = 0x000c;
const GICC_EOIR: u64 = 0x0010;
const GICC_IAR_ID_MASK: u64 = 0x3ff;

// GICv3 redistributor registers (one frame per CPU): control frame + SGI/PPI frame.
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

/// HCR_EL2 bits that route physical FIQ, IRQ and SError to EL2.
const HCR_EL2_FMO_IMO_AMO: u64 = (1 << 3) | (1 << 4) | (1 << 5);

/// Priority for the timer: middle of the range, so the mask (0xff) lets it through.
const TIMER_PRIORITY: u8 = 0x80;

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

/// GICv3: enable the distributor, wake our redistributor, enable the timer
/// PPI and turn on the system register CPU interface.
///
/// # Safety
/// `gicd`/`gicr` must be the GICv3 register frames from ACPI.
unsafe fn init_gicv3(gicd: u64, gicr: u64, irq: u32) {
    // SAFETY: `gicd`/`gicr` are the GIC register frames (caller guarantees),
    // and all offsets below are architected GICv3 registers. The CPU
    // interface only affects interrupt delivery, which is still masked.
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

        let bit = 1u32 << irq;
        write32(rd + GICR_IGROUPR0, read32(rd + GICR_IGROUPR0) | bit);
        write8(rd + GICR_IPRIORITYR + irq as u64, TIMER_PRIORITY);
        write32(rd + GICR_ISENABLER0, bit);

        // CPU interface: enable system register access, allow all priorities,
        // enable group 1. (ICC_*_EL1 registers also serve EL2 when ICC_SRE_EL2.SRE=1,
        // which the firmware sets.)
        write_sysreg!("S3_0_C12_C12_5", read_sysreg!("S3_0_C12_C12_5") | 1); // ICC_SRE_EL1.SRE
        arch::isb();
        write_sysreg!("S3_0_C4_C6_0", 0xff); // ICC_PMR_EL1: priority mask
        write_sysreg!("S3_0_C12_C12_3", 0); // ICC_BPR1_EL1: no priority grouping
        write_sysreg!("S3_0_C12_C12_4", read_sysreg!("S3_0_C12_C12_4") & !2); // ICC_CTLR_EL1: EOI also deactivates
        write_sysreg!("S3_0_C12_C12_7", 1); // ICC_IGRPEN1_EL1: enable group 1
        arch::isb();
    }
}

/// GICv2: enable the distributor and CPU interface and the timer PPI.
///
/// Interrupt groups are left as the firmware configured them (all non-secure
/// on a Raspberry Pi with TF-A; group 0 on QEMU without security extensions).
/// Writing 1 to the enable bits turns on whichever group we can see, which
/// is the same thing Linux does.
///
/// # Safety
/// `gicd`/`gicc` must be the GICv2 register frames from ACPI.
unsafe fn init_gicv2(gicd: u64, gicc: u64, irq: u32) {
    // SAFETY: `gicd`/`gicc` are the GIC register frames (caller guarantees),
    // and all offsets below are architected GICv2 registers.
    unsafe {
        write32(gicd + GICD_CTLR, read32(gicd + GICD_CTLR) | GICD_CTLR_ENABLE);
        write8(gicd + GICD_IPRIORITYR + irq as u64, TIMER_PRIORITY);
        write32(gicd + GICD_ISENABLER0, 1 << irq);

        write32(gicc + GICC_PMR, 0xff);
        write32(gicc + GICC_BPR, 0);
        write32(gicc + GICC_CTLR, read32(gicc + GICC_CTLR) | GICC_CTLR_ENABLE);
    }
    GICC_BASE.store(gicc, Ordering::Relaxed);
}

/// Set up the GIC and start the timer. Interrupts stay masked until
/// [`arch::enable_irqs`] is called.
///
/// # Safety
/// The addresses in `gic` must be the GIC register frames from ACPI, the
/// exception vectors must be installed, and this must be called once, on
/// the boot CPU.
pub unsafe fn init(gic: Gic, timer_irq: u32) {
    TIMER_IRQ.store(timer_irq as u64, Ordering::Relaxed);

    // SAFETY: forwarded from the caller.
    unsafe {
        match gic {
            Gic::V2 { gicd, gicc } => init_gicv2(gicd, gicc, timer_irq),
            Gic::V3 { gicd, gicr } => init_gicv3(gicd, gicr, timer_irq),
        }
    }

    // SAFETY: routing interrupts to our own exception level and arming the
    // virtual timer only affect interrupt delivery, which is still masked.
    unsafe {
        if arch::current_el() == 2 {
            // At EL2, physical interrupts are only taken if they are routed
            // to EL2; otherwise they would target EL1 and stay pending forever.
            write_sysreg!("HCR_EL2", read_sysreg!("HCR_EL2") | HCR_EL2_FMO_IMO_AMO);
            arch::isb();
        }

        // Generic timer: counts at CNTFRQ Hz. Fires when the count reaches CVAL.
        let interval = timer_frequency() / TICK_HZ;
        INTERVAL.store(interval, Ordering::Relaxed);
        let now = counter();
        BOOT_COUNT.store(now, Ordering::Relaxed);
        write_sysreg!("CNTV_CVAL_EL0", now + interval);
        write_sysreg!("CNTV_CTL_EL0", 1); // enable, not masked
    }
}

/// Acknowledge the highest priority pending interrupt. Returns the raw
/// acknowledge value (needed for EOI) and the interrupt ID.
fn acknowledge() -> (u64, u64) {
    match GICC_BASE.load(Ordering::Relaxed) {
        gicc @ 1.. => {
            // SAFETY: GICC_IAR of the GICv2 CPU interface set up in `init`.
            let iar = unsafe { read32(gicc + GICC_IAR) } as u64;
            (iar, iar & GICC_IAR_ID_MASK)
        }
        0 => {
            let iar = read_sysreg!("S3_0_C12_C12_0"); // ICC_IAR1_EL1
            (iar, iar)
        }
    }
}

/// Signal the end of the interrupt acknowledged as `iar`.
fn end_of_interrupt(iar: u64) {
    match GICC_BASE.load(Ordering::Relaxed) {
        // SAFETY: GICC_EOIR of the GICv2 CPU interface, value from GICC_IAR.
        gicc @ 1.. => unsafe { write32(gicc + GICC_EOIR, iar as u32) },
        // SAFETY: ICC_EOIR1_EL1 with the value from ICC_IAR1_EL1.
        0 => unsafe { write_sysreg!("S3_0_C12_C12_1", iar) },
    }
}

/// IRQ handler, called from the exception vector with IRQs masked.
pub fn handle_irq() {
    let (iar, intid) = acknowledge();
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
    end_of_interrupt(iar);
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
