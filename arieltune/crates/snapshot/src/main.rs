// SPDX-License-Identifier: GPL-2.0-only
//! `snaptune` — the standalone binary.
//!
//! Named `snaptune` rather than `snapshot` because `snapshot` is an unrelated
//! binary on the workstation (the GNOME screenshot tool answers
//! `Unknown option --ids`), and because it joins `arieltune`/`llmtune` as a
//! sibling. Blades get a `snapshot` compat symlink so the fleet scripts keep
//! working; see `docs/snapshot-tool.md` §3.5.
//!
//! This build is **read-only** and says so. It exists to be compared against the
//! shell tool on real blades before it is trusted with writes.

use clap::Parser;

fn main() -> anyhow::Result<()> {
    let cli = snapshot::cli::Cli::parse();
    let code = snapshot::cli::run(cli)?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}
