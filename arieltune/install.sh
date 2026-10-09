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
#   * `snapshot`  -> arieltune, the retired bc-250-snapshotter shell tool's name. The
#     suite's `snapshot` namespace serves it, and `snapshot list --ids` stays
#     machine-compatible because `arieltune apu` parses it. A shell tool found there is
#     backed up to /var/tmp before the symlink replaces it.
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

# The retired shell tool lives at this name; keep a copy of whatever is there before
# the symlink takes the name over. Its repo keeps the source, but a box may never have
# seen the repo.
if [ -e "$PREFIX/bin/snapshot" ] && [ ! -L "$PREFIX/bin/snapshot" ]; then
  RETIRED="/var/tmp/snapshot-retired-$(date +%Y%m%d)"
  echo ">> backing up the shell tool: $PREFIX/bin/snapshot -> $RETIRED"
  sudo cp -p "$PREFIX/bin/snapshot" "$RETIRED"
fi

echo ">> symlinks: at + compat (aputune/memtune/biostune/wikitune/snaptune/snapshot)"
sudo ln -sf arieltune "$PREFIX/bin/at"
for name in aputune memtune biostune wikitune snaptune snapshot; do
  sudo ln -sf arieltune "$PREFIX/bin/$name"
done

echo ">> bash completion (snaptune + snapshot): /etc/bash_completion.d/snaptune"
sudo install -d /etc/bash_completion.d
# The retired shell tool's completion completes by calling the tool it belongs to;
# this file covers the name instead.
sudo rm -f /etc/bash_completion.d/snapshot
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
