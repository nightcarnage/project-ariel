// SPDX-License-Identifier: GPL-2.0-only
//! GRUB surfaces: our `40_custom` blocks, and reading `grub.cfg`.
//!
//! ## Parse, do not `sed`
//!
//! The shell tool edits `/etc/grub.d/40_custom` with `sed` range deletes, and
//! uses **two different ranges** for the same conceptual operation:
//!
//! ```text
//! grub_remove_entry()  sed -i "/^# === ${snap_id}:/,/^}/d"     # up to the closing brace
//! cmd_prune()          sed -i "/^# === ${snap_id}:/,/^$/d"     # up to the first blank line
//! ```
//!
//! They happen to agree on today's block shape and diverge the moment a block
//! contains a blank line. Modelling the file as entries with a deterministic
//! renderer removes that class outright, and is a precondition for nesting
//! entries under a submenu.
//!
//! ## Brace depth, not indentation
//!
//! [`parse_menu_entries`] decides nesting from brace depth because `grub.cfg`
//! indents menu lines inside `if … fi` blocks that are *not* submenus — the
//! firmware entries look nested and are top-level. Reading whitespace instead
//! misclassifies them.

use serde::Serialize;

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

use crate::manifest::Manifest;

/// One of our blocks in `/etc/grub.d/40_custom`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CustomEntry {
    /// Snapshot id, taken from the `# === <id>: <description> ===` header.
    pub id: String,
    /// Description from the same header (everything after the first `": "`).
    pub description: String,
    /// The header line plus any comment lines that follow it, verbatim.
    pub comments: Vec<String>,
    /// The `menuentry '…' --id '…' {` line, when present.
    pub menuentry: Option<String>,
    /// Body lines after the `menuentry` line, including the closing `}`.
    pub body: Vec<String>,
}

impl CustomEntry {
    /// Render back to file text, byte-identical to the input for a block this
    /// parser produced.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for line in &self.comments {
            out.push_str(line);
            out.push('\n');
        }
        if let Some(menuentry) = &self.menuentry {
            out.push_str(menuentry);
            out.push('\n');
        }
        for line in &self.body {
            out.push_str(line);
            out.push('\n');
        }
        out
    }
}

/// The header prefix and suffix our entries use.
const HEADER_PREFIX: &str = "# === ";
const HEADER_SUFFIX: &str = " ===";

fn parse_header(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim_end();
    let rest = trimmed.strip_prefix(HEADER_PREFIX)?;
    let rest = rest.strip_suffix(HEADER_SUFFIX)?;
    match rest.split_once(": ") {
        Some((id, desc)) => Some((id.to_string(), desc.to_string())),
        // A header with no description is still a header.
        None => Some((rest.to_string(), String::new())),
    }
}

/// True when `line` is a `menuentry` statement.
///
/// The trailing whitespace requirement is load-bearing: `10_linux` also writes
/// `menuentry_id_option="--id gnulinux-…"` assignment lines, which a looser
/// match counts as entries. That produced two phantom entries in the shell
/// tool and is why this is a named function with its own test.
fn is_menuentry(line: &str) -> bool {
    let t = line.trim_start();
    t.strip_prefix("menuentry").is_some_and(|rest| {
        rest.starts_with(' ') || rest.starts_with('\t') || rest.starts_with('\'')
    })
}

fn is_submenu(line: &str) -> bool {
    let t = line.trim_start();
    t.strip_prefix("submenu").is_some_and(|rest| {
        rest.starts_with(' ') || rest.starts_with('\t') || rest.starts_with('\'')
    })
}

/// The quoted title from a `menuentry`/`submenu` line, if it has one.
fn quoted_title(line: &str) -> Option<String> {
    let start = line.find('\'')?;
    let rest = &line[start + 1..];
    let end = rest.find('\'')?;
    Some(rest[..end].to_string())
}

/// Parse every block of ours out of a `40_custom` file.
pub fn parse_custom(text: &str) -> Vec<CustomEntry> {
    let lines: Vec<&str> = text.lines().collect();
    let mut entries = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let Some((id, description)) = parse_header(lines[i]) else {
            i += 1;
            continue;
        };

        let mut comments = vec![lines[i].to_string()];
        i += 1;
        while i < lines.len() && lines[i].trim_start().starts_with('#') {
            comments.push(lines[i].to_string());
            i += 1;
        }

        // An optional `menuentry` line, then everything up to and including the
        // line that closes it.
        let mut menuentry = None;
        if i < lines.len() && is_menuentry(lines[i]) {
            menuentry = Some(lines[i].to_string());
            i += 1;
        }

        let mut body = Vec::new();
        while i < lines.len() {
            let line = lines[i];
            body.push(line.to_string());
            i += 1;
            if line.trim() == "}" {
                break;
            }
        }

        entries.push(CustomEntry {
            id,
            description,
            comments,
            menuentry,
            body,
        });
    }

    entries
}

