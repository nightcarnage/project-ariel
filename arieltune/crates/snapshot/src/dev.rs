// SPDX-License-Identifier: GPL-2.0-only
//! The dev loop: rebuild **only what the patch touched**, into a snapshot of its own.
//!
//! ## The problem this solves
//!
//! Developing a patch against the series does not need a kernel build. It needs
//! the amdgpu module relinked, which is a `make M=drivers/gpu/drm/amd/amdgpu
//! modules` — incremental, seconds rather than tens of minutes.
//!
//! The shell tool already does that build inside `create`. What it cannot do is
//! keep the result from landing on the running kernel: `mkinitcpio` reads the
//! module out of `/usr/lib/modules/<kver>/`, so `create` installs the freshly
//! built module **over the live one** and only then generates the snapshot's
//! image. Its steps 1 and 2 — preserve the initramfs, copy the known-good module
//! aside — exist to survive that window, which is the tell that the window is the
//! problem. If the build fails or the script dies in between, the box is left
//! booting a dev module with the fallback one edit away from being lost.
//!
//! So a dev snapshot must be derived **from the snapshot it is based on**, by
//! replacing the module inside that snapshot's own image, and the live module
//! tree must never be written. That also makes each iteration independently
//! bootable by construction rather than by remembering to snapshot first.
//!
//! ## Why the build set comes from git, not from the patch metadata
//!
//! The series carries a `touches` field per patch, and it would be tempting to
//! build exactly those directories. But `touches` is a *description of the patch
//! file*, while the question is what the tree actually differs in right now —
//! after a hand-edit, a half-reverted hunk, or a file the patch author forgot to
//! list. `git status` is the ground truth for "what am I touching", so it is what
//! decides the build set. The patch metadata is for explaining, not for building.
//!
//! ## Scope
//!
//! This module is **read-only**: it inspects a tree and works out what a dev
//! snapshot would involve. Nothing here builds or writes. That keeps the analysis
//! testable on a workstation with no kernel tree at all, and it means the plan can
//! be printed before anyone commits to a 30-second build.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::write::Runner;

/// A kernel source tree, as it stands right now.
#[derive(Debug, Clone)]
pub struct Tree {
    pub path: PathBuf,
    /// `include/config/kernel.release`, which is what the built module's vermagic
    /// is checked against. `None` when the tree has not been configured.
    pub release: Option<String>,
    /// Short git revision, when the tree is a git checkout.
    pub revision: Option<String>,
    /// Files git reports as modified or untracked, relative to the tree root.
    pub dirty: Vec<String>,
}

impl Tree {
    /// The module directories that a build would actually have to touch.
    ///
    /// Derived from the dirty files. A path outside `drivers/` cannot be expressed
    /// as an `M=` target, so it is reported separately rather than silently
    /// dropped: a dev build that quietly ignores half the diff is the failure mode
    /// worth being loud about.
    pub fn build_set(&self) -> (Vec<String>, Vec<String>) {
        let mut modules: Vec<String> = Vec::new();
        let mut elsewhere: Vec<String> = Vec::new();
        for path in &self.dirty {
            match module_of(path) {
                Some(dir) => {
                    if !modules.contains(&dir) {
                        modules.push(dir);
                    }
                }
                None => elsewhere.push(path.clone()),
            }
        }
        modules.sort();
        elsewhere.sort();
        (modules, elsewhere)
    }
}

/// The `M=` directory a changed source file belongs to.
///
/// `drivers/gpu/drm/amd/amdgpu/gmc_v10_0.c` -> `Some("drivers/gpu/drm/amd/amdgpu")`.
///
/// Only `drivers/` is mapped, because that is where the series lives and where
/// `M=` is meaningful. Anything else — `Makefile`, `arch/`, `include/` — returns
/// `None`, since those change the *configuration* of the whole tree and a
/// module-only rebuild would not pick them up.
fn module_of(path: &str) -> Option<String> {
    let dir = path.rsplit_once('/')?.0;
    dir.starts_with("drivers/").then(|| dir.to_string())
}

/// Parse `git status --porcelain` into tree-relative paths.
///
/// The format is `XY <path>`, with renames written `XY <orig> -> <new>`. Only the
/// destination matters: it is the file that now exists and needs building.
fn parse_porcelain(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            if line.len() < 4 {
                return None;
            }
            let path = &line[3..];
            let path = path.rsplit_once(" -> ").map_or(path, |(_, new)| new);
            let path = path.trim().trim_matches('"');
            (!path.is_empty()).then(|| path.to_string())
        })
        .collect()
}

