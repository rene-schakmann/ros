# Contributing to ROS

Thanks for your interest! ROS is a learning project, so contributions that make
it easier to understand are just as welcome as new features. Bug reports,
questions and documentation fixes all help.

## Getting started

1. Install [Docker](https://www.docker.com/). Everything else runs in the build
   container.
2. Fork and clone the repository, then run:

   ```sh
   make qemu    # build and boot in QEMU
   ```

3. Before you open a pull request, run:

   ```sh
   make check   # rustfmt, clippy, rustdoc
   make test    # unit tests (needs an AArch64 host, e.g. Docker on Apple Silicon)
   make smoke   # boots ROS in QEMU and tests the shell end to end
   ```

   CI runs the same commands, so if they pass locally your PR should pass too.

## Coding guidelines

- **Explain the hardware.** When you touch a register or a data structure,
  name the constant, say what it does, and point to the specification
  (section numbers are great).
- **Every `unsafe` block gets a `// SAFETY:` comment** explaining why it is
  sound. Clippy enforces this. `unsafe fn`s document their contract in a
  `# Safety` section.
- **All device register access goes through `src/mmio.rs`.** `read_volatile`
  breaks under VirtualBox (see the troubleshooting section in the README).
- **Keep parsing pure.** Code that interprets bytes (ACPI tables, USB
  descriptors, keyboard reports) should work on slices so it can be
  unit-tested on the host. Add tests for new parsers and edge cases.
- **No new dependencies without a good reason.** The kernel is meant to show
  how things work, so prefer writing the small piece yourself.
- Format with `make fmt` (`rustfmt.toml` holds the settings).

## Adding a shell command

Add a row to `COMMANDS` in `src/shell.rs` and write the handler. `help` and tab
completion pick it up automatically. If the command has interesting output,
consider adding a check to `scripts/qemu.py test`.

## Commit messages and pull requests

- Use the imperative mood and keep the subject short
  ("Add virtio-blk driver", "Fix key repeat after release").
- One logical change per pull request. Describe what you changed, why, and how
  you tested it (QEMU, VirtualBox, or both).
- If your change affects what the screen shows, run `make screenshots` and
  commit the updated images.

## Reporting bugs

Open an issue with the bug report template. Include the serial log
(`build/serial.log` for VirtualBox, the terminal output for QEMU), your
host OS, and whether you used QEMU or VirtualBox (with versions).

For security issues, see [SECURITY.md](SECURITY.md).

## License

By contributing, you agree that your contributions are dual licensed under the
MIT and Apache-2.0 licenses, as described in the [README](README.md#license).
