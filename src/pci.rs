//! PCI Express bus enumeration.
//!
//! Every PCI device has a 4 KiB "configuration space" describing it (vendor,
//! device type, which memory ranges it uses). With ECAM (Enhanced
//! Configuration Access Mechanism), all of these are mapped into memory:
//! device (bus, dev, func) lives at `ecam + (bus << 20 | dev << 15 | func << 12)`.
//!
//! The firmware already assigned addresses to all devices, so we only read.

use crate::mmio;
use alloc::vec::Vec;

// Configuration space offsets (PCI Local Bus Specification 3.0, section 6.1).
const VENDOR_ID: u64 = 0x00;
const COMMAND: u64 = 0x04;
const CLASS_REVISION: u64 = 0x08;
const HEADER_TYPE: u64 = 0x0e;
const BAR0: u64 = 0x10;

// Command register bits.
const COMMAND_MEMORY_SPACE: u32 = 1 << 1;
const COMMAND_BUS_MASTER: u32 = 1 << 2;
const COMMAND_INTERRUPT_DISABLE: u32 = 1 << 10;

/// Header type bit 7: the device has more than one function.
const HEADER_MULTI_FUNCTION: u8 = 0x80;

/// Class code of an xHCI USB controller: (class, subclass, programming interface).
pub const CLASS_XHCI: (u8, u8, u8) = (0x0c, 0x03, 0x30);

/// A PCI function found during [`scan`].
#[derive(Clone, Copy, Debug)]
pub struct Device {
    /// Bus number.
    pub bus: u8,
    /// Device number on the bus (0-31).
    pub dev: u8,
    /// Function number (0-7).
    pub func: u8,
    /// Vendor ID, e.g. 0x80ee for VirtualBox.
    pub vendor: u16,
    /// Device ID (vendor specific).
    pub device: u16,
    /// Class code, e.g. 0x0c (serial bus controller).
    pub class: u8,
    /// Subclass, e.g. 0x03 (USB).
    pub subclass: u8,
    /// Programming interface, e.g. 0x30 (xHCI).
    pub prog_if: u8,
    /// Address of the function's configuration space.
    config: u64,
}

impl Device {
    /// Read a 32-bit configuration register.
    pub fn read32(&self, offset: u64) -> u32 {
        debug_assert!(offset < 4096 && offset.is_multiple_of(4));
        // SAFETY: `config` is this function's 4 KiB ECAM window (found by `scan`).
        unsafe { mmio::read32(self.config + offset) }
    }

    /// Write a 32-bit configuration register.
    pub fn write32(&self, offset: u64, value: u32) {
        debug_assert!(offset < 4096 && offset.is_multiple_of(4));
        // SAFETY: `config` is this function's 4 KiB ECAM window (found by `scan`).
        unsafe { mmio::write32(self.config + offset, value) }
    }

    /// Address of a memory BAR (Base Address Register): where the device's
    /// registers appear in physical memory. The firmware already assigned it.
    pub fn bar(&self, index: u64) -> u64 {
        let low = self.read32(BAR0 + index * 4);
        let is_64bit = (low >> 1) & 3 == 2;
        let high = if is_64bit { self.read32(BAR0 + 4 + index * 4) as u64 } else { 0 };
        (high << 32) | (low & !0xf) as u64
    }

    /// Let the device respond to memory accesses and do DMA (bus mastering),
    /// with legacy interrupts off (we poll).
    pub fn enable(&self) {
        // The upper 16 bits are the status register (write 1 to clear), so
        // write zeros there.
        let command = self.read32(COMMAND) & 0xffff;
        self.write32(
            COMMAND,
            command | COMMAND_MEMORY_SPACE | COMMAND_BUS_MASTER | COMMAND_INTERRUPT_DISABLE,
        );
    }

    /// Whether the class code matches `(class, subclass, prog_if)`.
    pub fn is(&self, class: (u8, u8, u8)) -> bool {
        (self.class, self.subclass, self.prog_if) == class
    }

    /// Human-readable device type.
    pub fn class_name(&self) -> &'static str {
        class_name(self.class, self.subclass, self.prog_if)
    }

    /// Human-readable vendor name.
    pub fn vendor_name(&self) -> &'static str {
        vendor_name(self.vendor)
    }
}

fn class_name(class: u8, subclass: u8, prog_if: u8) -> &'static str {
    match (class, subclass) {
        (0x00, _) => "unclassified",
        (0x01, 0x00) => "SCSI storage",
        (0x01, 0x06) => "SATA storage",
        (0x01, 0x08) => "NVMe storage",
        (0x01, _) => "storage controller",
        (0x02, _) => "network controller",
        (0x03, _) => "display controller",
        (0x06, 0x00) => "host bridge",
        (0x06, 0x04) => "PCI bridge",
        (0x06, _) => "bridge",
        (0x08, _) => "system peripheral",
        (0x0c, 0x03) => match prog_if {
            0x30 => "USB xHCI controller",
            0x20 => "USB EHCI controller",
            _ => "USB controller",
        },
        _ => "other device",
    }
}

fn vendor_name(vendor: u16) -> &'static str {
    match vendor {
        0x80ee => "VirtualBox",
        0x15ad => "VMware",
        0x1af4 => "Red Hat (virtio)",
        0x1b36 => "Red Hat (QEMU)",
        0x8086 => "Intel",
        0x1022 => "AMD",
        0x10de => "NVIDIA",
        _ => "unknown vendor",
    }
}

/// Probe every bus/device/function slot and collect the ones that exist.
///
/// Returns an empty list when there is no ECAM window (`ecam == 0`).
///
/// # Safety
/// `ecam` must be the ECAM base for buses `bus_start..=bus_end` (from ACPI MCFG).
pub unsafe fn scan(ecam: u64, bus_start: u8, bus_end: u8) -> Vec<Device> {
    let mut found = Vec::new();
    if ecam == 0 {
        return found;
    }
    for bus in bus_start..=bus_end {
        for dev in 0..32u8 {
            for func in 0..8u8 {
                let config =
                    ecam + (((bus - bus_start) as u64) << 20) + ((dev as u64) << 15) + ((func as u64) << 12);
                // SAFETY: `config` lies inside the ECAM window (caller guarantees);
                // empty slots read as all ones.
                let (id, class, header_type) = unsafe {
                    let id = mmio::read32(config + VENDOR_ID);
                    if id & 0xffff == 0xffff {
                        if func == 0 {
                            break; // no device in this slot
                        }
                        continue;
                    }
                    (id, mmio::read32(config + CLASS_REVISION), mmio::read8(config + HEADER_TYPE))
                };
                found.push(Device {
                    bus,
                    dev,
                    func,
                    vendor: id as u16,
                    device: (id >> 16) as u16,
                    class: (class >> 24) as u8,
                    subclass: (class >> 16) as u8,
                    prog_if: (class >> 8) as u8,
                    config,
                });
                if func == 0 && header_type & HEADER_MULTI_FUNCTION == 0 {
                    break; // single-function device
                }
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_known_classes_and_vendors() {
        assert_eq!(class_name(0x0c, 0x03, 0x30), "USB xHCI controller");
        assert_eq!(class_name(0x06, 0x00, 0x00), "host bridge");
        assert_eq!(class_name(0xff, 0x00, 0x00), "other device");
        assert_eq!(vendor_name(0x80ee), "VirtualBox");
        assert_eq!(vendor_name(0x1234), "unknown vendor");
    }
}
