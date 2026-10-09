// SPDX-License-Identifier: GPL-2.0-only
//! Verify one snapshot's claims against what is actually on disk.
//!
//! `doctor` asks whether a *store* is coherent. This asks whether a *snapshot* is
//! what it says it is — a different question, because a manifest is a set of claims
//! written once and never re-checked. The module can be replaced, an entry can be
//! edited to point at another kernel, files can be moved out from under it.
//!
//! ## The five checks, and what each one is for
//!
//! **The manifest parses.** If it does not, every field below is coming from a line
//! scan, so this fails and the rest is not attempted rather than reported on bad
//! input.
//!
//! **The files it names exist** — read from the manifest's own `boot_files.*`, not
//! reconstructed from the id. A manifest naming one path while the entry boots
//! another is precisely the wrong-kernel boot this exists to catch.
//!
//! **The module is the one recorded.** `module.md5` against the stored module. This
//! is the field that says which code is really in here.
//!
//! **The GRUB entry boots *this* snapshot.** Its `linux` and `initrd` lines must
//! name this snapshot's own files. An entry pointing at another snapshot's kernel
//! produces a boot that succeeds and is not the one that was asked for — the
//! failure mode with no symptom.
//!
//! **The id is the content.** For a snapshot this tool registered, the id is
//! content-addressed over the module, the image and the kernel, so recomputing it
//! from the stored bytes must reproduce it exactly. Nothing else re-derives the id,
//! and the id is the only thing standing between "this is the module I built" and
//! "something else is in here now".

use std::path::{Path, PathBuf};

use crate::manifest;
use crate::report::{Report, Severity};
use crate::store::{Paths, Snapshot, Store};
use crate::{grub, id};

/// Verify one snapshot. Read-only.
pub fn one(paths: &Paths, snapshot: &Snapshot) -> Report {
    let mut report = Report::default();
    let sid = snapshot.id.as_str();

    // --- the manifest, which everything else is read through ---------------
    let Some(manifest) = &snapshot.manifest else {
        report.note(
            "manifest",
            Severity::Fail,
            "missing — only the artefacts are known, so nothing about it can be checked",
        );
        return report;
    };
    if let Some(err) = manifest.parse_error() {
        report.note("manifest", Severity::Fail, format!("does not parse: {err}"));
        // Every accessor below would fall back to a line scan. Reporting on those
        // would dress guesses up as verification.
        return report;
    }
    report.note("manifest", Severity::Ok, "parses");

    let fields = manifest.fields();
    let get = |key: &str| -> Option<String> {
        fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .filter(|v| !v.is_empty())
    };

    // --- the files the manifest names --------------------------------------
    let mut named = Vec::new();
    for key in [
        "boot_files.vmlinuz",
        "boot_files.initramfs",
        "boot_files.cmdline_file",
    ] {
        match get(key) {
            Some(path) if local(&paths.boot, Path::new(&path)).is_file() => named.push(path),
            Some(path) => {
                report.note(
                    "files",
                    Severity::Fail,
                    format!("{key} names {path}, which is not there"),
                );
            }
            None => report.note(
                "files",
                Severity::Fail,
                format!("{key} is not recorded, so the entry cannot be checked against it"),
            ),
        }
    }
    if named.len() == 3 {
        report.note("files", Severity::Ok, "all three it names are present");
    }

    // --- the module --------------------------------------------------------
    let module_dir = paths.root.join(sid).join("modules");
    let module_path = get("module.path").map(|p| local(&module_dir, Path::new(&p)));
    match (get("module.md5"), module_path.as_deref()) {
        (Some(recorded), Some(path)) if path.is_file() => {
            let actual = id::file_md5_hex(path);
            if actual.is_empty() {
                report.note(
                    "module",
                    Severity::Fail,
                    format!("{} cannot be read", path.display()),
                );
            } else if actual == recorded {
                report.note("module", Severity::Ok, format!("md5 {recorded}"));
            } else {
                report.note(
                    "module",
                    Severity::Fail,
                    format!(
                        "the stored module is not the one recorded: manifest says {recorded}, \
                         the file is {actual}"
                    ),
                );
            }
        }
        (Some(_), Some(path)) => report.note(
            "module",
            Severity::Fail,
            format!("{} is not there", path.display()),
        ),
        (None, _) => report.note(
            "module",
            Severity::Warn,
            "no md5 recorded, so the module cannot be identified",
        ),
        (_, None) => report.note(
            "module",
            Severity::Warn,
            "no module path recorded, so nothing points at the code that is in here",
        ),
    }

    // --- the menu entry ----------------------------------------------------
    report_entry(&mut report, paths, sid, snapshot.hidden, &get);

    // --- the id, re-derived ------------------------------------------------
    //
    // The strongest claim in the whole design, and the only check that can notice
    // the module being quietly swapped for another one: the id is a hash of the
    // module, the image and the kernel, so it can simply be recomputed.
    match get("snapshot.kind").as_deref() {
        Some(kind) if manifest::id_is_content_addressed(kind) => {
            report_identity(&mut report, paths, sid, &get, module_path.as_deref());
        }
        Some(other) => report.note(
            "identity",
            Severity::Warn,
            format!(
                "not checked: `{other}` ids come from a formula this tool does not \
                 compute, so recomputing one would only prove the formulas differ"
            ),
        ),
        None => report.note(
            "identity",
            Severity::Warn,
            "not checked: the manifest does not record which formula made this id, so \
             there is nothing to re-derive it from",
        ),
    }

    report
}