/// Render a whole file: `blocks` in order, with `passthrough` lines untouched.
pub fn render_custom(entries: &[CustomEntry]) -> String {
    entries.iter().map(CustomEntry::render).collect()
}

/// Remove the block for `id`, leaving every other byte of the file alone.
///
/// Returns the new text and whether anything was removed. Implemented by
/// scanning rather than by `parse` + re-render, so unrelated content in the file
/// (entries that are not ours) survives byte for byte.
pub fn remove_entry(text: &str, id: &str) -> (String, bool) {
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<&str> = Vec::new();
    let mut removed = false;
    let mut i = 0;

    while i < lines.len() {
        match parse_header(lines[i]) {
            Some((block_id, _)) if block_id == id => {
                removed = true;
                i += 1;
                // Skip trailing comments, the menuentry line, and the body.
                while i < lines.len() && lines[i].trim_start().starts_with('#') {
                    i += 1;
                }
                if i < lines.len() && is_menuentry(lines[i]) {
                    i += 1;
                }
                while i < lines.len() {
                    let line = lines[i];
                    i += 1;
                    if line.trim() == "}" {
                        break;
                    }
                }
            }
            _ => {
                out.push(lines[i]);
                i += 1;
            }
        }
    }

    let mut rendered = out.join("\n");
    // `lines()` drops a trailing newline; put it back when the input had one.
    if text.ends_with('\n') || !removed {
        rendered.push('\n');
    }
    (rendered, removed)
}

/// One `menuentry` as `grub.cfg` presents it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MenuEntry {
    /// `--id` value when the entry has one.
    pub id: Option<String>,
    pub title: String,
    /// `Some("Parent")` when nested, `None` at top level.
    pub submenu: Option<String>,
}

impl MenuEntry {
    /// The path `grub-reboot` needs: GRUB's `get_entry_number()` cannot resolve a
    /// nested entry from a bare id, it needs `Parent>Child`.
    pub fn path(&self) -> String {
        match &self.submenu {
            Some(parent) => format!("{parent}>{}", self.title),
            None => self.title.clone(),
        }
    }

    /// The id to show a human: the `--id` when there is one, else the title.
    pub fn display_id(&self) -> String {
        self.id.clone().unwrap_or_else(|| self.title.clone())
    }
}

/// The first quoted string in `s`, unquoted.
fn quoted(s: &str) -> Option<String> {
    let start = s.find(['\'', '"'])?;
    let quote = &s[start..start + 1];
    let rest = &s[start + 1..];
    let end = rest.find(quote)?;
    Some(rest[..end].to_string())
}

/// The id a `menuentry` line ends up with, in either of the two spellings
/// `grub.cfg` uses.
///
/// - `--id 'snap-11e61755'` — what our own `40_custom` blocks write.
/// - `$menuentry_id_option 'gnulinux-snap-11e61755-advanced-<uuid>'` — what
///   `10_linux` writes for every kernel it finds, which includes every
///   `vmlinuz-snap-*` this tool installs. The variable is assigned earlier in the
///   file, and that assignment line is deliberately *not* an entry (see
///   [`is_menuentry`]); the menuentry line carries the value it was given.
///
/// Both are real ids, and `grub-reboot` resolves either. Reading only the first
/// form hid the id of every distro-generated entry: `grub-list` printed a title in
/// the id column, and `boot --entry <that id>` refused an id that works. Found by
/// running the shell tool and this one side by side on blade 115.
fn id_option(line: &str) -> Option<String> {
    if let Some(at) = line.find("$menuentry_id_option") {
        if let Some(id) = quoted(&line[at..]) {
            return Some(id);
        }
    }
    let marker = "--id ";
    let rest = &line[line.find(marker)? + marker.len()..];
    quoted(rest).or_else(|| Some(rest.split_whitespace().next()?.to_string()))
}

/// Every menu entry in a `grub.cfg`, with its nesting, by brace depth.
pub fn parse_menu_entries(grub_cfg: &str) -> Vec<MenuEntry> {
    // (depth the submenu's *body* sits at, its title)
    let mut open: Vec<(usize, String)> = Vec::new();
    let mut entries = Vec::new();
    let mut depth: usize = 0;

    for line in grub_cfg.lines() {
        let opens = line.matches('{').count();
        let closes = line.matches('}').count();

        if is_menuentry(line) {
            let parent = open
                .iter()
                .rev()
                .find(|(d, _)| *d == depth)
                .map(|(_, title)| title.clone());
            entries.push(MenuEntry {
                id: id_option(line),
                title: quoted_title(line).unwrap_or_default(),
                submenu: parent,
            });
        } else if is_submenu(line) {
            open.push((depth + 1, quoted_title(line).unwrap_or_default()));
        }

        // Apply this line's braces, then drop any submenu we have left.
        depth = depth.saturating_add(opens).saturating_sub(closes);
        open.retain(|(d, _)| *d <= depth);
    }

    entries
}

