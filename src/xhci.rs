//! xHCI USB 3 host controller driver, just enough to find a USB keyboard.
//!
//! How the software talks to an xHCI controller:
//! - Registers (MMIO, through the PCI BAR) to reset/start it and ring
//!   "doorbells".
//! - Rings in RAM: circular lists of 16-byte TRBs (Transfer Request Blocks).
//!   We put commands on the *command ring* and transfers on per-endpoint
//!   *transfer rings*; the controller reports results on the *event ring*.
//! - Contexts in RAM: per-device structures describing the device and its
//!   endpoints.
//!
//! Each TRB has a "cycle" bit. The producer flips the bit it writes each time
//! the ring wraps around, so the consumer can tell new entries from old ones.
//!
//! Everything is polled; the controller's interrupts stay unused.
//!
//! The controller reads and writes RAM directly (DMA). On the Raspberry Pi 5
//! that DMA bypasses the CPU caches, so the driver *cleans* every structure it
//! hands to the controller and *invalidates* everything before reading what
//! the controller wrote (see `arch::dma_clean`). Each place is marked "DMA:".
//!
//! References: eXtensible Host Controller Interface for USB, revision 1.2
//! (xHCI spec); USB 2.0 specification, chapter 9; HID 1.11, appendix B.

use crate::arch;
use crate::interrupts::{counter, deadline, delay_ms, wait_until};
use crate::memory::alloc_page;
use crate::mmio;
use crate::serial::log;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;

// TRB types (bits 15:10 of the control word).
const TRB_NORMAL: u32 = 1;
const TRB_SETUP: u32 = 2;
const TRB_DATA: u32 = 3;
const TRB_STATUS: u32 = 4;
const TRB_LINK: u32 = 6;
const TRB_ENABLE_SLOT: u32 = 9;
const TRB_ADDRESS_DEVICE: u32 = 11;
const TRB_CONFIGURE_ENDPOINT: u32 = 12;
const TRB_EVALUATE_CONTEXT: u32 = 13;
const TRB_TRANSFER_EVENT: u32 = 32;
const TRB_COMMAND_COMPLETION: u32 = 33;

// TRB control flags.
const IOC: u32 = 1 << 5; // interrupt on completion: generate an event
const ISP: u32 = 1 << 2; // also generate an event on a short packet
const IDT: u32 = 1 << 6; // immediate data: the parameter holds the data itself

// Completion codes.
const SUCCESS: u8 = 1;
const SHORT_PACKET: u8 = 13;
const TIMEOUT: u8 = 0xff;

// Operational registers.
const USBCMD: u64 = 0x00;
const USBSTS: u64 = 0x04;
const CRCR: u64 = 0x18;
const DCBAAP: u64 = 0x30;
const CONFIG: u64 = 0x38;
const USBCMD_RUN: u32 = 1 << 0;
const USBCMD_RESET: u32 = 1 << 1;
const USBSTS_HALTED: u32 = 1 << 0;
const USBSTS_NOT_READY: u32 = 1 << 11;

// Port status and control (PORTSC) bits.
const PORT_CONNECTED: u32 = 1 << 0;
const PORT_ENABLED: u32 = 1 << 1;
const PORT_RESET: u32 = 1 << 4;
const PORT_POWER: u32 = 1 << 9;
const PORT_RESET_CHANGE: u32 = 1 << 21;
/// Bits that must be written back unchanged; everything else is either
/// "write 1 to clear" or would disable the port if we wrote 1.
const PORT_PRESERVE: u32 = 0x0e00_c3e0;
const PORT_CHANGE_BITS: u32 = 0x00fe_0000;

const RING_TRBS: usize = 256; // one 4 KiB page / 16 bytes

/// A Transfer Request Block as read from the event ring.
#[derive(Clone, Copy, Debug)]
struct Trb {
    param: u64,
    status: u32,
    control: u32,
}

impl Trb {
    fn kind(&self) -> u32 {
        (self.control >> 10) & 0x3f
    }
    fn code(&self) -> u8 {
        (self.status >> 24) as u8
    }
    fn slot(&self) -> u8 {
        (self.control >> 24) as u8
    }
    fn endpoint(&self) -> u8 {
        ((self.control >> 16) & 0x1f) as u8
    }
}

// Used for both registers and rings in RAM (plain loads/stores are fine there too).
use mmio::{read32, write32, write64};

