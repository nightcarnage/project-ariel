// SPDX-License-Identifier: GPL-2.0-only
//! cores — the BC-250 8-core CPU unlock + live per-core offline control.
//!
//! Two deliberately separated layers (see arieltune-core.md):
//!
//! * **Firmware layer** — the SMU-space core-presence mask (SMN 0x115A870).
//!   Stock `0x77` = 6C/12T (cores 3 and 7 masked), `0xFF` = 8C/16T. Written
//!   all-or-nothing via the q3 msg 0x98 backdoor ([`ariel_smu::ocq3::OcQ3`]).
//!   Applies at the next warm reboot; a cold boot reverts it. This module
//!   REFUSES any mask other than the known stock/full values — the community
//!   DXE drivers do the same, to avoid lockout on abnormal boards.
//!
//! * **OS layer** — per-thread online/offline via
//!   `/sys/devices/system/cpu/cpuN/online`. Arbitrary granularity (2 cores for
//!   a test, skip a defective core), instant, reversible, no firmware risk.
//!   This is the ONLY granular control arieltune ships; arbitrary firmware
//!   masks require the q2 0x23 SMU exploit and are parked as research.
//!
//! SAFETY RULES (must survive any edit):
//!   * No automatic reboots, ever. GabriWar's earlier unit rebooted itself and
//!     bootlooped a real board (the reset did not preserve the mask, and
//!     systemctl cannot reboot before D-Bus). `boot` applies and stops.
//!   * `boot` is the systemd-unit path: idempotent, exit 0 on refusal.
//!   * `verify` is ADVISORY. Its report is never read back for decisions:
//!     fleet images get cloned across blades, so persisted verdicts are
//!     untrustworthy lineage. Only live reads gate anything.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};

use ariel_smu::ocq3::{CoreUnlock, OcQ3, CORE_MASK_FULL, CORE_MASK_STOCK};

pub const UNIT_NAME: &str = "aputune-cores.service";
pub const UNIT_PATH: &str = "/etc/systemd/system/aputune-cores.service";
pub const REBOOT_MODE_PATH: &str = "/sys/kernel/reboot/mode";
pub const ACPI_OVERRIDE_DIR: &str = "/etc/initcpio/acpi_override";
pub const ACPI_BACKUP_DIR: &str = "/etc/initcpio/acpi_override-backups";
/// GRUB-prepended override cpio. A GRUB `initrd a.cpio b.img` chain does **not**
/// appear in `/proc/cmdline`, so this is a second source of the same tables that
/// is invisible to every cmdline-based check -- and the reason a blade can end up
/// with each table loaded twice.
pub const ACPI_CPIO_PATH: &str = "/boot/acpi_override.cpio";
pub const MKINITCPIO_CONF: &str = "/etc/mkinitcpio.conf";
pub const REPORT_DIR: &str = "/var/lib/aputune";
pub const ACPI_TABLES: &[(&str, &str, &str)] = &[
    (
        "SSDT-CST.aml",
        "https://github.com/mendesrr/bc250-acpi-fix-updated-8c/raw/refs/heads/main/SSDT-CST.aml",
        "4ed0dfbae974426bdcc1d7d55da1d9669e5b7cc7c54b6e0683e8f2a6c76fbb81",
    ),
    (
        "SSDT-PST.aml",
        "https://github.com/mendesrr/bc250-acpi-fix-updated-8c/raw/refs/heads/main/SSDT-PST.aml",
        "1fb4a2d01a32eee3b9226bcbb19f63878c346d434a360b1bb28f837d4a1a3eb0",
    ),
];

/// Firmware/OS core state, computed from live reads only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoreState {
    /// This blade's recorded factory mask — 6C/12T. Not necessarily `0x77`:
    /// firmware masks are per-die and a blade can legitimately ship `0xD7`.
    Locked,
    /// Mask 0xFF but fewer than 8 cores visible: warm reboot needed.
    PendingReboot,
    /// Mask 0xFF and 8 physical cores visible.
    Unlocked,
    /// Any other mask — all mutating verbs refuse.
    Abnormal(u8),
}

impl CoreState {
    pub fn label(self) -> &'static str {
        match self {
            CoreState::Locked => "LOCKED",
            CoreState::PendingReboot => "PENDING-REBOOT",
            CoreState::Unlocked => "UNLOCKED",
            CoreState::Abnormal(_) => "ABNORMAL",
        }
    }
}

/// Test-only convenience wrapper: the textbook-stock view of [`state_for_stock`],
/// so the table-driven cases can read without repeating the mask. Production
/// callers must pass the blade's *recorded* stock, not this constant.
#[cfg(test)]
fn state_for(mask: u8, visible_cores: u32) -> CoreState {
    state_for_stock(mask, visible_cores, CORE_MASK_STOCK)
}

/// Pure state derivation against **this blade's recorded stock**.
///
/// Firmware masks are per-die: blade 115 ships `0xD7`, not `0x77`. Judging it
/// against the textbook constant labelled a perfectly healthy blade `Abnormal`,
/// which sent the TUI down the *forced* path and made `cores apply` refuse a mask
/// the boot unit wrote without complaint. `Abnormal` must mean "we have no
/// baseline for this", never "it isn't the value I assumed".
pub fn state_for_stock(mask: u8, visible_cores: u32, stock: u8) -> CoreState {
    if mask == stock {
        CoreState::Locked
    } else if mask == CORE_MASK_FULL {
        if visible_cores >= 8 {
            CoreState::Unlocked
        } else {
            CoreState::PendingReboot
        }
    } else {
        CoreState::Abnormal(mask)
    }
}

/// Live snapshot: firmware mask, visible cores/threads, offlined threads, and
/// per-core thread online states (all gathered once, so the TUI draw is
/// fs-read-free).
#[derive(Clone, Debug)]
pub struct CoreSnapshot {
    pub state: CoreState,
    pub mask: u8,
    pub cores: u32,
    pub threads: u32,
    pub offline: u32,
    /// (core id, [(cpu, online), ..]) sorted by core id.
    pub per_core: Vec<(u32, Vec<(u32, bool)>)>,
}

