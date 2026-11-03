//! The interactive shell: read a line, run a command, repeat.
//!
//! Commands are listed once in [`COMMANDS`]; that table drives dispatch,
//! `help` and tab completion. To add a command, add a row and a handler.

use crate::acpi::Platform;
use crate::arch;
use crate::console::{self, PALETTE, color, cprint, print, println};
use crate::interrupts::{self, uptime_ms};
use crate::keyboard::{self, Key, Layout, USB};
use crate::memory::{self, kind};
use crate::pci;
use alloc::string::String;
use alloc::vec::Vec;

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Longest command line in characters.
const LINE_MAX: usize = 200;
/// Cursor blink period in milliseconds (on for half of it).
const CURSOR_BLINK_MS: u64 = 1000;

const BANNER: [(&str, u32); 5] = [
    (r" _   _  _____  _      _       ___    ____   _____  _   _  _____ ", color::LIGHT_CYAN),
    (r"| | | || ____|| |    | |     / _ \  |  _ \ | ____|| \ | || ____|", color::CYAN),
    (r"| |_| ||  _|  | |    | |    | | | | | |_) ||  _|  |  \| ||  _|  ", color::LIGHT_BLUE),
    (r"|  _  || |___ | |___ | |___ | |_| | |  _ < | |___ | |\  || |___ ", color::LIGHT_MAGENTA),
    (r"|_| |_||_____||_____||_____| \___/  |_| \_\|_____||_| \_||_____|", color::MAGENTA),
];

/// A shell command: name, argument hint, description and handler.
struct Command {
    name: &'static str,
    args: &'static str,
    help: &'static str,
    run: fn(&mut Shell, &str),
}

const fn cmd(
    name: &'static str,
    args: &'static str,
    help: &'static str,
    run: fn(&mut Shell, &str),
) -> Command {
    Command { name, args, help, run }
}

/// All commands, in the order `help` lists them.
const COMMANDS: &[Command] = &[
    cmd("help", "", "show this list", |_, _| help()),
    cmd("hello", "", "show the welcome screen", |sh, _| sh.banner()),
    cmd("echo", "<text>", "print text", |_, arg| println!("{arg}")),
    cmd("clear", "", "clear the screen", |_, _| {
        console::with(|c| c.clear());
    }),
    cmd("sysinfo", "", "CPU and hardware overview", |sh, _| sh.sysinfo()),
    cmd("time", "", "date and time", |_, _| time()),
    cmd("uptime", "", "time since boot, timer interrupts", |_, _| uptime()),
    cmd("sleep", "<sec>", "wait, sleeping between timer interrupts", |_, arg| sleep(arg)),
    cmd("mem", "", "memory usage", |_, _| mem()),
    cmd("memmap", "", "physical memory map from the firmware", |_, _| memmap()),
    cmd("lspci", "", "list PCI devices", |sh, _| sh.lspci()),
    cmd("lsusb", "", "list USB devices", |_, _| lsusb()),
    cmd("color", "<1-15>", "change text color", |_, arg| set_color(arg)),
    cmd("kbd", "<us|de>", "keyboard layout", |_, arg| set_layout(arg)),
    cmd("brk", "", "trigger a breakpoint exception (recoverable)", |_, _| breakpoint()),
    cmd("panic", "", "crash the kernel on purpose", |_, _| crash()),
    cmd("reboot", "", "restart the machine", |sh, _| arch::reboot(sh.platform.psci_hvc)),
    cmd("shutdown", "", "power off", |sh, _| arch::power_off(sh.platform.psci_hvc)),
];

/// Split a command line into command name and (trimmed) argument.
fn split_command(line: &str) -> (&str, &str) {
    match line.trim().split_once(' ') {
        Some((cmd, arg)) => (cmd, arg.trim()),
        None => (line.trim(), ""),
    }
}

/// The single command name starting with `prefix`, if exactly one matches.
fn complete(prefix: &str) -> Option<&'static str> {
    let mut matches = COMMANDS.iter().map(|c| c.name).filter(|name| name.starts_with(prefix));
    match (matches.next(), matches.next()) {
        (Some(only), None) => Some(only),
        _ => None,
    }
}

/// Shell state: what the kernel found at boot, plus the command history.
#[derive(Debug)]
pub struct Shell {
    platform: Platform,
    pci: Vec<pci::Device>,
    history: Vec<String>,
}

