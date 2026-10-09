// SPDX-License-Identifier: GPL-2.0-only
//! Registering a dev image as a snapshot.
//!
//! ## What this is for
//!
//! [`crate::dev`] builds a module and derives an initramfs from a parent image.
//! That produces *files*. A snapshot is the thing the rest of the system
//! understands: an id, a manifest, boot artefacts under `/boot`, and a GRUB entry.
//! This is the step between the two, and it is what makes a dev iteration show up
//! in the SNAP tab and become bootable through the normal flow.
//!
//! ## Derived, not generated
//!
//! Ordinarily a snapshot's initramfs is produced by `mkinitcpio`, which reads
//! modules out of `/usr/lib/modules/<kver>/` — so the module under test has to be
//! installed over the live one first. Here the image is already built, by
//! replacing a single member of the parent's: the live module tree is never
//! written, so there is no window to lose the fallback in and nothing to rescue.
//!
//! ## The id is the proof
//!
//! Ids are content-addressed over the module, the initramfs and the kernel, using
//! the same formula the shell tool's `register` uses ([`crate::id::register_id`]).
//! That is not bookkeeping: it means the id *is* the statement that this exact
//! module is in this exact image, so an id that already exists describes
//! byte-identical content and registering it again is refused rather than
//! overwriting.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::grub;
use crate::store::Paths;
use crate::write::{Action, Plan};

/// Everything registration needs to know.
///
/// A struct rather than eight positional arguments: most of the fields are paths,
/// and swapping two of them would produce a snapshot that boots the wrong kernel
/// while looking entirely successful.
#[derive(Debug, Clone)]
pub struct Request {
    pub id: String,
    pub description: String,
    /// The built, compressed module on disk.
    pub module: PathBuf,
    /// The derived initramfs on disk.
    pub initramfs: PathBuf,
    /// The kernel image to pair with that initramfs.
    pub vmlinuz: PathBuf,
    /// Kernel release the module was built for.
    pub kernel_version: String,
    /// The kernel command line, with `BOOT_IMAGE=` already pointing at the
    /// snapshot's own kernel.
    pub cmdline: String,
    /// The root filesystem's UUID, which the menu entry's `search` line needs.
    /// See [`root_uuid`].
    pub root_uuid: String,
    /// The snapshot or image this was derived from.
    pub parent: Option<String>,
    /// The source tree it was built from, for provenance.
    pub tree: Option<PathBuf>,
    /// Whether `/boot/acpi_override.cpio` exists and must be chained into the entry.
    pub acpi_override: bool,
}

/// The manifest for a dev snapshot.
///
/// Pure, so the provenance it records can be asserted rather than eyeballed. Kept
/// deliberately small: this records what was *done*, and the honest answer to
/// "which patches are live in it" is that nobody knows until it boots, so no
/// patch block is invented here.
pub fn manifest_text(req: &Request, timestamp: &str) -> String {
    let mut out = String::new();
    out.push_str("snapshot:\n");
    out.push_str(&format!("  id: \"{}\"\n", req.id));
    out.push_str(&format!("  timestamp: \"{timestamp}\"\n"));
    out.push_str(&format!("  description: \"{}\"\n", req.description));
    out.push_str("  status: \"dev\"\n");
    out.push_str("  kind: \"dev-build\"\n");
    if let Some(parent) = &req.parent {
        out.push_str(&format!("  parent: \"{parent}\"\n"));
    }
    out.push_str("\nkernel:\n");
    out.push_str(&format!("  version: \"{}\"\n", req.kernel_version));
    out.push_str(&format!(
        "  vmlinuz_snapshot: \"{}\"\n",
        req.vmlinuz.display()
    ));
    out.push_str("\nsource_tree:\n");
    match &req.tree {
        Some(tree) => {
            out.push_str(&format!("  path: \"{}\"\n", tree.display()));
            // Not `built-from-source`: that name means the full kernel build path
            // ran. This is the module-only dev path, and saying otherwise would
            // make `verify` draw the wrong conclusion about the tree.
            out.push_str("  mode: \"dev-module-build\"\n");
        }
        None => {
            out.push_str("  path: \"\"\n");
            out.push_str("  mode: \"running-module\"\n");
        }
    }
    // The entry builder reads `grub.root_uuid` to write the `search` line, and
    // refuses rather than guessing when it is absent — so it is written here.
    out.push_str("\ngrub:\n");
    out.push_str(&format!("  root_uuid: \"{}\"\n", req.root_uuid));
    out.push_str(&format!(
        "  entry_title: \"[{}] {}\"\n",
        req.id, req.description
    ));
    out.push_str("\nbuild:\n");
    out.push_str("  initramfs: \"derived from parent, one member replaced\"\n");
    out.push_str("  patches: \"unknown until this snapshot boots\"\n");
    out
}

