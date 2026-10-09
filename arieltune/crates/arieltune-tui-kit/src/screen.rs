// SPDX-License-Identifier: GPL-2.0-only
//! The contract every tab implements: how a screen takes keys, what it draws,
//! and how the shell and the active pane share one keyspace.
//!
//! Routing is pane-first for anything a pane may bind. The four merged apps each
//! bind bare `Tab`/`Shift-Tab`/`Shift`/`q` internally, so the shell cannot claim
//! those globally without stealing them from the panes: the active screen sees
//! those keys, and the shell acts only on what it returns as [`Outcome::Ignored`].
//!
//! **The shell's own keys are the other way round.** Bare digits, function keys and
//! modifier chords (`1`-`5`, `F1`-`F5`, Ctrl-Q, Ctrl-Tab) are bound by no pane, so the
//! shell acts on them *before* offering the key to the pane, unless the pane is in a
//! modal sub-state. The reason is that a pane can only get this wrong silently: a pane
//! that consumes every key it does not recognise has to carry a list of the shell's
//! keys, and a list of someone else's keys goes stale. The APU pane's said `1`-`4`,
//! written when the suite had four tabs, so `5` did nothing from that tab and nowhere
//! else. A pane therefore never needs to know which keys belong to the shell.

use crossterm::event::KeyEvent;
use ratatui::{layout::Rect, Frame};
use std::time::Duration;

/// What a screen returns after handling a key.
///
/// The shell routes pane-first for the keys a pane may bind: the active screen sees
/// them first, and the shell applies its own global bindings only when it returns
/// [`Outcome::Ignored`]. This keeps each pane in full control of its own keyspace --
/// important because the four merged apps each bind bare `Tab`/`Shift-Tab`/`Shift`/`q`
/// internally.
///
/// The shell's own bindings -- bare digits, function keys and modifier chords -- are
/// taken before the pane sees them, unless [`Screen::modal`] is true. See the module
/// docs for why: a pane cannot be trusted to remember which keys are not its own.
/// A pane that consumes unbound keys must still return [`Outcome::Ignored`] for them if
/// it wants to be correct when embedded without a shell, but it will never be *asked*
/// to.
pub enum Outcome {
    /// The screen handled the key; the shell does nothing further.
    Consumed,
    /// The screen did not handle it; the shell may apply a global binding.
    Ignored,
    /// Tear the whole app down (a pane explicitly asked to quit the suite).
    Quit,
    /// Tear down + reboot with this operator message (the memtune/biostune write flow).
    Reboot(String),
}

/// One tab in the suite. Each former standalone app (wiki/bios/apu/mem) implements
/// this; the shell owns the terminal, the event loop, the tab bar, and the status line.
pub trait Screen {
    /// Tab-bar label. Fixed order is enforced by the shell, not the screen.
    fn title(&self) -> &'static str;

    /// Render into the content area (the tab bar and global status line are NOT here).
    fn draw(&mut self, f: &mut Frame, area: Rect);

    /// Handle one key press.
    fn on_key(&mut self, key: KeyEvent) -> Outcome;

    /// Throttled background refresh (telemetry, polling a bench thread). Default no-op.
    fn tick(&mut self) {}

    /// How long the shell may block in `event::poll` before calling [`Screen::tick`].
    /// An idle screen (e.g. BIOS, no telemetry) returns ~1s; a live one ~90-300ms.
    fn tick_hint(&self) -> Duration {
        Duration::from_millis(250)
    }

    /// Called when this tab gains focus -- seed live data so the first frame is not blank.
    fn on_enter(&mut self) {}

    /// Called when leaving this tab -- pause heavy polling, drop hardware handles if desired.
    fn on_exit(&mut self) {}

    /// Right-aligned per-screen help/status hint, merged into the global status line.
    fn status_hint(&self) -> Option<String> {
        None
    }

    /// True while the screen is in a modal sub-state (search box, confirm, field edit,
    /// a popup). While true the shell MUST NOT steal global switch/quit keys.
    fn modal(&self) -> bool {
        false
    }
}
