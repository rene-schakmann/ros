#!/usr/bin/env bash
# Manage the ROS VirtualBox VM (Apple Silicon: ARM platform, EFI firmware).
set -euo pipefail

VM=ROS
CTL=VirtioSCSI   # controller created by `createvm --default` on ARM
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
IMG="$ROOT/build/ros.img"
VDI="$ROOT/build/ros.vdi"

vm_exists()  { VBoxManage showvminfo "$VM" >/dev/null 2>&1; }
vm_running() { VBoxManage list runningvms | grep -q "\"$VM\""; }

stop_vm() {
    if vm_running; then
        VBoxManage controlvm "$VM" poweroff >/dev/null 2>&1 || true
        sleep 2
    fi
}

detach_disk() {
    if vm_exists; then
        VBoxManage storageattach "$VM" --storagectl "$CTL" --port 0 --device 0 --medium none >/dev/null 2>&1 || true
    fi
    VBoxManage closemedium disk "$VDI" >/dev/null 2>&1 || true
}

make_vdi() {
    stop_vm
    detach_disk
    rm -f "$VDI"
    VBoxManage convertfromraw "$IMG" "$VDI" --format VDI >/dev/null
    echo "created $VDI"
}

make_vm() {
    if ! vm_exists; then
        # --default gives VirtualBox's recommended ARM setup:
        # EFI firmware, VMSVGA graphics, VirtIO-SCSI controller.
        VBoxManage createvm --name "$VM" --platform-architecture arm \
            --ostype Other_arm64 --default --register >/dev/null
        VBoxManage modifyvm "$VM" --memory 1024 --cpus 1 --audio-enabled off
        echo "created VM $VM"
    fi
    stop_vm
    # Serial port (PL011) -> build/serial.log: the kernel's debug log.
    VBoxManage modifyvm "$VM" --uart1 0x3f8 4 --uart-mode1 file "$ROOT/build/serial.log"
    VBoxManage storageattach "$VM" --storagectl "$CTL" --port 0 --device 0 \
        --type hdd --medium "$VDI"
    echo "attached $VDI to $VM"
}

destroy_vm() {
    stop_vm
    detach_disk
    if vm_exists; then
        VBoxManage unregistervm "$VM" --delete
    fi
}

case "${1:-}" in
    vdi)     make_vdi ;;
    vm)      make_vm ;;
    destroy) destroy_vm ;;
    *) echo "usage: $0 vdi|vm|destroy" >&2; exit 1 ;;
esac
