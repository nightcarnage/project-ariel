// SPDX-License-Identifier: GPL-2.0-only
//! Preflight for a snapshot store: is this box's boot configuration coherent?
//!
//! Every other module in the suite has a `doctor` — a read-only pass that answers
//! "what is the state of this thing, and what is wrong with it" in one command.
//! This is SNAP's, and it earns its place by checking the specific ways a store
//! goes wrong rather than restating what `list` already shows.
//!
//! ## What it looks for, and why each one
//!
//! **A menu entry whose files are gone.** The failure that is invisible until it
//! matters: `/boot/grub/grub.cfg` keeps a menu row, the kernel it names was deleted,
//! and nothing complains until GRUB is asked to boot it. This is how removing a
//! snapshot by hand goes wrong — the entry is regenerated from the *files*, so the
//! files have to go first, and a store can be left half-way through that.
//!
//! **A snapshot with no menu entry.** The opposite: the files are there and GRUB
//! cannot reach them.
//!
//! **`GRUB_DEFAULT` not set to `saved`.** `pin` writes `saved_entry`, which grub.cfg
//! ignores entirely when the default is a fixed number — so pinning silently does
//! nothing while reporting success. `next_entry` (the one-shot `boot`) is honoured
//! either way, because that branch is emitted independently. Worth saying out loud,
//! because the two verbs then appear to disagree with each other.
//!
//! **Orphaned `/boot` kernels.** A `vmlinuz-snap-*` with no snapshot behind it is
//! untracked, and `10_linux` turns every one of them into a *generated* menu entry
//! of its own — inferior, and invisible to this tool.
//!
//! **Unparseable manifests.** These degrade quietly: the store still lists the
//! snapshot, because every field falls back to a line scan, so the wrong answer is
//! only slightly wrong.

use std::path::Path;

use crate::store::{human_bytes, Paths, Snapshot, Store};
use crate::write;

/// How bad a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Nothing to do.
    Ok,
    /// Works, but something is not as it should be.
    Warn,
    /// Broken, or about to be.
    Fail,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Ok => "ok",
            Severity::Warn => "warn",
            Severity::Fail => "FAIL",
        }
    }
}

/// One line of the report.
#[derive(Debug, Clone)]
pub struct Finding {
    /// Short name of what was checked, e.g. `manifests`.
    pub key: String,
    pub severity: Severity,
    pub detail: String,
}

/// The whole preflight.
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub findings: Vec<Finding>,
}

impl Report {
    fn note(&mut self, key: &str, severity: Severity, detail: impl Into<String>) {
        self.findings.push(Finding {
            key: key.to_string(),
            severity,
            detail: detail.into(),
        });
    }

    /// The worst severity seen, which is what the exit gate keys off.
    pub fn worst(&self) -> Severity {
        self.findings
            .iter()
            .map(|f| f.severity)
            .max()
            .unwrap_or(Severity::Ok)
    }

    /// Findings a `--verify` run should refuse over.
    pub fn failures(&self) -> Vec<&Finding> {
        self.findings
            .iter()
            .filter(|f| f.severity == Severity::Fail)
            .collect()
    }

    /// The report as printed, one line per finding.
    pub fn lines(&self) -> Vec<String> {
        self.findings
            .iter()
            .map(|f| format!("  {:<12} {:<5} {}", f.key, f.severity.label(), f.detail))
            .collect()
    }
}

