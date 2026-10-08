// SPDX-License-Identifier: GPL-2.0-only
//! Compute-unit topology: read the live CU bitmap and render the harvest map.
//!
//! We talk to amdgpu directly via the DRM `AMDGPU_INFO` ioctl (query
//! `DEV_INFO` = 0x16) — no libdrm dependency. The returned
//! `drm_amdgpu_info_device` layout (offsets validated on a real BC-250 by the
//! original `cu_map.sh`):
//!   num_shader_engines            @ 20
//!   num_shader_arrays_per_engine  @ 24
//!   cu_active_number              @ 48
//!   cu_bitmap[4][4]              @ 56
//!
//! Each shader array on Cyan Skillfish has 5 WGPs = 10 CU slots.

use std::fs::{self, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::Path;

use anyhow::{Context, Result};

/// modprobe.d drop-in that persists the 40-CU liberation across reboots.
///
/// **This file is shared.** Anything in it that is not one of [`OWNED_KEYS`]
/// belongs to whoever put it there — on a real blade that is
/// `bc250_sdma_fw=navi12` and `bc250_early_sdma_trap=1`, SDMA stability
/// parameters with nothing to do with CU routing, and often the *only* copy of
/// those settings on the machine (`bc250_early_sdma_trap` is not on the kernel
/// command line). So this file is edited **line by line**, never wholesale.
///
/// It used to be overwritten in full on enable and deleted outright on disable.
/// Both directions silently dropped those foreign parameters, which is the kind
/// of change that shows up hours later as an unexplained hang rather than as an
/// error. See [`rewrite_dropin`] for the rules that replaced that.
pub const MODPROBE_CONF: &str = "/etc/modprobe.d/aputune-40cu.conf";

/// The `amdgpu` parameters this module takes responsibility for.
///
/// Exactly these keys are created, updated or removed. Every other line in the
/// drop-in — other modules, other `amdgpu` parameters, comments, blank lines —
/// is preserved.
const OWNED_KEYS: [&str; 2] = ["bc250_cc_write_mode", "bc250_flush_by_runlist"];

/// Values [`enable_persist`] writes.
///
/// `cc_write_mode=3` *is* the 40-CU liberation. `flush_by_runlist=1` is the
/// runlist rebuild flush (patch 25), which has always travelled with it here —
/// kept so existing installs do not change behaviour, even though it is also on
/// the kernel command line on the blades we run.
const ENABLE_KEYS: [(&str, &str); 2] = [
    ("bc250_cc_write_mode", "3"),
    ("bc250_flush_by_runlist", "1"),
];

/// Path of our drop-in *inside* the initramfs, for post-rebuild verification.
const DROPIN_IN_IMAGE: &str = "etc/modprobe.d/aputune-40cu.conf";

const AMDGPU_INFO_DEV_INFO: u32 = 0x16;
const DRM_IOCTL_BASE: u32 = 0x64; // 'd'
const DRM_COMMAND_BASE: u32 = 0x40;

// _IOWR(0x64, 0x40 + 0x05, struct drm_amdgpu_info) with struct = 32 bytes.
const fn iowr(nr: u32, size: u32) -> u64 {
    let dir = 3u32; // _IOC_READ | _IOC_WRITE
    ((dir << 30) | (size << 16) | (DRM_IOCTL_BASE << 8) | nr) as u64
}
const DRM_IOCTL_AMDGPU_INFO: u64 = iowr(DRM_COMMAND_BASE + 0x05, 32);

#[repr(C)]
struct DrmAmdgpuInfo {
    return_pointer: u64,
    return_size: u32,
    query: u32,
    _union: [u32; 4],
}

const RENDER_NODES: &[&str] = &["/dev/dri/renderD128", "/dev/dri/renderD129"];

/// Raw `DEV_INFO` buffer from the first render node that answers.
fn dev_info() -> Option<[u8; 1024]> {
    for node in RENDER_NODES {
        let Ok(f) = OpenOptions::new().read(true).write(true).open(node) else {
            continue;
        };
        let mut buf = [0u8; 1024];
        let mut req = DrmAmdgpuInfo {
            return_pointer: buf.as_mut_ptr() as u64,
            return_size: buf.len() as u32,
            query: AMDGPU_INFO_DEV_INFO,
            _union: [0; 4],
        };
        let rc = unsafe {
            libc::ioctl(
                f.as_raw_fd(),
                DRM_IOCTL_AMDGPU_INFO as libc::Ioctl,
                &mut req as *mut _,
            )
        };
        if rc == 0 {
            return Some(buf);
        }
    }
    None
}

fn u32_at(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

/// One shader array's CU layout.
pub struct ArrayMap {
    pub se: u32,
    pub sh: u32,
    pub bitmap: u32,
    pub active: u32,
    /// "full" | "contiguous" | "scattered"
    pub pattern: &'static str,
}

pub struct CuMap {
    // se/sh counts are parsed for the DEV_INFO layout's sake and future
    // multi-engine rendering; the current render keys off `arrays` directly.
    #[allow(dead_code)]
    pub num_se: u32,
    #[allow(dead_code)]
    pub num_sh: u32,
    pub active: u32,
    pub possible: u32,
    pub arrays: Vec<ArrayMap>,
}

const SLOTS_PER_ARRAY: usize = 10; // 5 WGP x 2 CU

fn classify(bitmap: u32) -> &'static str {
    let disabled: Vec<usize> = (0..SLOTS_PER_ARRAY)
        .filter(|i| bitmap & (1 << i) == 0)
        .collect();
    if disabled.is_empty() {
        "full"
    } else if disabled.windows(2).all(|w| w[1] == w[0] + 1) {
        "contiguous"
    } else {
        "scattered"
    }
}

/// Read the full harvest map. None if amdgpu can't be queried.
pub fn map() -> Option<CuMap> {
    let buf = dev_info()?;
    let num_se = u32_at(&buf, 20);
    let num_sh = u32_at(&buf, 24);
    let active = u32_at(&buf, 48);
    if num_se == 0 || num_se > 8 || num_sh == 0 || num_sh > 8 {
        return None; // implausible — wrong node / not amdgpu
    }
    let mut arrays = Vec::new();
    for se in 0..num_se {
        for sh in 0..num_sh {
            let bm = u32_at(&buf, 56 + ((se * 4 + sh) * 4) as usize);
            arrays.push(ArrayMap {
                se,
                sh,
                bitmap: bm,
                active: bm.count_ones(),
                pattern: classify(bm),
            });
        }
    }
    let possible = num_se * num_sh * SLOTS_PER_ARRAY as u32;
    Some(CuMap {
        num_se,
        num_sh,
        active,
        possible,
        arrays,
    })
}

/// Active CU count from amdgpu, if available. Used by patch detection (12).
pub fn active_cu_count() -> Option<u32> {
    map().map(|m| m.active)
}

/// Per-shader-array DRIVER WGP enumeration — how the APU came up at boot: the
/// harvest topology amdgpu read from the silicon (fuses + CC), including any
/// fuse-broken WGPs. A WGP counts as driver-enabled if amdgpu enumerated either
/// of its two CUs. This is the boot ground truth (DEV_INFO's `cu_bitmap` is set
/// at gfx init and is NOT changed by live CC writes), so it's distinct from the
/// live SPI dispatch mask (`curoute`): a WGP can be enumerated but SPI-gated, or
/// — on a marginal APU — never enumerated at all (a broken WGP).
///
/// Indices match `curoute::ARRAYS`: [SE0.SH0, SE0.SH1, SE1.SH0, SE1.SH1].
pub fn driver_wgp_masks() -> Option<[u32; 4]> {
    let m = map()?;
    let mut out = [0u32; 4];
    for a in &m.arrays {
        let idx = (a.se * 2 + a.sh) as usize;
        if idx >= 4 {
            continue;
        }
        let mut wm = 0u32;
        for w in 0..5u32 {
            // Two CU slots per WGP: bits (2w, 2w+1).
            if a.bitmap & (0x3 << (w * 2)) != 0 {
                wm |= 1 << w;
            }
        }
        out[idx] = wm;
    }
    Some(out)
}

/// Whether the 40-CU liberation is currently armed via the module parameter.
pub fn liberation_armed() -> bool {
    fs::read_to_string("/sys/module/amdgpu/parameters/bc250_cc_write_mode")
        .map(|s| s.trim() != "0" && !s.trim().is_empty())
        .unwrap_or(false)
}

/// Apply `set`/`remove` to the contents of a modprobe.d drop-in, touching only
/// the named `amdgpu` keys and leaving every other line exactly as it was.
///
/// This is pure on purpose, and unit-tested: its predecessor rewrote the file
/// from a constant and destroyed unrelated configuration, so the replacement is
/// worth *proving* rather than eyeballing.
///
/// Rules:
/// * A line shaped `options amdgpu <key>=<value>` whose `<key>` is in `set` is
///   rewritten **in place**, so the file keeps its original ordering.
/// * A line whose `<key>` is in `remove` is dropped whole. `remove` wins over
///   `set` for the same key — callers never ask for both, but the precedence is
///   defined rather than accidental.
/// * A `set` key with no existing line is appended at the end.
/// * Duplicate lines for a `set` key are collapsed to one.
/// * Anything else — other modules, other `amdgpu` parameters, comments, blank
///   lines, indentation — passes through untouched.
///
/// The returned text always ends in a newline if it is non-empty, which is what
/// modprobe.d files are expected to look like.
fn rewrite_dropin(text: &str, set: &[(&str, &str)], remove: &[&str]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut applied: Vec<&str> = Vec::new();

    for line in text.lines() {
        // Recognise exactly the shape we own; everything else is not ours.
        let parsed = line
            .trim()
            .strip_prefix("options amdgpu ")
            // A trailing comment is not part of the value.
            .and_then(|rest| rest.split('#').next())
            .map(str::trim)
            .and_then(|kv| kv.split_once('='))
            .map(|(k, v)| (k.trim(), v.trim()));

        let Some((key, _value)) = parsed else {
            out.push(line.to_string());
            continue;
        };

        if remove.contains(&key) {
            continue; // our option being turned off: drop the whole line
        }

        match set.iter().find(|(k, _)| *k == key) {
            Some((k, v)) => {
                if applied.contains(k) {
                    continue; // duplicate of a key we already wrote
                }
                applied.push(k);
                out.push(format!("options amdgpu {k}={v}"));
            }
            // Someone else's amdgpu parameter: keep it byte-for-byte.
            None => out.push(line.to_string()),
        }
    }

    for (k, v) in set {
        if !applied.contains(k) {
            out.push(format!("options amdgpu {k}={v}"));
        }
    }

    let mut body = out.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    body
}

/// Post-rebuild verification note for the returned description.
///
/// We *confirm* the drop-in reached the rebuilt image instead of asserting it.
/// This is the check whose absence made the old `mkinitcpio -P` advice look like
/// it worked: the command succeeded and the file was simply not in the image the
/// machine boots.
fn dropin_image_note() -> String {
    let Some(image) = crate::initramfs::boot_initramfs() else {
        return String::new();
    };
    if crate::initramfs::image_contains(&image, DROPIN_IN_IMAGE) {
        format!("; drop-in confirmed inside {}", image.display())
    } else {
        format!(
            "; WARNING: {} does not list {DROPIN_IN_IMAGE} — the change may not take \
             effect; check with `lsinitcpio {}`",
            image.display(),
            image.display()
        )
    }
}

/// Persist the 40-CU liberation and rebuild the image that will actually boot.
///
/// Requires root and the 40-CU kernel patch. Returns a human-readable
/// description of what changed, for the CLI to print — including *which* image
/// was rebuilt, because that is the part that used to be wrong.
///
/// Other people's `amdgpu` parameters in the drop-in are left alone.
pub fn enable_persist() -> Result<String> {
    let existing = fs::read_to_string(MODPROBE_CONF).unwrap_or_default();
    fs::write(MODPROBE_CONF, rewrite_dropin(&existing, &ENABLE_KEYS, &[]))
        .with_context(|| format!("write {MODPROBE_CONF} (need root)"))?;

    let rebuilt = crate::initramfs::rebuild_boot_image()?;
    Ok(format!(
        "armed 40-CU in {MODPROBE_CONF}; {rebuilt}{}",
        dropin_image_note()
    ))
}

/// Remove the persisted liberation and rebuild the image that will actually boot.
///
/// Only [`OWNED_KEYS`] are removed — in particular the SDMA stability parameters
/// stay put. Turning CU routing off must not quietly change anything else about
/// how amdgpu loads.
///
/// If our keys were the only thing in the file, the file is removed rather than
/// left as an empty drop-in.
pub fn disable_persist() -> Result<String> {
    let existing = fs::read_to_string(MODPROBE_CONF).unwrap_or_default();
    let updated = rewrite_dropin(&existing, &[], &OWNED_KEYS);

    if updated.trim().is_empty() {
        if Path::new(MODPROBE_CONF).exists() {
            fs::remove_file(MODPROBE_CONF)
                .with_context(|| format!("remove {MODPROBE_CONF} (need root)"))?;
        }
    } else {
        fs::write(MODPROBE_CONF, updated)
            .with_context(|| format!("write {MODPROBE_CONF} (need root)"))?;
    }

    let rebuilt = crate::initramfs::rebuild_boot_image()?;
    Ok(format!(
        "disarmed 40-CU in {MODPROBE_CONF}; {rebuilt}{}",
        dropin_image_note()
    ))
}

/// Whether the persisted drop-in is in place.
pub fn persist_present() -> bool {
    Path::new(MODPROBE_CONF).exists()
}

/// Render the harvest map as ASCII (filled vs harvested per slot).
pub fn render(m: &CuMap) -> String {
    let mut out = String::new();
    for a in &m.arrays {
        let bar: String = (0..SLOTS_PER_ARRAY)
            .map(|i| if a.bitmap & (1 << i) != 0 { '#' } else { '.' })
            .collect();
        out.push_str(&format!(
            "SE{} SH{}: {}  {:>2} CU  {}\n",
            a.se, a.sh, bar, a.active, a.pattern
        ));
    }
    let harvested = m.possible - m.active;
    out.push_str(&format!(
        "{}/{} CUs active, {} harvested\n",
        m.active, m.possible, harvested
    ));
    out
}

#[cfg(test)]
mod dropin_tests {
    use super::*;

    /// The bug this replaces: foreign `amdgpu` parameters must survive *both*
    /// directions. These are the real values found on blade 115.
    #[test]
    fn foreign_parameters_are_never_touched() {
        let on_disk = "\
options amdgpu bc250_cc_write_mode=3
options amdgpu bc250_flush_by_runlist=1
options amdgpu bc250_sdma_fw=navi12
options amdgpu bc250_early_sdma_trap=1
";
        let disarmed = rewrite_dropin(on_disk, &[], &OWNED_KEYS);
        assert!(
            disarmed.contains("bc250_sdma_fw=navi12"),
            "SDMA firmware selection was dropped: {disarmed}"
        );
        assert!(
            disarmed.contains("bc250_early_sdma_trap=1"),
            "early SDMA trap (conf-only, not on the cmdline) was dropped: {disarmed}"
        );
        assert!(!disarmed.contains("bc250_cc_write_mode"));
        assert!(!disarmed.contains("bc250_flush_by_runlist"));

        // ...and enabling must not delete them either.
        let rearmed = rewrite_dropin(&disarmed, &ENABLE_KEYS, &[]);
        assert!(rearmed.contains("bc250_sdma_fw=navi12"));
        assert!(rearmed.contains("bc250_early_sdma_trap=1"));
        assert!(rearmed.contains("bc250_cc_write_mode=3"));
    }

    /// Enabling on a box that has never had the drop-in writes both keys.
    #[test]
    fn enable_creates_the_file_from_nothing() {
        let out = rewrite_dropin("", &ENABLE_KEYS, &[]);
        assert_eq!(
            out,
            "options amdgpu bc250_cc_write_mode=3\noptions amdgpu bc250_flush_by_runlist=1\n"
        );
    }

    /// An existing line is rewritten where it stands, so the file keeps its
    /// order instead of shuffling to the bottom.
    #[test]
    fn existing_key_is_rewritten_in_place() {
        let text = "\
# my tuning
options amdgpu bc250_cc_write_mode=0
options amdgpu bc250_sdma_fw=navi12
";
        let out = rewrite_dropin(text, &ENABLE_KEYS, &[]);
        assert_eq!(
            out,
            "# my tuning\n\
             options amdgpu bc250_cc_write_mode=3\n\
             options amdgpu bc250_sdma_fw=navi12\n\
             options amdgpu bc250_flush_by_runlist=1\n"
        );
    }

    /// Applying the same edit twice is a no-op the second time — the command is
    /// idempotent, which matters because boot units and profiles both call it.
    #[test]
    fn edits_are_idempotent() {
        let once = rewrite_dropin("", &ENABLE_KEYS, &[]);
        let twice = rewrite_dropin(&once, &ENABLE_KEYS, &[]);
        assert_eq!(once, twice);

        let off = rewrite_dropin(&twice, &[], &OWNED_KEYS);
        assert_eq!(off, "");
        assert_eq!(rewrite_dropin(&off, &[], &OWNED_KEYS), "");
    }

    /// Duplicate lines for an owned key collapse to one rather than multiplying.
    #[test]
    fn duplicates_collapse() {
        let text = "\
options amdgpu bc250_cc_write_mode=0
options amdgpu bc250_cc_write_mode=1
";
        let out = rewrite_dropin(text, &ENABLE_KEYS, &[]);
        assert_eq!(out.matches("bc250_cc_write_mode").count(), 1, "{out}");
    }

    /// Comments, blank lines and other modules are not ours to reformat.
    #[test]
    fn unrelated_lines_pass_through() {
        let text = "\
# keep this comment

options nct6687 force=1
options amdgpu ppfeaturemask=0xfff77ef7
";
        assert_eq!(rewrite_dropin(text, &[], &OWNED_KEYS), text);
    }
}
