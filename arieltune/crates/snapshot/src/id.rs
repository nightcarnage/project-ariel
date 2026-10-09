// SPDX-License-Identifier: GPL-2.0-only
//! Snapshot identity — the three content-addressed formulas.
//!
//! Ported from `bc-250-snapshotter/snapshot` (`compute_snap_id`,
//! `compute_register_id`). The whole value of a snapshot id is that it is
//! *reproducible*: it is how you tell which module, initramfs and vmlinuz a
//! boot actually came from. So this module reproduces the shell byte for byte
//! rather than tidying the algorithm up.
//!
//! ## Three formulas, not one
//!
//! | | made by | formula |
//! |---|---|---|
//! | F1 | `create`, source tree present | `md5("<hash of 4 .c files>-<symvers8>" + "\n")` |
//! | F2 | `create`, no source tree | `md5("<kver>-<module8>")` — no newline |
//! | F3 | `register` / `import` | `md5("<mod8>-<initrd8>-<vmlinuz8>-<symvers8>" + "\n")` |
//!
//! ## The traps, all deliberate
//!
//! 1. **A missing component is the empty string, not an error.** The shell does
//!    `md5sum <path> | cut -c1-8`: a path that cannot be read prints nothing, so
//!    `cut` yields `""` and the id is still computed. [`file_md5_prefix8`]
//!    mirrors that. Returning an error here would change the id of every
//!    treeless or symvers-less snapshot.
//!
//! 2. **`cat f1 f2 f3 f4 | md5sum` is NOT the same case.** When nothing could be
//!    read the pipeline still hashes *empty input*, so the component is
//!    `d41d8cd9`, not `""`. [`concat_md5_prefix8`] keeps that distinction, and
//!    `tests::empty_input_hashes_to_d41d8cd9` pins it.
//!
//! 3. **`echo` vs `printf` is load-bearing.** F1/F3 hash a newline-terminated
//!    string (`echo`); F2 does not (`printf`). Using one for the other silently
//!    changes every id it produces, which is why the two final steps are
//!    separate functions with different names.
//!
//! 4. **F2 hashes the *live installed* module**, not the snapshot's own copy, so
//!    it is only reproducible while that module is unchanged.
//!
//! 5. **F1 records none of its inputs.** Once the tree is gone the id cannot be
//!    re-derived or audited — see `docs/snapshot-tool.md` §3.1 for the three
//!    snapshots on blade 104 where this is already true.
//!
//! The verified vectors in `tests` are real snapshots, not invented:
//! `snap-0848359a`, `snap-67d5ebcf` (F3) and `snap-da794092` (F2) each
//! reproduce exactly from their recorded components.

use std::path::Path;

use md5::{Digest, Md5};

/// Every snapshot id carries this prefix; the directory, the boot files and the
/// GRUB entry `--id` all use it.
pub const PREFIX: &str = "snap-";

/// The four source files F1 concatenates, in the order the shell `cat`s them.
/// Order matters: this is a hash of the concatenation, not of a set.
pub const F1_SOURCE_FILES: [&str; 4] = [
    "gmc_v10_0.c",
    "gfx_v10_0.c",
    "amdgpu_gmc.c",
    "amdgpu_amdkfd_gfx_v10.c",
];

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// MD5 of arbitrary bytes, first 8 hex characters.
///
/// MD5 is used here because the on-disk ids already are MD5 — this is content
/// addressing, not security, and matching the existing ids is the requirement.
pub fn md5_prefix8(bytes: &[u8]) -> String {
    let mut hasher = Md5::new();
    hasher.update(bytes);
    to_hex(&hasher.finalize())[..8].to_string()
}

/// MD5 of arbitrary bytes, all 32 hex characters.
///
/// The full digest is for *recording* (the manifest's `module.md5`, matching the
/// shell tool's `md5sum <module>`) rather than for addressing. Computed from the
/// bytes already in hand, so the manifest cannot end up describing a different
/// file from the one the image was built around.
pub fn md5_hex(bytes: &[u8]) -> String {
    let mut hasher = Md5::new();
    hasher.update(bytes);
    to_hex(&hasher.finalize())
}