impl Shell {
    /// Create a shell for the hardware found during boot.
    pub fn new(platform: Platform, pci: Vec<pci::Device>) -> Self {
        Shell { platform, pci, history: Vec::new() }
    }

    /// Show the banner and run commands forever.
    pub fn run(mut self) -> ! {
        self.banner();
        loop {
            self.prompt();
            let line = self.read_line();
            let line = line.trim();
            if !line.is_empty() && self.history.last().map(String::as_str) != Some(line) {
                self.history.push(String::from(line));
            }
            self.execute(line);
        }
    }

    fn banner(&self) {
        let cols = console::with(|c| c.size().0).unwrap_or(80);
        let pad = |width: usize| " ".repeat(cols.saturating_sub(width) / 2);
        println!("\n");
        console::with(|c| c.set_bold(true));
        for (line, rgb) in BANNER {
            cprint!(rgb, "{}{line}\n", pad(line.len()));
        }
        console::with(|c| c.set_bold(false));
        let sub = alloc::format!("Welcome to ROS v{VERSION} - type 'help' for commands");
        cprint!(color::YELLOW, "\n{}{sub}\n\n", pad(sub.len()));
    }

    fn prompt(&self) {
        console::with(|c| c.set_bold(true));
        cprint!(color::LIGHT_GREEN, "rene@ros");
        cprint!(color::WHITE, ":");
        cprint!(color::LIGHT_BLUE, "~");
        cprint!(color::WHITE, "$ ");
        console::with(|c| c.set_bold(false));
    }

    /// Line editor: typing, backspace, history (up/down), tab completion.
    fn read_line(&mut self) -> String {
        let mut line = String::new();
        let mut history_pos = self.history.len();
        loop {
            match wait_key() {
                Key::Enter => {
                    println!();
                    return line;
                }
                Key::Backspace => {
                    if line.pop().is_some() {
                        print!("\x08 \x08");
                    }
                }
                Key::Escape => replace_line(&mut line, ""),
                Key::Up if history_pos > 0 => {
                    history_pos -= 1;
                    replace_line(&mut line, &self.history[history_pos]);
                }
                Key::Down if history_pos < self.history.len() => {
                    history_pos += 1;
                    let entry = self.history.get(history_pos).map(String::as_str).unwrap_or("");
                    replace_line(&mut line, entry);
                }
                Key::Tab => {
                    if let Some(only) = complete(&line) {
                        let rest = &only[line.len()..];
                        print!("{rest} ");
                        line.push_str(rest);
                        line.push(' ');
                    }
                }
                Key::Char(c) if !c.is_control() && line.chars().count() < LINE_MAX => {
                    line.push(c);
                    print!("{c}");
                }
                _ => {}
            }
        }
    }

    fn execute(&mut self, line: &str) {
        let (name, arg) = split_command(line);
        if name.is_empty() {
            return;
        }
        match COMMANDS.iter().find(|c| c.name == name) {
            Some(command) => (command.run)(self, arg),
            None => cprint!(color::LIGHT_RED, "command not found: {name}\n"),
        }
    }

    fn sysinfo(&self) {
        let p = &self.platform;
        let (vendor, part) = arch::cpu_name();
        let (w, h) = console::with(|c| c.resolution()).unwrap_or_default();
        let (cols, rows) = console::with(|c| c.size()).unwrap_or_default();
        let usb = USB.lock();
        let row = |label: &str| cprint!(color::LIGHT_CYAN, "  {label:<9}");
        cprint!(color::WHITE, "ROS v{VERSION} - a tiny AArch64 operating system in Rust\n");
        row("CPU");
        println!("{vendor} part {part:#x}, running at EL{}", arch::current_el());
        row("Timer");
        println!(
            "ARM generic timer, {} MHz, IRQ {}, {} Hz tick",
            interrupts::timer_frequency() / 1_000_000,
            p.timer_irq,
            interrupts::TICK_HZ
        );
        row("GIC");
        println!("GICv3, distributor {:#x}, redistributors {:#x}", p.gicd, p.gicr);
        row("PCI");
        println!(
            "ECAM {:#x}, buses {}-{}, {} devices",
            p.pci_ecam,
            p.pci_bus_start,
            p.pci_bus_end,
            self.pci.len()
        );
        row("USB");
        match usb.as_ref() {
            Some(x) => {
                println!("xHCI {}.{}, {} devices", x.version >> 8, (x.version >> 4) & 0xf, x.devices.len());
            }
            None => println!("no controller"),
        }
        row("Screen");
        println!("{w}x{h} framebuffer, {cols}x{rows} characters");
        row("Serial");
        match p.uart {
            Some(base) => println!("PL011 UART at {base:#x}"),
            None => println!("none"),
        }
        row("Power");
        println!("PSCI via {}", if p.psci_hvc { "hvc (hypervisor call)" } else { "smc" });
    }

