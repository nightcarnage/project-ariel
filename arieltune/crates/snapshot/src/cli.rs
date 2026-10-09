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

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Serialize;

use crate::dev;
use crate::grub;
use crate::store::{human_bytes, Paths, Snapshot, Store};
use crate::write;

#[derive(Parser, Debug)]
#[command(
    name = "snaptune",
    version,
    about = "BC-250 snapshots: what is on this blade, and which one did I boot?",
    long_about = "Snapshot store: list, inspect, and manage the GRUB menu entries of\n\
        kernel snapshots created by the bc-250-snapshotter.\n\
        \n\
        Reads are safe anywhere. The four writers -- pin, unpin, hide, unhide -- edit\n\
        /etc/grub.d/40_custom and grubenv and so need root, and each takes --dry-run to\n\
        print exactly what it would do first.\n\
        \n\
        NOT IMPLEMENTED YET: create, register, rebuild, prune, boot, verify. Those still\n\
        belong to the shell tool, which remains authoritative."
)]
pub struct Cli {
    #[command(flatten)]
    pub global: Global,

    #[command(subcommand)]
    pub cmd: Cmd,
}

/// Options every verb shares.
///
/// Flattened into [`Cli`] for the standalone `snaptune`, and into the suite's
/// `arieltune snapshot` namespace too — the same arrangement `bios::Global` uses,
/// so the two entry points cannot drift apart.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct Global {
    /// Read a snapshot store from this root instead of /var/lib/kernel-snapshots.
    /// Boot, GRUB and cmdline paths are taken from under it too, so a fixture is
    /// readable end to end.
    #[arg(long, global = true, env = "SNAPSHOT_ROOT")]
    pub root: Option<PathBuf>,

    /// Emit JSON instead of a text table.
    #[arg(long, global = true)]
    pub json: bool,
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

    /// Compare two snapshots: which manifest fields differ, and how the boot
    /// artefacts differ. Read-only.
    ///
    /// The question this answers is "what changed between the snapshot I trust and
    /// the one I am about to boot", which the shell tool's `diff` answered from the
    /// source tree. Doing it from the manifests means it works on a blade with no
    /// tree at all.
    Diff {
        /// Snapshot id, e.g. snap-0848359a.
        a: String,
        /// Snapshot id to compare against.
        b: String,
    },

    /// Show the golden image, or mark one with `gold` — the snapshot hide and
    /// prune refuse to remove. Read-only without an id.
    ///
    /// Naming an id writes the store's marker file (needs root). It does not touch
    /// GRUB or /boot: the golden image is a protection, not a boot configuration.
    Golden {
        /// Snapshot id to mark as golden. Omit to just report the current one.
        id: Option<String>,
        /// Print the plan and change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Plan a dev snapshot: rebuild only the modules the tree has actually
    /// changed, into a snapshot of its own. Read-only so far.
    ///
    /// The dev loop does not need a kernel build, only the amdgpu module relinked
    /// (`make M=... modules`, seconds). The build set comes from `git status`, not
    /// from the patches' `touches` field, because the question is what the tree
    /// differs in right now — after hand-edits, half-reverted hunks, or a file the
    /// patch author never listed.
    ///
    /// This verb currently REPORTS the plan and writes nothing: the build and the
    /// snapshot assembly land next. It refuses rather than pretending when the
    /// tree builds a different kernel from the one running, or when every change
    /// is outside drivers/ and a module rebuild would capture none of it.
    Dev {
        /// Description of this iteration, e.g. "patch 25 variant".
        description: String,
        /// Kernel source tree (or set SNAPSHOT_TREE).
        #[arg(long, value_name = "DIR", env = "SNAPSHOT_TREE")]
        tree: Option<PathBuf>,
    },
    /// Pin a snapshot as the permanent GRUB default. Writes grubenv; needs root.
    Pin {
        /// Snapshot id, e.g. snap-0848359a.
        id: String,
        /// Print the plan and change nothing.
        #[arg(long)]
        dry_run: bool,
    },

    /// Revert the GRUB default to entry 0 (the distro kernel). Needs root.
    Unpin {
        /// Print the plan and change nothing.
        #[arg(long)]
        dry_run: bool,
    },

    /// Take a snapshot off the list AND off the GRUB menu. Data is preserved.
    Hide {
        /// Snapshot id to hide.
        id: String,
        /// Print the plan and change nothing.
        #[arg(long)]
        dry_run: bool,
    },

    /// Restore a hidden snapshot, rebuilding its GRUB entry from the manifest.
    Unhide {
        /// Snapshot id to restore.
        id: String,
        /// Print the plan and change nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

impl Global {
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

/// Run a command from the standalone binary. Returns a process exit code so
/// callers can shell out to this the way they shell out to `snapshot`.
pub fn run(cli: Cli) -> Result<i32> {
    run_with(cli.global, cli.cmd)
}

/// Run a command from a namespaced entry point (`arieltune snapshot …`), which
/// owns its own flattened [`Global`].
pub fn run_with(global: Global, cmd: Cmd) -> Result<i32> {
    let store = global.store();
    let json = global.json;
    match cmd {
        Cmd::List { ids, all } => cmd_list(&store, ids, all, json),
        Cmd::Show { id } => cmd_show(&store, &id, json),
        Cmd::Current => cmd_current(&store, json),
        Cmd::Lineage => cmd_lineage(&store, json),
        Cmd::GrubList => cmd_grub_list(&store, json),
        Cmd::Diff { a, b } => cmd_diff(&store, &a, &b, json),
        Cmd::Golden { id, dry_run } => cmd_golden(&store.paths, id.as_deref(), dry_run, json),
        Cmd::Dev { description, tree } => cmd_dev(&description, tree.as_deref(), json),
        Cmd::Pin { id, dry_run } => cmd_pin(&store.paths, &id, dry_run),
        Cmd::Unpin { dry_run } => cmd_unpin(&store.paths, dry_run),
        Cmd::Hide { id, dry_run } => cmd_hide(&store.paths, &id, dry_run),
        Cmd::Unhide { id, dry_run } => cmd_unhide(&store.paths, &id, dry_run),
    }
}

/// The writers touch /etc/grub.d, /boot/grub and grubenv. Say so up front rather
/// than letting the first write fail with a bare EACCES.
fn require_root() -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        anyhow::bail!(
            "this verb changes the boot configuration and needs root.\n\
             Re-run with sudo (or use --dry-run, which only prints the plan)."
        );
    }
    Ok(())
}

fn cmd_pin(paths: &Paths, id: &str, dry_run: bool) -> Result<i32> {
    // The plan refuses anything unsafe BEFORE a single byte is written.
    let state = write::State::read(paths)?;
    let plan = write::plan_pin(paths, &state, id)?;
    if !dry_run {
        require_root()?;
    }
    write::apply(&plan, dry_run)?;
    if !dry_run {
        write::verify_pin(id)?;
        println!("\nPinned: {id}");
    }
    Ok(0)
}

fn cmd_unpin(paths: &Paths, dry_run: bool) -> Result<i32> {
    let plan = write::plan_unpin(paths)?;
    if !dry_run {
        require_root()?;
    }
    write::apply(&plan, dry_run)?;
    if !dry_run {
        let now = write::saved_entry().unwrap_or_else(|| "<unset>".to_string());
        if now != "0" {
            eprintln!("warning: saved_entry is '{now}' after unpin, expected '0'");
        }
        println!("\nUnpinned: the GRUB default is entry 0 again.");
    }
    Ok(0)
}

fn cmd_hide(paths: &Paths, id: &str, dry_run: bool) -> Result<i32> {
    let state = write::State::read(paths)?;
    let plan = write::plan_hide(paths, &state, id)?;
    if !dry_run {
        require_root()?;
    }
    write::apply(&plan, dry_run)?;
    if !dry_run {
        println!("\nHidden: {id}");
        println!("Data preserved — 'snaptune unhide {id}' rebuilds the menu entry.");
        println!(" Note: the distro's OWN generated entry for this kernel, if any, is not");
        println!(" this tool's to remove — it sits under the distro's 'Advanced options'.");
    }
    Ok(0)
}

fn cmd_unhide(paths: &Paths, id: &str, dry_run: bool) -> Result<i32> {
    let state = write::State::read(paths)?;
    let plan = write::plan_unhide(paths, &state, id)?;
    if !dry_run {
        require_root()?;
    }
    write::apply(&plan, dry_run)?;
    if !dry_run {
        println!("\nUnhidden: {id} (menu entry rebuilt)");
    }
    Ok(0)
}

fn cmd_golden(paths: &Paths, id: Option<&str>, dry_run: bool, json: bool) -> Result<i32> {
    let store = Store::new(paths.clone());
    let Some(id) = id else {
        // Read-only form.
        let current = store.golden_id();
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({ "golden": current }))?
            );
        } else {
            match &current {
                Some(id) => println!("golden: {id}"),
                None => println!(
                    "golden: none set. `snap golden <id>` marks one; hide and prune \
                     then refuse to remove it."
                ),
            }
        }
        return Ok(0);
    };

    let state = write::State::read(paths)?;
    let plan = write::plan_golden(paths, &state, id)?;
    if !dry_run {
        require_root()?;
    }
    write::apply(&plan, dry_run)?;
    if !dry_run {
        println!("\nGolden: {id}");
        println!("This replaces whatever was golden before, which is now removable.");
    }
    Ok(0)
}

