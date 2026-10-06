#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only
# Build the BC-250-patched nct6687 fan-control driver and install it on a
# BC-250 carrier board. Writable PWM fan control (the in-kernel nct6683 is
# read-only; the BC-250 EC ignores its FAN_CFG-handshake writes).
#
# The BC-250 CPU is x86-64-v3 but CachyOS host build tools are x86-64-v4, so
# the kernel module CANNOT be built on the board itself (fixdep aborts with
# "CPU ISA level is lower than required"). Build on an x86-64-v4 host (a modern
# x86-64 build host) against the board's kernel build tree, then copy the .ko over.
#
# Usage:
#   On the board:   ./build-and-install.sh install <path-to-nct6687.ko>
#   On a v4 host:   ./build-and-install.sh build <kernel-build-tree> <upstream-nct6687d-src>
set -euo pipefail
UPSTREAM=https://github.com/Fred78290/nct6687d.git
UPSTREAM_COMMIT=cd735225a95e04dda3e2befd94ba77e1f7609dcc
HERE=$(cd "$(dirname "$0")" && pwd)

case "${1:-}" in
build)
  KBUILD=${2:?kernel build tree, e.g. /path/to/kbuild}
  SRC=${3:-/tmp/nct6687d}
  [ -d "$SRC/.git" ] || git clone "$UPSTREAM" "$SRC"
  git -C "$SRC" checkout "$UPSTREAM_COMMIT"
  git -C "$SRC" apply "$HERE/0001-nct6687-bc250-ec-firmware-attach.patch"
  git -C "$SRC" apply "$HERE/0002-nct6687-silence-secondary-port-open-bus.patch"
  # The board's linux-headers package may ship a trimmed tree missing
  # non-x86 arch Kconfigs; stub them so syncconfig can generate autoconf.h.
  for i in $(seq 1 40); do
    m=$(make -C "$KBUILD" syncconfig 2>&1 | sed -n 's/.*can.t open file "\([^"]*\)".*/\1/p' | head -1)
    [ -z "$m" ] && break
    mkdir -p "$KBUILD/$(dirname "$m")"; : > "$KBUILD/$m"
  done
  # A clang-built kernel's tree carries clang-only flags, so pass LLVM=1 to match
  # it; a gcc-built kernel (the CachyOS default — CONFIG_CC_IS_GCC=y) needs no
  # LLVM args and builds as before. Detected from the tree rather than assumed:
  # the board's shipped kernel is gcc-built, but a clang-built tree still works
  # here without editing this script. Say which one was picked — assuming the
  # answer is what produced the "the board's kernel is clang-built" myth in the
  # first place.
  LLVM_ARG=
  if [ -e "$KBUILD/include/config/CC_IS_CLANG" ]; then
    LLVM_ARG="LLVM=1"
    echo "kernel toolchain: clang  (the tree has CC_IS_CLANG, passing LLVM=1)"
  else
    echo "kernel toolchain: gcc    (no CC_IS_CLANG in the tree, no LLVM flags)"
  fi
  make -C "$KBUILD" M="$SRC" $LLVM_ARG modules
  echo "built: $SRC/nct6687.ko  (copy to the board and run: $0 install nct6687.ko)"
  ;;
install)
  KO=${2:?path to prebuilt nct6687.ko}
  K=$(uname -r)
  sudo install -Dm644 "$KO" "/lib/modules/$K/updates/nct6687.ko"
  sudo depmod -a
  printf 'blacklist nct6683\noptions nct6687 force=true\n' | sudo tee /etc/modprobe.d/bc250-nct6687.conf >/dev/null
  echo nct6687 | sudo tee /etc/modules-load.d/bc250-nct6687.conf >/dev/null
  sudo rm -f /etc/modprobe.d/nct6683.conf /etc/modules-load.d/nct6683.conf 2>/dev/null || true
  lsmod | grep -q nct6687 && sudo rmmod nct6687 || true
  lsmod | grep -q nct6683 && sudo rmmod nct6683 || true
  sudo modprobe nct6687
  echo "installed + loaded. verify: sensors | grep -A3 nct6686"
  ;;
*)
  echo "usage: $0 build <kbuild-tree> [upstream-src] | install <nct6687.ko>"; exit 1;;
esac
