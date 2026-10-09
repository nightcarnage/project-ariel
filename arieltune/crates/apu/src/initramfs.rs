// SPDX-License-Identifier: GPL-2.0-only
//! Rebuilding the initramfs that this machine will **actually boot**.
//!
//! ## Why this module exists
//!
//! The 40-CU liberation (`bc250_cc_write_mode`) is not a runtime knob: amdgpu is
//! loaded from the initramfs for early KMS, so the surrounding
//! `/etc/modprobe.d/*.conf` drop-ins are copied *into the image* and the in-image
//! copy is what the kernel consults when the module loads. Editing
//! `/etc/modprobe.d/` on its own therefore changes nothing about the next boot.
//!
//! The obvious follow-up — "then run `mkinitcpio -P`" — is what the CLI has
//! always told the user, and it is wrong on the blades we actually run. A blade
//! booted from a *snapshot* entry uses `/boot/initramfs-snap-<hash>.img`, and
//! **no preset produces that image**: `/etc/mkinitcpio.d/*.preset` only lists
//! `linux-cachyos-bore` and `linux-cachyos-lts`. `mkinitcpio -P` happily rebuilds
//! those two, reports success, and leaves the booted image untouched. The user is
//! told the change will take effect on the next boot; it will not.
//!
//! The failure is quiet and confusing: a parameter that was already baked in
//! keeps working while a newly added one silently stays at its default, which
//! looks like "the new option doesn't work" rather than "the image was never
//! rebuilt". Observed on blade 115 on 2026-10-07 — its booted image was missing
//! `etc/modprobe.d/smiflash.conf` and the `kernel/firmware/acpi/SSDT-*.aml` files
//! that had been in `/etc` for days, which is directly visible in
//! `lsinitcpio <image>`.
//!
//! So instead of guessing at presets, we find the image this boot came from and
//! rebuild *that* one. `BOOT_IMAGE=` in `/proc/cmdline` names the kernel the
//! bootloader handed over, and the image sits beside it under a symmetric name.
//!
//! Callers are expected to verify the result (`image_contains`) rather than trust
//! us — see `cu::enable_persist`, which confirms its drop-in really is in the
//! rebuilt image before telling the user the change will take effect.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use std::process::Command;

/// The kernel release we are running right now.
///
/// This is the version `mkinitcpio -k` wants, and it is deliberately taken from
/// the running kernel rather than from a preset: the whole point of this module
/// is to rebuild for *this* boot, not for whatever the presets happen to list.
fn running_kver() -> String {
    ariel_hal::running_kernel()
}

/// Derive the initramfs path that pairs with a `BOOT_IMAGE=` kernel path.
///
/// The naming convention is symmetric on these blades:
///
/// ```text
///   /boot/vmlinuz-snap-11e61755        <->  /boot/initramfs-snap-11e61755.img
///   /boot/vmlinuz-linux-cachyos-bore   <->  /boot/initramfs-linux-cachyos-bore.img
/// ```
///
/// so swapping the `vmlinuz-` component for `initramfs-` and appending `.img`
/// names the very image the bootloader passed to the kernel. Returns `None` for
/// anything that does not follow the convention (a UKI, an EFI stub, a bare
/// `vmlinuz` with no suffix) — the caller then falls back to the presets instead
/// of inventing a path.
///
/// Pure by construction: `BOOT_IMAGE` is bootloader-supplied text, so this is
/// unit-tested rather than trusted.
pub fn initramfs_for_boot_image(boot_image: &str) -> Option<PathBuf> {
    let path = Path::new(boot_image);
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_prefix("vmlinuz-")?;
    if stem.is_empty() {
        return None;
    }
    Some(path.with_file_name(format!("initramfs-{stem}.img")))
}

/// `BOOT_IMAGE=` from the live kernel command line, if the bootloader set it.
pub fn boot_image() -> Option<String> {
    let cmdline = fs::read_to_string("/proc/cmdline").ok()?;
    cmdline
        .split_whitespace()
        .find_map(|w| w.strip_prefix("BOOT_IMAGE="))
        .map(str::to_string)
}

/// The initramfs this boot came from, or `None` if it cannot be determined.
pub fn boot_initramfs() -> Option<PathBuf> {
    initramfs_for_boot_image(&boot_image()?)
}

