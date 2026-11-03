//! Exception handling: what the CPU does when something interrupts normal flow.
//!
//! On AArch64 the CPU jumps to a table of 16 entry points (the "vector table",
//! address in `VBAR_ELx`) whenever an exception happens:
//! - synchronous: caused by the current instruction (bad memory access,
//!   breakpoint, system call)
//! - IRQ: a device (e.g. the timer) raised an interrupt
//! - FIQ / SError: fast interrupts and asynchronous errors (unused here)
//!
//! Each entry saves all registers into a [`Frame`] on the stack, calls
//! `handle_exception` in Rust, restores the registers and returns with `eret`.
//!
//! ROS runs at whatever exception level the firmware handed over: EL1 in
//! QEMU and VirtualBox, EL2 (hypervisor mode) on the Raspberry Pi 5. The two
//! levels have their own copies of the exception registers (`ELR_EL1` vs.
//! `ELR_EL2`, ...), so the assembly below is a macro that builds one vector
//! table per level, and [`init`] installs the matching one.
//!
//! Reference: Arm Architecture Reference Manual (DDI 0487), chapter D1
//! ("The AArch64 System Level Programmers' Model").

use crate::arch::{self, read_sysreg, write_sysreg};
use crate::console::{self, color, cprint};
use core::arch::global_asm;
use core::mem::{offset_of, size_of};

/// CPU state saved on exception entry. Layout must match the assembly below
/// (checked at compile time).
#[repr(C)]
#[derive(Debug)]
pub struct Frame {
    /// General purpose registers x0-x30.
    pub x: [u64; 31],
    /// Exception link register: the address to return to.
    pub elr: u64,
    /// Saved processor state (flags, interrupt mask, ...).
    pub spsr: u64,
    _pad: u64,
    /// SIMD/FP registers, saved because Rust code may use them.
    q: [u128; 32],
    fpsr: u64,
    fpcr: u64,
}

// The assembly hardcodes these offsets; fail the build if the struct drifts.
const FRAME_SIZE: usize = 800;
const _: () = {
    assert!(size_of::<Frame>() == FRAME_SIZE);
    assert!(offset_of!(Frame, elr) == 248);
    assert!(offset_of!(Frame, q) == 272);
    assert!(offset_of!(Frame, fpsr) == 784);
    assert!(FRAME_SIZE.is_multiple_of(16)); // the stack pointer must stay 16-byte aligned
};

// Exception kinds: the vector table entry index modulo 4.
const KIND_SYNC: u64 = 0;
const KIND_IRQ: u64 = 1;

/// ESR exception class of a `brk` instruction.
const EC_BRK: u64 = 0x3c;