// USB standard requests (USB 2.0, table 9-4) and descriptor types (table 9-5).
const GET_DESCRIPTOR: u8 = 6;
const SET_CONFIGURATION: u8 = 9;
const DESCRIPTOR_DEVICE: u16 = 1;
const DESCRIPTOR_CONFIGURATION: u16 = 2;
const DESCRIPTOR_STRING: u16 = 3;
const DESCRIPTOR_INTERFACE: u8 = 4;
const DESCRIPTOR_ENDPOINT: u8 = 5;
const LANGUAGE_EN_US: u16 = 0x0409;
// HID class requests (HID 1.11, section 7.2).
const HID_SET_IDLE: u8 = 0x0a;
const HID_SET_PROTOCOL: u8 = 0x0b;
const HID_BOOT_PROTOCOL: u16 = 0;
// bmRequestType values.
const REQUEST_DEVICE_TO_HOST: u8 = 0x80;
const REQUEST_HOST_TO_DEVICE: u8 = 0x00;
const REQUEST_CLASS_INTERFACE: u8 = 0x21;

/// A ring we produce TRBs on (command ring or transfer ring).
#[derive(Debug)]
struct Ring {
    base: u64,
    index: usize,
    cycle: u32,
}

impl Ring {
    fn new() -> Ring {
        Ring { base: alloc_page(), index: 0, cycle: 1 }
    }

    fn write(addr: u64, param: u64, status: u32, control: u32) {
        // SAFETY: `addr` is a TRB slot inside this ring's page (from `alloc_page`).
        unsafe {
            write64(addr, param);
            write32(addr + 8, status);
            // The controller may be reading the ring right now: make sure the
            // TRB contents reach memory before flipping its cycle bit.
            arch::dma_clean(addr, 16); // DMA: includes a barrier
            write32(addr + 12, control);
            arch::dma_clean(addr, 16); // DMA
        }
    }

    /// Append a TRB; returns its address (events refer to TRBs by address).
    fn push(&mut self, param: u64, status: u32, control: u32) -> u64 {
        let addr = self.base + self.index as u64 * 16;
        Ring::write(addr, param, status, control | self.cycle);
        self.index += 1;
        if self.index == RING_TRBS - 1 {
            // Last slot: a Link TRB that jumps back to the start and tells the
            // controller to flip its expected cycle bit (Toggle Cycle, bit 1).
            let link = self.base + self.index as u64 * 16;
            Ring::write(link, self.base, 0, (TRB_LINK << 10) | (1 << 1) | self.cycle);
            self.index = 0;
            self.cycle ^= 1;
        }
        addr
    }
}

/// The ring the controller produces events on and we consume.
#[derive(Debug)]
struct EventRing {
    base: u64,
    index: usize,
    cycle: u32,
    /// Event Ring Dequeue Pointer register: tells the controller how far we read.
    erdp: u64,
}

impl EventRing {
    fn pop(&mut self) -> Option<Trb> {
        let addr = self.base + self.index as u64 * 16;
        arch::dma_invalidate(addr, 16); // DMA: see what the controller wrote
        // SAFETY: `addr` is a TRB slot inside the event ring segment we allocated.
        let control = unsafe { read32(addr + 12) };
        if control & 1 != self.cycle {
            return None; // not written by the controller yet
        }
        arch::dsb();
        // SAFETY: as above; the cycle bit says the controller finished writing it.
        let trb = unsafe {
            Trb {
                param: (read32(addr) as u64) | ((read32(addr + 4) as u64) << 32),
                status: read32(addr + 8),
                control,
            }
        };
        self.index += 1;
        if self.index == RING_TRBS {
            self.index = 0;
            self.cycle ^= 1;
        }
        // Bit 3 (Event Handler Busy) is write-1-to-clear.
        // SAFETY: `erdp` is interrupter 0's ERDP register.
        unsafe { write64(self.erdp, (self.base + self.index as u64 * 16) | (1 << 3)) };
        Some(trb)
    }
}

/// A USB device found during [`Xhci::enumerate`].
#[derive(Debug, Clone)]
pub struct UsbDevice {
    /// Root hub port number (1-based).
    pub port: u8,
    /// Device slot assigned by the controller.
    pub slot: u8,
    /// Link speed, e.g. "high speed (480 Mbit/s)".
    pub speed: &'static str,
    /// USB vendor ID.
    pub vendor: u16,
    /// USB product ID.
    pub product: u16,
    /// Product name from the string descriptor (may be empty).
    pub name: String,
    /// Whether ROS uses this device as a keyboard.
    pub is_keyboard: bool,
}