/// The snapshot id implied by a `BOOT_IMAGE=` path, when it names one.
///
/// ```text
///   /boot/vmlinuz-snap-bc0f6654       ->  Some("snap-bc0f6654")
///   /boot/vmlinuz-linux-cachyos-bore  ->  None
/// ```
///
/// The snapshotter's ids are content-addressed over the module, the initramfs
/// and the vmlinuz, so the id *is* the provenance proof — there is no need to
/// re-hash the image to say which artefact we are describing.
///
/// Pure: `BOOT_IMAGE` is bootloader-supplied text, so it is unit-tested rather
/// than trusted.
pub fn snapshot_id_for_boot_image(boot_image: &str) -> Option<String> {
    let stem = Path::new(boot_image)
        .file_name()?
        .to_str()?
        .strip_prefix("vmlinuz-")?;
    stem.starts_with("snap-").then(|| stem.to_string())
}

/// What the `snapshot` tool was able to say about a boot's snapshot id.
///
/// Four outcomes, kept apart on purpose. "No tool here", "a tool here that is
/// not ours", "the tool listed its ids and yours is not among them" and "your
/// snapshot is registered" are four different facts, and collapsing any two of
/// them produces a report that reads like a finding when it is really an
/// artefact of the host.
///
/// Not hypothetical: this was written on a workstation whose `/usr/bin/snapshot`
/// belongs to an unrelated program which answers `Unknown option --ids`. Calling
/// that "not installed" would be a small lie inside the one command whose entire
/// job is to say what it actually saw.
#[derive(Debug, PartialEq, Eq)]
enum StoreVerdict {
    /// Listed its ids, and this one is among them.
    Known,
    /// Listed its ids, and this one is not among them.
    Unknown,
    /// Something called `snapshot` is on PATH but would not list ids -- a
    /// different program, or one whose store is unavailable.
    Unusable,
    /// Nothing on PATH answered as a snapshot tool at all.
    Unavailable,
}

/// Ask the `snapshot` tool about `id`. Never fails, never guesses.
///
/// Only the machine-readable mode is consulted (`snapshot list --ids`), so a
/// presentation change in that tool cannot break this silently.
///
/// The name was the bc-250-snapshotter shell tool's; it is now a symlink to this
/// suite's `snapshot` namespace, so on a blade this asks our own store by name --
/// which is exactly why `list --ids` is kept machine-compatible rather than merely
/// human-readable.
fn store_verdict(id: &str) -> StoreVerdict {
    let Ok(out) = Command::new("snapshot").args(["list", "--ids"]).output() else {
        return StoreVerdict::Unavailable;
    };
    if !out.status.success() {
        return StoreVerdict::Unusable;
    }
    let listed = String::from_utf8_lossy(&out.stdout);
    if listed.lines().map(str::trim).any(|line| line == id) {
        StoreVerdict::Known
    } else {
        StoreVerdict::Unknown
    }
}

/// Describe **which artefact** the numbers printed around it are describing.
///
/// Not decoration, and cheap. Every confusing result in the 2026-10-07/08
/// investigation came from a report that described "the machine" while having
/// actually read one object:
///
///   * blade 105's stale 12-processor ACPI tables came from its *snapshot's*
///     initramfs, not from `/etc/initcpio/`, where they had been staged for days;
///   * `mkinitcpio -P` reports success while leaving a snapshot image untouched;
///   * the same ACPI tables load twice because GRUB chains a cpio onto that image
///     as well as the image carrying its own copy.
///
/// Naming the booted kernel, the image it came from and the snapshot id that
/// content-addresses both is the cheapest way to stop that class of mistake. It
/// needs no privilege and performs no write.
pub fn boot_provenance() -> Vec<String> {
    let Some(image) = boot_image() else {
        return vec!["booted image    : unknown (no BOOT_IMAGE= in /proc/cmdline)".to_string()];
    };
    let mut lines = vec![format!("booted kernel   : {image}")];

    match boot_initramfs() {
        Some(irf) => {
            let described = match fs::metadata(&irf) {
                Ok(m) => {
                    let mib = m.len() as f64 / (1024.0 * 1024.0);
                    // Relative age rather than an absolute date: no date library,
                    // and "how long ago was this image built" is the question that
                    // matters when chasing a change that never took effect.
                    let age = m
                        .modified()
                        .ok()
                        .and_then(|t| SystemTime::now().duration_since(t).ok())
                        .map(|d| format!("{}s ago", d.as_secs()))
                        .unwrap_or_else(|| "age unknown".to_string());
                    format!("{mib:.1} MiB, modified {age}")
                }
                Err(_) => "MISSING".to_string(),
            };
            lines.push(format!("booted initramfs: {} ({described})", irf.display()));
        }
        None => lines.push(
            "booted initramfs: not derived (name does not follow vmlinuz-/initramfs-)".to_string(),
        ),
    }

    if let Some(id) = snapshot_id_for_boot_image(&image) {
        let what = match store_verdict(&id) {
            StoreVerdict::Known => "known to the snapshot store",
            StoreVerdict::Unknown => {
                "NOT known to the snapshot store (pruned, or never registered)"
            }
            StoreVerdict::Unusable => {
                "a `snapshot` on PATH would not list ids (a different program, or a store it \
                 cannot read)"
            }
            StoreVerdict::Unavailable => "no `snapshot` tool on PATH",
        };
        lines.push(format!("snapshot        : {id} — {what}"));
    }
    lines
}

