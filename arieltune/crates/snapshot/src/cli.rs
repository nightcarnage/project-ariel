// SPDX-License-Identifier: GPL-2.0-only
//! The command surface.
//!
//! `list`/`show`/`current`/`lineage`/`grub-list` mirror the shell tool's verbs of
//! the same name, and `list --ids` is kept byte-compatible because
//! `apu::initramfs` parses it (`snapshot list --ids`) to answer "is this
//! snapshot one of ours?". Until that call site is replaced, that output is a
//! contract.
//!
//! Reading is always safe. The verbs that write — pin, unpin, hide, unhide,
//! golden, and `dev --register` — say so in their help, check for root, and take
//! `--dry-run` to print the plan first.
//!
//! `--json` is new, and is the reason agents can stop scraping the text table.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Serialize;

use crate::dev;
use crate::doctor;
use crate::grub;
use crate::id;
use crate::image;
use crate::manifest;
use crate::register;
use crate::report;
use crate::store::{human_bytes, Paths, Snapshot, Store};
use crate::verify;
use crate::write;

#[derive(Parser, Debug)]
#[command(
    name = "snaptune",
    version,
    about = "BC-250 snapshots: what is on this blade, and which one did I boot?",
    long_about = "Snapshot store: list, inspect, and manage the GRUB menu entries of\n\
        kernel snapshots created by the bc-250-snapshotter.\n\
        \n\
        Reads are safe anywhere. The writers -- create, rebuild, dev, register,\n\
        pin, unpin, hide, unhide, golden, boot and prune -- change the store,\n\
        /boot or the boot configuration (/etc/grub.d, grubenv) and so need root,\n\
        and each takes --dry-run to print exactly what it would do first. `prune`\n\
        deletes and cannot be undone; `boot` is one-shot and reverts on its own.\n\
        \n\
        Two things the writers deliberately do not do, both of which the shell\n\
        tool did: none of them installs a module into the live kernel tree, runs\n\
        `mkinitcpio`, or sets the GRUB default. A snapshot is derived by replacing\n\
        one member of a copy of an existing image, and promoting it is a separate,\n\
        deliberate step (`boot` to try it once, `pin` to keep it).\n\
        \n\
        The verb layout follows the other modules: flat, one word each, a report\n\
        verb for reading and a flag for anything that writes (`dev` previews unless\n\
        --run, as `apu build` and `mem set` do)."
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
    /// changed, into a snapshot of its own.
    ///
    /// The dev loop does not need a kernel build, only the amdgpu module relinked
    /// (`make M=... modules`, seconds). The build set comes from `git status`, not
    /// from the patches' `touches` field, because the question is what the tree
    /// differs in right now — after hand-edits, half-reverted hunks, or a file the
    /// patch author never listed.
    ///
    /// Without `--run` this only reports the plan. With `--run` it builds the
    /// module and derives an image from the parent's, verifying the result against
    /// an independent cpio oracle. With `--register` as well it then becomes a
    /// real snapshot: an id, a manifest, boot artefacts under `/boot`, and a GRUB
    /// entry — so the iteration shows up in the SNAP tab and boots through the
    /// normal flow.
    ///
    /// It refuses rather than pretending when the tree builds a different kernel
    /// from the one the parent image boots, or when every change is outside
    /// drivers/ and a module rebuild would capture none of it.
    Dev {
        /// Description of this iteration, e.g. "patch 25 variant".
        description: String,
        /// Kernel source tree (or set SNAPSHOT_TREE).
        #[arg(long, value_name = "DIR", env = "SNAPSHOT_TREE")]
        tree: Option<PathBuf>,
        /// Actually build the module and derive the image. Without it, only the
        /// plan is printed and nothing runs.
        #[arg(long)]
        run: bool,
        /// Parent snapshot whose image the dev image is derived from. Default: the
        /// booted snapshot.
        #[arg(long, value_name = "SNAP-ID")]
        parent: Option<String>,
        /// Derive from this image directly, instead of a snapshot's.
        #[arg(long, value_name = "IMAGE", conflicts_with = "parent")]
        from: Option<PathBuf>,
        /// Kernel release the module is being built for. Default: read from the
        /// parent image, else this machine's running kernel.
        #[arg(long = "for", value_name = "RELEASE")]
        for_release: Option<String>,
        /// Where to write the derived image. Default: scratch space, reported so
        /// the result can be inspected.
        #[arg(long, value_name = "IMAGE")]
        out: Option<PathBuf>,
        /// Register the result as a snapshot: give it an id, a manifest, boot
        /// artefacts and a GRUB entry. Implies --run. Needs root, and takes
        /// --dry-run to print the plan first.
        #[arg(long, requires = "run")]
        register: bool,
        /// With --register, print the registration plan and change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Arm a one-shot boot into a snapshot: the next boot uses it, the one after
    /// falls back to the pinned default. Needs root.
    ///
    /// One-shot rather than changing `saved_entry`, so this cannot leave a blade
    /// stuck booting a kernel that will not come up — a snapshot that fails to boot
    /// reverts by itself. The same check `pin` does applies here: the armed value is
    /// read back from grubenv, because `grub-reboot` exits 0 even when `grub.cfg`
    /// was generated without `GRUB_DEFAULT=saved` and would ignore it.
    #[command(alias = "reboot-into")]
    Boot {
        /// Snapshot id, e.g. snap-0848359a.
        id: String,
        /// Print the plan and change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Reboot as soon as the one-shot is armed and verified. Without it, only
        /// the arming happens, and the machine reboots when you say so.
        #[arg(long)]
        reboot: bool,
    },

    /// Preflight the store: is this box's boot configuration coherent? Read-only.
    ///
    /// Checks the ways a store actually goes wrong rather than restating `list`: an
    /// entry whose files were deleted (GRUB keeps the row and cannot boot it), a
    /// snapshot with no entry, a `/boot` kernel nothing claims, an unparseable
    /// manifest, and whether `GRUB_DEFAULT` is `saved` — because if it is not, `pin`
    /// writes grubenv and silently does nothing, while the one-shot `boot` still
    /// works. Names the offenders rather than only counting them.
    ///
    /// `--verify` turns the report into a gate: a non-zero exit when anything is
    /// broken, so post-reboot scripts can ask.
    Doctor {
        /// One JSON object on stdout and nothing else, for callers that parse it.
        #[arg(long)]
        json: bool,
        /// Exit non-zero when anything is broken, rather than only printing it.
        #[arg(long)]
        verify: bool,
    },

    /// Check a snapshot against what is actually on disk. Read-only.
    ///
    /// `doctor` asks whether the *store* is coherent; this asks whether one
    /// *snapshot* is what its manifest claims — a different question, because a
    /// manifest is written once and never checked again afterwards. It re-reads the
    /// files the manifest names, compares the module's digest against the recorded
    /// one, confirms the GRUB entry boots this snapshot's own kernel and image, and
    /// recomputes the id from the stored bytes.
    ///
    /// The id is optional: with none, every visible snapshot is checked and only the
    /// ones that are not clean are listed. Names the offenders rather than counting
    /// them.
    ///
    /// Exits 2 when anything failed — there is no `--verify` flag here, because
    /// gating is this verb's whole job.
    Verify {
        /// Snapshot id, e.g. snap-0848359a. Omit to check them all.
        id: Option<String>,
        /// One JSON object on stdout and nothing else, for callers that parse it.
        #[arg(long)]
        json: bool,
    },

    /// Rebuild an existing snapshot's module from its source tree, as a NEW
    /// snapshot. Read the original's id as its promise: it is untouched.
    ///
    /// The shell tool's `rebuild` writes the rebuilt module into the snapshot it
    /// names and keeps the id — so the id stops describing the content, which is
    /// exactly the mismatch `verify` reports. It then tells you to run `create`,
    /// which makes a new id anyway, so the in-place write was only staging.
    ///
    /// This refuses to stage. It builds the module for that snapshot's kernel,
    /// derives the image from that snapshot's own, and registers the result — so
    /// the original keeps meaning what its name says, and the rebuild is a
    /// first-class snapshot you can boot and verify.
    ///
    /// Refuses when the tree already produces the module the snapshot holds: there
    /// is nothing to rebuild, and saying so is more use than a second snapshot with
    /// identical content — which the content-addressed id would refuse as a
    /// duplicate in any case.
    Rebuild {
        /// Snapshot to rebuild for, e.g. snap-0848359a.
        id: String,
        /// Source tree to build in. Default: the one the snapshot recorded.
        #[arg(long, value_name = "DIR", env = "SNAPSHOT_TREE")]
        tree: Option<PathBuf>,
        /// Description for the new snapshot. Default: names the one it came from.
        #[arg(long, value_name = "TEXT")]
        description: Option<String>,
        /// Print the plan and change nothing.
        #[arg(long)]
        dry_run: bool,
    },

    /// Build a module and make it a bootable snapshot.
    ///
    /// Two ways to get the module. With `--tree` it is built from that source tree,
    /// which is the strongest provenance. Without one, the module the kernel package
    /// installed is adopted as it stands — the running, already-patched module, which
    /// is all a blade provisioned from prebuilt packages has.
    ///
    /// **What this deliberately does not do.** The shell tool's `create` installs the
    /// new module into the live tree, runs `depmod`, generates the snapshot's
    /// initramfs with `mkinitcpio`, then restores the known-good module and rebuilds
    /// *every* image on the machine with `mkinitcpio -P`. If it fails in between, the
    /// running system is left holding a module that has never booted. This derives
    /// the snapshot by replacing one member of a copy of an existing image and
    /// checking the result against the build, so the live module tree and the live
    /// initramfs are never touched and the machine's own boot never depends on an
    /// untested build.
    ///
    /// The difference worth knowing: the snapshot's initramfs is the base image's
    /// member set plus this module, not a fresh one generated from
    /// `/etc/mkinitcpio.conf`. If that file has gained a hook or a module since the
    /// base image was built, the snapshot will not carry it. `--from` chooses the
    /// base image; the default is the running kernel's own.
    ///
    /// Never sets the GRUB default: an untested snapshot must not become the
    /// permanent entry. `snaptune boot <id>` tests it once and reverts by itself;
    /// `snaptune pin <id>` makes it the default once it has earned it.
    Create {
        /// What this snapshot is, e.g. "golden-base: patches 1-3 (40 CU unlock)".
        description: String,
        /// Kernel source tree to build the module in, or set SNAPSHOT_TREE. Without
        /// it, the installed module is adopted as it stands.
        #[arg(long, value_name = "DIR", env = "SNAPSHOT_TREE")]
        tree: Option<PathBuf>,
        /// Adopt this already-compressed module instead of building one or taking
        /// the installed one.
        #[arg(long, value_name = "MODULE", conflicts_with = "tree")]
        module: Option<PathBuf>,
        /// Image to derive from. Default: the running kernel's own initramfs.
        #[arg(long, value_name = "IMAGE")]
        from: Option<PathBuf>,
        /// Kernel release to build and record for. Default: this machine's running
        /// kernel.
        #[arg(long = "for", value_name = "RELEASE")]
        for_release: Option<String>,
        /// Print the plan and change nothing.
        #[arg(long)]
        dry_run: bool,
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

    /// Delete a snapshot completely: its `/boot` artefacts, its directory and its
    /// GRUB entry. **Irreversible.** Needs root.
    ///
    /// Use this instead of deleting files by hand. The ordering matters and is not
    /// obvious: the distro's `10_linux` generates a menu entry for every
    /// `/boot/vmlinuz-*`, so the artefacts must be gone *before* `grub-mkconfig`
    /// runs, or the regenerated menu keeps an entry pointing at a kernel that no
    /// longer exists. That entry looks fine until GRUB tries to load it.
    ///
    /// Refuses the booted, golden and pinned snapshots, for the same reasons `hide`
    /// does: each is either running, deliberately kept, or the entry GRUB would
    /// fall back to.
    #[command(alias = "delete", alias = "rm")]
    Prune {
        /// Snapshot id, e.g. snap-0848359a.
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
        Cmd::Dev {
            description,
            tree,
            run,
            parent,
            from,
            for_release,
            out,
            register,
            dry_run,
        } => cmd_dev(
            &store,
            &description,
            tree.as_deref(),
            DevTargets {
                run,
                parent,
                from,
                for_release,
                out,
                register,
                dry_run,
            },
            json,
        ),
        Cmd::Pin { id, dry_run } => cmd_pin(&store.paths, &id, dry_run),
        Cmd::Unpin { dry_run } => cmd_unpin(&store.paths, dry_run),
        Cmd::Hide { id, dry_run } => cmd_hide(&store.paths, &id, dry_run),
        Cmd::Unhide { id, dry_run } => cmd_unhide(&store.paths, &id, dry_run),
        Cmd::Prune { id, dry_run } => cmd_prune(&store.paths, &id, dry_run),
        Cmd::Boot {
            id,
            dry_run,
            reboot,
        } => cmd_boot(&store.paths, &id, dry_run, reboot),
        Cmd::Doctor { json, verify } => cmd_doctor(&store.paths, json, verify),
        Cmd::Verify { id, json } => cmd_verify(&store, id.as_deref(), json),
        Cmd::Rebuild {
            id,
            tree,
            description,
            dry_run,
        } => cmd_rebuild(
            &store,
            &id,
            tree.as_deref(),
            description.as_deref(),
            dry_run,
            json,
        ),
        Cmd::Create {
            description,
            tree,
            module,
            from,
            for_release,
            dry_run,
        } => cmd_create(
            &store,
            &description,
            tree.as_deref(),
            module.as_deref(),
            from.as_deref(),
            for_release.as_deref(),
            dry_run,
            json,
        ),
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

fn cmd_prune(paths: &Paths, id: &str, dry_run: bool) -> Result<i32> {
    let state = write::State::read(paths)?;
    let plan = write::plan_prune(paths, &state, id)?;
    if !dry_run {
        require_root()?;
    }
    write::apply(&plan, dry_run)?;
    if !dry_run {
        println!("\nPruned: {id}");
        println!(
            "Its artefacts, its directory and its menu entry are gone. This cannot be undone."
        );
        println!(" The running kernel and the golden image were left alone.");
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

/// Which image a dev snapshot derives from, and where the result goes.
///
/// Grouped rather than passed as four positional arguments: three of them are
/// optional paths, and mistaking one for another is the kind of thing that looks
/// fine until the image comes out containing the wrong parent's contents.
struct DevTargets {
    run: bool,
    parent: Option<String>,
    from: Option<PathBuf>,
    for_release: Option<String>,
    out: Option<PathBuf>,
    register: bool,
    dry_run: bool,
}

/// What to *record* about how a snapshot came to be.
///
/// [`DevTargets`] says where the bytes come from; this says what the manifest
/// should claim about them. They are different questions — the dev loop and
/// `create` can derive from the same parent image but must not describe
/// themselves identically, and a reader deciding whether an id can be recomputed
/// is reading exactly this.
struct Provenance<'a> {
    /// `snapshot.kind` — which id formula applies. See
    /// [`crate::manifest::id_is_content_addressed`].
    kind: &'a str,
    /// `snapshot.status` — `dev` for a loop iteration, `active` for a snapshot in
    /// its own right, `golden` for the store's baseline.
    status: &'a str,
    /// The tree it was built in, when there was one. `None` for a snapshot taken
    /// of the running module, which was never built here.
    tree: Option<&'a dev::Tree>,
    /// The module directories the build relinked.
    rebuilt_modules: &'a [String],
}

/// Which kernel release a dev build is for.
///
/// The order matters. An explicit `--for` wins; otherwise the parent image is
/// asked, because the image knows the kernel it boots and that is authoritative;
/// only then does it fall back to this machine, which is the wrong answer whenever
/// you are building for a blade from a workstation.
fn dev_target_release(targets: &DevTargets, parent_image: Option<&Path>) -> Option<String> {
    if let Some(release) = &targets.for_release {
        return Some(release.clone());
    }
    if let Some(image) = parent_image {
        if let Ok(member) = image::find_member(image, "amdgpu.ko.zst") {
            if let Some(kver) = dev::kver_from_member(&member) {
                return Some(kver);
            }
        }
    }
    dev::running_release()
}

fn cmd_dev(
    store: &Store,
    description: &str,
    tree: Option<&std::path::Path>,
    targets: DevTargets,
    json: bool,
) -> Result<i32> {
    let Some(path) = tree else {
        anyhow::bail!(
            "no source tree given.\n\
             Pass --tree <dir> or set SNAPSHOT_TREE. This is the tree you are\n\
             editing, not the snapshot store."
        );
    };

    // Resolve where the image comes from first, so the tree can be judged against
    // the kernel that image boots rather than against this machine.
    let parent_image = match targets.from.clone() {
        Some(image) => Some(image),
        None => match (&targets.parent, store.booted_id()) {
            (Some(id), _) => Some(store.paths.boot.join(format!("initramfs-{id}.img"))),
            (None, Some(id)) => Some(store.paths.boot.join(format!("initramfs-{id}.img"))),
            (None, None) => None,
        },
    };

    // Resolved once and reused: the plan is judged against it, and if the build
    // is registered, the manifest records the same release. Two resolutions could
    // disagree.
    let target_release = dev_target_release(&targets, parent_image.as_deref());
    let plan = dev::plan(dev::inspect_tree(path)?, target_release.clone());

    if !json {
        println!("dev plan — {description}");
        println!();
        for line in plan.describe() {
            println!("{line}");
        }
        println!();
    }

    if !plan.can_build() {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "description": description,
                    "buildable": false,
                    "blockers": plan.blockers,
                }))?
            );
        } else {
            println!("Nothing was built and nothing was written.");
        }
        return Ok(2);
    }

    if !targets.run {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "description": description,
                    "buildable": true,
                    "modules": plan.modules,
                    "ran": false,
                }))?
            );
        } else {
            println!("Nothing was built and nothing was written. Pass --run to build.");
        }
        return Ok(0);
    }

    // The image this derives from. Everything the dev image is not explicitly told
    // to replace comes from here, so getting it wrong means shipping the parent's
    // contents under a new name.
    let parent_image = parent_image.filter(|image| image.is_file()).context(
        "no parent image: not booted from a snapshot, and none was named.\n\
             Pass --from <image> or --parent <id>.",
    )?;

    let work = std::env::temp_dir().join(format!("snapdev-{}", std::process::id()));

    // `--register` needs root, but the build must not be root's: `make` rewrites the
    // objects its inputs changed, and a root-owned `amdgpu_drv.o` breaks the next
    // build the developer runs as themselves. Hand the build to the tree's owner,
    // and the scratch directory with it, since that is where the stripped and
    // compressed module are written.
    let build_runner = match dev::build_as(&plan.tree.path) {
        Some((uid, gid)) => {
            std::fs::create_dir_all(&work)
                .with_context(|| format!("creating scratch space {}", work.display()))?;
            dev::hand_over(&work, uid, gid)?;
            write::System::as_user(uid, gid)
        }
        None => write::System::default(),
    };

    let prepared = dev::build(&plan.tree, &build_runner, &work)?;
    println!(
        "built       {} ({} bytes, stripped)",
        prepared.source.display(),
        prepared.raw_bytes
    );

    let dest = match targets.out.as_deref() {
        Some(path) => path.to_path_buf(),
        None => work.join("dev.img"),
    };
    let (member, derived) = dev::assemble(&parent_image, &prepared, &dest, &work)?;

    // Verify with the independent oracle, not our own parser: this is the check
    // that distinguishes "the image carries the module we built" from "the image
    // looks plausible".
    let got = image::verify_member(&dest, &member)
        .with_context(|| format!("verifying {member} in the derived image"))?;
    if got != prepared.bytes {
        anyhow::bail!(
            "the derived image does not carry the module we built \
             ({} bytes in the image, {} bytes built) — refusing to call this a success",
            got.len(),
            prepared.bytes.len()
        );
    }

    // Text mode reports the build before the registration plan, so the two outputs
    // read in the order they actually happened.
    if !json {
        println!("replaced    {member}");
        println!(
            "image       {} -> {} bytes (delta {})",
            derived.parent_bytes,
            derived.image_bytes,
            derived.delta()
        );
        println!("wrote       {}", dest.display());
        println!();
        println!("Verified: that member in the new image is byte-identical to the module built.");
        println!("The live module tree was not written and the base kernel is untouched.");
    }

    // Registration happens after verification, never before: promoting an image
    // that failed the oracle check into a boot menu entry is exactly the silent
    // wrong-image boot the whole check exists to prevent.
    let registered_id = if targets.register {
        Some(register_build(
            store,
            description,
            &prepared,
            &dest,
            &parent_image,
            target_release.as_deref(),
            &targets,
            &Provenance {
                kind: manifest::KIND_DEV,
                status: "dev",
                tree: Some(&plan.tree),
                rebuilt_modules: &plan.modules,
            },
            json,
        )?)
    } else {
        None
    };

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "description": description,
                "buildable": true,
                "ran": true,
                "module": prepared.source,
                "module_bytes": prepared.bytes.len(),
                "member": member,
                "parent_image": parent_image,
                "image": dest,
                "image_bytes": derived.image_bytes,
                "delta": derived.delta(),
                "verified": true,
                // Absent rather than null when registration was not asked for.
                "registered": if targets.dry_run { None } else { registered_id.clone() },
                "dry_run": targets.dry_run,
            }))?
        );
    } else if let Some(id) = &registered_id {
        if targets.dry_run {
            println!();
            println!("That is the registration plan. Run it again without --dry-run to apply it.");
        } else {
            println!();
            println!("Registered: {id}");
            println!("The SNAP tab lists it, and it is on the GRUB menu.");
        }
    }
    Ok(0)
}