/// Read the store and report on it. Read-only; writes nothing, anywhere.
pub fn examine(paths: &Paths) -> Report {
    let mut report = Report::default();
    let store = Store::new(paths.clone());

    // --- the store itself -------------------------------------------------
    if !paths.root.is_dir() {
        report.note(
            "store",
            Severity::Fail,
            format!(
                "{} is not a directory — no store here",
                paths.root.display()
            ),
        );
        // Stop: every other line would be describing a store that is not there.
        return report;
    }

    let snapshots = match store.discover() {
        Ok(snapshots) => snapshots,
        Err(err) => {
            report.note("store", Severity::Fail, err.to_string());
            return report;
        }
    };
    let hidden = snapshots.iter().filter(|s| s.hidden).count();
    report.note(
        "store",
        Severity::Ok,
        format!(
            "{} — {} snapshot(s), {hidden} hidden",
            paths.root.display(),
            snapshots.len()
        ),
    );

    // --- manifests --------------------------------------------------------
    let unreadable: Vec<&str> = snapshots
        .iter()
        .filter(|s| s.manifest_error.is_some())
        .map(|s| s.id.as_str())
        .collect();
    if unreadable.is_empty() {
        report.note(
            "manifests",
            Severity::Ok,
            format!("{} parse cleanly", snapshots.len()),
        );
    } else {
        // A Fail, not a Warn: every field of such a snapshot comes from a line scan,
        // so the store reports it with slightly wrong values rather than not at all.
        report.note(
            "manifests",
            Severity::Fail,
            format!(
                "{} unparseable: {}",
                unreadable.len(),
                unreadable.join(", ")
            ),
        );
    }

    // --- menu entries -----------------------------------------------------
    let no_entry: Vec<&str> = snapshots
        .iter()
        .filter(|s| !s.hidden && !s.grub_entry)
        .map(|s| s.id.as_str())
        .collect();
    // Hidden snapshots are *supposed* to have no entry, which is why they are not in
    // the list above: reporting them would make a healthy store look broken.
    if no_entry.is_empty() {
        report.note("menu", Severity::Ok, "every visible snapshot has an entry");
    } else {
        report.note(
            "menu",
            Severity::Fail,
            format!(
                "{} visible snapshot(s) with no GRUB entry: {} — the files are there \
                 and GRUB cannot reach them",
                no_entry.len(),
                no_entry.join(", ")
            ),
        );
    }

    // --- boot artefacts ---------------------------------------------------
    let incomplete: Vec<String> = snapshots
        .iter()
        .filter(|s| !s.boot_artifacts_complete())
        .map(|s| {
            let mut missing = Vec::new();
            if !s.vmlinuz {
                missing.push("vmlinuz");
            }
            if !s.initramfs {
                missing.push("initramfs");
            }
            if !s.cmdline_file {
                missing.push("cmdline");
            }
            format!("{} (no {})", s.id, missing.join("/"))
        })
        .collect();
    if incomplete.is_empty() {
        report.note(
            "artefacts",
            Severity::Ok,
            format!("all {} have kernel + initramfs + cmdline", snapshots.len()),
        );
    } else {
        report.note(
            "artefacts",
            Severity::Fail,
            format!(
                "{} incomplete: {} — GRUB would offer a boot it cannot finish",
                incomplete.len(),
                incomplete.join(", ")
            ),
        );
    }

    // --- the failure that is invisible until boot -------------------------
    //
    // Every `/boot/vmlinuz-snap-*` becomes a menu row, whether or not anything in the
    // store knows about it. A store half-way through a manual delete looks fine until
    // GRUB is asked to load the kernel that is not there.
    let orphans = orphaned_boot_kernels(paths, &snapshots);
    if orphans.is_empty() {
        report.note("orphans", Severity::Ok, "none in /boot");
    } else {
        report.note(
            "orphans",
            Severity::Fail,
            format!(
                "{} kernel(s) in /boot with no snapshot: {} — 10_linux gives each its \
                 own menu entry, and a missing initramfs makes it a boot that cannot \
                 finish",
                orphans.len(),
                orphans.join(", ")
            ),
        );
    }

    // --- grubenv ----------------------------------------------------------
    report_grubenv(&mut report, paths, &snapshots);

    // --- the tools this verb needs ----------------------------------------
    let missing_tools: Vec<&str> = ["grub-mkconfig", "grub-editenv", "grub-reboot"]
        .into_iter()
        .filter(|tool| !tool_present(tool))
        .collect();
    if missing_tools.is_empty() {
        report.note(
            "tools",
            Severity::Ok,
            "grub-mkconfig, grub-editenv, grub-reboot",
        );
    } else {
        report.note(
            "tools",
            Severity::Fail,
            format!(
                "{} not on PATH: {}",
                missing_tools.len(),
                missing_tools.join(", ")
            ),
        );
    }

    // --- room to work -----------------------------------------------------
    match free_bytes(&paths.boot) {
        Some(bytes) => {
            // A snapshot costs ~75 MB (kernel + initramfs + module). Below a few
            // hundred MB the next `dev --register` fails part-way through writing
            // /boot, which is the worst place to run out.
            let severity = if bytes < 512 * 1024 * 1024 {
                Severity::Warn
            } else {
                Severity::Ok
            };
            report.note(
                "space",
                severity,
                format!("{} free on {}", human_bytes(bytes), paths.boot.display()),
            );
        }
        None => report.note(
            "space",
            Severity::Warn,
            format!("cannot read free space on {}", paths.boot.display()),
        ),
    }

    report
}

