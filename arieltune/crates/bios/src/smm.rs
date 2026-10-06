// SPDX-License-Identifier: GPL-2.0-only
//! SMM SPI-flash medium — the no-rig OEM write path.
//!
//! Drives the `smiflash` kernel module (`/proc/smiflash`), which fires AMI's
//! SmiFlash SW-SMI handler to read/write the SPI flash from SMM context. This is
//! how biostune changes OEM `Setup` settings with no external programmer and
//! without going through the firmware's locked SetVariable path.
//!
//! The module must be loaded with the platform's real SW-SMI command port (the
//! FADT SMI_CMD field — 0xB0 on the BC-250, NOT the conventional 0xB2):
//!     sudo insmod smiflash.ko smi_port=0xB0
//! biostune can't insmod for you; `available()` reports if it's loaded.
//!
//! Addressing: `field = chip offset directly` (the handler biases it into the
//! 0xFF000000 MMIO window). Writes are AND-only (program bits 1->0; no erase) —
//! the OEM-edit path appends into erased (0xFF) free space, so that's fine.

use crate::kmod;
use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::process::Command;

const PROC: &str = "/proc/smiflash";
const MAXDATA: usize = 256;

/// Where a loose (non-DKMS) `smiflash.ko` is looked for, in order. The DKMS
/// install (the recommended path) makes the module available via `modprobe`
/// instead — `load()` tries that first.
const KO_PATHS: &[&str] = &["/usr/lib/biostune/smiflash.ko", "/tmp/smiflash.ko"];

/// Where `biostune install.sh` stages the DKMS driver sources, so
/// `biostune driver build` can (re)build them after install.
const DRIVER_SRC_DIRS: &[&str] = &["/usr/share/biostune/driver", "driver"];

/// The real SW-SMI command port from the ACPI FADT (`SMI_CMD` @ offset 48).
/// On the BC-250 this is 0xB0 — NOT the conventional 0xB2 (writing 0xB2 raises no
/// SMI at all). Never hardcode it; ask the firmware.
pub fn smi_cmd_port() -> io::Result<u16> {
    let d = std::fs::read("/sys/firmware/acpi/tables/FACP")?;
    if d.len() < 52 || &d[0..4] != b"FACP" {
        return Err(io::Error::other("FADT not present or malformed"));
    }
    let smi_cmd = u32::from_le_bytes([d[48], d[49], d[50], d[51]]);
    if smi_cmd == 0 || smi_cmd > 0xFFFF {
        return Err(io::Error::other(format!(
            "FADT SMI_CMD = 0x{smi_cmd:x} (no usable SW-SMI command port)"
        )));
    }
    Ok(smi_cmd as u16)
}

/// Find the installed loose smiflash.ko, if any.
pub fn ko_path() -> Option<&'static str> {
    KO_PATHS.iter().copied().find(|p| Path::new(p).exists())
}

/// Human-readable install state of the module for `driver status`.
pub fn install_state() -> String {
    if kmod::modprobe_resolves() {
        "installed (modprobe smiflash)".into()
    } else if let Some(p) = ko_path() {
        format!("loose module at {p}")
    } else if kmod::have_blob(&kmod::running_kver()) {
        format!(
            "not installed — a prebuilt for {} ships in this binary; \
             `arieltune bios driver build` installs it",
            kmod::running_kver()
        )
    } else {
        format!(
            "not installed — no prebuilt for {}; `arieltune bios driver build` \
             compiles it on the board (needs dkms + kernel headers)",
            kmod::running_kver()
        )
    }
}