/// Turn a verified dev image into a real snapshot.
///
/// The id is computed the same way the shell tool's `register` computes it, over
/// the module, the image and the kernel — so the id *is* the claim that this
/// module is in this image, and registering the same build twice is refused
/// rather than overwriting.
///
/// The kernel is resolved from the parent image's name rather than from the
/// running machine: on a workstation the running kernel is the wrong one, and
/// pairing a snapshot with a kernel it was not built for is a mistake that only
/// shows up as a hang at boot.
#[allow(clippy::too_many_arguments)]
fn register_build(
    store: &Store,
    description: &str,
    prepared: &dev::Prepared,
    image: &Path,
    parent_image: &Path,
    target_release: Option<&str>,
    targets: &DevTargets,
    provenance: &Provenance<'_>,
    quiet: bool,
) -> Result<String> {
    let kernel_version = target_release
        .context(
            "cannot tell which kernel release to record in the manifest.\n\
             Pass --for <release>, or point --from at a snapshot's own image.",
        )?
        .to_string();

    let vmlinuz = register::vmlinuz_for(parent_image)?;
    if !vmlinuz.is_file() {
        anyhow::bail!(
            "the parent image's kernel is not where its name says: {}\n\
             Registration copies the kernel, so it has to be there. Pass --from \
             with the image's real path under /boot, or --parent <snap-id>.",
            vmlinuz.display()
        );
    }

    let id = id::register_id(&prepared.compressed, image, &vmlinuz, None);
    let timestamp = register::now_timestamp()?;
    // Read from the live file, not from the plan: the entry is appended to it, and
    // writing only the new entry would delete every other snapshot's menu entry.
    let existing_custom = std::fs::read_to_string(&store.paths.grub_custom).unwrap_or_default();
    let cmdline = register::cmdline_for(&vmlinuz)?;

    // The image it was actually derived from, not whatever happens to be booted:
    // `--from` can name any image, and claiming the booted snapshot as the parent
    // records a lineage that does not exist.
    let parent_label = targets
        .parent
        .clone()
        .unwrap_or_else(|| register::parent_of(parent_image));

    let req = register::Request {
        id: id.clone(),
        description: description.to_string(),
        status: provenance.status.to_string(),
        kind: provenance.kind.to_string(),
        module: prepared.compressed.clone(),
        initramfs: image.to_path_buf(),
        vmlinuz_source: vmlinuz,
        kernel_version,
        cmdline,
        root_uuid: register::root_uuid()?,
        parent: Some(parent_label.clone()),
        tree: provenance.tree.map(|t| t.path.clone()),
        // What the build actually relinked. `changed_files` below is the stronger
        // claim but only a checkout of its own can make it; this one is knowable
        // either way, and it is what answers "did my edit get picked up?".
        rebuilt_modules: provenance.rebuilt_modules.to_vec(),
        // The one comparison worth recording: a parent that is a snapshot in this
        // store has a module on disk, and matching digests mean this build produced
        // what the parent already had. A parent that is a distro image has nothing to
        // compare against, and the manifest says so rather than implying a change.
        parent_module_md5: register::stored_module(&store.paths.root, &parent_label)
            .map(|path| id::file_md5_hex(&path))
            .filter(|md5| !md5.is_empty()),
        // Only a tree that is its own checkout can say what changed. Handing over
        // an empty list from a tree that cannot tell would record "nothing
        // changed", which is a different and false claim.
        changed_files: provenance
            .tree
            .and_then(|t| t.versioned.then(|| t.dirty.clone())),
        module_md5: prepared.md5.clone(),
        module_size: prepared.bytes.len() as u64,
        module_srcversion: prepared.srcversion.clone(),
        module_vermagic: prepared.vermagic.clone(),
        store_root: store.paths.root.clone(),
        boot: store.paths.boot.clone(),
        acpi_override: store.paths.boot.join("acpi_override.cpio").is_file(),
    };

    let plan = register::plan(&store.paths, &req, &timestamp, &existing_custom)?;
    if !targets.dry_run {
        require_root()?;
    }
    // `quiet` is the caller's `--json`: the registration plan is narration, and the
    // caller is about to print one object on stdout and nothing else.
    write::apply_with_mode(&plan, targets.dry_run, &write::System::default(), quiet)?;
    Ok(id)
}

