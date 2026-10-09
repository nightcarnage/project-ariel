// SPDX-License-Identifier: GPL-2.0-only
//! BC-250 kernel snapshot manager, ported from `bc-250-snapshotter/snapshot`.
//!
//! This crate **started read-only** — it existed to answer "what snapshots are on
//! this blade, and which one did I boot?" so it could be compared against the shell
//! tool it replaces before it was trusted with writes. It now owns writes too:
//! `prune`, `pin`, `hide`, `boot` and `dev --run --register` all change the store,
//! `/boot` or the boot configuration. The read-only verbs are `list`, `show`,
//! `current`, `lineage`, `grub-list`, `diff`, `doctor` and `verify`; every writing
//! verb is listed in `docs/snapshot-tool.md` with what it touches.
//!
//! Design and invariants: `docs/snapshot-tool.md` in this repository. The short
//! version of the rule that matters: the on-disk ids, manifest field names and
//! path conventions are a contract with existing snapshots, so this crate
//! reproduces them rather than improving them.
//!
//! | module | what it owns |
//! |---|---|
//! | [`id`] | the three content-addressed id formulas, with the shell's exact traps |
//! | [`manifest`] | `manifest.yaml`, parsed leniently because the writer's types vary |
//! | [`store`] | discovery over the snapshot root |
//! | [`grub`] | `40_custom` blocks and `grub.cfg` menu entries |
//! | [`register`] | turning a verified image into a bootable snapshot |
//! | [`write`](mod@write) | the action plan every writing verb is built from |
//! | [`doctor`] | whether a *store* is coherent |
//! | [`verify`] | whether one *snapshot* is what its manifest claims |
//! | [`report`] | the findings vocabulary those two share |
//! | [`cli`] | the command surface |

pub mod cli;
pub mod cpio;
pub mod dev;
pub mod doctor;
pub mod grub;
pub mod id;
pub mod image;
pub mod manifest;
pub mod register;
pub mod report;
pub mod screen;
pub mod store;
pub mod verify;
pub mod write;

pub use cli::{Cmd, Global};
pub use screen::{screen, SnapshotScreen};
