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
//! The analysis half — [`inspect_tree`] and [`plan`] — is **read-only**: it inspects
//! a tree and works out what a dev snapshot would involve, touching nothing. That
//! keeps the analysis testable on a workstation with no kernel tree at all, and it
//! means the plan can be printed before anyone commits to a 30-second build.
//!
//! The other half does real work: [`build`] runs `make` in the tree (as the tree's
//! owner, never as root — see [`build_as`]), and [`assemble`] rewrites one member
//! of a parent image into a new one. Neither ever writes to the *live* module tree
//! or to `/boot`; that is the whole point of deriving an image rather than
//! installing a module, and it is what the shell tool's `create` gets wrong.

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
    /// Short git revision, when the tree is a checkout of its own.
    pub revision: Option<String>,
    /// Files git reports as modified or untracked.
    ///
    /// **Only meaningful when `versioned` is set.** An extracted kernel source is
    /// routinely *not* a checkout of its own: on the blades it sits inside the
    /// packaging repo as an untracked subdirectory, where `git status` reports the
    /// PKGBUILD's patch files and never a driver edit. Reading `dirty` there gives
    /// an empty list that looks exactly like "nothing has changed".
    pub dirty: Vec<String>,
    /// Whether `dirty` reflects this tree at all — i.e. whether the tree is the
    /// root of its own git checkout rather than a subdirectory of someone else's.
    pub versioned: bool,
}

/// The directory to hand to `make` when the tree cannot say what changed.
///
/// Every patch in the series touches amdgpu, so this is the module a dev edit
/// almost always lands in. It is not a guess about *which files* changed — `make
/// M=` is itself incremental and decides that from timestamps. It only means the
/// narrowing-by-diff step is skipped when there is no diff to be had.
pub const DEFAULT_MODULE_DIR: &str = "drivers/gpu/drm/amd/amdgpu";

