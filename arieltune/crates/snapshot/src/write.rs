// SPDX-License-Identifier: GPL-2.0-only
//! The write path: pin, unpin, hide, unhide.
//!
//! ## Plan, then apply
//!
//! Unlike the shell tool — which edits files as it goes, with `sed` ranges and a
//! `mv` in the middle — every operation here is split in two:
//!
//! 1. [`State::read`] gathers the facts (what is booted, pinned, golden, which
//!    manifests exist, what the GRUB files currently say);
//! 2. a pure `plan_*` function turns those facts into a [`Plan`] of [`Action`]s;
//! 3. [`apply`] executes the plan, or prints it under `--dry-run`.
//!
//! That buys three things the shell version cannot offer: the guards are
//! testable without a blade, `--dry-run` is free and exact, and a refusal is
//! decided *before* anything has been written rather than half-way through.
//!
//! ## The guards are not decoration
//!
//! Removing a snapshot's menu entry while it is the booted kernel, the golden
//! image, or the pinned `saved_entry` leaves GRUB unable to resolve
//! `saved_entry=snap-<gone>`; it falls back to entry 0 and boots a different
//! kernel. The shell tool gained these guards only on 2026-10-09 — `hide` could
//! do all three before that. They are reproduced here deliberately.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::grub;
use crate::store::{Paths, Snapshot, Store};

/// One thing to do. Deliberately data, not a closure, so a plan can be printed
/// before it is applied and asserted in a test without touching a disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Replace a file's contents.
    Write {
        path: PathBuf,
        contents: String,
        what: String,
    },
    /// Move a file (the hide/unhide mechanism).
    Rename {
        from: PathBuf,
        to: PathBuf,
        what: String,
    },
    /// Create a directory, including any missing parents.
    Mkdir { path: PathBuf, what: String },
    /// Copy a file.
    ///
    /// Separate from [`Action::Write`] because the artefacts a snapshot owns —
    /// the compressed module, the initramfs, the kernel — are tens of megabytes
    /// each, and holding them as `contents` would mean a plan carried an entire
    /// snapshot in memory just to describe it.
    Copy {
        from: PathBuf,
        to: PathBuf,
        what: String,
    },
    /// Run a program. `argv[0]` is the program.
    Run { argv: Vec<String>, what: String },
    /// Delete a file, or a whole directory tree.
    ///
    /// **Tolerant of absence**, like `rm -f`. The paths are constructed from the
    /// snapshot id rather than typed, so a missing one is normal — a snapshot whose
    /// artefacts were partly removed by hand, or a prune re-run after a failure
    /// part-way through. Refusing there would strand a half-deleted snapshot that
    /// nothing could then clean up.
    Remove { path: PathBuf, what: String },
}

/// An ordered set of actions, plus why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub summary: String,
    pub actions: Vec<Action>,
}

impl Plan {
    /// Human-readable lines, one per action. This is what `--dry-run` prints.
    pub fn describe(&self) -> Vec<String> {
        self.actions
            .iter()
            .map(|action| match action {
                Action::Write { path, what, .. } => {
                    format!("write  {} ({})", path.display(), what)
                }
                Action::Rename { from, to, what } => {
                    format!("rename {} -> {} ({})", from.display(), to.display(), what)
                }
                Action::Mkdir { path, .. } => format!("mkdir  {}", path.display()),
                Action::Copy { from, to, .. } => {
                    format!("copy   {} -> {}", from.display(), to.display())
                }
                Action::Run { argv, what } => format!("run    {} ({})", argv.join(" "), what),
                Action::Remove { path, what } => {
                    format!("remove {} ({})", path.display(), what)
                }
            })
            .collect()
    }
}

/// Everything the planners need to know, read once.
#[derive(Debug, Clone)]
pub struct State {
    pub booted: Option<String>,
    pub golden: Option<String>,
    pub pinned: Option<String>,
    pub snapshots: Vec<Snapshot>,
    pub custom: String,
    pub grub_defaults: String,
    /// `/etc/default/grub` path, so the planner can name it in an action.
    pub grub_defaults_path: PathBuf,
    /// Whether `/boot/acpi_override.cpio` exists (it is chained into the initrd).
    pub acpi_override: bool,
}

impl State {
    /// Read the live state of a blade.
    pub fn read(paths: &Paths) -> Result<State> {
        let store = Store::new(paths.clone());
        Ok(State {
            booted: store.booted_id(),
            golden: store.golden_id(),
            pinned: saved_entry(),
            snapshots: store.discover()?,
            custom: std::fs::read_to_string(&paths.grub_custom).unwrap_or_default(),
            grub_defaults: std::fs::read_to_string(&paths.grub_defaults).unwrap_or_default(),
            grub_defaults_path: paths.grub_defaults.clone(),
            acpi_override: paths.boot.join("acpi_override.cpio").is_file(),
        })
    }

    fn find(&self, id: &str) -> Option<&Snapshot> {
        self.snapshots.iter().find(|s| s.id == id)
    }
}

/// `saved_entry` from grubenv, via `grub-editenv` rather than a hand-written
/// parser: grubenv carries a checksum and is not ours to reinterpret.
pub fn saved_entry() -> Option<String> {
    grubenv_value("saved_entry=")
}

/// `next_entry` from grubenv — the one-shot the bootloader consumes and clears.
pub fn next_entry() -> Option<String> {
    grubenv_value("next_entry=")
}