/// List every cpuN directory index (0..max). The cpuN dirs exist even when the
/// CPU is offline — offlined CPUs lose their `topology/` subtree but keep the
/// dir and the `online` file, so this is the recovery-safe enumeration.
pub fn cpu_dirs() -> Vec<u32> {
    let mut cpus: Vec<u32> = Vec::new();
    for e in fs::read_dir("/sys/devices/system/cpu")
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some(n) = name.strip_prefix("cpu") {
            if let Ok(n) = n.parse::<u32>() {
                cpus.push(n);
            }
        }
    }
    cpus.sort_unstable();
    cpus
}

/// Physical core (SMN mask bit) a logical CPU belongs to. Offlined CPUs lose
/// their `topology/` subtree, so this falls back through: own core_id -> the
/// online SMT sibling's core_id -> the x86 adjacent-SMT-pair index (cpu/2).
/// The last resort keeps offlined cores grouped and recoverable.
pub fn core_of(cpu: u32) -> Option<u32> {
    let read = |c: u32| -> Option<u32> {
        fs::read_to_string(format!("/sys/devices/system/cpu/cpu{c}/topology/core_id"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
    };
    if let Some(c) = read(cpu) {
        return Some(c);
    }
    if let Some(c) = read(cpu ^ 1) {
        return Some(c);
    }
    Some(cpu / 2)
}

/// Physical core id -> first CPU number map, sorted by core id.
pub fn core_map() -> Vec<(u32, u32)> {
    let mut map: Vec<(u32, u32)> = Vec::new();
    for cpu in cpu_dirs() {
        let Some(core) = core_of(cpu) else { continue };
        if !map.iter().any(|(c, _)| *c == core) {
            map.push((core, cpu));
        }
    }
    map.sort_by_key(|(c, _)| *c);
    map
}

fn cpu_is_online(cpu: u32) -> bool {
    let p = format!("/sys/devices/system/cpu/cpu{cpu}/online");
    if !Path::new(&p).exists() {
        return true; // cpu0 and non-hotpluggable CPUs have no online file
    }
    fs::read_to_string(&p)
        .map(|s| s.trim() != "0")
        .unwrap_or(true)
}

/// Total logical threads and offlined-thread count (sysfs, live).
fn thread_counts() -> (u32, u32) {
    let mut total = 0u32;
    let mut off = 0u32;
    for e in fs::read_dir("/sys/devices/system/cpu")
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some(n) = name.strip_prefix("cpu") {
            if n.parse::<u32>().is_ok() {
                total += 1;
                let cpu = n.parse::<u32>().unwrap();
                if !cpu_is_online(cpu) {
                    off += 1;
                }
            }
        }
    }
    (total, off)
}

/// This blade's recorded factory mask, falling back to the textbook `0x77` when
/// no baseline has been captured yet (see [`crate::stock`]).
fn recorded_stock() -> u8 {
    crate::stock::cpu_stock_mask().unwrap_or(CORE_MASK_STOCK)
}

/// Live snapshot (requires root + BC-250 — `OcQ3::open` refuses other silicon).
pub fn snapshot() -> Result<CoreSnapshot> {
    let q = OcQ3::open()?;
    let mask = q.core_mask()?;
    let per_core = core_threads();
    let cores = per_core.len() as u32;
    let (threads, offline) = thread_counts();
    Ok(CoreSnapshot {
        // Judge against the recorded baseline, so a blade whose factory mask is
        // 0xD7 reads Locked rather than Abnormal.
        state: state_for_stock(mask, cores, recorded_stock()),
        mask,
        cores,
        threads,
        offline,
        per_core,
    })
}

fn describe_mask(mask: u8) -> String {
    let on: Vec<u8> = (0..8).filter(|i| mask & (1 << *i) != 0).collect();
    let off: Vec<u8> = (0..8).filter(|i| mask & (1 << *i) == 0).collect();
    format!(
        "0x{mask:02X}  enabled=[{}] disabled=[{}]",
        on.iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(" "),
        off.iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(" ")
    )
}

fn mce_count() -> u32 {
    Command::new("dmesg")
        .output()
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| {
                    l.contains("Machine Check Exception")
                        || (l.contains("mce:") && (l.contains("error") || l.contains("corrected")))
                })
                .count() as u32
        })
        .unwrap_or(0)
}

/// Per-core CPU list with per-thread online flags, sorted by core id.
/// `(core_id, [(cpu, online), ..])` — used by the TUI core map. Groups EVERY
/// cpu dir (via [`core_of`]), so fully-offlined cores stay visible/recoverable.
pub fn core_threads() -> Vec<(u32, Vec<(u32, bool)>)> {
    let mut cores: Vec<(u32, Vec<(u32, bool)>)> = Vec::new();
    for cpu in cpu_dirs() {
        let Some(core) = core_of(cpu) else { continue };
        match cores.iter_mut().find(|(c, _)| *c == core) {
            Some((_, v)) => v.push((cpu, cpu_is_online(cpu))),
            None => cores.push((core, vec![(cpu, cpu_is_online(cpu))])),
        }
    }
    for (_, v) in cores.iter_mut() {
        v.sort_by_key(|(c, _)| *c);
    }
    cores.sort_by_key(|(c, _)| *c);
    cores
}

/// `aputune cores status`.
pub fn status() -> Result<()> {
    let s = snapshot()?;
    println!("SMN 0x115A870 = {}", describe_mask(s.mask));
    println!("state: {}", s.state.label());
    match s.state {
        CoreState::Locked => println!(
            "at this blade's recorded stock (mask 0x{:02X}) — run 'aputune cores apply' to unlock",
            s.mask
        ),
        CoreState::Unlocked => println!("all 8 cores / 16 threads active"),
        CoreState::PendingReboot => {
            println!("mask is set but the firmware has not re-enumerated yet — warm reboot needed")
        }
        CoreState::Abnormal(m) => println!("unknown mask 0x{m:02X} — all writes are refused"),
    }
    println!(
        "cores visible to this kernel: {} ({} threads, {} offline)",
        s.cores, s.threads, s.offline
    );
    println!("MCE entries in dmesg: {}", mce_count());
    println!(
        "boot unit: {}",
        if Path::new(UNIT_PATH).exists() {
            "installed"
        } else {
            "not installed"
        }
    );
    println!("state={}", s.state.label());
    Ok(())
}

