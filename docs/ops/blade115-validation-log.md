# Blade 115 — 8-core unlock validation & OOT driver fixes log (2026-08-18)

Internal ops record. Fork-only by design: this file is NOT part of any upstream
PR branch (the PR-worthy design handoff is `arieltune-core.md`). Keep it
appended as the fleet work continues.

## 1. Full from-scratch build on blade 115 (2026-08-18, the cachenetics PR dry run)

`arieltune apu build --pkgbuild /mnt/build-ssd/ariel-p28-pkgbuild/linux-cachyos
--run` executed the complete production flow on blade 115:

- **Materialize**: staged pkgbuild (PKGBUILD + cachyos-7.0.9-1.tar.gz) on the
  USB-SSD; `makepkg -o` extract, all 28 SERIES patches applied `--fuzz=0`
  clean on first pass (build log: `/mnt/build-ssd/ariel-p28-build.log`).
- **Build**: root privilege-drop to the PKGBUILD owner (kbuild `Step.uid`),
  `CC=gcc-15`; `gfx_v10_0.o` and `gmc_v10_0.o` (the patched files) compiled
  clean; only two pre-existing warnings from the patch-22 KFD runlist code.
- **Packages**: `linux-cachyos-7.0.9-1-x86_64.pkg.tar.zst` (156 MB) +
  `linux-cachyos-headers` (39 MB) in
  `/mnt/build-ssd/ariel-p28-pkgbuild/linux-cachyos/`.
- **Install**: `pacman -U` into the real (unbound) module tree, 40-CU
  modprobe conf, mkinitcpio for `7.0.9-1-cachyos`, GRUB update.
- **Verified**: `modinfo` on the installed `amdgpu.ko.zst` exposes
  `cs_eight_core_map` plus `bc250_cc_write_mode` / `bc250_flush_by_runlist` /
  `bc250_skip_sdma0` / `bc250_fault_probe`.
- **GRUB**: new `[ariel-p28]` menuentry in `/etc/grub.d/40_custom` (vmlinuz +
  initramfs from `/boot`, running cmdline minus `modtree=build2`),
  one-shot armed via `grub-reboot ariel-p28`.
- **Cleanups done**: stale `linux-cachyos-6.19.9.preset` (pointed at deleted
  `/boot/vmlinuz-snap-32849855`) removed so `mkinitcpio -P` no longer errors;
  `10_linux` left non-executable (the blade keeps its snapshot menu via
  `40_custom`).

Post-boot checklist: `arieltune apu doctor --verify`, confirm
`cs_eight_core_map` present, then `snapshot register` (parent snap-8fe794e8,
check for duplicate snapshot id in GRUB before registering — see
`/memories/repo/bc250-snapshot-tool-hazards.md`).

**2026-08-18 reboot + registration result**: booted `[ariel-p28]`
(`uname -r` = `7.0.9-1-cachyos`, no `modtree`); doctor green — liberation
series 24/24 live, 40/40 CUs, GPU manual pin 1500 MHz;
amdgpu params `cs_eight_core_map=N` (auto-detect) `bc250_cc_write_mode=3`
`bc250_skip_sdma0=1` `bc250_flush_by_runlist=1`; running module srcversion
`B5A99260FDE859308CE1437`. Snapshot registered as **`snap-59cee34a`**
(parent `snap-8fe794e8`, GRUB entry added, NOT default; no duplicate ids).

## 2. Forced 8-core unlock from an abnormal mask (blade 115, 2026-08-18)

Blade 115's live core mask was **0xD7** (`enabled=[0 1 2 4 6 7] disabled=[3 5]`,
state ABNORMAL) — stock 0x77 with bit 6 set, i.e. cores 3+5 masked but core 6
present. The safety gate refused it by design. Per explicit approval, the gate
now has an escape hatch:

- **Code**: `OcQ3::unlock_cores_any()` in `crates/ariel-smu/src/ocq3.rs` —
  skips the 0x77 check but keeps the same all-or-nothing q3 0x98 write and
  the 0xFF readback verify. Wired as `arieltune apu cores apply
  --force-abnormal` (`crates/apu/src/{cli,cores}.rs`) and as the TUI Cores
  panel `[F] force-unlock` key (warning popup + `[y]`/`[esc]` confirm, no
  auto-reboot — `crates/apu/src/screen.rs`). `boot`/unit path unchanged (no
  force at boot, per design). `unlock_cores()` default refusal intact.
  Tests 37/37 green.
- **Result on blade 115**: `apply --force-abnormal --reboot` wrote and
  verified 0xFF; warm reboot into pinned `snap-59cee34a` came back clean —
  mask `0xFF`, all 8 cores / 16 threads visible, 0 offline, 0 MCE entries,
  boot unit NOT installed (user: "no service"). The blade booted fine; no
  lockout.