/// `/boot` kernels whose snapshot is not in the store.
///
/// Only the snapshot ids the store knows are considered; a file is orphaned when
/// nothing claims it.
fn orphaned_boot_kernels(paths: &Paths, snapshots: &[Snapshot]) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(&paths.boot) else {
        return Vec::new();
    };
    let known: Vec<&str> = snapshots.iter().map(|s| s.id.as_str()).collect();
    let mut orphans = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // `vmlinuz-snap-<id>`, and only that: a distro kernel is not ours to judge.
        let Some(id) = name
            .strip_prefix("vmlinuz-")
            .filter(|rest| rest.starts_with("snap-"))
        else {
            continue;
        };
        if !known.contains(&id) {
            orphans.push(id.to_string());
        }
    }
    orphans.sort();
    orphans.dedup();
    orphans
}

/// grubenv, and whether anything will honour what it is written into.
fn report_grubenv(report: &mut Report, paths: &Paths, snapshots: &[Snapshot]) {
    let saved = write::saved_entry();
    match &saved {
        Some(id) if id != "0" => {
            let known = snapshots.iter().any(|s| &s.id == id);
            if known {
                report.note("pinned", Severity::Ok, format!("saved_entry={id}"));
            } else {
                report.note(
                    "pinned",
                    Severity::Fail,
                    format!(
                        "saved_entry={id} names a snapshot that is not in the store — \
                         GRUB would fall back to entry 0 and boot another kernel"
                    ),
                );
            }
        }
        Some(other) => report.note(
            "pinned",
            Severity::Ok,
            format!("saved_entry={other} (a fixed menu index, not a snapshot)"),
        ),
        None => report.note(
            "pinned",
            Severity::Warn,
            "grubenv has no saved_entry (or grub-editenv could not be read)".to_string(),
        ),
    }

    // The subtlety worth a line of its own: `pin` and `boot` do not fail the same way.
    let defaults = std::fs::read_to_string(&paths.grub_defaults).unwrap_or_default();
    match grub_default(&defaults) {
        Some(value) if value == "saved" => report.note(
            "grub-default",
            Severity::Ok,
            "GRUB_DEFAULT=saved — both pin and one-shot boot are honoured",
        ),
        Some(value) => report.note(
            "grub-default",
            Severity::Warn,
            format!(
                "GRUB_DEFAULT={value}, not `saved` — grub.cfg ignores saved_entry, so \
                 `pin` does nothing here (silently). `boot` still works: next_entry is \
                 honoured either way."
            ),
        ),
        None => report.note(
            "grub-default",
            Severity::Warn,
            format!(
                "cannot read GRUB_DEFAULT from {}",
                paths.grub_defaults.display()
            ),
        ),
    }

    if let Some(next) = write::next_entry() {
        if !next.is_empty() {
            report.note(
                "one-shot",
                Severity::Ok,
                format!("next_entry={next} is armed for the next boot"),
            );
        }
    }
}

/// `GRUB_DEFAULT=` out of `/etc/default/grub`, with quotes stripped.
///
/// Pure so the two spellings the file is written in (`saved` and `'saved'`) are
/// tested rather than assumed: a quote-blind comparison would report a correctly
/// configured box as broken.
fn grub_default(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.trim().strip_prefix("GRUB_DEFAULT="))
        .map(|value| value.trim().trim_matches(['"', '\'']).to_string())
}

/// Whether a program is on PATH.
fn tool_present(tool: &str) -> bool {
    std::process::Command::new(tool)
        .arg("--version")
        .output()
        .is_ok()
}

