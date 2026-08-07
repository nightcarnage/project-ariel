# BC-250 Production Guide — ROCm + PyTorch on CachyOS 7.0.9

**Date**: 2026-08-07  
**Hardware**: AMD BC-250 (gfx1013 / Cyan Skillfish), 40 CU, 17.2 GB VRAM  
**Kernel**: 7.0.9-cachyos with 22 applied patches (19 baseline + 24)  
**ROCm**: 7.2.4-1 (arch4edu)  
**PyTorch**: 2.12.0a0 custom gfx1013 build  
**Status**: ✅ PRODUCTION READY

---

## Quick Start

```bash
# Required for ALL GPU workloads
export HSA_ENABLE_SDMA=0
export HSA_OVERRIDE_GFX_VERSION=10.1.0   # ← MUST be shell env var, NOT os.environ!

# Test
python3 -c "import torch; print(torch.cuda.is_available())"
```

---

## Active Snapshot

**snap-a0af1eeb** — 19-patch baseline + patch 24 (all-VMID TLB flush)  
Pinned as GRUB default. Survives reboots.

---

## Patch Stack (22 applied, 2 on-disk alternates)

| # | Filename | What it does |
|---|----------|-------------|
| 01-11 | SMU/telemetry/clock patches | PMFW messaging, sensors, debugfs |
| 12 | `12-unlock-all-40-compute-units.patch` | ❌ NOT applied — Vulkan-only |
| 13 | `13-gfxoff-disable-gfx1013.patch` | Disable GFXOFF (prevents power-state hangs) |
| 14 | `14-gmc-kiq-bypass-dead-gpu.patch` | KIQ bypass in GMC TLB flush |
| 15 | `15-amdgpu-gmc-kiq-bypass.patch` | KIQ bypass in centralized GMC |
| 16 | `16-cu-unlock-cc-spi-safe-no-rlc.patch` | 40 CU unlock — ROCm-safe |
| 17 | `17-bc250-gfx1013-fault-probe.patch` | Instruction-fetch fault probe (diagnostic) |
| 18 | `18-ttm-guard-null-pages-on-unpopulate.patch` | NULL guard on TTM populate |
| 19 | `19-bc250-kfd-skip-sdma0.patch` | Skip broken SDMA0 (bc250_skip_sdma0=1) |
| 20-22 | TTM unpopulate + fno-lto | CR2=0x18 panic fix (complement patch 18) |
| 23 | `23-gb-addr-config-num-se.patch` | ❌ NOT applied — regression |
| 24 | `24-gmc-v10-flush-all-vmids.patch` | **All-VMID TLB flush — fixes aliasing bug** |

---

## What Works

| Workload | Recipe | Result |
|----------|--------|--------|
| Basic matmul (fp32/fp16) | `HSA_ENABLE_SDMA=0` | ✅ All sizes 512-4096 |
| Conv2d (MIOpen) | `HSA_ENABLE_SDMA=0` | ✅ 26/26 correct |
| Memory alloc up to 6GB | `HSA_ENABLE_SDMA=0` | ✅ |
| Multi-worker aliasing test | `HSA_ENABLE_SDMA=0` | ✅ 0/4000 corruptions |
| **LoRA training (backward)** | `HSA_ENABLE_SDMA=0 HSA_OVERRIDE_GFX_VERSION=10.1.0` | ✅ distilgpt2, 83ms/step |
| **hipfire LLM inference** | `HSA_ENABLE_SDMA=0 HSA_OVERRIDE_GFX_VERSION=10.1.0` | ✅ 85.7 tok/s (Qwen3.5 4B) |
| magnum bandwidth test | `HSA_ENABLE_SDMA=0 HSA_OVERRIDE_GFX_VERSION=10.1.0` | ✅ 259 GB/s norot |

---

## What Does NOT Work

| Issue | Root Cause | Workaround |
|-------|-----------|------------|
| bf16 | gfx1013 hardware (gfx10.1) predates bfloat16 | Use fp32 or fp16+GradScaler |
| rocBLAS small-rank GEMM | Kernel selection bug on gfx1013 | `HSA_OVERRIDE_GFX_VERSION=10.1.0` |
| LoRA backward without override | rocBLAS picks broken kernel variant | Use the override as shell env var |
| SDMA engine 0 | Lost completion IRQ at boot | `bc250_skip_sdma0=1` (patch 19) |
| GabriWar runlist patch | Regression — breaks hipfire + LoRA | Not applied (on disk for reference) |

