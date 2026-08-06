# BC-250 amdgpu liberation patches (curated)

The kernel-patch series aputune embeds and builds into the system. Authored on
`linux-cachyos-bore-7.0.2`; the amdgpu/SMU source files are structurally
identical through `7.0.9`, so the same patches apply unchanged. Applied via the
`makepkg` flow (driven by `aputune build`), not `make M=...`.

**Kernel: pin to `linux-cachyos-bore-7.0.9`** (the current known-good target).
Do **not** build against `7.0.11+` yet - those kernels regress the BC-250 SDMA
path. The folder is named for the validated kernel (`bc250-cachyos-7.0.9`).

This set is **curated for portability** - only patches that are safe and useful
on any BC-250 ship, numbered **01-19**. Every one except `12` is applied, in the
order listed. `12` is Studebaker's Vulkan-only CU unlock, kept in-tree as an
alternate to `16` but not built. See "Excluded" below for what was deliberately
dropped.

## Patch list (18 applied, plus `12` on disk as an alternate)

| # | Source | Purpose |
|---|---|---|
| `01` | `smu_types.h` | Declare the new `SMU_MSG_*` enum values the msg map needs |
| `02` | `cyan_skillfish_ppt.c` | Map 23 msgids (11->34); raise `CYAN_SKILLFISH_SCLK_MAX` 2000->2500 |
| `03` | `cyan_skillfish_ppt.c` | `set_performance_level` + `ForceGfxFreq`/`UnForceGfxFreq` |
| `04` | `cyan_skillfish_ppt.c` | `StartTelemetryReporting` so `SmuMetrics_t` populates (temp) |
| `05` | `cyan_skillfish_ppt.c` | GFXCLK sensor reads direct `QueryGfxclk` (metrics path races) |
| `06` | `cyan_skillfish_ppt.c` | CAC weight read helper (dep of the read-only CAC nodes) |
| `07` | `cyan_skillfish_ppt.c`, `amdgpu_smu.c`, `smu_cmn.h` | Read-only `*_cac_weight` debugfs + the `smu_send_raw` foundation |
| `08` | `smu_cmn.c` | `smu_cmn_send_raw` definitions + `amdgpu_smu_send_raw` node |
| `09` | `cyan_skillfish_ppt.c` | `cclk_soft_min/max` debugfs (CPU clock control) |
| `10` | `cyan_skillfish_ppt.c` | CAC print widened to 32-bit (correct CAC-node output) |
| `11` | `cyan_skillfish_ppt.c` | `cyan_skillfish_telemetry` node (clocks/pstates/voltages) |
| `12` | `gfx_v10_0.c` | *(on disk, NOT applied)* Studebaker's CU unlock: CC + SPI(0x1F) + RLC(0x1F) → all 40 CUs (⚠️ Vulkan-only, hangs ROCm/HSA) — superseded by `16` |
| `13` | `gfx_v10_0.c` | **NEW** GFXOFF disabled for gfx1013 — prevents GPU power-state hangs |
| `14` | `gmc_v10_0.c` | **NEW** KIQ bypass + dead-GPU detection in gmc_v10_0 TLB flush (5 sub-patches) |
| `15` | `amdgpu_gmc.c` | **NEW** KIQ bypass + dead-GPU detection in centralized GMC code (2 sub-patches) |
| `16` | `gfx_v10_0.c` | **NEW** BC-250 40 CU unlock — CC+SPI only, NO RLC_PG (safe for ROCm+HSA) |
| `17` | `gmc_v10_0.c` | **NEW** gfx1013 instruction-fetch fault probe — diagnostic, report-only |
| `18` | `amdgpu_ttm.c` | **NEW** Guard NULL `ttm->pages[]` on unpopulate — survive compute faults |

## What aputune does with them

- **GPU stability** (13/14/15): Three-layer protection against BC-250 GPU hangs.
  - Layer 1 (13): GFXOFF disabled for gfx1013 — prevents GPU entering unrecoverable power state.
  - Layer 2a (14): gmc_v10_0 KIQ bypass + dead-GPU detection — 5 sub-patches protecting TLB flush.
  - Layer 2b (15): amdgpu_gmc KIQ bypass + dead-GPU detection — centralized GMC protection.
  Together these eliminate the infamous BC-250 KIQ hang and provide graceful
  recovery when the GPU becomes unreachable (0xFFFFFFFF MMIO reads).
- **40-CU unlock** (16): `16` is the one the build applies. Studebaker's `12`
  stays on disk as the Vulkan-only alternate — see the comparison below.
- **Clock control** (01/02/03 + 07/08): `ForceGfxFreq` via the race-free
  `amdgpu_smu_send_raw` node - GPU `force`/`wake`/`deep-sleep`/`autosleep`.
- **CPU cclk** (09): soft min/max.
- **Telemetry** (04/05/11): live clocks, temp, voltages.
- **SDMA reliability** (19): Steers user SDMA queues off engine 0 to engine 1 (lost completion IRQ). Controlled by amdgpu.bc250_skip_sdma0=1. By Gabriel Duarte Guerra.

### CU unlock: patch 12 vs patch 16

`12` is Studebaker's original 40-CU unlock, written for Vulkan/RADV. `16` is the
ROCm-safe derivative, and it is the one `apu build` applies.

