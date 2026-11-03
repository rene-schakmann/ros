//! ROS - Rene's tiny OS.
//!
//! Boot happens in two stages:
//! 1. [`efi_main`] runs as a UEFI application. It borrows the firmware to
//!    reserve memory, set up the screen, find the ACPI tables, and then calls
//!    `ExitBootServices`: from that point on the firmware is gone.
//! 2. [`kernel_main`] runs on its own stack and drives the hardware itself:
//!    exception vectors, interrupt controller, timer, PCI, USB keyboard.
//!
//! Unit tests (`cargo test-host`) build the same modules as a normal Linux
//! program on an AArch64 host; the boot code below is left out there.

#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]
// Kernel code that only `kernel_main` reaches looks unused in unit tests.
#![cfg_attr(test, allow(dead_code, unused_imports))]

extern crate alloc;

mod acpi;
mod arch;
mod console;
mod exceptions;
mod interrupts;
mod keyboard;
mod memory;
mod mmio;
mod pci;
mod serial;
mod shell;
mod xhci;

use alloc::vec::Vec;
use console::{FrameBufferInfo, color, cprint};
use serial::log;
use spin::Once;

/// Kernel heap for `alloc` (Vec, String, Box).
const HEAP_SIZE: usize = 32 * 1024 * 1024;
/// Kernel stack. The firmware's stack lives in boot-services memory.
const STACK_SIZE: usize = 1024 * 1024;
/// Largest screen mode we pick (keeps the VM window a reasonable size).
const MAX_RESOLUTION: (usize, usize) = (1280, 800);

/// Everything the boot stage learned from the firmware.
#[derive(Debug)]
struct BootInfo {
    /// `None` when the firmware offers no usable graphics mode (serial only).
    framebuffer: Option<FrameBufferInfo>,
    /// Physical address of the ACPI 2.0 RSDP.
    rsdp: u64,
    /// Final memory map, taken at ExitBootServices.
    memory_map: Vec<memory::Region>,
}

static BOOT_INFO: Once<BootInfo> = Once::new();

/// Stage 1: the UEFI application entry point.
#[cfg(not(test))]
#[uefi::entry]
fn efi_main() -> uefi::Status {
    use uefi::boot;
    use uefi::mem::memory_map::MemoryMap;
    use uefi::table::cfg::ConfigTableEntry;

    // Firmware resets the machine after 5 minutes unless the watchdog is disabled.
    let _ = boot::set_watchdog_timer(0, 0x10000, None);

    // Reserve heap and kernel stack while the firmware can still hand out memory.
    // LOADER_DATA memory stays ours after ExitBootServices.
    let heap = allocate(HEAP_SIZE);
    // SAFETY: `heap` is fresh LOADER_DATA memory that nobody else uses.
    unsafe { memory::init_heap(heap, HEAP_SIZE) };
    let stack_top = allocate(STACK_SIZE) as u64 + STACK_SIZE as u64;

    let framebuffer = init_graphics();
    let mut rsdp = 0;
    uefi::system::with_config_table(|tables| {
        if let Some(t) = tables.iter().find(|t| t.guid == ConfigTableEntry::ACPI2_GUID) {
            rsdp = t.address as u64;
        }
    });
    assert!(rsdp != 0, "no ACPI 2.0 tables: ROS needs ACPI to find its hardware");

    // Point of no return: the firmware stops all its drivers and hands over the machine.
    // SAFETY: no boot-services objects (protocols, events) are used after this point.
    let map = unsafe { boot::exit_boot_services(None) };
    let memory_map = map
        .entries()
        .map(|d| memory::Region { kind: d.ty.0, start: d.phys_start, pages: d.page_count })
        .collect();
    BOOT_INFO.call_once(|| BootInfo { framebuffer, rsdp, memory_map });

    // Switch to our own stack (using the EL1 stack pointer) and enter the kernel.
    // SAFETY: `stack_top` is the 16-byte aligned end of our reserved stack, and
    // `kernel_main` never returns, so nothing uses the old stack again.
    unsafe {
        core::arch::asm!(
            "msr spsel, #1",
            "mov sp, {stack}",
            "b {kernel}",
            stack = in(reg) stack_top,
            kernel = sym kernel_main,
            options(noreturn),
        )
    }
}

/// Allocate `bytes` (rounded up to whole pages) from the firmware.
#[cfg(not(test))]
fn allocate(bytes: usize) -> *mut u8 {
    use uefi::boot::{self, AllocateType, MemoryType};
    let pages = bytes.div_ceil(memory::PAGE_SIZE as usize);
    boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, pages)
        .expect("out of memory during boot")
        .as_ptr()
}

/// Pick the largest screen mode up to [`MAX_RESOLUTION`] and return where
/// its framebuffer is. `None` if there is no graphics output.
#[cfg(not(test))]
fn init_graphics() -> Option<FrameBufferInfo> {
    use uefi::boot;
    use uefi::proto::console::gop::{GraphicsOutput, PixelFormat};

    let handle = boot::get_handle_for_protocol::<GraphicsOutput>().ok()?;
    let mut gop = boot::open_protocol_exclusive::<GraphicsOutput>(handle).ok()?;
    let usable = |format: PixelFormat| matches!(format, PixelFormat::Rgb | PixelFormat::Bgr);

    let best = gop
        .modes()
        .filter(|m| {
            let (w, h) = m.info().resolution();
            w <= MAX_RESOLUTION.0 && h <= MAX_RESOLUTION.1 && usable(m.info().pixel_format())
        })
        .max_by_key(|m| m.info().resolution().0 * m.info().resolution().1);
    if let Some(mode) = best {
        let _ = gop.set_mode(&mode);
    }

    let info = gop.current_mode_info();
    if !usable(info.pixel_format()) {
        return None;
    }
    let (width, height) = info.resolution();
    Some(FrameBufferInfo {
        base: gop.frame_buffer().as_mut_ptr() as u64,
        width,
        height,
        stride: info.stride(),
        bgr: info.pixel_format() == PixelFormat::Bgr,
    })
}