/// Run `mkinitcpio` with `args` and fail loudly with its output on error.
///
/// We do **not** swallow a non-zero exit: a silent failure here is precisely the
/// bug this module exists to remove, so the command's own stderr is surfaced.
fn run_mkinitcpio(args: &[&str]) -> Result<()> {
    let out = Command::new("mkinitcpio")
        .args(args)
        .output()
        .context("spawn mkinitcpio (is mkinitcpio installed?)")?;
    if !out.status.success() {
        // Include both streams: mkinitcpio reports the useful part on stdout and
        // the fatal reason on stderr depending on where it gave up.
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!(
            "mkinitcpio {} failed ({})\n--- stdout ---\n{}\n--- stderr ---\n{}",
            args.join(" "),
            out.status,
            stdout.trim(),
            stderr.trim()
        );
    }
    Ok(())
}

/// Rebuild whichever initramfs this machine will boot, and describe what was done.
///
/// * A `BOOT_IMAGE`-derived image that exists → rebuild exactly that image with
///   `mkinitcpio -g <image> -k <running kver>`.
/// * Otherwise → fall back to `mkinitcpio -P` (the classic preset rebuild), which
///   is correct for a normal, preset-booted system.
///
/// The returned string is meant to be *shown to the user*: it says which image
/// was rebuilt, so a failure to reach the boot path is visible rather than
/// implied. Callers that changed something specific inside the image should
/// follow up with [`image_contains`] and report that too.
pub fn rebuild_boot_image() -> Result<String> {
    let kver = running_kver();

    match boot_initramfs() {
        Some(image) if image.exists() => {
            let image_str = image
                .to_str()
                .context("boot initramfs path is not valid UTF-8")?;
            run_mkinitcpio(&["-g", image_str, "-k", &kver])?;
            Ok(format!(
                "rebuilt the booted image {} for kernel {}",
                image.display(),
                kver
            ))
        }
        Some(image) => {
            // BOOT_IMAGE named something we can compute but that is not on disk
            // (image renamed or removed). Rebuilding the presets is still the
            // best guess, but say so instead of pretending it was targeted.
            run_mkinitcpio(&["-P"])?;
            Ok(format!(
                "BOOT_IMAGE pointed at {}, which does not exist; rebuilt presets with \
                 `mkinitcpio -P` for kernel {} instead",
                image.display(),
                kver
            ))
        }
        None => {
            run_mkinitcpio(&["-P"])?;
            Ok(format!(
                "no BOOT_IMAGE-derived image (not a conventional vmlinuz-* boot); \
                 rebuilt presets with `mkinitcpio -P` for kernel {}",
                kver
            ))
        }
    }
}

