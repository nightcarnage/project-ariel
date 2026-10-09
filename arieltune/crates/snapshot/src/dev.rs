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

use anyhow::Result;

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
}