/// Interrupt IN endpoint of the keyboard: it delivers an 8-byte report
/// (pressed keys) whenever something changes.
#[derive(Debug)]
struct KeyboardEndpoint {
    slot: u8,
    dci: u8, // device context index = endpoint number * 2 + 1 for IN
    ring: Ring,
    buffer: u64,
    packet_size: u32,
}

/// A running xHCI controller and the devices found on it.
#[derive(Debug)]
pub struct Xhci {
    op: u64,
    doorbells: u64,
    max_ports: u8,
    ctx_size: u64,
    dcbaa: u64,
    commands: Ring,
    events: EventRing,
    /// Every boot keyboard on this controller, each with its endpoint.
    keyboards: Vec<KeyboardEndpoint>,
    reports: VecDeque<[u8; 8]>,
    /// Interface version in BCD, e.g. 0x0110 for xHCI 1.1.
    pub version: u16,
    /// Devices found by [`Xhci::enumerate`].
    pub devices: Vec<UsbDevice>,
}

impl Xhci {
    /// Reset and start the controller whose registers are at `mmio`.
    ///
    /// # Safety
    /// `mmio` must be BAR 0 of an xHCI controller (with memory access and bus
    /// mastering enabled) that nothing else drives.
    pub unsafe fn new(mmio: u64) -> Result<Xhci, &'static str> {
        // SAFETY: the capability registers start at BAR 0 (caller guarantees).
        let (cap_len, version, hcs1, hcs2, hcc1, db_off, rt_off) = unsafe {
            let first = read32(mmio);
            (
                (first & 0xff) as u64,
                (first >> 16) as u16,
                read32(mmio + 0x04),
                read32(mmio + 0x08),
                read32(mmio + 0x10),
                read32(mmio + 0x14) & !3,
                read32(mmio + 0x18) & !0x1f,
            )
        };
        let op = mmio + cap_len;
        let max_slots = hcs1 & 0xff;
        let max_ports = (hcs1 >> 24) as u8;
        let scratchpads = (((hcs2 >> 21) & 0x1f) << 5) | ((hcs2 >> 27) & 0x1f);
        let ctx_size = if hcc1 & (1 << 2) != 0 { 64 } else { 32 };
        let runtime = mmio + rt_off as u64;

