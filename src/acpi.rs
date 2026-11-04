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
//! Every table carries a checksum: all its bytes add up to 0 (mod 256). A
//! wrong checksum means a firmware bug or corrupted memory. Like Linux, ROS
//! reports it but still uses the table: refusing to boot would not help.
//!
//! Reference: ACPI Specification 6.5, chapter 5.2 ("ACPI System Description Tables").

use crate::interrupts::Gic;
use core::slice;

/// The machine ROS is running on, as far as it needs to know.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Board {
    /// A virtual machine or any other standards-compliant machine.
    #[default]
    Generic,
    /// Raspberry Pi 5 with the community UEFI firmware (worproject/rpi5-uefi).
    /// Its PCIe controllers are not ECAM compliant and its USB controllers
    /// sit inside the RP1 I/O chip instead of being PCI functions.
    RaspberryPi5,
}

impl Board {
    /// Human-readable board name.
    pub fn name(&self) -> &'static str {
        match self {
            Board::Generic => "generic UEFI machine",
            Board::RaspberryPi5 => "Raspberry Pi 5",
        }
    }
}

/// Hardware addresses and settings discovered from the ACPI tables.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Platform {
    /// PCIe ECAM (memory-mapped PCI config space) base, from MCFG.
    pub pci_ecam: u64,
    /// First PCI bus number covered by the ECAM window.
    pub pci_bus_start: u8,
    /// Last PCI bus number covered by the ECAM window.
    pub pci_bus_end: u8,
    /// GIC version (2 or 3), from MADT (0 if the firmware didn't say).
    pub gic_version: u8,
    /// GIC distributor base, from MADT.
    pub gicd: u64,
    /// GICv3 redistributor base, from MADT.
    pub gicr: u64,
    /// GICv2 CPU interface base, from MADT.
    pub gicc: u64,
    /// Interrupt ID of the EL1 virtual timer, from GTDT.
    pub timer_irq: u32,
    /// PSCI calls go through `hvc` (hypervisor) instead of `smc`, from FADT.
    pub psci_hvc: bool,
    /// PL011 UART base, from SPCR or the DSDT (optional, debug output only).
    pub uart: Option<u64>,
    /// Which board this is, from the DSDT's OEM IDs.
    pub board: Board,
    /// Raspberry Pi 5: where the firmware mapped the RP1 I/O chip's
    /// peripheral BAR (patched into the DSDT as `PBAR`).
    pub rp1_bar: Option<u64>,
    /// Signature of the first table with a wrong checksum, if any.
    pub bad_checksum: Option<[u8; 4]>,
}

impl Platform {
    /// The interrupt controller to drive, or an error if the MADT was incomplete.
    pub fn gic(&self) -> Result<Gic, Error> {
        let v3 = self.gic_version >= 3 || (self.gic_version == 0 && self.gicr != 0);
        match (v3, self.gicd, self.gicr, self.gicc) {
            (true, gicd @ 1.., gicr @ 1.., _) => Ok(Gic::V3 { gicd, gicr }),
            (false, gicd @ 1.., _, gicc @ 1..) => Ok(Gic::V2 { gicd, gicc }),
            _ => Err(Error::NoGic),
        }
    }