fn set_warm_reboot() {
    if let Err(e) = fs::write(REBOOT_MODE_PATH, "warm") {
        eprintln!("warning: could not set warm reboot mode ({e})");
    }
}

/// `aputune cores apply`. Never reboots unless `warm_reboot`.
///
/// The gate is **this blade's recorded stock** — the same rule `boot()` uses —
/// not the textbook `0x77`. Firmware masks are per-die (115 ships `0xD7`), so
/// demanding `0x77` here meant the interactive path refused a mask the boot unit
/// wrote without complaint. `force_abnormal` is for a mask we have no baseline
/// for at all: an acknowledgement, not a bypass of the check.
pub fn apply(warm_reboot: bool, force_abnormal: bool) -> Result<()> {
    let before = snapshot()?;
    println!("before: mask {}", describe_mask(before.mask));

    // Record the baseline BEFORE the first write. This is the last moment the
    // firmware's own value is visible, and on this path it is visible on a warm
    // reboot too -- so it is the one reliable capture point that does not need a
    // cold boot. Never overwrites an existing baseline.
    crate::stock::capture_if_absent("cores-apply");

    // Read the baseline *after* the capture attempt, so a first-ever run on a
    // 0xD7 blade records the real value before we judge the live mask by it.
    let stock = recorded_stock();
    let unexpected = before.mask != stock && before.mask != CORE_MASK_FULL;
    if unexpected && !force_abnormal {
        bail!(
            "refusing to write: live mask 0x{mask:02X} is neither this blade's recorded stock \
             0x{stock:02X} nor 0x{full:02X} (already unlocked). Nothing was changed. If \
             0x{mask:02X} IS this blade's factory layout, record it with \
             `arieltune apu stock capture --force`, or pass --force-abnormal to accept it once.",
            mask = before.mask,
            stock = stock,
            full = CORE_MASK_FULL
        );
    }
    if unexpected {
        println!(
            "WARNING: --force-abnormal accepted mask 0x{:02X}, which has no recorded baseline — EXPERIMENTAL",
            before.mask
        );
    }

    let q = OcQ3::open()?;
    // `unlock_cores()` is gated on 0x77 inside ocq3, so a blade whose recorded
    // stock is anything else has to take the any-mask path. That is not a bypass
    // here -- the mask already matched the value we recorded from this die.
    let outcome = if stock == CORE_MASK_STOCK && !unexpected {
        q.unlock_cores()?
    } else {
        q.unlock_cores_any()?
    };
    match outcome {
        CoreUnlock::AlreadyUnlocked => {
            println!("already unlocked, nothing to do");
        }
        CoreUnlock::Unlocked => {
            println!(
                "mask written and verified 0xFF — all 8 cores will appear after a WARM reboot"
            );
            println!(
                "a cold boot (power removed) reverts to 6 cores; the boot unit re-applies the mask"
            );
            println!("WARNING: the SoC power/thermal envelope changes with 2 extra cores — re-validate any CPU OC/undervolt and GPU VDDC settings");
            if warm_reboot {
                set_warm_reboot();
                println!("rebooting (warm)...");
                let st = Command::new("systemctl").arg("reboot").status()?;
                if !st.success() {
                    bail!("systemctl reboot failed ({st})");
                }
            } else {
                println!("run 'aputune cores status' after reboot; state should be UNLOCKED");
            }
        }
    }
    Ok(())
}

/// Boot-time path for `aputune-cores.service`. Idempotent; NEVER reboots.
///
/// EXITS NON-ZERO on a refusal it cannot explain. (It used to return Ok(()) for
/// every outcome, "no restart storm" -- but that made a genuine failure
/// indistinguishable from success: a blade whose firmware mask is not 0x77 got
/// "unexpected mask" and exit 0 forever, so the unlock silently never survived a
/// cold boot. A loud failure is worth one systemd restart attempt.)
pub fn boot() -> Result<()> {
    let mask = match OcQ3::open().and_then(|q| q.core_mask()) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("cores boot: {e}");
            return Ok(());
        }
    };
    if mask == CORE_MASK_FULL {
        println!("already unlocked — nothing to do");
        return Ok(());
    }

    // The gate is this BLADE's recorded stock, not the textbook 0x77. Firmware
    // masks are per-die: a blade can come up 0xD7 (115 does), and demanding 0x77
    // there means refusing forever. With no baseline yet, this is exactly the
    // right moment to take one -- after a cold boot the live mask IS the stock.
    let stock = match crate::stock::cpu_stock_mask() {
        Some(m) => m,
        None => {
            crate::stock::capture_if_absent("cores-boot");
            crate::stock::cpu_stock_mask().unwrap_or(CORE_MASK_STOCK)
        }
    };

    if mask != stock {
        anyhow::bail!(
            "refusing to write: live mask 0x{mask:02X} is neither this blade's recorded stock \
             0x{stock:02X} nor 0x{CORE_MASK_FULL:02X} (already unlocked). Nothing was changed. If \
             0x{mask:02X} IS this blade's factory layout, record it with \
             `arieltune apu stock capture --force` and re-run."
        );
    }

    let q = OcQ3::open()?;
    // lock_cores()/unlock_cores() are gated on 0x77 internally, so a blade whose
    // stock is abnormal must go through the any-mask path. That is not a bypass
    // here -- the mask already matched the value we recorded from this die.
    let outcome = if stock == CORE_MASK_STOCK {
        q.unlock_cores()?
    } else {
        q.unlock_cores_any()?
    };
    match outcome {
        CoreUnlock::Unlocked => {
            println!("mask set to 0xFF — all 8 cores will appear on your next reboot");
        }
        CoreUnlock::AlreadyUnlocked => {}
    }
    // A write failure propagates as a non-zero exit on purpose: the unit is then
    // marked failed and `systemctl --failed` shows it, instead of the old
    // print-and-return-Ok() that made a real failure look like success.
    Ok(())
}