/// Arm a one-shot boot, and optionally reboot into it.
///
/// **The reboot waits for the verification.** `grub-reboot` reports success whether
/// or not GRUB will honour `next_entry`, so rebooting on the strength of an exit code
/// boots whatever the default happened to be — silently, and by the time anyone
/// notices the machine is already coming up wrong. Arming is checked the way `pin` is
/// checked, and only then does the machine go down.
fn cmd_boot(paths: &Paths, id: &str, dry_run: bool, reboot: bool) -> Result<i32> {
    let state = write::State::read(paths)?;
    let plan = write::plan_boot(&state, id)?;
    if !dry_run {
        require_root()?;
    }
    write::apply(&plan, dry_run)?;

    if dry_run {
        if reboot {
            println!("\n--dry-run: the one-shot was not armed, so the machine was not rebooted.");
        }
        return Ok(0);
    }

    write::verify_boot(id)?;
    println!("\nArmed: {id} boots next, once. After that the pinned default returns.");
    if !reboot {
        println!("Reboot when you are ready — `snaptune boot {id} --reboot` does both.");
        return Ok(0);
    }

    println!("Rebooting...");
    // Deliberately not checked: if `reboot` is missing or refuses, the arming has
    // already happened and the operator has been told so — reporting a failure here
    // would suggest the one-shot did not take when it did.
    let _ = std::process::Command::new("reboot").status();
    Ok(0)
}

