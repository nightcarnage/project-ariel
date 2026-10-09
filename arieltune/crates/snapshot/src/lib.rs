// SPDX-License-Identifier: GPL-2.0-only
//! BC-250 kernel snapshot manager, ported from `bc-250-snapshotter/snapshot`.
//!
//! This crate is deliberately **read-only in its first version**. It exists to
//! answer "what snapshots are on this blade, and which one did I boot?" without
//! mutating anything, so it can be compared against the shell tool it replaces
//! before it is trusted with writes.
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
//! | [`store`] | read-only discovery over the snapshot root |
//! | [`grub`] | `40_custom` blocks and `grub.cfg` menu entries |
//! | [`cli`] | the read-only command surface |

pub mod cli;
pub mod cpio;
pub mod dev;
pub mod grub;
pub mod id;
pub mod manifest;
pub mod screen;
pub mod store;
pub mod write;

pub use cli::{Cmd, Global};
pub use screen::{screen, SnapshotScreen};
