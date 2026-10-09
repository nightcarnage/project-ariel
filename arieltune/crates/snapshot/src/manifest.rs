// SPDX-License-Identifier: GPL-2.0-only
//! The manifest: `manifest.yaml`, written by the shell tool.
//!
//! ## Why this is a `Value` tree and not a struct
//!
//! The manifest is emitted by a heredoc with shell interpolation, so its field
//! *types* are not consistent between snapshots. Observed on blade 104:
//!
//! ```text
//! module:
//!   srcversion: 53DEA5FD08F79B2EB5D0913   # bare
//!   size: 6584284 bytes                    # a string that reads like a number
//! patches:
//!   markers: 0                             # an integer
//!   files: []                              # an empty list
//! kernel:
//!   version: "7.0.9"                       # quoted, and sometimes the SHORT form
//! ```
//!
//! A typed `struct` with `#[derive(Deserialize)]` would refuse some of those and
//! silently coerce others. Since the field names are a contract with existing
//! snapshots (and with the shell tool during the transition), this module parses
//! into a [`Value`] tree and exposes *coercing* accessors that accept the
//! variants the writer actually produces, so a snapshot that was captured
//! correctly is never rejected over its formatting.
//!
//! Unknown fields are preserved by construction — nothing is thrown away, which
//! matters for the write path that comes later.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_yaml::Value;

/// A parsed `manifest.yaml`.
///
/// Parsing is deliberately **not** allowed to fail. Five manifests across blades
/// 104 and 115 are not valid YAML — the old writer ran `grep -c` (which prints
/// its count *and* exits 1) with `|| echo 0`, so a marker-less module appended a
/// bare `0` on its own line — and the shell tool reads them anyway with `grep`.
/// Refusing them here would make this tool worse at describing real snapshots
/// than the one it replaces. So the YAML error is recorded
/// ([`Manifest::parse_error`]) and the accessors fall back to the same
/// first-match line scan the shell uses.
#[derive(Debug, Clone)]
pub struct Manifest {
    /// The parsed tree, or `Value::Null` when the text did not parse.
    value: Value,
    /// The original text, kept so a failed parse can still be scanned.
    text: String,
    /// Set when the YAML did not parse.
    parse_error: Option<String>,
    /// Path this was read from, when it came from disk. Kept so error messages
    /// and `show` can name the artefact rather than just its contents.
    source: Option<PathBuf>,
}

impl Manifest {
    /// Parse manifest text. Never fails on malformed YAML; see the type docs.
    pub fn parse(text: &str) -> Result<Self> {
        let (value, parse_error) = match serde_yaml::from_str::<Value>(text) {
            Ok(value) => (value, None),
            Err(err) => (Value::Null, Some(err.to_string())),
        };
        Ok(Manifest {
            value,
            text: text.to_string(),
            parse_error,
            source: None,
        })
    }