        // SAFETY: `op` and `runtime` are the controller's register blocks
        // (offsets read from its capability registers); the pages written
        // below are freshly allocated, identity mapped and owned by us.
        unsafe {
            // 1. Stop the controller (the firmware may have left it running).
            write32(op + USBCMD, read32(op + USBCMD) & !USBCMD_RUN);
            if !wait_until(100, || read32(op + USBSTS) & USBSTS_HALTED != 0) {
                return Err("controller did not halt");
            }
            // 2. Reset it to a clean state.
            write32(op + USBCMD, USBCMD_RESET);
            if !wait_until(1000, || {
                read32(op + USBCMD) & USBCMD_RESET == 0 && read32(op + USBSTS) & USBSTS_NOT_READY == 0
            }) {
                return Err("controller reset timed out");
            }

            // 3. Device slots, and the table pointing at each device's context.
            let slots = max_slots.min(32);
            write32(op + CONFIG, slots);
            let dcbaa = alloc_page();
            if scratchpads > 0 {
                // Scratch memory the controller needs for itself.
                let array = alloc_page();
                for i in 0..scratchpads as u64 {
                    *((array + i * 8) as *mut u64) = alloc_page();
                }
                *(dcbaa as *mut u64) = array;
                arch::dma_clean(array, scratchpads as usize * 8); // DMA
            }
            arch::dma_clean(dcbaa, 8); // DMA
            write64(op + DCBAAP, dcbaa);

            // 4. Command ring.
            let commands = Ring::new();
            write64(op + CRCR, commands.base | 1);

            // 5. Event ring for interrupter 0: one segment, described by a
            //    one-entry "segment table".
            let segment = alloc_page();
            let table = alloc_page();
            *(table as *mut u64) = segment;
            *((table + 8) as *mut u32) = RING_TRBS as u32;
            arch::dma_clean(table, 16); // DMA
            let interrupter = runtime + 0x20;
            write32(interrupter + 0x08, 1); // ERSTSZ: table size
            write64(interrupter + 0x18, segment); // ERDP: dequeue pointer
            write64(interrupter + 0x10, table); // ERSTBA: table address (write last)

            // 6. Run.
            arch::dsb();
            write32(op + USBCMD, read32(op + USBCMD) | USBCMD_RUN);
            if !wait_until(100, || read32(op + USBSTS) & USBSTS_HALTED == 0) {
                return Err("controller did not start");
            }

            Ok(Xhci {
                op,
                doorbells: mmio + db_off as u64,
                max_ports,
                ctx_size,
                dcbaa,
                commands,
                events: EventRing { base: segment, index: 0, cycle: 1, erdp: interrupter + 0x18 },
                keyboards: Vec::new(),
                reports: VecDeque::new(),
                version,
                devices: Vec::new(),
            })
        }
    }

    fn ring_doorbell(&self, slot: u8, target: u8) {
        arch::dsb();
        // SAFETY: doorbell `slot` lies inside the doorbell array (slot <= max slots).
        unsafe { write32(self.doorbells + slot as u64 * 4, target as u32) };
    }

    fn portsc(&self, port: u8) -> u64 {
        self.op + 0x400 + (port as u64 - 1) * 0x10
    }

    /// Pointer to dword 0 of context `index` inside a context block.
    fn ctx(&self, base: u64, index: u64) -> *mut u32 {
        (base + index * self.ctx_size) as *mut u32
    }

    /// Next event, handing keyboard reports to the keyboard queue on the way.
    fn next_event(&mut self, timeout_ms: u64) -> Result<Trb, u8> {
        let end = deadline(timeout_ms);
        loop {
            if let Some(event) = self.events.pop() {
                if !self.handle_keyboard_event(&event) {
                    return Ok(event);
                }
            } else if counter() > end {
                return Err(TIMEOUT);
            }
        }
    }

    /// Run a command; `param` is 0 or the address of an input context page.
    fn command(&mut self, param: u64, control: u32) -> Result<Trb, u8> {
        if param != 0 {
            arch::dma_clean(param, 4096); // DMA: the input context we filled in
        }
        let addr = self.commands.push(param, 0, control);
        self.ring_doorbell(0, 0); // doorbell 0 = the command ring
        loop {
            let event = self.next_event(1000)?;
            if event.kind() == TRB_COMMAND_COMPLETION && event.param == addr {
                return if event.code() == SUCCESS { Ok(event) } else { Err(event.code()) };
            }
        }
    }

    /// A control transfer on endpoint 0: setup stage, optional data stage, status stage.
    #[allow(clippy::too_many_arguments)]
    fn control(
        &mut self,
        slot: u8,
        ring: &mut Ring,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        buffer: u64,
        len: u16,
    ) -> Result<(), u8> {
        let setup = request_type as u64
            | (request as u64) << 8
            | (value as u64) << 16
            | (index as u64) << 32
            | (len as u64) << 48;
        let device_to_host = request_type & 0x80 != 0;
        let transfer_type = match (len, device_to_host) {
            (0, _) => 0,
            (_, false) => 2,
            (_, true) => 3,
        };
        ring.push(setup, 8, (TRB_SETUP << 10) | IDT | (transfer_type << 16));
        if len > 0 {
            ring.push(buffer, len as u32, (TRB_DATA << 10) | ((device_to_host as u32) << 16));
        }
        let status_in = len == 0 || !device_to_host;
        let status = ring.push(0, 0, (TRB_STATUS << 10) | IOC | ((status_in as u32) << 16));
        self.ring_doorbell(slot, 1);

        loop {
            let event = self.next_event(1000)?;
            if event.kind() != TRB_TRANSFER_EVENT || event.slot() != slot {
                continue;
            }
            match event.code() {
                SUCCESS if event.param == status => return Ok(()),
                SUCCESS | SHORT_PACKET => continue,
                code => return Err(code),
            }
        }
    }

    /// GET_DESCRIPTOR into the DMA page `buffer`; returns a copy of the bytes.
    fn get_descriptor(
        &mut self,
        slot: u8,
        ring: &mut Ring,
        kind: u16,
        index: u16,
        buffer: u64,
        len: u16,
    ) -> Result<Vec<u8>, u8> {
        let language = if kind == DESCRIPTOR_STRING { LANGUAGE_EN_US } else { 0 };
        self.control(
            slot,
            ring,
            REQUEST_DEVICE_TO_HOST,
            GET_DESCRIPTOR,
            (kind << 8) | index,
            language,
            buffer,
            len,
        )?;
        arch::dma_invalidate(buffer, len as usize); // DMA: see what the device sent
        // SAFETY: `buffer` is a 4 KiB page we own (len <= 4096), and the
        // controller has finished writing to it (status stage completed).
        Ok(unsafe { core::slice::from_raw_parts(buffer as *const u8, len as usize) }.to_vec())
    }

    /// Reset every port with something plugged in and set the device up.
    pub fn enumerate(&mut self) {
        // Ports start unpowered after a reset if the controller supports port
        // power control (the Raspberry Pi 5's does). Power them all first.
        let mut powered_up = false;
        for port in 1..=self.max_ports {
            let reg = self.portsc(port);
            // SAFETY: PORTSC of an existing port; we only add the power bit.
            unsafe {
                let status = read32(reg);
                if status & PORT_POWER == 0 {
                    write32(reg, (status & PORT_PRESERVE) | PORT_POWER);
                    powered_up = true;
                }
            }
        }
        // Give devices time to power up and report their connection.
        delay_ms(if powered_up { 300 } else { 50 });
        for port in 1..=self.max_ports {
            // SAFETY: PORTSC of an existing port (1..=max_ports).
            let status = unsafe { read32(self.portsc(port)) };
            if status & PORT_CONNECTED == 0 {
                continue;
            }
            if let Err(err) = self.setup_port(port) {
                log!("usb: port {port}: setup failed: {err}");
            }
        }
    }

    /// Port reset: required for USB 2 ports before the device can be talked to.
    fn reset_port(&mut self, port: u8) -> Result<u8, &'static str> {
        let reg = self.portsc(port);
        // SAFETY: PORTSC of an existing port; we preserve the bits that must
        // not change and only clear change bits / start a reset.
        unsafe {
            let status = read32(reg);
            if status & PORT_ENABLED == 0 {
                write32(reg, (status & PORT_PRESERVE) | PORT_RESET);
                if !wait_until(500, || read32(reg) & PORT_RESET_CHANGE != 0) {
                    return Err("port reset timed out");
                }
            }
            let status = read32(reg);
            write32(reg, (status & PORT_PRESERVE) | (status & PORT_CHANGE_BITS));
            if status & PORT_ENABLED == 0 {
                return Err("port not enabled after reset");
            }
            delay_ms(10); // reset recovery time
            Ok(((status >> 10) & 0xf) as u8)
        }
    }

    fn setup_port(&mut self, port: u8) -> Result<(), &'static str> {
        let speed = self.reset_port(port)?;
        let (speed_name, mut packet_size) = speed_info(speed);

        // Ask the controller for a device slot.
        let slot = self.command(0, TRB_ENABLE_SLOT << 10).map_err(|_| "enable slot failed")?.slot();

        // Output context: written by the controller. Input context: our requests.
        let output = alloc_page();
        let input = alloc_page();
        let entry = self.dcbaa + slot as u64 * 8;
        // SAFETY: `dcbaa` is our page with one u64 entry per slot (slot < 256).
        unsafe { *(entry as *mut u64) = output };
        arch::dma_clean(entry, 8); // DMA
        let mut ep0 = Ring::new();

        // Address Device: the controller assigns a USB address and sets up endpoint 0.
        // SAFETY: `input` is a freshly allocated page holding the input context.
        unsafe {
            *self.ctx(input, 0).add(1) = 0b11; // add slot + endpoint 0 contexts
            let slot_ctx = self.ctx(input, 1);
            *slot_ctx = ((speed as u32) << 20) | (1 << 27);
            *slot_ctx.add(1) = (port as u32) << 16;
            self.write_ep0(input, &ep0, packet_size);
        }
        self.command(input, (TRB_ADDRESS_DEVICE << 10) | ((slot as u32) << 24))
            .map_err(|_| "address device failed")?;

        // Device descriptor. The first 8 bytes tell us endpoint 0's real packet size.
        let buf = alloc_page();
        let desc = self
            .get_descriptor(slot, &mut ep0, DESCRIPTOR_DEVICE, 0, buf, 8)
            .map_err(|_| "no device descriptor")?;
        let real = if speed >= 4 { 1u32 << desc[7] } else { desc[7] as u32 };
        if real != packet_size && real != 0 {
            packet_size = real;
            // SAFETY: `input` is our input context page.
            unsafe {
                core::ptr::write_bytes(input as *mut u8, 0, 4096);
                *self.ctx(input, 0).add(1) = 0b10; // evaluate endpoint 0 only
                self.write_ep0(input, &ep0, packet_size);
            }
            self.command(input, (TRB_EVALUATE_CONTEXT << 10) | ((slot as u32) << 24))
                .map_err(|_| "evaluate context failed")?;
        }
        let desc = self
            .get_descriptor(slot, &mut ep0, DESCRIPTOR_DEVICE, 0, buf, 18)
            .map_err(|_| "no device descriptor")?;
        let vendor = u16::from_le_bytes([desc[8], desc[9]]);
        let product = u16::from_le_bytes([desc[10], desc[11]]);
        let product_string = desc[15];

        // Configuration descriptor: first the header for the total length, then all of it.
        let head = self
            .get_descriptor(slot, &mut ep0, DESCRIPTOR_CONFIGURATION, 0, buf, 9)
            .map_err(|_| "no config descriptor")?;
        let total = u16::from_le_bytes([head[2], head[3]]).min(4096);
        let config_value = head[5];
        let config = self
            .get_descriptor(slot, &mut ep0, DESCRIPTOR_CONFIGURATION, 0, buf, total)
            .map_err(|_| "no config descriptor")?;
        let keyboard = find_boot_keyboard(&config);

        // Activate the configuration.
        self.control(slot, &mut ep0, REQUEST_HOST_TO_DEVICE, SET_CONFIGURATION, config_value as u16, 0, 0, 0)
            .map_err(|_| "set configuration failed")?;

        if let Some(kb) = keyboard {
            // HID class requests: use the simple "boot" report format and only
            // send reports when something changes (idle rate 0).
            let interface = kb.interface as u16;
            let _ = self.control(
                slot,
                &mut ep0,
                REQUEST_CLASS_INTERFACE,
                HID_SET_PROTOCOL,
                HID_BOOT_PROTOCOL,
                interface,
                0,
                0,
            );
            let _ = self.control(slot, &mut ep0, REQUEST_CLASS_INTERFACE, HID_SET_IDLE, 0, interface, 0, 0);
            self.start_keyboard(slot, port, speed, &kb)?;
        }

        // The product name comes last: some devices reject (stall) string
        // requests, which halts endpoint 0, and that must not stop the
        // keyboard from working.
        let name = if product_string != 0 {
            self.get_descriptor(slot, &mut ep0, DESCRIPTOR_STRING, product_string as u16, buf, 255)
                .map(|d| decode_string(&d))
                .unwrap_or_default()
        } else {
            String::new()
        };

        log!("usb: port {port} slot {slot}: {vendor:04x}:{product:04x} \"{name}\" {speed_name}");
        self.devices.push(UsbDevice {
            port,
            slot,
            speed: speed_name,
            vendor,
            product,
            name,
            is_keyboard: keyboard.is_some(),
        });
        Ok(())
    }

    /// Fill in endpoint 0's context inside the input context at `input`.
    ///
    /// # Safety
    /// `input` must be an input context page owned by us.
    unsafe fn write_ep0(&self, input: u64, ring: &Ring, packet_size: u32) {
        let ep = self.ctx(input, 2);
        // SAFETY: context 2 lies inside the 4 KiB input context page (caller guarantees).
        unsafe {
            *ep.add(1) = (3 << 1) | (4 << 3) | (packet_size << 16); // 3 retries, type control
            *ep.add(2) = ring.base as u32 | 1; // dequeue pointer + cycle state
            *ep.add(3) = (ring.base >> 32) as u32;
            *ep.add(4) = 8; // average TRB length
        }
    }

    fn start_keyboard(
        &mut self,
        slot: u8,
        port: u8,
        speed: u8,
        kb: &BootKeyboard,
    ) -> Result<(), &'static str> {
        let dci = (kb.endpoint & 0xf) * 2 + 1;
        let ring = Ring::new();
        let interval = interrupt_interval(speed, kb.interval);
        let input = alloc_page();
        // SAFETY: `input` is a freshly allocated page holding the input context;
        // `dci + 1 <= 32` contexts fit into it.
        unsafe {
            *self.ctx(input, 0).add(1) = 1 | (1 << dci);
            let slot_ctx = self.ctx(input, 1);
            *slot_ctx = ((speed as u32) << 20) | ((dci as u32) << 27);
            *slot_ctx.add(1) = (port as u32) << 16;
            let ep = self.ctx(input, dci as u64 + 1);
            *ep = interval << 16;
            *ep.add(1) = (3 << 1) | (7 << 3) | ((kb.packet_size as u32) << 16); // type interrupt IN
            *ep.add(2) = ring.base as u32 | 1;
            *ep.add(3) = (ring.base >> 32) as u32;
            *ep.add(4) = 8 | ((kb.packet_size as u32) << 16);
        }
        self.command(input, (TRB_CONFIGURE_ENDPOINT << 10) | ((slot as u32) << 24))
            .map_err(|_| "configure endpoint failed")?;

        self.keyboards.push(KeyboardEndpoint {
            slot,
            dci,
            ring,
            buffer: alloc_page(),
            packet_size: kb.packet_size.clamp(8, 64) as u32,
        });
        self.queue_keyboard_transfer(self.keyboards.len() - 1);
        Ok(())
    }

    /// Ask keyboard number `index` for its next report.
    fn queue_keyboard_transfer(&mut self, index: usize) {
        let kb = &mut self.keyboards[index];
        kb.ring.push(kb.buffer, kb.packet_size, (TRB_NORMAL << 10) | IOC | ISP);
        let (slot, dci) = (kb.slot, kb.dci);
        self.ring_doorbell(slot, dci);
    }

    /// If `event` is a keyboard report, queue it and return true.
    fn handle_keyboard_event(&mut self, event: &Trb) -> bool {
        if event.kind() != TRB_TRANSFER_EVENT {
            return false;
        }
        let Some(index) =
            self.keyboards.iter().position(|kb| kb.slot == event.slot() && kb.dci == event.endpoint())
        else {
            return false;
        };
        let kb = &self.keyboards[index];
        if matches!(event.code(), SUCCESS | SHORT_PACKET) {
            let mut report = [0u8; 8];
            arch::dma_invalidate(kb.buffer, 8); // DMA: see the new report
            // SAFETY: `buffer` is the keyboard's DMA page, and the transfer is complete.
            unsafe { core::ptr::copy_nonoverlapping(kb.buffer as *const u8, report.as_mut_ptr(), 8) };
            self.reports.push_back(report);
        } else {
            log!("usb: keyboard transfer error {}", event.code());
        }
        self.queue_keyboard_transfer(index);
        true
    }

    /// Collect finished transfers; returns the next keyboard report, if any.
    pub fn poll(&mut self) -> Option<[u8; 8]> {
        while let Some(event) = self.events.pop() {
            self.handle_keyboard_event(&event);
        }
        self.reports.pop_front()
    }

    /// Whether a boot keyboard was found and its endpoint is running.
    pub fn has_keyboard(&self) -> bool {
        !self.keyboards.is_empty()
    }
}