/// Free bytes on the filesystem holding `path`.
fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    Some(stat.f_bavail as u64 * stat.f_frsize as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("snap-doctor-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("boot")).unwrap();
        std::fs::create_dir_all(dir.join("etc/grub.d")).unwrap();
        std::fs::create_dir_all(dir.join("etc/default")).unwrap();
        dir
    }

    /// A store with one snapshot, optionally missing named artefacts.
    ///
    /// `omit` takes `"vmlinuz"`, `"initramfs"` or `"cmdline"` and deletes that file
    /// while leaving the manifest claiming it — which is exactly the half-deleted
    /// state this verb exists to catch.
    fn store_with(tag: &str, id: &str, omit: &[&str]) -> PathBuf {
        let dir = fixture(tag);
        let snap = dir.join(id);
        std::fs::create_dir_all(&snap).unwrap();
        std::fs::write(
            snap.join("manifest.yaml"),
            format!(
                "snapshot:\n  id: \"{id}\"\n  timestamp: \"2026-10-09T00:00:00Z\"\n  \
                 description: \"{id}\"\n"
            ),
        )
        .unwrap();
        for (file, kind) in [
            (format!("vmlinuz-{id}"), "vmlinuz"),
            (format!("initramfs-{id}.img"), "initramfs"),
            (format!("{id}.cmdline"), "cmdline"),
        ] {
            if !omit.contains(&kind) {
                std::fs::write(dir.join("boot").join(file), b"x").unwrap();
            }
        }
        // The `# === <id>: … ===` marker is what `grub::parse_custom` keys the entry
        // off, so a fixture without it is not an entry at all — and the check under
        // test would then pass or fail for the wrong reason.
        std::fs::write(
            dir.join("etc/grub.d/40_custom"),
            format!(
                "#!/bin/sh\nexec tail -n +3 $0\n\
                 # === {id}: {id} ===\n\
                 menuentry 'x' --id '{id}' {{\n}}\n"
            ),
        )
        .unwrap();
        std::fs::write(dir.join("etc/default/grub"), "GRUB_DEFAULT=saved\n").unwrap();
        dir
    }

    fn key<'a>(report: &'a Report, key: &str) -> &'a Finding {
        report
            .findings
            .iter()
            .find(|f| f.key == key)
            .unwrap_or_else(|| panic!("no {key} finding in {:?}", report.lines()))
    }

    #[test]
    fn a_healthy_store_has_nothing_to_report() {
        let dir = store_with("healthy", "snap-aaaaaaaa", &[]);
        let report = examine(&Paths::under(&dir));

        assert_eq!(key(&report, "manifests").severity, Severity::Ok);
        assert_eq!(key(&report, "menu").severity, Severity::Ok);
        assert_eq!(key(&report, "artefacts").severity, Severity::Ok);
        assert_eq!(key(&report, "orphans").severity, Severity::Ok);
        assert!(key(&report, "store").detail.contains("1 snapshot(s)"));
        // The two lines that need the real machine (the grub tools, and grubenv, which
        // is read from the running system rather than the fixture) are deliberately
        // not asserted: a CI box without grub installed must not make this test lie.
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The failure this verb exists for: an entry GRUB would offer and could not
    /// finish. Deleting a snapshot's files without regenerating the menu is how a
    /// store ends up here.
    #[test]
    fn a_missing_initramfs_is_a_failure_and_is_named() {
        let dir = store_with("no-initramfs", "snap-aaaaaaaa", &["initramfs"]);
        let report = examine(&Paths::under(&dir));

        let artefacts = key(&report, "artefacts");
        assert_eq!(artefacts.severity, Severity::Fail);
        assert!(
            artefacts.detail.contains("initramfs"),
            "{}",
            artefacts.detail
        );
        assert!(
            artefacts.detail.contains("snap-aaaaaaaa"),
            "{}",
            artefacts.detail
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A kernel in /boot that no snapshot claims. `10_linux` makes a menu entry out of
    /// every one of them, so they are not harmless leftovers.
    #[test]
    fn a_boot_kernel_with_no_snapshot_is_reported() {
        let dir = store_with("orphan", "snap-aaaaaaaa", &[]);
        std::fs::write(dir.join("boot/vmlinuz-snap-deadbeef"), b"x").unwrap();

        let report = examine(&Paths::under(&dir));
        let orphans = key(&report, "orphans");
        assert_eq!(orphans.severity, Severity::Fail);
        assert!(
            orphans.detail.contains("snap-deadbeef"),
            "{}",
            orphans.detail
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The distro's own kernel is not ours to judge.
    #[test]
    fn distro_kernels_are_not_orphans() {
        let dir = store_with("distro", "snap-aaaaaaaa", &[]);
        std::fs::write(dir.join("boot/vmlinuz-linux-cachyos-bore"), b"x").unwrap();

        let report = examine(&Paths::under(&dir));
        assert_eq!(key(&report, "orphans").severity, Severity::Ok);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A malformed name is still an untracked kernel. `10_linux` sees the file and
    /// makes an entry for it whatever it is called, so the check must not skip it for
    /// failing to look like a snapshot id.
    #[test]
    fn a_malformed_name_is_still_reported() {
        let dir = store_with("malformed", "snap-aaaaaaaa", &[]);
        std::fs::write(dir.join("boot/vmlinuz-snap-"), b"x").unwrap();

        let report = examine(&Paths::under(&dir));
        assert_eq!(key(&report, "orphans").severity, Severity::Fail);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unparseable manifest is a failure, not a warning: the store still lists the
    /// snapshot, so every field of it comes from a line scan and is quietly wrong.
    #[test]
    fn an_unparseable_manifest_is_a_failure() {
        let dir = store_with("bad-manifest", "snap-aaaaaaaa", &[]);
        std::fs::write(
            dir.join("snap-aaaaaaaa/manifest.yaml"),
            "snapshot:\n   id: a\n  timestamp: b\n",
        )
        .unwrap();

        let report = examine(&Paths::under(&dir));
        let manifests = key(&report, "manifests");
        assert_eq!(manifests.severity, Severity::Fail);
        assert!(
            manifests.detail.contains("snap-aaaaaaaa"),
            "{}",
            manifests.detail
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Both spellings the file is written in, and the trap: comparing without
    /// stripping quotes reports a correctly configured box as broken.
    #[test]
    fn grub_default_is_read_through_quotes() {
        assert_eq!(
            grub_default("GRUB_DEFAULT=saved\n").as_deref(),
            Some("saved")
        );
        assert_eq!(
            grub_default("GRUB_DEFAULT='saved'\n").as_deref(),
            Some("saved")
        );
        assert_eq!(
            grub_default("GRUB_DEFAULT=\"saved\"\n").as_deref(),
            Some("saved")
        );
        assert_eq!(
            grub_default("GRUB_TIMEOUT=5\nGRUB_DEFAULT=0\n").as_deref(),
            Some("0")
        );
        // A comment, and a key that merely starts the same way, must not be taken for
        // it — `strip_prefix` on the trimmed line is what keeps them apart.
        assert_eq!(grub_default("# GRUB_DEFAULT=saved\n").as_deref(), None);
        assert_eq!(grub_default("GRUB_DEFAULT_DISABLED=x\n").as_deref(), None);
    }

    #[test]
    fn the_worst_severity_drives_the_gate() {
        let mut report = Report::default();
        assert_eq!(
            report.worst(),
            Severity::Ok,
            "an empty report is not a failure"
        );
        report.note("a", Severity::Ok, "fine");
        report.note("b", Severity::Warn, "eh");
        assert_eq!(report.worst(), Severity::Warn);
        assert!(
            report.failures().is_empty(),
            "a warning is not something to refuse over"
        );
        report.note("c", Severity::Fail, "broken");
        assert_eq!(report.worst(), Severity::Fail);
        assert_eq!(report.failures().len(), 1);
    }

    #[test]
    fn a_missing_store_is_reported_rather_than_panicking() {
        let dir = fixture("no-store");
        let report = examine(&Paths::under(dir.join("nowhere")));

        assert_eq!(key(&report, "store").severity, Severity::Fail);
        // ...and it stops there: every other line would be describing a store that is
        // not there.
        assert_eq!(report.findings.len(), 1, "{:?}", report.lines());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
