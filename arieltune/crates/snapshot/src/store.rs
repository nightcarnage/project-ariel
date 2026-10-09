// SPDX-License-Identifier: GPL-2.0-only
//! Discovery: what snapshots exist, and what is on disk for each.
//!
//! **Read-only.** Nothing in this module writes, which is what makes every verb
//! built on it — `list`, `show`, `doctor`, `verify` — safe to run on a blade you
//! are not responsible for. That is a property of *discovery*, not of the tool: the
//! writing verbs exist and are listed in the crate docs. Nothing they do can be
//! reached from here.
//!
//! Path conventions are a contract with existing snapshots, so they are
//! reproduced rather than improved (`docs/snapshot-tool.md` §3.3):
//!
//! ```text
//! /var/lib/kernel-snapshots/snap-<id>/manifest.yaml
//! /var/lib/kernel-snapshots/snap-<id>/manifest.yaml.hidden   # hide = rename
//! /var/lib/kernel-snapshots/.golden-snapshot                 # id as text
//! /boot/vmlinuz-snap-<id>
//! /boot/initramfs-snap-<id>.img
//! /boot/snap-<id>.cmdline
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::grub;
use crate::manifest::Manifest;

/// The id prefix every snapshot carries.
pub const ID_PREFIX: &str = "snap-";

/// The file, directly under the store root, holding the golden snapshot's id as
/// text. `list` marks it with a star and the manifest calls the same snapshot
/// `status: golden` — which is why the writer records both, so they cannot
/// disagree about which snapshot is the baseline.
pub const GOLDEN_MARKER: &str = ".golden-snapshot";

/// Where the shell tool keeps its manifests.
pub const DEFAULT_ROOT: &str = "/var/lib/kernel-snapshots";
/// Boot artefacts live beside the distro's own kernels.
pub const DEFAULT_BOOT: &str = "/boot";
/// Our GRUB entries.
pub const DEFAULT_GRUB_CUSTOM: &str = "/etc/grub.d/40_custom";
/// The generated GRUB menu.
pub const DEFAULT_GRUB_CFG: &str = "/boot/grub/grub.cfg";
/// GRUB's own settings, where `GRUB_DEFAULT=saved` must live for a pin to work.
pub const DEFAULT_GRUB_DEFAULTS: &str = "/etc/default/grub";
/// The kernel command line, read to learn which snapshot this boot came from.
pub const DEFAULT_CMDLINE: &str = "/proc/cmdline";

/// Every path the tool reads, in one place so tests can point them at a fixture.
#[derive(Debug, Clone)]
pub struct Paths {
    pub root: PathBuf,
    pub boot: PathBuf,
    pub grub_custom: PathBuf,
    pub grub_cfg: PathBuf,
    pub cmdline: PathBuf,
    pub grub_defaults: PathBuf,
}

impl Default for Paths {
    fn default() -> Self {
        Paths {
            root: PathBuf::from(DEFAULT_ROOT),
            boot: PathBuf::from(DEFAULT_BOOT),
            grub_custom: PathBuf::from(DEFAULT_GRUB_CUSTOM),
            grub_cfg: PathBuf::from(DEFAULT_GRUB_CFG),
            cmdline: PathBuf::from(DEFAULT_CMDLINE),
            grub_defaults: PathBuf::from(DEFAULT_GRUB_DEFAULTS),
        }
    }
}

impl Paths {
    /// The paths of a real blade.
    pub fn system() -> Self {
        Paths::default()
    }

    /// The same layout under a different root, for tests and `--root`.
    pub fn under(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Paths {
            boot: root.join("boot"),
            grub_custom: root.join("etc/grub.d/40_custom"),
            grub_cfg: root.join("boot/grub/grub.cfg"),
            cmdline: root.join("proc/cmdline"),
            grub_defaults: root.join("etc/default/grub"),
            root,
        }
    }
}

