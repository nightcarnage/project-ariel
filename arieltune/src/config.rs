// SPDX-License-Identifier: GPL-2.0-only
//! Suite config. For M0 this only carries the default landing tab; later milestones
//! extend it. Read from `/var/lib/arieltune/config.toml` when present (best-effort;
//! a missing or malformed file falls back to defaults, never an error to the user).

use serde::Deserialize;
use std::path::PathBuf;

/// The four tabs, in fixed left-to-right order. This IS the tab order.
pub const TAB_ORDER: [&str; 5] = ["wiki", "bios", "apu", "mem", "snap"];

/// Names a tab answers to besides its key in [`TAB_ORDER`].
///
/// `snap` is the key — it matches the other tabs' names (`apu`, `mem`, `bios`),
/// which is what the CLI namespace and the `1`-`5` routing resolve against — while
/// `snapshot` is what the verb surface used to be called and `snaptune` is the
/// module's own name. All three land on the same tab, so renaming costs nothing.
const TAB_ALIASES: [(&str, &str); 2] = [("snapshot", "snap"), ("snaptune", "snap")];

/// Resolve a tab name (case-insensitive) to its index in [`TAB_ORDER`].
pub fn tab_index(name: &str) -> Option<usize> {
    let n = name.trim().to_ascii_lowercase();
    let key = match TAB_ALIASES.iter().find(|(alias, _)| *alias == n) {
        Some((_, key)) => *key,
        None => n.as_str(),
    };
    TAB_ORDER.iter().position(|t| *t == key)
}

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    /// Which tab the TUI opens to when nothing overrides it. Defaults to `wiki`.
    pub default_tab: Option<String>,
}

fn config_path() -> PathBuf {
    PathBuf::from("/var/lib/arieltune/config.toml")
}

impl Config {
    /// Best-effort load; defaults on any error.
    pub fn load() -> Config {
        match std::fs::read_to_string(config_path()) {
            Ok(s) => toml::from_str(&s).unwrap_or_default(),
            Err(_) => Config::default(),
        }
    }

    /// Resolve the launch tab index given an explicit override (from `--tab` or a bare
    /// `arieltune <app>` subcommand). Priority: explicit > config `default_tab` > wiki.
    pub fn resolve_launch_tab(&self, explicit: Option<&str>) -> usize {
        explicit
            .and_then(tab_index)
            .or_else(|| self.default_tab.as_deref().and_then(tab_index))
            .unwrap_or(0) // wiki
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_order_is_wiki_bios_apu_mem() {
        assert_eq!(TAB_ORDER, ["wiki", "bios", "apu", "mem", "snap"]);
    }

    #[test]
    fn tab_index_is_case_insensitive() {
        assert_eq!(tab_index("WIKI"), Some(0));
        assert_eq!(tab_index(" Apu "), Some(2));
        assert_eq!(tab_index("mem"), Some(3));
        assert_eq!(tab_index("nope"), None);
    }

    /// The tab shell is `snap`, matching the other tabs' names. The old namespace
    /// and the module's own name must still resolve, or every existing script and
    /// every piece of muscle memory breaks at once.
    #[test]
    fn the_branded_name_reaches_the_same_tab() {
        let snap = tab_index("snap");
        assert_eq!(snap, Some(4), "the tab is the fifth");
        assert_eq!(tab_index("snaptune"), snap, "the module's name");
        assert_eq!(tab_index("snapshot"), snap, "the namespace it used to be");
        assert_eq!(tab_index("SNAP"), snap);
        assert_eq!(tab_index(" SnapTune "), snap);
    }

    #[test]
    fn resolve_priority_explicit_over_config_over_wiki() {
        let cfg = Config {
            default_tab: Some("bios".into()),
        };
        // explicit wins
        assert_eq!(cfg.resolve_launch_tab(Some("apu")), 2);
        // config default when no explicit
        assert_eq!(cfg.resolve_launch_tab(None), 1);
        // wiki when neither
        assert_eq!(Config::default().resolve_launch_tab(None), 0);
        // an invalid explicit falls through to config
        assert_eq!(cfg.resolve_launch_tab(Some("bogus")), 1);
    }
}
