#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only
# Project Ariel — convenience installer.
#
# Forwards to arieltune/install.sh so you can build + install straight from the
# repo root. All arguments are passed through, e.g.:
#
#   ./install.sh                 build (release) + install to /usr/local/bin
#   ./install.sh --with-units    print how to lay + enable the APU GPU power unit
#   ./install.sh --with-driver   also build the BIOS smiflash DKMS driver
#
# Needs a Rust toolchain (cargo) to build and sudo to install. See README.md.
set -euo pipefail
exec "$(dirname "$0")/arieltune/install.sh" "$@"