/// Preflight the store. Read-only, and safe anywhere — it is the one verb here that
/// is useful precisely when something is already wrong.
fn cmd_doctor(paths: &Paths, json: bool, verify: bool) -> Result<i32> {
    let report = doctor::examine(paths);

    if json {
        // One object on stdout, nothing else, so callers parse it cleanly — the same
        // contract `apu doctor --json` keeps.
        let findings: Vec<serde_json::Value> = report
            .findings
            .iter()
            .map(|f| {
                serde_json::json!({
                    "check": f.key,
                    "severity": f.severity.label(),
                    "detail": f.detail,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "store": paths.root,
                "worst": report.worst().label(),
                "findings": findings,
            }))?
        );
        return Ok(exit_for(&report, verify));
    }

    println!("snaptune doctor");
    for line in report.lines() {
        println!("{line}");
    }

    let failures = report.failures();
    if failures.is_empty() {
        println!("\n  -> nothing broken. `snaptune list` for the snapshots themselves.");
    } else {
        println!(
            "\n  -> {} thing(s) broken. `snaptune verify <id>` for one snapshot in \
             detail; `prune` removes one properly.",
            failures.len()
        );
    }
    Ok(exit_for(&report, verify))
}

/// The exit code, which is what makes `--verify` a gate.
fn exit_for(report: &report::Report, verify: bool) -> i32 {
    if verify && report.worst() == report::Severity::Fail {
        2
    } else {
        0
    }
}

/// Check a snapshot, or all of them, against what is on disk.
///
/// Gating is unconditional here, unlike `doctor --verify`: the other verbs report as
/// a courtesy while being useful anyway, whereas a verifier that answers non-zero for
/// a broken snapshot is the entire point of running it.
fn cmd_verify(store: &Store, id: Option<&str>, json: bool) -> Result<i32> {
    let paths = &store.paths;

    // One snapshot, or every visible one. Hidden snapshots are left out when no id is
    // given: they have no entry by design, so the entry check would report the design
    // as a fault — and a snapshot someone deliberately put aside is not the one whose
    // integrity needs a gate.
    let reports: Vec<(String, report::Report)> = match id {
        Some(id) => match store.get(id)? {
            Some(snapshot) => vec![(snapshot.id.clone(), verify::one(paths, &snapshot))],
            None => {
                eprintln!("snapshot {id} not found in {}", paths.root.display());
                return Ok(1);
            }
        },
        None => verify::all(paths),
    };

    let worst = reports
        .iter()
        .map(|(_, r)| r.worst())
        .max()
        .unwrap_or(report::Severity::Ok);

    if json {
        // One object on stdout, nothing else — the same contract `doctor --json` keeps.
        let snapshots: Vec<serde_json::Value> = reports
            .iter()
            .map(|(id, report)| {
                serde_json::json!({
                    "id": id,
                    "worst": report.worst().label(),
                    "findings": report
                        .findings
                        .iter()
                        .map(|f| serde_json::json!({
                            "check": f.key,
                            "severity": f.severity.label(),
                            "detail": f.detail,
                        }))
                        .collect::<Vec<_>>(),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "store": paths.root,
                "worst": worst.label(),
                "snapshots": snapshots,
            }))?
        );
        return Ok(if worst == report::Severity::Fail {
            2
        } else {
            0
        });
    }

    // With no failures and no id, the per-check detail is 5 lines times N snapshots of
    // "ok" — noise. The summary is the answer; `show` still has the detail.
    let quiet = id.is_none();
    println!("snaptune verify");
    for (sid, report) in &reports {
        if quiet && report.worst() == report::Severity::Ok {
            continue;
        }
        println!("\n  {sid}");
        for line in report.lines() {
            println!("{line}");
        }
    }

    let broken: usize = reports.iter().map(|(_, r)| r.failures().len()).sum();
    let unchecked = reports
        .iter()
        .filter(|(_, r)| r.worst() == report::Severity::Warn)
        .count();

    // An `ok` line means a check ran and passed. A `warn` line can mean "this is not
    // quite right" or "this could not be checked", and those are different answers to
    // "is this snapshot what it claims?" — so the summary says how many snapshots were
    // only partly verifiable rather than calling the run clean.
    if broken == 0 {
        let mut line = format!("\n  -> {} snapshot(s), nothing broken", reports.len());
        if unchecked > 0 {
            line.push_str(&format!(
                "; {unchecked} only partly checked (see the `warn` lines)"
            ));
        }
        println!("{line}.");
    } else {
        println!(
            "\n  -> {broken} problem(s) across {} snapshot(s). `prune` removes one \
             properly if the fault is in the files; `snaptune doctor` first if more \
             than one is broken.",
            reports.len()
        );
    }
    Ok(if broken > 0 { 2 } else { 0 })
}