/// One snapshot, as found on disk. Every field is an observed fact; nothing is
/// inferred from another field.
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub id: String,
    /// Absent when neither `manifest.yaml` nor its `.hidden` form is readable.
    #[serde(skip)]
    pub manifest: Option<Manifest>,
    /// Set when a manifest file **exists but cannot be parsed**, with the error.
    ///
    /// Kept separate from "no manifest": a snapshot directory with no manifest
    /// is a bare id, which is legitimate, while a manifest that is present and
    /// unreadable is a defect that would otherwise be invisible — the snapshot
    /// would just render as its own id, exactly like the healthy case.
    pub manifest_error: Option<String>,
    /// Hidden by renaming the manifest to `manifest.yaml.hidden`.
    pub hidden: bool,
    /// Named by `/var/lib/kernel-snapshots/.golden-snapshot`.
    pub golden: bool,
    /// The snapshot this boot came from, per `BOOT_IMAGE=` in the cmdline.
    pub booted: bool,
    /// A block for this id exists in `40_custom`.
    pub grub_entry: bool,
    /// `/boot/vmlinuz-snap-<id>` exists.
    pub vmlinuz: bool,
    /// `/boot/initramfs-snap-<id>.img` exists.
    pub initramfs: bool,
    /// `/boot/snap-<id>.cmdline` exists.
    pub cmdline_file: bool,
}

impl Snapshot {
    /// `snapshot.description`, or the id when there is no manifest.
    pub fn description(&self) -> String {
        self.manifest
            .as_ref()
            .and_then(|m| m.description())
            .unwrap_or_else(|| self.id.clone())
    }

    /// `snapshot.status`, as recorded.
    pub fn status(&self) -> Option<String> {
        self.manifest.as_ref().and_then(|m| m.status())
    }

    /// `snapshot.timestamp`, as recorded (full ISO 8601).
    pub fn timestamp(&self) -> Option<String> {
        self.manifest.as_ref().and_then(|m| m.timestamp())
    }

    /// The date part of the timestamp, matching the shell tool's DATE column.
    pub fn date(&self) -> Option<String> {
        let stamp = self.timestamp()?;
        let date = stamp.get(..10)?;
        // Only claim a date when it looks like one; a malformed stamp is shown
        // by `show`, not silently reinterpreted here.
        let bytes = date.as_bytes();
        let shaped = bytes.len() == 10
            && bytes[4] == b'-'
            && bytes[7] == b'-'
            && bytes
                .iter()
                .enumerate()
                .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit());
        shaped.then(|| date.to_string())
    }

    /// `kernel.version`
    pub fn kernel_version(&self) -> Option<String> {
        self.manifest.as_ref().and_then(|m| m.kernel_version())
    }

    /// True when all three boot artefacts are present. A snapshot missing any of
    /// them cannot boot, whatever its manifest says.
    pub fn boot_artifacts_complete(&self) -> bool {
        self.vmlinuz && self.initramfs && self.cmdline_file
    }

    /// Sort key for display: newest first, as the shell tool lists them.
    fn sort_key(&self) -> (String, String) {
        // ISO 8601 sorts lexicographically, so no date maths is needed. Absent
        // timestamps sort last rather than being invented.
        let stamp = self.timestamp().unwrap_or_else(|| String::from("0000"));
        (stamp, self.id.clone())
    }

    /// Bytes this snapshot costs on disk: its three boot artefacts plus its own
    /// module copy.
    ///
    /// Worth showing on an archive blade, where the standing question is which of
    /// these can go — and that is not answerable from a list of ids.
    pub fn size_bytes(&self, paths: &Paths) -> u64 {
        [
            paths.boot.join(format!("vmlinuz-{}", self.id)),
            paths.boot.join(format!("initramfs-{}.img", self.id)),
            paths.boot.join(format!("{}.cmdline", self.id)),
            paths.root.join(&self.id).join("modules/amdgpu.ko.zst"),
        ]
        .iter()
        .filter_map(|path| std::fs::metadata(path).ok())
        .map(|meta| meta.len())
        .sum()
    }

    /// Everything about this snapshot that is wrong or worth knowing.
    ///
    /// **Computed, not recorded.** The manifest states what the writer believed
    /// at capture time; this states what is true on disk now. A snapshot whose
    /// initramfs was deleted still carries a manifest claiming it exists, and
    /// that difference is the point.
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();

        if let Some(err) = &self.manifest_error {
            out.push(format!(
                "manifest is not valid YAML — the fields shown came from a line scan ({err})"
            ));
        }

        let mut missing = Vec::new();
        if !self.vmlinuz {
            missing.push("vmlinuz");
        }
        if !self.initramfs {
            missing.push("initramfs");
        }
        if !self.cmdline_file {
            missing.push("cmdline");
        }
        if !missing.is_empty() {
            out.push(format!(
                "cannot boot: {} missing from /boot",
                missing.join(" and ")
            ));
        }

        if !self.grub_entry {
            out.push("no GRUB entry — not bootable from the menu".to_string());
        }

        if self.hidden {
            out.push(
                "hidden: the manifest is renamed, so the shell tool omits it from its list"
                    .to_string(),
            );
        }

        // The auditability gap: a build-from-source id hashes four source files
        // and Module.symvers, and the manifest records none of them, so the id
        // cannot be re-derived once the tree is gone. Three snapshots on blade
        // 104 are already in that state.
        let built_from_source = self
            .manifest
            .as_ref()
            .and_then(|m| m.source_tree_mode())
            .as_deref()
            == Some("built-from-source");
        if built_from_source && self.manifest_error.is_none() {
            out.push(
                "id hashed from a source tree, but the manifest records none of its inputs — \
                 it cannot be re-derived"
                    .to_string(),
            );
        }

        out
    }
}

