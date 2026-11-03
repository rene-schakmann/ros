//! PL011 UART driver: the serial port.
//!
//! Used as a debug log (VirtualBox writes it to `build/serial.log`, QEMU to
//! the terminal) and as a fallback keyboard. Everything printed on screen is
//! mirrored here, which also makes automated testing possible.
//!
//! Reference: Arm PrimeCell UART (PL011) Technical Reference Manual.

use crate::mmio;
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicU64, Ordering};

/// UART base address; 0 means "no UART", and all output is dropped.
static BASE: AtomicU64 = AtomicU64::new(0);

const DR: u64 = 0x00; // data register
const FR: u64 = 0x18; // flag register
const FR_RXFE: u32 = 1 << 4; // receive FIFO empty
const FR_TXFF: u32 = 1 << 5; // transmit FIFO full

/// Start using the UART at `base`. The firmware already configured baud rate
/// etc.; we just remember the address.
///
/// # Safety
/// `base` must be the register base of a PL011 UART (as reported by ACPI)
/// that nothing else drives.
pub unsafe fn init(base: u64) {
    BASE.store(base, Ordering::Relaxed);
}

/// Address of the register at `offset`, or `None` before [`init`].
fn reg(offset: u64) -> Option<u64> {
    match BASE.load(Ordering::Relaxed) {
        0 => None,
        base => Some(base + offset),
    }
}

fn write_byte(b: u8) {
    let (Some(fr), Some(dr)) = (reg(FR), reg(DR)) else { return };
    // SAFETY: `init` was given a valid PL011 base, so FR and DR are its registers.
    unsafe {
        while mmio::read32(fr) & FR_TXFF != 0 {}
        mmio::write32(dr, b as u32);
    }
}

/// Next received byte, if any. Never blocks.
pub fn read_byte() -> Option<u8> {
    let (fr, dr) = (reg(FR)?, reg(DR)?);
    // SAFETY: `init` was given a valid PL011 base, so FR and DR are its registers.
    unsafe { if mmio::read32(fr) & FR_RXFE != 0 { None } else { Some(mmio::read32(dr) as u8) } }
}

/// [`Write`] adapter for the serial port; translates `\n` to `\r\n`.
#[derive(Debug, Clone, Copy)]
pub struct Serial;

impl Write for Serial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            if b == b'\n' {
                write_byte(b'\r');
            }
            write_byte(b);
        }
        Ok(())
    }
}

/// Log a line to the serial port only (not the screen).
macro_rules! log {
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let _ = writeln!($crate::serial::Serial, $($arg)*);
    }};
}
pub(crate) use log;