    /// Base addresses of xHCI USB controllers that are not PCI functions.
    pub fn platform_xhci(&self) -> impl Iterator<Item = u64> {
        let rp1 = self.rp1_bar.into_iter();
        rp1.flat_map(|bar| [bar + RP1_USBHOST0, bar + RP1_USBHOST1])
    }
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
    /// No usable GIC in the MADT: interrupts can't work.
    NoGic,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        f.write_str(match self {
            Error::BadRsdp => "invalid RSDP signature",
            Error::OldRevision => "ACPI 1.0 is not supported (need an XSDT)",
            Error::BadXsdt => "invalid XSDT",
            Error::NoGic => "no usable GICv2/GICv3 interrupt controller in the MADT",
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
const HEADER_OEM_ID: usize = 10; // 6 bytes
const HEADER_OEM_TABLE_ID: usize = 16; // 8 bytes
const SPCR_INTERFACE_TYPE: usize = 36;
const SPCR_BASE_ADDRESS: usize = 40; // a Generic Address Structure (GAS)

// MADT entry types (ACPI 6.5, table 5.21).
const MADT_GICC: u8 = 0x0B;
const MADT_GICD: u8 = 0x0C;
const MADT_GICR: u8 = 0x0E;
// GICC entry fields (ACPI 6.5, table 5.37).
const GICC_FLAGS: usize = 12;
const GICC_FLAG_ENABLED: u32 = 1 << 0;
const GICC_PHYSICAL_BASE: usize = 32; // GICv2 CPU interface
const GICC_GICR_BASE: usize = 60; // per-CPU redistributor (GICv3)
// GICD entry fields (ACPI 6.5, table 5.39).
const GICD_BASE: usize = 8;
const GICD_VERSION: usize = 20;
// GICR entry fields (ACPI 6.5, table 5.41).
const GICR_DISCOVERY_BASE: usize = 4;

// SPCR interface types (Microsoft "Debug Port Table 2" spec, table 3).
const SPCR_PL011: u8 = 0x03;
const SPCR_SBSA_32BIT: u8 = 0x0d;
const SPCR_SBSA: u8 = 0x0e;
/// GAS address space ID for system memory (MMIO).
const GAS_SYSTEM_MEMORY: u8 = 0;

// Raspberry Pi 5 / RP1 (from the firmware sources: edk2-platforms, RpiSiliconPkg/Include/Rp1.h).
const RPI5_OEM_ID: &[u8; 6] = b"RPIFDN";
const RPI5_OEM_TABLE_ID: &[u8; 4] = b"RPI5";
/// Placeholder the firmware replaces with the real value; still there means "not mapped".
const ACPI_PATCH_QWORD_VALUE: u64 = 0xABCD_EF01_2345_6789;
/// Offsets of the two xHCI controllers inside the RP1 peripheral BAR.
const RP1_USBHOST0: u64 = 0x0020_0000;
const RP1_USBHOST1: u64 = 0x0030_0000;

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

/// Whether all bytes of `bytes` add up to 0 (mod 256), as ACPI requires.
fn checksum_ok(bytes: &[u8]) -> bool {
    bytes.iter().fold(0u8, |sum, &b| sum.wrapping_add(b)) == 0
}

/// The RSDP has two checksums: one over the ACPI 1.0 part (20 bytes) and
/// one over the whole ACPI 2.0 structure (36 bytes).
fn rsdp_checksum_ok(rsdp: &[u8]) -> bool {
    rsdp.len() >= 36 && checksum_ok(&rsdp[..20]) && checksum_ok(&rsdp[..36])
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
    // Remember the first table whose checksum is wrong.
    let check = |t: &[u8], p: &mut Platform| {
        if !checksum_ok(t) && p.bad_checksum.is_none() {
            p.bad_checksum = t.first_chunk::<4>().copied();
        }
    };
    if !rsdp_checksum_ok(rsdp) {
        p.bad_checksum = Some(*b"RSDP");
    }
    check(xsdt, &mut p);
    // The XSDT body is a list of 64-bit pointers to all other tables.
    for entry in xsdt[HEADER_LEN..].as_chunks::<8>().0 {
        let addr = u64::from_le_bytes(*entry);
        // SAFETY: every XSDT entry points to a valid table.
        let t = unsafe { table(addr) };
        check(t, &mut p);
        match &t[..4] {
            b"MCFG" => parse_mcfg(t, &mut p),
            b"APIC" => parse_madt(t, &mut p),
            b"GTDT" => p.timer_irq = u32_at(t, GTDT_VIRTUAL_TIMER_GSIV).unwrap_or(p.timer_irq),
            b"SPCR" => p.uart = p.uart.or(parse_spcr(t)),
            b"FACP" => {
                let dsdt = parse_fadt(t, &mut p);
                if let Some(dsdt) = dsdt.filter(|&a| a != 0) {
                    // SAFETY: the FADT's X_DSDT field points to the valid DSDT.
                    let dsdt = unsafe { table(dsdt) };
                    check(dsdt, &mut p);
                    parse_dsdt(dsdt, &mut p);
                }
            }
            _ => {}
        }
    }

    p.gic()?;
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
            MADT_GICD => {
                p.gicd = u64_at(entry, GICD_BASE).unwrap_or(p.gicd);
                p.gic_version = u8_at(entry, GICD_VERSION).unwrap_or(0);
            }
            MADT_GICR => p.gicr = u64_at(entry, GICR_DISCOVERY_BASE).unwrap_or(p.gicr),
            // The first enabled CPU is the boot CPU (good enough for one core).
            MADT_GICC if p.gicc == 0 => {
                let enabled = u32_at(entry, GICC_FLAGS).is_some_and(|f| f & GICC_FLAG_ENABLED != 0);
                if enabled {
                    p.gicc = u64_at(entry, GICC_PHYSICAL_BASE).unwrap_or(0);
                    // Some firmware only lists per-CPU redistributors in the GICC entries.
                    if p.gicr == 0 {
                        p.gicr = u64_at(entry, GICC_GICR_BASE).unwrap_or(0);
                    }
                }
            }
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

/// SPCR (Serial Port Console Redirection): the firmware's debug console.
/// Returns the base of a PL011-compatible UART in MMIO space.
fn parse_spcr(t: &[u8]) -> Option<u64> {
    let kind = u8_at(t, SPCR_INTERFACE_TYPE)?;
    let space = u8_at(t, SPCR_BASE_ADDRESS)?;
    let base = u64_at(t, SPCR_BASE_ADDRESS + 4)?;
    let pl011 = matches!(kind, SPCR_PL011 | SPCR_SBSA_32BIT | SPCR_SBSA);
    (pl011 && space == GAS_SYSTEM_MEMORY && base != 0).then_some(base)
}

/// Read what we need from the DSDT: board identity, the UART (if SPCR
/// didn't name one) and, on a Raspberry Pi 5, the RP1 BAR.
fn parse_dsdt(dsdt: &[u8], p: &mut Platform) {
    let oem_id = dsdt.get(HEADER_OEM_ID..HEADER_OEM_ID + 6);
    let oem_table = dsdt.get(HEADER_OEM_TABLE_ID..HEADER_OEM_TABLE_ID + 4);
    if oem_id == Some(RPI5_OEM_ID) && oem_table == Some(RPI5_OEM_TABLE_ID) {
        p.board = Board::RaspberryPi5;
        p.rp1_bar = find_qword_name(dsdt, b"PBAR").filter(|&v| v != ACPI_PATCH_QWORD_VALUE && v != 0);
    }
    if p.uart.is_none() {
        p.uart = find_pl011(dsdt);
    }
}

/// Find `Name (NAME, <qword>)` in AML and return the value.
///
/// AML encodes it as `NameOp (0x08)`, the 4-character name, `QWordPrefix
/// (0x0E)` and 8 little-endian bytes. The firmware patches such placeholders
/// with real values before handing over the tables.
fn find_qword_name(aml: &[u8], name: &[u8; 4]) -> Option<u64> {
    let mut pattern = [0u8; 6];
    pattern[0] = 0x08;
    pattern[1..5].copy_from_slice(name);
    pattern[5] = 0x0e;
    let at = aml.windows(6).position(|w| w == pattern)?;
    u64_at(aml, at + 6)
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

    /// Set byte 9 (the header's checksum field) so the table sums to 0.
    fn fix_checksum(t: &mut [u8]) {
        t[9] = 0;
        t[9] = 0u8.wrapping_sub(t.iter().fold(0u8, |s, &b| s.wrapping_add(b)));
    }

    #[test]
    fn validates_checksums() {
        let mut t = table_with(b"APIC", 44);
        t[40] = 0x5a;
        assert!(!checksum_ok(&t));
        fix_checksum(&mut t);
        assert!(checksum_ok(&t));

        let mut rsdp = vec![0u8; 36];
        rsdp[..8].copy_from_slice(b"RSD PTR ");
        rsdp[8] = 0u8.wrapping_sub(rsdp[..20].iter().fold(0u8, |s, &b| s.wrapping_add(b)));
        assert!(rsdp_checksum_ok(&rsdp));
        rsdp[30] = 1; // only covered by the extended checksum
        assert!(!rsdp_checksum_ok(&rsdp));
        rsdp[32] = 0xff; // extended checksum field
        assert!(rsdp_checksum_ok(&rsdp));
        assert!(!rsdp_checksum_ok(&rsdp[..20]));
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

    /// GICC entry (ACPI 6.5: 82 bytes) with CPU interface and redistributor bases.
    fn gicc_entry(enabled: bool, gicc: u64, gicr: u64) -> Vec<u8> {
        let mut e = vec![0u8; 82];
        e[0] = MADT_GICC;
        e[1] = 82;
        e[GICC_FLAGS] = enabled as u8;
        e[GICC_PHYSICAL_BASE..][..8].copy_from_slice(&gicc.to_le_bytes());
        e[GICC_GICR_BASE..][..8].copy_from_slice(&gicr.to_le_bytes());
        e[68..76].copy_from_slice(&0x0123u64.to_le_bytes()); // MPIDR, must not be mistaken for GICR
        e
    }

    fn gicd_entry(base: u64, version: u8) -> Vec<u8> {
        let mut e = vec![0u8; 24];
        e[0] = MADT_GICD;
        e[1] = 24;
        e[GICD_BASE..][..8].copy_from_slice(&base.to_le_bytes());
        e[GICD_VERSION] = version;
        e
    }

    #[test]
    fn parses_gicv2_madt_like_raspberry_pi_5() {
        let mut t = table_with(b"APIC", 44);
        t.extend(gicc_entry(false, 0xdead, 0));
        t.extend(gicc_entry(true, 0x10_7fff_a000, 0));
        t.extend(gicd_entry(0x10_7fff_9000, 2));
        let mut p = Platform::default();
        parse_madt(&t, &mut p);
        assert_eq!((p.gicd, p.gicc, p.gicr, p.gic_version), (0x10_7fff_9000, 0x10_7fff_a000, 0, 2));
        assert_eq!(p.gic(), Ok(Gic::V2 { gicd: 0x10_7fff_9000, gicc: 0x10_7fff_a000 }));
    }

    #[test]
    fn gicv3_redistributor_from_gicc_entry() {
        let mut t = table_with(b"APIC", 44);
        t.extend(gicc_entry(true, 0, 0x080a_0000));
        t.extend(gicd_entry(0x0800_0000, 3));
        let mut p = Platform::default();
        parse_madt(&t, &mut p);
        assert_eq!(p.gic(), Ok(Gic::V3 { gicd: 0x0800_0000, gicr: 0x080a_0000 }));
    }

    #[test]
    fn incomplete_madt_has_no_gic() {
        let p = Platform { gicd: 0x1000, gic_version: 3, ..Default::default() };
        assert_eq!(p.gic(), Err(Error::NoGic));
        assert_eq!(Platform::default().gic(), Err(Error::NoGic));
    }

    #[test]
    fn parses_spcr_pl011() {
        let mut t = table_with(b"SPCR", 80);
        t[SPCR_INTERFACE_TYPE] = SPCR_PL011;
        t[SPCR_BASE_ADDRESS] = GAS_SYSTEM_MEMORY;
        t[SPCR_BASE_ADDRESS + 4..][..8].copy_from_slice(&0x10_7d00_1000u64.to_le_bytes());
        assert_eq!(parse_spcr(&t), Some(0x10_7d00_1000));
        t[SPCR_INTERFACE_TYPE] = 0x00; // 16550: not supported
        assert_eq!(parse_spcr(&t), None);
    }

    fn rpi5_dsdt(pbar: u64) -> Vec<u8> {
        let mut dsdt = table_with(b"DSDT", 36);
        dsdt[HEADER_OEM_ID..][..6].copy_from_slice(RPI5_OEM_ID);
        dsdt[HEADER_OEM_TABLE_ID..][..8].copy_from_slice(b"RPI5    ");
        dsdt.extend_from_slice(b"\x5b\x82\x10RP1B\x08_CCA\x00\x08PBAR\x0e");
        dsdt.extend_from_slice(&pbar.to_le_bytes());
        dsdt.extend_from_slice(b"\x70PBAR\x60"); // a later reference must not confuse the search
        dsdt
    }

    #[test]
    fn detects_raspberry_pi_5_and_rp1() {
        let mut p = Platform::default();
        parse_dsdt(&rpi5_dsdt(0x1f_0000_0000), &mut p);
        assert_eq!(p.board, Board::RaspberryPi5);
        assert_eq!(p.rp1_bar, Some(0x1f_0000_0000));
        assert_eq!(p.platform_xhci().collect::<Vec<_>>(), [0x1f_0020_0000, 0x1f_0030_0000]);
    }

    #[test]
    fn unpatched_rp1_bar_is_ignored() {
        let mut p = Platform::default();
        parse_dsdt(&rpi5_dsdt(ACPI_PATCH_QWORD_VALUE), &mut p);
        assert_eq!((p.board, p.rp1_bar), (Board::RaspberryPi5, None));
        assert_eq!(p.platform_xhci().count(), 0);
    }

    #[test]
    fn generic_dsdt_is_not_a_raspberry_pi() {
        let mut p = Platform::default();
        parse_dsdt(&table_with(b"DSDT", 36), &mut p);
        assert_eq!((p.board, p.rp1_bar), (Board::Generic, None));
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