/// A table of entries, for when the caller has to say what was available.
fn entry_table(entries: &[MenuEntry]) -> String {
    if entries.is_empty() {
        return "  (grub.cfg has no menu entries at all)".to_string();
    }
    let mut out = String::new();
    for entry in entries {
        let nested = if entry.submenu.is_some() {
            "  [nested]"
        } else {
            ""
        };
        out.push_str(&format!(
            "  {:<50} {}{nested}\n",
            entry.display_id(),
            entry.title
        ));
    }
    out
}

/// Is this line an active assignment to `key`? Comments are not.
fn is_active_assignment(line: &str, key: &str) -> bool {
    let trimmed = line.trim_start();
    let Some(rest) = trimmed.strip_prefix(key) else {
        return false;
    };
    rest.starts_with('=')
}

/// The value of the first active `key=` assignment, unquoted.
pub fn grub_default_value(text: &str, key: &str) -> Option<String> {
    text.lines()
        .find(|line| is_active_assignment(line, key))
        .and_then(|line| line.split_once('=').map(|(_, v)| v))
        .map(|v| v.trim().trim_matches(['\'', '"']).trim().to_string())
}

/// Set `key='value'` in `/etc/default/grub`. Returns the new text and whether it
/// differs from the old.
///
/// **Idempotent, byte for byte.** When the key is already set to that value the text
/// comes back identical *and* `changed` is false, which matters because the caller
/// regenerates `grub.cfg` only when something changed — and "changed" has to mean the
/// file really would change.
///
/// Appends the assignment when there is none. A commented-out one is left commented:
/// `# GRUB_DISABLE_SUBMENU='false'   (disabled by snaptune…)` records that someone
/// deliberately turned something off, and rewriting it in place would erase that.
pub fn set_grub_default(text: &str, key: &str, value: &str) -> (String, bool) {
    let mut out: Vec<String> = Vec::new();
    let mut written = false;

    for line in text.lines() {
        if !written && is_active_assignment(line, key) {
            written = true;
            if grub_default_value(line, key).as_deref() == Some(value) {
                out.push(line.to_string());
            } else {
                out.push(format!("{key}='{value}'"));
            }
            continue;
        }
        out.push(line.to_string());
    }
    if !written {
        out.push(format!("{key}='{value}'"));
    }

    let mut rendered = out.join("\n");
    if text.ends_with('\n') || !written {
        rendered.push('\n');
    }
    let changed = rendered != text;
    (rendered, changed)
}

/// Comment out the active `key=` assignment, with a note saying why.
///
/// Returns the new text and whether anything changed. Already-commented lines are left
/// alone, so running it twice is the same as running it once.
pub fn comment_grub_default(text: &str, key: &str, note: &str) -> (String, bool) {
    let mut changed = false;
    let out: Vec<String> = text
        .lines()
        .map(|line| {
            if is_active_assignment(line, key) {
                changed = true;
                if note.is_empty() {
                    format!("# {line}")
                } else {
                    format!("# {line}   ({note})")
                }
            } else {
                line.to_string()
            }
        })
        .collect();

    let mut rendered = out.join("\n");
    if text.ends_with('\n') {
        rendered.push('\n');
    }
    // Defined the same way as `set_grub_default`: the text differing is the fact that
    // matters, since the caller skips regenerating `grub.cfg` when nothing changed.
    let changed = rendered != text;
    (rendered, changed)
}

/// Is the menu flattened by `/etc/default/grub`?
///
/// True for the spellings the setting is actually written in, quoted or not: the
/// shell tool's own check accepts `true`, `yes` and `y`, so a box that says `yes` must
/// not be reported as nested.
pub fn submenus_disabled(defaults: &str) -> bool {
    grub_default_value(defaults, "GRUB_DISABLE_SUBMENU").is_some_and(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "true" | "yes" | "y" | "1"
        )
    })
}

/// Is this an id `10_linux` generated for a kernel?
///
/// `10_linux` enumerates every `/boot/vmlinuz-*` with no exclusion list, so it writes
/// one `gnulinux-<name>-advanced-<uuid>` entry per installed kernel *and* one per
/// `vmlinuz-snap-*` this tool installs. Those are the entries a flattened menu promotes
/// to the top level — which is why a flattened store shows each snapshot twice, the
/// twin being the one with no usable `--id` and no `BOOT_IMAGE=`.
pub fn is_generated_id(id: Option<&str>) -> bool {
    id.is_some_and(|id| id.starts_with("gnulinux-"))
}