/// Bytes as a short human string, for on-screen totals.
///
/// Deliberately coarse (one decimal, four units): this exists to answer "can I
/// afford this?" at a glance, not to be parsed.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Reads a snapshot store. Holds paths only; every method is a fresh read, so a
/// long-lived instance cannot show stale state.
#[derive(Debug, Clone, Default)]
pub struct Store {
    pub paths: Paths,
}

impl Store {
    pub fn new(paths: Paths) -> Self {
        Store { paths }
    }

    /// The store of a real blade.
    pub fn system() -> Self {
        Store::new(Paths::system())
    }

    /// The id this boot came from, if any.
    ///
    /// Mirrors `get_current_booted_snap()`: take `BOOT_IMAGE=`, drop the
    /// `/boot/vmlinuz-` prefix, and accept the result only when it starts with
    /// `snap-`. A distro kernel returns `None` rather than a wrong answer.
    pub fn booted_id(&self) -> Option<String> {
        let cmdline = std::fs::read_to_string(&self.paths.cmdline).ok()?;
        booted_id_from_cmdline(&cmdline)
    }

    /// The golden snapshot's id, if the marker is present.
    pub fn golden_id(&self) -> Option<String> {
        let text = std::fs::read_to_string(self.paths.root.join(GOLDEN_MARKER)).ok()?;
        let id = text.trim();
        (!id.is_empty()).then(|| id.to_string())
    }

    /// Every snapshot in the store, newest first.
    ///
    /// A store that does not exist is an empty store, not an error: this runs on
    /// blades and workstations alike and must not fail just for being elsewhere.
    pub fn discover(&self) -> Result<Vec<Snapshot>> {
        let mut snapshots = Vec::new();
        let entries = match std::fs::read_dir(&self.paths.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(snapshots),
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("reading snapshot root {}", self.paths.root.display())
                })
            }
        };

        let golden = self.golden_id();
        let booted = self.booted_id();
        let custom = std::fs::read_to_string(&self.paths.grub_custom).unwrap_or_default();
        let entry_ids: Vec<String> = grub::parse_custom(&custom)
            .into_iter()
            .map(|e| e.id)
            .collect();

        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with(ID_PREFIX) {
                continue;
            }

            let dir = entry.path();
            let visible_manifest = dir.join("manifest.yaml");
            let hidden_manifest = dir.join("manifest.yaml.hidden");

            let hidden = !visible_manifest.is_file() && hidden_manifest.is_file();

            // `Manifest::load` only fails on I/O now, so an `Err` here means a
            // file that exists and cannot be READ. A file that exists and cannot
            // be PARSED loads fine and reports itself through `parse_error`.
            let (manifest, manifest_error) = match Manifest::load(&visible_manifest) {
                Ok(manifest) => (Some(manifest), None),
                Err(visible_err) => match Manifest::load(&hidden_manifest) {
                    Ok(manifest) => (Some(manifest), None),
                    Err(hidden_err) => {
                        // Name a file that is actually there.
                        if visible_manifest.is_file() {
                            (None, Some(visible_err.to_string()))
                        } else if hidden_manifest.is_file() {
                            (None, Some(hidden_err.to_string()))
                        } else {
                            (None, None)
                        }
                    }
                },
            };
            let manifest_error = manifest
                .as_ref()
                .and_then(|m| m.parse_error().map(str::to_string))
                .or(manifest_error);

            // The id in the manifest, when present, is what the tool believes;
            // the directory name is what everything else keys on. They disagree
            // only if someone renamed a directory by hand, so they are kept
            // separate rather than reconciled.
            snapshots.push(Snapshot {
                id: name.clone(),
                manifest,
                manifest_error,
                hidden,
                golden: golden.as_deref() == Some(name.as_str()),
                booted: booted.as_deref() == Some(name.as_str()),
                grub_entry: entry_ids.iter().any(|id| id == &name),
                vmlinuz: self.paths.boot.join(format!("vmlinuz-{name}")).is_file(),
                initramfs: self
                    .paths
                    .boot
                    .join(format!("initramfs-{name}.img"))
                    .is_file(),
                cmdline_file: self.paths.boot.join(format!("{name}.cmdline")).is_file(),
            });
        }

        // Newest first, then by id, so the order is total and stable.
        snapshots.sort_by_key(|s| std::cmp::Reverse(s.sort_key()));
        Ok(snapshots)
    }

    /// One snapshot by id.
    pub fn get(&self, id: &str) -> Result<Option<Snapshot>> {
        Ok(self.discover()?.into_iter().find(|s| s.id == id))
    }

    /// Snapshots the shell tool would list: hidden ones are excluded, because
    /// `snapshot list` excludes them and `list --ids` is a contract that
    /// arieltune parses.
    pub fn visible(&self) -> Result<Vec<Snapshot>> {
        Ok(self.discover()?.into_iter().filter(|s| !s.hidden).collect())
    }
}

