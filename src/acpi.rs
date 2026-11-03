//! Minimal ACPI table parser.
//!
//! The firmware describes the machine's hardware in ACPI tables: where the
//! interrupt controller lives, where PCI config space is, which interrupt the
//! timer uses. We read those instead of hardcoding addresses, because
//! VirtualBox and QEMU place devices differently, and VirtualBox even moves
//! them around depending on the VM configuration.
//!
//! Only the few fields ROS needs are read. Each table is first turned into a
//! byte slice of its declared length; everything after that is bounds-checked
//! slice parsing (and unit-tested on the host).
//!
//! Reference: ACPI Specification 6.5, chapter 5.2 ("ACPI System Description Tables").

use core::slice;

/// Hardware addresses and settings discovered from the ACPI tables.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Platform {
    /// PCIe ECAM (memory-mapped PCI config space) base, from MCFG.
    pub pci_ecam: u64,
    /// First PCI bus number covered by the ECAM window.
    pub pci_bus_start: u8,
    /// Last PCI bus number covered by the ECAM window.
    pub pci_bus_end: u8,
    /// GICv3 distributor base, from MADT.
    pub gicd: u64,
    /// GICv3 redistributor base, from MADT.
    pub gicr: u64,
    /// Interrupt ID of the EL1 virtual timer, from GTDT.
    pub timer_irq: u32,
    /// PSCI calls go through `hvc` (hypervisor) instead of `smc`, from FADT.
    pub psci_hvc: bool,
    /// PL011 UART base, found in the DSDT (optional, debug output only).
    pub uart: Option<u64>,
}

/// Why the ACPI tables could not be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The RSDP signature is not `"RSD PTR "`.
    BadRsdp,
    /// The RSDP is ACPI 1.0 (no 64-bit XSDT).
    OldRevision,
    /// The XSDT signature is wrong or the table is truncated.
    BadXsdt,
    /// No GIC distributor/redistributor in the MADT: interrupts can't work.
    NoGic,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        f.write_str(match self {
            Error::BadRsdp => "invalid RSDP signature",
            Error::OldRevision => "ACPI 1.0 is not supported (need an XSDT)",
            Error::BadXsdt => "invalid XSDT",
            Error::NoGic => "no GICv3 distributor/redistributor in the MADT",
        })
    }
}

/// Every ACPI table starts with this 36-byte header (signature, length, ...).
const HEADER_LEN: usize = 36;

// Field offsets, relative to the start of the table.
const RSDP_REVISION: usize = 15;
const RSDP_XSDT_ADDRESS: usize = 24;
const MCFG_FIRST_ENTRY: usize = 44; // after the header and 8 reserved bytes
const MADT_FIRST_ENTRY: usize = 44; // after the header, local APIC address and flags
const GTDT_VIRTUAL_TIMER_GSIV: usize = 64;
const FADT_ARM_BOOT_FLAGS: usize = 129;
const FADT_ARM_PSCI_USE_HVC: u16 = 1 << 1;
const FADT_X_DSDT: usize = 140;

// MADT entry types (ACPI 6.5, table 5.21).
const MADT_GICC: u8 = 0x0B;
const MADT_GICD: u8 = 0x0C;
const MADT_GICR: u8 = 0x0E;
/// GICC entry: offset of the per-CPU redistributor base ("GICR Base Address").
const GICC_GICR_BASE: usize = 68;

/// Default virtual timer interrupt (PPI 27) if the firmware has no GTDT.
const DEFAULT_TIMER_IRQ: u32 = 27;

/// Little-endian field readers that return `None` instead of panicking
/// when a (possibly corrupt) table is too short.
fn le<const N: usize>(bytes: &[u8], offset: usize) -> Option<[u8; N]> {
    bytes.get(offset..offset.checked_add(N)?)?.try_into().ok()
}
fn u8_at(b: &[u8], off: usize) -> Option<u8> {
    b.get(off).copied()
}
fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    le(b, off).map(u16::from_le_bytes)
}
fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    le(b, off).map(u32::from_le_bytes)
}
fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    le(b, off).map(u64::from_le_bytes)
}

/// View the ACPI table at physical address `addr` as a byte slice of the
/// length declared in its header.
///
/// # Safety
/// `addr` must point to a valid, identity-mapped ACPI table that is never
/// modified or freed (ACPI memory is reserved for the lifetime of the OS).
unsafe fn table(addr: u64) -> &'static [u8] {
    // SAFETY: the caller guarantees a valid table header at `addr`; reading
    // its length (offset 4) and then exactly that many bytes stays inside it.
    unsafe {
        let len = (addr as *const u8).add(4).cast::<u32>().read_unaligned() as usize;
        slice::from_raw_parts(addr as *const u8, len.max(HEADER_LEN))
    }
}