/// The root filesystem's UUID, which the entry's `search --fs-uuid` line needs.
///
/// `findmnt -n -o UUID /`, matching the shell tool, falling back to `/etc/fstab`.
///
/// **Refuses rather than defaulting.** The shell tool substitutes the literal
/// string `unknown` when it cannot tell, which produces a menu entry that looks
/// fine and cannot boot. Failing here is louder and much cheaper to find.
pub fn root_uuid() -> Result<String> {
    if let Ok(out) = std::process::Command::new("findmnt")
        .args(["-n", "-o", "UUID", "/"])
        .output()
    {
        if out.status.success() {
            let uuid = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !uuid.is_empty() {
                return Ok(uuid);
            }
        }
    }
    if let Ok(fstab) = std::fs::read_to_string("/etc/fstab") {
        for line in fstab.lines() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() >= 2 && fields[1] == "/" {
                if let Some(uuid) = fields[0].strip_prefix("UUID=") {
                    return Ok(uuid.to_string());
                }
            }
        }
    }
    bail!(
        "cannot determine the root filesystem UUID (tried findmnt and /etc/fstab).\n\
         The menu entry needs it for its `search` line, and an entry without a \
         working one cannot boot."
    )
}

/// The kernel image that pairs with an initramfs, by the convention these blades
/// use in both directions:
///
/// ```text
///   initramfs-snap-11e61755.img       <->  vmlinuz-snap-11e61755
///   initramfs-linux-cachyos-bore.img  <->  vmlinuz-linux-cachyos-bore
/// ```
///
/// Pure, because deriving a kernel path by hand is how you end up booting a
/// snapshot with a kernel that does not match its modules.
pub fn vmlinuz_for(initramfs: &Path) -> Result<PathBuf> {
    let name = initramfs
        .file_name()
        .and_then(|n| n.to_str())
        .context("the initramfs path has no file name")?;
    let stem = name
        .strip_prefix("initramfs-")
        .with_context(|| format!("{name:?} does not follow the initramfs-<name>.img convention"))?;
    let stem = stem
        .strip_suffix(".img")
        .with_context(|| format!("{name:?} does not follow the initramfs-<name>.img convention"))?;
    if stem.is_empty() {
        bail!("{name:?} names no kernel");
    }
    Ok(initramfs.with_file_name(format!("vmlinuz-{stem}")))
}

/// Work out how to register `req`, without doing any of it.
///
/// `existing_custom` is the current contents of `/etc/grub.d/40_custom`, passed in
/// rather than read here so this stays pure and so the caller can show it in a
/// dry run. It must be read from the live file: the new entry is appended to it,
/// and writing only the new entry would delete every other snapshot's menu entry.
pub fn plan(paths: &Paths, req: &Request, timestamp: &str, existing_custom: &str) -> Result<Plan> {
    // Refuse before anything is written.
    if paths.root.join(&req.id).exists() {
        bail!(
            "{} is already registered.\n\
             Ids are content-addressed over the module, the image and the kernel, so an id \
             that already exists describes byte-identical content — there is nothing to add. \
             Change the source and build again for a new id.",
            req.id
        );
    }
    for (what, path) in [
        ("module", &req.module),
        ("initramfs", &req.initramfs),
        ("kernel", &req.vmlinuz),
    ] {
        if !path.is_file() {
            bail!("the {what} to register does not exist: {}", path.display());
        }
    }

    let snap_dir = paths.root.join(&req.id);
    let module_name = req
        .module
        .file_name()
        .context("the module path has no file name")?;
    let vmlinuz = paths.boot.join(format!("vmlinuz-{}", req.id));
    let initramfs = paths.boot.join(format!("initramfs-{}.img", req.id));
    let cmdline = paths.boot.join(format!("{}.cmdline", req.id));

    let mut actions = vec![
        Action::Mkdir {
            path: snap_dir.join("modules"),
            what: "the snapshot directory".to_string(),
        },
        Action::Write {
            path: snap_dir.join("manifest.yaml"),
            contents: manifest_text(req, timestamp),
            what: "the manifest".to_string(),
        },
        Action::Copy {
            from: req.module.clone(),
            to: snap_dir.join("modules").join(module_name),
            what: "the built module".to_string(),
        },
        // The kernel is copied rather than referenced, so the snapshot keeps
        // booting even if the parent's is replaced.
        Action::Copy {
            from: req.vmlinuz.clone(),
            to: vmlinuz,
            what: "the kernel".to_string(),
        },
        Action::Copy {
            from: req.initramfs.clone(),
            to: initramfs,
            what: "the derived initramfs".to_string(),
        },
        Action::Write {
            path: cmdline,
            contents: format!("{}\n", req.cmdline),
            what: "the kernel command line".to_string(),
        },
    ];

    // The menu entry comes from the manifest we just wrote, through the same
    // builder the other verbs use, so a dev snapshot's entry cannot drift from a
    // hand-made one.
    let manifest = crate::manifest::Manifest::parse(&manifest_text(req, timestamp))?;
    let entry = grub::entry_from_manifest(
        &manifest,
        &req.id,
        req.acpi_override
            .then(|| paths.boot.join("acpi_override.cpio")),
    )?;
    // Appended, never prepended: the entry order in the file is the boot menu
    // order, and silently reordering someone's menu is not this command's job.
    actions.push(Action::Write {
        path: paths.grub_custom.clone(),
        contents: append_entry(existing_custom, &entry),
        what: format!("append its menu entry to {}", paths.grub_custom.display()),
    });
    actions.push(Action::Run {
        argv: vec![
            "grub-mkconfig".to_string(),
            "-o".to_string(),
            paths.grub_cfg.display().to_string(),
        ],
        what: "regenerate grub.cfg".to_string(),
    });

    Ok(Plan {
        summary: format!("register {} as a bootable snapshot", req.id),
        actions,
    })
}