- **Knowns**: warm-reboot semantics unchanged (cold boot reverts to the
  board's own mask — likely 0xD7 again on this blade); the 2 extra cores
  change the SoC power/thermal envelope; 8-core ACPI SSDTs were NOT
  installed, so threads 12-15 have no C-state tables (stock stops at C00B).

## 3. Full validation pass + OOT driver fixes (blade 115, 2026-08-18)

After the forced unlock, a complete validation + cleanup pass landed. Everything
below is LIVE on blade 115 (kernel `7.0.9-1-cachyos`, pinned `snap-59cee34a`).

**8-core unlock — verified from every angle**
- `SMN 0x115A870 = 0xFF`, state UNLOCKED, `lscpu` 16 CPUs on-line, 8C×2T.
- New threads 12-15 compute correctly pinned via `taskset` (sum(0..99999)
  each = 4999950000). `arieltune apu cores verify` swept all 8 cores (one
  thread per physical core by design) twice: no failures, 0 MCE.
- Boot unit `aputune-cores.service` installed + enabled; verified running at
  boot (exit 0, idempotent "already unlocked"). The mask even survived a
  `reboot/mode=cold` reset (0xFF persisted) — the board kept the unlock.
- Pin state: `snapshot current` = `snap-59cee34a`, GRUB Default =
  `snap-59cee34a`, one-shot none.

**nct6687 sensor driver (out-of-tree, extra/nct6687.ko.zst)**
- Config consolidated: `/etc/modprobe.d/sensors.conf` removed; single
  `bc250-nct6687.conf` (`blacklist nct6683` + `options nct6687 force=true`).
- Quirk fixed: the `force=1 refused: chip ID 0xffff` boot line came from the
  SECONDARY SIO port (0x4e) open-bus fall-through, not a real refusal. New
  repo patch `0002-nct6687-silence-secondary-port-open-bus.patch` (kmod dir)
  makes it a silent skip. Rebuilt ON the blade with `LLVM=1`
  (srcversion `2D4B6235D20CD0BD87ABE3A`).
- Initramfs trap found & fixed: mkinitcpio `MODULES=(nct6687 nct6687 nct6687)`
  embedded the OLD module and loaded it pre-root (initramfs
  systemd-modules-load), which is why the fixed binary still showed the
  refusal at boot. Removed from `MODULES`, rebuilt the preset initramfs, and
  synced the snapshot initrd (`initramfs-snap-59cee34a.img` — identical copy
  of the preset image). Boot log is now clean: probe at real root, no
  refusal line. NOTE: swapping the initrd in place makes the snapshot
  register-id (`md5(module)+md5(initramfs)`) stale bookkeeping-wise; the pin
  still works (GRUB references by path). Clean fix if desired: re-register +
  re-pin.

**smiflash SMM SPI-flash driver (out-of-tree, updates/dkms/smiflash.ko.zst)**
- It is Studebaker's (Cachenetics, initial Project Ariel commit `64d9ca8`,
  `crates/bios/driver/`). Now built the CORRECT way: `dkms` installed on the
  blade, module `Makefile` fixed to auto-detect a clang-built kernel via the
  `CC_IS_CLANG` marker and pass `LLVM=1` (gcc kernels unaffected) —
  commit `d68a980`. `dkms add/build/install` → `updates/dkms/`, AUTOINSTALL
  survives kernel upgrades. Old loose `extra/smiflash.ko.zst` removed.
- Loads at boot (`smi_port=0xb0` from FADT), `/proc/smiflash` live,
  `arieltune bios driver status` = loaded [ok].

**Head display autostart**
- getty@tty1 autologin + `~/.bash_profile` tty1/non-SSH guard now launches
  `sudo -n arieltune apu` (the APU TUI) on the framebuffer head instead of
  nvtop. Crash-restart loop kept (restart if exit < 5 s). Backup:
  `~/.bash_profile.bak-nvtop`.

**Final boot-log review**
- Failed systemd units: 0. MCE entries: 0. nct6687/smiflash clean.
- Remaining known log noise (none functional):
  - 16× `ACPI BIOS Error: Could not resolve symbol [\_PR.C000..C00F]` — the
    stock BIOS ACPI gap; fixed by `arieltune apu cores acpi install`
    (8-core SSDT-CST/PST override), NOT installed on this blade yet.
  - 1× `Pm2ControlBlock zero Address` ACPI warning — cosmetic AMI BIOS bug.
  - 2× amdgpu DC `dal_irq_service_ack/set` WARNINGs — display-core IRQ noise.
  - Kernel taint: out-of-tree + unsigned (nct6687) + `gpu_recovery`
    dangerous-option — all expected/by design.

## 4. License restructure (2026-08-18)

The fork originally relicensed userspace to Apache-2.0; that is incompatible
with upstream cachenetics' GPL-2.0-only. Reverted: whole project GPL-2.0-only,
kernel subtrees GPL-2.0, `THIRD_PARTY_NOTICES` added (GabriWar MIT ×2,
amd-bc250-docs CC BY-SA 4.0/MIT, Fred78290/nct6687d GPL-2.0, Fabian slot
PENDING his grant for patches 13/14/15). Fork commit `4d92e17`.

## 5. TUI autostart switch + misc (same day)

- tty1 head autostart: nvtop → `sudo -n arieltune apu` (see above).
- `bc250-fan-control.service` crash-loops while the blade sits on the
  the lab net (script detects blade numbers only on the cluster net);
  self-resolves on the hive net — left alone.
- `systemctl halt` confusion (2026-08-18): halt leaves the machine powered on
  by design; `poweroff`/`shutdown` cut power. Not a bug.