/// Where this snapshot's own copy of an artefact lives *in the store being read*.
///
/// A manifest records absolute paths, and on the host that wrote it those paths are
/// exactly where its store keeps those files. Read from anywhere else — a copy, a
/// fixture, an archive — the recorded paths still name the *original* host's files,
/// so taking them literally would verify a different store entirely: a snapshot whose
/// copied module had been corrupted would report clean against the module that is
/// still sitting on the machine it came from. Resolving against the root in use, and
/// falling back to the recorded path when there is nothing local, is what makes
/// `--root` mean what it says.
fn local(base: &Path, recorded: &Path) -> PathBuf {
    match recorded.file_name() {
        Some(name) if base.join(name).is_file() => base.join(name),
        _ => recorded.to_path_buf(),
    }
}

/// The last path component, so a GRUB path and a store path can be compared without
/// insisting they be spelled identically.
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The entry in `40_custom` must exist, carry this id, and boot this snapshot.
fn report_entry(
    report: &mut Report,
    paths: &Paths,
    sid: &str,
    hidden: bool,
    get: &impl Fn(&str) -> Option<String>,
) {
    let text = match std::fs::read_to_string(&paths.grub_custom) {
        Ok(text) => text,
        Err(_) => {
            // A warning rather than a failure: a store is still verifiable on a host
            // without GRUB installed, and calling that a broken snapshot would be
            // wrong.
            report.note(
                "entry",
                Severity::Warn,
                format!("cannot read {}", paths.grub_custom.display()),
            );
            return;
        }
    };
    let entries = grub::parse_custom(&text);
    let Some(entry) = entries.iter().find(|e| e.id == sid) else {
        if hidden {
            // Not a fault, and saying so would be the same false alarm `doctor`
            // already had to be cured of: a hidden snapshot is off the menu *by
            // design*, so GRUB is offering nothing and nothing is broken.
            report.note(
                "entry",
                Severity::Warn,
                "hidden, so it has no entry by design — nothing is offered; `unhide` \
                 would give it one"
                    .to_string(),
            );
        } else {
            report.note(
                "entry",
                Severity::Fail,
                "no entry in 40_custom — GRUB cannot reach this snapshot".to_string(),
            );
        }
        return;
    };

    // The `linux` and `initrd` lines, against what the manifest says they are. Compared
    // by file name rather than by full path: a GRUB entry names the file where GRUB
    // will read it, while the manifest records where the store keeps it, and the two
    // need not be spelled the same for the entry to boot the right kernel. That the
    // manifest's own path exists is the `files` check's job, not this one's.
    let want_kernel = get("boot_files.vmlinuz");
    let want_initramfs = get("boot_files.initramfs");
    let mut kernel: Option<String> = None;
    let mut initrds: Vec<String> = Vec::new();
    for line in &entry.body {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("linux ") {
            kernel = rest.split_whitespace().next().map(str::to_string);
        } else if let Some(rest) = line.strip_prefix("initrd ") {
            initrds = rest.split_whitespace().map(str::to_string).collect();
        }
    }

    let mut problems = Vec::new();
    match (&kernel, &want_kernel) {
        (Some(actual), Some(want)) if basename(actual) == basename(want) => {}
        (Some(actual), Some(want)) => {
            problems.push(format!(
                "the entry boots {actual}, the manifest names {want}"
            ));
        }
        (None, _) => problems.push("the entry has no `linux` line".to_string()),
        (_, None) => problems.push("the manifest names no kernel to compare against".to_string()),
    }
    match &want_initramfs {
        Some(want) => {
            if !initrds.iter().any(|p| basename(p) == basename(want)) {
                problems.push(format!("the entry's initrd does not include {want}"));
            }
        }
        None => problems.push("the manifest names no initramfs".to_string()),
    }

    if problems.is_empty() {
        report.note(
            "entry",
            Severity::Ok,
            "boots this snapshot's own kernel and image",
        );
    } else {
        report.note("entry", Severity::Fail, problems.join("; "));
    }
}

