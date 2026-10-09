// SPDX-License-Identifier: GPL-2.0-only
//! Reading and rewriting `newc` cpio archives — the format an initramfs is made of.
//!
//! ## Why this exists rather than shelling out
//!
//! Deriving a dev snapshot's initramfs means replacing **one member** inside a
//! ~54 MiB archive. The obvious way is to unpack the whole thing, swap the file,
//! and repack — thousands of files through `cpio` twice. This walks the archive
//! once and rewrites the single record, which is the difference between a dev loop
//! that feels instant and one that pauses for ten seconds each iteration.
//!
//! ## The format
//!
//! `newc` is a flat sequence of records, each: a 110-byte ASCII header, the name
//! (NUL-terminated, padded to 4), then the data (padded to 4). The header carries
//! every field as 8 uppercase hex digits, so `filesize` is at byte 54 and
//! `namesize` at byte 94. The archive ends with a zero-length record named
//! `TRAILER!!!`.
//!
//! ## What this is for
//!
//! An Arch initramfs is **two concatenated archives**: an uncompressed early
//! cpio (microcode, ACPI overrides) followed by a zstd'd cpio holding the
//! modules. [`extent`] finds where the first one ends, which is how the
//! compressed half is located without guessing at magic bytes.

use anyhow::{bail, Context, Result};

const HEADER: usize = 110;
const MAGIC: &[u8] = b"070701";
const TRAILER: &str = "TRAILER!!!";

/// Padding needed to bring `n` up to a 4-byte boundary.
fn pad4(n: usize) -> usize {
    (4 - n % 4) % 4
}

/// One member of an archive, located rather than copied.
#[derive(Debug, Clone)]
pub struct Member {
    pub name: String,
    /// Offset of the 110-byte header.
    pub header_at: usize,
    /// Offset of the data.
    pub data_at: usize,
    pub data_len: usize,
    /// Offset just past this record's padding — the next record starts here.
    pub end: usize,
}

/// Read the 8 hex digits of field `n` (0-based from the magic).
fn field(header: &[u8], n: usize) -> Result<usize> {
    let at = 6 + n * 8;
    let text = std::str::from_utf8(&header[at..at + 8])
        .with_context(|| format!("header field {n} is not ASCII"))?;
    usize::from_str_radix(text, 16)
        .with_context(|| format!("header field {n} is not hex: {text:?}"))
}

/// Walk an archive into its members, in order. Stops at the trailer, which is
/// included so that its `end` is the archive's extent.
pub fn parse(bytes: &[u8]) -> Result<Vec<Member>> {
    let mut members = Vec::new();
    let mut at = 0usize;
    loop {
        if at + HEADER > bytes.len() {
            bail!("archive ends mid-header at {at}");
        }
        let header = &bytes[at..at + HEADER];
        if &header[..6] != MAGIC {
            bail!(
                "not a newc archive at {at}: magic {:?}",
                String::from_utf8_lossy(&header[..6])
            );
        }
        let filesize = field(header, 6)?;
        // Field 11, not 12: 12 is `check`, which is always zero and reads as a
        // zero-length name.
        let namesize = field(header, 11)?;
        if namesize == 0 {
            bail!("zero-length name at {at}");
        }

        let name_at = at + HEADER;
        let name_bytes = bytes
            .get(name_at..name_at + namesize)
            .context("archive ends mid-name")?;
        // The name is NUL-terminated, so the last byte is dropped.
        let name = String::from_utf8_lossy(&name_bytes[..namesize - 1]).into_owned();

        let data_at = name_at + namesize + pad4(name_at + namesize);
        let end = data_at + filesize + pad4(filesize);
        if end > bytes.len() {
            bail!("member {name:?} claims data past the end of the archive");
        }

        let trailer = name == TRAILER;
        members.push(Member {
            name,
            header_at: at,
            data_at,
            data_len: filesize,
            end,
        });
        if trailer {
            return Ok(members);
        }
        at = end;
    }
}

/// The offset just past the first archive in `bytes`.
///
/// An initramfs is a concatenation: find where the uncompressed early archive
/// stops and the compressed one begins. Doing it by walking the format beats
/// searching for zstd's magic bytes, which occur inside file data — on a real
/// blade image the first `28 B5 2F FD` is 36 MB *before* the split, inside the
/// early archive's own contents.
///
/// Note this is the end of the **trailer record**, not the start of the next
/// archive: mkinitcpio pads between them. See [`compressed_at`].
pub fn extent(bytes: &[u8]) -> Result<usize> {
    Ok(parse(bytes)?
        .last()
        .map(|m| m.end)
        .expect("parse always returns the trailer"))
}

