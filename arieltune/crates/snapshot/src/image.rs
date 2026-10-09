// SPDX-License-Identifier: GPL-2.0-only
//! Deriving one initramfs image from another.
//!
//! ## The operation
//!
//! A dev snapshot's image is its parent's image with **one member replaced** —
//! the freshly built `amdgpu.ko.zst`. Everything else, including the early
//! microcode and ACPI archive, is carried through byte-for-byte.
//!
//! ## Why not regenerate it
//!
//! The obvious way to build an image containing a new module is `mkinitcpio`, and
//! that is what the shell tool does. But mkinitcpio reads modules out of
//! `/usr/lib/modules/<kver>/`, so the only way it can see a dev module is for the
//! dev module to have been installed over the live one. `snapshot create` does
//! exactly that (step 5) before generating the image (step 7), which is why it
//! first spends two steps preserving the initramfs and copying the known-good
//! module aside — and why three `initramfs-rescue-*` images had accumulated on
//! blade 115 in four days.
//!
//! Deriving from the parent removes the window entirely: the live module tree is
//! never written, so there is nothing to preserve and nothing to rescue. Each
//! iteration is bootable by construction rather than by remembering to snapshot.
//!
//! ## Verification is deliberately not self-referential
//!
//! [`verify_member`] reads the member back with `lsinitcpio`, not with
//! [`crate::cpio`]. Checking our own rewrite with our own parser would agree with
//! itself even when both are wrong — which is precisely how the append approach
//! looked correct until an independent tool disagreed.

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::cpio;

/// What a derivation did, for reporting and for tests to assert on.
#[derive(Debug, Clone)]
pub struct Derived {
    /// Size of the parent image.
    pub parent_bytes: u64,
    /// Size of the new image.
    pub image_bytes: u64,
    /// Bytes carried through untouched (the early archive plus its padding).
    pub carried_bytes: usize,
    /// Size of the replacement member.
    pub member_bytes: usize,
}

impl Derived {
    /// How much the image grew or shrank.
    pub fn delta(&self) -> i64 {
        self.image_bytes as i64 - self.parent_bytes as i64
    }
}

/// Write `dest`: a copy of `parent` in which `member` holds `new_data`.
///
/// `work` is scratch space for the decompress/recompress step; it is created if
/// needed and left in place, since callers often want to inspect it.
///
/// ## Which archive
///
/// The member is searched for in the early archive first, then the compressed one.
/// It is worth being explicit about which wins on these blades, because it is the
/// opposite of the intuitive answer: **the kernel modules are in the uncompressed
/// early archive**, not the compressed one. amdgpu has to load from the initramfs
/// for early KMS, so mkinitcpio keeps it where it can be reached first. On a real
/// image from blade 115 the early archive holds 25 `.ko` files and the compressed
/// archive holds none.
///
/// Guessing "the compressed one" would have been wrong for the only member we
/// actually care about.
pub fn derive(
    parent: &Path,
    member: &str,
    new_data: &[u8],
    dest: &Path,
    work: &Path,
) -> Result<Derived> {
    let bytes = std::fs::read(parent)
        .with_context(|| format!("reading the parent image {}", parent.display()))?;
    let parent_bytes = bytes.len() as u64;

    // Split where the compressed archive begins. The bytes before it are the early
    // archive *plus* mkinitcpio's zero padding, which is why the padding is
    // reproduced rather than computed from the archive length.
    let at = cpio::compressed_at(&bytes)?;
    let (early, compressed) = bytes.split_at(at);

    std::fs::create_dir_all(work)
        .with_context(|| format!("creating scratch space {}", work.display()))?;

    let mut out: Vec<u8> = Vec::with_capacity(bytes.len() + new_data.len());
    if cpio::contains(early, member) {
        // Rewrite the early archive; the compressed half is carried through
        // byte-for-byte, which keeps the change as small as it can be.
        out.extend_from_slice(&cpio::replace_member(early, member, new_data)?);
        // The compressed archive must still start on a 4 KiB boundary, and a
        // grown early archive would otherwise shift it.
        while !out.len().is_multiple_of(PAD_TO) {
            out.push(0);
        }
        out.extend_from_slice(compressed);
        std::fs::write(dest, &out)
            .with_context(|| format!("writing the image {}", dest.display()))?;
        return Ok(Derived {
            parent_bytes,
            image_bytes: out.len() as u64,
            carried_bytes: compressed.len(),
            member_bytes: new_data.len(),
        });
    }

    // Not in the early archive: decompress the second one and rewrite it there.
    let plain = work.join("main.cpio");
    let repacked = work.join("main.new.cpio.zst");
    decompress(compressed, &plain)?;
    let main = std::fs::read(&plain).with_context(|| format!("reading {}", plain.display()))?;
    let patched = cpio::replace_member(&main, member, new_data)
        .with_context(|| format!("replacing {member} in {}", parent.display()))?;
    std::fs::write(&plain, &patched).with_context(|| format!("writing {}", plain.display()))?;
    compress(&plain, &repacked)?;

    out.extend_from_slice(early);
    out.extend_from_slice(
        &std::fs::read(&repacked).with_context(|| format!("reading {}", repacked.display()))?,
    );
    std::fs::write(dest, &out).with_context(|| format!("writing the image {}", dest.display()))?;

    Ok(Derived {
        parent_bytes,
        image_bytes: out.len() as u64,
        carried_bytes: early.len(),
        member_bytes: new_data.len(),
    })
}

