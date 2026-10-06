// SPDX-License-Identifier: GPL-2.0-only
//! Prebuilt `smiflash` modules — one per supported kernel series.
//!
//! [`crate::smm::build`] can compile the module on the board, but that needs the
//! *booted* kernel's build tree. That tree is not guaranteed to exist: installing
//! a newer kernel replaces the tree for the old release, so a snapshot boot into
//! an older kernel can find itself with no tree and no way to build — and the
//! driver is then silently unavailable. Losing the 7.0.9 tree on blade 115 did
//! exactly that, on the same afternoon the 7.0.9 snapshots stopped booting.
//!
//! The fan driver (`nct6687`) never had that failure mode because it ships
//! prebuilt blobs inside the binary and installs whichever one matches the
//! running kernel. This module gives `smiflash` the same guarantee, keeping the
//! on-board DKMS build as the fallback for kernels we do not carry.
//!
//! The table key is the exact release string, `-bore` suffix included. That
//! suffix is the whole risk: a module built from the plain `linux-cachyos/` tree
//! reports `7.0.9-1-cachyos`, `depmod` indexes it quite happily, and the `-bore`
//! kernel then refuses to load it. [`tests::every_embedded_module_declares_its_own_key`]
//! pins the blobs to their keys so that mistake cannot ship again.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// Overlay directory inside the running kernel's module tree. `updates/` wins
/// over the in-tree copy and is the same place the fan driver installs to.
const INSTALL_DIR: &str = "updates";

/// Prebuilt `smiflash` modules, keyed by the exact kernel release they were
/// built against.
///
/// Two entries for two kernels: the supported set is `Kernel::ALL` —
/// `7.2.9-1-cachyos-bore` and `7.0.9-1-cachyos-bore` — and both are covered
/// here, so every supported kernel has a driver with no build tree required.
///
/// This table is deliberately **not** symmetric with the fan driver's
/// `NCT6687_KO`, which carries a third entry for the older stock
/// `7.0.9-1-cachyos` build. That asymmetry is the point, not an omission: for
/// the fan driver the fallback is *read-only sensors*, so covering that flavour
/// buys real function. Here the fallback is the driver simply being absent from
/// an optional BIOS-editing path, against a fleet steering to the two `-bore`
/// series — so there is nothing to buy. A kernel outside this table falls
/// through to the on-board build, which reports itself as such.
const SMIFLASH_KO: &[(&str, &[u8])] = &[
    (
        "7.2.9-1-cachyos-bore",
        include_bytes!("../driver/prebuilt/smiflash-7.2.9-1-cachyos-bore.ko"),
    ),
    (
        "7.0.9-1-cachyos-bore",
        include_bytes!("../driver/prebuilt/smiflash-7.0.9-1-cachyos-bore.ko"),
    ),
];

/// Running kernel release (`uname -r`).
pub fn running_kver() -> String {
    fs::read_to_string("/proc/sys/kernel/osrelease")
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// The embedded module matching `kver`, when we carry one.
pub fn blob(kver: &str) -> Option<&'static [u8]> {
    SMIFLASH_KO
        .iter()
        .find(|(k, _)| *k == kver)
        .map(|(_, b)| *b)
}

/// Is a prebuilt module available for `kver`?
pub fn have_blob(kver: &str) -> bool {
    blob(kver).is_some()
}

/// Can `modprobe` resolve `smiflash` for the running kernel already?
pub(crate) fn modprobe_resolves() -> bool {
    Command::new("modinfo")
        .arg("smiflash")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Install the embedded driver for the running kernel when `modprobe` cannot
/// already resolve it. Returns true when the module is available to load
/// afterwards.
///
/// Mirrors the fan driver's `install_writable_module`: best-effort, needs root,
/// and a kernel we carry no blob for returns false so the caller can fall back
/// to the on-board build. We never force a mismatched blob in — a `.ko` with the
/// wrong vermagic gets indexed by depmod and then fails to bind, which is worse
/// than having none.
pub fn install_embedded() -> bool {
    if modprobe_resolves() {
        return true;
    }
    let kver = running_kver();
    let Some(bytes) = blob(&kver) else {
        return false;
    };
    let dst = PathBuf::from(format!("/lib/modules/{kver}/{INSTALL_DIR}/smiflash.ko"));
    if let Some(parent) = dst.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if fs::write(&dst, bytes).is_err() {
        return false;
    }
    let _ = Command::new("depmod").arg("-a").status();
    modprobe_resolves()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the table: both supported series resolve.
    #[test]
    fn every_supported_series_has_an_embedded_module() {
        for k in ["7.2.9-1-cachyos-bore", "7.0.9-1-cachyos-bore"] {
            let b = blob(k).unwrap_or_else(|| panic!("no embedded smiflash for {k}"));
            assert!(b.len() > 10_000, "{k}: blob is implausibly small");
        }
    }

    /// A kernel we do not ship must fall through to the on-board build rather
    /// than being handed a near-miss. `7.0.9-1-cachyos` (no `-bore`) is the
    /// dangerous near-miss, so it is called out explicitly.
    #[test]
    fn an_unsupported_kernel_gets_no_blob() {
        assert!(blob("7.0.9-1-cachyos").is_none());
        assert!(blob("6.19.9-1-cachyos").is_none());
        assert!(blob("6.18.55-1-cachyos-lts").is_none());
        assert!(blob("").is_none());
        assert!(!have_blob("7.0.9-1-cachyos"));
    }

    /// Each blob must declare the release it is keyed under, as an ELF object.
    ///
    /// This is the regression guard for the mistake that cost real time: a
    /// module built from the plain `linux-cachyos/` tree carries
    /// `7.0.9-1-cachyos` in its modinfo banner. It installs, `depmod` the
    /// indexes, and only the kernel refuses it — the failure surfaces as
    /// "unknown filesystem type" a boot later, nowhere near the cause.
    #[test]
    fn every_embedded_module_declares_its_own_key() {
        for (kver, bytes) in SMIFLASH_KO {
            assert!(
                bytes.starts_with(b"\x7fELF"),
                "{kver}: embedded module is not an ELF object"
            );
            let banner = String::from_utf8_lossy(bytes);
            assert!(
                banner.contains(kver),
                "{kver}: release string absent from the blob — wrong kernel flavour?"
            );
        }
    }

    /// Two series must not ship the same bytes; identical blobs would mean one
    /// of them was built against the wrong tree.
    #[test]
    fn the_embedded_modules_are_distinct_binaries() {
        for (i, (ka, a)) in SMIFLASH_KO.iter().enumerate() {
            for (kb, b) in &SMIFLASH_KO[i + 1..] {
                assert_ne!(a, b, "{ka} and {kb} embed identical bytes");
            }
        }
        assert_eq!(SMIFLASH_KO.len(), 2, "expected exactly the two bore series");
    }
}
