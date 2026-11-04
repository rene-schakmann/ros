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

The kernel runs at the exception level the firmware hands over: EL1 (kernel
mode) in QEMU and VirtualBox, EL2 (hypervisor mode) on the Raspberry Pi 5. It
keeps the firmware's page tables, which map all memory 1:1, so a physical
address can be used as a pointer.

## Memory

- **Heap** (`memory::init_heap`): 32 MiB reserved from the firmware during
  boot, managed by `linked_list_allocator`. It backs `Vec`, `String`, `Box`.
- **Frames** (`memory::alloc_pages`): a bump allocator over the CONVENTIONAL
  regions of the final memory map. Drivers use it for DMA buffers (xHCI rings,
  contexts, descriptors). Pages are never freed; driver buffers live as long
  as the OS.
- Boot-services memory is left alone: the firmware's page tables live there.

## Exceptions and interrupts

`exceptions.rs` defines the 16-entry vector table in assembly, once per
exception level (EL1 and EL2 have separate `ELR`/`SPSR`/`ESR`/`FAR`
registers); `exceptions::init` installs the one matching the current level.
Each entry saves
all general purpose and SIMD registers into a `Frame` (layout checked at compile
time) and calls `handle_exception` in Rust:

- **IRQ** → `interrupts::handle_irq`: acknowledge (`ICC_IAR1_EL1`), handle the
  timer tick, signal end of interrupt (`ICC_EOIR1_EL1`).
- **Breakpoint** (`brk`) → print a message, skip the instruction, resume.
- **Anything else** → panic screen with the registers, then halt.

The MADT says which GIC generation the machine has. A GICv3 (QEMU, VirtualBox)
is set up with affinity routing and the `ICC_*` system register CPU interface;
a GICv2 (the Pi 5's GIC-400) through its memory-mapped CPU interface. Only the
virtual timer interrupt (PPI 27, or whatever the ACPI GTDT says) is enabled.
At EL2, `HCR_EL2.IMO/FMO/AMO` route interrupts to EL2 so they aren't stuck
waiting for an EL1 that doesn't exist. The timer fires at 100 Hz.
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
| SPCR  | PL011 UART base (the firmware's serial console)          |
| DSDT  | board identity; UART fallback (hardware ID `ARMH0011`); on the Pi 5 the RP1 BAR (`PBAR`) |

Each table is turned into a bounds-checked byte slice before it is parsed, and
the parsers are unit-tested with synthetic tables. Checksums are verified (all
bytes of a table add up to 0); like Linux, ROS warns about a wrong one at boot
but still uses the table.

## Raspberry Pi 5

The Pi 5 differs from the virtual machines in four ways, each handled in one place:

| Difference | Where it's handled |
|------------|--------------------|
| Firmware hands over at EL2 | `exceptions.rs` (EL2 vectors), `interrupts.rs` (IRQ routing), `arch::psci_call` (`smc`, since `hvc` would trap into ROS itself) |
| GIC-400 is a GICv2 | `interrupts::init_gicv2` |
| USB controllers live in the RP1 I/O chip, which sits behind PCIe but whose peripherals are not PCI functions | `acpi::parse_dsdt` finds the RP1 BAR the firmware mapped (`PBAR`), the xHCI controllers are at `+0x200000` and `+0x300000` |
| RP1 DMA is not cache coherent, and only reaches the first 3 GiB | `arch::dma_clean`/`dma_invalidate` around every xHCI structure; `memory::DMA_LIMIT` |

The Broadcom PCIe controllers are not ECAM compliant (one config window per
controller), so on the Pi the PCI scan is skipped. The framebuffer is written
back from the CPU cache after every update so the display engine sees it.

QEMU can't emulate a Pi 5, but `PROFILE=gicv2-el2 make smoke` boots ROS on a
QEMU machine with a GICv2 and the firmware at EL2, which covers the first two
rows in CI.

## Device register access

Under a hypervisor every device register access traps and is emulated. The
hypervisor must decode the faulting instruction, and VirtualBox can only handle
simple `ldr`/`str` forms. `read_volatile` may compile to loads with address
write-back, which stop the VM with `VERR_NEM_IPE_2`. So all register access goes
through `src/mmio.rs`, which uses inline assembly. These functions are `unsafe`:
callers document why the address is a register they own.

## USB keyboard (xHCI)

`xhci.rs` implements the minimum of the xHCI specification needed for a
keyboard. ROS starts every controller it finds (PCI functions plus the Pi 5's
RP1 controllers) and polls them all:

1. Halt and reset the controller, set up the device context array, a command
   ring and an event ring.
2. Power all ports (needed after a reset when the controller has port power
   control). For each connected port: reset it, enable a slot, *Address Device*, read the
   device/configuration descriptors and the product name, *Set Configuration*.
3. If an interface is a HID boot keyboard, switch it to the boot protocol and
   configure its interrupt IN endpoint. The product name string is read last:
   some devices stall that request, which halts endpoint 0.
4. Keep one 8-byte transfer queued on each keyboard's endpoint (a controller
   can have several). Each completion is a keyboard report; `keyboard.rs`
   turns reports into key presses (with US/DE layouts, Ctrl and key repeat).

Everything is polled. The serial port is a second keyboard: `keyboard.rs`
decodes UTF-8 and the ANSI escape sequences a terminal sends for arrow keys,
Home, End and Delete (a lone ESC is the Escape key if nothing follows within
100 ms), so both inputs produce the same keys.

## Shell

`shell.rs` reads a line with a small line editor (`LineEditor`). It keeps the
text and the cursor position, and every edit returns what to print: a
backspace character moves the cursor one cell left without erasing, so
inserting in the middle prints the new character and the rest of the line,
then moves back. The framebuffer console and serial terminals both
understand that, so they show the same line.

## Console

`console.rs` draws text on the framebuffer with a pre-rendered bitmap font and
alpha blending for smooth edges. It keeps a shadow copy of the screen in RAM,
because reading video memory is slow, and scrolls on that copy. Everything
printed is mirrored to the serial port.

## Testing

| Layer        | How                                                               |
|--------------|-------------------------------------------------------------------|
| Unit tests   | `make test`: modules compiled for `aarch64-unknown-linux-gnu`; the boot code is left out with `cfg(test)`. Covers ACPI parsing and checksums, USB descriptor parsing, keyboard layouts, repeat and serial input decoding, the console, the frame allocator, the line editor and shell helpers. |
| Lints        | `make check`: `rustfmt`, `clippy -D warnings` for both targets (every `unsafe` block needs a `// SAFETY:` comment), `rustdoc -D warnings`. |
| End to end   | `make smoke`: `scripts/qemu.py` boots the image in QEMU with two USB keyboards, waits for the prompt, types commands on the emulated USB keyboard through QMP and on the serial port (line editing with escape sequences, UTF-8, Ctrl-C), checks the serial output and expects `shutdown` to power the VM off. |

CI runs all three on every push and pull request.