impl Tree {
    /// The module directories a build would actually have to touch.
    ///
    /// With a versioned tree this is derived from the changed files. Without one
    /// there is no diff to derive it from, so the series' own directory is handed
    /// over and **make** decides what is stale from its own dependency tracking.
    /// That is the honest fallback: the alternative is refusing to build a tree
    /// that is perfectly buildable.
    ///
    /// A path outside `drivers/` cannot be expressed as an `M=` target, so it is
    /// reported separately rather than silently dropped: a dev build that quietly
    /// ignores half the diff is the failure mode worth being loud about.
    pub fn build_set(&self) -> (Vec<String>, Vec<String>) {
        if !self.versioned {
            return (vec![DEFAULT_MODULE_DIR.to_string()], Vec::new());
        }

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

    // Is this tree the root of its own checkout, or a subdirectory of someone
    // else's? Getting this wrong is silent: a nested tree reports the *parent*
    // repo's changes, which for a driver edit means an empty list.
    let toplevel = git(path, &["rev-parse", "--show-toplevel"]);
    let versioned = match &toplevel {
        Some(root) => {
            let root = Path::new(root);
            let (a, b) = (
                std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()),
                std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()),
            );
            a == b
        }
        None => false,
    };

    Ok(Tree {
        path: path.to_path_buf(),
        release,
        revision: versioned
            .then(|| git(path, &["rev-parse", "--short", "HEAD"]))
            .flatten(),
        // `--relative` so paths come back relative to the tree rather than to the
        // repository root; without it a nested checkout reports names the tree
        // cannot resolve.
        dirty: if versioned {
            git(path, &["status", "--porcelain", "--relative"])
                .map(|out| parse_porcelain(&out))
                .unwrap_or_default()
        } else {
            Vec::new()
        },
        versioned,
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
    /// The uncompressed module, for reporting.
    pub raw_bytes: u64,
    /// The compressed module on disk — the same bytes as [`Prepared::bytes`], but
    /// as a file, so registration can copy it into the snapshot store without the
    /// caller having to write it out again.
    pub compressed: PathBuf,
    /// MD5 of the compressed module, all 32 hex characters.
    ///
    /// Computed from [`Prepared::bytes`] rather than by hashing the file, so the
    /// digest recorded in a manifest describes the exact bytes that were put into
    /// the image — a re-read could pick up a different file and would still look
    /// right.
    pub md5: String,
    /// `modinfo -F srcversion`, when the module can be asked.
    ///
    /// This is the identity a kernel module actually carries: it changes whenever
    /// the module's source changes, which makes it the one field that can answer
    /// "is the module I am running the one this snapshot was built with?".
    pub srcversion: Option<String>,
    /// `modinfo -F vermagic`, when the module can be asked.
    pub vermagic: Option<String>,
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
        // `-j` matters more than it looks: without it this is a single-threaded
        // compile of a large driver on a machine with eight cores, which turns a
        // seconds-long relink into minutes. The shell tool used `-j$(nproc)` and
        // there was no reason to drop it.
        let jobs = std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(1);
        // `-C` rather than a working directory, so the same argument vector works
        // through any Runner implementation.
        let argv: Vec<String> = vec![
            "make".to_string(),
            "-C".to_string(),
            tree.path.display().to_string(),
            format!("M={dir}"),
            format!("-j{jobs}"),
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

    // Read once and hash the bytes in hand: hashing the file separately would let
    // the digest describe a different read of the same path.
    let bytes =
        std::fs::read(&compressed).with_context(|| format!("reading {}", compressed.display()))?;

    // Asked of the module that was just built, not of the installed one: the whole
    // point is to record what *this* build produced.
    let mut prepared = Prepared {
        md5: crate::id::md5_hex(&bytes),
        bytes,
        source: ko,
        raw_bytes,
        compressed,
        srcversion: None,
        vermagic: None,
    };
    prepared.srcversion = module_fact("srcversion", &prepared.compressed);
    prepared.vermagic = module_fact("vermagic", &prepared.compressed);
    Ok(prepared)
}

/// Ask `modinfo` for one field of a module.
///
/// Returns `None` rather than a placeholder when `modinfo` is missing or says
/// nothing — the caller decides how to say "unknown", because the right wording
/// differs between a command's output and a manifest's.
pub fn module_fact(field: &str, ko: &Path) -> Option<String> {
    let out = std::process::Command::new("modinfo")
        .args(["-F", field])
        .arg(ko)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&out.stdout);
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// Whether a build should be handed to `owner` rather than run by `euid`.
///
/// The rule, split out so it is testable without a filesystem: only root needs to
/// step aside, and only for a tree that somebody else owns.
pub fn needs_privilege_drop(euid: u32, owner: u32) -> bool {
    euid == 0 && owner != 0
}

/// The user a build of this tree should run as, when that is not us.
///
/// **This is not a nicety.** `dev --register` must run as root to write `/boot`,
/// `/etc/grub.d` and the store, but `make` rewrites the objects whose inputs
/// changed — and those objects would then be root's. The developer's next build,
/// as themselves, then fails on a file it cannot overwrite. Observed on blade 115:
/// `amdgpu_drv.o`, `amdgpu.o`, `amdgpu.mod.o` and `amdgpu.ko` all came back
/// root-owned after a sudo-run iteration.
pub fn build_as(tree: &Path) -> Option<(u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(tree).ok()?;
    if needs_privilege_drop(unsafe { libc::geteuid() }, meta.uid() as u32) {
        Some((meta.uid() as u32, meta.gid() as u32))
    } else {
        None
    }
}

/// Give a directory to another user, so a dropped-privilege build can write in it.
pub fn hand_over(dir: &Path, uid: u32, gid: u32) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes())
        .context("the scratch path contains a NUL byte")?;
    if unsafe { libc::chown(c.as_ptr(), uid, gid) } != 0 {
        bail!(
            "cannot give {} to {uid}:{gid}: {}",
            dir.display(),
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

/// The kernel package's own name for a release, from
/// `/usr/lib/modules/<release>/pkgbase`.
///
/// The files in `/boot` are named after the *package* (`linux-cachyos-bore`), not
/// after the release (`7.2.9-1-cachyos-bore`), and `pkgbase` is where the package
/// records which it is. Guessing the name is how a snapshot ends up paired with
/// another kernel's initramfs.
pub fn pkgbase(release: &str) -> Option<String> {
    let text =
        std::fs::read_to_string(Path::new(MODULES_ROOT).join(release).join("pkgbase")).ok()?;
    let name = text.trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// The names an initramfs for `release` goes by, most specific first.
///
/// Pure, and separate from the lookup, because "which file is this kernel's
/// initramfs" is worth testing on a machine that has no kernels at all.
fn initramfs_candidates(release: &str, pkgbase: Option<&str>) -> Vec<String> {
    let mut names = Vec::new();
    if let Some(pkg) = pkgbase {
        names.push(format!("initramfs-{pkg}.img"));
    }
    // A host that names the file after the release rather than the package. These
    // blades do not, but the shell tool accepted it and there is no reason to be
    // narrower than the thing being replaced.
    names.push(format!("initramfs-{release}.img"));
    names.push("initramfs-linux-cachyos.img".to_string());
    names
}

/// The initramfs the running system boots from, for `release`.
///
/// This is what `create` derives a snapshot from by default: the image that
/// currently boots, with the module replaced. `None` rather than a guess when
/// nothing matches — a snapshot derived from the *wrong* kernel's image boots a
/// kernel its modules do not match.
pub fn distro_initramfs(boot: &Path, release: &str) -> Option<PathBuf> {
    let pkg = pkgbase(release);
    for name in initramfs_candidates(release, pkg.as_deref()) {
        let path = boot.join(&name);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

/// Where the kernel package installs its modules.
const MODULES_ROOT: &str = "/usr/lib/modules";

/// The module the kernel package installed for `release`, if it is there.
///
/// `create` can snapshot the running system, and the module that is running is the
/// one the package installed — not one this tool built. Its location is the
/// installed tree's business, so it is derived from the release rather than from any
/// source tree.
///
pub fn installed_module(release: &str) -> Option<PathBuf> {
    installed_module_under(Path::new(MODULES_ROOT), release)
}

/// [`installed_module`], with the modules root as an argument so it can be tested.
fn installed_module_under(root: &Path, release: &str) -> Option<PathBuf> {
    let dir = root.join(release).join("kernel").join(DEFAULT_MODULE_DIR);
    // Found by extension rather than by name: the file is `amdgpu.ko.zst` today,
    // and hard-coding that would be the thing to break the day the series grows a
    // second module.
    let mut found: Option<PathBuf> = None;
    for entry in std::fs::read_dir(dir).ok()? {
        let path = entry.ok()?.path();
        if path.extension().and_then(|e| e.to_str()) == Some("zst") {
            found = Some(path);
        }
    }
    found
}

/// Prepare an already-compressed module — the installed, running one — for
/// insertion into an image.
///
/// The compressed bytes are copied **byte for byte** rather than re-stripped and
/// re-compressed. This is the module that is installed and running, so it is the
/// artefact that has actually booted; passing it through `strip` and `zstd` again
/// would produce a snapshot that differs from the thing that was validated, for no
/// gain at all.
///
/// The uncompressed copy exists only to be asked questions: `modinfo` reads the
/// vermagic and srcversion from it, and its size is the module's raw size.
///
pub fn adopt_module(installed: &Path, runner: &dyn Runner, work: &Path) -> Result<Prepared> {
    let name = installed
        .file_name()
        .and_then(|name| name.to_str())
        .context("the installed module has no file name")?;
    // `amdgpu.ko.zst` -> `amdgpu.ko`. The stem matters: `assemble` looks for
    // `<stem>.zst` inside the parent image, so this is what decides which member
    // gets replaced.
    let stem = name.strip_suffix(".zst").unwrap_or(name).to_string();

    std::fs::create_dir_all(work)
        .with_context(|| format!("creating scratch space {}", work.display()))?;

    // Read once and hash the bytes in hand, the same rule `build` follows: hashing
    // the path separately would let the digest describe a different read of it.
    let bytes =
        std::fs::read(installed).with_context(|| format!("reading {}", installed.display()))?;
    if bytes.is_empty() {
        bail!("{} is empty; refusing to snapshot it", installed.display());
    }

    let compressed = work.join(name);
    std::fs::write(&compressed, &bytes)
        .with_context(|| format!("writing {}", compressed.display()))?;

    let decompressed = work.join(&stem);
    runner.run(&[
        "zstd".to_string(),
        "-d".to_string(),
        "-f".to_string(),
        compressed.display().to_string(),
        "-o".to_string(),
        decompressed.display().to_string(),
    ])?;

    let raw_bytes = std::fs::metadata(&decompressed)
        .with_context(|| format!("stat {}", decompressed.display()))?
        .len();

    Ok(Prepared {
        md5: crate::id::md5_hex(&bytes),
        bytes,
        source: decompressed.clone(),
        raw_bytes,
        compressed,
        srcversion: module_fact("srcversion", &decompressed),
        vermagic: module_fact("vermagic", &decompressed),
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
    let member = module_member(parent_image, stem)?;
    let derived = crate::image::derive(parent_image, &member, &prepared.bytes, dest, work)?;
    Ok((member, derived))
}

/// Which member of an image holds the `amdgpu` module.
///
/// The module's own name is the preferred answer, because the image names the
/// member after the installed module. But it cannot be the *only* answer: a module
/// handed to `create --module` may have been renamed on the way here, and this
/// store's own dev path stores it as `module.ko.zst` regardless of what it is.
/// Renaming a copy must not decide which file in someone's kernel gets replaced.
///
/// So the fallback locates it the way the image does — by the directory the kernel
/// package puts the module in — and refuses when that is not unambiguous. Refusing
/// is the point: replacing the wrong member of a boot image produces a machine that
/// boots and is not the one that was asked for.
fn module_member(image: &Path, stem: &str) -> Result<String> {
    let exact = crate::image::members_ending(image, &format!("{stem}.zst"))?;
    if exact.len() == 1 {
        return Ok(exact.into_iter().next().expect("checked"));
    }

    let candidates: Vec<String> = crate::image::members_ending(image, ".ko.zst")?
        .into_iter()
        .filter(|name| name.contains(DEFAULT_MODULE_DIR))
        .collect();
    match candidates.len() {
        1 => Ok(candidates.into_iter().next().expect("checked")),
        0 => bail!(
            "no member under {DEFAULT_MODULE_DIR} ending in .ko.zst in the early archive of \
             {} — this image does not carry the module",
            image.display()
        ),
        _ => bail!(
            "{} members under {DEFAULT_MODULE_DIR} in {} ({candidates:?}) and the module to \
             place is {stem:?}, which matches none of them — refusing to guess which one the \
             machine boots",
            candidates.len(),
            image.display()
        ),
    }
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
    // "Nothing changed" is only a refusal when the tree can actually tell us that.
    // An unversioned tree has no diff to read, and refusing it would reject the
    // normal case of a source tree extracted for a build.
    if tree.versioned && tree.dirty.is_empty() {
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

    /// The revision line, which has to tell "no changes" apart from "cannot tell".
    ///
    /// An unversioned tree reporting "clean" would be a lie with consequences: it
    /// looks like a reason to stop, when in fact the tree simply has no diff to
    /// read and is perfectly buildable.
    fn revision_line(&self) -> String {
        if !self.tree.versioned {
            return "no VCS of its own (not a checkout — cannot diff)".to_string();
        }
        match self.tree.dirty.len() {
            0 => format!(
                "{} (clean)",
                self.tree.revision.as_deref().unwrap_or("unversioned")
            ),
            n => format!(
                "{} ({n} changed)",
                self.tree.revision.as_deref().unwrap_or("unversioned")
            ),
        }
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
            format!("revision    {}", self.revision_line()),
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
            if !self.tree.versioned {
                // Say why the narrowing step was skipped, or the directory looks
                // arbitrary.
                out.push(
                    "            (no VCS in this tree, so make decides what is \
                     stale from timestamps)"
                        .to_string(),
                );
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
            // "The image being derived from", not "the parent snapshot's" — this plan
            // is printed by `create` too, whose base image is the running kernel's
            // own initramfs rather than any snapshot's.
            out.push(
                "would       derive an image carrying this module, from the base image \
                 in the plan above"
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

    fn have_zstd() -> bool {
        Command::new("zstd")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn zstd_to(payload: &[u8], dest: &Path) {
        let src = dest.with_extension("raw");
        std::fs::write(&src, payload).unwrap();
        let ok = Command::new("zstd")
            .args(["-q", "-f"])
            .arg(&src)
            .arg("-o")
            .arg(dest)
            .status()
            .expect("running zstd")
            .success();
        assert!(ok, "zstd failed");
        let _ = std::fs::remove_file(&src);
    }

    fn tree(dirty: &[&str]) -> Tree {
        Tree {
            path: PathBuf::from("/tree"),
            release: Some("7.0.9-1-cachyos-bore".to_string()),
            revision: Some("a3f19c2".to_string()),
            dirty: dirty.iter().map(|s| s.to_string()).collect(),
            versioned: true,
        }
    }

    /// The shape that actually occurs on the blades: an extracted source tree
    /// that is not a checkout of its own.
    fn unversioned_tree() -> Tree {
        Tree {
            path: PathBuf::from("/tree"),
            release: Some("7.0.9-1-cachyos-bore".to_string()),
            revision: None,
            dirty: Vec::new(),
            versioned: false,
        }
    }

    /// `/boot` is named after the package, not the release, and the candidates are
    /// ordered most specific first so a host with both does not pick the wrong one.
    #[test]
    fn the_initramfs_candidates_prefer_the_package_name() {
        let with_pkg = initramfs_candidates("7.2.9-1-cachyos-bore", Some("linux-cachyos-bore"));
        assert_eq!(with_pkg[0], "initramfs-linux-cachyos-bore.img");
        assert_eq!(
            with_pkg[1], "initramfs-7.2.9-1-cachyos-bore.img",
            "the release-named file is still tried, just after the package's"
        );
        assert_eq!(with_pkg.last().unwrap(), "initramfs-linux-cachyos.img");

        // No `pkgbase`: no package name to try, and the rest still apply.
        let without = initramfs_candidates("7.2.9-1-cachyos-bore", None);
        assert_eq!(without[0], "initramfs-7.2.9-1-cachyos-bore.img");
        assert!(!without.iter().any(|n| n.contains("bore-bore")));
    }

    /// The lookup returns the first candidate that is actually there, and `None`
    /// rather than a path to nothing when none is.
    #[test]
    fn the_distro_initramfs_is_the_first_candidate_that_exists() {
        let dir = std::env::temp_dir().join(format!("snapdev-initrd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        assert_eq!(distro_initramfs(&dir, "7.2.9-nope"), None);

        // The generic fallback alone.
        std::fs::write(dir.join("initramfs-linux-cachyos.img"), b"image").unwrap();
        assert_eq!(
            distro_initramfs(&dir, "7.2.9-nope"),
            Some(dir.join("initramfs-linux-cachyos.img"))
        );

        // Adding a release-named one wins over the generic fallback.
        std::fs::write(dir.join("initramfs-7.2.9-nope.img"), b"image").unwrap();
        assert_eq!(
            distro_initramfs(&dir, "7.2.9-nope"),
            Some(dir.join("initramfs-7.2.9-nope.img"))
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The installed module is found by extension, not by name: the day the series
    /// grows a second module, or the file is renamed, this still answers.
    #[test]
    fn the_installed_module_is_found_under_its_release() {
        let dir = std::env::temp_dir().join(format!("snapdev-inst-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let module_dir = dir
            .join("7.2.9-1-cachyos-bore")
            .join("kernel")
            .join(DEFAULT_MODULE_DIR);
        std::fs::create_dir_all(&module_dir).unwrap();
        std::fs::write(module_dir.join("amdgpu.ko.zst"), b"module").unwrap();
        // A second entry proves the scan picks the `.zst` rather than the first
        // thing it sees.
        std::fs::write(module_dir.join("amdgpu.ko"), b"uncompressed").unwrap();

        let got = installed_module_under(&dir, "7.2.9-1-cachyos-bore");
        assert_eq!(got, Some(module_dir.join("amdgpu.ko.zst")));

        // A release with nothing installed is `None`, not a path to nothing.
        assert_eq!(installed_module_under(&dir, "9.9.9-nope"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The adopted module's digest must describe the bytes that go into the image,
    /// which are the installed bytes — not a re-compressed copy of them.
    #[test]
    fn adopting_a_module_keeps_its_bytes_and_hashes_those() {
        if !have_zstd() {
            eprintln!("skipping: no zstd on PATH");
            return;
        }
        let dir = std::env::temp_dir().join(format!("snapdev-adopt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // A real, compressed file, so `zstd -d` in `adopt_module` has something to
        // do — and so `raw_bytes` is a fact rather than a stub.
        let payload = vec![7u8; 4096];
        let compressed = dir.join("amdgpu.ko.zst");
        zstd_to(&payload, &compressed);

        let work = dir.join("work");
        let prepared = adopt_module(&compressed, &crate::write::System::default(), &work).unwrap();

        assert_eq!(
            prepared.bytes,
            std::fs::read(&compressed).unwrap(),
            "the compressed bytes must be the installed ones, unchanged"
        );
        assert_eq!(prepared.md5, crate::id::md5_hex(&prepared.bytes));
        assert_eq!(
            prepared.raw_bytes,
            payload.len() as u64,
            "raw size is the uncompressed size, which is a different number"
        );
        // The stem decides which member `assemble` replaces, so it has to survive.
        assert_eq!(
            prepared.source.file_name().and_then(|n| n.to_str()),
            Some("amdgpu.ko")
        );
        let _ = std::fs::remove_dir_all(&dir);
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
            versioned: true,
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
        // Single-threaded, this build is minutes instead of seconds on an 8-core
        // box — the difference is one flag, so it is worth pinning.
        assert!(
            makes[0].iter().any(|a| a.starts_with("-j")),
            "the build must be parallel: {:?}",
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

    /// The case that broke on the real tree: no VCS, so no diff to read. It must
    /// fall back to letting `make` decide rather than claiming nothing changed.
    #[test]
    fn an_unversioned_tree_still_builds_the_series_directory() {
        let t = unversioned_tree();
        let (modules, elsewhere) = t.build_set();
        assert_eq!(modules, vec![DEFAULT_MODULE_DIR]);
        assert!(elsewhere.is_empty());

        let p = plan(t, Some("7.0.9-1-cachyos-bore".to_string()));
        assert!(
            p.can_build(),
            "an unversioned tree must not be refused: {:?}",
            p.blockers
        );

        let text = p.describe().join("\n");
        assert!(
            text.contains("no VCS of its own"),
            "it must say why it cannot diff: {text}"
        );
        assert!(
            !text.contains("(clean)"),
            "reporting 'clean' would look like a reason to stop: {text}"
        );
        assert!(
            text.contains("make decides what is stale"),
            "and why it is building that directory: {text}"
        );
    }

    /// A versioned tree really *can* tell us nothing changed, and that stays a
    /// refusal — the fallback must not swallow the genuine case.
    #[test]
    fn a_clean_versioned_tree_is_still_refused() {
        let p = plan(tree(&[]), Some("7.0.9-1-cachyos-bore".to_string()));
        assert!(!p.can_build());
        assert!(
            p.blockers.iter().any(|b| b.contains("clean")),
            "{:?}",
            p.blockers
        );
    }

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

    /// The rule that keeps a sudo-run build from taking the tree with it.
    ///
    /// Observed on blade 115: after `sudo snaptune dev --run --register`, four
    /// objects under the amdgpu directory came back owned by root — exactly the
    /// ones `make` had rewritten — and the developer's next build, as themselves,
    /// would have failed on a file it could not overwrite.
    #[test]
    fn only_root_building_someone_elses_tree_steps_aside() {
        // Root building a user's tree: hand it over.
        assert!(needs_privilege_drop(0, 1000));
        // Root building root's own tree: nothing to hand over.
        assert!(!needs_privilege_drop(0, 0));
        // A user building their own tree: already the right owner.
        assert!(!needs_privilege_drop(1000, 1000));
        // A user building a tree they do not own: their own permissions decide,
        // and silently becoming the other user would be a surprise.
        assert!(!needs_privilege_drop(1000, 0));
    }
}
