// SPDX-License-Identifier: GPL-2.0-only
//! The command surface. Every verb here is **read-only**.
//!
//! `list`/`show`/`current`/`lineage`/`grub-list` mirror the shell tool's verbs of
//! the same name, and `list --ids` is kept byte-compatible because
//! `apu::initramfs` parses it (`snapshot list --ids`) to answer "is this
//! snapshot one of ours?". Until that call site is replaced, that output is a
//! contract.
//!
//! `--json` is new, and is the reason agents can stop scraping the text table.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Serialize;

use crate::grub;
use crate::store::{Paths, Snapshot, Store};

#[derive(Parser, Debug)]
#[command(
    name = "snaptune",
    version,
    about = "BC-250 snapshots: what is on this blade, and which one did I boot?",
    long_about = "Read-only view of the bc-250-snapshotter store.\n\
        \n\
        Lists snapshots and their boot artefacts, reports which snapshot the running kernel\n\
        came from, walks the parent lineage, and shows GRUB menu entries with their nesting.\n\
        \n\
        This build cannot modify anything. Snapshot creation, pinning and removal still belong\n\
        to the shell tool until this agrees with it on every blade."
)]
pub struct Cli {
    /// Read a snapshot store from this root instead of /var/lib/kernel-snapshots.
    /// Boot, GRUB and cmdline paths are taken from under it too, so a fixture is
    /// readable end to end.
    #[arg(long, global = true, env = "SNAPSHOT_ROOT")]
    pub root: Option<PathBuf>,

    /// Emit JSON instead of a text table.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// List snapshots, newest first. Hidden ones are excluded, as the shell tool
    /// does; pass --all to include them.
    List {
        /// Print ids only, one per line. Machine-readable; kept for compatibility.
        #[arg(long)]
        ids: bool,
        /// Include snapshots hidden with `snapshot hide`.
        #[arg(long)]
        all: bool,
    },

    /// Show one snapshot in detail.
    Show {
        /// Snapshot id, e.g. snap-0848359a.
        id: String,
    },

    /// Report the snapshot this boot came from.
    Current,

    /// Follow snapshot.parent from the booted snapshot.
    Lineage,

    /// List GRUB menu entries, marking the nested ones.
    #[command(name = "grub-list", alias = "grublist", alias = "grub")]
    GrubList,
}

impl Cli {
    fn paths(&self) -> Paths {
        match &self.root {
            Some(root) => Paths::under(root),
            None => Paths::system(),
        }
    }

    fn store(&self) -> Store {
        Store::new(self.paths())
    }
}

/// A snapshot flattened for output. One struct for both formats so the JSON and
/// the table can never disagree.
#[derive(Debug, Serialize)]
struct Row {
    id: String,
    date: Option<String>,
    status: Option<String>,
    description: String,
    kernel: Option<String>,
    hidden: bool,
    golden: bool,
    booted: bool,
    grub_entry: bool,
    boot_files_complete: bool,
    /// Present only when a manifest exists but could not be parsed.
    #[serde(skip_serializing_if = "Option::is_none")]
    manifest_error: Option<String>,
}

impl Row {
    fn of(snapshot: &Snapshot) -> Row {
        Row {
            id: snapshot.id.clone(),
            date: snapshot.date(),
            status: snapshot.status(),
            description: snapshot.description(),
            kernel: snapshot.kernel_version(),
            hidden: snapshot.hidden,
            golden: snapshot.golden,
            booted: snapshot.booted,
            grub_entry: snapshot.grub_entry,
            boot_files_complete: snapshot.boot_artifacts_complete(),
            manifest_error: snapshot.manifest_error.clone(),
        }
    }

    /// `▶` booted, `★` golden, `·` hidden — the shell tool's vocabulary.
    fn markers(&self) -> String {
        let mut s = String::new();
        s.push(if self.booted { '▶' } else { ' ' });
        s.push(if self.golden { '★' } else { ' ' });
        s
    }
}

/// Run a command. Returns a process exit code so callers can shell out to this
/// the way they shell out to `snapshot`.
pub fn run(cli: Cli) -> Result<i32> {
    let store = cli.store();
    match cli.cmd {
        Cmd::List { ids, all } => cmd_list(&store, ids, all, cli.json),
        Cmd::Show { id } => cmd_show(&store, &id, cli.json),
        Cmd::Current => cmd_current(&store, cli.json),
        Cmd::Lineage => cmd_lineage(&store, cli.json),
        Cmd::GrubList => cmd_grub_list(&store, cli.json),
    }
}