/// `md5sum <path> | cut -c1-8`, **including its failure mode**.
///
/// An unreadable path yields `""` (the shell's `cut` sees no stdout), never an
/// error and never a hash of empty input. See trap 1 in the module docs.
pub fn file_md5_prefix8(path: &Path) -> String {
    match std::fs::read(path) {
        Ok(bytes) => md5_prefix8(&bytes),
        Err(_) => String::new(),
    }
}

/// `md5sum <path>`, all 32 hex characters, or `""` when it cannot be read.
///
/// The full digest for recording a *comparison* — "is this module the same as the
/// one its parent stores?" — where an unreadable file must not answer "yes".
pub fn file_md5_hex(path: &Path) -> String {
    match std::fs::read(path) {
        Ok(bytes) => md5_hex(&bytes),
        Err(_) => String::new(),
    }
}

/// `cat <paths...> | md5sum | cut -c1-8`.
///
/// Unreadable inputs contribute nothing; if *none* could be read this hashes
/// empty input, giving `d41d8cd9`. Deliberately different from
/// [`file_md5_prefix8`] — see trap 2.
pub fn concat_md5_prefix8(paths: &[&Path]) -> String {
    let mut buf = Vec::new();
    for path in paths {
        if let Ok(bytes) = std::fs::read(path) {
            buf.extend_from_slice(&bytes);
        }
    }
    md5_prefix8(&buf)
}

/// The final step of F1 and F3: `echo "<combined>" | md5sum | cut -c1-8`.
///
/// `echo` terminates the string with a newline and that newline is part of the
/// hashed bytes.
pub fn id_from_echo_combined(combined: &str) -> String {
    format!(
        "{PREFIX}{}",
        md5_prefix8(format!("{combined}\n").as_bytes())
    )
}

/// The final step of F2: `printf '%s-%s' <a> <b> | md5sum | cut -c1-8`.
///
/// `printf` does **not** add a newline. Not interchangeable with
/// [`id_from_echo_combined`].
pub fn id_from_printf_pair(a: &str, b: &str) -> String {
    format!("{PREFIX}{}", md5_prefix8(format!("{a}-{b}").as_bytes()))
}

/// F1 — `create` with a source tree present.
///
/// `tree` is the kernel source root; the files are read from
/// `<tree>/drivers/gpu/drm/amd/amdgpu/`, and `Module.symvers` from the tree root.
pub fn create_id_from_tree(tree: &Path) -> String {
    let module_src = tree.join("drivers/gpu/drm/amd/amdgpu");
    let paths: Vec<std::path::PathBuf> = F1_SOURCE_FILES
        .iter()
        .map(|name| module_src.join(name))
        .collect();
    let refs: Vec<&Path> = paths.iter().map(|p| p.as_path()).collect();
    let source_hash = concat_md5_prefix8(&refs);
    let symvers_hash = file_md5_prefix8(&tree.join("Module.symvers"));
    id_from_echo_combined(&format!("{source_hash}-{symvers_hash}"))
}

/// F2 — `create` with no source tree.
///
/// `kver` is the kernel release the module belongs to and `module_path` is the
/// **installed** module (`/usr/lib/modules/<kver>/kernel/.../amdgpu.ko.zst`),
/// matching the shell. Note the shell computes this id from the live installed
/// module, so it is only reproducible while that module is unchanged.
pub fn create_id_treeless(kver: &str, module_path: &Path) -> String {
    let module_hash = file_md5_prefix8(module_path);
    id_from_printf_pair(kver, &module_hash)
}