/// `aputune cores install` — write + enable the boot unit.
pub fn install() -> Result<()> {
    let unit = "[Unit]\n\
         Description=BC-250 8-core unlock (SMU msg 0x98)\n\
         After=multi-user.target\n\
         ConditionPathExists=/sys/bus/pci/devices/0000:00:00.0/config\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         ExecStart=/usr/local/bin/arieltune apu cores boot\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n"
        .to_string();
    fs::write(UNIT_PATH, unit).context("write unit file")?;
    for args in [&["daemon-reload"][..], &["enable", UNIT_NAME][..]] {
        let st = Command::new("systemctl").args(args).status()?;
        if !st.success() {
            bail!("systemctl {} failed ({st})", args.join(" "));
        }
    }
    println!("installed: {UNIT_PATH} (enabled)");
    println!("the unit NEVER reboots; after a cold boot it sets the mask, and the cores appear on your next reboot");
    Ok(())
}

/// `aputune cores uninstall`.
pub fn uninstall() -> Result<()> {
    let _ = Command::new("systemctl")
        .args(["disable", "--now", UNIT_NAME])
        .status();
    let _ = fs::remove_file(UNIT_PATH);
    let _ = Command::new("systemctl").arg("daemon-reload").status();
    println!("removed. cores stay unlocked until the next cold boot.");
    Ok(())
}

/// `aputune cores stock` — put the CPU back to this blade's factory layout.
///
/// No software path can write the mask **back**. The q3 `0x98` write takes an
/// *address*, not a value, and the firmware always stores `0xFF` there; a warm
/// reboot then preserves whatever is loaded, by design. Only removing power makes
/// the firmware re-initialise the mask from this die's own binning.
///
/// So this verb does the one thing software can actually do -- stop the boot unit
/// from re-applying the unlock, which would otherwise undo the cold boot on the
/// way back up -- and then states plainly what is left for the operator to do. It
/// performs no SMU write at all, and it never cuts power: the mains switch is not
/// reachable from the blade.
pub fn restore_stock() -> Result<()> {
    let s = snapshot()?;
    let recorded = crate::stock::cpu_stock_mask();
    let unit = Path::new(UNIT_PATH).exists();

    println!("live mask: {}", describe_mask(s.mask));
    match recorded {
        Some(m) => println!("recorded stock for this blade: 0x{m:02X}"),
        None => println!(
            "recorded stock for this blade: unknown (no baseline captured yet). The cold boot \
             below still restores this die's real factory mask -- the firmware re-initialises it \
             from its own binning -- we simply cannot predict the value. `arieltune apu stock \
             capture` afterwards records it."
        ),
    }
    println!("boot unit installed: {}", if unit { "yes" } else { "no" });

    if recorded == Some(s.mask) && !unit {
        println!("already at stock on both counts — nothing to do");
        return Ok(());
    }

    if unit {
        // Idempotent, and reversible with `cores install`. Leaving it enabled
        // would re-apply the unlock the moment the blade came back up.
        uninstall()?;
    }

    println!();
    println!("next: a COLD boot — power removed, not a warm reboot.");
    println!("  · a warm reboot preserves the mask by design; nothing writes it back");
    println!("  · a cold boot re-initialises it to this die's factory value");
    println!("  · from the workstation: scripts/bc250-power coldcycle <blade>");
    println!("  · afterwards `arieltune apu cores status` should report the stock mask");
    Ok(())
}

// ---------------------------------------------------------------------------
// OS layer — live per-core offline/online
// ---------------------------------------------------------------------------

fn set_core_online(core: u32, online: bool) -> Result<u32> {
    // BOTH threads of the core, from the robust grouping (offline-safe).
    let cpus: Vec<u32> = core_threads()
        .iter()
        .find(|(c, _)| *c == core)
        .map(|(_, v)| v.iter().map(|(cpu, _)| *cpu).collect())
        .unwrap_or_else(|| {
            // Fully-offline core with no readable topology: the adjacent SMT
            // pair (x86 enumeration order) is the best available guess.
            vec![2 * core, 2 * core + 1]
        });
    if cpus.is_empty() {
        bail!("core {core} has no known logical CPUs");
    }
    if !online && cpus.contains(&0) {
        bail!("refusing to offline cpu0 (core 0) — the kernel cannot offline cpu0");
    }
    let mut touched = 0;
    for cpu in &cpus {
        let p = format!("/sys/devices/system/cpu/cpu{cpu}/online");
        if Path::new(&p).exists() {
            fs::write(&p, if online { "1" } else { "0" }).with_context(|| format!("write {p}"))?;
            touched += 1;
        }
    }
    let (threads, offline) = thread_counts();
    println!(
        "core {core} {} — now {threads} threads visible, {offline} offline",
        if online { "online" } else { "offline" }
    );
    Ok(touched)
}

/// Offline all cores except 0, or one specific core. OS layer only.
pub fn offline(core: Option<u32>) -> Result<()> {
    match core {
        Some(c) => {
            set_core_online(c, false)?;
        }
        None => {
            // No topology needed: write 0 to every hotpluggable CPU but cpu0.
            // This path must keep working when cores are already offline.
            for cpu in cpu_dirs() {
                if cpu == 0 {
                    continue;
                }
                let p = format!("/sys/devices/system/cpu/cpu{cpu}/online");
                if Path::new(&p).exists() {
                    fs::write(&p, "0").with_context(|| format!("write {p}"))?;
                }
            }
            let (threads, offline) = thread_counts();
            println!(
                "all cores except 0 offline — now {threads} threads visible, {offline} offline"
            );
        }
    }
    Ok(())
}

