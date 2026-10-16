// SPDX-License-Identifier: GPL-2.0-only
//! `snaptune` — the standalone binary.
//!
//! Named `snaptune` rather than `snapshot` because it joins `arieltune`/`llmtune`
//! as a sibling, and because the name `snapshot` is taken twice over: on a workstation
//! by the GNOME screenshot tool, and on the blades by the bc-250-snapshotter shell tool
//! this replaces.
//!
//! That tool is **retired**: `/usr/local/bin/snapshot` is now a compat symlink to
//! `arieltune`, so the name reaches this namespace, and `snapshot list --ids` is kept
//! machine-compatible because `arieltune apu` parses it. This standalone binary is the
//! same code without the suite around it -- what a box that wants only this namespace
//! installs. See `docs/snaptune.md` §2.
//!
//! This build writes as well as reads: `prune`, `pin`, `hide`, `boot` and
//! `dev --run --register` change the store, `/boot` or the boot configuration, and
//! each refuses or prints its plan under `--dry-run`.

use clap::Parser;

fn main() -> anyhow::Result<()> {
    // Printing to a pipe that has gone away — `snaptune verify | head` — is an
    // ordinary thing to do, and a verifier that prints every snapshot is exactly the
    // verb people will pipe. Rust ignores `SIGPIPE` at startup, so the write instead
    // fails with `EPIPE` and the `println!` panics, which reads as a crash in the
    // command rather than a closed pager. Restoring the default disposition makes the
    // process exit quietly, the way every other CLI does.
    //
    // SAFETY: resetting a signal to its default disposition is async-signal-safe and
    // requires no initialisation.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    snapshot::cli::adopt_source_tree();

    let cli = snapshot::cli::Cli::parse();
    let code = snapshot::cli::run(cli)?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}
