#!/usr/bin/env python3
"""Run ROS in QEMU: interactively, as an automated boot test, or to take screenshots.

    scripts/qemu.py run                 # serial console in this terminal (Ctrl-A X quits)
    scripts/qemu.py test                # boot, type commands, check the output, shut down
    scripts/qemu.py screenshots DIR     # boot, run commands, save PNG screenshots to DIR

The machine mirrors what ROS expects from VirtualBox: AArch64, UEFI firmware
with ACPI, GICv3, a PL011 UART, an xHCI controller with a USB keyboard and a
simple framebuffer (ramfb). Commands are typed on the emulated USB keyboard
through QMP, so the test also covers the xHCI and HID code.

Only the Python standard library is used. Environment variables:
    QEMU        QEMU binary (default: qemu-system-aarch64)
    QEMU_EFI    UEFI firmware image (default: searched in common locations)
    ROS_IMAGE   disk image (default: build/ros.img)
    TIMEOUT     seconds to wait for each step (default: 120)
    PROFILE     machine profile (default: gicv3):
                  gicv3      GICv3, firmware hands over at EL1 (like VirtualBox)
                  gicv2-el2  GICv2, firmware hands over at EL2 (like the Raspberry Pi 5)
"""

import json
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
IMAGE = Path(os.environ.get("ROS_IMAGE", ROOT / "build" / "ros.img"))
TIMEOUT = float(os.environ.get("TIMEOUT", "120"))
PROMPT = "rene@ros:~$ "

PROFILES = {
    "gicv3": {"machine": "virt,gic-version=3", "el": 1, "gic": 3},
    "gicv2-el2": {"machine": "virt,gic-version=2,virtualization=on", "el": 2, "gic": 2},
}
PROFILE_NAME = os.environ.get("PROFILE", "gicv3")
if PROFILE_NAME not in PROFILES:
    sys.exit(f"error: unknown PROFILE {PROFILE_NAME!r}; choose one of {', '.join(PROFILES)}")
PROFILE = PROFILES[PROFILE_NAME]

FIRMWARE_CANDIDATES = [
    "/usr/share/qemu-efi-aarch64/QEMU_EFI.fd",  # Debian, Ubuntu
    "/usr/share/AAVMF/AAVMF_CODE.fd",
    "/usr/share/edk2/aarch64/QEMU_EFI.fd",  # Fedora
    "/opt/homebrew/share/qemu/edk2-aarch64-code.fd",  # Homebrew (Apple Silicon)
    "/usr/local/share/qemu/edk2-aarch64-code.fd",  # Homebrew (Intel)
]

# Characters we can type, as QEMU key codes ("qcodes"): (key, needs shift).
KEYS = {c: (c, False) for c in "abcdefghijklmnopqrstuvwxyz0123456789"}
KEYS.update({c.upper(): (c, True) for c in "abcdefghijklmnopqrstuvwxyz"})
KEYS.update({" ": ("spc", False), "-": ("minus", False), ".": ("dot", False),
             "/": ("slash", False), "\n": ("ret", False)})

ANSI = re.compile(r"\x1b\[[0-9;=?]*[A-Za-z]")


def firmware() -> str:
    path = os.environ.get("QEMU_EFI") or next((p for p in FIRMWARE_CANDIDATES if Path(p).exists()), None)
    if not path:
        sys.exit("error: no AArch64 UEFI firmware found; install qemu-efi-aarch64 or set QEMU_EFI")
    return path


def qemu_command(extra: list[str]) -> list[str]:
    if not IMAGE.exists():
        sys.exit(f"error: {IMAGE} not found; build it first (make image)")
    return [
        os.environ.get("QEMU", "qemu-system-aarch64"),
        "-machine", PROFILE["machine"],
        "-cpu", "cortex-a72",
        "-m", "1024",
        "-bios", firmware(),
        "-drive", f"if=none,format=raw,file={IMAGE},id=disk,snapshot=on",
        "-device", "virtio-blk-pci,drive=disk",
        "-device", "qemu-xhci",
        "-device", "usb-kbd",
        "-device", "ramfb",
        "-nic", "none",
        "-no-reboot",
        *extra,
    ]