/// Extract the snapshot id from a kernel command line.
///
/// Pure, so the parsing is unit-tested instead of trusted: `BOOT_IMAGE=` is
/// bootloader-supplied text.
pub fn booted_id_from_cmdline(cmdline: &str) -> Option<String> {
    let boot_image = cmdline
        .split_whitespace()
        .find_map(|word| word.strip_prefix("BOOT_IMAGE="))?;
    let stem = Path::new(boot_image).file_name()?.to_str()?;
    let stem = stem.strip_prefix("vmlinuz-")?;
    stem.starts_with(ID_PREFIX).then(|| stem.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "snapshot-store-test-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// Build a fixture that looks like a blade: two snapshots, one hidden, a
    /// golden marker, boot artefacts, and a `40_custom` with one entry.
    fn fixture(tag: &str) -> PathBuf {
        let root = temp_dir(tag);
        let snap = root.join("snap-aaaaaaaa");
        std::fs::create_dir_all(snap.join("modules")).unwrap();
        std::fs::write(
            snap.join("manifest.yaml"),
            "snapshot:\n  id: \"snap-aaaaaaaa\"\n  timestamp: \"2026-10-08T19:20:41Z\"\n  description: \"newest\"\n  status: \"active\"\n",
        )
        .unwrap();

        let hidden = root.join("snap-bbbbbbbb");
        std::fs::create_dir_all(&hidden).unwrap();
        std::fs::write(
            hidden.join("manifest.yaml.hidden"),
            "snapshot:\n  id: \"snap-bbbbbbbb\"\n  timestamp: \"2026-08-25T10:00:00Z\"\n  description: \"older, hidden\"\n",
        )
        .unwrap();

        std::fs::write(root.join(".golden-snapshot"), "snap-aaaaaaaa\n").unwrap();

        std::fs::create_dir_all(root.join("boot")).unwrap();
        for name in ["snap-aaaaaaaa", "snap-bbbbbbbb"] {
            std::fs::write(root.join(format!("boot/vmlinuz-{name}")), b"k").unwrap();
            std::fs::write(root.join(format!("boot/{name}.cmdline")), b"c").unwrap();
        }
        // Only the first gets an initramfs, so the completeness flag is exercised.
        std::fs::write(root.join("boot/initramfs-snap-aaaaaaaa.img"), b"i").unwrap();

        std::fs::create_dir_all(root.join("etc/grub.d")).unwrap();
        std::fs::write(
            root.join("etc/grub.d/40_custom"),
            "# === snap-aaaaaaaa: newest ===\nmenuentry 'x' --id 'snap-aaaaaaaa' {\n}\n",
        )
        .unwrap();

        std::fs::create_dir_all(root.join("proc")).unwrap();
        std::fs::write(
            root.join("proc/cmdline"),
            "BOOT_IMAGE=/boot/vmlinuz-snap-aaaaaaaa root=UUID=8387e4a0 rw\n",
        )
        .unwrap();

        root
    }

    #[test]
    fn finds_both_snapshots_newest_first() {
        let root = fixture("discover");
        let store = Store::new(Paths::under(&root));
        let found = store.discover().unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].id, "snap-aaaaaaaa");
        assert_eq!(found[1].id, "snap-bbbbbbbb");
    }

    #[test]
    fn reports_the_observed_facts() {
        let root = fixture("facts");
        let store = Store::new(Paths::under(&root));
        let found = store.discover().unwrap();

        let newest = &found[0];
        assert!(newest.golden);
        assert!(newest.booted);
        assert!(newest.grub_entry);
        assert!(newest.boot_artifacts_complete());
        assert_eq!(newest.date().as_deref(), Some("2026-10-08"));
        assert_eq!(newest.status().as_deref(), Some("active"));

        let older = &found[1];
        assert!(older.hidden, "manifest.yaml.hidden means hidden");
        assert!(!older.golden);
        assert!(!older.booted);
        assert!(!older.grub_entry);
        assert!(
            !older.boot_artifacts_complete(),
            "its initramfs is missing, and the flag must say so"
        );
        assert_eq!(older.description(), "older, hidden");
    }

    /// A missing store is empty, not an error — this runs on workstations.
    #[test]
    fn absent_root_is_an_empty_store() {
        let store = Store::new(Paths::under("/nonexistent/snapshot/root"));
        assert!(store.discover().unwrap().is_empty());
        assert!(store.booted_id().is_none());
        assert!(store.golden_id().is_none());
    }

    #[test]
    fn booted_id_ignores_a_distro_kernel() {
        assert_eq!(
            booted_id_from_cmdline("BOOT_IMAGE=/boot/vmlinuz-snap-0848359a root=UUID=x"),
            Some("snap-0848359a".to_string())
        );
        assert_eq!(
            booted_id_from_cmdline("BOOT_IMAGE=/boot/vmlinuz-linux-cachyos-bore root=UUID=x"),
            None
        );
        assert_eq!(booted_id_from_cmdline("no boot image here"), None);
    }

    /// A malformed timestamp must not be truncated into a plausible date.
    /// A minimal snapshot, for exercising the derived views without a fixture of
    /// files on disk.
    fn bare_snapshot(id: &str) -> Snapshot {
        Snapshot {
            id: id.to_string(),
            manifest: None,
            manifest_error: None,
            hidden: false,
            golden: false,
            booted: false,
            grub_entry: true,
            vmlinuz: true,
            initramfs: true,
            cmdline_file: true,
        }
    }

    /// A healthy snapshot has nothing to report — otherwise the warnings list
    /// becomes noise nobody reads.
    #[test]
    fn a_healthy_snapshot_has_no_warnings() {
        assert!(bare_snapshot("snap-aaaaaaaa").warnings().is_empty());
    }

    #[test]
    fn warnings_name_the_missing_artefacts() {
        let mut snap = bare_snapshot("snap-aaaaaaaa");
        snap.initramfs = false;
        let warnings = snap.warnings();
        assert!(
            warnings.iter().any(|w| w.contains("cannot boot")),
            "{warnings:?}"
        );
        assert!(warnings.iter().any(|w| w.contains("initramfs")));
        assert!(
            !warnings.iter().any(|w| w.contains("vmlinuz")),
            "vmlinuz is present, so it must not be listed: {warnings:?}"
        );
    }

    #[test]
    fn warnings_flag_a_missing_grub_entry_and_a_hidden_manifest() {
        let mut snap = bare_snapshot("snap-aaaaaaaa");
        snap.grub_entry = false;
        snap.hidden = true;
        let warnings = snap.warnings();
        assert!(warnings.iter().any(|w| w.contains("no GRUB entry")));
        assert!(warnings.iter().any(|w| w.contains("hidden")));
    }

    /// The auditability gap this crate found: a build-from-source id hashes four
    /// source files and Module.symvers, none of which the manifest records.
    #[test]
    fn warnings_flag_an_underivable_id() {
        let mut snap = bare_snapshot("snap-aaaaaaaa");
        snap.manifest =
            Some(Manifest::parse("source_tree:\n  mode: \"built-from-source\"\n").expect("parses"));
        let warnings = snap.warnings();
        assert!(
            warnings.iter().any(|w| w.contains("cannot be re-derived")),
            "{warnings:?}"
        );

        // A running-module capture records what it hashed, so it is fine.
        let mut fine = bare_snapshot("snap-bbbbbbbb");
        fine.manifest =
            Some(Manifest::parse("source_tree:\n  mode: \"running-module\"\n").expect("parses"));
        assert!(fine.warnings().is_empty(), "{:?}", fine.warnings());
    }

    #[test]
    fn an_unreadable_manifest_is_itself_a_warning() {
        let mut snap = bare_snapshot("snap-aaaaaaaa");
        snap.manifest_error = Some("could not find expected ':'".to_string());
        let warnings = snap.warnings();
        assert!(warnings.iter().any(|w| w.contains("not valid YAML")));
    }

    #[test]
    fn sizes_add_up_from_the_real_files() {
        let root = fixture("sizes");
        let store = Store::new(Paths::under(&root));
        let found = store.discover().unwrap();
        let newest = found.iter().find(|s| s.id == "snap-aaaaaaaa").unwrap();
        // The fixture writes 1 byte each for vmlinuz, cmdline and initramfs.
        assert_eq!(newest.size_bytes(&store.paths), 3);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn human_bytes_reads_at_a_glance() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1024 * 1024 * 3 / 2), "1.5 MiB");
        assert_eq!(human_bytes(2 * 1024 * 1024 * 1024), "2.0 GiB");
    }

    #[test]
    fn a_malformed_timestamp_must_not_be_truncated_into_a_date() {
        let root = temp_dir("baddate");
        let snap = root.join("snap-cccccccc");
        std::fs::create_dir_all(&snap).unwrap();
        std::fs::write(
            snap.join("manifest.yaml"),
            "snapshot:\n  id: \"snap-cccccccc\"\n  timestamp: \"not-a-date-at-all\"\n",
        )
        .unwrap();
        let store = Store::new(Paths::under(&root));
        let found = store.discover().unwrap();
        assert_eq!(found[0].date(), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `list` must exclude hidden snapshots, because the shell tool does and
    /// `list --ids` is parsed by arieltune.
    #[test]
    fn visible_excludes_hidden_snapshots() {
        let root = fixture("visible");
        let store = Store::new(Paths::under(&root));

        let all = store.discover().unwrap();
        assert_eq!(all.len(), 2, "discover sees everything");

        let visible = store.visible().unwrap();
        assert_eq!(visible.len(), 1, "visible drops the hidden one");
        assert_eq!(visible[0].id, "snap-aaaaaaaa");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A manifest that exists but will not parse must be *named*, not rendered
    /// as a bare id — otherwise the defect looks exactly like the healthy
    /// "no manifest" case.
    #[test]
    fn an_unparseable_manifest_is_reported() {
        let root = temp_dir("badmanifest");
        let snap = root.join("snap-dddddddd");
        std::fs::create_dir_all(&snap).unwrap();
        std::fs::write(snap.join("manifest.yaml"), "this: [is not: valid\n").unwrap();

        let store = Store::new(Paths::under(&root));
        let found = store.discover().unwrap();
        assert_eq!(found.len(), 1);
        // The manifest is still loaded (so its fields can be read), and the
        // defect is reported alongside it.
        let manifest = found[0]
            .manifest
            .as_ref()
            .expect("still loaded so its fields can be scanned");
        assert!(manifest.parse_error().is_some(), "the defect is recorded");
        let err = found[0]
            .manifest_error
            .as_deref()
            .expect("the parse failure is surfaced");
        assert!(!err.is_empty(), "unexpected message: {err}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// No manifest at all is legitimate and must NOT be reported as an error.
    #[test]
    fn a_directory_with_no_manifest_is_not_an_error() {
        let root = temp_dir("nomanifest");
        std::fs::create_dir_all(root.join("snap-eeeeeeee")).unwrap();
        let store = Store::new(Paths::under(&root));
        let found = store.discover().unwrap();
        assert_eq!(found.len(), 1);
        assert!(found[0].manifest.is_none());
        assert!(found[0].manifest_error.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}
