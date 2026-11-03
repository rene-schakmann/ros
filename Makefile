# ROS - host-side entry point. Everything is built inside Docker (see
# Dockerfile), so the host only needs Docker, plus VirtualBox for `make run`.
IMAGE := ros-build
VM    := ROS
DOCKER_RUN = docker run --rm $(1) -v "$(CURDIR)":/ros -v ros-cargo:/usr/local/cargo/registry -w /ros $(IMAGE)

all: image

help:
	@echo "make image        build build/ros.img (GPT disk with the kernel as BOOTAA64.EFI)"
	@echo "make rpi5         build build/ros-rpi5.img, an SD card image for the Raspberry Pi 5"
	@echo "make qemu         boot in QEMU, serial console in this terminal (Ctrl-A X quits)"
	@echo "make run          boot in VirtualBox (Apple Silicon)"
	@echo "make log          follow the VirtualBox VM's serial log"
	@echo "make test         unit tests"
	@echo "make check        rustfmt, clippy and rustdoc"
	@echo "make smoke        boot in QEMU and run the end-to-end shell test"
	@echo "make screenshots  regenerate docs/screenshots/*.png"
	@echo "make fmt          format the code"
	@echo "make clean        remove build output and the VirtualBox VM"

# Build the toolchain container image (cached by Docker).
docker:
	docker build -q -t $(IMAGE) . >/dev/null

image: docker
	$(call DOCKER_RUN) make -f build.mk

test check fmt rpi5 screenshots: docker
	$(call DOCKER_RUN) make -f build.mk $@

# PROFILE=gicv2-el2 make smoke tests the Raspberry Pi 5 code paths in QEMU.
smoke: docker
	$(call DOCKER_RUN,-e PROFILE) make -f build.mk smoke

qemu: docker
	$(call DOCKER_RUN,-it) make -f build.mk qemu

# Convert raw image to VirtualBox disk
vdi: image
	./scripts/mkvm.sh vdi

# Create/refresh the VM and attach the fresh disk
vm: vdi
	./scripts/mkvm.sh vm

# Build everything and boot it in VirtualBox
run: vm
	VBoxManage startvm $(VM)

# Follow the kernel's serial log
log:
	tail -f build/serial.log

clean:
	-./scripts/mkvm.sh destroy
	rm -rf build target

.PHONY: all help docker image rpi5 test check fmt smoke screenshots qemu vdi vm run log clean