/// The argument `grub-reboot` should be given for `want`, or the reason it must
/// not be armed at all.
///
/// **Why this is a lookup and not a pass-through.** `grub-reboot` writes
/// `next_entry` whether or not GRUB can resolve what it was handed, and an
/// unresolvable value does not fail: GRUB boots **entry 0** instead — silently,
/// and taking the place of a correctly-saved default. Confirmed on blade 115,
/// where arming a bare nested id (a real `--id` present in the file) booted
/// "CachyOS Linux". So the entry is resolved here first and anything that cannot
/// be resolved is refused rather than written.
///
/// A nested entry is accepted only under its full `Parent>Child` path, which is
/// what GRUB's `get_entry_number()` needs; the bare id is the thing that
/// silently boots entry 0.
///
/// Refuses ambiguity rather than taking the first match: a title that appears in
/// two submenus would otherwise arm whichever happens to come first in the file,
/// which is not a coin worth flipping on a machine you may only reach over the
/// network.
pub fn resolve_boot_entry(entries: &[MenuEntry], want: &str) -> Result<String> {
    let matches: Vec<&MenuEntry> = entries
        .iter()
        .filter(|e| e.id.as_deref() == Some(want) || e.title == want || e.path() == want)
        .collect();

    let entry = match matches.as_slice() {
        [] => bail!(
            "no GRUB entry is called '{want}'.\n{}",
            entry_table(entries)
        ),
        [one] => *one,
        many => bail!(
            "'{want}' matches {} entries, so which to boot is ambiguous:\n{}\n\
             Name the one you mean by its full path.",
            many.len(),
            many.iter()
                .map(|e| format!("  {}", e.path()))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    };

    if let Some(parent) = &entry.submenu {
        if want != entry.path() {
            bail!(
                "cannot arm '{want}': it is inside the submenu '{parent}'.\n\n\
                 grub-reboot needs the submenu path, not a bare nested id. Given the bare\n\
                 id it matches nothing and boots entry 0 — silently, and it takes the place\n\
                 of a correctly-saved default.\n\n\
                 Use: snaptune boot --entry '{}'",
                entry.path()
            );
        }
    }
    Ok(want.to_string())
}

/// Rebuild a snapshot's `40_custom` block from its manifest.
///
/// Mirrors the shell tool's `grub_write_entry_from_manifest`, including the
/// header and `--id` forms that [`remove_entry`] and the shell's `sed` both key
/// on, so the two tools can edit the same file during a transition.
///
/// `BOOT_IMAGE=` is rewritten to this snapshot's own vmlinuz. The manifest's
/// `cmdline` is the **live** one captured at the time, so it names whichever
/// kernel was booted then — `snap-da794092`'s contains it twice, both pointing at
/// a different snapshot. Every occurrence is dropped and exactly one is added.
///
/// Refuses rather than guessing: a manifest missing a field the entry needs is an
/// error naming that field, because a half-written entry is not bootable and is
/// hard to notice afterwards.
pub fn entry_from_manifest(
    manifest: &Manifest,
    id: &str,
    acpi_override: Option<PathBuf>,
) -> Result<String> {
    let description = manifest.description().unwrap_or_else(|| id.to_string());
    let title = manifest
        .entry_title()
        .unwrap_or_else(|| format!("[{id}] {description}"));
    let root_uuid = manifest
        .root_uuid()
        .context("cannot rebuild the entry: manifest has no grub.root_uuid")?;
    let vmlinuz = manifest
        .vmlinuz()
        .context("cannot rebuild the entry: manifest has no boot_files.vmlinuz")?;
    let initramfs = manifest
        .initramfs()
        .context("cannot rebuild the entry: manifest has no boot_files.initramfs")?;
    let cmdline = manifest
        .cmdline()
        .context("cannot rebuild the entry: manifest has no cmdline")?;

    let rest = strip_boot_image(&cmdline);
    let initrd_prefix = match &acpi_override {
        Some(path) => format!("{} ", path.display()),
        None => String::new(),
    };

    Ok(format!(
        "\n# === {id}: {description} ===\n\
         # Restored by: snaptune\n\
         menuentry '{title}' --id '{id}' {{\n\
         \x20   insmod gzio\n\
         \x20   insmod part_gpt\n\
         \x20   insmod ext2\n\
         \x20   search --no-floppy --fs-uuid --set=root {root_uuid}\n\
         \x20   linux {vmlinuz} BOOT_IMAGE={vmlinuz}{rest}\n\
         \x20   initrd {initrd_prefix}{initramfs}\n\
         }}\n"
    ))
}

/// Drop every `BOOT_IMAGE=` token, keeping the rest in order.
///
/// A whitespace split rather than a substitution, because the defect being
/// defended against is a *duplicated* token and a regex replace would leave a
/// doubled space or a stray one at the end.
pub fn strip_boot_image(cmdline: &str) -> String {
    cmdline
        .split_whitespace()
        .filter(|word| !word.starts_with("BOOT_IMAGE="))
        .map(|word| format!(" {word}"))
        .collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CUSTOM: &str = r#"#!/bin/sh
exec tail -n +3 $0
# this line is not ours

# === snap-0848359a: imported 7.2.9 ===
# Created: 2026-10-08T20:11:00Z
# Kernel: 7.2.9-1-cachyos-bore
menuentry '[snap-0848359a] imported 7.2.9' --id 'snap-0848359a' {
    insmod gzio
    search --no-floppy --fs-uuid --set=root 8387e4a0
    linux /boot/vmlinuz-snap-0848359a BOOT_IMAGE=/boot/vmlinuz-snap-0848359a root=UUID=8387e4a0 rw
    initrd /boot/initramfs-snap-0848359a.img
}

# === snap-da794092: merge-test ===
# Restored by: snapshot unhide
menuentry '[snap-da794092] merge-test' --id 'snap-da794092' {
    linux /boot/vmlinuz-snap-da794092 root=UUID=8387e4a0 rw
    initrd /boot/initramfs-snap-da794092.img
}
"#;

    #[test]
    fn parses_both_blocks_and_ignores_the_prologue() {
        let entries = parse_custom(CUSTOM);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "snap-0848359a");
        assert_eq!(entries[0].description, "imported 7.2.9");
        assert_eq!(entries[1].id, "snap-da794092");
        assert_eq!(entries[1].comments[1], "# Restored by: snapshot unhide");
    }

    /// Parse then render must reproduce the blocks byte for byte — this is what
    /// makes the write path safe to build on.
    #[test]
    fn round_trips_byte_for_byte() {
        let entries = parse_custom(CUSTOM);
        let rendered = render_custom(&entries);
        for entry in &entries {
            assert!(
                CUSTOM.contains(entry.render().trim_end()),
                "rendered block not found verbatim: {}",
                entry.id
            );
        }
        assert!(rendered.contains("initrd /boot/initramfs-snap-da794092.img"));
    }

    /// Removing one block leaves the other and the prologue untouched.
    #[test]
    fn remove_takes_only_the_named_block() {
        let (after, removed) = remove_entry(CUSTOM, "snap-0848359a");
        assert!(removed);
        assert!(!after.contains("snap-0848359a"));
        assert!(after.contains("snap-da794092"));
        assert!(after.contains("exec tail -n +3 $0"));
        assert!(after.contains("# this line is not ours"));
    }

    #[test]
    fn remove_of_an_absent_id_changes_nothing() {
        let (after, removed) = remove_entry(CUSTOM, "snap-deadbeef");
        assert!(!removed);
        assert_eq!(after, CUSTOM);
    }

    /// The `if … fi` case: a firmware entry is indented but NOT nested.
    #[test]
    fn indentation_inside_if_blocks_is_not_nesting() {
        let cfg = r#"menuentry 'CachyOS Linux' --id 'top' {
    linux /vmlinuz
}
if [ "$grub_platform" = "efi" ]; then
	menuentry 'UEFI Firmware Settings' --id 'uefi-firmware' {
		fwsetup
	}
fi
submenu 'Advanced options' --id 'advanced' {
	menuentry 'with Linux linux-cachyos' --id 'nested-one' {
		linux /vmlinuz
	}
}
"#;
        let entries = parse_menu_entries(cfg);
        // Three menuentries: the top-level one, the firmware one inside if/fi,
        // and the one inside the submenu. The submenu itself is not an entry.
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].submenu, None);
        assert_eq!(entries[1].title, "UEFI Firmware Settings");
        assert_eq!(
            entries[1].submenu, None,
            "a firmware entry inside if/fi is top level, not nested"
        );
        assert_eq!(entries[2].submenu.as_deref(), Some("Advanced options"));
        assert_eq!(
            entries[2].path(),
            "Advanced options>with Linux linux-cachyos"
        );
    }

    /// A slice of a real `/etc/default/grub`: comments, an unrelated key, and a
    /// commented-out assignment that must stay commented.
    const DEFAULTS: &str = r#"# GRUB boot loader configuration