/// Inspect a tree. Read-only; runs `git` but never `make`.
pub fn inspect_tree(path: &Path) -> Result<Tree> {
    if !path.is_dir() {
        anyhow::bail!("not a directory: {}", path.display());
    }
    // A tree with no .config cannot produce a module at all, and finding that out
    // from a failed make 30 seconds in is worse than being told now.
    if !path.join(".config").is_file() {
        anyhow::bail!(
            "no .config in {} — copy one from /usr/lib/modules/<kver>/build/ first",
            path.display()
        );
    }

    let release = std::fs::read_to_string(path.join("include/config/kernel.release"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    Ok(Tree {
        path: path.to_path_buf(),
        release,
        revision: git(path, &["rev-parse", "--short", "HEAD"]),
        dirty: git(path, &["status", "--porcelain"])
            .map(|out| parse_porcelain(&out))
            .unwrap_or_default(),
    })
}

/// Run a git command in `path`, returning trimmed stdout. `None` on any failure:
/// a tree that is not a git checkout is unusual but not an error.
fn git(path: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The kernel release this machine is running, for comparing against the tree.
pub fn running_release() -> Option<String> {
    let out = Command::new("uname").arg("-r").output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A module built from the tree and prepared for insertion into an image.
#[derive(Debug, Clone)]
pub struct Prepared {
    /// The compressed module: exactly the bytes that go into the image.
    pub bytes: Vec<u8>,
    /// The uncompressed `.ko` the build produced.
    pub source: PathBuf,
    /// Size of the uncompressed module, for reporting.
    pub raw_bytes: u64,
}

/// Build the module directories the tree has changed, and compress the result.
///
/// The commands go through [`Runner`] so the argument vector is testable without
/// a kernel tree: this is the step that spends real time, and a wrong `M=` is not
/// something to discover after thirty seconds of compiling.
///
/// `make M=<dir> modules` is incremental by nature — it relinks only the objects
/// whose sources changed — which is what makes the loop fast. It is *not* a kernel
/// build, and nothing here runs `modules_prepare` or touches the package: if the
/// tree is configured, this is seconds.
pub fn build(tree: &Tree, runner: &dyn Runner, work: &Path) -> Result<Prepared> {
    let (modules, _elsewhere) = tree.build_set();
    if modules.is_empty() {
        bail!("nothing to build: no changed file under drivers/");
    }
    std::fs::create_dir_all(work)
        .with_context(|| format!("creating scratch space {}", work.display()))?;

    for dir in &modules {
        // `-C` rather than a working directory, so the same argument vector works
        // through any Runner implementation.
        let argv: Vec<String> = vec![
            "make".to_string(),
            "-C".to_string(),
            tree.path.display().to_string(),
            format!("M={dir}"),
            "modules".to_string(),
        ];
        runner.run(&argv)?;
    }

    // Locate what the build produced rather than assuming a filename: the module
    // is `amdgpu.ko` today, and hard-coding that would be the thing to break the
    // day the series grows a second one.
    let ko = newest_ko(&tree.path, &modules)
        .with_context(|| format!("no .ko produced under any of {modules:?}"))?;

    let stripped = work.join("module.ko");
    runner.run(&[
        "strip".to_string(),
        "--strip-debug".to_string(),
        ko.display().to_string(),
        "-o".to_string(),
        stripped.display().to_string(),
    ])?;

    let raw_bytes = std::fs::metadata(&stripped)
        .with_context(|| format!("stat {}", stripped.display()))?
        .len();
    let compressed = work.join("module.ko.zst");
    runner.run(&[
        "zstd".to_string(),
        "-q".to_string(),
        "-f".to_string(),
        stripped.display().to_string(),
        "-o".to_string(),
        compressed.display().to_string(),
    ])?;

    Ok(Prepared {
        bytes: std::fs::read(&compressed)
            .with_context(|| format!("reading {}", compressed.display()))?,
        source: ko,
        raw_bytes,
    })
}

/// The most recently modified `.ko` under any of `dirs`.
///
/// Newest rather than first: an incremental build leaves older objects behind, and
/// picking one of those would quietly snapshot the previous iteration.
fn newest_ko(tree: &Path, dirs: &[String]) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(tree.join(dir)) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("ko") {
                continue;
            }
            let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            if best.as_ref().is_none_or(|(when, _)| modified > *when) {
                best = Some((modified, path));
            }
        }
    }
    best.map(|(_, path)| path)
}

/// Derive the dev image from the parent image, with the built module in it.
///
/// Returns the member path and the report. The member is located **in the parent
/// image** rather than constructed from a version string: the path inside an
/// initramfs is mkinitcpio's business, and building it by hand is how you replace
/// a file that is not the one the machine boots. The suffix comes from the module
/// the build actually produced.
pub fn assemble(
    parent_image: &Path,
    prepared: &Prepared,
    dest: &Path,
    work: &Path,
) -> Result<(String, crate::image::Derived)> {
    if prepared.bytes.is_empty() {
        bail!("the built module is empty; refusing to derive an image from it");
    }
    let stem = prepared
        .source
        .file_name()
        .and_then(|name| name.to_str())
        .context("the built module has no file name")?;
    let member = crate::image::find_member(parent_image, &format!("{stem}.zst"))?;
    let derived = crate::image::derive(parent_image, &member, &prepared.bytes, dest, work)?;
    Ok((member, derived))
}

/// The kernel release a module inside an image is built for.
///
/// Taken from the member path rather than from the machine you happen to be
/// standing at:
///
/// ```text
///   usr/lib/modules/7.2.9-1-cachyos-bore/kernel/drivers/.../amdgpu.ko.zst
///                    ^^^^^^^^^^^^^^^^^^^^
/// ```
///
/// This matters more than it looks. The vermagic gate exists so a module the
/// target kernel would refuse never gets built into an image — but "the target" is
/// whatever boots *that image*, and that is frequently not the machine running the
/// command. Building for a blade from a workstation is the normal case, and
/// comparing there against the workstation's own kernel refuses work that is
/// perfectly correct.
///
/// Pure, so it is tested rather than trusted.
pub fn kver_from_member(member: &str) -> Option<String> {
    let mut parts = member.split('/');
    // `usr/lib/modules/<kver>/...`
    if parts.next()? != "usr" || parts.next()? != "lib" || parts.next()? != "modules" {
        return None;
    }
    let kver = parts.next()?;
    (!kver.is_empty()).then(|| kver.to_string())
}

/// What a dev snapshot from this tree would involve.
#[derive(Debug, Clone)]
pub struct DevPlan {
    pub tree: Tree,
    pub running: Option<String>,
    /// `M=` targets, and the files that forced them.
    pub modules: Vec<String>,
    /// Changed files that a module rebuild would not capture.
    pub elsewhere: Vec<String>,
    /// Reasons this tree cannot produce a bootable module. Non-empty means stop.
    pub blockers: Vec<String>,
}

/// Work out the plan. Pure given an inspected tree.
pub fn plan(tree: Tree, running: Option<String>) -> DevPlan {
    let (modules, elsewhere) = tree.build_set();
    let mut blockers = Vec::new();

    match (&tree.release, &running) {
        (Some(release), Some(running)) if release != running => blockers.push(format!(
            "tree builds {release} but this machine runs {running} — \
             the kernel would refuse the module (vermagic mismatch)"
        )),
        // Equal releases: the normal case, nothing to say.
        (Some(_), Some(_)) => {}
        (Some(_), None) => {}
        (None, _) => blockers.push(
            "no include/config/kernel.release — the tree is not configured, or was \
             never prepared (`make modules_prepare`)"
                .to_string(),
        ),
    }

    if modules.is_empty() && !tree.dirty.is_empty() {
        blockers.push(
            "every changed file is outside drivers/ — a module rebuild would \
             capture none of it"
                .to_string(),
        );
    }
    if tree.dirty.is_empty() {
        blockers.push(
            "the tree is clean — there is nothing to rebuild. Edit a patch, or pass \
             the tree a revision is checked out for"
                .to_string(),
        );
    }

    DevPlan {
        tree,
        running,
        modules,
        elsewhere,
        blockers,
    }
}

impl DevPlan {
    pub fn can_build(&self) -> bool {
        self.blockers.is_empty()
    }

    /// Human-readable lines. Nothing here writes.
    pub fn describe(&self) -> Vec<String> {
        let mut out = vec![
            format!("tree        {}", self.tree.path.display()),
            format!(
                "release     {}",
                self.tree.release.as_deref().unwrap_or("unknown")
            ),
            format!(
                "running     {}",
                self.running.as_deref().unwrap_or("unknown")
            ),
            format!(
                "revision    {}{}",
                self.tree.revision.as_deref().unwrap_or("not a git tree"),
                if self.tree.dirty.is_empty() {
                    " (clean)".to_string()
                } else {
                    format!(" ({} changed)", self.tree.dirty.len())
                }
            ),
        ];

        if !self.tree.dirty.is_empty() {
            out.push(String::new());
            out.push("changed".to_string());
            for path in &self.tree.dirty {
                out.push(format!("  {path}"));
            }
        }

        out.push(String::new());
        if self.modules.is_empty() {
            out.push("build       nothing to rebuild".to_string());
        } else {
            for module in &self.modules {
                out.push(format!("build       make M={module} modules"));
            }
        }

        if !self.elsewhere.is_empty() {
            out.push(String::new());
            out.push("not captured by a module rebuild".to_string());
            for path in &self.elsewhere {
                out.push(format!("  {path}"));
            }
        }

        if !self.blockers.is_empty() {
            out.push(String::new());
            for blocker in &self.blockers {
                out.push(format!("REFUSED     {blocker}"));
            }
        } else {
            out.push(String::new());
            out.push(
                "would       assemble a snapshot from this module, derived from the \
                 parent snapshot's own initramfs"
                    .to_string(),
            );
            out.push(
                "            the live module tree is not written; the base kernel \
                 stays bootable"
                    .to_string(),
            );
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(dirty: &[&str]) -> Tree {
        Tree {
            path: PathBuf::from("/tree"),
            release: Some("7.0.9-1-cachyos-bore".to_string()),
            revision: Some("a3f19c2".to_string()),
            dirty: dirty.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn a_changed_driver_file_maps_to_its_module_directory() {
        let t = tree(&["drivers/gpu/drm/amd/amdgpu/gmc_v10_0.c"]);
        let (modules, elsewhere) = t.build_set();
        assert_eq!(modules, vec!["drivers/gpu/drm/amd/amdgpu"]);
        assert!(elsewhere.is_empty());
    }

    #[test]
    fn several_files_in_one_module_produce_one_build() {
        let t = tree(&[
            "drivers/gpu/drm/amd/amdgpu/gmc_v10_0.c",
            "drivers/gpu/drm/amd/amdgpu/amdgpu_gmc.c",
        ]);
        let (modules, _) = t.build_set();
        assert_eq!(
            modules,
            vec!["drivers/gpu/drm/amd/amdgpu"],
            "two files, one M= target"
        );
    }

    /// A change outside drivers/ cannot be expressed as `M=`, and quietly
    /// ignoring it would mean shipping a dev snapshot that contains half the diff.
    #[test]
    fn a_change_outside_drivers_is_reported_not_dropped() {
        let t = tree(&["drivers/gpu/drm/amd/amdgpu/gmc_v10_0.c", "Makefile"]);
        let (modules, elsewhere) = t.build_set();
        assert_eq!(modules, vec!["drivers/gpu/drm/amd/amdgpu"]);
        assert_eq!(elsewhere, vec!["Makefile"]);
    }

    #[test]
    fn porcelain_paths_survive_renames_and_untracked_files() {
        let text = " M drivers/gpu/drm/amd/amdgpu/gmc_v10_0.c\n\
                    ?? drivers/gpu/drm/amd/amdgpu/new_file.c\n\
                    R  old/path.c -> drivers/gpu/drm/amd/amdgpu/renamed.c\n";
        assert_eq!(
            parse_porcelain(text),
            vec![
                "drivers/gpu/drm/amd/amdgpu/gmc_v10_0.c",
                "drivers/gpu/drm/amd/amdgpu/new_file.c",
                "drivers/gpu/drm/amd/amdgpu/renamed.c",
            ]
        );
    }

    /// The vermagic mismatch is the one that wastes a whole build cycle, so it
    /// must be caught before `make` runs, not after.
    #[test]
    fn a_tree_for_another_kernel_is_refused_before_building() {
        let p = plan(
            tree(&["drivers/gpu/drm/amd/amdgpu/gmc_v10_0.c"]),
            Some("7.2.9-1-cachyos-bore".to_string()),
        );
        assert!(!p.can_build());
        assert!(
            p.blockers.iter().any(|b| b.contains("vermagic")),
            "{:?}",
            p.blockers
        );
    }

    #[test]
    fn a_matching_tree_with_changes_is_buildable() {
        let p = plan(
            tree(&["drivers/gpu/drm/amd/amdgpu/gmc_v10_0.c"]),
            Some("7.0.9-1-cachyos-bore".to_string()),
        );
        assert!(p.can_build(), "{:?}", p.blockers);
        assert_eq!(p.modules, vec!["drivers/gpu/drm/amd/amdgpu"]);
    }

    #[test]
    fn a_clean_tree_has_nothing_to_do() {
        let p = plan(tree(&[]), Some("7.0.9-1-cachyos-bore".to_string()));
        assert!(!p.can_build());
        assert!(
            p.blockers.iter().any(|b| b.contains("clean")),
            "{:?}",
            p.blockers
        );
    }

    /// Changes that are all outside drivers/ must refuse rather than produce a
    /// snapshot identical to its parent and call that success.
    #[test]
    fn changes_entirely_outside_drivers_refuse() {
        let p = plan(
            tree(&["Makefile"]),
            Some("7.0.9-1-cachyos-bore".to_string()),
        );
        assert!(!p.can_build());
        assert!(
            p.blockers.iter().any(|b| b.contains("outside drivers/")),
            "{:?}",
            p.blockers
        );
    }

    #[test]
    fn describe_names_the_build_and_the_risk() {
        let p = plan(
            tree(&["drivers/gpu/drm/amd/amdgpu/amdgpu_gmc.c"]),
            Some("7.0.9-1-cachyos-bore".to_string()),
        );
        let text = p.describe().join("\n");
        assert!(
            text.contains("make M=drivers/gpu/drm/amd/amdgpu modules"),
            "{text}"
        );
        assert!(
            text.contains("live module tree is not written"),
            "the plan should say what it will not touch: {text}"
        );
    }

    // ---- the build step ----------------------------------------------------

    /// A tree on disk with one changed driver directory and a stub `.ko`.
    fn tree_dir(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("snap-dev-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("drivers/gpu/drm/amd/amdgpu");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(root.join(".config"), b"CONFIG_FOO=y\n").unwrap();
        std::fs::write(dir.join("amdgpu.ko"), vec![0u8; 128]).unwrap();
        std::fs::create_dir_all(root.join("work")).unwrap();
        root
    }

    fn dirty_tree(root: &Path, dirty: &[&str]) -> Tree {
        Tree {
            path: root.to_path_buf(),
            release: Some("7.0.9-1-cachyos-bore".to_string()),
            revision: Some("a3f19c2".to_string()),
            dirty: dirty.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// The argument vector is the thing worth pinning: a wrong `M=` is not
    /// something to discover thirty seconds into a compile.
    #[test]
    fn build_invokes_make_per_module_and_compresses_the_result() {
        let root = tree_dir("build");
        let t = dirty_tree(&root, &["drivers/gpu/drm/amd/amdgpu/gmc_v10_0.c"]);
        let work = root.join("work");
        // The runner records instead of running, so stage what the post-processing
        // steps would have produced.
        std::fs::write(work.join("module.ko"), vec![0u8; 96]).unwrap();
        std::fs::write(work.join("module.ko.zst"), b"compressed module").unwrap();

        let runner = crate::write::Recording::default();
        let prepared = build(&t, &runner, &work).unwrap();

        let calls = runner.calls.borrow().clone();
        let makes: Vec<&Vec<String>> = calls.iter().filter(|c| c[0] == "make").collect();
        assert_eq!(makes.len(), 1, "one make per changed module directory");
        assert!(
            makes[0].contains(&"M=drivers/gpu/drm/amd/amdgpu".to_string()),
            "{:?}",
            makes[0]
        );
        assert!(
            makes[0].contains(&"modules".to_string()),
            "it must be a modules build, not a full build: {:?}",
            makes[0]
        );
        assert!(
            calls.iter().any(|c| c[0] == "zstd"),
            "the module has to be compressed before it can go in an image"
        );
        assert!(calls.iter().any(|c| c[0] == "strip"), "and stripped first");
        assert_eq!(prepared.bytes, b"compressed module");
        assert_eq!(
            prepared.raw_bytes, 96,
            "the stripped size is what is reported"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Two changed directories must produce two `M=` invocations, not one.
    #[test]
    fn build_covers_every_changed_module_directory() {
        let root = tree_dir("build-multi");
        let other = root.join("drivers/gpu/drm/amdkfd");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("kfd.ko"), vec![0u8; 64]).unwrap();
        std::fs::write(root.join("work/module.ko"), vec![0u8; 64]).unwrap();
        std::fs::write(root.join("work/module.ko.zst"), b"x").unwrap();

        let t = dirty_tree(
            &root,
            &[
                "drivers/gpu/drm/amd/amdgpu/gmc_v10_0.c",
                "drivers/gpu/drm/amdkfd/kfd_chardev.c",
            ],
        );
        let runner = crate::write::Recording::default();
        build(&t, &runner, &root.join("work")).unwrap();

        let calls = runner.calls.borrow().clone();
        let makes: Vec<&Vec<String>> = calls.iter().filter(|c| c[0] == "make").collect();
        assert_eq!(makes.len(), 2, "{makes:?}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn build_refuses_a_tree_with_nothing_changed() {
        let root = tree_dir("build-clean");
        let t = dirty_tree(&root, &[]);
        let runner = crate::write::Recording::default();

        let err = build(&t, &runner, &root.join("work")).unwrap_err();
        assert!(err.to_string().contains("nothing to build"), "{err}");
        assert!(
            runner.calls.borrow().is_empty(),
            "refusing must not have run anything"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A build that produces no module must not proceed to make an image out of
    /// nothing — that is how a snapshot ends up silently identical to its parent.
    #[test]
    fn build_fails_when_the_build_produced_no_module() {
        let root = tree_dir("build-noout");
        std::fs::remove_file(root.join("drivers/gpu/drm/amd/amdgpu/amdgpu.ko")).unwrap();
        let t = dirty_tree(&root, &["drivers/gpu/drm/amd/amdgpu/gmc_v10_0.c"]);
        let runner = crate::write::Recording::default();

        let err = build(&t, &runner, &root.join("work")).unwrap_err();
        assert!(format!("{err:#}").contains("no .ko produced"), "{err:#}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Newest rather than first: an incremental build leaves older objects behind,
    /// and picking one of those would quietly snapshot the previous iteration.
    #[test]
    fn the_module_wins_over_an_older_one() {
        let root = tree_dir("build-newest");
        let dir = root.join("drivers/gpu/drm/amd/amdgpu");
        std::fs::write(dir.join("stale.ko"), vec![0u8; 16]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.join("fresh.ko"), vec![0u8; 16]).unwrap();

        let found = newest_ko(&root, &["drivers/gpu/drm/amd/amdgpu".to_string()]).unwrap();
        assert_eq!(found.file_name().unwrap(), "fresh.ko");

        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- which kernel the build is for -------------------------------------

    #[test]
    fn the_kver_is_read_out_of_the_member_path() {
        assert_eq!(
            kver_from_member(
                "usr/lib/modules/7.2.9-1-cachyos-bore/kernel/drivers/gpu/drm/amd/amdgpu/amdgpu.ko.zst"
            ),
            Some("7.2.9-1-cachyos-bore".to_string())
        );
        // Anything that is not a module path must say so rather than guess.
        assert_eq!(kver_from_member("boot/vmlinuz"), None);
        assert_eq!(kver_from_member("usr/lib/modules/"), None);
        assert_eq!(kver_from_member(""), None);
    }

    /// The gate has to compare against the kernel the *image* is for. Otherwise
    /// building for a blade from a workstation is refused for being correct.
    #[test]
    fn building_for_another_kernel_is_not_a_refusal() {
        // Tree is 7.2.9-bore, this machine runs 7.2.9-cachyos: a mismatch locally...
        let local = plan(
            tree(&["drivers/gpu/drm/amd/amdgpu/gmc_v10_0.c"]),
            Some("7.2.9-1-cachyos".to_string()),
        );
        assert!(
            !local.can_build(),
            "against the local kernel it should warn"
        );

        // ...but asked about the kernel the target boots, the same tree is exactly
        // right and must not be refused. (The fixture tree builds
        // 7.0.9-1-cachyos-bore, which is why that is the release that clears it.)
        let targeted = plan(
            tree(&["drivers/gpu/drm/amd/amdgpu/gmc_v10_0.c"]),
            Some("7.0.9-1-cachyos-bore".to_string()),
        );
        assert!(targeted.can_build(), "{:?}", targeted.blockers);
    }
}