fn cmd_list(store: &Store, ids_only: bool, all: bool, json: bool) -> Result<i32> {
    let snapshots = if all {
        store.discover()?
    } else {
        store.visible()?
    };

    if ids_only {
        // One id per line, nothing else — arieltune parses this.
        for snapshot in &snapshots {
            println!("{}", snapshot.id);
        }
        return Ok(0);
    }

    let rows: Vec<Row> = snapshots.iter().map(Row::of).collect();

    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(0);
    }

    if rows.is_empty() {
        println!("No snapshots in {}", store.paths.root.display());
        return Ok(0);
    }

    let width = rows.iter().map(|r| r.id.len()).max().unwrap_or(12).max(12);
    println!(
        "{:<2} {:<width$}  {:<10}  {:<11}  DESCRIPTION",
        "",
        "SNAPSHOT",
        "DATE",
        "STATUS",
        width = width
    );
    for row in &rows {
        println!(
            "{:<2} {:<width$}  {:<10}  {:<11}  {}{}",
            row.markers(),
            row.id,
            row.date.clone().unwrap_or_else(|| "-".to_string()),
            row.status.clone().unwrap_or_else(|| "-".to_string()),
            row.description,
            if row.hidden { "  (hidden)" } else { "" },
            width = width
        );
    }

    // Things the table cannot show but that matter: a manifest that exists and
    // will not parse, and boot artefacts that are missing.
    let unparseable: Vec<&Row> = rows.iter().filter(|r| r.manifest_error.is_some()).collect();
    if !unparseable.is_empty() {
        println!();
        println!(
            "{} snapshot(s) with an unreadable manifest:",
            unparseable.len()
        );
        for row in unparseable {
            println!(
                "  {} — {}",
                row.id,
                row.manifest_error.as_deref().unwrap_or("")
            );
        }
    }

    // A snapshot missing any boot artefact is worth naming, because it is
    // indistinguishable from a healthy one in the table above.
    let broken: Vec<&Row> = rows.iter().filter(|r| !r.boot_files_complete).collect();
    if !broken.is_empty() {
        println!();
        println!("{} snapshot(s) missing boot artefacts:", broken.len());
        for row in broken {
            println!(
                "  {} — check /boot/vmlinuz-{}, initramfs and .cmdline",
                row.id, row.id
            );
        }
    }

    Ok(0)
}

fn cmd_show(store: &Store, id: &str, json: bool) -> Result<i32> {
    let Some(snapshot) = store.get(id)? else {
        eprintln!("snapshot {id} not found in {}", store.paths.root.display());
        return Ok(1);
    };
    let row = Row::of(&snapshot);

    if json {
        println!("{}", serde_json::to_string_pretty(&row)?);
        return Ok(0);
    }

    let manifest = snapshot.manifest.as_ref();
    println!("Snapshot      : {}", row.id);
    println!("Description   : {}", row.description);
    println!(
        "Date          : {}",
        row.date.as_deref().unwrap_or("unknown")
    );
    println!(
        "Status        : {}",
        row.status.as_deref().unwrap_or("unknown")
    );
    if let Some(kernel) = &row.kernel {
        println!("Kernel        : {kernel}");
    }
    if let Some(parent) = manifest.and_then(|m| m.parent()) {
        println!("Parent        : {parent}");
    }
    if let Some(mode) = manifest.and_then(|m| m.source_tree_mode()) {
        println!("Source tree   : {mode}");
    }
    if manifest.is_some_and(|m| m.registered()) {
        println!("Registered    : yes (imported, no build needed)");
    }
    println!(
        "Hidden        : {}",
        if snapshot.hidden { "yes" } else { "no" }
    );
    println!(
        "Golden        : {}",
        if snapshot.golden { "yes" } else { "no" }
    );
    println!(
        "Booted        : {}",
        if snapshot.booted { "yes" } else { "no" }
    );

    if let Some(err) = &snapshot.manifest_error {
        println!();
        println!("MANIFEST UNREADABLE: {err}");
    }

    println!();
    println!("Boot artefacts:");
    for (label, present) in [
        ("vmlinuz", snapshot.vmlinuz),
        ("initramfs", snapshot.initramfs),
        ("cmdline", snapshot.cmdline_file),
        ("GRUB entry", snapshot.grub_entry),
    ] {
        println!("  {:<11} {}", label, if present { "OK" } else { "MISSING" });
    }

    if let Some(manifest) = manifest {
        if let Some(cmdline) = manifest.cmdline() {
            println!();
            println!("Recorded cmdline (NOT safe to boot with — BOOT_IMAGE= is rewritten on use):");
            println!("  {cmdline}");
        }
    }

    Ok(0)
}