/// Put the new entry at the end of an existing `40_custom`.
///
/// Split out and pure so the "append, do not replace" rule is testable: the file
/// holds every other snapshot's entry, and writing only the new one would delete
/// them all.
pub fn append_entry(existing: &str, entry: &str) -> String {
    let mut out = existing.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(entry);
    out
}

/// The current time as the shell tool writes it: `date -u +%Y-%m-%dT%H:%M:%SZ`.
///
/// Shelled out rather than computed here because there is no date dependency in
/// this crate, and the store's timestamps are compared as strings — matching the
/// tool that wrote every existing manifest matters more than avoiding one
/// `date(1)` call.
pub fn now_timestamp() -> Result<String> {
    let out = std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .context("running date(1) to timestamp the manifest")?;
    if !out.status.success() {
        bail!("date(1) failed while timestamping the manifest");
    }
    let stamp = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if stamp.is_empty() {
        bail!("date(1) produced an empty timestamp");
    }
    Ok(stamp)
}

/// Read the live kernel command line, with `BOOT_IMAGE=` pointed at `vmlinuz`.
pub fn cmdline_for(vmlinuz: &Path) -> Result<String> {
    let live = std::fs::read_to_string("/proc/cmdline")
        .context("reading /proc/cmdline to derive the snapshot's command line")?;
    Ok(rewrite_boot_image(live.trim(), vmlinuz))
}