fn cmd_dev(description: &str, tree: Option<&std::path::Path>, json: bool) -> Result<i32> {
    let Some(path) = tree else {
        anyhow::bail!(
            "no source tree given.\n\
             Pass --tree <dir> or set SNAPSHOT_TREE. This is the tree you are\n\
             editing, not the snapshot store."
        );
    };

    let plan = dev::plan(dev::inspect_tree(path)?, dev::running_release());

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "description": description,
                "tree": plan.tree.path,
                "release": plan.tree.release,
                "running": plan.running,
                "revision": plan.tree.revision,
                "changed": plan.tree.dirty,
                "modules": plan.modules,
                "elsewhere": plan.elsewhere,
                "blockers": plan.blockers,
                "buildable": plan.can_build(),
            }))?
        );
    } else {
        println!("dev plan — {description}");
        println!();
        for line in plan.describe() {
            println!("{line}");
        }
        println!();
        println!("Nothing was built and nothing was written.");
    }

    // Non-zero when the tree cannot produce a module, so an agent can gate on the
    // exit code instead of parsing prose — the `doctor --verify` convention.
    Ok(if plan.can_build() { 0 } else { 2 })
}

/// One field that differs between two snapshots.
#[derive(Debug, Serialize)]
struct FieldDiff {
    field: String,
    a: String,
    b: String,
}

