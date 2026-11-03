# Runs inside the Docker build container (see Makefile), or directly on a
# host that has the Rust toolchain, mtools, dosfstools, fdisk and QEMU.
TARGET  := aarch64-unknown-uefi
BUILD   := build
EFI     := target/$(TARGET)/release/ros.efi
IMG     := $(BUILD)/ros.img

# 64 MiB disk, GPT, one EFI System Partition from sector 2048 (60 MiB).
ESP_START   := 2048
ESP_SECTORS := 122880
ESP_TYPE    := C12A7328-F81F-11D2-BA4B-00A0C93EC93B

all: $(IMG)

$(EFI): Cargo.toml Cargo.lock $(wildcard src/*.rs)
	cargo build --release --locked
	file $@

# Raw disk image: GPT with a FAT32 EFI System Partition holding the kernel at
# the default boot path \EFI\BOOT\BOOTAA64.EFI (plus startup.nsh for the UEFI shell).
$(IMG): $(EFI) startup.nsh
	rm -f $@ $(BUILD)/esp.img
	mkdir -p $(BUILD)
	truncate -s 64M $@
	printf 'label: gpt\nstart=$(ESP_START), size=$(ESP_SECTORS), type=$(ESP_TYPE), name="ESP"\n' | sfdisk -q $@
	truncate -s $$(( $(ESP_SECTORS) * 512 )) $(BUILD)/esp.img
	mkfs.fat -F 32 -s 1 -n ROS $(BUILD)/esp.img >/dev/null
	mmd -i $(BUILD)/esp.img ::/EFI ::/EFI/BOOT
	mcopy -i $(BUILD)/esp.img $(EFI) ::/EFI/BOOT/BOOTAA64.EFI
	mcopy -i $(BUILD)/esp.img startup.nsh ::/startup.nsh
	dd if=$(BUILD)/esp.img of=$@ bs=512 seek=$(ESP_START) conv=notrunc status=none
	rm -f $(BUILD)/esp.img

# Formatting, lints (kernel and unit tests) and documentation.
check:
	cargo fmt --check
	cargo clippy --release --locked -- -D warnings
	cargo clippy-host --locked -- -D warnings
	RUSTDOCFLAGS="-D warnings" cargo doc --document-private-items --locked

fmt:
	cargo fmt

# Unit tests, run as a Linux program (needs an AArch64 host).
test:
	cargo test-host --locked

# Boot the image in QEMU and drive the shell over the emulated USB keyboard.
smoke: $(IMG)
	python3 scripts/qemu.py test

# Interactive QEMU session on the serial console (Ctrl-A X quits).
qemu: $(IMG)
	python3 scripts/qemu.py run

screenshots: $(IMG)
	python3 scripts/qemu.py screenshots docs/screenshots

clean:
	rm -rf $(BUILD) target

.PHONY: all check fmt test smoke qemu screenshots clean $(EFI)
