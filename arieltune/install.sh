#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only
# Build and install arieltune -- the unified BC-250 tuning suite (WIKI|BIOS|APU|MEM).
#
#   ./install.sh                 build (release) + install to /usr/local/bin
#   ./install.sh --with-units    print how to lay + enable the APU GPU power unit
#   ./install.sh --with-driver   also build+install the BIOS smiflash DKMS driver
#
# Installs ONE binary (`arieltune`) plus:
#   * `at`        short alias  -> arieltune
#   * `aputune` `memtune` `biostune` `wikitune` `snaptune`  compat symlinks -> arieltune
#     (argv[0] dispatch keeps old commands + any systemd units that call them working)
#
# `snapshot` is deliberately NOT among them: that name belongs to the
# bc-250-snapshotter shell tool until it is retired, and shadowing it would replace the
# tool being replaced. The dispatch already understands the name, so the cutover is a
# symlink and nothing else.
#
# Also installs bash completion for `snaptune` (ids + verbs), from
# completions/snaptune.bash -> /etc/bash_completion.d/.
#
# Needs: a Rust toolchain (cargo) to build, sudo to install.
set -euo pipefail
cd "$(dirname "$0")"

PREFIX="${PREFIX:-/usr/local}"
BIN="$PREFIX/bin/arieltune"
WITH_UNITS=0
WITH_DRIVER=0
for a in "$@"; do
  case "$a" in
    --with-units)  WITH_UNITS=1 ;;
    --with-driver) WITH_DRIVER=1 ;;
    -h|--help) sed -n '2,14p' "$0"; exit 0 ;;
    *) echo "unknown arg: $a" >&2; exit 2 ;;
  esac
done

command -v cargo >/dev/null 2>&1 || {
  echo "error: cargo (Rust toolchain) not found -- install Rust from https://rustup.rs" >&2
  exit 1
}

echo ">> building release binary..."
cargo build --release --locked

echo ">> installing $BIN (sudo)..."
sudo install -d "$PREFIX/bin"
sudo install -m755 target/release/arieltune "$BIN"

echo ">> symlinks: at + compat (aputune/memtune/biostune/wikitune/snaptune)"
sudo ln -sf arieltune "$PREFIX/bin/at"
for name in aputune memtune biostune wikitune snaptune; do
  sudo ln -sf arieltune "$PREFIX/bin/$name"
done

echo ">> bash completion (snaptune): /etc/bash_completion.d/snaptune"
sudo install -d /etc/bash_completion.d
sudo install -m644 completions/snaptune.bash /etc/bash_completion.d/snaptune

echo ">> installed: $("$BIN" --version)"

if [ "$WITH_UNITS" = 1 ]; then
  # The unit is not a file this script copies: choosing a power mode writes and
  # enables it (arieltune-gpu.service, driven by the persisted power.json), so
  # there is nothing to lay until a mode is picked. Say that instead of
  # implying the flag installed something.
  echo ">> APU GPU power unit: the mode command writes + enables it, so pick one:"
  echo "     sudo arieltune apu gpu governor-on | autosleep-on | force <mhz>"
  echo "   optional CU-routing unit:"
  echo "     sudo arieltune apu cu route-save && sudo systemctl enable arieltune-route"
fi
if [ "$WITH_DRIVER" = 1 ]; then
  echo ">> building the BIOS smiflash DKMS driver (on-board)..."
  sudo "$BIN" bios driver build || echo "   (driver build needs a BC-250 + kernel headers)"
fi

echo
echo "Done. Launch the TUI:        arieltune            (opens on WIKI)"
echo "  jump straight to a tab:    arieltune apu        (or bios | mem | wiki)"
echo "  per-app CLI (unchanged):   arieltune apu gpu apply-boot   == aputune gpu apply-boot"
echo "  migrate an old box:        sudo arieltune migrate --apply"