/// Name a compression frame by its magic, for error messages and for reporting
/// what we found rather than only what we wanted.
fn frame_kind(bytes: &[u8]) -> &'static str {
    match bytes {
        [0x28, 0xB5, 0x2F, 0xFD, ..] => "zstd",
        [0xFD, b'7', b'z', b'X', b'Z', 0x00, ..] => "xz",
        [0x1F, 0x8B, ..] => "gzip",
        [0x42, 0x5A, 0x68, ..] => "bzip2",
        [0x04, 0x22, 0x4D, 0x18, ..] => "lz4",
        _ => "unknown",
    }
}

/// Where the archive *following* the first one starts.
///
/// mkinitcpio pads between the two archives, and the gap is zero-filled up to a
/// 4 KiB boundary on the blades this runs on. Rather than hard-code 4096 — a
/// number that would silently stop matching the day mkinitcpio changed it — this
/// skips the zeros and then insists on recognising a frame, so a changed layout is
/// a loud error rather than a shift by a few thousand bytes.
///
/// Skipping zeros cannot overrun into the data: every compression magic starts
/// with a non-zero byte.
pub fn compressed_at(bytes: &[u8]) -> Result<usize> {
    let mut at = extent(bytes)?;
    while at < bytes.len() && bytes[at] == 0 {
        at += 1;
    }
    if at >= bytes.len() {
        bail!("the first archive is the whole file; no compressed archive follows");
    }
    let kind = frame_kind(&bytes[at..]);
    if kind == "unknown" {
        let end = (at + 4).min(bytes.len());
        bail!(
            "no compression frame at {at} (found {:02X?}) — the padding or the image \
             layout has changed",
            &bytes[at..end]
        );
    }
    Ok(at)
}