/// Best-effort check that `needle` (an in-image path, e.g.
/// `etc/modprobe.d/aputune-40cu.conf`) is present in `image`.
///
/// Used by callers to *verify* their own edit landed rather than assert it. We
/// match against `lsinitcpio`'s listing — for `etc/modprobe.d/...` that is the
/// exact path inside the cpio, and for the ACPI case
/// (`kernel/firmware/acpi/SSDT-CST.aml`) it is the firmware path the early
/// initramfs loader reads. A `false` here is treated as advisory, not fatal:
/// whether a file is *supposed* to be in the image is the caller's business, and
/// the hooks that decide it can legitimately change.
pub fn image_contains(image: &Path, needle: &str) -> bool {
    let Ok(out) = Command::new("lsinitcpio").arg(image).output() else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    String::from_utf8_lossy(&out.stdout).contains(needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The snapshot naming used by the blades: `vmlinuz-snap-11e61755` boots
    /// `initramfs-snap-11e61755.img`. This is the case that `mkinitcpio -P`
    /// misses, so it is the one that must keep working.
    #[test]
    fn derives_the_snapshot_image() {
        assert_eq!(
            initramfs_for_boot_image("/boot/vmlinuz-snap-11e61755"),
            Some(PathBuf::from("/boot/initramfs-snap-11e61755.img"))
        );
    }

    /// The ordinary preset naming must keep working too, so a non-snapshot
    /// system still gets a targeted rebuild rather than falling back to `-P`.
    #[test]
    fn derives_the_preset_image() {
        assert_eq!(
            initramfs_for_boot_image("/boot/vmlinuz-linux-cachyos-bore"),
            Some(PathBuf::from("/boot/initramfs-linux-cachyos-bore.img"))
        );
    }

    /// Anything we do not recognise must return `None` so the caller falls back
    /// to `mkinitcpio -P` instead of guessing at a path and failing the boot.
    #[test]
    fn refuses_to_guess_for_unconventional_kernels() {
        // Bare `vmlinuz` (no dash) — no suffix to mirror.
        assert_eq!(initramfs_for_boot_image("/boot/vmlinuz"), None);
        // A UKI / EFI stub is not paired with a loose initramfs at all.
        assert_eq!(initramfs_for_boot_image("/efi/EFI/Linux/arch.efi"), None);
        // Empty stem after the prefix would name `/boot/initramfs-.img`.
        assert_eq!(initramfs_for_boot_image("/boot/vmlinuz-"), None);
    }

    /// A snapshot boot names its snapshot; anything else must not. This is what
    /// decides whether to ask the `snapshot` tool about this boot at all.
    #[test]
    fn extracts_the_snapshot_id_from_the_boot_image() {
        assert_eq!(
            snapshot_id_for_boot_image("/boot/vmlinuz-snap-bc0f6654").as_deref(),
            Some("snap-bc0f6654")
        );
        // A preset kernel is not a snapshot and must not be labelled as one.
        assert_eq!(
            snapshot_id_for_boot_image("/boot/vmlinuz-linux-cachyos-bore"),
            None
        );
        // Unconventional names yield nothing rather than a guess.
        assert_eq!(snapshot_id_for_boot_image("/boot/vmlinuz"), None);
        assert_eq!(snapshot_id_for_boot_image("/efi/EFI/Linux/arch.efi"), None);
    }

    /// Provenance must never panic and never overclaim. This runs on ordinary
    /// developer machines with no BC-250, no snapshot image and usually no
    /// `snapshot` tool -- which is exactly the "say so, do not guess" path that a
    /// blade provisioned from packages also takes.
    #[test]
    fn provenance_degrades_without_claiming_failure() {
        let lines = boot_provenance();
        assert!(
            !lines.is_empty(),
            "must always say something about the boot"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("booted kernel") || l.starts_with("booted image")),
            "must name the kernel, or admit it cannot: {lines:?}"
        );
        // Never report a snapshot for a boot that is not one.
        if snapshot_id_for_boot_image(&boot_image().unwrap_or_default()).is_none() {
            assert!(
                !lines.iter().any(|l| l.starts_with("snapshot")),
                "a non-snapshot boot must not be labelled with a snapshot: {lines:?}"
            );
        }
    }

    /// Exercises the tool-facing path, and pins the distinction the verdicts
    /// exist to preserve.
    ///
    /// Every other test here reaches only the no-tool case, because they run on
    /// machines without a usable `snapshot` tool. That leaves the interesting state
    /// untested: a boot entry whose snapshot was pruned or never registered.
    ///
    /// The assertion is written against whatever this host's tool actually did,
    /// so it holds everywhere and still exercises the real path on a blade --
    /// where `snapshot list --ids` succeeds and the verdict must be `Unknown`.
    #[test]
    fn verdict_matches_what_the_tool_on_this_host_did() {
        let listed = Command::new("snapshot").args(["list", "--ids"]).output();
        let verdict = store_verdict("snap-ffffffff");
        match listed {
            // A usable tool must call a fabricated id UNKNOWN -- never KNOWN, and
            // never confused with the tool itself being absent.
            Ok(o) if o.status.success() => assert_eq!(verdict, StoreVerdict::Unknown),
            Ok(_) => assert_eq!(verdict, StoreVerdict::Unusable),
            Err(_) => assert_eq!(verdict, StoreVerdict::Unavailable),
        }
    }
}
