#!/usr/bin/env bash
# Can this runner actually run a KVM guest? Three things have to line up:
#   1. /dev/kvm exists
#   2. the CPU exposes vmx/svm and the kvm module has nested=1
#   3. THIS user may open the device
# GitHub ships /dev/kvm as root:kvm 0660, so step 3 fails for the runner user
# until a udev rule opens it up - which is what the well-known "Enable KVM
# group perms" snippet does. Test all three instead of stopping at step 1.
set -uo pipefail
echo "=========== runner: ${1:-?} ==========="
echo "--- 1. device ---"
ls -la /dev/kvm 2>&1 || echo "NO /dev/kvm"
echo "--- 2. cpu + nested ---"
grep -m1 -oE "(vmx|svm)" /proc/cpuinfo || echo "no vmx/svm"
for m in kvm_intel kvm_amd; do
  p="/sys/module/$m/parameters/nested"
  [ -e "$p" ] && echo "$m nested=$(cat $p)" || echo "$m not loaded"
done
echo "--- 3. permission as $(id -un) ---"
if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then echo "readable/writable"; else echo "NOT permitted for this user (root:kvm 0660)"; fi

echo "--- open it up the way CI does ---"
echo 'KERNEL=="kvm", GROUP="kvm", MODE="0666", OPTIONS+="static_node=kvm"' | sudo tee /etc/udev/rules.d/99-kvm4all.rules >/dev/null
sudo udevadm control --reload-rules
sudo udevadm trigger --name-match=kvm
sleep 1
ls -la /dev/kvm

echo "--- install qemu ---"
sudo apt-get update -qq >/dev/null 2>&1
sudo apt-get install -y -qq qemu-system-x86 >/dev/null 2>&1
qemu-system-x86_64 --version | head -1

# A real VM: 512M, no disk, boot the BIOS and let it print. If KVM is refused
# we see "failed to initialize kvm"; if it works the machine simply idles
# until the timeout, so treat "no kvm error" as success.
echo "--- booting a throwaway VM with -enable-kvm ---"
out=$(timeout 20 qemu-system-x86_64 -enable-kvm -cpu host -m 512 \
        -display none -no-reboot -serial stdio 2>&1 </dev/null || true)
echo "$out" | tail -8
if echo "$out" | grep -qiE "failed to initialize kvm|could not access kvm|permission denied"; then
  echo "RESULT: KVM NOT USABLE"
elif echo "$out" | grep -qiE "seaBIOS|Booting|No bootable"; then
  echo "RESULT: KVM WORKS - a guest actually started"
else
  echo "RESULT: no KVM error, but no guest output either (see log)"
fi
