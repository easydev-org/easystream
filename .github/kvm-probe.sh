#!/usr/bin/env bash
set -uo pipefail
echo "=========== runner: ${1:-?} ==========="
ls -la /dev/kvm 2>&1 || echo NO_KVM
grep -m1 -oE "(vmx|svm)" /proc/cpuinfo || true
for m in kvm_intel kvm_amd; do p=/sys/module/$m/parameters/nested; [ -e $p ] && echo "$m nested=$(cat $p)"; done

echo "--- open /dev/kvm to all (the usual CI snippet) ---"
echo 'KERNEL=="kvm", GROUP="kvm", MODE="0666", OPTIONS+="device_node"' | sudo tee /etc/udev/rules.d/99-kvm4all.rules >/dev/null
sudo udevadm control --reload-rules && sudo udevadm trigger --name-match=kvm
ls -la /dev/kvm

sudo apt-get update -qq >/dev/null 2>&1
sudo apt-get install -y -qq qemu-system-x86 >/dev/null 2>&1

# Definitive test, no boot media needed: ask QEMU to query the KVM version.
# That call only succeeds if /dev/kvm opened AND the kernel KVM module
# initialised, so it isolates exactly the thing under test.
echo "--- qemu KVM version query ---"
v=$(qemu-system-x86_64 -accel kvm -machine none -display none -nodefaults -S -qmp stdio <<< '{"execute":"qmp_capabilities"}{"execute":"query-kvm"}{"execute":"quit"}' 2>&1 || true)
echo "$v" | head -6
echo
if echo "$v" | grep -q ""enabled": true"; then
  echo "RESULT: KVM ENABLED AND WORKING"
elif echo "$v" | grep -qiE "permission denied|could not access|failed to initialize"; then
  echo "RESULT: KVM PRESENT BUT REFUSED"
else
  echo "RESULT: inconclusive, read the output above"
fi