/// Load the smiflash driver (idempotent) with the FADT-derived SW-SMI port.
/// No-op if already loaded. Needs root and an installed smiflash.ko.
pub fn load() -> io::Result<()> {
    if Smm::available() {
        return Ok(());
    }
    let port = smi_cmd_port()?;
    // Prefer the module this binary carries for this kernel — that path needs
    // no dkms, no headers and no build tree, so it survives the kernel upgrade
    // that removes an older release's tree. Then a DKMS install someone else
    // made, then a loose smiflash.ko for the manual/dev path.
    if !kmod::modprobe_resolves() {
        let _ = kmod::install_embedded();
    }
    if kmod::modprobe_resolves() {
        let status = Command::new("modprobe")
            .arg("smiflash")
            .arg(format!("smi_port=0x{port:x}"))
            .status()?;
        if status.success() && Smm::available() {
            return Ok(());
        }
    }
    let ko = ko_path().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "smiflash module not installed for {}. Run `arieltune bios driver build` — \
                 it installs the prebuilt this binary carries, or compiles it on the board \
                 when we ship none for your kernel.",
                kmod::running_kver()
            ),
        )
    })?;
    let status = Command::new("insmod")
        .arg(ko)
        .arg(format!("smi_port=0x{port:x}"))
        .status()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "insmod {ko} smi_port=0x{port:x} failed (root? kernel-version match?)"
        )));
    }
    if !Smm::available() {
        return Err(io::Error::other(
            "insmod reported ok but /proc/smiflash absent",
        ));
    }
    Ok(())
}

/// Make the smiflash module available for the running kernel.
///
/// Installs the prebuilt module this binary carries whenever we have one for the
/// running kernel — no `dkms`, no kernel headers, no build tree, so it works on a
/// snapshot boot whose tree is gone. Only for a kernel we ship none of does it
/// fall back to compiling on the board via the staged `driver/install-dkms.sh`
/// (idempotent; needs root, `dkms` and headers, which the prepare hook makes
/// usable on a BC-250). Returns the script's output on failure.
pub fn build() -> io::Result<()> {
    if !kmod::install_embedded() {
        let script = DRIVER_SRC_DIRS
            .iter()
            .map(|d| format!("{d}/install-dkms.sh"))
            .find(|p| Path::new(p).exists())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "no prebuilt smiflash for {} and driver sources not found in \
                         {DRIVER_SRC_DIRS:?} — reinstall arieltune (install.sh stages them), \
                         or run driver/install-dkms.sh from a checkout.",
                        kmod::running_kver()
                    ),
                )
            })?;
        let status = Command::new("sh").arg(&script).status()?;
        if !status.success() {
            return Err(io::Error::other(format!(
                "{script} failed — see its output above (need root, dkms, and kernel headers)."
            )));
        }
    }
    // Persist autoload so the driver comes back after reboots: the DKMS install
    // only drops the module into the module tree; nothing else writes the
    // modules-load / modprobe.d entries. The port is persisted from the FADT
    // (same source load() uses), so a boot autoload and a manual load can never
    // disagree about smi_port.
    let port = smi_cmd_port()
        .map_err(|e| io::Error::other(format!("cannot persist smiflash boot config: {e}")))?;
    fs::create_dir_all("/etc/modules-load.d").ok();
    fs::create_dir_all("/etc/modprobe.d").ok();
    if fs::write("/etc/modules-load.d/99-smiflash.conf", "smiflash\n").is_err()
        || fs::write(
            "/etc/modprobe.d/smiflash.conf",
            format!("options smiflash smi_port=0x{port:x}\n"),
        )
        .is_err()
    {
        eprintln!(
            "warning: could not persist smiflash boot config (boot autoload will be missing)"
        );
    }
    Ok(())
}

/// Unload the smiflash driver (best-effort).
pub fn unload() -> io::Result<()> {
    let status = Command::new("rmmod").arg("smiflash").status()?;
    if !status.success() {
        return Err(io::Error::other("rmmod smiflash failed"));
    }
    Ok(())
}

// SW-SMI command bytes
const BEGIN: u8 = 0x20;
const READ: u8 = 0x21;
const WRITE: u8 = 0x23;
const END: u8 = 0x24;

