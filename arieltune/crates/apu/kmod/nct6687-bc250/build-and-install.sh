#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only
# Build the BC-250-patched nct6687 fan-control driver and install it on a
# BC-250 carrier board. Writable PWM fan control (the in-kernel nct6683 is
# read-only; the BC-250 EC ignores its FAN_CFG-handshake writes).
#
# This builds ON THE BOARD. An earlier version of this header claimed the
# opposite, on the grounds that the board's CPU is x86-64-v3 while CachyOS
# build tools are x86-64-v4. That describes the CachyOS *linux-headers package*,
# not the board: it ships fixdep/modpost/objtool compiled -march=x86-64-v4, so
# glibc refuses to start them on the Zen2 ("CPU ISA level is lower than
# required"). The tools only use baseline instructions, so stripping the
# advisory ISA note fixes it — and a locally built headers package carries no
# such note at all. `build` applies the shared tree prep (which is a property of
# the tree, not of this driver) before compiling.
#
# Building on a separate x86-64-v4 host still works if you prefer: prepare that
# host's tree, compile there, and copy the .ko over.
#
# Usage:
#   On the board:   ./build-and-install.sh build                 # running kernel
#                   ./build-and-install.sh install <path-to-nct6687.ko>
#   Off-board:      ./build-and-install.sh build <other-kbuild> [upstream-src]
set -euo pipefail
UPSTREAM=https://github.com/Fred78290/nct6687d.git
UPSTREAM_COMMIT=cd735225a95e04dda3e2befd94ba77e1f7609dcc
HERE=$(cd "$(dirname "$0")" && pwd)
# Shared kernel-tree prep, used by the smiflash driver too — both problems it
# fixes (the x86-64-v4 ISA note on the headers package's host tools, and the
# missing autoconf.h) belong to the tree rather than to either driver, so it
# lives in one place instead of being copied.
TREE_PREP="$HERE/../../../bios/driver/prepare.sh"

case "${1:-}" in
build)
  KBUILD=${2:-/lib/modules/$(uname -r)/build}
  SRC=${3:-/var/tmp/nct6687d}
  [ -d "$KBUILD" ] || { echo "no kernel build tree at $KBUILD" >&2; exit 1; }
  # Prepare the tree before anything compiles in it.
  [ -x "$TREE_PREP" ] || [ -f "$TREE_PREP" ] || {
    echo "tree prep not found at $TREE_PREP" >&2; exit 1; }
  sh "$TREE_PREP" "$(basename "$KBUILD")" "$KBUILD"
  [ -d "$SRC/.git" ] || git clone "$UPSTREAM" "$SRC"
  git -C "$SRC" checkout -q "$UPSTREAM_COMMIT"
  # Force the checkout back to pristine upstream before patching, so re-running
  # `build` does not die with "patch does not apply" against a tree that already
  # carries them. `checkout -- .` is not sufficient: a previously applied patch
  # can be staged, and checkout restores from the index, not from HEAD. SRC is
  # treated as a scratch build directory — local edits in it are discarded here.
  git -C "$SRC" reset -q --hard "$UPSTREAM_COMMIT"
  git -C "$SRC" clean -qfd
  git -C "$SRC" apply "$HERE/0001-nct6687-bc250-ec-firmware-attach.patch"
  git -C "$SRC" apply "$HERE/0002-nct6687-silence-secondary-port-open-bus.patch"
  # The board's linux-headers package may ship a trimmed tree and omits
  # autoconf.h; the shared tree prep above has already stubbed the missing
  # Kconfig sources and run syncconfig, so nothing to repeat here.
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
