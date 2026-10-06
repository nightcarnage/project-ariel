#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only
# Kernel-tree prep — make a stripped/incomplete kernel-headers tree buildable
# on a BC-250, so an out-of-tree module compiles ON THE BOARD with no
# cross-build host.
#
# This is the smiflash DKMS PRE_BUILD hook (and is reused by
# `arieltune bios driver build`). The nct6687 fan driver's build calls it too —
# both problems it fixes are properties of the *tree*, not of any driver.
# It is idempotent and non-destructive: on a complete headers tree (most distros)
# every step is a no-op and a normal module build proceeds.
#
# Two problems it fixes, both seen on ASRock BC-250 + CachyOS:
#   1. The prebuilt host tools (fixdep/modpost/objtool/…) are compiled
#      -march=x86-64-v4, so their ELF carries a "x86 ISA needed: v4" note and
#      glibc refuses to start them on the Zen2 (no AVX-512) BC-250 CPU
#      ("CPU ISA level is lower than required"). But they only *use* baseline
#      instructions — stripping the advisory note lets them run unchanged.
#      Note this is the *package's* doing, not the board's limit: a locally
#      built headers package carries no such note and needs none of this.
#   2. include/generated/autoconf.h is missing (the headers package never ran
#      modules_prepare) and crypto/Kconfig sources arch Kconfig files the package
#      stripped. We stub the missing sources and run syncconfig to generate it.
set -eu

# Arguments: [KVER] [TREE]
#   KVER  kernel release whose /lib/modules/<KVER>/build is to be prepared
#         (default: the running kernel)
#   TREE  explicit build tree, instead of /lib/modules/<KVER>/build — lets a
#         caller prepare a tree that is not installed under /lib/modules, such
#         as one rsync'd over for an off-board build
KVER="${1:-$(uname -r)}"
KDIR="${2:-/lib/modules/$KVER/build}"
[ -d "$KDIR" ] || { echo "kernel-tree prep: no kernel build tree at $KDIR" >&2; exit 1; }

# 1. ISA-note strip — only when this CPU lacks AVX-512 (so we don't touch a
#    machine whose tools already run). Removing the note is harmless: it drops an
#    advisory ELF note, not any code.
if ! grep -qm1 avx512f /proc/cpuinfo 2>/dev/null \
   && command -v objcopy >/dev/null 2>&1 && command -v readelf >/dev/null 2>&1; then
	find "$KDIR/scripts" "$KDIR/tools" -type f -perm -u+x 2>/dev/null | while IFS= read -r f; do
		if readelf -n "$f" 2>/dev/null | grep -q 'x86 ISA needed'; then
			objcopy --remove-section .note.gnu.property "$f" "$f.nn" 2>/dev/null \
				&& mv "$f.nn" "$f" && chmod +x "$f" || rm -f "$f.nn"
		fi
	done
fi

# 2. Generate autoconf.h if the headers package omitted it. Stub every Kconfig
#    source the tree references but the package stripped, then syncconfig.
if [ ! -f "$KDIR/include/generated/autoconf.h" ]; then
	cd "$KDIR"
	i=0
	while [ "$i" -lt 40 ]; do
		i=$((i + 1))
		if out="$(make syncconfig 2>&1)"; then break; fi
		miss="$(printf '%s' "$out" | grep -oE "can.t open file \"[^\"]+\"" | sed -E 's/.*"(.*)"/\1/' | head -1)"
		[ -z "$miss" ] && { printf '%s\n' "$out" | tail -3 >&2; break; }
		mkdir -p "$(dirname "$miss")"
		: > "$miss"
	done
fi

echo "kernel-tree prep: tree ready for $KVER"