/// Replace the `BOOT_IMAGE=` token, keeping every other parameter exactly.
///
/// Pure: a snapshot that silently dropped a kernel parameter would boot with a
/// different configuration from the one it was derived from, and the difference
/// would be invisible until something did not work.
pub fn rewrite_boot_image(cmdline: &str, vmlinuz: &Path) -> String {
    let mut parts: Vec<String> = cmdline
        .split_whitespace()
        .filter(|p| !p.starts_with("BOOT_IMAGE="))
        .map(str::to_string)
        .collect();
    parts.insert(0, format!("BOOT_IMAGE={}", vmlinuz.display()));
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both naming directions, because deriving a kernel path by hand is how you
    /// boot a snapshot with a kernel that does not match its modules.
    #[test]
    fn the_kernel_is_derived_from_the_initramfs_name() {
        assert_eq!(
            vmlinuz_for(Path::new("/boot/initramfs-snap-0848359a.img")).unwrap(),
            PathBuf::from("/boot/vmlinuz-snap-0848359a")
        );
        // The distro image, which is what a dev snapshot on a stock boot derives
        // from.
        assert_eq!(
            vmlinuz_for(Path::new("/boot/initramfs-linux-cachyos-bore.img")).unwrap(),
            PathBuf::from("/boot/vmlinuz-linux-cachyos-bore")
        );
        // Anything else is refused rather than guessed at.
        assert!(vmlinuz_for(Path::new("/boot/vmlinuz-linux")).is_err());
        assert!(vmlinuz_for(Path::new("/boot/initramfs-.img")).is_err());
    }

    /// The one that would be destructive: `40_custom` holds every other
    /// snapshot's entry, so a registration that wrote only its own would delete
    /// every other way to boot.
    #[test]
    fn the_menu_entry_is_appended_and_nothing_is_lost() {
        let existing = "menuentry 'snap-aaaaaaaa' {\n}\n";
        let new = "menuentry 'snap-bbbbbbbb' {\n}\n";
        let out = append_entry(existing, new);

        assert!(
            out.contains("snap-aaaaaaaa"),
            "the old entry must survive: {out}"
        );
        assert!(
            out.contains("snap-bbbbbbbb"),
            "and the new one must be there: {out}"
        );
        assert!(
            out.find("snap-aaaaaaaa") < out.find("snap-bbbbbbbb"),
            "appended means after, not before: {out}"
        );
    }

    #[test]
    fn appending_to_an_empty_file_still_works() {
        assert_eq!(
            append_entry("", "menuentry 'x' {\n}\n"),
            "menuentry 'x' {\n}\n"
        );
    }

    /// A file with no trailing newline must not have its last line welded to the
    /// new entry.
    #[test]
    fn a_missing_trailing_newline_does_not_corrupt_the_previous_entry() {
        let out = append_entry("menuentry 'old' {\n}", "menuentry 'new' {\n}\n");
        assert!(out.contains("}\nmenuentry 'new'"), "{out}");
    }

    /// Every other kernel parameter has to survive: a snapshot that silently
    /// dropped one would boot with a different configuration from the image it
    /// was derived from, and nobody would notice until something did not work.
    #[test]
    fn rewriting_the_boot_image_keeps_every_other_parameter() {
        let live = "BOOT_IMAGE=/boot/vmlinuz-linux-cachyos-bore root=UUID=abc rw \
                    amdgpu.bc250_sdma_fw=navi12 mitigations=off";
        let out = rewrite_boot_image(live, Path::new("/boot/vmlinuz-snap-deadbeef"));

        assert!(
            out.starts_with("BOOT_IMAGE=/boot/vmlinuz-snap-deadbeef"),
            "{out}"
        );
        assert!(
            !out.contains("vmlinuz-linux-cachyos-bore"),
            "the old image must be gone: {out}"
        );
        for keep in [
            "root=UUID=abc",
            "rw",
            "amdgpu.bc250_sdma_fw=navi12",
            "mitigations=off",
        ] {
            assert!(out.contains(keep), "{keep} must survive: {out}");
        }
    }

    #[test]
    fn rewriting_is_idempotent() {
        let live = "BOOT_IMAGE=/boot/vmlinuz-old root=UUID=abc rw";
        let once = rewrite_boot_image(live, Path::new("/boot/vmlinuz-new"));
        let twice = rewrite_boot_image(&once, Path::new("/boot/vmlinuz-new"));
        assert_eq!(once, twice, "a second rewrite must not duplicate anything");
    }

    fn request() -> Request {
        Request {
            id: "snap-3f9a1c2b".to_string(),
            description: "printf marker".to_string(),
            module: PathBuf::from("/work/module.ko.zst"),
            initramfs: PathBuf::from("/work/dev.img"),
            vmlinuz: PathBuf::from("/boot/vmlinuz-linux-cachyos-bore"),
            kernel_version: "7.2.9-1-cachyos-bore".to_string(),
            cmdline: "BOOT_IMAGE=/boot/vmlinuz-snap-3f9a1c2b root=UUID=abc rw".to_string(),
            root_uuid: "757f018a-71b0-44fc-aac8-c5b2ed165923".to_string(),
            parent: Some("linux-cachyos-bore".to_string()),
            tree: Some(PathBuf::from("/home/nightcarnage/tree")),
            acpi_override: false,
        }
    }

    /// The manifest has to carry the root UUID, or the entry builder refuses —
    /// correctly, since an entry without a working `search` line cannot boot.
    #[test]
    fn the_manifest_carries_what_the_entry_needs() {
        let text = manifest_text(&request(), "2026-10-09T00:00:00Z");
        assert!(text.contains("root_uuid: \"757f018a"), "{text}");
        assert!(
            text.contains("entry_title: \"[snap-3f9a1c2b] printf marker\""),
            "{text}"
        );
        assert!(text.contains("  id: \"snap-3f9a1c2b\""), "{text}");

        // ...and it must parse, since the entry is built from the parsed form.
        let parsed = crate::manifest::Manifest::parse(&text);
        assert!(parsed.is_ok(), "{:?}", parsed.err());
        let parsed = parsed.unwrap();
        assert_eq!(
            parsed.root_uuid().as_deref(),
            Some("757f018a-71b0-44fc-aac8-c5b2ed165923")
        );
    }

    /// Recording the patch state would be inventing it — nobody knows until it
    /// boots, and the whole point of the honesty rule is that unknown is said out
    /// loud rather than filled in with something plausible.
    #[test]
    fn the_manifest_does_not_pretend_to_know_the_patch_state() {
        let text = manifest_text(&request(), "2026-10-09T00:00:00Z");
        assert!(
            text.contains("unknown until this snapshot boots"),
            "it must say the patches are unknown: {text}"
        );
        assert!(
            !text.contains("patches:\n  applied:"),
            "and must not emit the old, wrong shape: {text}"
        );
        // The mode is not `built-from-source`: the full kernel build did not run.
        assert!(text.contains("dev-module-build"), "{text}");
    }
}