/// Print a green `[ ok ]` boot message.
fn ok(what: core::fmt::Arguments) {
    status(color::LIGHT_GREEN, " ok ", what);
}

/// Print a red `[fail]` boot message.
fn fail(what: core::fmt::Arguments) {
    status(color::LIGHT_RED, "fail", what);
}

fn status(rgb: u32, label: &str, what: core::fmt::Arguments) {
    cprint!(color::GRAY, "[");
    cprint!(rgb, "{label}");
    cprint!(color::GRAY, "] ");
    console::_print(format_args!("{what}\n"));
}

/// Stage 2: the kernel, running on its own stack after ExitBootServices.
extern "C" fn kernel_main() -> ! {
    let boot = BOOT_INFO.get().expect("boot info missing");

    exceptions::init();
    // The screen first, so that even an ACPI error below is visible.
    if let Some(fb) = boot.framebuffer {
        // SAFETY: the framebuffer from GOP stays valid after ExitBootServices.
        unsafe { console::init(fb) };
    }
    // SAFETY: `rsdp` came from the firmware's configuration table, and the
    // ACPI tables stay mapped (identity mapping, reserved memory).
    let platform = match unsafe { acpi::parse(boot.rsdp) } {
        Ok(platform) => platform,
        Err(err) => panic!("ACPI: {err}"),
    };
    if let Some(uart) = platform.uart {
        // SAFETY: ACPI says a PL011 lives at `uart`.
        unsafe { serial::init(uart) };
    }
    log!("\nROS kernel started, firmware exited");

    memory::init_frames(boot.memory_map.clone());
    cprint!(color::WHITE, "ROS kernel booting on a {}\n\n", platform.board.name());
    ok(format_args!("left UEFI boot services, running at EL{} on our own stack", arch::current_el()));
    match &boot.framebuffer {
        Some(fb) => ok(format_args!("framebuffer {}x{} at {:#x}", fb.width, fb.height, fb.base)),
        None => fail(format_args!("no framebuffer (serial console only)")),
    }
    ok(format_args!("exception vectors installed"));
    let free = memory::stats().free;
    ok(format_args!("memory: {} MiB free, {} MiB heap", free >> 20, HEAP_SIZE >> 20));

    let gic = platform.gic().unwrap_or_else(|err| panic!("ACPI: {err}"));
    // SAFETY: GIC addresses from ACPI, vectors installed, single call on the boot CPU.
    unsafe { interrupts::init(gic, platform.timer_irq) };
    arch::enable_irqs();
    let version = if matches!(gic, interrupts::Gic::V2 { .. }) { 2 } else { 3 };
    ok(format_args!("GICv{version} interrupt controller, timer at {} Hz", interrupts::TICK_HZ));

    let devices = if platform.board == acpi::Board::RaspberryPi5 {
        // The Broadcom PCIe controllers only expose one device through a
        // config window; probing other slots can hang the bus. The only
        // device we need, the RP1 USB, is found through ACPI instead.
        ok(format_args!("PCI: skipped (not ECAM compliant on this board)"));
        Vec::new()
    } else {
        // SAFETY: the ECAM window and bus range come from the ACPI MCFG table.
        let devices = unsafe { pci::scan(platform.pci_ecam, platform.pci_bus_start, platform.pci_bus_end) };
        ok(format_args!("PCI: {} devices", devices.len()));
        devices
    };

    init_usb(&devices, &platform);

    shell::Shell::new(platform, devices).run()
}

/// Start every xHCI controller (PCI functions and, on the Raspberry Pi 5,
/// the RP1's built-in ones) and look for a keyboard.
fn init_usb(devices: &[pci::Device], platform: &acpi::Platform) {
    let pci = devices.iter().filter(|d| d.is(pci::CLASS_XHCI)).map(|d| {
        d.enable();
        d.bar(0)
    });
    let bases: Vec<u64> = pci.chain(platform.platform_xhci()).collect();
    if bases.is_empty() {
        return fail(format_args!("no USB xHCI controller"));
    }

    let mut controllers = Vec::new();
    for base in bases {
        // SAFETY: `base` is BAR 0 of an enabled xHCI PCI function, or an xHCI
        // controller the firmware described in ACPI; no other driver touches it.
        match unsafe { xhci::Xhci::new(base) } {
            Ok(mut usb) => {
                usb.enumerate();
                controllers.push(usb);
            }
            Err(e) => fail(format_args!("USB controller at {base:#x}: {e}")),
        }
    }
    let count: usize = controllers.iter().map(|c| c.devices.len()).sum();
    let keyboard = controllers.iter().any(xhci::Xhci::has_keyboard);
    if !controllers.is_empty() {
        ok(format_args!("USB: {count} devices on {} controller(s)", controllers.len()));
    }
    *keyboard::USB.lock() = controllers;
    if keyboard {
        ok(format_args!("USB keyboard ready"));
    } else {
        fail(format_args!("no USB keyboard (serial input still works)"));
    }
}

/// Rust panics (failed `expect`, out of memory, ...) end up here.
#[cfg(not(test))]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    arch::disable_irqs();
    // SAFETY: the panicking code might hold the console lock, but it will
    // never continue, so taking the lock over is fine.
    unsafe { console::CONSOLE.force_unlock() };
    cprint!(color::LIGHT_RED, "\n*** KERNEL PANIC ***\n{info}\n");
    arch::halt()
}