/// Port speed ID (PORTSC bits 13:10) -> name and default endpoint 0 packet size.
fn speed_info(speed: u8) -> (&'static str, u32) {
    match speed {
        1 => ("full speed (12 Mbit/s)", 8),
        2 => ("low speed (1.5 Mbit/s)", 8),
        3 => ("high speed (480 Mbit/s)", 64),
        4 => ("super speed (5 Gbit/s)", 512),
        _ => ("super speed plus (10 Gbit/s)", 512),
    }
}

/// Endpoint context interval: polling period as 2^n * 125 µs.
///
/// Full/low speed devices give `bInterval` in milliseconds; high/super speed
/// devices give the exponent plus one (xHCI spec, section 6.2.3.6).
fn interrupt_interval(speed: u8, b_interval: u8) -> u32 {
    let b_interval = b_interval.max(1) as u32;
    if speed <= 2 { (b_interval * 8).ilog2().clamp(3, 10) } else { (b_interval - 1).min(15) }
}

/// The parts of a HID boot keyboard's descriptors we need.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BootKeyboard {
    interface: u8,
    endpoint: u8,
    packet_size: u16,
    interval: u8,
}

/// Walk the configuration descriptor (a list of [length, type, ...] records)
/// looking for a HID boot keyboard interface and its interrupt IN endpoint.
fn find_boot_keyboard(config: &[u8]) -> Option<BootKeyboard> {
    let mut i = 0;
    let mut in_keyboard = None;
    while i + 2 <= config.len() {
        let len = config[i] as usize;
        if len < 2 || i + len > config.len() {
            break;
        }
        let d = &config[i..i + len];
        match d[1] {
            DESCRIPTOR_INTERFACE if len >= 9 => {
                // Interface: class 3 (HID), subclass 1 (boot), protocol 1 (keyboard).
                in_keyboard = (d[5] == 3 && d[6] == 1 && d[7] == 1).then_some(d[2]);
            }
            DESCRIPTOR_ENDPOINT if len >= 7 => {
                let is_interrupt_in = d[3] & 3 == 3 && d[2] & 0x80 != 0;
                if let (Some(interface), true) = (in_keyboard, is_interrupt_in) {
                    return Some(BootKeyboard {
                        interface,
                        endpoint: d[2],
                        packet_size: u16::from_le_bytes([d[4], d[5]]) & 0x7ff,
                        interval: d[6],
                    });
                }
            }
            _ => {}
        }
        i += len;
    }
    None
}