/// Walk the tables reachable from the RSDP (Root System Description Pointer).
///
/// # Safety
/// `rsdp` must point to a valid, identity-mapped ACPI 2.0+ RSDP, and the
/// tables it references must be valid as well (as provided by the firmware).
pub unsafe fn parse(rsdp: u64) -> Result<Platform, Error> {
    // SAFETY: the RSDP of ACPI 2.0+ is 36 bytes long (caller guarantees validity).
    let rsdp = unsafe { slice::from_raw_parts(rsdp as *const u8, 36) };
    if &rsdp[..8] != b"RSD PTR " {
        return Err(Error::BadRsdp);
    }
    if rsdp[RSDP_REVISION] < 2 {
        return Err(Error::OldRevision);
    }
    let xsdt_addr = u64_at(rsdp, RSDP_XSDT_ADDRESS).ok_or(Error::BadXsdt)?;
    // SAFETY: the firmware-provided RSDP points to a valid XSDT.
    let xsdt = unsafe { table(xsdt_addr) };
    if &xsdt[..4] != b"XSDT" {
        return Err(Error::BadXsdt);
    }

    let mut p = Platform { timer_irq: DEFAULT_TIMER_IRQ, psci_hvc: true, ..Default::default() };
    // The XSDT body is a list of 64-bit pointers to all other tables.
    for entry in xsdt[HEADER_LEN..].as_chunks::<8>().0 {
        let addr = u64::from_le_bytes(*entry);
        // SAFETY: every XSDT entry points to a valid table.
        let t = unsafe { table(addr) };
        match &t[..4] {
            b"MCFG" => parse_mcfg(t, &mut p),
            b"APIC" => parse_madt(t, &mut p),
            b"GTDT" => p.timer_irq = u32_at(t, GTDT_VIRTUAL_TIMER_GSIV).unwrap_or(p.timer_irq),
            b"FACP" => {
                let dsdt = parse_fadt(t, &mut p);
                if let Some(dsdt) = dsdt.filter(|&a| a != 0) {
                    // SAFETY: the FADT's X_DSDT field points to the valid DSDT.
                    p.uart = find_pl011(unsafe { table(dsdt) });
                }
            }
            _ => {}
        }
    }

    if p.gicd == 0 || p.gicr == 0 {
        return Err(Error::NoGic);
    }
    Ok(p)
}

/// MCFG: PCIe ECAM regions. We use the first allocation entry:
/// base (8), segment group (2), start bus (1), end bus (1), reserved (4).
fn parse_mcfg(t: &[u8], p: &mut Platform) {
    let entry = MCFG_FIRST_ENTRY;
    if let (Some(base), Some(start), Some(end)) =
        (u64_at(t, entry), u8_at(t, entry + 10), u8_at(t, entry + 11))
    {
        p.pci_ecam = base;
        p.pci_bus_start = start;
        p.pci_bus_end = end;
    }
}

/// MADT ("APIC"): a list of variable-length `[type, length, ...]` entries
/// describing interrupt controllers.
fn parse_madt(t: &[u8], p: &mut Platform) {
    let mut offset = MADT_FIRST_ENTRY;
    while let (Some(kind), Some(len)) = (u8_at(t, offset), u8_at(t, offset + 1)) {
        let len = len as usize;
        if len < 2 {
            break; // corrupt entry: avoid looping forever
        }
        let Some(entry) = t.get(offset..offset + len) else { break };
        match kind {
            MADT_GICD => p.gicd = u64_at(entry, 8).unwrap_or(p.gicd),
            MADT_GICR => p.gicr = u64_at(entry, 4).unwrap_or(p.gicr),
            // Some firmware only lists per-CPU redistributors in the GICC entries.
            MADT_GICC if p.gicr == 0 => p.gicr = u64_at(entry, GICC_GICR_BASE).unwrap_or(0),
            _ => {}
        }
        offset += len;
    }
}

/// FADT ("FACP"): read the ARM boot flags and return the DSDT address.
fn parse_fadt(t: &[u8], p: &mut Platform) -> Option<u64> {
    if let Some(flags) = u16_at(t, FADT_ARM_BOOT_FLAGS) {
        p.psci_hvc = flags & FADT_ARM_PSCI_USE_HVC != 0;
    }
    u64_at(t, FADT_X_DSDT)
}