global_asm!(
    r#"
// One vector table entry: make room for a Frame, save x0/x1, pass the entry
// index in x0 and continue in the shared code for exception level `el`.
.macro SAVE_AND_CALL kind, el
    .balign 0x80
    sub sp, sp, #800        // FRAME_SIZE
    stp x0, x1, [sp, #0]
    mov x0, #\kind
    b exception_common_el\el
.endm

// A complete vector table plus its shared save/call/restore code for one
// exception level (1 or 2).
.macro VECTORS el
.section .text.vectors_el\el, "ax"
.balign 2048
.global exception_vectors_el\el
exception_vectors_el\el:
    // Current EL, using SP_EL0
    SAVE_AND_CALL 0, \el
    SAVE_AND_CALL 1, \el
    SAVE_AND_CALL 2, \el
    SAVE_AND_CALL 3, \el
    // Current EL, using SP_ELx (this is us: the kernel)
    SAVE_AND_CALL 4, \el
    SAVE_AND_CALL 5, \el
    SAVE_AND_CALL 6, \el
    SAVE_AND_CALL 7, \el
    // Lower EL, AArch64 (user programs, if we had any)
    SAVE_AND_CALL 8, \el
    SAVE_AND_CALL 9, \el
    SAVE_AND_CALL 10, \el
    SAVE_AND_CALL 11, \el
    // Lower EL, AArch32
    SAVE_AND_CALL 12, \el
    SAVE_AND_CALL 13, \el
    SAVE_AND_CALL 14, \el
    SAVE_AND_CALL 15, \el

// Shared tail of all entries: x0 = kind, x1 = frame. Save the remaining
// registers, call into Rust, restore everything and return.
exception_common_el\el:
    stp x2, x3, [sp, #16]
    stp x4, x5, [sp, #32]
    stp x6, x7, [sp, #48]
    stp x8, x9, [sp, #64]
    stp x10, x11, [sp, #80]
    stp x12, x13, [sp, #96]
    stp x14, x15, [sp, #112]
    stp x16, x17, [sp, #128]
    stp x18, x19, [sp, #144]
    stp x20, x21, [sp, #160]
    stp x22, x23, [sp, #176]
    stp x24, x25, [sp, #192]
    stp x26, x27, [sp, #208]
    stp x28, x29, [sp, #224]
    str x30, [sp, #240]
    mrs x2, elr_el\el
    mrs x3, spsr_el\el
    stp x2, x3, [sp, #248]  // elr, spsr
    add x2, sp, #272        // q registers
    stp q0, q1, [x2, #0]
    stp q2, q3, [x2, #32]
    stp q4, q5, [x2, #64]
    stp q6, q7, [x2, #96]
    stp q8, q9, [x2, #128]
    stp q10, q11, [x2, #160]
    stp q12, q13, [x2, #192]
    stp q14, q15, [x2, #224]
    stp q16, q17, [x2, #256]
    stp q18, q19, [x2, #288]
    stp q20, q21, [x2, #320]
    stp q22, q23, [x2, #352]
    stp q24, q25, [x2, #384]
    stp q26, q27, [x2, #416]
    stp q28, q29, [x2, #448]
    stp q30, q31, [x2, #480]
    mrs x3, fpsr
    mrs x4, fpcr
    str x3, [sp, #784]
    str x4, [sp, #792]

    mov x1, sp
    bl handle_exception

    ldr x3, [sp, #784]
    ldr x4, [sp, #792]
    msr fpsr, x3
    msr fpcr, x4
    add x2, sp, #272
    ldp q0, q1, [x2, #0]
    ldp q2, q3, [x2, #32]
    ldp q4, q5, [x2, #64]
    ldp q6, q7, [x2, #96]
    ldp q8, q9, [x2, #128]
    ldp q10, q11, [x2, #160]
    ldp q12, q13, [x2, #192]
    ldp q14, q15, [x2, #224]
    ldp q16, q17, [x2, #256]
    ldp q18, q19, [x2, #288]
    ldp q20, q21, [x2, #320]
    ldp q22, q23, [x2, #352]
    ldp q24, q25, [x2, #384]
    ldp q26, q27, [x2, #416]
    ldp q28, q29, [x2, #448]
    ldp q30, q31, [x2, #480]
    ldp x2, x3, [sp, #248]
    msr elr_el\el, x2
    msr spsr_el\el, x3
    ldp x0, x1, [sp, #0]
    ldp x2, x3, [sp, #16]
    ldp x4, x5, [sp, #32]
    ldp x6, x7, [sp, #48]
    ldp x8, x9, [sp, #64]
    ldp x10, x11, [sp, #80]
    ldp x12, x13, [sp, #96]
    ldp x14, x15, [sp, #112]
    ldp x16, x17, [sp, #128]
    ldp x18, x19, [sp, #144]
    ldp x20, x21, [sp, #160]
    ldp x22, x23, [sp, #176]
    ldp x24, x25, [sp, #192]
    ldp x26, x27, [sp, #208]
    ldp x28, x29, [sp, #224]
    ldr x30, [sp, #240]
    add sp, sp, #800
    eret
.endm

VECTORS 1
VECTORS 2
"#
);

unsafe extern "C" {
    /// Vector table for running at EL1 (defined in the assembly above).
    static exception_vectors_el1: u8;
    /// Vector table for running at EL2 (defined in the assembly above).
    static exception_vectors_el2: u8;
}

/// Point the CPU at the vector table for the current exception level.
pub fn init() {
    // SAFETY: both tables are complete, 2 KiB aligned vector tables whose
    // entries all lead to `handle_exception`; each is installed only at the
    // exception level whose registers it uses.
    unsafe {
        if arch::current_el() == 2 {
            write_sysreg!("VBAR_EL2", &raw const exception_vectors_el2 as u64);
        } else {
            write_sysreg!("VBAR_EL1", &raw const exception_vectors_el1 as u64);
        }
    }
    arch::isb();
}

/// Exception syndrome register of the current exception level.
fn esr() -> u64 {
    if arch::current_el() == 2 { read_sysreg!("ESR_EL2") } else { read_sysreg!("ESR_EL1") }
}

/// Fault address register of the current exception level.
fn far() -> u64 {
    if arch::current_el() == 2 { read_sysreg!("FAR_EL2") } else { read_sysreg!("FAR_EL1") }
}

/// Exception classes (ESR bits 31:26) worth naming.
fn class_name(ec: u64) -> &'static str {
    match ec {
        0x00 => "unknown instruction",
        0x15 => "system call (svc)",
        0x20 | 0x21 => "instruction abort (bad code address)",
        0x22 => "misaligned program counter",
        0x24 | 0x25 => "data abort (bad memory access)",
        0x26 => "misaligned stack pointer",
        0x2f => "SError (asynchronous external abort)",
        EC_BRK => "breakpoint (brk)",
        _ => "other",
    }
}

/// Rust side of every exception vector. `kind` is the vector table index (0-15).
#[unsafe(no_mangle)]
extern "C" fn handle_exception(kind: u64, frame: &mut Frame) {
    match kind % 4 {
        KIND_IRQ => crate::interrupts::handle_irq(),
        KIND_SYNC => {
            let esr = esr();
            if esr >> 26 == EC_BRK {
                // Breakpoint: report it and continue after the `brk` instruction.
                cprint!(color::YELLOW, "breakpoint hit at {:#x}, resuming\n", frame.elr);
                frame.elr += 4;
                return;
            }
            fatal(frame, esr, "CPU exception");
        }
        _ => fatal(frame, esr(), "unexpected FIQ/SError"),
    }
}

/// Print the panic screen with the saved registers and stop.
fn fatal(frame: &Frame, esr: u64, what: &str) -> ! {
    // SAFETY: the interrupted code might have held the console lock, but it
    // will never run again, so taking the lock over is fine.
    unsafe { console::CONSOLE.force_unlock() };
    let ec = esr >> 26;
    cprint!(color::LIGHT_RED, "\n*** KERNEL PANIC: {what} ***\n");
    cprint!(color::LIGHT_RED, "{} (ESR={esr:#x})\n", class_name(ec));
    cprint!(color::WHITE, "at pc={:#x}  fault address={:#x}\n", frame.elr, far());
    for i in (0..30).step_by(3) {
        cprint!(
            color::GRAY,
            "x{:<2}={:016x}  x{:<2}={:016x}  x{:<2}={:016x}\n",
            i,
            frame.x[i],
            i + 1,
            frame.x[i + 1],
            i + 2,
            frame.x[i + 2]
        );
    }
    cprint!(color::YELLOW, "\nSystem halted. Power-cycle the machine or restart the VM.\n");
    arch::halt()
}