/// Boundary mkinitcpio aligns the compressed archive to.
const PAD_TO: usize = 4096;

/// Read a member back out of an image, using `cpio`.
///
/// The independent oracle. Checking our own rewrite with [`crate::cpio`] would
/// agree with itself even when both are wrong — which is exactly how the append
/// approach looked correct until a different tool disagreed.
///
/// `cpio` reads the archive directly out of the file, so it sees the **early**
/// (uncompressed) archive, which is where the modules live. That is the case this
/// exists to check; for a member of the compressed archive it will not find
/// anything, and says so rather than returning empty bytes as though that were
/// the contents.
pub fn verify_member(image: &Path, member: &str) -> Result<Vec<u8>> {
    let file = std::fs::File::open(image)
        .with_context(|| format!("opening the image {}", image.display()))?;
    let out = Command::new("cpio")
        .args(["-i", "--to-stdout", "--quiet"])
        .arg(member)
        .stdin(std::process::Stdio::from(file))
        .stderr(std::process::Stdio::piped())
        .output()
        .context("running cpio (is cpio installed?)")?;

    if out.stdout.is_empty() {
        // `cpio -i --to-stdout` exits 0 whether or not the pattern matched, so the
        // exit status says nothing about whether the member was found and
        // emptiness is the only signal available. That makes this correct only for
        // members that have content — which is the case that matters, since the
        // thing being verified is a compressed module. A *directory* member is
        // legitimately empty and would be reported as absent.
        bail!(
            "cpio extracted nothing for {member} from {} (absent, or a directory)",
            image.display()
        );
    }
    if !out.status.success() {
        bail!(
            "cpio failed on {member} in {}: {}",
            image.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// Does this image carry this member?
pub fn has_member(image: &Path, member: &str) -> bool {
    verify_member(image, member).is_ok()
}

/// The full path of the member whose path ends in `suffix`, from the early archive.
///
/// Taken from the parent image rather than constructed from the kernel release:
/// the path inside an initramfs is mkinitcpio's business, and assembling it from
/// a version string is how you end up replacing a file that is not the one the
/// machine boots.
pub fn find_member(image: &Path, suffix: &str) -> Result<String> {
    let bytes =
        std::fs::read(image).with_context(|| format!("reading the image {}", image.display()))?;
    let at = cpio::compressed_at(&bytes)?;
    // The early archive is where the modules are; see `derive`.
    let early = cpio::parse(&bytes[..at])?;
    early
        .iter()
        .find(|m| m.name.ends_with(suffix))
        .map(|m| m.name.clone())
        .with_context(|| {
            format!(
                "no member ending in {suffix:?} in the early archive of {}",
                image.display()
            )
        })
}

fn decompress(input: &[u8], dest: &Path) -> Result<()> {
    let out = run_zstd(&["-d"], input, dest)?;
    if !out {
        bail!("zstd could not decompress the archive");
    }
    Ok(())
}

fn compress(input: &Path, dest: &Path) -> Result<()> {
    let bytes = std::fs::read(input)?;
    run_zstd(&[], &bytes, dest).map(|_| ())
}

/// Run `zstd` over a buffer using files as the pipe.
///
/// Files rather than stdin/stdout pipes on purpose: 54 MiB in and out through two
/// 64 KiB pipes deadlocks as soon as both sides fill. The cost is one temp copy,
/// which is nothing next to the build this is feeding.
fn run_zstd(flags: &[&str], input: &[u8], dest: &Path) -> Result<bool> {
    let src = dest.with_extension("in");
    std::fs::write(&src, input)?;
    let status = Command::new("zstd")
        .args(flags)
        .arg("-q")
        .arg("-f")
        .arg(&src)
        .arg("-o")
        .arg(dest)
        .status()
        .context("running zstd")?
        .success();
    let _ = std::fs::remove_file(&src);
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODULE: &str =
        "usr/lib/modules/7.2.9-1-cachyos-bore/kernel/drivers/gpu/drm/amd/amdgpu/amdgpu.ko.zst";

    fn have_zstd() -> bool {
        Command::new("zstd")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Build an image shaped like the real one: an early archive, zero padding,
    /// then a zstd'd archive holding the modules.
    fn synthetic_image(dir: &Path, module: &[u8]) -> std::path::PathBuf {
        let early = cpio::build(&[
            ("early_cpio", b"1"),
            ("kernel/x86/microcode/AuthenticAMD.bin", b"ucode"),
        ]);
        let main = cpio::build(&[
            ("usr/lib/modules/7.2.9-1-cachyos-bore/modules.dep", b"dep"),
            (MODULE, module),
        ]);

        let main_zst = dir.join("main.zst");
        let main_plain = dir.join("main.plain");
        std::fs::write(&main_plain, &main).unwrap();
        run_zstd(&[], &main, &main_zst).unwrap();

        let mut image = early;
        // mkinitcpio's zero padding to a 4 KiB boundary.
        while !image.len().is_multiple_of(4096) {
            image.push(0);
        }
        image.extend_from_slice(&std::fs::read(&main_zst).unwrap());

        let path = dir.join("parent.img");
        std::fs::write(&path, &image).unwrap();
        path
    }

    #[test]
    fn derive_replaces_the_member_and_carries_the_rest_through() {
        if !have_zstd() {
            eprintln!("zstd not installed; skipping");
            return;
        }
        let dir = std::env::temp_dir().join(format!("snap-image-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let parent = synthetic_image(&dir, b"OLD MODULE BYTES");
        let parent_bytes = std::fs::read(&parent).unwrap();
        let dest = dir.join("child.img");
        let new_module = b"NEW MODULE BYTES, A DIFFERENT LENGTH ENTIRELY";

        let report = derive(&parent, MODULE, new_module, &dest, &dir.join("work")).unwrap();

        assert_eq!(report.parent_bytes as usize, parent_bytes.len());
        assert_eq!(report.member_bytes, new_module.len());

        // The replacement landed...
        let child = std::fs::read(&dest).unwrap();
        let at = cpio::compressed_at(&child).unwrap();
        assert_eq!(
            &child[..at],
            &parent_bytes[..cpio::compressed_at(&parent_bytes).unwrap()],
            "the early archive and padding must be byte-identical"
        );

        let main = all_members(&zstd_plain(&dest, &dir));
        let member = main.iter().find(|m| m.name == MODULE).unwrap();
        assert_eq!(member.data_len, new_module.len());

        // ...and the parent is untouched.
        assert_eq!(std::fs::read(&parent).unwrap(), parent_bytes);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn derive_refuses_a_member_the_parent_does_not_have() {
        if !have_zstd() {
            eprintln!("zstd not installed; skipping");
            return;
        }
        let dir = std::env::temp_dir().join(format!("snap-image-miss-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let parent = synthetic_image(&dir, b"OLD");
        let err = derive(
            &parent,
            "usr/lib/modules/7.2.9-1-cachyos-bore/no/such.ko.zst",
            b"x",
            &dir.join("child.img"),
            &dir.join("work"),
        )
        .unwrap_err();

        assert!(
            format!("{err:#}").contains("replacing"),
            "the error should name the step: {err:#}"
        );
        assert!(!dir.join("child.img").exists(), "no image on failure");

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn zstd_plain(image: &Path, dir: &Path) -> Vec<u8> {
        let bytes = std::fs::read(image).unwrap();
        let at = cpio::compressed_at(&bytes).unwrap();
        let out = dir.join("check.cpio");
        run_zstd(&["-d"], &bytes[at..], &out).unwrap();
        std::fs::read(&out).unwrap()
    }

    /// Every member across every archive in a decompressed stream.
    ///
    /// A real image holds several concatenated archives and the kernel modules
    /// are not in the first one, so `cpio::parse` alone finds no modules at all.
    fn all_members(stream: &[u8]) -> Vec<cpio::Member> {
        cpio::parse_concatenated(stream)
            .unwrap()
            .into_iter()
            .flat_map(|archive| archive.members)
            .collect()
    }

    /// The assertion that matters, against a **real** image from a blade — and it
    /// uses `lsinitcpio`, an oracle independent of this crate, because checking our
    /// own rewrite with our own parser would agree with itself even when both are
    /// wrong.
    ///
    /// ```text
    /// SNAPDEV_IMAGE=/var/tmp/snapdev/real.img \
    ///   cargo test -p snapshot real_image_derive -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "needs a real initramfs; set SNAPDEV_IMAGE"]
    fn real_image_derive() {
        let Ok(path) = std::env::var("SNAPDEV_IMAGE") else {
            println!("SNAPDEV_IMAGE not set; nothing to check");
            return;
        };
        let parent = Path::new(&path);
        let dir = std::env::temp_dir().join(format!("snap-image-real-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let bytes = std::fs::read(parent).unwrap();
        let at = cpio::compressed_at(&bytes).unwrap();

        // The modules live in the EARLY archive on these blades, so that is where
        // the test has to look for amdgpu — the same fact that decided which
        // archive `derive` rewrites.
        let early = cpio::parse(&bytes[..at]).unwrap();
        let module = early
            .iter()
            .find(|m| m.name.ends_with("amdgpu.ko.zst"))
            .expect("the early archive must carry amdgpu");
        let member = module.name.clone();
        println!(
            "early archive: {} members, module at data offset {}",
            early.len(),
            module.data_at
        );

        let before = verify_member(parent, &member).expect("lsinitcpio reads the parent");
        let replacement = vec![0xA5u8; before.len() + 1234];

        let dest = dir.join("child.img");
        let report = derive(parent, &member, &replacement, &dest, &dir.join("work")).unwrap();
        println!(
            "parent {} bytes -> child {} bytes (delta {}); carried {} compressed bytes; module {} -> {}",
            report.parent_bytes,
            report.image_bytes,
            report.delta(),
            report.carried_bytes,
            before.len(),
            report.member_bytes
        );

        let after = verify_member(&dest, &member).expect("lsinitcpio reads the child");
        assert_ne!(after, before, "the member must actually have changed");
        assert_eq!(after, replacement, "and must hold exactly the new bytes");

        // Rewriting the early archive means the compressed half — tens of MB — is
        // carried through untouched. That is the property that makes this cheap.
        let child = std::fs::read(&dest).unwrap();
        let child_at = cpio::compressed_at(&child).unwrap();
        assert_eq!(
            &child[child_at..],
            &bytes[at..],
            "the compressed half must be byte-identical"
        );
        assert!(
            child_at.is_multiple_of(PAD_TO),
            "the compressed archive must stay on its boundary"
        );

        // Other members survive: the image is still a usable initramfs.
        //
        // Regular files only: a directory has no content, and a symlink's "data"
        // is its target, which `cpio --to-stdout` does not emit — so either would
        // be reported as absent by a check that cannot distinguish "gone" from
        // "nothing to extract".
        let files: Vec<_> = early
            .iter()
            .filter(|m| m.is_regular_file() && m.data_len > 0)
            .take(3)
            .collect();
        assert!(
            !files.is_empty(),
            "the early archive should hold regular files"
        );
        for other in files {
            assert!(
                has_member(&dest, &other.name),
                "unrelated member {} must survive",
                other.name
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