    fn lspci(&self) {
        for d in &self.pci {
            cprint!(color::LIGHT_CYAN, "{:02x}:{:02x}.{} ", d.bus, d.dev, d.func);
            println!("[{:04x}:{:04x}] {} - {}", d.vendor, d.device, d.class_name(), d.vendor_name());
        }
    }
}

fn wait_key() -> Key {
    loop {
        if let Some(key) = keyboard::poll() {
            return key;
        }
        console::with(|c| c.show_cursor(uptime_ms() % CURSOR_BLINK_MS < CURSOR_BLINK_MS / 2));
        // Sleep until the next interrupt (at the latest the 10 ms timer tick).
        arch::wait_for_interrupt();
    }
}

fn replace_line(line: &mut String, new: &str) {
    for _ in 0..line.chars().count() {
        print!("\x08 \x08");
    }
    line.clear();
    line.push_str(new);
    print!("{new}");
}

fn help() {
    println!("Commands:");
    for c in COMMANDS {
        let usage =
            if c.args.is_empty() { String::from(c.name) } else { alloc::format!("{} {}", c.name, c.args) };
        cprint!(color::LIGHT_CYAN, "  {usage:<14}");
        println!("{}", c.help);
    }
    cprint!(color::GRAY, "Keys: up/down = history, tab = complete, esc = clear line\n");
}

fn time() {
    // The real-time clock is read through the firmware's runtime services,
    // one of the few firmware features that survive ExitBootServices.
    match uefi::runtime::get_time() {
        Ok(t) => println!(
            "{}-{:02}-{:02} {:02}:{:02}:{:02}",
            t.year(),
            t.month(),
            t.day(),
            t.hour(),
            t.minute(),
            t.second()
        ),
        Err(_) => println!("time: clock not available"),
    }
}

/// Format milliseconds as "1h 02m 03.004s".
fn format_duration(ms: u64) -> String {
    let s = ms / 1000;
    alloc::format!("{}h {:02}m {:02}.{:03}s", s / 3600, s / 60 % 60, s % 60, ms % 1000)
}

fn uptime() {
    println!("up {}, {} timer interrupts handled", format_duration(uptime_ms()), interrupts::ticks());
}

fn sleep(arg: &str) {
    let Some(ms) = arg.parse::<u64>().ok().and_then(|secs| secs.checked_mul(1000)) else {
        return println!("usage: sleep <seconds>");
    };
    let secs = ms / 1000;
    let (start, ticks) = (uptime_ms(), interrupts::ticks());
    while uptime_ms() < start.saturating_add(ms) {
        arch::wait_for_interrupt();
    }
    println!("slept {secs}s, woke up {} times by the timer", interrupts::ticks() - ticks);
}

/// Human-readable size: "12 MiB" or "348 KiB".
fn size(bytes: u64) -> String {
    if bytes >= 10 * 1024 * 1024 {
        alloc::format!("{} MiB", bytes >> 20)
    } else {
        alloc::format!("{} KiB", bytes >> 10)
    }
}

fn mem() {
    let s = memory::stats();
    let (heap_used, heap_size) = memory::heap_stats();
    let row = |label: &str, value: String| {
        cprint!(color::LIGHT_CYAN, "  {label:<24}");
        println!("{value:>10}");
    };
    row("RAM total", size(s.total));
    row("free", size(s.free));
    row("kernel image + heap", size(s.kernel));
    row("driver buffers", size(s.used_by_drivers));
    row("firmware (runtime/ACPI)", size(s.firmware));
    row("boot leftovers", size(s.reclaimable));
    row("heap in use", alloc::format!("{} of {}", size(heap_used as u64), size(heap_size as u64)));
}

fn kind_name(k: u32) -> &'static str {
    match k {
        kind::LOADER_CODE => "kernel code",
        kind::LOADER_DATA => "kernel data",
        kind::BOOT_CODE | kind::BOOT_DATA => "boot leftovers",
        kind::RUNTIME_CODE | kind::RUNTIME_DATA => "firmware runtime",
        kind::CONVENTIONAL => "free",
        kind::ACPI_RECLAIM | kind::ACPI_NVS => "ACPI tables",
        kind::MMIO => "device MMIO",
        _ => "reserved",
    }
}