---

## Key Technical Findings

### The aliasing bug is fixed by patch 24
Patch 24 flushes ALL mapped VMIDs on every TLB invalidation through the direct
MMIO path (bypassing KIQ which is known to wedge on BC-250). Validated with
4-worker concurrent GPU stress: 0 corruptions across 4000 iterations.

### The LoRA backward hang is a rocBLAS kernel selection bug
The LoRA backward pass creates small-rank GEMMs (rank=8 adapter gradients) that
hit a rocBLAS kernel variant broken on gfx1013. gfx1010 kernels are ISA-identical
and work correctly. The override must be a **shell environment variable** set
before process start — `os.environ` inside Python does NOT work because HIP
initialization already happened.

### The runlist patch causes a regression
GabriWar's `bc250_flush_by_runlist` approach (patch 25) was tested and reverted:
- The chardev.c unmap ioctl hook never fires for PyTorch/hipfire workloads
- Even with the function never called, GPU compute hangs (LoRA backward, hipfire)
- Kept on disk for reference; not applied in production

### PR #8838 (native gfx1013 rocBLAS) is cosmetic
Adding gfx1013 to rocBLAS target lists produces code objects identical to gfx1010
(same ISA). The env var override achieves the same result. Not worth building.

---

## Debugging Tools

| Tool | Location | Purpose |
|------|----------|---------|
| Wavefront scanner | `/tmp/scan_waves_fast.sh` | Detect hung/poisoned waves (640 slots, <2s) |
| Ring error counters | `/sys/kernel/debug/dri/0000:01:00.0/amdgpu_error_*` | CP queue health |
| Fence info | `/sys/kernel/debug/dri/0000:01:00.0/amdgpu_fence_info` | Ring completion status |
| GFXOFF status | `/sys/kernel/debug/dri/0000:01:00.0/amdgpu_gfxoff_status` | Should be empty (patch 13) |
| magnum-test | `/usr/local/bin/magnum-test` | Memory bandwidth benchmark |
| hipfire | `/usr/local/bin/hipfire` | LLM inference + bench + diag |

---

## Build & Deploy

```bash
# Build from USB SSD source
cd /mnt/usb-ssd/cachyos-7.0.9
sudo make M=drivers/gpu/drm/amd/amdgpu modules LLVM=1 -j$(nproc)
sudo strip --strip-debug drivers/gpu/drm/amd/amdgpu/amdgpu.ko
sudo zstd -T0 -19 --rm drivers/gpu/drm/amd/amdgpu/amdgpu.ko -o /tmp/amdgpu.ko.zst
sudo cp /tmp/amdgpu.ko.zst /lib/modules/7.0.9/kernel/drivers/gpu/drm/amd/amdgpu/
sudo mkinitcpio -p linux-cachyos

# Register snapshot
sudo /srv/data/bc-250-snapshotter/snapshot register \
  --module /lib/modules/7.0.9/kernel/drivers/gpu/drm/amd/amdgpu/amdgpu.ko.zst \
  --initramfs /boot/initramfs-linux-cachyos.img \
  --vmlinuz /boot/vmlinuz-linux-cachyos-7.0.9 \
  --desc "description" \
  --parent snap-a0af1eeb

# Pin and reboot
sudo /srv/data/bc-250-snapshotter/snapshot pin snap-XXXXXXXX
sudo reboot -f
```

---

## Env Vars — Permanent Setup

Add to `/etc/environment` or shell rc:
```bash
HSA_ENABLE_SDMA=0
HSA_OVERRIDE_GFX_VERSION=10.1.0
```

Kernel cmdline (in GRUB):
```
amdgpu.bc250_skip_sdma0=1 amdgpu.ppfeaturemask=0xfff73ef7
```

---

## Production Snapshots

```
snap-31bbf471 (19-patch, stable fallback)
  └─ snap-a0af1eeb (19 + 24: all-VMID TLB flush)  ← PINNED
```