/// Rewrite one regular member in place, leaving every other byte untouched.
///
/// Errors rather than appending when the member is absent: a dev snapshot whose
/// module silently failed to land is the failure this whole path exists to avoid,
/// and a caller that gets `Ok` from a no-op would report success.
pub fn replace_member(bytes: &[u8], name: &str, new_data: &[u8]) -> Result<Vec<u8>> {
    let members = parse(bytes)?;
    let member = members
        .iter()
        .find(|m| m.name == name)
        .with_context(|| format!("no member {name:?} in this archive"))?;
    if member.name == TRAILER {
        bail!("{TRAILER} is not a rewritable member");
    }

    let mut out = Vec::with_capacity(bytes.len() - member.data_len + new_data.len());
    out.extend_from_slice(&bytes[..member.data_at]);

    // Keep the original header (ino, mode, mtime, ownership) and change only the
    // size, so the replacement is as close to a byte-for-byte edit as possible.
    // The header starts 110 + namesize + padding *before* the data, so it has to
    // come from the member rather than be derived from `data_at`.
    let header_at = member.header_at;
    let header = &mut out[header_at..header_at + HEADER];
    let size = format!("{:08X}", new_data.len());
    header[54..62].copy_from_slice(size.as_bytes());

    out.extend_from_slice(new_data);
    out.resize(out.len() + pad4(new_data.len()), 0);
    out.extend_from_slice(&bytes[member.end..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an archive the way cpio would, so the parser is tested against the
    /// real byte layout rather than against its own writer.
    fn build(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        // Bound rather than inlined: `chain([...])` would borrow a temporary.
        let trailer = [(TRAILER, &b""[..])];
        for (ino, (name, data)) in (1usize..).zip(entries.iter().chain(trailer.iter())) {
            let name_bytes = [name.as_bytes(), &[0u8]].concat();
            let header = format!(
                "070701{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}",
                ino, 0o100644, 0, 0, 1, 0, data.len(), 0, 0, 0, 0, name_bytes.len(), 0
            );
            assert_eq!(header.len(), HEADER);
            out.extend_from_slice(header.as_bytes());
            out.extend_from_slice(&name_bytes);
            out.resize(out.len() + pad4(out.len()), 0);
            out.extend_from_slice(data);
            out.resize(out.len() + pad4(data.len()), 0);
        }
        out
    }

    #[test]
    fn parse_finds_every_member_in_order() {
        let archive = build(&[("usr/one", b"first"), ("usr/two", b"second")]);
        let members = parse(&archive).unwrap();
        let names: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["usr/one", "usr/two", TRAILER]);
        assert_eq!(members[0].data_len, 5);
        assert_eq!(members[1].data_len, 6);
    }

    /// The trailer's end is the whole archive, which is what lets a caller find
    /// the compressed half of an initramfs without scanning for magic bytes.
    #[test]
    fn extent_is_the_end_of_the_trailer() {
        let entries: &[(&str, &[u8])] = &[("a", b"1234"), ("bb", b"56789")];
        let archive = build(entries);
        assert_eq!(extent(&archive).unwrap(), archive.len());
    }

    #[test]
    fn data_is_readable_through_the_offsets() {
        let archive = build(&[("path/to/thing", b"payload")]);
        let m = &parse(&archive).unwrap()[0];
        assert_eq!(&archive[m.data_at..m.data_at + m.data_len], b"payload");
    }

    #[test]
    fn replace_swaps_only_the_named_member() {
        let archive = build(&[
            ("keep", b"unchanged"),
            ("swap", b"old"),
            ("tail", b"also kept"),
        ]);
        let out = replace_member(&archive, "swap", b"a much longer replacement").unwrap();

        let members = parse(&out).unwrap();
        let get = |n: &str| -> Vec<u8> {
            let m = members.iter().find(|m| m.name == n).unwrap();
            out[m.data_at..m.data_at + m.data_len].to_vec()
        };
        assert_eq!(get("swap"), b"a much longer replacement");
        assert_eq!(get("keep"), b"unchanged", "other members are untouched");
        assert_eq!(get("tail"), b"also kept");
    }

    /// A member that grew must still leave a parseable archive — the padding
    /// arithmetic is where this would silently corrupt if it were wrong.
    #[test]
    fn a_grown_member_leaves_the_archive_parseable() {
        let archive = build(&[("before", b"x"), ("module", b"tiny"), ("after", b"y")]);
        let out = replace_member(&archive, "module", &vec![0xABu8; 5000]).unwrap();

        assert_eq!(extent(&out).unwrap(), out.len());
        let members = parse(&out).unwrap();
        assert_eq!(members.len(), 4, "same member count");
        let m = members.iter().find(|m| m.name == "module").unwrap();
        assert_eq!(m.data_len, 5000);
        assert!(out[m.data_at..m.data_at + 5000].iter().all(|b| *b == 0xAB));
    }

    /// A shrunk member must not leave the following record misaligned.
    #[test]
    fn a_shrunk_member_keeps_later_members_readable() {
        let archive = build(&[("big", &vec![0u8; 4096]), ("after", b"still here")]);
        let out = replace_member(&archive, "big", b"z").unwrap();
        let members = parse(&out).unwrap();
        let m = members.iter().find(|m| m.name == "after").unwrap();
        assert_eq!(&out[m.data_at..m.data_at + m.data_len], b"still here");
    }

    /// The whole point of erroring: a caller must never be able to read `Ok` from
    /// a replacement that did not happen.
    #[test]
    fn replacing_a_missing_member_is_an_error() {
        let archive = build(&[("present", b"data")]);
        let err = replace_member(&archive, "absent", b"x").unwrap_err();
        assert!(err.to_string().contains("no member"), "{err}");
    }

    #[test]
    fn a_non_archive_is_rejected_by_name_not_panic() {
        // Long enough to get past the length check, so this exercises the magic
        // test rather than the bounds test.
        let not_an_archive = vec![b'x'; 512];
        let err = extent(&not_an_archive).unwrap_err();
        assert!(err.to_string().contains("not a newc archive"), "{err}");
    }

    /// A short buffer fails the bounds check, and must say so rather than claim
    /// the bytes are not an archive.
    #[test]
    fn a_short_buffer_reports_truncation() {
        let err = extent(b"too short").unwrap_err();
        assert!(err.to_string().contains("mid-header"), "{err}");
    }

    /// A truncated archive must error, not index past the end.
    #[test]
    fn a_truncated_archive_errors() {
        let archive = build(&[("some/file", b"0123456789")]);
        let cut = &archive[..archive.len() - 6];
        assert!(parse(cut).is_err());
    }

    /// Validate the split against a **real** initramfs from a blade.
    ///
    /// The whole design rests on one assumption: that walking the early archive to
    /// its trailer lands exactly on the compressed one. A synthetic fixture cannot
    /// show that, because the real thing is produced by mkinitcpio and is the only
    /// layout that matters.
    ///
    /// Not a unit test — it needs an image copied off a blade:
    ///
    /// ```text
    /// SNAPDEV_IMAGE=/var/tmp/real.img \
    ///   cargo test -p snapshot real_image_split -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "needs a real initramfs; set SNAPDEV_IMAGE"]
    fn real_image_split() {
        let path = match std::env::var("SNAPDEV_IMAGE") {
            Ok(p) => p,
            Err(_) => {
                println!("SNAPDEV_IMAGE not set; nothing to check");
                return;
            }
        };
        let bytes = std::fs::read(&path).expect("read the image");
        let split = extent(&bytes).expect("the early archive must parse");
        let start = compressed_at(&bytes).expect("there must be a compressed archive");

        let early = parse(&bytes[..split]).expect("the early archive alone must parse");
        println!(
            "image {} bytes; early archive ends at {split}; {} early members",
            bytes.len(),
            early.len()
        );
        println!(
            "compressed archive starts at {start} (padding {} bytes)",
            start - split
        );
        println!("frame: {}", frame_kind(&bytes[start..]));

        // Everything between the two archives must be padding, or the skip walked
        // over something real.
        assert!(
            bytes[split..start].iter().all(|b| *b == 0),
            "the gap between the archives is not zero padding"
        );
        assert_eq!(
            frame_kind(&bytes[start..]),
            "zstd",
            "the second archive should be zstd on these blades"
        );
    }
}
