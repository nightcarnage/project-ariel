//! The blade's **pre-liberation ("stock") layout** — captured once, never
//! overwritten.
//!
//! Two independent facts are recorded, and their *observability windows* differ
//! completely — which is the whole reason this module exists:
//!
//! * **CPU core mask** (`SMN 0x115A870`). The firmware re-initialises it at every
//!   COLD boot from that die's own binning, so each blade's true stock is
//!   whatever its silicon reports before the first SMU write. A warm reboot keeps
//!   any written value; only a full power loss brings the firmware's back.
//!
//! * **CU layout.** The driver reads `cu_bitmap` once, at gfx init. With the
//!   40-CU unlock armed (`bc250_cc_write_mode=3`) the driver is told every shader
//!   array is full, so enumeration reports 40 on *every* boot and the real
//!   silicon layout is **no longer observable from software**. It must be
//!   captured before the unlock is ever armed — there is no second chance.
//!
//! So the CU half is recorded as `None` (with the reason spelled out) when the
//! unlock was already armed at capture time, rather than silently banking a
//! bogus 40.
//!
//! ## Why this exists
//!
//! `cores boot` assumed stock was `0x77`; `cu route-factory` assumes every array
//! carries 3 WGPs (`0x07`). Both are traps on a blade whose silicon was binned
//! differently — they restore the *assumed* layout over the real one, and on a
//! `0xD7` blade `cores boot` refuses outright and **exits 0**, so the unlock
//! silently fails to survive a cold boot. Recording the baseline makes "factory"
//! mean *this machine's* factory, and turns "we don't know" into a fact we can
//! check rather than a constant we can guess wrong.

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use ariel_smu::ocq3::OcQ3;

use crate::cu;

/// Where the one-shot baseline lives. Beside the CU route profile, under
/// `/var/lib/` — this is machine state, not configuration.
pub const STOCK_PATH: &str = "/var/lib/arieltune/stock.json";

/// The CPU half of the baseline.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CpuStock {
    /// The mask as the firmware presented it at capture time.
    pub core_mask: u8,
    pub enabled: Vec<u8>,
    pub disabled: Vec<u8>,
}

/// The CU half of the baseline. Only obtainable while the unlock is disarmed.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CuStock {
    /// CUs the driver enumerated at gfx init — the silicon truth.
    pub enumerated_cu: u32,
    /// Per-array WGP mask derived from that enumeration. This — not `0x07` — is
    /// what `route-factory` should restore on this blade.
    pub per_array_wgp: [u32; 4],
    /// Same shape, counted in CUs (2 per WGP), for direct display.
    pub per_array_cu: [u32; 4],
}

/// The recorded pre-liberation baseline.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Stock {
    /// Unix seconds. Formatted at display time so this crate needs no date lib.
    pub captured_at_unix: u64,
    /// What triggered the capture (`install`, `first-run`, `cores-apply`, ...).
    pub source: String,
    /// Kernel release the capture was taken under.
    pub kernel: String,
    pub cpu: CpuStock,
    /// `None` when the CU layout was not observable — see `cu_unavailable`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cu: Option<CuStock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cu_unavailable: Option<String>,
}

fn bits(mask: u8) -> (Vec<u8>, Vec<u8>) {
    let on: Vec<u8> = (0..8).filter(|i| mask & (1 << *i) != 0).collect();
    let off: Vec<u8> = (0..8).filter(|i| mask & (1 << *i) == 0).collect();
    (on, off)
}

/// Read the CPU mask straight from the SMU. Needs root + BC-250.
pub fn live_cpu_mask() -> Result<u8> {
    OcQ3::open().and_then(|q| q.core_mask()).context(
        "read the CPU core mask (needs root and a BC-250; OCq3::open refuses other silicon)",
    )
}

/// Sample both halves of the baseline from the live machine.
///
/// The CPU half always works if the mailbox opens. The CU half is skipped — with
/// the reason recorded — when the 40-CU unlock is armed, because the enumeration
/// is then pinned at 40 and carries no information about the silicon.
pub fn sample(source: &str) -> Result<Stock> {
    let mask = live_cpu_mask()?;
    let (enabled, disabled) = bits(mask);

    let (cu_stock, cu_unavailable) = if cu::liberation_armed() {
        (
            None,
            Some(
                "40-CU unlock is armed (bc250_cc_write_mode>0): the driver is told every \
                 shader array is full, so enumeration reads 40 on every boot and the real \
                 layout is no longer observable. Un-arm the unlock and cold-boot to capture it."
                    .to_string(),
            ),
        )
    } else {
        match cu::driver_wgp_masks() {
            Some(wm) => {
                let per_cu = std::array::from_fn(|i| (wm[i] & 0x1f).count_ones() * 2);
                (
                    Some(CuStock {
                        enumerated_cu: per_cu.iter().sum(),
                        per_array_wgp: wm,
                        per_array_cu: per_cu,
                    }),
                    None,
                )
            }
            None => (None, Some("amdgpu enumeration unavailable".to_string())),
        }
    };

    Ok(Stock {
        captured_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        source: source.to_string(),
        kernel: ariel_hal::running_kernel(),
        cpu: CpuStock {
            core_mask: mask,
            enabled,
            disabled,
        },
        cu: cu_stock,
        cu_unavailable,
    })
}