/// Find the PL011 UART's base address in the DSDT.
///
/// The DSDT is bytecode (AML), too complex to interpret here. We cheat: find
/// the PL011 hardware ID string `ARMH0011` and take the first `Memory32Fixed`
/// resource descriptor (`0x86, 0x09, 0x00, rw, base:u32, size:u32`) after it.
fn find_pl011(dsdt: &[u8]) -> Option<u64> {
    let id = dsdt.windows(8).position(|w| w == b"ARMH0011")?;
    let rest = &dsdt[id..(id + 128).min(dsdt.len())];
    let res = rest.windows(3).position(|w| w == [0x86, 0x09, 0x00])?;
    u32_at(rest, res + 4).map(u64::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    fn table_with(signature: &[u8; 4], len: usize) -> Vec<u8> {
        let mut t = vec![0u8; len];
        t[..4].copy_from_slice(signature);
        t[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        t
    }

    #[test]
    fn reads_little_endian_fields_with_bounds_checks() {
        let b = [0x78, 0x56, 0x34, 0x12, 0xff];
        assert_eq!(u32_at(&b, 0), Some(0x1234_5678));
        assert_eq!(u16_at(&b, 3), Some(0xff12));
        assert_eq!(u32_at(&b, 2), None);
        assert_eq!(u64_at(&b, usize::MAX), None);
    }

    #[test]
    fn parses_mcfg_first_entry() {
        let mut t = table_with(b"MCFG", 60);
        t[44..52].copy_from_slice(&0x4010_0000_0000u64.to_le_bytes());
        t[54] = 0;
        t[55] = 0xff;
        let mut p = Platform::default();
        parse_mcfg(&t, &mut p);
        assert_eq!((p.pci_ecam, p.pci_bus_start, p.pci_bus_end), (0x4010_0000_0000, 0, 0xff));
    }

    #[test]
    fn parses_madt_gic_entries() {
        let mut t = table_with(b"APIC", 44);
        // GICD entry: type 0x0C, length 24, base at offset 8.
        let mut gicd = vec![0u8; 24];
        gicd[0] = MADT_GICD;
        gicd[1] = 24;
        gicd[8..16].copy_from_slice(&0x0800_0000u64.to_le_bytes());
        // GICR entry: type 0x0E, length 16, base at offset 4.
        let mut gicr = vec![0u8; 16];
        gicr[0] = MADT_GICR;
        gicr[1] = 16;
        gicr[4..12].copy_from_slice(&0x080a_0000u64.to_le_bytes());
        t.extend_from_slice(&gicd);
        t.extend_from_slice(&gicr);

        let mut p = Platform::default();
        parse_madt(&t, &mut p);
        assert_eq!(p.gicd, 0x0800_0000);
        assert_eq!(p.gicr, 0x080a_0000);
    }

    #[test]
    fn madt_stops_on_zero_length_entry() {
        let mut t = table_with(b"APIC", 44);
        t.extend_from_slice(&[MADT_GICD, 0, 0, 0]);
        let mut p = Platform::default();
        parse_madt(&t, &mut p); // must terminate
        assert_eq!(p.gicd, 0);
    }

    #[test]
    fn fadt_reads_psci_conduit_and_dsdt() {
        let mut t = table_with(b"FACP", 276);
        t[FADT_ARM_BOOT_FLAGS..][..2].copy_from_slice(&(FADT_ARM_PSCI_USE_HVC | 1).to_le_bytes());
        t[FADT_X_DSDT..][..8].copy_from_slice(&0xdead_b000u64.to_le_bytes());
        let mut p = Platform::default();
        assert_eq!(parse_fadt(&t, &mut p), Some(0xdead_b000));
        assert!(p.psci_hvc);

        t[FADT_ARM_BOOT_FLAGS] = 1; // PSCI compliant, but via smc
        parse_fadt(&t, &mut p);
        assert!(!p.psci_hvc);
    }

    #[test]
    fn finds_pl011_in_dsdt() {
        let mut dsdt = table_with(b"DSDT", 36);
        dsdt.extend_from_slice(b"\x08_HID\x0dARMH0011\x00\x08_CRS\x11\x14\x0a\x11");
        dsdt.extend_from_slice(&[0x86, 0x09, 0x00, 0x01]);
        dsdt.extend_from_slice(&0x0900_0000u32.to_le_bytes());
        dsdt.extend_from_slice(&0x1000u32.to_le_bytes());
        assert_eq!(find_pl011(&dsdt), Some(0x0900_0000));
    }

    #[test]
    fn no_pl011_without_hardware_id() {
        let mut dsdt = table_with(b"DSDT", 36);
        dsdt.extend_from_slice(&[0x86, 0x09, 0x00, 0x01, 0, 0, 0, 9]);
        assert_eq!(find_pl011(&dsdt), None);
    }
}