    /// Read and parse `path`.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading manifest {}", path.display()))?;
        let mut manifest = Manifest::parse(&text)?;
        manifest.source = Some(path.to_path_buf());
        Ok(manifest)
    }

    /// Where this manifest came from, if it was loaded from disk.
    pub fn source(&self) -> Option<&Path> {
        self.source.as_deref()
    }

    /// The raw tree, for fields this API does not surface yet.
    pub fn raw(&self) -> &Value {
        &self.value
    }

    fn at(&self, block: &str, key: &str) -> Option<&Value> {
        self.value.get(block)?.get(key)
    }

    /// Look up `key` anywhere in the tree, at most one mapping deep.
    ///
    /// `registered: true` is written by the register path in a heredoc whose
    /// nesting has varied, so its depth is not something to rely on.
    fn anywhere(&self, key: &str) -> Option<&Value> {
        if let Some(v) = self.value.get(key) {
            return Some(v);
        }
        if let Some(map) = self.value.as_mapping() {
            for (_, child) in map {
                if let Some(v) = child.get(key) {
                    return Some(v);
                }
            }
        }
        None
    }

    fn text(&self, value: Option<&Value>) -> Option<String> {
        match value? {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    }

    /// The error from the YAML parse, when the file could not be parsed.
    ///
    /// Set instead of failing, so a broken manifest is *visible* while still
    /// being readable.
    pub fn parse_error(&self) -> Option<&str> {
        self.parse_error.as_deref()
    }

    /// First-match line scan for `key`, mirroring the shell tool's
    /// `grep <key>: | head -1`.
    ///
    /// Used only when the YAML did not parse. Deliberately the *same* heuristic
    /// the tool being replaced uses, so nothing it could describe is lost here.
    fn scan(&self, key: &str) -> Option<String> {
        for line in self.text.lines() {
            let trimmed = line.trim_start();
            let Some(rest) = trimmed.strip_prefix(key) else {
                continue;
            };
            // Require the colon, so `vmlinuz` does not match `vmlinuz_source`.
            let Some(rest) = rest.strip_prefix(':') else {
                continue;
            };
            let value = rest.trim().trim_matches('"').trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
        None
    }

    /// A field from the parsed tree, falling back to [`Manifest::scan`].
    fn field(&self, block: &str, key: &str, scan_key: &str) -> Option<String> {
        self.text(self.at(block, key))
            .or_else(|| self.scan(scan_key))
    }

    // ---- semantic accessors ------------------------------------------------

    /// `snapshot.id`
    pub fn id(&self) -> Option<String> {
        self.field("snapshot", "id", "id")
    }

    /// `snapshot.description`
    pub fn description(&self) -> Option<String> {
        self.field("snapshot", "description", "description")
    }

    /// `snapshot.timestamp`
    pub fn timestamp(&self) -> Option<String> {
        self.field("snapshot", "timestamp", "timestamp")
    }

    /// `snapshot.status`
    pub fn status(&self) -> Option<String> {
        self.field("snapshot", "status", "status")
    }

    /// `snapshot.parent` — the id this snapshot was derived from.
    pub fn parent(&self) -> Option<String> {
        self.field("snapshot", "parent", "parent")
    }

    /// `kernel.version`. **Note:** three manifests on blade 104 record the short
    /// `7.0.9` where the id was computed with `7.0.9-1-cachyos`, so this is the
    /// *recorded* release and not proof of which string produced the id.
    pub fn kernel_version(&self) -> Option<String> {
        self.field("kernel", "version", "version")
    }

    /// `source_tree.mode` — `built-from-source`, `running-module`, or absent on
    /// manifests written before the field existed.
    pub fn source_tree_mode(&self) -> Option<String> {
        self.field("source_tree", "mode", "mode")
    }

    /// True when the register path wrote `registered: true`.
    pub fn registered(&self) -> bool {
        matches!(self.anywhere("registered"), Some(Value::Bool(true)))
            || self.text(self.anywhere("registered")).as_deref() == Some("true")
            || self.scan("registered").as_deref() == Some("true")
    }

    /// `grub.entry_title`
    pub fn entry_title(&self) -> Option<String> {
        self.field("grub", "entry_title", "entry_title")
    }

    /// `grub.root_uuid`
    pub fn root_uuid(&self) -> Option<String> {
        self.field("grub", "root_uuid", "root_uuid")
    }

    /// `boot_files.vmlinuz`
    pub fn vmlinuz(&self) -> Option<String> {
        self.field("boot_files", "vmlinuz", "vmlinuz")
    }

    /// `boot_files.initramfs`
    pub fn initramfs(&self) -> Option<String> {
        self.field("boot_files", "initramfs", "initramfs")
    }

    /// The recorded kernel command line.
    ///
    /// **Not safe to boot with.** This is the *live* cmdline at capture time, so
    /// it names whichever kernel was booted then, and can contain `BOOT_IMAGE=`
    /// more than once — `snap-da794092`'s has it twice, both pointing at
    /// `vmlinuz-snap-2d2b1734`. Any code that builds a GRUB line from this must
    /// rewrite `BOOT_IMAGE=`.
    pub fn cmdline(&self) -> Option<String> {
        self.text(self.value.get("cmdline"))
            .or_else(|| self.scan("cmdline"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real manifest, taken verbatim from `snap-da794092` on blade 104 —
    /// including the duplicated `BOOT_IMAGE=` and the unquoted `size`.
    const REAL: &str = r#"snapshot:
  id: "snap-da794092"
  timestamp: "2026-10-08T19:20:41Z"
  description: "merge-test PR#5 tree-matching, running-module path (104, 2026-10-08)"
  status: "golden"
  host: "04"
  parent: "snap-2d2b1734"
  git_commit: "not-a-git-repo"
kernel:
  version: "7.0.9-1-cachyos"
  vmlinuz_source: "/boot/vmlinuz-linux-cachyos"
  vmlinuz_snapshot: "/boot/vmlinuz-snap-da794092"
initramfs:
  snapshot: "/boot/initramfs-snap-da794092.img"
module:
  path: /var/lib/kernel-snapshots/snap-da794092/modules/amdgpu.ko.zst
  srcversion: 53DEA5FD08F79B2EB5D0913
  vermagic: 7.0.9-1-cachyos SMP preempt mod_unload
  size: 6584284 bytes
patches:
  applied: false
  markers: 0
  files: []
cmdline: "BOOT_IMAGE=/boot/vmlinuz-snap-2d2b1734 BOOT_IMAGE=/boot/vmlinuz-snap-2d2b1734 root=UUID=8387e4a0 rw"
grub:
  entry_id: "snap-da794092"
  entry_title: "[snap-da794092] merge-test PR#5 tree-matching, running-module path (104, 2026-10-08)"
  root_uuid: "8387e4a0-b18f-4142-b61d-59fa9acc3865"
boot_files:
  vmlinuz: "/boot/vmlinuz-snap-da794092"
  initramfs: "/boot/initramfs-snap-da794092.img"
  cmdline_file: "/boot/snap-da794092.cmdline"
source_tree:
  path: ""
  mode: "running-module"
  bc250_markers: "0"
config:
  mkinitcpio_conf_md5: "abc123"
firmware:
  cyan_skillfish_md5: "def456"
test_results:
  boot_test: "PASS"
  gpu_alive: "PASS"
"#;

    fn real() -> Manifest {
        Manifest::parse(REAL).expect("real manifest parses")
    }

    #[test]
    fn reads_the_fields_the_tool_needs() {
        let m = real();
        assert_eq!(m.id().as_deref(), Some("snap-da794092"));
        assert_eq!(m.status().as_deref(), Some("golden"));
        assert_eq!(m.parent().as_deref(), Some("snap-2d2b1734"));
        assert_eq!(m.kernel_version().as_deref(), Some("7.0.9-1-cachyos"));
        assert_eq!(m.source_tree_mode().as_deref(), Some("running-module"));
        assert_eq!(
            m.root_uuid().as_deref(),
            Some("8387e4a0-b18f-4142-b61d-59fa9acc3865")
        );
        assert_eq!(m.vmlinuz().as_deref(), Some("/boot/vmlinuz-snap-da794092"));
        assert!(m.description().unwrap().starts_with("merge-test PR#5"));
    }

    /// The duplicate-`BOOT_IMAGE` defect is preserved on read, not silently
    /// tidied: the reader must expose what is on disk so callers can decide.
    #[test]
    fn cmdline_exposes_the_duplicate_boot_image() {
        let cmdline = real().cmdline().expect("cmdline present");
        assert_eq!(cmdline.matches("BOOT_IMAGE=").count(), 2);
        assert!(cmdline.contains("vmlinuz-snap-2d2b1734"));
        assert!(!cmdline.contains("vmlinuz-snap-da794092"));
    }

    /// An unquoted `6584284 bytes` must not fail the parse, and the neighbours
    /// that read like numbers must still be reachable as text.
    #[test]
    fn tolerates_mixed_scalar_types() {
        let m = real();
        assert_eq!(m.raw()["module"]["size"].as_str(), Some("6584284 bytes"));
        assert_eq!(m.raw()["patches"]["markers"].as_u64(), Some(0));
        assert!(m.raw()["patches"]["files"]
            .as_sequence()
            .unwrap()
            .is_empty());
    }

    /// `registered: true` is written at a depth that has varied, so the lookup
    /// must not depend on where it sits.
    #[test]
    fn registered_is_found_at_any_depth() {
        let top = Manifest::parse("registered: true\n").unwrap();
        assert!(top.registered());

        let nested = Manifest::parse("snapshot:\n  registered: true\n").unwrap();
        assert!(nested.registered());

        let absent = Manifest::parse("snapshot:\n  id: \"snap-x\"\n").unwrap();
        assert!(!absent.registered());
    }

    /// A manifest written by the pre-fix shell tool must still be readable.
    ///
    /// This text is the real defect from `snap-870746fb` on blade 104: the old
    /// writer ran `grep -c` (which prints its count *and* exits 1) with
    /// `|| echo 0`, so a marker-less module appended a bare `0` on its own line
    /// and the file stopped being valid YAML.
    const LEGACY_BROKEN: &str = r#"snapshot:
  id: "snap-870746fb"
  timestamp: "2026-07-05T18:00:00Z"
  description: "p0 7.0.9-gcf stock (rescue) 2026-07-05"
  status: "active"
module:
  size: "5823372 bytes"
  bc250_markers: 0
0

initramfs:
  snapshot: "/boot/initramfs-snap-870746fb.img"
"#;

    #[test]
    fn a_legacy_unparseable_manifest_still_yields_its_fields() {
        let m = Manifest::parse(LEGACY_BROKEN).expect("never fails");
        assert!(
            m.parse_error().is_some(),
            "the defect must be recorded, not hidden"
        );
        // ...and the fields the shell tool would show are still available.
        assert_eq!(m.id().as_deref(), Some("snap-870746fb"));
        assert_eq!(
            m.description().as_deref(),
            Some("p0 7.0.9-gcf stock (rescue) 2026-07-05")
        );
        assert_eq!(m.timestamp().as_deref(), Some("2026-07-05T18:00:00Z"));
        assert_eq!(m.status().as_deref(), Some("active"));
    }

    #[test]
    fn a_clean_manifest_records_no_parse_error() {
        assert!(real().parse_error().is_none());
    }
}
