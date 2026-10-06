// SPDX-License-Identifier: GPL-2.0-only
//! Which kernel a build targets, and which one is currently booted.
//!
//! Two kernels are supported at once, and both ship inside the binary: the
//! liberation series, the prebuilt nct6687 module, the PKGBUILD pin and the
//! toolchain notes all vary per kernel. "Which kernel" is therefore a
//! first-class value rather than a constant baked at compile time.
//!
//! 7.2.9 is the default target. 7.0.9 is still supported because blades in the
//! field boot it, and because a snapshot setup can drift back and forth between
//! the two while validating — one binary must serve both, or the validation is
//! measuring the binary rather than the kernel.
//!
//! Nothing here reads the *target's* kernel: a build may be aimed at a remote
//! host, so the target is always explicit (see [`Kernel::DEFAULT`] and the
//! `--kernel` flag). [`Kernel::booted`] answers a different question — what is
//! running *here* — and is what the patch-state probe and the TUI use.

use std::sync::OnceLock;

/// A supported CachyOS kernel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kernel {
    /// `linux-cachyos-bore` 7.2.9-1 — the default target.
    Bore729,
    /// `linux-cachyos` 7.0.9-1 — still supported, no longer the default.
    Bore709,
}

impl Kernel {
    /// Target used when neither a flag nor the environment picks one.
    pub const DEFAULT: Kernel = Kernel::Bore729;

    /// Every supported kernel, default first (the order the UI lists them in).
    pub const ALL: [Kernel; 2] = [Kernel::Bore729, Kernel::Bore709];

    /// `uname -r` this kernel reports, i.e. the vermagic a module must match.
    ///
    /// Both targets are built from the `linux-cachyos-bore/` PKGBUILD, so both
    /// carry the `-cachyos-bore` suffix and differ only in the version. That
    /// suffix is not cosmetic: a prebuilt module built for one release will not
    /// load on the other, which is why the release is part of the identity.
    pub fn release(self) -> &'static str {
        match self {
            Kernel::Bore729 => "7.2.9-1-cachyos-bore",
            Kernel::Bore709 => "7.0.9-1-cachyos-bore",
        }
    }

    /// Short human label (`7.2.9`), for headers and menus.
    pub fn label(self) -> &'static str {
        match self {
            Kernel::Bore729 => "7.2.9",
            Kernel::Bore709 => "7.0.9",
        }
    }

    /// Directory under `crates/apu/patches/` holding this kernel's series.
    pub fn series_dir(self) -> &'static str {
        match self {
            Kernel::Bore729 => "bc250-cachyos-7.2.9",
            Kernel::Bore709 => "bc250-cachyos-7.0.9",
        }
    }

    /// Commit in `CachyOS/linux-cachyos` that produces this kernel.
    ///
    /// Quoted in the "no PKGBUILD dir" hint so a fresh install has an exact,
    /// known-good checkout rather than whatever `master` happens to be.
    pub fn pkgbuild_pin(self) -> &'static str {
        match self {
            Kernel::Bore729 => "b11ba14854d9748bdb3e3daf7a90e8d3a31004ec",
            Kernel::Bore709 => "791fb8ea6d3cf7c85e596678c25c56fa140591be",
        }
    }

    /// PKGBUILD subdirectory inside `CachyOS/linux-cachyos` for this kernel.
    ///
    /// Both targets use `linux-cachyos-bore/` — that is what the fleet is
    /// provisioned with (`blade-onboard.sh` pins it for 7.0.9 too), and the
    /// plain `linux-cachyos/` would build a release whose vermagic the shipped
    /// prebuilt modules do not match.
    pub fn pkgbuild_subdir(self) -> &'static str {
        match self {
            Kernel::Bore729 => "linux-cachyos-bore",
            Kernel::Bore709 => "linux-cachyos-bore",
        }
    }

    /// Recognise a kernel from a `uname -r`-style release string.
    ///
    /// Deliberately loose: the exact suffix is not known until the module is
    /// built, and a locally built kernel carries a `-local` or `+` suffix. Only
    /// the version is matched, which is what distinguishes the series.
    pub fn from_release(release: &str) -> Option<Kernel> {
        if release.contains("7.2.9") {
            Some(Kernel::Bore729)
        } else if release.contains("7.0.9") {
            Some(Kernel::Bore709)
        } else {
            None
        }
    }

    /// Parse a `--kernel` argument: `7.2.9`, `729`, `72`, `7.0.9`, `709`, `70`.
    pub fn parse(arg: &str) -> Option<Kernel> {
        let a = arg.trim().to_ascii_lowercase();
        let digits: String = a.chars().filter(|c| c.is_ascii_digit()).collect();
        match digits.as_str() {
            "729" | "72" => Some(Kernel::Bore729),
            "709" | "70" => Some(Kernel::Bore709),
            _ => None,
        }
    }

    /// The kernel booted on *this* host, or [`Kernel::DEFAULT`] when it is not
    /// one we support.
    ///
    /// Cached: the answer cannot change while the process runs, and the probe
    /// and the TUI must agree — two callers reading it independently could
    /// otherwise disagree across a reboot mid-session and mis-label the rows.
    pub fn booted() -> Kernel {
        static BOOTED: OnceLock<Kernel> = OnceLock::new();
        *BOOTED.get_or_init(|| {
            std::fs::read_to_string("/proc/sys/kernel/osrelease")
                .ok()
                .and_then(|s| Kernel::from_release(s.trim()))
                .unwrap_or(Kernel::DEFAULT)
        })
    }

    /// True when this kernel is the one currently booted here — the case where
    /// a build's output could be booted straight away without a reboot.
    pub fn is_booted(self) -> bool {
        self == Kernel::booted()
    }
}

