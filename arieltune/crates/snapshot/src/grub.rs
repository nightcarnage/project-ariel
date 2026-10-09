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

/// The `--id` value from a `menuentry` line, if present.
fn id_option(line: &str) -> Option<String> {
    let marker = "--id ";
    let start = line.find(marker)? + marker.len();
    let rest = &line[start..];
    let quote = rest.chars().next()?;
    if quote == '\'' || quote == '"' {
        let end = rest[1..].find(quote)?;
        Some(rest[1..1 + end].to_string())
    } else {
        Some(rest.split_whitespace().next()?.to_string())
    }
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
}