class Machine:
    """A QEMU instance with the serial port on a pipe and a QMP control socket."""

    def __init__(self) -> None:
        self.tmp = tempfile.mkdtemp(prefix="ros-qemu-")
        qmp_path = os.path.join(self.tmp, "qmp.sock")
        self.proc = subprocess.Popen(
            qemu_command(["-display", "none", "-serial", "stdio", "-monitor", "none",
                          "-qmp", f"unix:{qmp_path},server=on,wait=off"]),
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        )
        self.output = ""
        self.lock = threading.Lock()
        threading.Thread(target=self._read_serial, daemon=True).start()

        deadline = time.monotonic() + 10
        while not os.path.exists(qmp_path):
            if time.monotonic() > deadline or self.proc.poll() is not None:
                raise RuntimeError("QEMU did not start:\n" + self.output)
            time.sleep(0.05)
        self.qmp = socket.socket(socket.AF_UNIX)
        self.qmp.connect(qmp_path)
        self.qmp_file = self.qmp.makefile("rw")
        self.qmp_file.readline()  # greeting
        self.command("qmp_capabilities")

    def _read_serial(self) -> None:
        for chunk in iter(lambda: self.proc.stdout.read1(4096), b""):
            text = ANSI.sub("", chunk.decode("utf-8", "replace")).replace("\r", "")
            with self.lock:
                self.output += text
            if os.environ.get("VERBOSE"):
                sys.stdout.write(text)
                sys.stdout.flush()

    def command(self, name: str, **arguments) -> dict:
        self.qmp_file.write(json.dumps({"execute": name, "arguments": arguments}) + "\n")
        self.qmp_file.flush()
        while True:
            reply = json.loads(self.qmp_file.readline())
            if "event" in reply:
                continue
            if "error" in reply:
                raise RuntimeError(f"QMP {name}: {reply['error']}")
            return reply

    def wait_for(self, text: str, start: int = 0, timeout: float = TIMEOUT) -> str:
        """Wait until `text` appears in the serial output after offset `start`."""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            with self.lock:
                index = self.output.find(text, start)
                if index >= 0:
                    return self.output[start:index + len(text)]
            if self.proc.poll() is not None:
                break
            time.sleep(0.05)
        raise TimeoutError(f"timed out waiting for {text!r}; serial output so far:\n{self.output}")

    def type(self, text: str) -> None:
        """Type `text` on the emulated USB keyboard."""
        for char in text:
            key, shift = KEYS[char]
            keys = [{"type": "qcode", "data": "shift"}] if shift else []
            keys.append({"type": "qcode", "data": key})
            self.command("send-key", keys=keys, **{"hold-time": 30})
            time.sleep(0.04)

    def run(self, line: str) -> str:
        """Type a command and return its output (up to the next prompt)."""
        start = len(self.output)
        self.type(line + "\n")
        output = self.wait_for(PROMPT, start)
        return output[: -len(PROMPT)]

    def screenshot(self, path: Path) -> None:
        time.sleep(0.6)  # let the cursor blink settle and the screen update
        self.command("screendump", filename=str(path.resolve()), format="png")
        print(f"  saved {path}")

    def close(self) -> None:
        if self.proc.poll() is None:
            self.proc.kill()
        self.proc.wait()
        shutil.rmtree(self.tmp, ignore_errors=True)


def check(condition: bool, message: str, output: str) -> None:
    if not condition:
        raise AssertionError(f"{message}\n--- output ---\n{output}")
    print(f"  ok  {message}")


def boot(machine: Machine) -> str:
    started = time.monotonic()
    output = machine.wait_for(PROMPT)
    print(f"  booted to the shell in {time.monotonic() - started:.1f}s")
    return output


def test() -> None:
    print(f"profile {PROFILE_NAME}: -machine {PROFILE['machine']}")
    machine = Machine()
    try:
        out = boot(machine)
        check("ROS kernel started, firmware exited" in out, "kernel took over from UEFI", out)
        for line in ["exception vectors installed", "PCI:", "USB keyboard ready"]:
            check(f"[ ok ] {line}" in out, f"boot step: {line}", out)
        check("[fail]" not in out, "no failed boot steps", out)
        check(f"running at EL{PROFILE['el']}" in out, f"kernel runs at EL{PROFILE['el']}", out)
        check(f"GICv{PROFILE['gic']} interrupt controller" in out, f"GICv{PROFILE['gic']} driver", out)

        out = machine.run("echo hello from the USB keyboard")
        check("hello from the USB keyboard" in out, "USB keyboard input and echo", out)
        out = machine.run("sysinfo")
        check(f"GICv{PROFILE['gic']}, distributor" in out and "PL011 UART" in out, "sysinfo shows hardware", out)
        out = machine.run("lsusb")
        check("keyboard" in out, "lsusb lists the keyboard", out)
        out = machine.run("uptime")
        check("timer interrupts handled" in out, "timer interrupts are running", out)
        out = machine.run("brk")
        check("breakpoint hit" in out, "breakpoint exception is recovered", out)
        out = machine.run("nosuchcommand")
        check("command not found" in out, "unknown commands are reported", out)

        machine.type("shutdown\n")
        code = machine.proc.wait(timeout=TIMEOUT)
        check(code == 0, "shutdown powers off the VM (PSCI)", machine.output)
        print("smoke test passed")
    finally:
        machine.close()


def screenshots(directory: Path) -> None:
    directory.mkdir(parents=True, exist_ok=True)
    machine = Machine()
    try:
        boot(machine)
        machine.screenshot(directory / "boot.png")
        for name, commands in [
            ("help", ["clear", "help"]),
            ("sysinfo", ["clear", "sysinfo", "lspci", "lsusb"]),
            ("shell", ["clear", "mem", "uptime", "time", "kbd de", "color 14", "echo hallo welt"]),
        ]:
            for command in commands:
                machine.run(command)
            machine.screenshot(directory / f"{name}.png")
        machine.run("clear")
        machine.run("brk")
        start = len(machine.output)
        machine.type("panic\n")
        machine.wait_for("System halted", start)
        machine.screenshot(directory / "panic.png")
    finally:
        machine.close()


def main() -> None:
    match sys.argv[1:]:
        case ["run"]:
            # Serial console on stdio; the screen is not shown (no display in Docker).
            os.execvp(os.environ.get("QEMU", "qemu-system-aarch64"),
                      qemu_command(["-nographic"]))
        case ["test"]:
            test()
        case ["screenshots", directory]:
            screenshots(Path(directory))
        case _:
            sys.exit(__doc__)


if __name__ == "__main__":
    main()