/// Decode a string descriptor: `[length, type=3, utf16le...]`.
/// Malformed descriptors give an empty or partial string, never a panic.
fn decode_string(desc: &[u8]) -> String {
    let len = desc.first().map_or(0, |&l| l as usize).min(desc.len());
    let units =
        desc.get(2..len).unwrap_or_default().as_chunks::<2>().0.iter().map(|&c| u16::from_le_bytes(c));
    char::decode_utf16(units).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Configuration descriptor of a typical USB keyboard (QEMU's usb-kbd).
    const KEYBOARD_CONFIG: [u8; 34] = [
        9, 2, 34, 0, 1, 1, 0, 0xa0, 50, // configuration
        9, 4, 0, 0, 1, 3, 1, 1, 0, // interface 0: HID, boot, keyboard
        9, 0x21, 0x11, 0x01, 0, 1, 0x22, 63, 0, // HID descriptor
        7, 5, 0x81, 3, 8, 0, 7, // endpoint 1 IN, interrupt, 8 bytes, 7 ms
    ];

    #[test]
    fn finds_boot_keyboard_endpoint() {
        let kb = find_boot_keyboard(&KEYBOARD_CONFIG).unwrap();
        assert_eq!(kb, BootKeyboard { interface: 0, endpoint: 0x81, packet_size: 8, interval: 7 });
    }

    #[test]
    fn ignores_non_keyboard_interfaces() {
        let mut mouse = KEYBOARD_CONFIG;
        mouse[16] = 2; // protocol 2 = mouse
        assert_eq!(find_boot_keyboard(&mouse), None);
        assert_eq!(find_boot_keyboard(&KEYBOARD_CONFIG[..20]), None); // truncated
        assert_eq!(find_boot_keyboard(&[0, 0, 0]), None); // zero length
    }

    #[test]
    fn decodes_string_descriptors() {
        let desc = [8, 3, b'R', 0, b'O', 0, b'S', 0];
        assert_eq!(decode_string(&desc), "ROS");
        assert_eq!(decode_string(&[200, 3, b'A', 0]), "A"); // length too large
        assert_eq!(decode_string(&[1]), "");
        assert_eq!(decode_string(&[]), "");
    }

    #[test]
    fn computes_interrupt_interval() {
        assert_eq!(interrupt_interval(1, 10), 6); // 10 ms -> 2^6 * 125 us = 8 ms
        assert_eq!(interrupt_interval(2, 0), 3);
        assert_eq!(interrupt_interval(1, 255), 10);
        assert_eq!(interrupt_interval(3, 4), 3); // 2^(4-1) * 125 us = 1 ms
        assert_eq!(interrupt_interval(4, 20), 15);
    }

    #[test]
    fn decodes_trb_fields() {
        let trb = Trb {
            param: 0,
            status: (SUCCESS as u32) << 24,
            control: (3 << 24) | (5 << 16) | (TRB_TRANSFER_EVENT << 10),
        };
        assert_eq!((trb.kind(), trb.code(), trb.slot(), trb.endpoint()), (TRB_TRANSFER_EVENT, SUCCESS, 3, 5));
        assert_eq!(speed_info(3), ("high speed (480 Mbit/s)", 64));
        assert_eq!(speed_info(5), ("super speed plus (10 Gbit/s)", 512));
    }
}