GRUB_DEFAULT=saved
GRUB_TIMEOUT=5
#GRUB_DISABLE_SUBMENU='false'
GRUB_THEME="/usr/share/grub/themes/cachyos/theme.txt"
"#;

    #[test]
    fn grub_defaults_are_read_unquoted_and_only_when_active() {
        assert_eq!(
            grub_default_value(DEFAULTS, "GRUB_DEFAULT").as_deref(),
            Some("saved")
        );
        assert_eq!(
            grub_default_value(DEFAULTS, "GRUB_THEME").as_deref(),
            Some("/usr/share/grub/themes/cachyos/theme.txt")
        );
        assert!(
            grub_default_value(DEFAULTS, "GRUB_DISABLE_SUBMENU").is_none(),
            "a commented-out assignment is not the value: reading it would make \
             flatten think it had already run"
        );
        assert!(grub_default_value(DEFAULTS, "GRUB_TIMEOUT_STYLE").is_none());
    }

    #[test]
    fn flattening_sets_the_key_and_leaves_every_other_line_alone() {
        let (out, changed) = set_grub_default(DEFAULTS, "GRUB_DISABLE_SUBMENU", "true");
        assert!(changed, "there was no active assignment to leave alone");
        assert!(out.contains("GRUB_DISABLE_SUBMENU='true'\n"));
        assert!(
            out.contains("GRUB_DEFAULT=saved"),
            "unrelated keys untouched"
        );
        assert!(
            out.contains("#GRUB_DISABLE_SUBMENU='false'"),
            "the commented one stays commented: it records that someone turned it off"
        );
        assert_eq!(
            out.lines().count(),
            DEFAULTS.lines().count() + 1,
            "appended, not inserted"
        );
    }

    #[test]
    fn flattening_replaces_an_active_key_rather_than_appending_a_second_one() {
        let text = "GRUB_DEFAULT=saved\nGRUB_DISABLE_SUBMENU='false'\n";
        let (out, changed) = set_grub_default(text, "GRUB_DISABLE_SUBMENU", "true");
        assert!(changed);
        assert_eq!(out, "GRUB_DEFAULT=saved\nGRUB_DISABLE_SUBMENU='true'\n");
        assert_eq!(out.matches("GRUB_DISABLE_SUBMENU").count(), 1);
    }

    /// The caller decides whether to regenerate `grub.cfg` from `changed`, so
    /// "changed" has to mean the file really would change — otherwise every run
    /// regenerates and rewrites a bootloader config for nothing.
    #[test]
    fn flattening_what_is_already_flat_changes_nothing_at_all() {
        let text = "GRUB_DEFAULT=saved\nGRUB_DISABLE_SUBMENU='true'\n";
        let (out, changed) = set_grub_default(text, "GRUB_DISABLE_SUBMENU", "true");
        assert!(!changed, "already flattened");
        assert_eq!(out, text, "and byte-identical, not merely equivalent");
    }

    /// The setting is written by hand as often as by a tool, and the shell tool's own
    /// check accepts three spellings. Reading only `true` would report a correctly
    /// nested box as flattened.
    #[test]
    fn every_spelling_of_the_flatten_setting_is_recognised() {
        for flat in [
            "GRUB_DISABLE_SUBMENU='true'\n",
            "GRUB_DISABLE_SUBMENU=true\n",
            "GRUB_DISABLE_SUBMENU=\"yes\"\n",
            "GRUB_DISABLE_SUBMENU=y\n",
            "GRUB_DISABLE_SUBMENU='TRUE'\n",
        ] {
            assert!(submenus_disabled(flat), "{flat:?} means flat");
        }
        for nested in [
            "GRUB_DISABLE_SUBMENU='false'\n",
            "GRUB_DISABLE_SUBMENU=false\n",
            "GRUB_DISABLE_SUBMENU='no'\n",
            "",
            "#GRUB_DISABLE_SUBMENU='true'\n",
        ] {
            assert!(!submenus_disabled(nested), "{nested:?} does not mean flat");
        }
    }

    /// The twins a flattened menu promotes are exactly `10_linux`'s, which is what
    /// makes the count a measurement rather than a guess.
    #[test]
    fn only_ten_linux_generated_ids_count_as_duplicates() {
        assert!(is_generated_id(Some(
            "gnulinux-snap-11e61755-advanced-757f018a-71b0"
        )));
        assert!(is_generated_id(Some(
            "gnulinux-linux-cachyos-bore-advanced-a"
        )));
        for ours in [Some("snap-11e61755"), Some("uefi-firmware"), None] {
            assert!(!is_generated_id(ours), "{ours:?} is not a generated twin");
        }
    }

    #[test]
    fn commenting_the_theme_records_why_and_is_idempotent() {
        let (out, changed) = comment_grub_default(DEFAULTS, "GRUB_THEME", "because rows");
        assert!(changed);
        assert!(
            out.contains(
                "# GRUB_THEME=\"/usr/share/grub/themes/cachyos/theme.txt\"   (because rows)\n"
            ),
            "{out}"
        );
        assert!(out.contains("GRUB_DEFAULT=saved"), "other keys untouched");
        assert!(
            grub_default_value(&out, "GRUB_THEME").is_none(),
            "now inactive"
        );

        let (again, changed) = comment_grub_default(&out, "GRUB_THEME", "because rows");
        assert!(!changed, "running it twice is the same as running it once");
        assert_eq!(again, out);
    }

    #[test]
    fn commenting_a_key_that_is_not_there_changes_nothing() {
        let (out, changed) = comment_grub_default(DEFAULTS, "GRUB_THEME_EXTRA", "note");
        assert!(!changed);
        assert_eq!(out, DEFAULTS);
    }

    /// The fixture the `resolve_boot_entry` tests read: one top-level entry with an
    /// id, one submenu holding a real nested entry, and one title-only entry.
    const CFG: &str = r#"menuentry 'CachyOS Linux' --id 'cachyos' {
	linux /vmlinuz-linux-cachyos
}
submenu 'Advanced options for CachyOS' --id 'advanced' {
	menuentry 'CachyOS Linux, with Linux linux-cachyos-lts' --id 'gnulinux-lts-advanced-3f2a' {
		linux /vmlinuz-linux-cachyos-lts
	}
}
menuentry 'Memory Test (memtest86+)' {
	tar
}
"#;

    #[test]
    fn a_top_level_entry_resolves_by_id_and_by_title() {
        let entries = parse_menu_entries(CFG);
        assert_eq!(
            resolve_boot_entry(&entries, "cachyos").unwrap(),
            "cachyos",
            "the --id is what grub-reboot is usually given"
        );
        assert_eq!(
            resolve_boot_entry(&entries, "CachyOS Linux").unwrap(),
            "CachyOS Linux",
            "the exact title is the other thing grub-reboot accepts"
        );
        assert_eq!(
            resolve_boot_entry(&entries, "Memory Test (memtest86+)").unwrap(),
            "Memory Test (memtest86+)",
            "an entry with no --id is still reachable by title"
        );
    }

    /// The refusal this exists for: a real nested `--id`, present in the file, that
    /// grub-reboot cannot resolve. Arming it boots entry 0 instead, silently, and
    /// takes the place of a correctly-saved default.
    #[test]
    fn a_bare_nested_id_is_refused_with_the_path_to_use_instead() {
        let entries = parse_menu_entries(CFG);
        let why = resolve_boot_entry(&entries, "gnulinux-lts-advanced-3f2a")
            .expect_err("the bare nested id must not be armed");
        assert!(
            why.to_string()
                .contains("inside the submenu 'Advanced options"),
            "the refusal must name the submenu: {why}"
        );
        assert!(
            why.to_string().contains(
                "Advanced options for CachyOS>CachyOS Linux, with Linux linux-cachyos-lts"
            ),
            "and give the exact path to use instead: {why}"
        );
    }

    #[test]
    fn a_nested_entry_resolves_when_named_by_its_full_path() {
        let entries = parse_menu_entries(CFG);
        let path = "Advanced options for CachyOS>CachyOS Linux, with Linux linux-cachyos-lts";
        assert_eq!(
            resolve_boot_entry(&entries, path).unwrap(),
            path,
            "the path is what get_entry_number() needs, so it is the way through"
        );
    }

    #[test]
    fn a_name_that_matches_nothing_is_refused_before_anything_is_written() {
        let entries = parse_menu_entries(CFG);
        let why = resolve_boot_entry(&entries, "linux-lts")
            .expect_err("a substring must not count as a match");
        let text = why.to_string();
        assert!(
            text.contains("no GRUB entry is called 'linux-lts'"),
            "{text}"
        );
        assert!(
            text.contains("cachyos"),
            "and it should list what there is: {text}"
        );
        assert!(text.contains("[nested]"), "marking the nested ones: {text}");
    }

    /// Two entries with the same title in different submenus: taking the first
    /// would arm whichever happens to come first in the file.
    #[test]
    fn an_ambiguous_title_is_refused_rather_than_guessed() {
        let cfg = r#"submenu 'A' {
	menuentry 'Same Title' --id 'a-same' { linux /vmlinuz }
}
submenu 'B' {
	menuentry 'Same Title' --id 'b-same' { linux /vmlinuz }
}
"#;
        let entries = parse_menu_entries(cfg);
        let why = resolve_boot_entry(&entries, "Same Title").expect_err("ambiguous");
        let text = why.to_string();
        assert!(text.contains("ambiguous"), "{text}");
        assert!(
            text.contains("A>Same Title") && text.contains("B>Same Title"),
            "{text}"
        );

        // ...and the path resolves it.
        assert_eq!(
            resolve_boot_entry(&entries, "B>Same Title").unwrap(),
            "B>Same Title"
        );
    }

    /// `10_linux` puts the id in a variable rather than after a literal `--id`, so a
    /// reader that only knows `--id` shows a title where an id exists.
    #[test]
    fn the_menuentry_id_option_variable_form_is_read_too() {
        let cfg = r#"menuentry_id_option="--id gnulinux-simple-8387e4a0"
menuentry 'CachyOS Linux, with Linux linux-cachyos-bore' --class cachyos --class gnu-linux $menuentry_id_option 'gnulinux-linux-cachyos-bore-advanced-757f018a' {
	linux /vmlinuz-linux-cachyos-bore
}
menuentry 'ours' --id 'snap-11e61755' {
	linux /vmlinuz-snap-11e61755
}
"#;
        let entries = parse_menu_entries(cfg);
        assert_eq!(
            entries.len(),
            2,
            "the assignment line is still not an entry: {entries:?}"
        );
        assert_eq!(
            entries[0].id.as_deref(),
            Some("gnulinux-linux-cachyos-bore-advanced-757f018a"),
            "the variable form carries a real id, and grub-reboot resolves it"
        );
        assert_eq!(
            entries[0].display_id(),
            "gnulinux-linux-cachyos-bore-advanced-757f018a"
        );
        assert_eq!(
            entries[1].id.as_deref(),
            Some("snap-11e61755"),
            "the literal form still works"
        );
    }

    /// Every id `grub-list` prints must be one `boot --entry` will accept, or the
    /// listing sends people to a command that refuses what it just showed them.
    #[test]
    fn an_id_from_the_variable_form_can_be_armed() {
        let cfg = "menuentry 'CachyOS Linux' $menuentry_id_option 'gnulinux-linux-cachyos-advanced-abc' {\n}\n";
        let entries = parse_menu_entries(cfg);
        assert_eq!(
            resolve_boot_entry(&entries, "gnulinux-linux-cachyos-advanced-abc").unwrap(),
            "gnulinux-linux-cachyos-advanced-abc"
        );
    }

    /// `menuentry_id_option="--id …"` is an assignment, not an entry.
    #[test]
    fn id_option_assignment_lines_are_not_entries() {
        let cfg = "menuentry_id_option=\"--id gnulinux-simple-8387e4a0\"\n";
        assert!(parse_menu_entries(cfg).is_empty());
    }

    #[test]
    fn nested_entries_expose_the_path_grub_needs() {
        let cfg = "submenu 'Advanced options for CachyOS Linux' {\n\tmenuentry 'CachyOS Linux, with Linux linux-cachyos' --id 'x-advanced-y' {\n\t}\n}\n";
        let entries = parse_menu_entries(cfg);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].path(),
            "Advanced options for CachyOS Linux>CachyOS Linux, with Linux linux-cachyos"
        );
        assert_eq!(entries[0].display_id(), "x-advanced-y");
    }

    // ---- entry rendering ---------------------------------------------------

    fn manifest(id: &str, cmdline: &str) -> Manifest {
        Manifest::parse(&format!(
            "snapshot:\n  id: \"{id}\"\n  description: \"a test snapshot\"\ngrub:\n  entry_title: \"[{id}] a test snapshot\"\n  root_uuid: \"8387e4a0-b18f-4142-b61d-59fa9acc3865\"\nboot_files:\n  vmlinuz: \"/boot/vmlinuz-{id}\"\n  initramfs: \"/boot/initramfs-{id}.img\"\ncmdline: \"{cmdline}\"\n"
        ))
        .expect("parse")
    }

    /// The block we write must be one our own parser removes — that is the
    /// interop contract, and the shell tool matches the same header line.
    #[test]
    fn entry_round_trips_through_the_parser() {
        let m = manifest(
            "snap-abc12345",
            "BOOT_IMAGE=/boot/vmlinuz-other root=UUID=x rw",
        );
        let entry = entry_from_manifest(&m, "snap-abc12345", None).expect("renders");

        assert!(entry.contains("# === snap-abc12345: a test snapshot ==="));
        assert!(entry.contains("--id 'snap-abc12345'"));

        let parsed = parse_custom(&entry);
        assert_eq!(parsed.len(), 1, "exactly one block: {entry}");
        assert_eq!(parsed[0].id, "snap-abc12345");

        // ...and removing it from a file that contains it works.
        let file = format!("#!/bin/sh\nexec tail -n +3 $0\n{entry}");
        let (after, removed) = remove_entry(&file, "snap-abc12345");
        assert!(removed);
        assert!(!after.contains("snap-abc12345"));
        assert!(after.contains("exec tail -n +3 $0"));
    }

    /// The recorded cmdline names whichever kernel was booted at capture time, so
    /// the rebuilt entry must carry this snapshot's own vmlinuz — exactly once.
    #[test]
    fn entry_rewrites_boot_image_to_the_snapshots_own_vmlinuz() {
        let m = manifest(
            "snap-abc12345",
            "BOOT_IMAGE=/boot/vmlinuz-snap-2d2b1734 BOOT_IMAGE=/boot/vmlinuz-snap-2d2b1734 root=UUID=8387e4a0 rw",
        );
        let entry = entry_from_manifest(&m, "snap-abc12345", None).expect("renders");
        assert_eq!(entry.matches("BOOT_IMAGE=").count(), 1);
        assert!(entry.contains("BOOT_IMAGE=/boot/vmlinuz-snap-abc12345"));
        assert!(!entry.contains("vmlinuz-snap-2d2b1734"));
        assert!(entry.contains(" root=UUID=8387e4a0 rw\n"));
    }

    #[test]
    fn entry_chains_the_acpi_override_when_present() {
        let m = manifest("snap-abc12345", "root=UUID=x rw");
        let entry = entry_from_manifest(
            &m,
            "snap-abc12345",
            Some(PathBuf::from("/boot/acpi_override.cpio")),
        )
        .expect("renders");
        assert!(entry.contains("initrd /boot/acpi_override.cpio /boot/initramfs-snap-abc12345.img"));
    }

    #[test]
    fn entry_refuses_a_manifest_missing_a_field() {
        let bare = Manifest::parse("snapshot:\n  id: \"snap-x\"\n").expect("parses");
        let err = entry_from_manifest(&bare, "snap-x", None).unwrap_err();
        assert!(err.to_string().contains("root_uuid"), "{err}");
    }

    #[test]
    fn strip_boot_image_drops_every_occurrence() {
        assert_eq!(
            strip_boot_image("BOOT_IMAGE=/a BOOT_IMAGE=/a root=UUID=x rw"),
            " root=UUID=x rw"
        );
        assert_eq!(strip_boot_image("root=UUID=x"), " root=UUID=x");
        assert_eq!(strip_boot_image(""), "");
    }
}
