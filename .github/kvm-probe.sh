#!/usr/bin/env bash
# Report whether this runner can actually give QEMU hardware acceleration.
# $1 is just a label for the log.
set -uo pipefail
echo "=========== runner: ${1:-?} ==========="
echo "--- /dev/kvm ---"
ls -la /dev/kvm 2>&1 || echo "NO /dev/kvm"
echo "--- cpu virtualization flag ---"
grep -m1 -oE "(vmx|svm)" /proc/cpuinfo || echo "no vmx/svm in /proc/cpuinfo"
echo "--- kvm module nested param ---"
for m in kvm_intel kvm_amd; do
  p="/sys/module/$m/parameters/nested"
  if [ -e "$p" ]; then echo "$m nested=$(cat $p)"; else echo "$m not loaded"; fi
done
echo "--- kvm in lsmod ---"
lsmod 2>/dev/null | grep -E "^kvm" || echo "no kvm modules"

if [ ! -e /dev/kvm ]; then
  echo "RESULT: NO KVM - a bake here would run under TCG (10-50x slower)"
  exit 0
fi

echo "--- install qemu ---"
sudo apt-get update -qq >/dev/null 2>&1
sudo apt-get install -y -qq qemu-system-x86 >/dev/null 2>&1
qemu-system-x86_64 --version | head -1

# The only honest test: create a KVM VM and see whether it starts. -kernel
# /dev/null is invalid on purpose - we only care whether KVM initialises
# before the kernel load is attempted.
echo "--- qemu -enable-kvm smoke test ---"
out=$(timeout 30 qemu-system-x86_64 -enable-kvm -cpu host -m 512 -display none -no-reboot -kernel /dev/null 2>&1 || true)
echo "$out" | head -5
if echo "$out" | grep -qiE "kvm (init|version)|Could not access KVM"; then
  echo "RESULT: KVM OPENED (message above is about the bogus -kernel, not KVM)"
elif echo "$out" | grep -qi "permission denied"; then
  echo "RESULT: /dev/kvm EXISTS BUT NOT PERMITTED"
else
  echo "RESULT: see message above"
fi