/// Re-derive the content-addressed id and compare it with the name on the directory.
fn report_identity(
    report: &mut Report,
    paths: &Paths,
    sid: &str,
    get: &impl Fn(&str) -> Option<String>,
    module_path: Option<&Path>,
) {
    let (Some(module), Some(initramfs), Some(vmlinuz)) = (
        module_path,
        get("boot_files.initramfs"),
        get("boot_files.vmlinuz"),
    ) else {
        report.note(
            "identity",
            Severity::Warn,
            "cannot re-derive the id: the manifest does not name all three components",
        );
        return;
    };

    let recomputed = id::register_id(
        module,
        &local(&paths.boot, Path::new(&initramfs)),
        &local(&paths.boot, Path::new(&vmlinuz)),
        None,
    );
    if recomputed == sid {
        report.note("identity", Severity::Ok, "the id is the content");
    } else {
        report.note(
            "identity",
            Severity::Fail,
            format!(
                "the content hashes to {recomputed}, but this snapshot is named {sid} — \
                 the bytes are not what the name claims"
            ),
        );
    }
}

/// Verify every snapshot in the store, worst first.
///
/// Hidden ones are included: being off the menu says nothing about whether the
/// files behind it are still there, and a snapshot someone put aside deliberately
/// is exactly the kind of thing whose rot goes unnoticed. What must not happen is
/// reporting its missing entry as a fault, so `one` is told it is hidden.
pub fn all(paths: &Paths) -> Vec<(String, Report)> {
    let store = Store::new(paths.clone());
    let Ok(snapshots) = store.discover() else {
        return Vec::new();
    };
    let mut out: Vec<(String, Report)> = snapshots
        .iter()
        .map(|s| (s.id.clone(), one(paths, s)))
        .collect();
    // Worst first, then by id: a reader meets the broken ones before the healthy
    // ones, and two runs that found the same things still print the same order.
    out.sort_by(|a, b| b.1.worst().cmp(&a.1.worst()).then_with(|| a.0.cmp(&b.0)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::{register, write};

    fn fixture(tag: &str) -> PathBuf {
        // The *root* is deliberately not named `snap-*`: the snapshots live inside
        // it, and `discover` looks for `snap-*` entries under the root it is given.
        // Named `snap-…`, a leftover fixture is listed as a snapshot by anyone who
        // points `--root` at `/tmp`.
        let dir = std::env::temp_dir().join(format!("snaptune-fx-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("boot")).unwrap();
        std::fs::create_dir_all(dir.join("etc/grub.d")).unwrap();
        dir
    }

    /// A snapshot laid out the way `dev --register` writes one, so the identity check
    /// has something true to confirm.
    fn good(tag: &str) -> (Paths, Snapshot) {
        use std::sync::atomic::{AtomicU8, Ordering};
        static N: AtomicU8 = AtomicU8::new(0);
        let id = format!(
            "snap-{:08x}",
            0xa5a5_0000u32 + N.fetch_add(1, Ordering::SeqCst) as u32
        );

        let dir = fixture(tag);
        // Directly under the store root: `discover` looks for `snap-*` entries
        // there, so a subdirectory would be invisible to it.
        let snap_dir = dir.join(&id);
        std::fs::create_dir_all(snap_dir.join("modules")).unwrap();
        let boot = dir.join("boot");

        // Real bytes, because the identity check hashes them.
        std::fs::write(snap_dir.join("modules/module.ko.zst"), b"MODULE").unwrap();
        std::fs::write(boot.join(format!("vmlinuz-{id}")), b"KERNEL").unwrap();
        std::fs::write(boot.join(format!("initramfs-{id}.img")), b"IMAGE").unwrap();
        std::fs::write(boot.join(format!("{id}.cmdline")), b"quiet").unwrap();

        let module = snap_dir.join("modules/module.ko.zst");
        let md5 = id::file_md5_hex(&module);
        let expected = id::register_id(
            &module,
            &boot.join(format!("initramfs-{id}.img")),
            &boot.join(format!("vmlinuz-{id}")),
            None,
        );

        // The layout keys on the *directory* name, so register under the real id.
        let real_dir = dir.join(&expected);
        std::fs::rename(&snap_dir, &real_dir).unwrap();
        // The boot artefacts are named after the id, and the id is a hash of their
        // contents — so their real names only become knowable once it is computed.
        for (from, to) in [
            (format!("vmlinuz-{id}"), format!("vmlinuz-{expected}")),
            (
                format!("initramfs-{id}.img"),
                format!("initramfs-{expected}.img"),
            ),
            (format!("{id}.cmdline"), format!("{expected}.cmdline")),
        ] {
            std::fs::rename(boot.join(from), boot.join(to)).unwrap();
        }

        let module = real_dir.join("modules/module.ko.zst");

        std::fs::write(
            real_dir.join("manifest.yaml"),
            format!(
                "snapshot:\n  id: \"{expected}\"\n  timestamp: \"2026-10-09T00:00:00Z\"\n  \
                 description: \"verify fixture\"\n  kind: \"{}\"\n\
                 boot_files:\n  vmlinuz: \"{}\"\n  initramfs: \"{}\"\n  cmdline_file: \"{}\"\n\
                 module:\n  path: {}\n  md5: \"{md5}\"\n",
                manifest::KIND_DEV,
                boot.join(format!("vmlinuz-{expected}")).display(),
                boot.join(format!("initramfs-{expected}.img")).display(),
                boot.join(format!("{expected}.cmdline")).display(),
                module.display(),
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join("etc/grub.d/40_custom"),
            format!(
                "#!/bin/sh\nexec tail -n +3 $0\n\
                 # === {expected}: verify fixture ===\n\
                 menuentry '[{expected}] verify fixture' --id '{expected}' {{\n\
                 \x20   linux /boot/vmlinuz-{expected} BOOT_IMAGE=/boot/vmlinuz-{expected} root=UUID=x rw\n\
                 \x20   initrd /boot/acpi_override.cpio /boot/initramfs-{expected}.img\n\
                 }}\n"
            ),
        )
        .unwrap();

        let paths = Paths::under(&dir);
        let store = Store::new(paths.clone());
        let snapshot = store
            .discover()
            .unwrap()
            .into_iter()
            .next()
            .expect("the fixture snapshot");
        (paths, snapshot)
    }

    /// Copy a fixture tree, so a test can read the same snapshot from a second root —
    /// which is what `--root` does when it is pointed at a copy, a fixture or an
    /// archive rather than the store the manifest was written against.
    fn copy_tree(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let dest = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_tree(&entry.path(), &dest);
            } else {
                std::fs::copy(entry.path(), dest).unwrap();
            }
        }
    }

    fn key<'a>(report: &'a Report, key: &str) -> &'a crate::report::Finding {
        report
            .findings
            .iter()
            .find(|f| f.key == key)
            .unwrap_or_else(|| panic!("no {key} finding in {:?}", report.lines()))
    }

    #[test]
    fn a_well_formed_snapshot_verifies() {
        let (paths, snapshot) = good("clean");
        let report = one(&paths, &snapshot);
        for check in ["manifest", "files", "module", "entry", "identity"] {
            assert_eq!(
                key(&report, check).severity,
                Severity::Ok,
                "{check}: {:?}",
                key(&report, check)
            );
        }
        assert_eq!(report.worst(), Severity::Ok, "{:?}", report.lines());
    }

    /// The check nothing else makes: the id is a hash of the contents, so it can be
    /// recomputed. Swapping the module is otherwise undetectable — the md5 field would
    /// still match the manifest if someone edited both, but the id would not.
    #[test]
    fn a_swapped_module_is_caught_by_the_id_not_just_the_digest() {
        let (paths, snapshot) = good("swapped");
        let module = paths.root.join(&snapshot.id).join("modules/module.ko.zst");
        std::fs::write(&module, b"DIFFERENT MODULE").unwrap();

        let report = one(&paths, &snapshot);
        // Both fire, and for different reasons — the digest says the module changed,
        // the identity says the name no longer describes the contents.
        assert_eq!(key(&report, "module").severity, Severity::Fail);
        assert_eq!(key(&report, "identity").severity, Severity::Fail);
        assert!(
            key(&report, "identity")
                .detail
                .contains("not what the name claims"),
            "{}",
            key(&report, "identity").detail
        );
    }

    /// The silent failure: an entry that boots *a* kernel successfully, just not this
    /// snapshot's. Nothing about that is visible at boot.
    #[test]
    fn an_entry_pointing_at_another_kernel_is_caught() {
        let (paths, snapshot) = good("wrong-entry");
        std::fs::write(
            paths.grub_custom.clone(),
            format!(
                "#!/bin/sh\nexec tail -n +3 $0\n\
                 # === {}: verify fixture ===\n\
                 menuentry 'x' --id '{}' {{\n\
                 \x20   linux /boot/vmlinuz-linux-cachyos-bore root=UUID=x rw\n\
                 \x20   initrd /boot/initramfs-linux-cachyos-bore.img\n\
                 }}\n",
                snapshot.id, snapshot.id
            ),
        )
        .unwrap();

        let report = one(&paths, &snapshot);
        let entry = key(&report, "entry");
        assert_eq!(entry.severity, Severity::Fail);
        assert!(
            entry
                .detail
                .contains("boots /boot/vmlinuz-linux-cachyos-bore"),
            "{entry:?}"
        );
    }

    #[test]
    fn a_missing_file_the_manifest_names_is_a_failure() {
        let (paths, snapshot) = good("missing-file");
        std::fs::remove_file(paths.boot.join(format!("initramfs-{}.img", snapshot.id))).unwrap();

        let report = one(&paths, &snapshot);
        assert_eq!(key(&report, "files").severity, Severity::Fail);
        assert!(
            key(&report, "files").detail.contains("not there"),
            "{:?}",
            key(&report, "files")
        );
    }

    /// An unparseable manifest stops the verification rather than dressing guesses up
    /// as findings: every accessor below it would be reading a line scan.
    #[test]
    fn an_unparseable_manifest_stops_the_rest() {
        let (paths, snapshot) = good("bad-manifest");
        std::fs::write(
            paths.root.join(&snapshot.id).join("manifest.yaml"),
            "snapshot:\n   id: a\n  timestamp: b\n",
        )
        .unwrap();

        let store = Store::new(paths.clone());
        let snapshot = store.discover().unwrap().into_iter().next().unwrap();
        let report = one(&paths, &snapshot);

        assert_eq!(key(&report, "manifest").severity, Severity::Fail);
        assert_eq!(report.findings.len(), 1, "{:?}", report.lines());
    }

    /// A snapshot from another writer has an id this tool cannot recompute, and must
    /// say so rather than failing a check it was never able to make.
    #[test]
    fn a_foreign_id_is_not_treated_as_a_mismatch() {
        let (paths, snapshot) = good("foreign");
        let manifest = paths.root.join(&snapshot.id).join("manifest.yaml");
        let text = std::fs::read_to_string(&manifest).unwrap().replace(
            &format!("kind: \"{}\"", manifest::KIND_DEV),
            "kind: \"created-from-running\"",
        );
        std::fs::write(&manifest, text).unwrap();

        let store = Store::new(paths.clone());
        let snapshot = store.discover().unwrap().into_iter().next().unwrap();
        let report = one(&paths, &snapshot);

        let identity = key(&report, "identity");
        // A warning, not an `ok`: saying the id is not recomputable and calling that a
        // pass would read as "this was verified", which is the one thing it was not.
        assert_eq!(identity.severity, Severity::Warn);
        assert!(identity.detail.contains("not checked"), "{identity:?}");
        // And it must not be mistaken for a fault: the snapshot may be perfectly good.
        assert!(report.failures().is_empty(), "{:?}", report.lines());
    }

    /// A manifest names the store it was written against. Read from *another* root,
    /// the same names still point at the original host's files — so a copied snapshot
    /// whose module had been corrupted would verify clean against the module still
    /// sitting on the machine it came from. The copied store's own files are what
    /// "this snapshot" means, and this is the test that says so.
    #[test]
    fn a_second_root_is_checked_against_its_own_files() {
        let (paths, snapshot) = good("elsewhere");
        let mirror = paths.root.join("../snaptune-fx-mirror");
        let _ = std::fs::remove_dir_all(&mirror);
        copy_tree(&paths.root, &mirror);

        // Corrupt the copy's module and nothing else. The original is untouched, and
        // its digest still matches the manifest.
        let copied_module = mirror.join(&snapshot.id).join("modules/module.ko.zst");
        std::fs::write(&copied_module, b"MODULE-SWAPPED").unwrap();

        let elsewhere = Paths::under(&mirror);
        let store = Store::new(elsewhere.clone());
        let snapshot = store
            .discover()
            .unwrap()
            .into_iter()
            .next()
            .expect("the copied snapshot");
        let report = one(&elsewhere, &snapshot);

        assert_eq!(
            key(&report, "module").severity,
            Severity::Fail,
            "the copy's module was replaced but the check passed: {:?}",
            report.lines()
        );
        // And the identity check sees it too, because the module is an id input.
        assert_eq!(key(&report, "identity").severity, Severity::Fail);

        let _ = std::fs::remove_dir_all(&mirror);
    }

    /// The two halves of the loop, run against each other.
    ///
    /// Fixtures built by hand can only show that each side is self-consistent: a
    /// manifest written the way I *believe* `register` writes one proves nothing
    /// about how it really does, and the id is a hash of three files whose argument
    /// order is not visible in any of them. So this runs the production writer and
    /// the production verifier over the same bytes — a disagreement about the
    /// formula, the order, or which file a path resolves to fails here rather than
    /// on a blade.
    #[test]
    fn a_snapshot_registered_for_real_verifies_clean() {
        let dir = fixture("roundtrip");
        let work = dir.join("work");
        // `plan` writes `40_custom` with `fs::write`, which will not create parents.
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(dir.join("etc/grub.d")).unwrap();
        std::fs::create_dir_all(dir.join("boot")).unwrap();

        let module = work.join("module.ko.zst");
        let initramfs = work.join("dev.img");
        let vmlinuz_source = work.join("vmlinuz-linux");
        std::fs::write(&module, b"compressed module bytes").unwrap();
        std::fs::write(&initramfs, b"derived initramfs").unwrap();
        std::fs::write(&vmlinuz_source, b"kernel").unwrap();

        let paths = Paths::under(&dir);
        // The same call `register_dev` makes, with the same arguments in the same
        // order. That agreement is half of what this test is for.
        let id = id::register_id(&module, &initramfs, &vmlinuz_source, None);

        let req = register::Request {
            id: id.clone(),
            description: "roundtrip".to_string(),
            status: "dev".to_string(),
            kind: manifest::KIND_DEV.to_string(),
            module: module.clone(),
            initramfs: initramfs.clone(),
            vmlinuz_source: vmlinuz_source.clone(),
            kernel_version: "7.2.9-1-cachyos-bore".to_string(),
            cmdline: format!("BOOT_IMAGE=/boot/vmlinuz-{id} root=UUID=x rw"),
            root_uuid: "757f018a-71b0-44fc-aac8-c5b2ed165923".to_string(),
            parent: Some("linux-cachyos-bore".to_string()),
            tree: None,
            rebuilt_modules: vec!["drivers/gpu/drm/amd/amdgpu".to_string()],
            changed_files: None,
            module_md5: id::file_md5_hex(&module),
            parent_module_md5: None,
            module_size: 22,
            module_srcversion: Some("11F98DC7EE7590D51AA9C9C".to_string()),
            module_vermagic: Some("7.2.9-1-cachyos-bore SMP preempt mod_unload".to_string()),
            store_root: paths.root.clone(),
            boot: paths.boot.clone(),
            acpi_override: false,
        };

        let plan = register::plan(&paths, &req, "2026-10-09T00:00:00Z", "").unwrap();
        // Every file action happens for real; only the commands are recorded, so
        // nothing here spawns `grub-mkconfig`.
        write::apply_with(&plan, false, &write::Recording::default()).unwrap();

        let store = Store::new(paths.clone());
        let snapshot = store
            .discover()
            .unwrap()
            .into_iter()
            .find(|s| s.id == id)
            .unwrap_or_else(|| panic!("{id} was registered but is not in the store"));

        let report = one(&paths, &snapshot);
        assert_eq!(
            report.worst(),
            Severity::Ok,
            "a snapshot this tool registered does not verify clean: {:?}",
            report.lines()
        );
        // Named explicitly, because a `warn` here would mean the strongest check had
        // quietly stopped running on the snapshots it exists for.
        assert_eq!(key(&report, "identity").severity, Severity::Ok);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A hidden snapshot has no menu entry on purpose. Reporting that as a fault
    /// would be the same false alarm `doctor` had: it is the *deliberate* absence,
    /// and the files behind it are what is worth checking.
    #[test]
    fn a_hidden_snapshot_without_an_entry_warns_rather_than_fails() {
        let (paths, snapshot) = good("hidden");
        let dir = paths.root.join(&snapshot.id);
        std::fs::rename(dir.join("manifest.yaml"), dir.join("manifest.yaml.hidden")).unwrap();
        // Hiding takes the entry off the menu, which is the whole point of it.
        std::fs::write(&paths.grub_custom, "# nothing here\n").unwrap();

        let store = Store::new(paths.clone());
        let snapshot = store
            .discover()
            .unwrap()
            .into_iter()
            .find(|s| s.id == snapshot.id)
            .expect("the hidden snapshot");
        assert!(snapshot.hidden, "the fixture should read as hidden");

        let report = one(&paths, &snapshot);
        let entry = key(&report, "entry");
        assert_eq!(entry.severity, Severity::Warn, "{:?}", report.lines());
        assert!(entry.detail.contains("hidden"), "{entry:?}");
        assert!(report.failures().is_empty(), "{:?}", report.lines());
    }
}