/// Rebuild one snapshot's module, as a new snapshot.
///
/// The parent is the snapshot being rebuilt rather than whatever is booted: the
/// question this answers is "what does my tree produce for *that* kernel", and
/// answering it against the running kernel would silently build for the wrong
/// one whenever you are standing on a different snapshot — which, on a blade you
/// are bisecting patches on, is most of the time.
#[allow(clippy::too_many_arguments)]
fn cmd_rebuild(
    store: &Store,
    id: &str,
    tree: Option<&Path>,
    description: Option<&str>,
    dry_run: bool,
    json: bool,
) -> Result<i32> {
    let Some(snapshot) = store.get(id)? else {
        eprintln!("snapshot {id} not found in {}", store.paths.root.display());
        return Ok(1);
    };
    let Some(manifest) = &snapshot.manifest else {
        // Without a manifest there is no recorded tree, no kernel and no module
        // digest — nothing to rebuild *for*. Guessing any of the three would be a
        // build for an unknown target.
        anyhow::bail!(
            "{id} has no manifest, so nothing says which kernel to rebuild for or \
             from which tree.\n\
             `snaptune verify {id}` says what is wrong with it."
        );
    };

    let release = manifest.kernel_version().context(format!(
        "{id}'s manifest records no kernel version, so there is no release to \
         rebuild for."
    ))?;

    // The tree: `--tree` wins, because a recorded path can go stale and the user
    // may have moved the checkout.
    let recorded = manifest.source_tree_path().filter(|p| !p.is_empty());
    let Some(tree_path) = tree
        .map(Path::to_path_buf)
        .or_else(|| recorded.map(PathBuf::from))
    else {
        anyhow::bail!(
            "{id} does not record a source tree, so there is nothing to rebuild \
             from.\n\
             Pass --tree <dir>. (Its manifest says source_tree.mode = {}, which is \
             what a snapshot taken of the running module looks like.)",
            manifest.source_tree_mode().as_deref().unwrap_or("nothing")
        );
    };
    if !tree_path.is_dir() {
        anyhow::bail!(
            "the tree to rebuild in is not there: {}\n\
             Pass --tree <dir> if it moved. Nothing was built.",
            tree_path.display()
        );
    }

    // Derive from *this* snapshot's own image. Its kernel and its initramfs are
    // what make it bootable; deriving from anything else would pair the rebuilt
    // module with a kernel it was not built for.
    let parent_image = manifest
        .initramfs()
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .with_context(|| {
            format!(
                "{id}'s own initramfs is not where its manifest says it is, so the \
                 rebuild would have nothing to derive from.\n\
                 `snaptune verify {id}` shows which file is missing."
            )
        })?;

    let plan = dev::plan(dev::inspect_tree(&tree_path)?, Some(release.clone()));

    // One object on stdout and nothing else, the contract every other verb's
    // `--json` keeps. Filled in as the run learns things, so each exit point below
    // reports what it actually did rather than leaving the caller to infer it.
    let mut out = serde_json::json!({
        "id": id,
        "release": release,
        "tree": plan.tree.path,
        "parent_image": parent_image,
        "dry_run": dry_run,
    });

    if !json {
        println!("rebuild plan — {id} (kernel {release})");
        // The tree, release and revision come from `describe()` below; what only this
        // verb knows is which image the rebuild derives from, so that is what the
        // header says rather than repeating them.
        println!("  derives from {}", parent_image.display());
        for line in plan.describe() {
            println!("{line}");
        }
    }

    if !plan.can_build() {
        if json {
            out["buildable"] = serde_json::json!(false);
            out["blockers"] = serde_json::json!(plan.blockers);
            println!("{}", serde_json::to_string(&out)?);
        } else {
            println!("\nNothing was built and nothing was written.");
        }
        return Ok(2);
    }

    if dry_run {
        if json {
            out["buildable"] = serde_json::json!(true);
            out["built"] = serde_json::json!(false);
            println!("{}", serde_json::to_string(&out)?);
        } else {
            println!("\n--dry-run: nothing was built and nothing was written.");
        }
        return Ok(0);
    }

    // Same privilege rule as the dev loop: `--register` needs root for the writes
    // under /boot and the store, but the build must not be root's, or `make`
    // leaves root-owned objects in the developer's tree.
    let work = std::env::temp_dir().join(format!("snaprebuild-{}", std::process::id()));
    let build_runner = match dev::build_as(&plan.tree.path) {
        Some((uid, gid)) => {
            std::fs::create_dir_all(&work)
                .with_context(|| format!("creating scratch space {}", work.display()))?;
            dev::hand_over(&work, uid, gid)?;
            write::System::as_user(uid, gid)
        }
        None => write::System::default(),
    };

    let prepared = dev::build(&plan.tree, &build_runner, &work)?;
    out["module"] = serde_json::json!(prepared.source);
    out["module_md5"] = serde_json::json!(prepared.md5);
    out["module_bytes"] = serde_json::json!(prepared.bytes.len());
    if !json {
        println!(
            "built       {} ({} bytes, stripped)",
            prepared.source.display(),
            prepared.raw_bytes
        );
    }

    // Compared *before* deriving, and before anything is written: an unchanged
    // tree means there is nothing to rebuild, and the answer to "did my edit land"
    // is more useful as a sentence than as a second snapshot with the same
    // contents (which the content-addressed id would refuse as a duplicate).
    out["previous_module_md5"] = serde_json::json!(manifest.module_md5());
    if let Some(recorded_md5) = manifest.module_md5() {
        if recorded_md5 == prepared.md5 {
            if json {
                out["built"] = serde_json::json!(true);
                out["changed"] = serde_json::json!(false);
                out["registered"] = serde_json::json!(null);
                println!("{}", serde_json::to_string(&out)?);
            } else {
                println!();
                println!(
                    "Nothing to rebuild: this tree already produces the module {id} holds \
                     (md5 {recorded_md5})."
                );
                println!("{id} is unchanged, and no new snapshot was registered.");
            }
            return Ok(0);
        }
        if !json {
            println!("module      {recorded_md5} -> {}", prepared.md5);
        }
    } else if !json {
        println!(
            "module      {} (nothing recorded to compare against)",
            prepared.md5
        );
    }

    let dest = work.join("rebuild.img");
    // The report is not printed: the member and byte counts are already in it, and
    // the check that matters is the oracle below, not our own arithmetic.
    let (member, _derived) = dev::assemble(&parent_image, &prepared, &dest, &work)?;
    let got = image::verify_member(&dest, &member).with_context(|| {
        format!("verifying {member} in the image derived for the rebuild of {id}")
    })?;
    if got != prepared.bytes {
        anyhow::bail!(
            "the derived image does not carry the module we built \
             ({} bytes in the image, {} bytes built) — refusing to register it",
            got.len(),
            prepared.bytes.len()
        );
    }
    out["member"] = serde_json::json!(member);
    out["image"] = serde_json::json!(dest);
    out["verified"] = serde_json::json!(true);
    out["changed"] = serde_json::json!(true);
    if !json {
        println!("replaced    {member}");
        println!("wrote       {}", dest.display());
    }

    let description =
        description
            .map(str::to_string)
            .unwrap_or_else(|| match manifest.description() {
                Some(what) if !what.is_empty() => format!("rebuild of {id}: {what}"),
                _ => format!("rebuild of {id}"),
            });

    let targets = DevTargets {
        run: true,
        // The snapshot being rebuilt is the parent: that is the lineage, and
        // recording the booted one instead would invent a relationship.
        parent: Some(id.to_string()),
        from: Some(parent_image.clone()),
        for_release: Some(release.clone()),
        out: None,
        register: true,
        dry_run: false,
    };

    let new_id = register_build(
        store,
        &description,
        &prepared,
        &dest,
        &parent_image,
        Some(&release),
        &targets,
        &Provenance {
            kind: manifest::KIND_REBUILT,
            status: "active",
            tree: Some(&plan.tree),
            rebuilt_modules: &plan.modules,
        },
        json,
    )?;

    out["registered"] = serde_json::json!(new_id);
    if json {
        println!("{}", serde_json::to_string(&out)?);
    } else {
        println!();
        println!("Registered: {new_id}");
        println!("{id} was not modified; its module and image are as they were.");
        println!("`snaptune boot {new_id}` tests it once. `snaptune verify {new_id}` checks it.");
    }
    Ok(0)
}