impl std::fmt::Display for Kernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_dotted_and_bare_forms() {
        for s in ["7.2.9", "729", "72", " 7.2.9 "] {
            assert_eq!(Kernel::parse(s), Some(Kernel::Bore729), "{s}");
        }
        for s in ["7.0.9", "709", "70"] {
            assert_eq!(Kernel::parse(s), Some(Kernel::Bore709), "{s}");
        }
        assert_eq!(Kernel::parse("7.3"), None);
        assert_eq!(Kernel::parse(""), None);
    }

    #[test]
    fn from_release_tolerates_suffixes() {
        assert_eq!(
            Kernel::from_release("7.2.9-1-cachyos-bore"),
            Some(Kernel::Bore729)
        );
        assert_eq!(
            Kernel::from_release("7.2.9-1-cachyos-bore-local"),
            Some(Kernel::Bore729)
        );
        assert_eq!(
            Kernel::from_release("7.0.9-1-cachyos"),
            Some(Kernel::Bore709)
        );
        assert_eq!(Kernel::from_release("6.19.9-1-cachyos"), None);
    }

    #[test]
    fn the_two_kernels_never_share_a_release_or_series_dir() {
        let (a, b) = (Kernel::Bore729, Kernel::Bore709);
        assert_ne!(a.release(), b.release());
        assert_ne!(a.series_dir(), b.series_dir());
        assert_ne!(a.pkgbuild_pin(), b.pkgbuild_pin());
        // Both are built from `linux-cachyos-bore/` — sharing that subdirectory
        // is expected, and it is the *version* that separates the two.
        assert_eq!(a.pkgbuild_subdir(), "linux-cachyos-bore");
        assert_eq!(b.pkgbuild_subdir(), "linux-cachyos-bore");
    }

    /// The release strings are what the embedded module table is keyed by, so
    /// they have to be exactly what the fleet's kernels report.
    #[test]
    fn release_strings_match_the_fleet() {
        assert_eq!(Kernel::Bore729.release(), "7.2.9-1-cachyos-bore");
        assert_eq!(Kernel::Bore709.release(), "7.0.9-1-cachyos-bore");
    }

    #[test]
    fn default_is_7_2_9() {
        assert_eq!(Kernel::DEFAULT, Kernel::Bore729);
        assert_eq!(Kernel::ALL[0], Kernel::Bore729);
    }
}