/// Merge neighbouring regions with the same meaning to keep the list short.
/// Returns `(name, start, end)` triples.
fn merge_regions(regions: &[memory::Region]) -> Vec<(&'static str, u64, u64)> {
    let mut merged: Vec<(&str, u64, u64)> = Vec::new();
    for r in regions {
        let name = kind_name(r.kind);
        match merged.last_mut() {
            Some((n, _, e)) if *n == name && *e == r.start => *e = r.end(),
            _ => merged.push((name, r.start, r.end())),
        }
    }
    merged
}

fn memmap() {
    for (name, start, end) in merge_regions(&memory::regions()) {
        cprint!(color::LIGHT_CYAN, "  {start:#012x}-{:#012x}", end - 1);
        println!(" {:>8} KiB  {name}", (end - start) / 1024);
    }
}

fn lsusb() {
    let usb = USB.lock();
    let Some(x) = usb.as_ref() else {
        return println!("no USB controller");
    };
    if x.devices.is_empty() {
        println!("no USB devices");
    }
    for d in &x.devices {
        cprint!(color::LIGHT_CYAN, "port {} slot {} ", d.port, d.slot);
        print!(
            "[{:04x}:{:04x}] {}",
            d.vendor,
            d.product,
            if d.name.is_empty() { "(no name)" } else { &d.name }
        );
        cprint!(color::GRAY, ", {}{}\n", d.speed, if d.is_keyboard { ", keyboard" } else { "" });
    }
}

/// Raise a breakpoint exception; the handler reports it and resumes.
fn breakpoint() {
    // SAFETY: the exception handler skips the `brk` instruction and returns here.
    unsafe { core::arch::asm!("brk #0") };
}

/// Read an unmapped address, which ends in the kernel panic screen.
fn crash() {
    println!("reading from an unmapped address...");
    // SAFETY: deliberately unsound: the data abort never returns (see `exceptions::fatal`).
    unsafe { core::ptr::read_volatile(0xdead_0000_0000 as *const u64) };
}

fn set_color(arg: &str) {
    match arg.parse::<usize>() {
        Ok(n @ 1..=15) => {
            console::with(|c| c.set_fg(PALETTE[n]));
        }
        _ => println!("usage: color <1-15>"),
    }
}

fn set_layout(arg: &str) {
    match arg {
        "us" => keyboard::set_layout(Layout::Us),
        "de" => keyboard::set_layout(Layout::De),
        _ => {
            let now = if keyboard::layout() == Layout::De { "de" } else { "us" };
            return println!("usage: kbd <us|de>  (current: {now})");
        }
    }
    println!("keyboard layout: {arg}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::Region;

    #[test]
    fn splits_command_lines() {
        assert_eq!(split_command("echo  hello world "), ("echo", "hello world"));
        assert_eq!(split_command("  help"), ("help", ""));
        assert_eq!(split_command(""), ("", ""));
    }

    #[test]
    fn completes_unique_prefixes() {
        assert_eq!(complete("sys"), Some("sysinfo"));
        assert_eq!(complete("ls"), None); // lspci and lsusb
        assert_eq!(complete("lsu"), Some("lsusb"));
        assert_eq!(complete("xyz"), None);
    }

    #[test]
    fn command_names_are_unique() {
        for (i, a) in COMMANDS.iter().enumerate() {
            assert!(COMMANDS[i + 1..].iter().all(|b| b.name != a.name), "duplicate command {}", a.name);
        }
    }

    #[test]
    fn formats_sizes_and_durations() {
        assert_eq!(size(512 * 1024), "512 KiB");
        assert_eq!(size(32 * 1024 * 1024), "32 MiB");
        assert_eq!(format_duration(3_723_004), "1h 02m 03.004s");
    }

    #[test]
    fn merges_adjacent_regions_of_the_same_kind() {
        let r = |kind, start, pages| Region { kind, start, pages };
        let map = [
            r(kind::BOOT_CODE, 0x0000, 1),
            r(kind::BOOT_DATA, 0x1000, 1), // same name as BOOT_CODE
            r(kind::CONVENTIONAL, 0x2000, 2),
            r(kind::CONVENTIONAL, 0x8000, 1), // not adjacent
        ];
        assert_eq!(
            merge_regions(&map),
            [("boot leftovers", 0x0000, 0x2000), ("free", 0x2000, 0x4000), ("free", 0x8000, 0x9000)]
        );
    }
}