/// Build a module and make it a bootable snapshot.
///
/// The shape is `dev --run --register` with a different module source and a
/// different default parent: where the dev loop always derives from a snapshot or an
/// image you name, this derives from the running kernel's own initramfs and can take
/// the module straight off the installed system.
#[allow(clippy::too_many_arguments)]
fn cmd_create(
    store: &Store,
    description: &str,
    tree: Option<&Path>,
    module: Option<&Path>,
    from: Option<&Path>,
    for_release: Option<&str>,
    dry_run: bool,
    json: bool,
) -> Result<i32> {
    // An explicit `--for` wins; otherwise this machine's kernel, which is the right
    // answer exactly when you are standing on the machine you are snapshotting —
    // and the wrong one when you are not, which is why it is overridable and why the
    // tree's own release is checked against it below.
    let release = match for_release {
        Some(release) => release.to_string(),
        None => dev::running_release().context(
            "cannot tell which kernel to snapshot: `uname -r` did not answer.\n\
             Pass --for <release> to name it.",
        )?,
    };

    // The image the snapshot derives from. The running kernel's own initramfs is
    // what currently boots, so replacing one member of it is the smallest honest
    // step from "what is running" to "what this snapshot is".
    let base = match from {
        Some(image) => image.to_path_buf(),
        None => dev::distro_initramfs(&store.paths.boot, &release).with_context(|| {
            format!(
                "no initramfs for {release} under {}.\n\
                 Pass --from <image> to choose the image the snapshot derives from.",
                store.paths.boot.display()
            )
        })?,
    };
    if !base.is_file() {
        anyhow::bail!("the image to derive from is not there: {}", base.display());
    }

    // Where the module comes from, decided before anything is printed so the plan
    // can say which of the three it is.
    let source = match (tree, module) {
        (Some(path), _) => ModuleSource::Tree(dev::plan(
            dev::inspect_tree(path)?,
            // The release it must match is the one being recorded, not this
            // machine's: `--for` can point at another kernel deliberately.
            Some(release.clone()),
        )),
        (None, Some(path)) => ModuleSource::Adopted(path.to_path_buf()),
        (None, None) => {
            let installed = dev::installed_module(&release).with_context(|| {
                format!(
                    "no source tree given and no module installed for {release}.\n\
                     Pass --tree <dir> to build one, or --module <file> to supply one."
                )
            })?;
            ModuleSource::Adopted(installed)
        }
    };

    // One object on stdout and nothing else when `--json` is asked for, the same
    // contract the other verbs keep. Filled in as the run learns things.
    let mut out = serde_json::json!({
        "description": description,
        "release": release,
        "base_image": base,
        "module_source": source.kind(),
        "dry_run": dry_run,
    });

    if !json {
        println!("create plan — {description}");
        println!("  release     {release}");
        println!("  derives     {}", base.display());
        match &source {
            ModuleSource::Tree(_) => println!("  module      built from the tree below"),
            ModuleSource::Adopted(path) => {
                println!("  module      {} (adopted byte-for-byte)", path.display());
            }
        }
        if let ModuleSource::Tree(plan) = &source {
            for line in plan.describe() {
                println!("{line}");
            }
        }
    }

    if let ModuleSource::Tree(plan) = &source {
        if !plan.can_build() {
            if json {
                out["buildable"] = serde_json::json!(false);
                out["blockers"] = serde_json::json!(plan.blockers);
                println!("{}", serde_json::to_string(&out)?);
            } else {
                println!("\nNothing was built and nothing was written.");
            }
            return Ok(2);
        }
    }

    if dry_run {
        if json {
            out["built"] = serde_json::json!(false);
            println!("{}", serde_json::to_string(&out)?);
        } else {
            println!("\n--dry-run: nothing was built and nothing was written.");
        }
        return Ok(0);
    }

    let work = std::env::temp_dir().join(format!("snapcreate-{}", std::process::id()));
    let (prepared, built_tree, rebuilt_modules) = match &source {
        ModuleSource::Tree(plan) => {
            // Root is needed for the store and /boot writes, but the build must not
            // be root's or `make` leaves root-owned objects in the developer's tree.
            let runner = match dev::build_as(&plan.tree.path) {
                Some((uid, gid)) => {
                    std::fs::create_dir_all(&work)
                        .with_context(|| format!("creating scratch space {}", work.display()))?;
                    dev::hand_over(&work, uid, gid)?;
                    write::System::as_user(uid, gid)
                }
                None => write::System::default(),
            };
            let prepared = dev::build(&plan.tree, &runner, &work)?;
            (prepared, Some(&plan.tree), plan.modules.clone())
        }
        ModuleSource::Adopted(path) => (
            dev::adopt_module(path, &write::System::default(), &work)?,
            None,
            Vec::new(),
        ),
    };
    out["module_md5"] = serde_json::json!(prepared.md5);
    out["module_bytes"] = serde_json::json!(prepared.bytes.len());
    if !json {
        println!(
            "module      {} ({} bytes compressed)",
            prepared.md5,
            prepared.bytes.len()
        );
    }

    let dest = work.join("created.img");
    let (member, _derived) = dev::assemble(&base, &prepared, &dest, &work)?;
    // The independent oracle, not our own arithmetic: this is the check that
    // separates "the image carries the module we asked for" from "the image looks
    // plausible".
    let got = image::verify_member(&dest, &member).with_context(|| {
        format!(
            "verifying {member} in the image derived from {}",
            base.display()
        )
    })?;
    if got != prepared.bytes {
        anyhow::bail!(
            "the derived image does not carry the module we prepared \
             ({} bytes in the image, {} bytes prepared) — refusing to register it",
            got.len(),
            prepared.bytes.len()
        );
    }
    out["member"] = serde_json::json!(member);
    out["image"] = serde_json::json!(dest);
    out["verified"] = serde_json::json!(true);
    if !json {
        println!("replaced    {member}");
        println!("wrote       {}", dest.display());
    }

    // The shell marks the first snapshot a store ever holds as its baseline. A store
    // with no marker is one whose baseline is undecided, and `list` shows the column.
    let golden = store.golden_id().is_none();
    let status = if golden {
        register::STATUS_GOLDEN
    } else {
        register::STATUS_ACTIVE
    };
    if golden && !json {
        println!("golden      this is the first snapshot in the store, so it becomes the baseline");
    }

    let targets = DevTargets {
        run: true,
        // No `--parent`: the lineage of a created snapshot is the image it was
        // derived from, which `register_build` reads off that image's name.
        parent: None,
        from: Some(base.clone()),
        for_release: Some(release.clone()),
        out: None,
        register: true,
        dry_run: false,
    };

    let id = register_build(
        store,
        description,
        &prepared,
        &dest,
        &base,
        Some(&release),
        &targets,
        &Provenance {
            kind: manifest::KIND_CREATED,
            status,
            tree: built_tree,
            rebuilt_modules: &rebuilt_modules,
        },
        json,
    )?;

    out["golden"] = serde_json::json!(golden);
    out["registered"] = serde_json::json!(id);
    if json {
        println!("{}", serde_json::to_string(&out)?);
    } else {
        println!();
        println!("Created: {id}");
        println!("This is NOT the GRUB default.");
        println!("  `snaptune boot {id}`   tests it once, reverting by itself");
        println!("  `snaptune verify {id}` checks it against what is on disk");
        println!("  `snaptune pin {id}`    makes it the permanent default");
    }
    Ok(0)
}