/// A structural comparison of two snapshots.
///
/// Pure, so the diff itself is tested rather than eyeballed at a terminal.
#[derive(Debug, Serialize)]
struct Diff {
    a: String,
    b: String,
    /// Boot artefacts: presence and total size.
    boot: Vec<FieldDiff>,
    /// Manifest fields present in both but with different values.
    changed: Vec<FieldDiff>,
    /// Manifest fields only one side has.
    only_a: Vec<(String, String)>,
    only_b: Vec<(String, String)>,
}

impl Diff {
    fn is_empty(&self) -> bool {
        self.boot.is_empty()
            && self.changed.is_empty()
            && self.only_a.is_empty()
            && self.only_b.is_empty()
    }
}

fn diff_of(paths: &Paths, left: &Snapshot, right: &Snapshot) -> Diff {
    // BTreeMap so the output is ordered and stable, which is what makes a diff
    // readable and what makes `--json` diffable a second time.
    let fields = |s: &Snapshot| -> BTreeMap<String, String> {
        s.manifest
            .as_ref()
            .map(|m| m.fields().into_iter().collect())
            .unwrap_or_default()
    };
    let (lf, rf) = (fields(left), fields(right));

    let mut changed = Vec::new();
    let mut only_a = Vec::new();
    let mut only_b = Vec::new();
    for (key, lval) in &lf {
        match rf.get(key) {
            Some(rval) if rval == lval => {}
            Some(rval) => changed.push(FieldDiff {
                field: key.clone(),
                a: lval.clone(),
                b: rval.clone(),
            }),
            None => only_a.push((key.clone(), lval.clone())),
        }
    }
    for (key, rval) in &rf {
        if !lf.contains_key(key) {
            only_b.push((key.clone(), rval.clone()));
        }
    }

    let yn = |present: bool| {
        if present {
            "present".to_string()
        } else {
            "MISSING".to_string()
        }
    };
    let mut boot = Vec::new();
    for (field, get) in [
        (
            "vmlinuz",
            (|s: &Snapshot| s.vmlinuz) as fn(&Snapshot) -> bool,
        ),
        ("initramfs", |s: &Snapshot| s.initramfs),
        ("cmdline", |s: &Snapshot| s.cmdline_file),
        ("grub_entry", |s: &Snapshot| s.grub_entry),
    ] {
        let (a, b) = (yn(get(left)), yn(get(right)));
        if a != b {
            boot.push(FieldDiff {
                field: field.to_string(),
                a,
                b,
            });
        }
    }
    let (lsize, rsize) = (left.size_bytes(paths), right.size_bytes(paths));
    if lsize != rsize {
        boot.push(FieldDiff {
            field: "size on disk".to_string(),
            a: human_bytes(lsize),
            b: human_bytes(rsize),
        });
    }

    Diff {
        a: left.id.clone(),
        b: right.id.clone(),
        boot,
        changed,
        only_a,
        only_b,
    }
}