| Aspect | Patch 12 (Studebaker, Vulkan) | Patch 16 (applied) |
|---|---|---|
| Registers | CC + SPI + **RLC** | CC + SPI only |
| RLC_PG_ALWAYS_ON_WGP_MASK | Written (mode 3) | **NOT written** |
| Vulkan/RADV | ✅ Works | ✅ Works |
| ROCm/HSA (rocBLAS, HIP) | ❌ Hangs on first KFD queue | ✅ Works |
| Module param | `bc250_cc_write_mode=3` | `bc250_cc_write_mode=3` |
| Probe modes | 1 (SE0 only), 4 (all SAs) | 1 (SE0 only), 4 (all SAs) |

**Why no RLC write?** Writing RLC_PG_ALWAYS_ON_WGP_MASK while RLC firmware is
running triggers the WGP bring-up state machine for harvested WGPs 3-4 whose
handshake registers are uninitialized. The RLC stalls waiting for an ACK that
never arrives → any subsequent KFD queue operation hangs. BC-250 already has
RLC_PG_CNTL=0 (PG globally off via ppfeaturemask), so the RLC write is
redundant.

**Which ships:** `16`, because it covers both Vulkan and ROCm. Studebaker's `12`
stays on disk for Vulkan-only setups — swap it in by pointing the `16` registry
entry in `crates/apu/src/patches.rs` at `12-unlock-all-40-compute-units.patch`.
Do NOT use `12` if any ROCm/HSA workload is planned.

## GPU stability architecture (patches 13-15)

The BC-250 has a known hardware issue: when the GPU enters certain power states
(GFXOFF) or paths that require firmware coordination (KIQ ring), it can become
unresponsive. Because the BC-250's internal PCIe fabric lacks completion
timeout, any MMIO read to a dead GPU hangs the CPU indefinitely.

The three-layer protection:

```
Layer 1 (patch 13): Prevent entry into dangerous power states
  └─ gfx_v10_0_check_gfxoff_flag() → disable GFXOFF for gfx1013

Layer 2 (patches 14+15): Bypass dangerous code paths + detect dead GPU
  ├─ gmc_v10_0_flush_gpu_tlb() → goto use_mmio for gfx10.1.x
  ├─ amdgpu_gmc_flush_gpu_tlb_pasid() → direct MMIO callout
  ├─ amdgpu_gmc_fw_reg_write_reg_wait() → direct MMIO poll loop
  └─ All paths: pre-read health check + 0xFFFFFFFF dead-GPU detection

Layer 3 (patch 19): Steer user DMA away from broken SDMA0. Clear even-numbered SDMA queue bits so user queues use engine 1 only. amdgpu.bc250_skip_sdma0=1 (default off, A/B-testable).
```

All three layers are gated on `IP_VERSION(10, 1, x)` so they are no-ops on
non-BC-250 hardware.

## The open compute defect (patches 17-18)

ROCm inference is stable on this part. What is not stable is any workload with a
second heavy GPU phase — a training backward pass, an img2img reverse pass,
multi-step generation. The first phase completes; the next one dies.

When it surfaces as a page fault rather than a HIP error, the fault is always an
`SQC (inst)` read — the shader *instruction* cache. The wave's PC was already
wrong before the fault was raised, so the fault handler is reporting the
symptom, not causing it.

Seven of nine faults collected across separate boots with ASLR active share an
exact shape: top byte `0xff`, low 20 bits `0xbb000`, only bits [39:20] moving.
The process's own code objects live at `0x7f...`. `0x7f` -> `0xff` is one bit —
bit 47, the canonical-form boundary — and the top byte of a 48-bit shader
address is exactly `COMPUTE_PGM_HI`, which is 8 bits wide and carries bits
[47:40]. So this is one wrong bit in a otherwise plausible address.

- **`17` is the measurement.** It logs the raw interrupt vector and the
  bit-47-cleared candidate so the candidate can be checked against the faulting
  process's GPU VA map. It changes no control flow and fixes nothing. Gate with
  `amdgpu.bc250_fault_probe=0`.
- **`18` is containment.** Compute faults on this part were escalating to kernel
  panics via a missing NULL check in TTM cleanup, which made the defect
  expensive to study — you lose the blade instead of getting a log. With the
  guard the process dies and the machine survives.

Neither patch claims a fix. They exist so the defect can be isolated. Deliberately
*not* shipped: fault-handler retry, address rewriting, warmup dispatches, and
`AMD_SERIALIZE_KERNEL`-style throttling — those change timing or hide the symptom
without addressing why bit 47 is set.

## Excluded (deliberately not shipped)

Patches that exist in the research tree but are left out of the curated series:

- **UMC wiring** - only relevant to the memory controller; that's memtune's
  domain, not aputune.
- **Power-brake / DiDT throttle** - experimental BAPM stall tuning; "MGCG
  deadlocks compute". Research-only, risky on other boards.
- **In-kernel SW-DPM ladder** - the firmware exposes no usable GPU load signal
  (released clock is a flat ~1500 MHz idle/light-load), so an in-kernel auto-DPM
  can't drive a ladder reliably. aputune does power **app-driven** instead
  (`gpu autosleep`).
- **Raw-msgid debugfs** - exposes raw SMU pokes (reset/pstate/VMID); a footgun.
  aputune uses the specific msgids it needs via `smu_send_raw`.

## Build

`aputune build` materializes these patches, runs the `makepkg` flow (CC=gcc-15),
installs the package, arms 40-CU via modprobe.d, rebuilds initramfs, and (with
`--target user@host`) deploys + reboots. Verified end-to-end on a real BC-250
running `linux-cachyos-bore-7.0.9`.
| `19` | `kfd_device_queue_manager.c` | **NEW** BC-250 SDMA0 skip — restrict user queues to SDMA1 (SDMA0 completion IRQ is lost at boot) |