/// Online one core or every present CPU. The `None` (all) path writes 1 to
/// every hotpluggable CPU dir with no topology dependency — the guaranteed
/// recovery route after anything got offlined.
pub fn online(core: Option<u32>) -> Result<()> {
    match core {
        Some(c) => {
            set_core_online(c, true)?;
        }
        None => {
            for cpu in cpu_dirs() {
                let p = format!("/sys/devices/system/cpu/cpu{cpu}/online");
                if Path::new(&p).exists() {
                    fs::write(&p, "1").with_context(|| format!("write {p}"))?;
                }
            }
            let (threads, offline) = thread_counts();
            println!("all cpus online — now {threads} threads visible, {offline} offline");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// verify — advisory per-core stress-ng sweep (never a gate)
// ---------------------------------------------------------------------------

fn stress_one(cpu: u32, seconds: u64) -> (Option<f64>, Option<u64>, Option<u64>) {
    let out = Command::new("taskset")
        .args(["-c", &cpu.to_string()])
        .args([
            "stress-ng",
            "--cpu",
            "1",
            "--cpu-method",
            "all",
            "--verify",
            "--metrics-brief",
        ])
        .args(["-t", &format!("{seconds}s")])
        .output();
    let Ok(out) = out else {
        return (None, None, None);
    };
    let text = String::from_utf8_lossy(&out.stdout);
    // bogo ops/s: the metric table's cpu row, 7th whitespace field.
    let mut rate = None;
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.first() == Some(&"cpu") && f.len() >= 7 {
            rate = f[6].parse::<f64>().ok();
            break;
        }
    }
    let find = |tag: &str| -> Option<u64> {
        text.lines()
            .find_map(|l| l.strip_prefix(&format!("{tag}:")))
            .and_then(|s| s.split_whitespace().next())
            .and_then(|v| v.parse::<u64>().ok())
    };
    let passed = find("passed");
    let failed = find("failed");
    (rate, passed, failed)
}

/// Run the advisory sweep and return report lines (also the TUI worker path).
pub fn sweep_lines(seconds: u64) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    if Command::new("stress-ng").arg("--version").output().is_err() {
        bail!("stress-ng not installed (the sweep needs it; verify is advisory)");
    }
    let map = core_map();
    out.push(format!(
        "per-core sweep ({}s each, 1 thread pinned per physical core) on {} cores",
        seconds,
        map.len()
    ));
    out.push("core  cpu  bogo-ops/s     passed failed  note".into());
    let mut rates: Vec<(u32, f64)> = Vec::new();
    let mut problems: Vec<String> = Vec::new();
    for (core, cpu) in &map {
        let (rate, passed, failed) = stress_one(*cpu, seconds);
        let tag = if *core == 3 || *core == 7 { "NEW" } else { "" };
        let fail = failed.unwrap_or(0);
        if fail > 0 {
            problems.push(format!(
                "core {core}: {fail} verify failures (wrong results)"
            ));
        }
        if let Some(r) = rate {
            rates.push((*core, r));
        }
        out.push(format!(
            "{:<4} {:<4} {:>12} {:>8} {:>7}  {}",
            core,
            cpu,
            rate.map(|r| format!("{r:.0}"))
                .unwrap_or_else(|| "?".into()),
            passed.map(|p| p.to_string()).unwrap_or_else(|| "?".into()),
            failed.map(|f| f.to_string()).unwrap_or_else(|| "?".into()),
            tag
        ));
    }
    if !rates.is_empty() {
        rates.sort_by_key(|(_, r)| *r as i64);
        let med = rates[rates.len() / 2].1;
        out.push(format!("deviation from median ({med:.0} bogo-ops/s):"));
        for (core, r) in &rates {
            let pct = (r - med) / med * 100.0;
            let tag = if *core == 3 || *core == 7 {
                "   <-- NEW"
            } else {
                ""
            };
            out.push(format!("  core {core}: {r:>10.0}  ({pct:+.1}%){tag}"));
        }
    }
    out.push(format!("MCE entries in dmesg: {}", mce_count()));
    if problems.is_empty() {
        out.push(
            "verdict: no verify failures observed (advisory only — this never gates anything)"
                .into(),
        );
    } else {
        out.push(format!("verdict: PROBLEMS — {}", problems.join("; ")));
        out.push("  a core that fails --verify produces WRONG results. Do not use it.".into());
    }
    Ok(out)
}

/// `aputune cores verify [seconds]` — advisory sweep, prints + saves a report.
pub fn verify(seconds: u64) -> Result<()> {
    let lines = sweep_lines(seconds)?;
    for l in &lines {
        println!("{l}");
    }

    // Report for the record only — nothing reads it back for decisions
    // (fleet images are cloned across blades; persisted verdicts lie).
    fs::create_dir_all(REPORT_DIR).ok();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = fs::write(
        format!("{REPORT_DIR}/cores-verify-{ts}.txt"),
        lines.join("\n"),
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// ACPI — 8-core SSDT-CST/PST initcpio override
// ---------------------------------------------------------------------------

/// Threads (`cpuN`) with no cpuidle states, i.e. no C-states bound to them.
///
/// This deliberately walks every `cpuN` directory instead of using [`core_map`],
/// which yields one CPU per **core**. Counting through that reported *cores*
/// while the surrounding text talked about *threads*: blade 105 printed
/// "cpus without idle states: 2" directly above a hint about "threads 12-15" —
/// the same fact stated in two different units on one screen.
fn threads_without_idle() -> Vec<u32> {
    threads_missing("cpuidle")
}

/// Threads (`cpuN`) with no cpufreq policy, i.e. no P-states bound to them.
fn threads_without_freq() -> Vec<u32> {
    threads_missing("cpufreq")
}

/// Every `cpuN` whose `sysfs/<kind>` directory is absent or empty.
///
/// "Empty" counts as missing on purpose: the kernel creates `cpuidle/` before it
/// populates the state subdirectories, so a present-but-empty directory means no
/// states were ever bound.
fn threads_missing(kind: &str) -> Vec<u32> {
    let mut out = Vec::new();
    let root = Path::new("/sys/devices/system/cpu");
    let Ok(entries) = fs::read_dir(root) else {
        return out;
    };
    for e in entries.flatten() {
        let Some(name) = e.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Some(num) = name.strip_prefix("cpu").and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        let dir = root.join(&name).join(kind);
        let empty = fs::read_dir(&dir)
            .map(|mut d| d.next().is_none())
            .unwrap_or(true);
        if !dir.is_dir() || empty {
            out.push(num);
        }
    }
    out.sort_unstable();
    out
}

/// The OEM id of a raw ACPI table: 6 bytes at offset 10, space padded.
fn acpi_oem_id(bytes: &[u8]) -> Option<String> {
    let raw = bytes.get(10..16)?;
    Some(String::from_utf8_lossy(raw).trim().to_string())
}

/// Distinct 4-character processor names an AML blob refers to.
///
/// The `prefix` argument is the whole point. This firmware's DSDT declares its
/// processors as `Processor (P000, ...)` — **`P0xx`** — while AMD's own CPU
/// SSDT refers to `\_PR.C0xx`, which does not exist in this namespace. So a
/// `C0xx` count measures a table that cannot attach, and a `P0xx` count measures
/// the one that does. Counting the wrong prefix is how the old code produced a
/// number that happened to look right on one blade and would be wrong on any
/// correctly-built table.
fn processor_names(bytes: &[u8], prefix: &[u8; 2]) -> std::collections::BTreeSet<String> {
    let mut found = std::collections::BTreeSet::new();
    for i in 0..bytes.len().saturating_sub(3) {
        if bytes[i] == prefix[0] && bytes[i + 1] == prefix[1] {
            // Only the two-character tail is hex. Validating the whole name would
            // reject every `P0xx` object outright: `P` is not a hex digit, though
            // `C` is — which is exactly how a `C0xx`-only counter could look
            // plausible while being unable to see the names that matter.
            if bytes[i + 2..i + 4].iter().all(u8::is_ascii_hexdigit) {
                found.insert(String::from_utf8_lossy(&bytes[i..i + 4]).into_owned());
            }
        }
    }
    found
}

/// The OEM table id of a raw ACPI table: 8 bytes at offset 16, space padded.
///
/// This -- not the file name -- is what distinguishes `PSTATES` from `P_CST3`,
/// and it is how the kernel reports an upgrade (`SSDT  HACK  P_CST3`).
fn acpi_oem_table_id(bytes: &[u8]) -> Option<String> {
    let raw = bytes.get(16..24)?;
    Some(String::from_utf8_lossy(raw).trim().to_string())
}

/// One loaded OEM-HACK override table, as the kernel actually loaded it.
#[derive(Debug, Clone)]
struct LoadedOverride {
    /// OEM table id (`PSTATES`, `P_CST3`).
    table_id: String,
    /// Distinct `P0xx` processor names the table declares.
    processors: u32,
    /// The `/sys/firmware/acpi/tables/` entry it was read from.
    file: String,
}

/// Every loaded OEM-HACK override table.
///
/// Plural on purpose. The same table can be supplied by two independent sources
/// at once: the initramfs (staged under [`ACPI_OVERRIDE_DIR`] by [`acpi_install`])
/// and a GRUB-prepended [`ACPI_CPIO_PATH`] chained into the *same* `initrd` line.
/// Because a GRUB `initrd` chain is invisible in `/proc/cmdline`, the only
/// honest evidence is the loaded tables themselves: blade 115 has each table
/// loaded twice, which is harmless only for as long as both copies agree.
///
/// Reads `/sys/firmware/acpi/tables/` -- what the kernel actually loaded -- not
/// the staged copy under [`ACPI_OVERRIDE_DIR`]. The staged directory can be
/// absent while an override is very much in force: blade 105 delivers its tables
/// from the GRUB cpio and has no staging dir at all, so a check against the
/// staging dir reported "none installed" on the one blade that was genuinely
/// losing threads. Read the effect, not the intent.
///
/// Overrides are identified by OEM id `HACK`. The firmware's own tables are
/// deliberately excluded: they declare more processors than are active and would
/// overstate coverage.
fn loaded_overrides() -> Vec<LoadedOverride> {
    let dir = Path::new("/sys/firmware/acpi/tables");
    let mut found: Vec<LoadedOverride> = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return found;
    };
    for e in entries.flatten() {
        let path = e.path();
        if !path.is_file() {
            continue; // `dynamic/`, `data/`
        }
        let Ok(bytes) = fs::read(&path) else { continue };
        if acpi_oem_id(&bytes).as_deref() != Some("HACK") {
            continue;
        }
        found.push(LoadedOverride {
            table_id: acpi_oem_table_id(&bytes).unwrap_or_default(),
            // The C-state table defines coverage; the P-state table carries the
            // same processor list, so either measures the same thing.
            processors: processor_names(&bytes, b"P0").len() as u32,
            file: path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
        });
    }
    found.sort_by(|a, b| (&a.table_id, &a.file).cmp(&(&b.table_id, &b.file)));
    found
}

/// Render a cpu list compactly: `cpu12 cpu13`.
fn fmt_cpus(list: &[u32]) -> String {
    list.iter()
        .map(|c| format!("cpu{c}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `aputune cores acpi status` — what the loaded tables cover, against what this
/// box actually has.
///
/// States the comparison rather than two independent numbers, because that
/// comparison *is* the failure mode: an override covering fewer processors than
/// the box has threads leaves the surplus threads with no C-states and no
/// frequency control, silently. Over-coverage is free — extra methods attach to
/// processors no CPU claims and do nothing — so only under-coverage is reported
/// as a problem.
pub fn acpi_status() -> Result<()> {
    let s = snapshot()?;
    let no_idle = threads_without_idle();
    let no_freq = threads_without_freq();

    // Name the board. This command answers "is my firmware supported", and that
    // answer is a property of the BIOS version *and* the loaded tables -- so
    // print both rather than leaving the version to be worked out from DMI
    // somewhere else. The verified-version list lives in `bc250-board`; ask it
    // instead of re-deriving a second opinion here.
    let board = bc250_board::detect_bios();
    let product = if board.product.trim().is_empty() {
        "(unreadable)".to_string()
    } else {
        board.product.clone()
    };
    let version = if board.bios_version.trim().is_empty() {
        "(unreadable)".to_string()
    } else {
        board.bios_version.clone()
    };
    println!(
        "board: {product} · BIOS {version} · {}",
        board.compat.label()
    );

    println!("logical threads: {}", s.threads);

    // Every loaded override, not just the widest one: a stale copy or a
    // duplicated table is invisible in a single "covers N" line, and those are
    // the two states that actually bit us.
    let loaded = loaded_overrides();
    if loaded.is_empty() {
        println!("override covers: none loaded — no OEM-HACK table in /sys/firmware/acpi/tables");
    } else {
        for o in &loaded {
            let id = if o.table_id.is_empty() {
                "<unnamed>"
            } else {
                o.table_id.as_str()
            };
            println!(
                "override loaded: {id} declares {} processors (from {})",
                o.processors, o.file
            );
        }
        let mut ids: Vec<&str> = loaded.iter().map(|o| o.table_id.as_str()).collect();
        ids.sort_unstable();
        let dups: Vec<&str> = ids
            .windows(2)
            .filter(|w| w[0] == w[1])
            .map(|w| w[0])
            .collect();
        if !dups.is_empty() {
            println!(
                "! {} loaded twice — two sources are supplying it ({ACPI_CPIO_PATH} chained\n  \
                 into the initrd line, plus the initramfs). If they ever differ, which one wins\n  \
                 is undefined.",
                dups.join(", ")
            );
        }
        println!(
            "override covers: {} processors",
            loaded.iter().map(|o| o.processors).max().unwrap_or(0)
        );
    }

    println!(
        "threads without cpuidle: {} {}",
        no_idle.len(),
        fmt_cpus(&no_idle)
    );
    println!(
        "threads without cpufreq: {} {}",
        no_freq.len(),
        fmt_cpus(&no_freq)
    );

    if !no_idle.is_empty() || !no_freq.is_empty() {
        println!();
        println!(
            "! {} thread(s) are outside the override's coverage, so the cores they belong to\n  \
             cannot enter C-states and cannot scale frequency.",
            no_idle.len().max(no_freq.len())
        );
        println!(
            "  hint: `arieltune apu cores acpi install` stages tables covering {} processors",
            s.threads
        );
    }
    Ok(())
}

/// `aputune cores acpi install` — stage the 8-core tables + rebuild initramfs.
pub fn acpi_install() -> Result<()> {
    if !Path::new(MKINITCPIO_CONF).exists() {
        bail!(
            "{MKINITCPIO_CONF} not found — initcpio ACPI override needs it (Arch-family initramfs)"
        );
    }
    fs::create_dir_all(ACPI_OVERRIDE_DIR).context("mkdir acpi override")?;
    fs::create_dir_all(ACPI_BACKUP_DIR).context("mkdir acpi backup")?;
    for (name, url, sha) in ACPI_TABLES {
        let dst = Path::new(ACPI_OVERRIDE_DIR).join(name);
        if dst.exists() {
            fs::copy(&dst, Path::new(ACPI_BACKUP_DIR).join(name))
                .with_context(|| format!("backup {name}"))?;
        }
        let st = Command::new("curl")
            .args(["-fsSL", "-o"])
            .arg(&dst)
            .arg(*url)
            .status()?;
        if !st.success() {
            bail!("download of {name} failed (network needed)");
        }
        // Pin the artifact: refuse a changed AML instead of staging it.
        let out = Command::new("sha256sum").arg(&dst).output()?;
        let got = String::from_utf8_lossy(&out.stdout);
        let got = got.split_whitespace().next().unwrap_or("");
        if got != *sha {
            bail!("sha256 mismatch for {name} (expected {sha}, got {got}) — refusing staged AML");
        }
        println!("staged {name} (backup kept in {ACPI_BACKUP_DIR})");
    }
    // Ensure the acpi_override hook exists.
    let conf = fs::read_to_string(MKINITCPIO_CONF)?;
    let mut new_conf = conf.clone();
    if !conf.contains("acpi_override") {
        if let Some(hooks_line) = conf.lines().find(|l| l.starts_with("HOOKS=")) {
            let inner = &hooks_line[6..];
            let mut hooks: Vec<&str> = inner
                .trim_start_matches('(')
                .trim_end_matches(')')
                .split_whitespace()
                .collect();
            // Insert before the last hook (usually filesystems/encrypt...).
            let insert_at = hooks.len().saturating_sub(1).min(if hooks.is_empty() {
                0
            } else {
                hooks.len() - 1
            });
            hooks.insert(insert_at, "acpi_override");
            new_conf = new_conf.replace(hooks_line, &format!("HOOKS=({})", hooks.join(" ")));
        } else {
            bail!("no HOOKS= line in {MKINITCPIO_CONF}");
        }
    }
    if new_conf != conf {
        fs::write(MKINITCPIO_CONF, &new_conf).context("update mkinitcpio.conf")?;
        println!("added acpi_override hook to HOOKS");
    }
    // Rebuild the image this machine actually boots — not `mkinitcpio -P`.
    //
    // Same root cause as the CU drop-in: a blade booted from a snapshot image
    // has no preset that produces it, so `-P` reports success while leaving the
    // booted initramfs untouched, and the SSDT override never reaches early
    // boot. Verified on blade 115: the booted image was missing
    // `kernel/firmware/acpi/SSDT-*.aml` that had been staged in /etc for days.
    println!("{}", crate::initramfs::rebuild_boot_image()?);
    // Confirm the tables really are in the image rather than assuming the hook
    // did the right thing (the hook can legitimately change).
    if let Some(image) = crate::initramfs::boot_initramfs() {
        for (name, _, _) in ACPI_TABLES {
            let inside = format!("kernel/firmware/acpi/{name}");
            if !crate::initramfs::image_contains(&image, &inside) {
                println!(
                    "note: {inside} is not inside {} — check `lsinitcpio {}`",
                    image.display(),
                    image.display()
                );
            }
        }
    }
    // The staged pair must cover at least as many processors as this box has, or
    // the surplus threads come back with no C-states and no frequency control --
    // silently. That is exactly how blade 105 lost cpu12-15 to an older
    // 12-processor pair. Over-coverage is free; under-coverage is the only
    // failure mode, so say something now rather than after the reboot.
    let (threads, _) = thread_counts();
    for (name, _, _) in ACPI_TABLES {
        let path = Path::new(ACPI_OVERRIDE_DIR).join(name);
        match fs::read(&path) {
            Ok(bytes) => {
                let n = processor_names(&bytes, b"P0").len() as u32;
                if n < threads {
                    println!(
                        "WARNING: {name} declares {n} processors but this box has {threads} \
                         threads — the surplus will have no C-states after the reboot"
                    );
                }
            }
            Err(e) => {
                println!(
                    "note: could not read {} to check coverage ({e})",
                    path.display()
                )
            }
        }
    }
    // A second source can shadow or duplicate these. GRUB chains the override
    // cpio into the same initrd line, and a GRUB `initrd` chain never appears in
    // /proc/cmdline -- so point at the check rather than letting the two copies
    // silently disagree.
    if Path::new(ACPI_CPIO_PATH).exists() {
        println!(
            "note: {ACPI_CPIO_PATH} also exists. If GRUB chains it into the initrd line, these \
             tables load twice and precedence between the copies is undefined — check with \
             `grep acpi_override /boot/grub/grub.cfg`."
        );
    }
    println!("done — reboot for the 8-core tables to take effect");
    Ok(())
}

/// `aputune cores acpi revert`.
pub fn acpi_revert() -> Result<()> {
    let mut reverted = false;
    for (name, _, _) in ACPI_TABLES {
        let dst = Path::new(ACPI_OVERRIDE_DIR).join(name);
        let bak = Path::new(ACPI_BACKUP_DIR).join(name);
        if bak.exists() {
            fs::copy(&bak, &dst).with_context(|| format!("restore {name}"))?;
            reverted = true;
        } else {
            let _ = fs::remove_file(&dst);
        }
    }
    if reverted {
        println!("restored previous tables");
    } else {
        println!("no backup found — override tables removed");
    }
    if Path::new(MKINITCPIO_CONF).exists() {
        // Same reason as `acpi_install`: rebuild the *booted* image, not the
        // presets, or the removal never reaches early boot.
        println!("{}", crate::initramfs::rebuild_boot_image()?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The name-scheme trap, in one test. This firmware's DSDT declares its
    /// processors as `P0xx`; AMD's own CPU SSDT refers to `C0xx`, which does not
    /// exist here and can never attach. Counting the wrong prefix measures a
    /// table that cannot work — which is what the old `cst_objects` did.
    #[test]
    fn counts_only_the_prefix_that_can_attach() {
        let mut blob = Vec::new();
        blob.extend_from_slice(b"C000"); // referenced but undefined on this firmware
        blob.extend_from_slice(b"P000"); // the name the DSDT actually declares
        blob.extend_from_slice(b"P001");
        blob.extend_from_slice(b"P001"); // duplicate must not double-count
        assert_eq!(processor_names(&blob, b"P0").len(), 2);
        assert_eq!(processor_names(&blob, b"C0").len(), 1);
    }

    /// Four hex characters are required; a near-miss is not a processor name.
    #[test]
    fn ignores_non_processor_names() {
        let got = processor_names(b"P0ZZ P0 P000", b"P0");
        assert!(got.contains("P000"), "{got:?}");
        assert!(!got.iter().any(|s| s.contains('Z')), "{got:?}");
    }

    /// The OEM table id lives at offset 16. It is how the kernel (and this code)
    /// tells `SSDT-CST` from `SSDT-PST` when both carry the same OEM id, and so
    /// how a duplicated table gets noticed at all.
    #[test]
    fn reads_the_oem_table_id_from_a_table_header() {
        let mut t = vec![0u8; 36];
        t[..4].copy_from_slice(b"SSDT");
        t[10..16].copy_from_slice(b"HACK  ");
        t[16..24].copy_from_slice(b"P_CST3  ");
        assert_eq!(acpi_oem_table_id(&t).as_deref(), Some("P_CST3"));
        // Too short to hold a header: no claim, and no panic.
        assert_eq!(acpi_oem_table_id(&t[..8]), None);
    }

    /// The OEM id lives at offset 10 of the ACPI header. It is how coverage gets
    /// attributed to a real loaded override instead of the firmware's own
    /// tables, which declare more processors than are active.
    #[test]
    fn reads_the_oem_id_from_a_table_header() {
        let mut t = vec![0u8; 36];
        t[..4].copy_from_slice(b"SSDT");
        t[10..16].copy_from_slice(b"HACK  ");
        assert_eq!(acpi_oem_id(&t).as_deref(), Some("HACK"));
        // Too short to hold a header: no claim, and no panic.
        assert_eq!(acpi_oem_id(&t[..8]), None);
    }

    /// Threads are rendered as `cpuN`, so a threaded list in the output cannot
    /// be read as a core count — the units confusion this replaced.
    #[test]
    fn renders_cpu_names() {
        assert_eq!(fmt_cpus(&[12, 13, 14, 15]), "cpu12 cpu13 cpu14 cpu15");
        assert_eq!(fmt_cpus(&[]), "");
    }

    #[test]
    fn state_machine() {
        assert_eq!(state_for(CORE_MASK_STOCK, 6), CoreState::Locked);
        assert_eq!(state_for(CORE_MASK_STOCK, 8), CoreState::Locked);
        assert_eq!(state_for(CORE_MASK_FULL, 6), CoreState::PendingReboot);
        assert_eq!(state_for(CORE_MASK_FULL, 8), CoreState::Unlocked);
        assert_eq!(state_for(0x7F, 7), CoreState::Abnormal(0x7F));
        assert_eq!(state_for(0x03, 2), CoreState::Abnormal(0x03));
        assert_eq!(state_for(0x00, 0), CoreState::Abnormal(0x00));
    }

    /// A per-die factory mask must read as `Locked`, not `Abnormal`. Blade 115
    /// ships `0xD7`; judging it against the textbook `0x77` is what sent it down
    /// the forced path and made `cores apply` refuse a mask `cores boot` accepts.
    #[test]
    fn recorded_stock_is_not_abnormal() {
        // With a baseline for that mask, the factory value is simply "locked".
        assert_eq!(state_for_stock(0xD7, 6, 0xD7), CoreState::Locked);
        assert_eq!(
            state_for_stock(CORE_MASK_STOCK, 6, CORE_MASK_STOCK),
            CoreState::Locked
        );
        // Without a baseline for it, the mask stays Abnormal so the escape hatch
        // still has something to warn about.
        assert_eq!(
            state_for_stock(0xD7, 6, CORE_MASK_STOCK),
            CoreState::Abnormal(0xD7)
        );
        assert_eq!(
            state_for_stock(CORE_MASK_STOCK, 6, 0xD7),
            CoreState::Abnormal(0x77)
        );
        // The full mask is judged the same way whatever the baseline says.
        assert_eq!(
            state_for_stock(CORE_MASK_FULL, 8, 0xD7),
            CoreState::Unlocked
        );
        assert_eq!(
            state_for_stock(CORE_MASK_FULL, 6, 0xD7),
            CoreState::PendingReboot
        );
    }

    #[test]
    fn mask_descriptions() {
        assert!(describe_mask(0x77).contains("enabled=[0 1 2 4 5 6]"));
        assert!(describe_mask(0x77).contains("disabled=[3 7]"));
        assert!(describe_mask(0xFF).contains("enabled=[0 1 2 3 4 5 6 7]"));
    }
}