/// F3 — `register` / `import`.
///
/// `symvers_path` is `None` when no source tree is present, which is the normal
/// case for an imported snapshot: the component is then the empty string.
pub fn register_id(
    module_path: &Path,
    initramfs_path: &Path,
    vmlinuz_path: &Path,
    symvers_path: Option<&Path>,
) -> String {
    let module_hash = file_md5_prefix8(module_path);
    let initrd_hash = file_md5_prefix8(initramfs_path);
    let vmlinuz_hash = file_md5_prefix8(vmlinuz_path);
    let symvers_hash = match symvers_path {
        Some(p) => file_md5_prefix8(p),
        None => String::new(),
    };
    id_from_echo_combined(&format!(
        "{module_hash}-{initrd_hash}-{vmlinuz_hash}-{symvers_hash}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real snapshot on blade 104, reproduced from its recorded components.
    /// `symvers` is empty because that blade has no source tree.
    #[test]
    fn register_formula_reproduces_snap_0848359a() {
        let combined = "ecdb619e-f21daf6a-ddb9f7cc-";
        assert_eq!(id_from_echo_combined(combined), "snap-0848359a");
    }

    /// A second real one, to show the first is not a coincidence.
    #[test]
    fn register_formula_reproduces_snap_67d5ebcf() {
        let combined = "2b55d5a3-a033475e-176f3286-";
        assert_eq!(id_from_echo_combined(combined), "snap-67d5ebcf");
    }

    /// Real snapshot on blade 104 created through the treeless path.
    #[test]
    fn treeless_formula_reproduces_snap_da794092() {
        assert_eq!(
            id_from_printf_pair("7.0.9-1-cachyos", "df096b91"),
            "snap-da794092"
        );
    }

    /// Trap 3: the `echo` newline is part of the hash. If someone "tidies"
    /// F1/F3 into the `printf` form, every id changes — this fails loudly.
    #[test]
    fn newline_is_load_bearing() {
        let with_newline = md5_prefix8(b"ecdb619e-f21daf6a-ddb9f7cc-\n");
        let without = md5_prefix8(b"ecdb619e-f21daf6a-ddb9f7cc-");
        assert_ne!(with_newline, without);
        assert_eq!(with_newline, "0848359a");
    }

    /// The same combined components produce *different* ids through the two
    /// final steps, which is exactly why they are separate functions.
    #[test]
    fn echo_and_printf_steps_differ() {
        let combined = "ecdb619e-f21daf6a-ddb9f7cc-";
        let echoed = id_from_echo_combined(combined);
        let printed = id_from_printf_pair(combined, "");
        assert_ne!(echoed, printed);
    }

    /// Trap 2: `md5sum` of empty *input* is a real hash, not the empty string.
    /// This is what `cat` of unreadable files produces (`d41d8cd9` in shell).
    #[test]
    fn empty_input_hashes_to_d41d8cd9() {
        assert_eq!(md5_prefix8(b""), "d41d8cd9");
    }

    /// Trap 1: an unreadable path contributes the empty string.
    #[test]
    fn unreadable_path_contributes_empty_string() {
        let missing = Path::new("/nonexistent/definitely/not/here");
        assert_eq!(file_md5_prefix8(missing), "");
    }

    /// Trap 1 vs trap 2 in one assertion: `concat` of unreadable paths is NOT
    /// the empty string, it is the hash of nothing.
    #[test]
    fn concat_of_missing_files_is_not_empty_string() {
        let missing = Path::new("/nonexistent/definitely/not/here");
        assert_eq!(concat_md5_prefix8(&[missing]), "d41d8cd9");
        assert_ne!(concat_md5_prefix8(&[missing]), "");
    }

    /// F3 with everything readable: a self-consistent round trip through the
    /// file-reading path, checked against the same string step.
    #[test]
    fn register_id_from_real_temp_files() {
        let dir = std::env::temp_dir().join(format!("snapshot-id-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let module = dir.join("amdgpu.ko.zst");
        let initramfs = dir.join("initramfs.img");
        let vmlinuz = dir.join("vmlinuz");
        std::fs::write(&module, b"module-bytes").expect("write module");
        std::fs::write(&initramfs, b"initramfs-bytes").expect("write initramfs");
        std::fs::write(&vmlinuz, b"vmlinuz-bytes").expect("write vmlinuz");

        let expected = id_from_echo_combined(&format!(
            "{}-{}-{}-",
            md5_prefix8(b"module-bytes"),
            md5_prefix8(b"initramfs-bytes"),
            md5_prefix8(b"vmlinuz-bytes"),
        ));
        let got = register_id(&module, &initramfs, &vmlinuz, None);
        assert_eq!(got, expected);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
