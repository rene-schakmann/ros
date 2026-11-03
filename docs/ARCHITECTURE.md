# ROS architecture

This document explains how ROS goes from power-on to a shell prompt, and how
each subsystem works. Every source file also starts with a module comment that
describes it, and `make check` builds the API docs
(`target/aarch64-unknown-uefi/doc/ros/index.html`).

## Design goals

- **Readable first.** Code is written to be read by someone learning how an OS
  talks to hardware. Comments explain *why*, constants have names, and each
  driver links the specification it follows.
- **Real hardware interfaces, no shortcuts.** After `ExitBootServices` the
  firmware is gone. ROS finds devices through ACPI and PCI and drives them
  through their registers, as on a physical machine.
- **Small.** Single core, no scheduler, identity-mapped memory, polling
  wherever interrupts aren't needed.

## Boot: two stages

```text
 UEFI firmware ──loads──> BOOTAA64.EFI
                               │
   stage 1: efi_main           │  runs as a UEFI application
   - disable the watchdog      │
   - allocate heap + stack     │  (LOADER_DATA: stays ours afterwards)
   - choose a screen mode      │  (Graphics Output Protocol)
   - find the ACPI RSDP        │  (UEFI configuration table)
   - ExitBootServices ─────────┘  firmware drivers stop here
                               │
   stage 2: kernel_main        │  on our own stack (SP_EL1)
   - exception vectors         │  src/exceptions.rs
   - ACPI tables               │  src/acpi.rs
   - serial log, console       │  src/serial.rs, src/console.rs
   - page allocator            │  src/memory.rs
   - GICv3 + timer, IRQs on    │  src/interrupts.rs
   - PCI scan                  │  src/pci.rs
   - xHCI + USB keyboard       │  src/xhci.rs, src/keyboard.rs
   - shell                     ▼  src/shell.rs (never returns)
```

The kernel runs at EL1 (kernel mode). It keeps the firmware's page tables,
which map all memory 1:1, so a physical address can be used as a pointer.

## Memory

- **Heap** (`memory::init_heap`): 32 MiB reserved from the firmware during
  boot, managed by `linked_list_allocator`. It backs `Vec`, `String`, `Box`.
- **Frames** (`memory::alloc_pages`): a bump allocator over the CONVENTIONAL
  regions of the final memory map. Drivers use it for DMA buffers (xHCI rings,
  contexts, descriptors). Pages are never freed; driver buffers live as long
  as the OS.
- Boot-services memory is left alone: the firmware's page tables live there.

## Exceptions and interrupts

`exceptions.rs` defines the 16-entry vector table in assembly. Each entry saves
all general purpose and SIMD registers into a `Frame` (layout checked at compile
time) and calls `handle_exception` in Rust:

- **IRQ** → `interrupts::handle_irq`: acknowledge (`ICC_IAR1_EL1`), handle the
  timer tick, signal end of interrupt (`ICC_EOIR1_EL1`).
- **Breakpoint** (`brk`) → print a message, skip the instruction, resume.
- **Anything else** → panic screen with the registers, then halt.

The GICv3 is set up with affinity routing; only the virtual timer interrupt
(PPI 27, or whatever the ACPI GTDT says) is enabled. The timer fires at 100 Hz.
Each tick wakes the CPU from `wfi`, so the shell can poll the keyboard without
spinning.

## Hardware discovery (ACPI)

QEMU and VirtualBox place devices at different addresses, and VirtualBox moves
them depending on the VM settings. ROS reads them from the ACPI tables instead
of hardcoding them:

| Table | What ROS takes from it                                   |
|-------|----------------------------------------------------------|
| MADT  | GIC distributor and redistributor base addresses         |
| MCFG  | PCIe ECAM base and bus range                             |
| GTDT  | virtual timer interrupt number                           |
| FADT  | whether PSCI uses `hvc` or `smc`; the DSDT address       |
| DSDT  | PL011 UART base (found by its hardware ID `ARMH0011`)    |

Each table is turned into a bounds-checked byte slice before it is parsed, and
the parsers are unit-tested with synthetic tables.

## Device register access

Under a hypervisor every device register access traps and is emulated. The
hypervisor must decode the faulting instruction, and VirtualBox can only handle
simple `ldr`/`str` forms. `read_volatile` may compile to loads with address
write-back, which stop the VM with `VERR_NEM_IPE_2`. So all register access goes
through `src/mmio.rs`, which uses inline assembly. These functions are `unsafe`:
callers document why the address is a register they own.

## USB keyboard (xHCI)

`xhci.rs` implements the minimum of the xHCI specification needed for a
keyboard:

1. Halt and reset the controller, set up the device context array, a command
   ring and an event ring.
2. For each connected port: reset it, enable a slot, *Address Device*, read the
   device/configuration descriptors and the product name, *Set Configuration*.
3. If an interface is a HID boot keyboard, switch it to the boot protocol and
   configure its interrupt IN endpoint.
4. Keep one 8-byte transfer queued on that endpoint. Each completion is a
   keyboard report; `keyboard.rs` turns reports into key presses (with US/DE
   layouts and key repeat).

Everything is polled. The serial port is an alternative input, which is how
the end-to-end test could also drive the shell.

## Console

`console.rs` draws text on the framebuffer with a pre-rendered bitmap font and
alpha blending for smooth edges. It keeps a shadow copy of the screen in RAM,
because reading video memory is slow, and scrolls on that copy. Everything
printed is mirrored to the serial port.

## Testing

| Layer        | How                                                               |
|--------------|-------------------------------------------------------------------|
| Unit tests   | `make test`: modules compiled for `aarch64-unknown-linux-gnu`; the boot code is left out with `cfg(test)`. Covers ACPI parsing, USB descriptor parsing, keyboard layouts and repeat, the console, the frame allocator and shell helpers. |
| Lints        | `make check`: `rustfmt`, `clippy -D warnings` for both targets (every `unsafe` block needs a `// SAFETY:` comment), `rustdoc -D warnings`. |
| End to end   | `make smoke`: `scripts/qemu.py` boots the image in QEMU, waits for the prompt, types commands on the emulated USB keyboard through QMP, checks the serial output and expects `shutdown` to power the VM off. |

CI runs all three on every push and pull request.