// _IOWR('F', 1, struct smiflash_op) — the op struct is __packed = 270 bytes:
//   B(cmd) I(offset) I(size) B(status) I(dlen) 256s(data)
// dir=3<<30 | size(270)<<16 | 'F'(0x46)<<8 | nr(1)
const SMIFLASH_DO: u64 = (3 << 30) | (270 << 16) | ((b'F' as u64) << 8) | 1;

#[repr(C, packed)]
struct Op {
    cmd: u8,
    offset: u32,
    size: u32,
    status: u8,
    dlen: u32,
    data: [u8; MAXDATA],
}

impl Op {
    fn new(cmd: u8, offset: u32, size: u32, payload: &[u8]) -> Self {
        let mut data = [0u8; MAXDATA];
        let n = payload.len().min(MAXDATA);
        data[..n].copy_from_slice(&payload[..n]);
        Op {
            cmd,
            offset,
            size,
            status: 0xEE,
            dlen: n as u32,
            data,
        }
    }
}

/// An open handle to the SMM flash driver.
pub struct Smm {
    file: std::fs::File,
}

impl Smm {
    /// Is the smiflash driver present? (module loaded)
    pub fn available() -> bool {
        std::path::Path::new(PROC).exists()
    }

    pub fn open() -> io::Result<Self> {
        if !Self::available() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "{PROC} absent — smiflash driver not loaded. Run `arieltune bios driver load` \
                     (loads it with the FADT SMI_CMD port). If it isn't installed yet, run \
                     `arieltune bios driver build` (DKMS — builds it for your kernel on the board)."
                ),
            ));
        }
        let file = OpenOptions::new().read(true).write(true).open(PROC)?;
        Ok(Smm { file })
    }

    /// Fire one SMI. Returns (status, returned 256-byte data buffer).
    fn ioctl(&self, mut op: Op) -> io::Result<(u8, [u8; MAXDATA])> {
        let rc = unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                SMIFLASH_DO as libc::Ioctl,
                &mut op as *mut Op,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((op.status, op.data))
    }

    /// Read `n` bytes of flash at chip `off` (any length; chunked, transaction-wrapped).
    pub fn read(&self, off: u32, n: usize) -> io::Result<Vec<u8>> {
        let mut out = Vec::with_capacity(n);
        self.ioctl(Op::new(BEGIN, 0, 0, &[]))?;
        let res = (|| {
            while out.len() < n {
                let c = (n - out.len()).min(MAXDATA);
                let (st, data) =
                    self.ioctl(Op::new(READ, off + out.len() as u32, c as u32, &[]))?;
                if st != 0 {
                    return Err(io::Error::other(format!(
                        "smm read @0x{:x} status=0x{:02x}",
                        off as usize + out.len(),
                        st
                    )));
                }
                out.extend_from_slice(&data[..c]);
            }
            Ok(())
        })();
        let _ = self.ioctl(Op::new(END, 0, 0, &[]));
        res?;
        Ok(out)
    }

    /// Write `data` at chip `off` (any length; chunked). AND-only: clears bits
    /// (1->0) only — caller must ensure the target is erased (0xFF) where bits
    /// need setting. No erase is performed.
    pub fn write(&self, off: u32, data: &[u8]) -> io::Result<()> {
        self.ioctl(Op::new(BEGIN, 0, 0, &[]))?;
        let res = (|| {
            let mut p = 0;
            while p < data.len() {
                let chunk = &data[p..(p + MAXDATA).min(data.len())];
                let (st, _) =
                    self.ioctl(Op::new(WRITE, off + p as u32, chunk.len() as u32, chunk))?;
                if st != 0 {
                    return Err(io::Error::other(format!(
                        "smm write @0x{:x} status=0x{:02x}",
                        off as usize + p,
                        st
                    )));
                }
                p += chunk.len();
            }
            Ok(())
        })();
        let _ = self.ioctl(Op::new(END, 0, 0, &[]));
        res
    }
}