/// Where a created snapshot's module came from.
enum ModuleSource {
    /// Built from a source tree, with the plan that says what it will build.
    Tree(dev::DevPlan),
    /// Taken as it stands: the installed module, or one named with `--module`.
    Adopted(PathBuf),
}

impl ModuleSource {
    /// The value `--json` reports. A stable word rather than the path, because what a
    /// caller needs to know is which of the three routes was taken.
    fn kind(&self) -> &'static str {
        match self {
            ModuleSource::Tree(_) => "tree",
            ModuleSource::Adopted(_) => "adopted",
        }
    }
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

    fn parse_dev(args: &[&str]) -> Result<Cmd, clap::Error> {
        let mut argv = vec!["snaptune", "dev", "an iteration"];
        argv.extend_from_slice(args);
        Cli::try_parse_from(argv).map(|cli| cli.cmd)
    }

    /// Registering is only meaningful for a build that actually ran: promoting an
    /// image that was never built would register the parent under a new name.
    #[test]
    fn register_requires_run() {
        assert!(
            parse_dev(&["--register"]).is_err(),
            "--register without --run must be refused, not silently ignored"
        );
        assert!(
            parse_dev(&["--run", "--register"]).is_ok(),
            "--run --register is the shape that works"
        );
    }

    /// Arming and rebooting are separate, because arming is useful on its own: the
    /// one-shot stays armed until the machine actually goes down.
    #[test]
    fn boot_arms_by_default_and_reboots_only_when_asked() {
        let cmd = Cli::try_parse_from(["snaptune", "boot", "snap-aaaaaaaa"]).expect("parse");
        match cmd.cmd {
            Cmd::Boot {
                id,
                dry_run,
                reboot,
            } => {
                assert_eq!(id, "snap-aaaaaaaa");
                assert!(
                    !dry_run,
                    "arming is the point; it is not a dry run by default"
                );
                assert!(!reboot, "and the machine must not go down unless asked");
            }
            other => panic!("expected boot, got {other:?}"),
        }

        let cmd =
            Cli::try_parse_from(["snaptune", "boot", "snap-aaaaaaaa", "--reboot", "--dry-run"])
                .expect("parse");
        assert!(matches!(
            cmd.cmd,
            Cmd::Boot {
                reboot: true,
                dry_run: true,
                ..
            }
        ));

        // The alias, spelled the way someone would guess it.
        assert!(Cli::try_parse_from(["snaptune", "reboot-into", "snap-aaaaaaaa"]).is_ok());
    }

    /// A dry run has to be expressible without also building, so the plan can be
    /// reviewed before anything takes ten minutes.
    #[test]
    fn a_dev_dry_run_parses() {
        let cmd = parse_dev(&["--run", "--register", "--dry-run"]).expect("parse");
        match cmd {
            Cmd::Dev {
                register, dry_run, ..
            } => {
                assert!(register && dry_run);
            }
            other => panic!("expected dev, got {other:?}"),
        }
    }

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

    // ---- rebuild ---------------------------------------------------------

    /// A store holding one snapshot, in a root of its own. The root is not named
    /// `snap-*`: the snapshots live inside it, and `discover` looks for `snap-*`
    /// entries *under* the root it is given.
    fn store_with(tag: &str, id: &str, manifest: &str, boot_file: bool) -> Store {
        let dir = std::env::temp_dir().join(format!("snapcli-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(id).join("modules")).unwrap();
        std::fs::create_dir_all(dir.join("boot")).unwrap();
        std::fs::write(dir.join(id).join("manifest.yaml"), manifest).unwrap();
        if boot_file {
            std::fs::write(
                dir.join("boot").join(format!("initramfs-{id}.img")),
                b"image",
            )
            .unwrap();
        }
        Store::new(Paths::under(&dir))
    }

    /// No `source_tree.path`: the shape of a snapshot taken of the running module.
    fn manifest_no_tree(id: &str) -> String {
        format!(
            "snapshot:\n  id: \"{id}\"\n  description: \"fixture\"\n  status: \"active\"\n\
             kernel:\n  version: \"7.2.9-1-cachyos-bore\"\n\
             module:\n  md5: \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\n\
             source_tree:\n  mode: \"running-module\"\n"
        )
    }

    fn manifest_with_tree(id: &str, tree: &Path, initramfs: &Path) -> String {
        format!(
            "snapshot:\n  id: \"{id}\"\n  description: \"fixture\"\n  status: \"active\"\n\
             kernel:\n  version: \"7.2.9-1-cachyos-bore\"\n\
             module:\n  md5: \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\n\
             source_tree:\n  path: \"{}\"\n  mode: \"built-from-source\"\n\
             boot_files:\n  initramfs: \"{}\"\n",
            tree.display(),
            initramfs.display()
        )
    }

    /// An unknown id is reported as such, and is not silently treated as an empty
    /// store — which would go on to build for a kernel nobody named.
    #[test]
    fn rebuild_reports_an_unknown_snapshot() {
        let store = store_with(
            "unknown",
            "snap-aaaaaaaa",
            &manifest_no_tree("snap-aaaaaaaa"),
            false,
        );
        let rc = cmd_rebuild(&store, "snap-bbbbbbbb", None, None, false, false).unwrap();
        assert_eq!(rc, 1);
    }

    /// A snapshot taken of the running module records no tree, so there is nothing
    /// to rebuild from. Guessing one would build a module for a kernel the caller
    /// never asked about, and the error has to say how to supply it.
    #[test]
    fn rebuild_refuses_a_snapshot_that_records_no_tree() {
        let store = store_with(
            "notree",
            "snap-aaaaaaaa",
            &manifest_no_tree("snap-aaaaaaaa"),
            false,
        );
        let err = cmd_rebuild(&store, "snap-aaaaaaaa", None, None, false, false).unwrap_err();
        let text = format!("{err}");
        assert!(text.contains("does not record a source tree"), "{text}");
        assert!(text.contains("--tree"), "it must say how to fix it: {text}");
    }

    /// `--tree` is honoured even when the manifest records one, because a checkout
    /// that has moved is the normal reason to be passing it.
    #[test]
    fn rebuild_refuses_a_tree_that_is_not_there() {
        let store = store_with(
            "gone",
            "snap-aaaaaaaa",
            &manifest_no_tree("snap-aaaaaaaa"),
            false,
        );
        let missing = std::env::temp_dir().join("snapcli-no-such-tree");
        let _ = std::fs::remove_dir_all(&missing);
        let err =
            cmd_rebuild(&store, "snap-aaaaaaaa", Some(&missing), None, false, false).unwrap_err();
        let text = format!("{err}");
        assert!(text.contains("not there"), "{text}");
        assert!(text.contains("Nothing was built"), "{text}");
    }

    /// A tree that cannot produce a module stops the verb *before* anything is
    /// written: the store must be exactly as it was, or a refusal is not a refusal.
    #[test]
    fn rebuild_stops_before_writing_when_the_tree_cannot_build() {
        let tree = std::env::temp_dir().join(format!("snapcli-baretree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tree);
        std::fs::create_dir_all(&tree).unwrap();

        let id = "snap-aaaaaaaa";
        // The store root is made first so the initramfs path the manifest records
        // is a real file: a rebuild derives from *this* snapshot's own image, and
        // naming one that is not there is a different refusal.
        let boot = std::env::temp_dir().join(format!("snapcli-bareboot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&boot);
        std::fs::create_dir_all(&boot).unwrap();
        let initramfs = boot.join(format!("initramfs-{id}.img"));
        std::fs::write(&initramfs, b"image").unwrap();

        let store = store_with(
            "bare",
            id,
            &manifest_with_tree(id, &tree, &initramfs),
            false,
        );
        let before = std::fs::read_dir(&store.paths.root).unwrap().count();

        // Configured, but for a different kernel: the vermagic gate has to stop
        // this before the module is built, because a module the target kernel
        // refuses is worse than no module — it looks like a snapshot until boot.
        std::fs::write(tree.join(".config"), b"CONFIG_X=y\n").unwrap();
        std::fs::create_dir_all(tree.join("include/config")).unwrap();
        std::fs::write(tree.join("include/config/kernel.release"), b"9.9.9-nope\n").unwrap();

        let rc = cmd_rebuild(&store, id, Some(&tree), None, false, false).unwrap();

        assert_eq!(rc, 2, "a tree built for another kernel is a refusal");
        let after = std::fs::read_dir(&store.paths.root).unwrap().count();
        assert_eq!(after, before, "a refusal must not write anything");
        let _ = std::fs::remove_dir_all(&store.paths.root);
        let _ = std::fs::remove_dir_all(&tree);
        let _ = std::fs::remove_dir_all(&boot);
    }

    /// A tree with no `.config` cannot produce a module, and finding that out from
    /// a failed `make` half a minute in is worse than being told now. It must also
    /// still write nothing.
    #[test]
    fn rebuild_reports_an_unconfigured_tree_without_writing() {
        let tree = std::env::temp_dir().join(format!("snapcli-noconfig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tree);
        std::fs::create_dir_all(&tree).unwrap();

        let id = "snap-aaaaaaaa";
        let boot = std::env::temp_dir().join(format!("snapcli-nc-boot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&boot);
        std::fs::create_dir_all(&boot).unwrap();
        let initramfs = boot.join(format!("initramfs-{id}.img"));
        std::fs::write(&initramfs, b"image").unwrap();

        let store = store_with(
            "noconfig",
            id,
            &manifest_with_tree(id, &tree, &initramfs),
            false,
        );
        let before = std::fs::read_dir(&store.paths.root).unwrap().count();

        let err = cmd_rebuild(&store, id, Some(&tree), None, false, false).unwrap_err();
        let text = format!("{err}");
        assert!(text.contains(".config"), "{text}");

        let after = std::fs::read_dir(&store.paths.root).unwrap().count();
        assert_eq!(after, before, "a refusal must not write anything");
        let _ = std::fs::remove_dir_all(&store.paths.root);
        let _ = std::fs::remove_dir_all(&tree);
        let _ = std::fs::remove_dir_all(&boot);
    }

    /// The flags parse, including `SNAPSHOT_TREE` as the tree, matching `dev`.
    #[test]
    fn rebuild_parses_the_tree_from_the_flag_or_the_environment() {
        let cmd = Cli::try_parse_from(["snaptune", "rebuild", "snap-0848359a", "--dry-run"])
            .expect("parse");
        match cmd.cmd {
            Cmd::Rebuild {
                id,
                tree,
                description,
                dry_run,
            } => {
                assert_eq!(id, "snap-0848359a");
                assert_eq!(tree, None);
                assert_eq!(description, None);
                assert!(dry_run);
            }
            other => panic!("expected rebuild, got {other:?}"),
        }
    }

    // ---- create ----------------------------------------------------------

    /// `--module` and `--tree` are two answers to one question, and silently
    /// preferring one would build a module the caller did not ask for.
    #[test]
    fn create_refuses_both_a_tree_and_an_explicit_module() {
        assert!(
            Cli::try_parse_from([
                "snaptune",
                "create",
                "desc",
                "--tree",
                "/tree",
                "--module",
                "/m.ko.zst"
            ])
            .is_err(),
            "--tree with --module must be refused, not resolved by precedence"
        );
    }

    #[test]
    fn create_parses_a_description_and_its_sources() {
        let cmd = Cli::try_parse_from([
            "snaptune",
            "create",
            "golden-base: patches 1-3",
            "--from",
            "/boot/initramfs-x.img",
            "--for",
            "7.2.9-1-cachyos-bore",
            "--dry-run",
        ])
        .expect("parse");
        match cmd.cmd {
            Cmd::Create {
                description,
                tree,
                module,
                from,
                for_release,
                dry_run,
            } => {
                assert_eq!(description, "golden-base: patches 1-3");
                assert_eq!(tree, None);
                assert_eq!(module, None);
                assert_eq!(from, Some(PathBuf::from("/boot/initramfs-x.img")));
                assert_eq!(for_release.as_deref(), Some("7.2.9-1-cachyos-bore"));
                assert!(dry_run);
            }
            other => panic!("expected create, got {other:?}"),
        }
    }

    /// A store whose baseline is already decided must not have it reassigned by the
    /// next snapshot created, which is what `create` keys its `status` off.
    #[test]
    fn create_only_claims_golden_when_the_store_has_no_baseline() {
        let dir = std::env::temp_dir().join(format!("snapcli-gold-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let store = Store::new(Paths::under(&dir));
        assert!(
            store.golden_id().is_none(),
            "an empty store has no baseline"
        );

        std::fs::write(dir.join(crate::store::GOLDEN_MARKER), "snap-aaaaaaaa\n").unwrap();
        assert_eq!(store.golden_id().as_deref(), Some("snap-aaaaaaaa"));
        assert!(
            !store.golden_id().unwrap().is_empty(),
            "a marker that reads back empty would silently re-claim golden"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An image that is not there is refused before anything is built, and the
    /// refusal names the file it wanted.
    #[test]
    fn create_refuses_a_base_image_that_is_not_there() {
        let dir = std::env::temp_dir().join(format!("snapcli-nobase-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::new(Paths::under(&dir));

        let missing = dir.join("initramfs-nope.img");
        let err = cmd_create(
            &store,
            "desc",
            None,
            Some(&dir.join("module.ko.zst")),
            Some(&missing),
            Some("7.2.9-1-cachyos-bore"),
            false,
            false,
        )
        .unwrap_err();
        let text = format!("{err}");
        assert!(text.contains("not there"), "{text}");
        assert!(text.contains("initramfs-nope.img"), "{text}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With no tree and no installed module for the release there is nothing to
    /// snapshot, and the error has to name both ways out.
    #[test]
    fn create_refuses_when_there_is_no_module_to_take() {
        let dir = std::env::temp_dir().join(format!("snapcli-nomod-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("initramfs-x.img");
        std::fs::write(&base, b"image").unwrap();
        let store = Store::new(Paths::under(&dir));

        // A release that certainly has no modules installed.
        let err = cmd_create(
            &store,
            "desc",
            None,
            None,
            Some(&base),
            Some("9.9.9-no-such-kernel"),
            false,
            false,
        )
        .unwrap_err();
        let text = format!("{err}");
        assert!(text.contains("no module installed"), "{text}");
        assert!(text.contains("--tree"), "{text}");
        assert!(text.contains("--module"), "{text}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `create` writes into the store and `/boot`, so a dry run has to be able to
    /// show the plan without any of that. The tree here cannot build, which makes
    /// this the refusal path — and a refusal must not write either.
    #[test]
    fn create_dry_run_writes_nothing_into_the_store() {
        let dir = std::env::temp_dir().join(format!("snapcli-dry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("store")).unwrap();
        let store = Store::new(Paths::under(dir.join("store")));

        let tree = dir.join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join(".config"), b"CONFIG_X=y\n").unwrap();

        let rc = cmd_create(
            &store,
            "desc",
            Some(&tree),
            None,
            Some(&dir.join("whatever.img")),
            Some("7.2.9-1-cachyos-bore"),
            true,
            false,
        );

        assert!(
            matches!(rc, Err(_) | Ok(2)),
            "an unbuildable tree is a refusal: {rc:?}"
        );
        let entries = std::fs::read_dir(dir.join("store")).unwrap().count();
        assert_eq!(entries, 0, "nothing may be written into the store");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