fn cmd_diff(store: &Store, a: &str, b: &str, json: bool) -> Result<i32> {
    let left = store
        .get(a)?
        .with_context(|| format!("no snapshot with id {a}"))?;
    let right = store
        .get(b)?
        .with_context(|| format!("no snapshot with id {b}"))?;
    let diff = diff_of(&store.paths, &left, &right);

    if json {
        println!("{}", serde_json::to_string_pretty(&diff)?);
        return Ok(0);
    }

    println!("{}  ->  {}", diff.a, diff.b);
    if diff.is_empty() {
        println!("\nidentical as far as the manifests and boot artefacts go");
        return Ok(0);
    }
    if !diff.boot.is_empty() {
        println!("\nboot");
        for d in &diff.boot {
            println!("  {:<16} {}  ->  {}", d.field, d.a, d.b);
        }
    }
    if !diff.changed.is_empty() {
        println!("\nmanifest");
        for d in &diff.changed {
            println!("  {:<28} {}  ->  {}", d.field, d.a, d.b);
        }
    }
    for (id, rows) in [(&diff.a, &diff.only_a), (&diff.b, &diff.only_b)] {
        if rows.is_empty() {
            continue;
        }
        println!("\nonly in {id}");
        for (key, value) in rows.iter() {
            println!("  {key}: {value}");
        }
    }
    Ok(0)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Manifest;

    fn snap(id: &str, manifest: &str) -> Snapshot {
        Snapshot {
            id: id.to_string(),
            manifest: Some(Manifest::parse(manifest).expect("parse")),
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

    fn paths() -> Paths {
        Paths::under("/fixture")
    }

    /// A diff is only useful if it stays quiet about everything that did not
    /// change: a field-by-field dump of two manifests is not a diff.
    #[test]
    fn diff_reports_only_what_changed() {
        let a = snap(
            "snap-aaaaaaaa",
            "snapshot:\n  id: \"snap-aaaaaaaa\"\nkernel:\n  version: \"7.2.9\"\nmodule:\n  srcversion: \"AAA\"\n",
        );
        let b = snap(
            "snap-bbbbbbbb",
            "snapshot:\n  id: \"snap-bbbbbbbb\"\nkernel:\n  version: \"7.0.9\"\nmodule:\n  srcversion: \"AAA\"\n",
        );
        let diff = diff_of(&paths(), &a, &b);

        let fields: Vec<&str> = diff.changed.iter().map(|d| d.field.as_str()).collect();
        assert!(fields.contains(&"snapshot.id"), "{fields:?}");
        assert!(fields.contains(&"kernel.version"), "{fields:?}");
        assert!(
            !fields.contains(&"module.srcversion"),
            "an unchanged field is not a change: {fields:?}"
        );
        assert!(diff.only_a.is_empty() && diff.only_b.is_empty(), "{diff:?}");
    }

    /// A field on one side only is not the same finding as a changed field, and
    /// conflating them hides which snapshot gained something.
    #[test]
    fn diff_separates_asymmetric_fields_from_changed_ones() {
        let a = snap(
            "snap-aaaaaaaa",
            "snapshot:\n  id: \"snap-aaaaaaaa\"\npatches:\n  applied: true\n",
        );
        let b = snap(
            "snap-bbbbbbbb",
            "snapshot:\n  id: \"snap-bbbbbbbb\"\ntest_results:\n  boot_test: PASS\n",
        );
        let diff = diff_of(&paths(), &a, &b);

        assert_eq!(diff.changed.len(), 1, "only snapshot.id is on both sides");
        assert_eq!(
            diff.only_a,
            vec![("patches.applied".to_string(), "true".to_string())]
        );
        assert_eq!(
            diff.only_b,
            vec![("test_results.boot_test".to_string(), "PASS".to_string())]
        );
    }

    #[test]
    fn identical_snapshots_have_no_diff() {
        let m = "snapshot:\n  id: \"snap-aaaaaaaa\"\n  status: active\n";
        let diff = diff_of(
            &paths(),
            &snap("snap-aaaaaaaa", m),
            &snap("snap-aaaaaaaa", m),
        );
        assert!(diff.is_empty(), "{diff:?}");
    }

    /// The boot artefacts are the half the manifests cannot report — a snapshot
    /// whose vmlinuz has been deleted still has a manifest claiming otherwise.
    #[test]
    fn diff_notices_a_missing_boot_artefact() {
        let a = snap("snap-aaaaaaaa", "snapshot:\n  id: \"snap-aaaaaaaa\"\n");
        let mut b = snap("snap-bbbbbbbb", "snapshot:\n  id: \"snap-bbbbbbbb\"\n");
        b.vmlinuz = false;
        let diff = diff_of(&paths(), &a, &b);

        let fields: Vec<&str> = diff.boot.iter().map(|d| d.field.as_str()).collect();
        assert!(fields.contains(&"vmlinuz"), "{fields:?}");
    }

    /// A manifest that will not parse still diffs: `fields()` is empty rather
    /// than an error, which is the whole reason `Manifest::parse` never fails.
    #[test]
    fn an_unparseable_manifest_diffs_as_empty_rather_than_exploding() {
        let a = snap("snap-aaaaaaaa", "snapshot:\n  id: \"snap-aaaaaaaa\"\n");
        let b = snap("snap-bbbbbbbb", "snapshot:\n  id: x\n bad: [unclosed\n");
        let diff = diff_of(&paths(), &a, &b);
        assert!(
            !diff.only_a.is_empty(),
            "the broken side contributes nothing"
        );
        assert!(diff.only_b.is_empty(), "{:?}", diff.only_b);
    }
}
