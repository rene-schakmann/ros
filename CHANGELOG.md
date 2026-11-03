# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.0] - 2026-11-03

### Added

- **Raspberry Pi 5 support (experimental)** on the community UEFI firmware
  (worproject/rpi5-uefi). `make rpi5` builds an SD card image; the README
  explains how to flash and boot it.
- Running at EL2: separate exception vector tables for EL1 and EL2, interrupt
  routing to EL2, PSCI via `smc` from EL2.
- GICv2 interrupt controller driver (the Pi 5's GIC-400), chosen from the MADT.
- USB controllers described in ACPI instead of PCI (the Pi 5's RP1 xHCI
  controllers), and support for several USB controllers at once.
- Cache maintenance for non-coherent DMA (xHCI rings, contexts, buffers) and
  for the framebuffer. DMA buffers prefer memory below 3 GiB.
- UART discovery through the ACPI SPCR table (DSDT search as fallback).
- xHCI port power-on for controllers with port power control.
- `PROFILE=gicv2-el2 make smoke`: QEMU boot test with GICv2 at EL2, run in CI
  next to the GICv3/EL1 profile. CI also builds the Pi 5 image.
- `sysinfo` shows the board; `lsusb` lists devices per controller.

### Fixed

- The MADT GICC entry's redistributor base was read from the MPIDR field
  (offset 68 instead of 60). It only mattered for firmware without a GICR entry.
- An ACPI error at boot is now shown on screen (the console starts first).

## [0.3.0] - 2026-11-03

First public release.

### Added

- QEMU support: `make qemu` boots ROS with the serial console in the terminal,
  no VirtualBox needed.
- End-to-end test (`make smoke`): boots in QEMU, types on the emulated USB
  keyboard, checks the output and powers off.
- 31 unit tests for ACPI parsing, USB descriptor parsing, keyboard layouts and
  key repeat, the console, the page allocator and shell helpers (`make test`).
- `make check` (rustfmt, clippy for kernel and tests, rustdoc) and
  `make screenshots`.
- GitHub Actions CI and release workflow; Dependabot.
- Documentation: architecture guide, contributing guide, code of conduct,
  security policy, dual MIT/Apache-2.0 license, screenshots in the README.
- Tab key works over the serial console.

### Changed

- Rust edition 2024, toolchain pinned to Rust 1.99.1 (`rust-toolchain.toml`).
- Dependencies: `spin` 0.9 → 0.12, `noto-sans-mono-bitmap` 0.3 → 0.4 (sharper,
  slightly larger glyphs), `linked_list_allocator` 0.10.6.
- MMIO and system register writes are `unsafe`, and every `unsafe` block
  documents why it is sound (enforced by clippy).
- ACPI tables are parsed from bounds-checked slices and their signatures are
  validated. Missing GIC information is reported clearly instead of crashing.
- Shell commands are defined in one table that drives dispatch, `help` and tab
  completion. The undocumented `about` alias for `sysinfo` was removed.
- Magic numbers replaced by named constants that cite the specifications.

### Fixed

- `uptime` and timeouts no longer overflow after ~9 days.
- A malformed USB string descriptor could panic the kernel.
- USB descriptors were returned as `'static` slices into a reused DMA buffer;
  they are now copied.
- No framebuffer no longer panics at boot; ROS continues on the serial console.
- `sleep` with a huge argument could overflow.

## [0.2.0] - 2026-09-26

- USB keyboard via xHCI, GICv3 interrupts, ARM generic timer, ACPI discovery,
  PCI enumeration, framebuffer console, shell with history and tab completion.

[Unreleased]: https://github.com/rene-schakmann/ros/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/rene-schakmann/ros/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/rene-schakmann/ros/releases/tag/v0.3.0