/// One `key=value` out of grubenv.
fn grubenv_value(prefix: &str) -> Option<String> {
    let out = std::process::Command::new("grub-editenv")
        .arg("list")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|line| line.strip_prefix(prefix).map(str::to_string))
}

/// Confirm GRUB will actually honour an armed one-shot boot.
///
/// `grub-reboot` exits 0 and can still change nothing: `next_entry` is only
/// consulted when `grub.cfg` was generated with `GRUB_DEFAULT=saved`. Reading the
/// value back is the only way to tell "armed" from "written to a file nothing
/// reads" — and the difference is a reboot into a kernel nobody asked for, on a
/// machine that may only be reachable over the network.
pub fn verify_boot(id: &str) -> Result<()> {
    let now = next_entry();
    if now.as_deref() != Some(id) {
        bail!(
            "the one-shot boot did not take: grubenv has next_entry={}, expected {id}.\n\
             GRUB only consults next_entry when GRUB_DEFAULT=saved. Check that in \
             /etc/default/grub and that grub-mkconfig has run since.",
            now.as_deref().unwrap_or("<unset>")
        );
    }
    Ok(())
}

/// Refuse the three combinations that break a pin. See the module docs.
fn guard_removable(state: &State, id: &str, verb: &str) -> Result<()> {
    if state.booted.as_deref() == Some(id) {
        bail!(
            "cannot {verb} {id}: it is the booted snapshot. Boot another one first.\n\
             Removing its entry would leave the running kernel unbootable."
        );
    }
    if state.golden.as_deref() == Some(id) {
        bail!(
            "cannot {verb} {id}: it is the golden image.\n\
             Remove /var/lib/kernel-snapshots/.golden-snapshot first if you really mean it."
        );
    }
    if state.pinned.as_deref() == Some(id) {
        bail!(
            "cannot {verb} {id}: it is the pinned GRUB default.\n\
             GRUB resolves saved_entry to its menu entry; removing that entry makes GRUB fall\n\
             back to entry 0 and boot a different kernel. Pin another snapshot, or unpin first."
        );
    }
    Ok(())
}