fn cmd_current(store: &Store, json: bool) -> Result<i32> {
    match store.booted_id() {
        Some(id) => {
            if json {
                let row = store.get(&id)?.as_ref().map(Row::of);
                println!("{}", serde_json::to_string_pretty(&row)?);
            } else {
                println!("Booted snapshot: {id}");
                if let Some(snapshot) = store.get(&id)? {
                    println!("Description    : {}", snapshot.description());
                    if let Some(kernel) = snapshot.kernel_version() {
                        println!("Kernel         : {kernel}");
                    }
                }
            }
            Ok(0)
        }
        None => {
            if json {
                println!("null");
            } else {
                println!("Booted from a distro kernel, not a snapshot.");
                println!(
                    "(no BOOT_IMAGE=vmlinuz-snap-* in {})",
                    store.paths.cmdline.display()
                );
            }
            Ok(0)
        }
    }
}

fn cmd_lineage(store: &Store, json: bool) -> Result<i32> {
    let Some(start) = store.booted_id() else {
        if json {
            println!("[]");
        } else {
            println!("Booted from a distro kernel — no snapshot lineage to walk.");
        }
        return Ok(0);
    };

    let snapshots = store.discover()?;
    let mut chain: Vec<Row> = Vec::new();
    let mut current = Some(start);
    let mut seen: Vec<String> = Vec::new();

    while let Some(id) = current {
        // A cycle would otherwise hang the tool on a corrupted store.
        if seen.contains(&id) {
            break;
        }
        seen.push(id.clone());
        let Some(snapshot) = snapshots.iter().find(|s| s.id == id) else {
            break;
        };
        chain.push(Row::of(snapshot));
        current = snapshot.manifest.as_ref().and_then(|m| m.parent());
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&chain)?);
        return Ok(0);
    }

    for (depth, row) in chain.iter().enumerate() {
        println!(
            "{}{}  {}  {}",
            "  ".repeat(depth),
            row.id,
            row.date.as_deref().unwrap_or("-"),
            row.description
        );
    }
    Ok(0)
}

fn cmd_grub_list(store: &Store, json: bool) -> Result<i32> {
    let text = std::fs::read_to_string(&store.paths.grub_cfg).with_context(|| {
        format!(
            "reading {} — grub.cfg is only readable by root",
            store.paths.grub_cfg.display()
        )
    })?;
    let entries = grub::parse_menu_entries(&text);

    if json {
        let payload: Vec<_> = entries
            .iter()
            .map(|e| {
                serde_json::json!({
                    "id": e.id,
                    "title": e.title,
                    "submenu": e.submenu,
                    "nested": e.submenu.is_some(),
                    "path": e.path(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(0);
    }

    println!("GRUB menu entries in {}:", store.paths.grub_cfg.display());
    println!();
    for entry in &entries {
        println!(
            "  {:<52} {}{}",
            entry.display_id(),
            entry.title,
            if entry.submenu.is_some() {
                "  [nested]"
            } else {
                ""
            }
        );
    }

    let nested = entries.iter().filter(|e| e.submenu.is_some()).count();
    if nested > 0 {
        println!();
        println!(
            "{nested} entr{} nested. grub-reboot cannot resolve a bare nested id — it boots",
            if nested == 1 { "y is" } else { "ies are" }
        );
        println!("entry 0 instead, overriding saved_entry. Name the path:");
        for entry in entries.iter().filter(|e| e.submenu.is_some()).take(3) {
            println!("  {}", entry.path());
        }
    }

    Ok(0)
}