/// Load the recorded baseline, if any. Never fails — a corrupt or absent file
/// simply means "not captured".
pub fn load() -> Option<Stock> {
    let s = fs::read_to_string(STOCK_PATH).ok()?;
    serde_json::from_str(&s).ok()
}

pub fn is_captured() -> bool {
    load().is_some()
}

/// The recorded CPU stock mask, if we have it.
pub fn cpu_stock_mask() -> Option<u8> {
    load().map(|s| s.cpu.core_mask)
}

/// The recorded CU factory masks, if we have them. `None` means either "not
/// captured yet" or "captured while armed" — callers must fall back explicitly.
pub fn cu_factory_masks() -> Option<[u32; 4]> {
    load().and_then(|s| s.cu.map(|c| c.per_array_wgp))
}

/// Capture the baseline **only if nothing is recorded yet**.
///
/// This is the safe, idempotent entry point to call from read-only commands: the
/// baseline is a one-time fact about the silicon, so an existing file is never
/// rewritten. Returns `Some(stock)` when this call performed the capture.
pub fn capture_if_absent(source: &str) -> Option<Stock> {
    if is_captured() {
        return None;
    }
    match sample(source) {
        Ok(s) => match write(&s) {
            Ok(()) => {
                println!(
                    "arieltune: captured pre-liberation baseline (cpu mask 0x{:02X}{}) -> {STOCK_PATH}",
                    s.cpu.core_mask,
                    match &s.cu {
                        Some(c) => format!(", {} CU enumerated", c.enumerated_cu),
                        None => ", CU layout NOT observable".to_string(),
                    }
                );
                Some(s)
            }
            Err(e) => {
                eprintln!("arieltune: could not record the stock baseline: {e:#}");
                None
            }
        },
        Err(e) => {
            // Not fatal: a non-root or foreign-SoC caller just doesn't get a
            // baseline recorded this time.
            eprintln!("arieltune: stock baseline not captured ({e:#})");
            None
        }
    }
}

/// Force a fresh capture, overwriting any existing baseline.
pub fn capture_now(source: &str) -> Result<Stock> {
    let s = sample(source)?;
    write(&s)?;
    Ok(s)
}

fn write(s: &Stock) -> Result<()> {
    if let Some(dir) = Path::new(STOCK_PATH).parent() {
        fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
    }
    let json = serde_json::to_string_pretty(s)?;
    fs::write(STOCK_PATH, json).with_context(|| format!("write {STOCK_PATH}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_split_a_normal_and_an_abnormal_mask() {
        // Textbook stock: cores 3 and 7 masked.
        let (on, off) = bits(0x77);
        assert_eq!(on, vec![0, 1, 2, 4, 5, 6]);
        assert_eq!(off, vec![3, 7]);
        // 115's actual firmware layout: the mirror, core 7 on and core 5 off.
        let (on, off) = bits(0xD7);
        assert_eq!(on, vec![0, 1, 2, 4, 6, 7]);
        assert_eq!(off, vec![3, 5]);
        // Fully unlocked.
        let (on, off) = bits(0xFF);
        assert_eq!(on, vec![0, 1, 2, 3, 4, 5, 6, 7]);
        assert!(off.is_empty());
    }

    #[test]
    fn stock_round_trips_through_json() {
        let s = Stock {
            captured_at_unix: 1_760_000_000,
            source: "test".into(),
            kernel: "7.2.9-1-cachyos-bore".into(),
            cpu: CpuStock {
                core_mask: 0xD7,
                enabled: vec![0, 1, 2, 4, 6, 7],
                disabled: vec![3, 5],
            },
            cu: Some(CuStock {
                enumerated_cu: 24,
                per_array_wgp: [0x07; 4],
                per_array_cu: [6; 4],
            }),
            cu_unavailable: None,
        };
        let j = serde_json::to_string(&s).unwrap();
        let back: Stock = serde_json::from_str(&j).unwrap();
        assert_eq!(back.cpu.core_mask, 0xD7);
        assert_eq!(back.cu.unwrap().enumerated_cu, 24);
        // The unavailable reason is omitted when absent, and optional when read
        // back — so an older file without the field still parses.
        assert!(!j.contains("cu_unavailable"));
    }

    #[test]
    fn absent_cu_is_recorded_with_a_reason_not_a_lie() {
        let s = Stock {
            captured_at_unix: 0,
            source: "test".into(),
            kernel: "k".into(),
            cpu: CpuStock {
                core_mask: 0xFF,
                enabled: (0..8).collect(),
                disabled: vec![],
            },
            cu: None,
            cu_unavailable: Some("armed".into()),
        };
        let back: Stock = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert!(back.cu.is_none());
        assert_eq!(back.cu_unavailable.as_deref(), Some("armed"));
    }
}