/// The `/etc/default/grub` edits `pin` needs: a saved default scheme, and the
/// distro's submenus left alone.
///
/// Returns the new contents only when something actually changes, so a plan does
/// not rewrite an unchanged file.
fn grub_defaults_for_pin(current: &str) -> Option<String> {
    let mut text = current.to_string();
    let mut changed = false;

    // GRUB_DEFAULT=saved, or grub-set-default silently does nothing.
    let has_saved = text
        .lines()
        .any(|l| l.trim_start().starts_with("GRUB_DEFAULT=saved"));
    if !has_saved {
        if text
            .lines()
            .any(|l| l.trim_start().starts_with("GRUB_DEFAULT="))
        {
            text = text
                .lines()
                .map(|l| {
                    if l.trim_start().starts_with("GRUB_DEFAULT=") {
                        "GRUB_DEFAULT=saved".to_string()
                    } else {
                        l.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
        } else {
            text.push_str("\nGRUB_DEFAULT=saved\n");
        }
        text.push('\n');
        changed = true;
    }

    // An earlier version of the shell tool forced GRUB_DISABLE_SUBMENU=true on
    // every pin, which promoted the generated per-snapshot entries out of the
    // submenu that hid them. Undo that, unless the operator asks for flat.
    let keep_flat = std::env::var("SNAPSHOT_KEEP_FLAT").as_deref() == Ok("1");
    if !keep_flat
        && text.lines().any(|l| {
            let t = l.trim_start();
            t.starts_with("GRUB_DISABLE_SUBMENU=")
                && (t.ends_with("true") || t.ends_with("yes") || t.ends_with("y"))
        })
    {
        text = text
            .lines()
            .map(|l| {
                let t = l.trim_start();
                if t.starts_with("GRUB_DISABLE_SUBMENU=") {
                    "GRUB_DISABLE_SUBMENU='false'".to_string()
                } else {
                    l.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        text.push('\n');
        changed = true;
    }

    changed.then_some(text)
}

/// `grub-mkconfig -o <grub.cfg>`, the regeneration every menu change needs.
fn regen_action(paths: &Paths) -> Action {
    Action::Run {
        argv: vec![
            "grub-mkconfig".to_string(),
            "-o".to_string(),
            paths.grub_cfg.display().to_string(),
        ],
        what: "regenerate the GRUB menu".to_string(),
    }
}

/// Pin a snapshot as the permanent GRUB default.
pub fn plan_pin(paths: &Paths, state: &State, id: &str) -> Result<Plan> {
    let Some(snapshot) = state.find(id) else {
        bail!("snapshot {id} not found");
    };
    if snapshot.hidden {
        bail!(
            "cannot pin {id}: it is hidden. Unhide it first — a hidden snapshot has no menu entry."
        );
    }
    if !snapshot.boot_artifacts_complete() {
        bail!(
            "cannot pin {id}: its boot artefacts are incomplete (vmlinuz {}, initramfs {}, cmdline {}).",
            yn(snapshot.vmlinuz),
            yn(snapshot.initramfs),
            yn(snapshot.cmdline_file)
        );
    }
    if !snapshot.grub_entry {
        bail!(
            "cannot pin {id}: it has no block in {} — regenerate the entry first.",
            paths.grub_custom.display()
        );
    }

    let mut actions = Vec::new();
    if let Some(contents) = grub_defaults_for_pin(&state.grub_defaults) {
        actions.push(Action::Write {
            path: state.grub_defaults_path.clone(),
            contents,
            what: "use the saved-default scheme (GRUB_DEFAULT=saved)".to_string(),
        });
    }
    actions.push(Action::Run {
        argv: vec!["grub-set-default".to_string(), id.to_string()],
        what: format!("set saved_entry={id}"),
    });
    actions.push(regen_action(paths));

    Ok(Plan {
        summary: format!("pin {id} as the permanent GRUB default"),
        actions,
    })
}

/// Mark a snapshot as the golden image.
///
/// The marker is a file in the store rather than a GRUB change, but it is not
/// cosmetic: [`guard_removable`] refuses to hide or prune whatever it names, so
/// this moves the protection from one snapshot to another and is worth a plan.
pub fn plan_golden(paths: &Paths, state: &State, id: &str) -> Result<Plan> {
    let Some(_snapshot) = state.find(id) else {
        bail!("snapshot {id} not found");
    };
    // Read the marker the same way `guard_removable` does, rather than the
    // per-row flag: the two are derived from one file, and a planner that
    // disagreed with the guard would be a trap.
    if state.golden.as_deref() == Some(id) {
        bail!("{id} is already the golden image");
    }
    Ok(Plan {
        summary: format!("mark {id} as the golden image"),
        actions: vec![Action::Write {
            path: paths.root.join(".golden-snapshot"),
            contents: format!("{id}\n"),
            what: "the golden marker, which hide and prune then refuse to remove".to_string(),
        }],
    })
}

/// Revert to the distro kernel by clearing the pinned default.
pub fn plan_unpin(paths: &Paths) -> Result<Plan> {
    Ok(Plan {
        summary: "unpin: revert the GRUB default to entry 0".to_string(),
        actions: vec![
            Action::Run {
                argv: vec!["grub-set-default".to_string(), "0".to_string()],
                what: "clear saved_entry".to_string(),
            },
            regen_action(paths),
        ],
    })
}

/// Arm a one-shot boot into a snapshot: the next boot uses it, the one after does
/// not.
///
/// **One-shot on purpose.** This writes GRUB's `next_entry`, which the bootloader
/// consumes and clears, so a snapshot that will not come up falls back to the pinned
/// default by itself. Nothing here can leave a blade stuck booting a bad kernel —
/// which is the difference between this and changing `saved_entry`, and the reason
/// the reboot key in the pane is safe to make the default answer.
///
/// The plan deliberately arms and stops. Rebooting is the shell's job: it tears the
/// terminal down first, so the machine does not go down with the TUI still holding
/// the alternate screen.
///
/// No `Paths` argument: arming writes grubenv, which GRUB reads at boot, so nothing
/// in `/boot` or `grub.cfg` needs regenerating — unlike every other writer here.
pub fn plan_boot(state: &State, id: &str) -> Result<Plan> {
    let Some(snapshot) = state.find(id) else {
        bail!("snapshot {id} not found");
    };

    // Both of these produce a boot that GRUB cannot complete, and it would fail at
    // the menu rather than here — on a machine you may only reach over the network.
    if !snapshot.grub_entry {
        bail!(
            "cannot boot {id}: it has no GRUB menu entry.\n\
             Run `unhide` if it is hidden, or `verify` to see what is missing."
        );
    }
    if !snapshot.vmlinuz || !snapshot.initramfs {
        let missing = match (snapshot.vmlinuz, snapshot.initramfs) {
            (false, false) => "its kernel and its initramfs",
            (false, true) => "its kernel",
            _ => "its initramfs",
        };
        bail!(
            "cannot boot {id}: {missing} is missing from /boot.\n\
             The menu entry is there, so GRUB would offer a boot it cannot finish."
        );
    }

    Ok(Plan {
        summary: format!("boot {id} on the next boot only, then reboot now"),
        actions: vec![Action::Run {
            argv: vec!["grub-reboot".to_string(), id.to_string()],
            what: format!("arm a one-shot boot into {id} (the boot after falls back)"),
        }],
    })
}

/// Take a snapshot off the list and off the menu. Reversible: `unhide` rebuilds.
pub fn plan_hide(paths: &Paths, state: &State, id: &str) -> Result<Plan> {
    let Some(snapshot) = state.find(id) else {
        bail!("snapshot {id} not found");
    };
    if snapshot.hidden {
        bail!("{id} is already hidden");
    }
    guard_removable(state, id, "hide")?;

    let dir = paths.root.join(id);
    let mut actions = vec![Action::Rename {
        from: dir.join("manifest.yaml"),
        to: dir.join("manifest.yaml.hidden"),
        what: "hide the manifest (data preserved)".to_string(),
    }];

    let (pruned, removed) = grub::remove_entry(&state.custom, id);
    if removed {
        actions.push(Action::Write {
            path: paths.grub_custom.clone(),
            contents: pruned,
            what: "remove its menu entry".to_string(),
        });
        actions.push(regen_action(paths));
    }

    Ok(Plan {
        summary: format!("hide {id} (off the list and off the menu; data preserved)"),
        actions,
    })
}

/// Delete a snapshot completely: its boot artefacts, its directory, and its menu
/// entry.
///
/// **The order is not cosmetic.** `10_linux` generates a `gnulinux-<id>-advanced-…`
/// menu entry for *every* `/boot/vmlinuz-*`, so removing the tool's own entry from
/// `40_custom` is not enough to get a snapshot off the menu. The artefacts have to
/// be gone *before* `grub-mkconfig` runs, or the regenerated file keeps an entry
/// pointing at a kernel that no longer exists — which looks fine until GRUB tries
/// to load it. Doing exactly this by hand on blade 115 produced exactly that.
///
/// Irreversible, so it takes the same guards as removing a menu entry: the booted
/// snapshot is running, the golden one is there on purpose, and the pinned one is
/// the entry GRUB would fall back to.
pub fn plan_prune(paths: &Paths, state: &State, id: &str) -> Result<Plan> {
    if state.find(id).is_none() {
        bail!(
            "snapshot {id} not found.\n\
             `prune` deletes a snapshot; it cannot tidy up leftovers it does not \
             know about."
        );
    }
    guard_removable(state, id, "prune")?;

    let mut actions: Vec<Action> = [
        (paths.boot.join(format!("vmlinuz-{id}")), "its kernel"),
        (
            paths.boot.join(format!("initramfs-{id}.img")),
            "its initramfs",
        ),
        (paths.boot.join(format!("{id}.cmdline")), "its command line"),
        (paths.root.join(id), "its directory (manifest and module)"),
    ]
    .into_iter()
    .map(|(path, what)| Action::Remove {
        // Reported at plan time, so a dry run says which parts are actually there
        // rather than implying it will delete four things that may be three.
        what: if path.exists() {
            what.to_string()
        } else {
            format!("{what} — already gone")
        },
        path,
    })
    .collect();

    let (pruned, removed) = grub::remove_entry(&state.custom, id);
    if removed {
        actions.push(Action::Write {
            path: paths.grub_custom.clone(),
            contents: pruned,
            what: "remove its entry from 40_custom".to_string(),
        });
    }
    // Always regenerate, even when there was no `40_custom` entry to remove: the
    // distro's own generated entry for this kernel is still in grub.cfg, and only
    // a regeneration after the artefacts are gone will drop it.
    actions.push(regen_action(paths));

    Ok(Plan {
        summary: format!("prune {id} — delete its artefacts, its directory and its menu entry"),
        actions,
    })
}

/// Bring a hidden snapshot back, rebuilding its menu entry from the manifest.
pub fn plan_unhide(paths: &Paths, state: &State, id: &str) -> Result<Plan> {
    let Some(snapshot) = state.find(id) else {
        bail!("snapshot {id} not found");
    };
    if !snapshot.hidden {
        bail!("{id} is not hidden");
    }

    let dir = paths.root.join(id);
    let mut actions = vec![Action::Rename {
        from: dir.join("manifest.yaml.hidden"),
        to: dir.join("manifest.yaml"),
        what: "restore the manifest".to_string(),
    }];

    let manifest = snapshot
        .manifest
        .as_ref()
        .context("cannot rebuild the entry: no readable manifest")?;
    let entry = grub::entry_from_manifest(
        manifest,
        id,
        if state.acpi_override {
            Some(paths.boot.join("acpi_override.cpio"))
        } else {
            None
        },
    )?;
    actions.push(Action::Write {
        path: paths.grub_custom.clone(),
        contents: format!("{}{}", state.custom, entry),
        what: "rebuild its menu entry".to_string(),
    });
    actions.push(regen_action(paths));

    Ok(Plan {
        summary: format!("unhide {id} (menu entry rebuilt from the manifest)"),
        actions,
    })
}

fn yn(present: bool) -> &'static str {
    if present {
        "present"
    } else {
        "MISSING"
    }
}

/// Where a plan's `Run` actions go.
///
/// Exists so the write path can be exercised end to end without a test spawning
/// `grub-mkconfig` on whatever machine happens to be running the suite. The
/// alternative — building fixtures carefully so that no step has a command in it —
/// would make the tests lie about what real plans contain.
pub trait Runner {
    fn run(&self, argv: &[String]) -> Result<()>;
}

/// Runs commands for real.
#[derive(Debug, Default, Clone, Copy)]
pub struct System {
    /// Run commands as this user, when set.
    ///
    /// `dev --register` needs root for the writes under `/boot`, `/etc/grub.d` and
    /// the store — but the *build* must not be root's. `make` rewrites the objects
    /// whose inputs changed, and those objects would then belong to root, which
    /// breaks the next build the developer runs as themselves. Handing the build
    /// to the tree's owner is what keeps the tree usable.
    pub as_user: Option<(u32, u32)>,
}

impl System {
    /// A runner that executes as `uid`/`gid`.
    pub fn as_user(uid: u32, gid: u32) -> Self {
        Self {
            as_user: Some((uid, gid)),
        }
    }
}

impl Runner for System {
    fn run(&self, argv: &[String]) -> Result<()> {
        let (program, args) = argv.split_first().context("empty argv")?;
        let mut command = std::process::Command::new(program);
        command.args(args);
        if let Some((uid, gid)) = self.as_user {
            use std::os::unix::process::CommandExt;
            // gid before uid: once the uid has dropped, setting the group is no
            // longer permitted.
            command.gid(gid).uid(uid);
        }
        let out = command
            .output()
            .with_context(|| format!("running {program}"))?;
        if !out.status.success() {
            // Stop at the first failure: a half-applied plan is worse than a
            // refused one, and the caller sees which step it was.
            bail!(
                "{} failed: {}",
                argv.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }
}

/// Records commands instead of running them, for tests.
///
/// Cloneable and shared, so a test can hand one to the pane and still read back
/// what was issued.
#[derive(Debug, Default, Clone)]
pub struct Recording {
    pub calls: std::rc::Rc<std::cell::RefCell<Vec<Vec<String>>>>,
}

impl Runner for Recording {
    fn run(&self, argv: &[String]) -> Result<()> {
        self.calls.borrow_mut().push(argv.to_vec());
        Ok(())
    }
}

/// Execute a plan. With `dry_run`, print it and touch nothing.
pub fn apply(plan: &Plan, dry_run: bool) -> Result<()> {
    apply_with(plan, dry_run, &System::default())
}

/// Execute a plan, sending its commands to `runner`.
///
/// `quiet` suppresses the plan, and exists for one case: a verb that emits JSON on
/// stdout. The plan is narration about what is being done, and prose interleaved
/// with a JSON object makes the object unparseable — `snaptune --json create … | jq`
/// would fail on the summary line. It is a parameter rather than a move to stderr
/// because for `--dry-run` the plan *is* the result: redirecting it away from stdout
/// would break `snaptune prune <id> --dry-run > plan.txt`.
pub fn apply_with(plan: &Plan, dry_run: bool, runner: &dyn Runner) -> Result<()> {
    apply_with_mode(plan, dry_run, runner, false)
}

/// [`apply_with`], with the plan's printing under the caller's control.
pub fn apply_with_mode(plan: &Plan, dry_run: bool, runner: &dyn Runner, quiet: bool) -> Result<()> {
    if !quiet {
        println!("{}", plan.summary);
        for line in plan.describe() {
            println!("  {line}");
        }
    }
    if dry_run {
        if !quiet {
            println!("\n--dry-run: nothing was changed.");
        }
        return Ok(());
    }

    for action in &plan.actions {
        match action {
            Action::Write { path, contents, .. } => {
                std::fs::write(path, contents)
                    .with_context(|| format!("writing {}", path.display()))?;
            }
            Action::Rename { from, to, .. } => {
                std::fs::rename(from, to)
                    .with_context(|| format!("renaming {} to {}", from.display(), to.display()))?;
            }
            Action::Mkdir { path, .. } => {
                std::fs::create_dir_all(path)
                    .with_context(|| format!("creating {}", path.display()))?;
            }
            Action::Copy { from, to, .. } => {
                std::fs::copy(from, to)
                    .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
            }
            Action::Run { argv, .. } => runner.run(argv)?,
            Action::Remove { path, .. } => {
                // `symlink_metadata`, not `is_dir()`: the latter follows a symlink,
                // so a symlink to a directory would be walked and deleted instead of
                // removed. These paths are built from the snapshot id, but a build
                // tool that deletes the wrong thing is not a risk worth taking.
                match std::fs::symlink_metadata(path) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        return Err(e).with_context(|| format!("looking at {}", path.display()));
                    }
                    Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path)
                        .with_context(|| format!("removing {}", path.display()))?,
                    Ok(_) => std::fs::remove_file(path)
                        .with_context(|| format!("removing {}", path.display()))?,
                }
            }
        }
    }
    Ok(())
}

/// Confirm a pin actually took, the way the shell tool does — `grub-set-default`
/// can succeed while `GRUB_DEFAULT` (not `saved`) makes it inert.
pub fn verify_pin(id: &str) -> Result<()> {
    let now = saved_entry();
    if now.as_deref() != Some(id) {
        bail!(
            "pin did not take: grubenv has saved_entry={}, expected {id}.\n\
             Check that GRUB_DEFAULT=saved is in /etc/default/grub and that grub-mkconfig ran.",
            now.as_deref().unwrap_or("<unset>")
        );
    }
    Ok(())
}

/// Build a manifest for tests without touching a disk.
#[cfg(test)]
fn manifest_with(id: &str) -> crate::manifest::Manifest {
    crate::manifest::Manifest::parse(&format!(
            "snapshot:\n  id: \"{id}\"\n  description: \"test {id}\"\ngrub:\n  root_uuid: \"8387e4a0-b18f-4142-b61d-59fa9acc3865\"\nboot_files:\n  vmlinuz: \"/boot/vmlinuz-{id}\"\n  initramfs: \"/boot/initramfs-{id}.img\"\ncmdline: \"BOOT_IMAGE=/boot/vmlinuz-other root=UUID=8387e4a0 rw\"\n"
        ))
        .expect("parse")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> Paths {
        Paths::under(fixture_root())
    }

    /// A real, writable scratch root.
    ///
    /// `/fixture` was enough while every test only *planned*, because a plan
    /// names paths without touching them. The moment one applies a plan, a fake
    /// root fails the write for the wrong reason — so the planning tests would
    /// keep passing while the apply path was never really exercised.
    fn fixture_root() -> PathBuf {
        let dir = std::env::temp_dir().join("snapshot-write-fixture");
        std::fs::create_dir_all(dir.join("etc/default")).expect("scratch dir");
        dir
    }

    fn snapshot(id: &str, hidden: bool) -> Snapshot {
        Snapshot {
            id: id.to_string(),
            manifest: Some(manifest_with(id)),
            manifest_error: None,
            hidden,
            golden: false,
            booted: false,
            grub_entry: true,
            vmlinuz: true,
            initramfs: true,
            cmdline_file: true,
        }
    }

    fn state() -> State {
        State {
            booted: Some("snap-booted".to_string()),
            golden: Some("snap-golden".to_string()),
            pinned: Some("snap-pinned".to_string()),
            snapshots: vec![
                snapshot("snap-booted", false),
                snapshot("snap-golden", false),
                snapshot("snap-pinned", false),
                snapshot("snap-target", false),
                snapshot("snap-hidden", true),
            ],
            custom: grub::entry_from_manifest(&manifest_with("snap-target"), "snap-target", None)
                .expect("render the existing entry"),
            grub_defaults: "GRUB_DEFAULT=0\nGRUB_DISABLE_SUBMENU='false'\nGRUB_TIMEOUT=5\n"
                .to_string(),
            grub_defaults_path: fixture_root().join("etc/default/grub"),
            acpi_override: false,
        }
    }

    // ---- guards ------------------------------------------------------------

    #[test]
    fn hide_refuses_the_booted_snapshot() {
        let err = plan_hide(&paths(), &state(), "snap-booted").unwrap_err();
        assert!(err.to_string().contains("booted snapshot"), "{err}");
    }

    #[test]
    fn hide_refuses_the_golden_image() {
        let err = plan_hide(&paths(), &state(), "snap-golden").unwrap_err();
        assert!(err.to_string().contains("golden image"), "{err}");
    }

    #[test]
    fn hide_refuses_the_pinned_default() {
        let err = plan_hide(&paths(), &state(), "snap-pinned").unwrap_err();
        assert!(err.to_string().contains("pinned GRUB default"), "{err}");
        // ...and says the way out, because that is the actionable part.
        assert!(err.to_string().contains("unpin"), "{err}");
    }

    #[test]
    fn a_refusal_produces_no_actions() {
        // The point of planning first: refusing must not have written anything.
        assert!(plan_hide(&paths(), &state(), "snap-booted").is_err());
    }

    // ---- prune -------------------------------------------------------------

    #[test]
    fn prune_removes_every_artefact() {
        let plan = plan_prune(&paths(), &state(), "snap-target").expect("plans");
        let removed: Vec<String> = plan
            .actions
            .iter()
            .filter_map(|a| match a {
                Action::Remove { path, .. } => Some(path.display().to_string()),
                _ => None,
            })
            .collect();
        for expected in [
            "/boot/vmlinuz-snap-target",
            "/boot/initramfs-snap-target.img",
            "/boot/snap-target.cmdline",
        ] {
            assert!(
                removed.iter().any(|p| p.ends_with(expected)),
                "must delete {expected}: {removed:?}"
            );
        }
        // The snapshot's own directory too, or the module is left orphaned with
        // nothing pointing at it.
        let dir = paths().root.join("snap-target").display().to_string();
        assert!(
            removed.contains(&dir),
            "must delete the snapshot directory ({dir}): {removed:?}"
        );
    }

    /// The trap, and the reason this verb exists rather than a hand-written `rm`.
    ///
    /// `10_linux` writes a menu entry for every `/boot/vmlinuz-*`. Regenerating
    /// before the artefacts are gone leaves that entry pointing at a kernel that
    /// no longer exists — and it looks fine until GRUB tries to load it.
    #[test]
    fn prune_deletes_before_it_regenerates() {
        let plan = plan_prune(&paths(), &state(), "snap-target").expect("plans");

        let last_remove = plan
            .actions
            .iter()
            .rposition(|a| matches!(a, Action::Remove { .. }))
            .expect("removes the artefacts");
        let regen = plan
            .actions
            .iter()
            .position(|a| matches!(a, Action::Run { argv, .. } if argv[0] == "grub-mkconfig"))
            .expect("regenerates the menu");

        assert!(
            last_remove < regen,
            "every removal must come before grub-mkconfig, or the distro's own \
             entry for the deleted kernel survives: {:?}",
            plan.describe()
        );
    }

    /// Even with no `40_custom` entry to drop, the menu still has to be rebuilt:
    /// the distro's generated entry is not in that file.
    #[test]
    fn prune_regenerates_even_without_a_custom_entry() {
        let mut s = state();
        s.custom = "#!/bin/sh\nexec tail -n +3 $0\n".to_string();
        let plan = plan_prune(&paths(), &s, "snap-target").expect("plans");
        assert!(
            plan.actions
                .iter()
                .any(|a| matches!(a, Action::Run { argv, .. } if argv[0] == "grub-mkconfig")),
            "grub.cfg must still be regenerated: {:?}",
            plan.describe()
        );
    }

    #[test]
    fn prune_refuses_the_booted_golden_and_pinned_snapshots() {
        for (id, expected) in [
            ("snap-booted", "booted snapshot"),
            ("snap-golden", "golden image"),
            ("snap-pinned", "pinned GRUB default"),
        ] {
            let err = plan_prune(&paths(), &state(), id).unwrap_err();
            assert!(
                err.to_string().contains(expected),
                "pruning {id} must be refused as the {expected}: {err}"
            );
        }
    }

    #[test]
    fn prune_refuses_a_snapshot_it_does_not_know() {
        let err = plan_prune(&paths(), &state(), "snap-absent").unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }

    /// A hidden snapshot must still be prunable. Its manifest is renamed, not gone,
    /// so refusing it would leave no way to remove one except unhiding first — and
    /// `hide` is what you do to something you are about to delete.
    #[test]
    fn prune_removes_a_hidden_snapshot_too() {
        let plan = plan_prune(&paths(), &state(), "snap-hidden").expect("plans");
        let dir = paths().root.join("snap-hidden").display().to_string();
        assert!(
            plan.actions.iter().any(
                |a| matches!(a, Action::Remove { path, .. } if path.display().to_string() == dir)
            ),
            "the directory holding manifest.yaml.hidden must be removed: {:?}",
            plan.describe()
        );
    }

    /// A dry run should say which parts are actually there, not imply it will
    /// delete four things when two are already gone.
    #[test]
    fn prune_says_when_a_part_is_already_gone() {
        let plan = plan_prune(&paths(), &state(), "snap-target").expect("plans");
        let described = plan.describe().join("\n");
        assert!(
            described.contains("already gone"),
            "a missing artefact should be reported as such:\n{described}"
        );
    }

    // ---- boot --------------------------------------------------------------

    /// Arming only. The reboot belongs to the shell, which tears the terminal down
    /// first — rebooting from inside the pane would take the machine with the TUI
    /// still holding the alternate screen.
    #[test]
    fn boot_arms_a_one_shot_and_leaves_rebooting_to_the_caller() {
        let plan = plan_boot(&state(), "snap-target").expect("plans");
        assert_eq!(plan.actions.len(), 1, "arming only: {:?}", plan.describe());
        match &plan.actions[0] {
            Action::Run { argv, .. } => {
                assert_eq!(argv[0], "grub-reboot");
                assert_eq!(argv[1], "snap-target");
            }
            other => panic!("expected grub-reboot, got {other:?}"),
        }
        // Arming writes grubenv, which GRUB reads at boot. Nothing in /boot or
        // grub.cfg changes, so regenerating the menu would be busy-work.
        assert!(
            !plan
                .actions
                .iter()
                .any(|a| matches!(a, Action::Run { argv, .. } if argv[0] == "grub-mkconfig")),
            "arming must not regenerate the menu: {:?}",
            plan.describe()
        );
    }

    #[test]
    fn boot_refuses_a_snapshot_with_no_menu_entry() {
        let mut s = state();
        s.snapshots
            .iter_mut()
            .find(|x| x.id == "snap-target")
            .expect("fixture")
            .grub_entry = false;
        let err = plan_boot(&s, "snap-target").unwrap_err();
        assert!(err.to_string().contains("no GRUB menu entry"), "{err}");
    }

    /// The menu entry can be there while the files it points at are not. GRUB would
    /// offer the boot and fail at the prompt — on a blade reached over the network,
    /// that is a machine you cannot get back.
    #[test]
    fn boot_refuses_a_snapshot_whose_artefacts_are_missing() {
        for (vmlinuz, initramfs, expected) in [
            (false, true, "its kernel"),
            (true, false, "its initramfs"),
            (false, false, "its kernel and its initramfs"),
        ] {
            let mut s = state();
            let target = s
                .snapshots
                .iter_mut()
                .find(|x| x.id == "snap-target")
                .expect("fixture");
            target.vmlinuz = vmlinuz;
            target.initramfs = initramfs;
            let err = plan_boot(&s, "snap-target").unwrap_err();
            assert!(err.to_string().contains(expected), "{err}");
        }
    }

    #[test]
    fn boot_refuses_a_snapshot_it_does_not_know() {
        let err = plan_boot(&state(), "snap-absent").unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }

    // ---- hide / unhide -----------------------------------------------------

    #[test]
    fn hide_renames_and_removes_the_entry() {
        let plan = plan_hide(&paths(), &state(), "snap-target").expect("plans");
        assert!(matches!(plan.actions[0], Action::Rename { .. }));
        let removed = plan
            .actions
            .iter()
            .find_map(|a| match a {
                Action::Write { contents, .. } => Some(contents.clone()),
                _ => None,
            })
            .expect("writes 40_custom");
        assert!(!removed.contains("snap-target"), "entry must be gone");
        assert!(plan
            .actions
            .iter()
            .any(|a| matches!(a, Action::Run { argv, .. }
            if argv[0] == "grub-mkconfig")));
    }

    #[test]
    fn hide_of_an_already_hidden_snapshot_is_refused() {
        let err = plan_hide(&paths(), &state(), "snap-hidden").unwrap_err();
        assert!(err.to_string().contains("already hidden"), "{err}");
    }

    #[test]
    fn unhide_restores_the_manifest_and_rebuilds_the_entry() {
        let plan = plan_unhide(&paths(), &state(), "snap-hidden").expect("plans");
        assert!(matches!(&plan.actions[0], Action::Rename { from, .. }
            if from.ends_with("manifest.yaml.hidden")));
        let written = plan
            .actions
            .iter()
            .find_map(|a| match a {
                Action::Write { contents, .. } => Some(contents.clone()),
                _ => None,
            })
            .expect("writes 40_custom");
        assert!(
            written.contains("# === snap-hidden:"),
            "header our tool removes by"
        );
        assert!(written.contains("--id 'snap-hidden'"));
    }

    /// The rebuilt entry must carry the snapshot's OWN vmlinuz, because the
    /// recorded cmdline names whichever kernel was booted at capture time.
    #[test]
    fn rebuilt_entry_rewrites_boot_image() {
        let plan = plan_unhide(&paths(), &state(), "snap-hidden").expect("plans");
        let written = plan
            .actions
            .iter()
            .find_map(|a| match a {
                Action::Write { contents, .. } => Some(contents.clone()),
                _ => None,
            })
            .unwrap();

        // Scope to the block we appended: the file legitimately holds one
        // BOOT_IMAGE per entry, so counting across the whole file proves nothing.
        let rebuilt = written
            .split("# === snap-hidden:")
            .nth(1)
            .expect("the new block is appended to the file");
        assert!(rebuilt.contains("BOOT_IMAGE=/boot/vmlinuz-snap-hidden"));
        assert!(!rebuilt.contains("vmlinuz-other"));
        assert_eq!(rebuilt.matches("BOOT_IMAGE=").count(), 1);
    }

    // ---- pin / unpin -------------------------------------------------------

    #[test]
    fn pin_sets_saved_default_scheme_and_verifies() {
        let mut st = state();
        st.grub_defaults = "GRUB_DEFAULT=0\n".to_string();
        let plan = plan_pin(&paths(), &st, "snap-target").expect("plans");

        let defaults = plan
            .actions
            .iter()
            .find_map(|a| match a {
                Action::Write { contents, .. } => Some(contents.clone()),
                _ => None,
            })
            .expect("rewrites /etc/default/grub");
        assert!(defaults.contains("GRUB_DEFAULT=saved"));
        assert!(!defaults.contains("GRUB_DEFAULT=0"));

        assert!(plan
            .actions
            .iter()
            .any(|a| matches!(a, Action::Run { argv, .. }
            if argv == &vec!["grub-set-default".to_string(), "snap-target".to_string()])));
    }

    /// Nothing to change means no Write action — a pin must not rewrite an
    /// already-correct /etc/default/grub.
    #[test]
    fn pin_leaves_grub_defaults_alone_when_already_saved() {
        let mut st = state();
        st.grub_defaults = "GRUB_DEFAULT=saved\nGRUB_DISABLE_SUBMENU='false'\n".to_string();
        let plan = plan_pin(&paths(), &st, "snap-target").expect("plans");
        assert!(
            !plan
                .actions
                .iter()
                .any(|a| matches!(a, Action::Write { .. })),
            "nothing needed writing: {:?}",
            plan.describe()
        );
    }

    /// The over-broad flattening an earlier shell version forced gets undone.
    #[test]
    fn pin_restores_submenus_when_they_were_forced_flat() {
        let mut st = state();
        st.grub_defaults = "GRUB_DEFAULT=saved\nGRUB_DISABLE_SUBMENU=true\n".to_string();
        let plan = plan_pin(&paths(), &st, "snap-target").expect("plans");
        let defaults = plan
            .actions
            .iter()
            .find_map(|a| match a {
                Action::Write { contents, .. } => Some(contents.clone()),
                _ => None,
            })
            .expect("rewrites defaults");
        assert!(defaults.contains("GRUB_DISABLE_SUBMENU='false'"));
    }

    #[test]
    fn pin_refuses_a_hidden_or_artefact_less_snapshot() {
        let err = plan_pin(&paths(), &state(), "snap-hidden").unwrap_err();
        assert!(err.to_string().contains("hidden"), "{err}");

        let mut st = state();
        if let Some(s) = st.snapshots.iter_mut().find(|s| s.id == "snap-target") {
            s.initramfs = false;
        }
        let err = plan_pin(&paths(), &st, "snap-target").unwrap_err();
        assert!(err.to_string().contains("incomplete"), "{err}");
    }

    #[test]
    fn unpin_clears_the_saved_entry() {
        let plan = plan_unpin(&paths()).expect("plans");
        assert!(plan
            .actions
            .iter()
            .any(|a| matches!(a, Action::Run { argv, .. }
            if argv == &vec!["grub-set-default".to_string(), "0".to_string()])));
    }

    #[test]
    fn plan_describe_names_every_action() {
        let plan = plan_hide(&paths(), &state(), "snap-target").unwrap();
        let lines = plan.describe();
        assert_eq!(lines.len(), plan.actions.len());
        assert!(lines.iter().any(|l| l.starts_with("rename")));
        assert!(lines.iter().any(|l| l.starts_with("run")));
    }

    #[test]
    fn golden_writes_the_marker_that_protects() {
        let plan = plan_golden(&paths(), &state(), "snap-target").expect("plans");
        match &plan.actions[0] {
            Action::Write { path, contents, .. } => {
                assert!(path.ends_with(".golden-snapshot"), "{path:?}");
                assert_eq!(contents, "snap-target\n");
            }
            other => panic!("expected a write, got {other:?}"),
        }
    }

    /// Re-marking the snapshot that is already golden must not be a silent no-op
    /// the operator reads as success.
    #[test]
    fn golden_refuses_a_snapshot_that_is_already_golden() {
        let err = plan_golden(&paths(), &state(), "snap-golden").unwrap_err();
        assert!(
            err.to_string().contains("already the golden image"),
            "{err}"
        );
    }

    /// Applying a plan writes the files and issues the commands, and the runner
    /// seam means a test can assert *which* commands without spawning them.
    #[test]
    fn apply_issues_the_commands_to_the_runner() {
        let runner = Recording::default();
        let plan = plan_pin(&paths(), &state(), "snap-target").expect("plans");
        apply_with(&plan, false, &runner).expect("applies");

        let calls = runner.calls.borrow();
        assert!(
            calls
                .iter()
                .any(|argv| argv.first().map(String::as_str) == Some("grub-set-default")),
            "pin must set the default: {calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|argv| argv.first().map(String::as_str) == Some("grub-mkconfig")),
            "...and regenerate the menu: {calls:?}"
        );
    }

    /// `--dry-run` must not reach the runner at all.
    #[test]
    fn a_dry_run_issues_no_commands() {
        let runner = Recording::default();
        let plan = plan_unpin(&paths()).expect("plans");
        apply_with(&plan, true, &runner).expect("applies");
        assert!(runner.calls.borrow().is_empty(), "dry-run ran something");
    }
}
