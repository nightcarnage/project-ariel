// SPDX-License-Identifier: GPL-2.0-only
//! The SNAP tab.
//!
//! The module is snaptune, and its standalone binary keeps that name; the tab and
//! the CLI namespace are `snap`, matching WIKI/BIOS/APU/MEM.
//!
//! ## What this pane may and may not do
//!
//! `apu` documents a deliberate boundary: the GPU governor and `apply-boot` are
//! CLI-only and never spawned from the TUI, because a second SMU writer is
//! dangerous. The snapshot equivalent is that `prune` deletes kernels and `boot`
//! reboots the machine — so neither is here, at any confirmation level.
//!
//! This pane drives the *menu*: pin, unpin, hide, unhide. All four are
//! reversible, all four go through [`crate::write`]'s plan/apply, and each one
//! shows the plan in a confirmation popup before anything is written. The
//! destructive verbs stay in the CLI.
//!
//! ## Keys
//!
//! Deliberately the same vocabulary as every other pane, because a tab that
//! answers to its own private keys is a tab people stop visiting. Bare `q` (or
//! `Esc`, or Ctrl-C) leaves the suite, exactly as WIKI and MEM behave. `Tab`
//! moves focus between the list and the provenance panel, and the navigation keys
//! act on whichever one holds focus — the habit APU and WIKI establish. Digits
//! `1`-`5` and anything with Ctrl/Alt are handed back to the shell, so switching
//! tabs works here as it does everywhere else.
//!
//! ## Why the logic is not in `draw`
//!
//! Selection, filtering and the pending-action state are plain data with plain
//! methods ([`SnapshotScreen::select_next`], [`SnapshotScreen::plan_for`], …) so
//! they can be tested without a terminal. `draw` only renders that state.

use std::path::PathBuf;
use std::time::Duration;

use arieltune_tui_kit::{theme, widgets, Outcome, Screen};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::store::{human_bytes, Paths, Snapshot, Store};
use crate::write::{self, Plan, Runner, System};

/// What the operator has asked for but not yet confirmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pending {
    Pin(String),
    Unpin,
    Hide(String),
    Unhide(String),
}

impl Pending {
    fn verb(&self) -> &'static str {
        match self {
            Pending::Pin(_) => "pin",
            Pending::Unpin => "unpin",
            Pending::Hide(_) => "hide",
            Pending::Unhide(_) => "unhide",
        }
    }
}

/// Which of the pane's two regions the navigation keys act on.
///
/// The suite's panes all work this way — WIKI toggles `Nav`/`Content`, APU cycles
/// five panels — so `Tab` then arrows is one habit across every tab rather than a
/// separate one to learn here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    List,
    Provenance,
}

/// Rows the list moves, and lines the panel scrolls, for one `PgUp`/`PgDn`.
/// Matches WIKI's page step so paging feels the same in both.
const PAGE: i32 = 15;

/// Panel height the list will not give up: two border rows, the header, and at
/// least three rows of data to choose from.
const MIN_LIST_ROWS: u16 = 6;

/// The pane's whole state.
pub struct SnapshotScreen {
    pub paths: Paths,
    /// Every snapshot in the store, hidden ones included.
    ///
    /// Hidden rows are kept rather than filtered: this is the pane from which you
    /// unhide them, so dropping them would make that unreachable. They are
    /// annotated instead, the way the CLI's `list` does it.
    snapshots: Vec<Snapshot>,
    /// The pinned GRUB default. grubenv is global rather than per-snapshot, so it
    /// is read once per refresh and compared per row.
    pinned: Option<String>,
    selection: usize,
    /// First list row drawn. Kept in step with `selection` by the renderer, which
    /// is the only place that knows how many rows fit — without it the selection
    /// walks off the bottom of the panel and the arrow keys look dead.
    list_off: usize,
    /// Which region the arrows act on. Shown in the panel titles, so `Tab` never
    /// looks like it did nothing.
    focus: Focus,
    /// `enter` grows the provenance panel; while it is up, PgUp/PgDn read it.
    detail: bool,
    /// First provenance line shown, so a manifest longer than the panel is still
    /// readable rather than silently truncated.
    detail_scroll: usize,
    /// Where this pane's commands go. Injected so tests can confirm an action
    /// without spawning `grub-mkconfig`; the suite always gets [`System`].
    runner: Box<dyn Runner>,
    /// Set while a confirmation popup is up. `modal()` reports it, which stops
    /// the shell stealing digits and quit keys mid-confirmation.
    pending: Option<Pending>,
    status: String,
    error: Option<String>,
}

impl Default for SnapshotScreen {
    fn default() -> Self {
        SnapshotScreen::with_paths(Paths::system())
    }
}

impl SnapshotScreen {
    /// The pane on a real blade.
    pub fn with_paths(paths: Paths) -> Self {
        SnapshotScreen::with_runner(paths, Box::new(System::default()))
    }

    /// The pane with an injected command runner, for tests.
    pub fn with_runner(paths: Paths, runner: Box<dyn Runner>) -> Self {
        let mut screen = SnapshotScreen {
            paths,
            snapshots: Vec::new(),
            pinned: None,
            selection: 0,
            list_off: 0,
            focus: Focus::List,
            // Provenance is the reason to look at a snapshot, so it starts up.
            detail: true,
            detail_scroll: 0,
            runner,
            pending: None,
            status: String::new(),
            error: None,
        };
        screen.refresh();
        screen
    }

    /// Re-read the store. Cheap (a directory scan plus small files).
    pub fn refresh(&mut self) {
        match Store::new(self.paths.clone()).discover() {
            Ok(snapshots) => {
                if self.selection >= snapshots.len() {
                    self.selection = snapshots.len().saturating_sub(1);
                }
                self.detail_scroll = 0;
                self.pinned = write::saved_entry();

                // Totals, because on an archive blade the standing question is
                // "which of these can go?" and that is answered in bytes.
                let bytes: u64 = snapshots
                    .iter()
                    .map(|snapshot| snapshot.size_bytes(&self.paths))
                    .sum();
                let hidden = snapshots.iter().filter(|s| s.hidden).count();
                self.status = format!(
                    "{} snapshot(s) · {} hidden · {} on disk",
                    snapshots.len(),
                    hidden,
                    human_bytes(bytes)
                );
                self.snapshots = snapshots;
                self.error = None;
            }
            Err(err) => {
                self.error = Some(err.to_string());
                self.status = "cannot read the snapshot store".to_string();
            }
        }
    }

    pub fn selected_id(&self) -> Option<&str> {
        self.snapshots.get(self.selection).map(|s| s.id.as_str())
    }

    /// The selected snapshot, for the provenance panel.
    pub fn selected(&self) -> Option<&Snapshot> {
        self.snapshots.get(self.selection)
    }

    /// True while the confirmation popup is up.
    pub fn is_confirming(&self) -> bool {
        self.pending.is_some()
    }

    pub fn select_next(&mut self) {
        self.move_selection(1);
    }

    pub fn select_prev(&mut self) {
        self.move_selection(-1);
    }

    /// Move the selection by `delta` rows, clamped to the list.
    pub fn move_selection(&mut self, delta: i32) {
        if self.snapshots.is_empty() {
            return;
        }
        let last = (self.snapshots.len() - 1) as i32;
        self.selection = (self.selection as i32 + delta).clamp(0, last) as usize;
        // A scroll offset from the previous snapshot would open the new one
        // half-read, which looks like missing data.
        self.detail_scroll = 0;
    }

    /// Scroll the provenance panel. Clamped by the renderer, which is the only
    /// place that knows how many lines fit.
    pub fn scroll_detail(&mut self, delta: i32) {
        let next = self.detail_scroll as i32 + delta;
        self.detail_scroll = next.max(0) as usize;
    }

    /// Which region has focus.
    pub fn focus(&self) -> Focus {
        self.focus
    }

    /// Move focus to the other region.
    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::List => Focus::Provenance,
            Focus::Provenance => Focus::List,
        };
    }

    /// Navigate whichever region has focus. One entry point, so `Tab` is the only
    /// thing that decides where an arrow goes.
    pub fn step(&mut self, delta: i32) {
        match self.focus {
            Focus::List => self.move_selection(delta),
            Focus::Provenance => self.scroll_detail(delta),
        }
    }

    /// `Home`/`g`: the top of the focused region.
    pub fn to_start(&mut self) {
        match self.focus {
            Focus::List => {
                self.selection = 0;
                self.detail_scroll = 0;
            }
            Focus::Provenance => self.detail_scroll = 0,
        }
    }

    /// `End`/`G`: the bottom of the focused region.
    pub fn to_end(&mut self) {
        match self.focus {
            Focus::List => {
                self.selection = self.snapshots.len().saturating_sub(1);
                self.detail_scroll = 0;
            }
            // Aim past the end and let the renderer clamp, rather than guessing
            // the panel height here. `usize::MAX` would overflow the arithmetic
            // in `scroll_detail`, so aim at the real line count.
            Focus::Provenance => self.detail_scroll = self.provenance_lines().len(),
        }
    }

    pub fn toggle_detail(&mut self) {
        self.detail = !self.detail;
        self.detail_scroll = 0;
        // Expanding the panel is a request to read it, so focus follows the
        // intent — the same move WIKI makes when `Enter` jumps to the content.
        self.focus = if self.detail {
            Focus::Provenance
        } else {
            Focus::List
        };
    }

    /// Ask for an action. Nothing is written until [`SnapshotScreen::confirm`].
    pub fn request(&mut self, action: Pending) {
        self.error = None;
        self.pending = Some(action);
    }

    pub fn cancel(&mut self) {
        self.pending = None;
        self.status = "cancelled".to_string();
    }

    /// Work out what the pending action would do, without doing it.
    ///
    /// Split out so the refusal paths are testable, and so the popup can show the
    /// exact plan before the operator commits to it.
    pub fn plan_for(&self, action: &Pending) -> anyhow::Result<Plan> {
        let state = write::State::read(&self.paths)?;
        match action {
            Pending::Pin(id) => write::plan_pin(&self.paths, &state, id),
            Pending::Unpin => write::plan_unpin(&self.paths),
            Pending::Hide(id) => write::plan_hide(&self.paths, &state, id),
            Pending::Unhide(id) => write::plan_unhide(&self.paths, &state, id),
        }
    }

    /// Execute the pending action.
    ///
    /// `grub-mkconfig` takes a second or two and runs on the event-loop thread;
    /// that is deliberate. It is short, it is a one-off on an explicit keypress,
    /// and the alternative — a background thread writing to `/boot` while the
    /// operator keeps pressing keys — is worse.
    pub fn confirm(&mut self) {
        let Some(action) = self.pending.clone() else {
            return;
        };
        match self.plan_for(&action) {
            Ok(plan) => match write::apply_with(&plan, false, self.runner.as_ref()) {
                Ok(()) => {
                    self.status = format!("{}: done", action.verb());
                    self.pending = None;
                    self.refresh();
                }
                Err(err) => {
                    self.error = Some(err.to_string());
                    self.pending = None;
                }
            },
            Err(err) => {
                // A refusal is a normal outcome, not a crash: the guards exist
                // precisely so this happens instead of a broken pin.
                self.error = Some(err.to_string());
                self.pending = None;
            }
        }
    }

    /// The lines the confirmation popup shows.
    fn confirm_lines(&self, plan: &Plan) -> Vec<Line<'static>> {
        let mut lines = vec![
            Line::from(Span::styled(plan.summary.clone(), theme::header())),
            Line::from(""),
        ];
        for step in plan.describe() {
            lines.push(Line::from(Span::raw(step)));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "y to apply · n, q or Esc to cancel",
            theme::dim(),
        )));
        lines
    }
}

impl Screen for SnapshotScreen {
    fn title(&self) -> &'static str {
        "SNAP"
    }

    fn on_enter(&mut self) {
        self.refresh();
    }

    fn tick_hint(&self) -> Duration {
        // Nothing polls; a slow tick keeps the loop cheap.
        Duration::from_millis(1000)
    }

    fn modal(&self) -> bool {
        self.pending.is_some()
    }

    fn status_hint(&self) -> Option<String> {
        // Describes the region that currently has focus, so the keys on screen are
        // the ones that will actually do something.
        Some(match self.focus {
            Focus::List => {
                "list: ↑↓ move · tab provenance · enter detail · p/P pin/unpin · h/H hide/unhide · r refresh"
                    .to_string()
            }
            Focus::Provenance => {
                "provenance: ↑↓ scroll · PgUp/PgDn page · g/G top/bottom · tab list".to_string()
            }
        })
    }

    fn on_key(&mut self, key: KeyEvent) -> Outcome {
        // While confirming, the popup owns the keyboard — including digits and
        // `q`, which is why `modal()` also reports true.
        if self.is_confirming() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.confirm(),
                // `q` backs out here as it does in APU's and MEM's popups. Without
                // it, the key that quits every other screen would do nothing at
                // all on the one screen that writes to /boot.
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('q') | KeyCode::Esc => {
                    self.cancel()
                }
                // Anything else is swallowed rather than acted on: a stray key
                // must not answer a question about writing to /boot.
                _ => {}
            }
            return Outcome::Consumed;
        }

        // Ctrl-C quits, as it does in WIKI. Checked before the hand-back below,
        // which would otherwise give it to the shell, where nothing is bound.
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Outcome::Quit;
        }

        // Hand the shell's globals back untouched: modified keys (Ctrl-Q quit,
        // Ctrl-Tab cycle, Alt-1..5), the function keys, and bare 1-5. This check
        // has to come first — `Char('p')` matches Ctrl-P too, and arming a pin
        // from a stray chord is exactly the accident the ordering prevents.
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            || matches!(key.code, KeyCode::F(_))
            || matches!(key.code, KeyCode::Char('1'..='5'))
        {
            return Outcome::Ignored;
        }

        match key.code {
            // Bare `q` or `Esc` leaves the suite, the way WIKI and MEM do. The
            // shell's Ctrl-Q remains the always-available alternative.
            KeyCode::Char('q') | KeyCode::Esc => return Outcome::Quit,

            // Focus first, then navigate the focused region.
            KeyCode::Tab | KeyCode::BackTab => self.toggle_focus(),
            KeyCode::Up | KeyCode::Char('k') => self.step(-1),
            KeyCode::Down | KeyCode::Char('j') => self.step(1),
            KeyCode::PageUp => self.step(-PAGE),
            KeyCode::PageDown => self.step(PAGE),
            KeyCode::Home | KeyCode::Char('g') => self.to_start(),
            KeyCode::End | KeyCode::Char('G') => self.to_end(),

            // This pane's own verbs.
            KeyCode::Enter => self.toggle_detail(),
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char('p') => {
                if let Some(id) = self.selected_id() {
                    let id = id.to_string();
                    self.request(Pending::Pin(id));
                }
            }
            KeyCode::Char('P') => self.request(Pending::Unpin),
            KeyCode::Char('h') => {
                if let Some(id) = self.selected_id() {
                    let id = id.to_string();
                    self.request(Pending::Hide(id));
                }
            }
            KeyCode::Char('H') => {
                if let Some(id) = self.selected_id() {
                    let id = id.to_string();
                    self.request(Pending::Unhide(id));
                }
            }

            // Anything else falls through so the shell can switch tabs / quit.
            _ => return Outcome::Ignored,
        }
        Outcome::Consumed
    }

    fn draw(&mut self, f: &mut Frame, area: Rect) {
        // The popup needs the plan, which costs a read: compute it once here
        // rather than reaching into `self` from inside the render calls.
        let popup_lines = self
            .pending
            .clone()
            .and_then(|pending| self.plan_for(&pending).ok())
            .map(|plan| self.confirm_lines(&plan));

        // Provenance is the point of this pane, so the panel is on by default and
        // `enter` grows it for reading a long manifest.
        //
        // Bounded, though: the panel asks for 55% of whatever is left, and on a
        // short terminal that leaves the list one or two data rows. The list is how
        // you choose what the panel describes, so it keeps a floor.
        let provenance = if self.detail {
            let wanted = (area.height * 55 / 100).max(6);
            let ceiling = area.height.saturating_sub(MIN_LIST_ROWS + 3);
            wanted.min(ceiling).max(4)
        } else {
            4
        };
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(4),
                Constraint::Length(provenance),
                Constraint::Length(3),
            ])
            .split(area);

        self.draw_list(f, rows[0]);
        self.draw_provenance(f, rows[1]);
        self.draw_status(f, rows[2]);

        if let Some(lines) = popup_lines {
            let popup = centered(area, 76, 60);
            f.render_widget(Clear, popup);
            f.render_widget(
                Paragraph::new(lines)
                    .block(widgets::panel("confirm", true))
                    .alignment(Alignment::Left),
                popup,
            );
        }
    }
}

impl SnapshotScreen {
    /// The list: markers, id, date, status, kernel, description.
    ///
    /// A table rather than a column of ids. The description is how a human
    /// recognises a snapshot and the date is how they place it; showing either
    /// only on demand means scrolling through hashes to find the one you mean.
    fn draw_list(&mut self, f: &mut Frame, area: Rect) {
        let id_w = self
            .snapshots
            .iter()
            .map(|snapshot| snapshot.id.len())
            .max()
            .unwrap_or(12)
            .clamp(12, 16);
        let kernel_w = 21; // "7.2.9-1-cachyos-bore"
                           // The kernel column only earns its place when a description still fits.
        let show_kernel = area.width as usize >= 100;

        // Two border rows plus the header line are not rows of data.
        let visible = area.height.saturating_sub(3) as usize;
        self.follow_selection(visible);
        let total = self.snapshots.len();

        let mut lines: Vec<Line> = Vec::new();
        if self.snapshots.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("No snapshots in {}", self.paths.root.display()),
                theme::dim(),
            )));
        } else {
            let header = if show_kernel {
                format!(
                    "    {:<id_w$} {:<10} {:<9} {:<kernel_w$} DESCRIPTION",
                    "SNAPSHOT", "DATE", "STATUS", "KERNEL"
                )
            } else {
                format!(
                    "    {:<id_w$} {:<10} {:<9} DESCRIPTION",
                    "SNAPSHOT", "DATE", "STATUS"
                )
            };
            lines.push(Line::from(Span::styled(header, theme::dim())));
        }

        // The header stays put and only the rows scroll, so the columns keep their
        // labels while the list moves.
        for (index, snap) in self
            .snapshots
            .iter()
            .enumerate()
            .skip(self.list_off)
            .take(visible)
        {
            let selected = index == self.selection;
            let style = if selected {
                theme::header()
            } else {
                theme::dim()
            };
            let pinned = self.pinned.as_deref() == Some(snap.id.as_str());
            // ▶ booted, ★ golden, P pinned. The letters are deliberate: ▶ and ★
            // are typographic, P is unambiguous about what it means.
            let markers = format!(
                "{}{}{}",
                if snap.booted { '▶' } else { ' ' },
                if snap.golden { '★' } else { ' ' },
                if pinned { 'P' } else { ' ' }
            );
            let description = if snap.hidden {
                format!("{} (hidden)", snap.description())
            } else {
                snap.description()
            };

            let mut spans = vec![
                Span::styled(format!("{markers} "), style),
                Span::styled(format!("{:<id_w$} ", snap.id), style),
                Span::styled(
                    format!("{:<10} ", snap.date().unwrap_or_else(|| "-".to_string())),
                    style,
                ),
                Span::styled(
                    format!("{:<9} ", snap.status().unwrap_or_else(|| "-".to_string())),
                    style,
                ),
            ];
            if show_kernel {
                spans.push(Span::styled(
                    format!(
                        "{:<kernel_w$} ",
                        snap.kernel_version().unwrap_or_else(|| "-".to_string())
                    ),
                    style,
                ));
            }
            spans.push(Span::styled(description, style));
            lines.push(Line::from(spans));
        }

        // A scrolled list says where it is, so it cannot be mistaken for the whole
        // short list. Same shape as the provenance panel's title.
        let title = if visible > 0 && total > visible {
            format!(
                "{}  {}-{} of {}",
                self.list_title(),
                self.list_off + 1,
                (self.list_off + visible).min(total),
                total
            )
        } else {
            self.list_title().to_string()
        };
        f.render_widget(
            Paragraph::new(lines).block(widgets::panel(&title, self.focus == Focus::List)),
            area,
        );
    }

    /// The list panel's title, marked when it holds focus.
    ///
    /// The `▸` is APU's convention for the focused panel; without some mark, `Tab`
    /// only changes a colour and looks like it did nothing.
    fn list_title(&self) -> &'static str {
        if self.focus == Focus::List {
            "▸ SNAP"
        } else {
            "SNAP"
        }
    }

    /// The provenance panel's title, marked when it holds focus.
    fn provenance_title(&self, base: &str) -> String {
        if self.focus == Focus::Provenance {
            format!("▸ {base}")
        } else {
            base.to_string()
        }
    }

    /// The provenance panel for the selected snapshot.
    fn draw_provenance(&mut self, f: &mut Frame, area: Rect) {
        let lines = self.provenance_lines();
        let visible = area.height.saturating_sub(2) as usize;
        let total = lines.len();
        // Clamp here rather than in `scroll_detail`, because this is the only
        // place that knows how many lines actually fit.
        self.detail_scroll = self.detail_scroll.min(total.saturating_sub(visible));

        let shown: Vec<Line> = lines
            .into_iter()
            .skip(self.detail_scroll)
            .take(visible)
            .collect();

        let title = if total > visible {
            format!(
                "provenance  {}-{} of {}  ·  PgUp/PgDn",
                self.detail_scroll + 1,
                (self.detail_scroll + visible).min(total),
                total
            )
        } else {
            "provenance".to_string()
        };

        f.render_widget(
            Paragraph::new(shown).block(widgets::panel(
                &self.provenance_title(&title),
                self.focus == Focus::Provenance,
            )),
            area,
        );
    }

    /// Build the provenance lines for the selected snapshot.
    ///
    /// Sectioned by the question being asked rather than in the order the
    /// manifest happens to store things. A screen of `key: value` is not
    /// provenance anyone can act on; these sections are:
    ///
    /// - **warnings** — what is wrong *now*, computed from the disk rather than
    ///   believed from the manifest;
    /// - **boot** — whether it can boot at all;
    /// - **disk** — what it costs;
    /// - then the manifest itself, grouped: **identity**, **origin**,
    ///   **contents**, **boot record**.
    fn provenance_lines(&self) -> Vec<Line<'static>> {
        let Some(snap) = self.selected() else {
            return vec![Line::from(Span::styled(
                "no snapshot selected",
                theme::dim(),
            ))];
        };

        let heading = |text: &str| Line::from(Span::styled(format!("── {text}"), theme::header()));
        let mut lines: Vec<Line<'static>> = Vec::new();

        // These flags decide which keys are refused, so they lead.
        let mut flags: Vec<&str> = Vec::new();
        if snap.booted {
            flags.push("booted");
        }
        if snap.golden {
            flags.push("golden");
        }
        if snap.hidden {
            flags.push("hidden");
        }
        if self.pinned.as_deref() == Some(snap.id.as_str()) {
            flags.push("pinned (GRUB default)");
        }
        if !flags.is_empty() {
            lines.push(Line::from(Span::styled(flags.join(" · "), theme::header())));
        }

        let warnings = snap.warnings();
        if !warnings.is_empty() {
            lines.push(heading("warnings"));
            for warning in warnings {
                lines.push(Line::from(Span::raw(format!("  ! {warning}"))));
            }
        }

        lines.push(heading("boot"));
        lines.push(Line::from(Span::raw(format!(
            "  vmlinuz {} · initramfs {} · cmdline {} · GRUB entry {}",
            yn(snap.vmlinuz),
            yn(snap.initramfs),
            yn(snap.cmdline_file),
            yn(snap.grub_entry)
        ))));

        match &snap.manifest {
            None => lines.push(Line::from(Span::styled(
                "  no manifest — only the artefacts and the GRUB entry are known",
                theme::dim(),
            ))),
            Some(manifest) => {
                let fields = manifest.fields();
                for section in Section::ORDER {
                    let rows: Vec<&(String, String)> = fields
                        .iter()
                        .filter(|(key, _)| section_for(key) == section)
                        .collect();
                    if rows.is_empty() {
                        continue;
                    }
                    lines.push(heading(section.label()));
                    for (key, value) in rows {
                        lines.push(Line::from(Span::raw(format!("  {key}: {value}"))));
                    }
                }
            }
        }

        // Last, because it is the one line that answers a question nobody asked
        // until they are deciding what to delete.
        lines.push(heading("disk"));
        lines.push(Line::from(Span::raw(format!(
            "  {} total for this snapshot",
            human_bytes(snap.size_bytes(&self.paths))
        ))));

        lines
    }

    /// Move the selected row into the window, scrolling as little as possible.
    ///
    /// Called from the renderer because the panel height is only known there. The
    /// alternative — a fixed page size in the selection code — would scroll by the
    /// wrong amount on every terminal that is not the one it was written on.
    fn follow_selection(&mut self, visible: usize) {
        if self.snapshots.is_empty() || visible == 0 {
            self.list_off = 0;
            return;
        }
        if self.selection < self.list_off {
            self.list_off = self.selection;
        } else if self.selection >= self.list_off + visible {
            self.list_off = self.selection + 1 - visible;
        }
        // Never leave a gap at the bottom: if the last row is on screen, the
        // window is flush with the end of the list.
        self.list_off = self
            .list_off
            .min(self.snapshots.len().saturating_sub(visible));
    }

    fn draw_status(&self, f: &mut Frame, area: Rect) {
        let line = match &self.error {
            Some(err) => Line::from(Span::styled(format!("! {err}"), theme::header())),
            None => Line::from(Span::raw(self.status.clone())),
        };
        f.render_widget(
            Paragraph::new(line)
                .block(widgets::panel("status", false))
                .wrap(Wrap { trim: true }),
            area,
        );
    }
}

/// How the manifest's fields are grouped on screen.
///
/// The grouping is presentation, not schema: it exists so a reader sees "how it
/// was captured" as a block instead of hunting for `source_tree.mode` among
/// thirty unrelated keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Identity,
    Origin,
    Contents,
    BootRecord,
}

impl Section {
    const ORDER: [Section; 4] = [
        Section::Identity,
        Section::Origin,
        Section::Contents,
        Section::BootRecord,
    ];

    fn label(self) -> &'static str {
        match self {
            Section::Identity => "identity",
            Section::Origin => "origin",
            Section::Contents => "contents",
            Section::BootRecord => "recorded at capture",
        }
    }
}

/// Which section a dotted manifest path belongs to.
///
/// Keyed on the top-level block, so a field added inside a known block lands in
/// the right group without being named here. Unknown blocks fall to `origin`,
/// which is the safest: it is where provenance belongs.
fn section_for(key: &str) -> Section {
    match key.split('.').next().unwrap_or_default() {
        "snapshot" => Section::Identity,
        "kernel" | "source_tree" | "registered" => Section::Origin,
        "module" | "patches" | "config" | "firmware" | "initramfs" => Section::Contents,
        "grub" | "boot_files" | "test_results" | "cmdline" => Section::BootRecord,
        _ => Section::Origin,
    }
}

fn yn(present: bool) -> &'static str {
    if present {
        "OK"
    } else {
        "MISSING"
    }
}

/// A rectangle centred in `area`, at most `pct_w`/`pct_h` per cent of it.
fn centered(area: Rect, pct_w: u16, pct_h: u16) -> Rect {
    let width = area.width * pct_w / 100;
    let height = area.height * pct_h / 100;
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

/// Build the pane for the suite shell (the `arieltune` tab set).
///
/// Honours `SNAPSHOT_ROOT` the same way the CLI's `--root` does, so the tab can
/// be pointed at a fixture — for a demo, or on a workstation with no store of
/// its own — without needing a blade.
pub fn screen() -> SnapshotScreen {
    SnapshotScreen::with_paths(paths_for_env(
        std::env::var("SNAPSHOT_ROOT").ok().as_deref(),
    ))
}

/// Resolve which store the pane reads. Pure, so the choice is tested rather than
/// discovered: an empty or unset variable means the real blade layout.
fn paths_for_env(root: Option<&str>) -> Paths {
    match root {
        Some(root) if !root.is_empty() => Paths::under(root),
        _ => Paths::system(),
    }
}

/// Build the pane pointed at a specific store, for tests and `--root`.
pub fn screen_at(paths: PathBuf) -> SnapshotScreen {
    SnapshotScreen::with_paths(Paths::under(paths))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn alt(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
    }

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "snapshot-screen-test-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// Two snapshots, neither booted, golden nor pinned, so actions are allowed.
    fn fixture(tag: &str) -> PathBuf {
        fixture_ids(
            tag,
            &[
                ("snap-aaaaaaaa", "2026-10-08"),
                ("snap-bbbbbbbb", "2026-08-25"),
            ],
        )
    }

    /// The same shape with `n` snapshots, for testing a list longer than the panel.
    fn fixture_many(tag: &str, n: usize) -> PathBuf {
        let ids: Vec<String> = (0..n).map(|i| format!("snap-{i:08x}")).collect();
        let pairs: Vec<(&str, &str)> = ids.iter().map(|id| (id.as_str(), "2026-01-01")).collect();
        fixture_ids(tag, &pairs)
    }

    fn fixture_ids(tag: &str, snapshots: &[(&str, &str)]) -> PathBuf {
        let root = temp_root(tag);
        for (id, date) in snapshots {
            let (id, date) = (*id, *date);
            let snap = root.join(id);
            std::fs::create_dir_all(&snap).unwrap();
            // Modelled on a real manifest rather than the minimum: this pane's job
            // is provenance, so a fixture with one block would prove nothing about
            // how the sections lay out.
            std::fs::write(
                snap.join("manifest.yaml"),
                format!(
                    "snapshot:\n  id: \"{id}\"\n  timestamp: \"{date}T10:00:00Z\"\n  \
                     description: \"{id} desc\"\n  status: \"active\"\n  parent: \"base\"\n\
                     kernel:\n  version: \"7.2.9-1-cachyos-bore\"\n\
                     source_tree:\n  mode: \"running-module\"\n\
                     module:\n  srcversion: \"ABC1234567890\"\n  size: \"6584284 bytes\"\n\
                     patches:\n  applied: false\n  markers: 0\n\
                     grub:\n  entry_title: \"[{id}] {id} desc\"\n\
                     test_results:\n  boot_test: \"PASS\"\n"
                ),
            )
            .unwrap();
        }
        std::fs::create_dir_all(root.join("boot")).unwrap();
        // Real artefacts, so the pane shows a healthy snapshot as well as a
        // damaged one: a fixture where everything is missing only ever proves the
        // warnings render.
        for (id, _) in snapshots {
            std::fs::write(root.join(format!("boot/vmlinuz-{id}")), vec![0u8; 1024]).unwrap();
            std::fs::write(
                root.join(format!("boot/initramfs-{id}.img")),
                vec![0u8; 4096],
            )
            .unwrap();
            std::fs::write(root.join(format!("boot/{id}.cmdline")), b"quiet").unwrap();
        }
        std::fs::create_dir_all(root.join("etc/grub.d")).unwrap();
        std::fs::create_dir_all(root.join("proc")).unwrap();
        std::fs::write(
            root.join("proc/cmdline"),
            "BOOT_IMAGE=/boot/vmlinuz-linux-cachyos\n",
        )
        .unwrap();
        // Only the first gets a menu entry, so the rest carry the "no GRUB entry"
        // warning — one healthy snapshot is what the panel needs to show both.
        let first = snapshots.first().map(|(id, _)| *id).unwrap_or("snap-none");
        std::fs::write(
            root.join("etc/grub.d/40_custom"),
            format!(
                "#!/bin/sh\nexec tail -n +3 $0\n\
                 # === {first}: {first} desc ===\n\
                 menuentry 'x' --id '{first}' {{\n}}\n"
            ),
        )
        .unwrap();
        root
    }

    fn screen(tag: &str) -> (SnapshotScreen, PathBuf) {
        let root = fixture(tag);
        let paths = Paths::under(&root);
        // A recording runner, so confirming an action in a test writes the fixture
        // files but never spawns grub-mkconfig on the test machine.
        (
            SnapshotScreen::with_runner(paths, Box::new(write::Recording::default())),
            root,
        )
    }

    #[test]
    fn lists_snapshots_newest_first() {
        let (s, root) = screen("list");
        assert_eq!(s.snapshots.len(), 2);
        assert_eq!(s.selected_id(), Some("snap-aaaaaaaa"));
        assert!(
            s.status.starts_with("2 snapshot(s)"),
            "status should lead with the count: {}",
            s.status
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The point of the panel: a snapshot is recognised by its description and
    /// placed by its date, so both belong in the list rather than behind a key.
    #[test]
    fn the_list_shows_date_and_description() {
        let (mut s, root) = screen("columns");
        let text = render(&mut s, 120, 24);

        assert!(text.contains("DESCRIPTION"), "no description column");
        assert!(text.contains("2026-10-08"), "first date missing");
        assert!(text.contains("2026-08-25"), "second date missing");
        assert!(text.contains("snap-aaaaaaaa desc"), "description missing");
        assert!(text.contains("SNAPSHOT"), "no header");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Narrow panes drop the kernel column so the description still fits.
    #[test]
    fn a_narrow_pane_still_shows_the_description() {
        let (mut s, root) = screen("narrow");
        let text = render(&mut s, 74, 24);
        assert!(
            !text.contains("KERNEL"),
            "kernel column should have been dropped"
        );
        assert!(
            text.contains("snap-aaaaaaaa desc"),
            "description must survive"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn selection_clamps_at_both_ends() {
        let (mut s, root) = screen("clamp");
        s.select_prev();
        assert_eq!(s.selection, 0, "cannot go above the first row");
        s.select_next();
        s.select_next();
        s.select_next();
        assert_eq!(s.selection, 1, "cannot go past the last row");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The list has to scroll. With more rows than fit, moving down must bring the
    /// selected row into view — otherwise the arrows silently highlight a row that
    /// is not on screen, which is what the archive blade looks like (26 rows in a
    /// panel that holds ten).
    #[test]
    fn the_list_scrolls_to_follow_the_selection() {
        let root = fixture_many("overflow", 30);
        let paths = Paths::under(&root);
        let mut s = SnapshotScreen::with_runner(paths, Box::new(write::Recording::default()));
        assert_eq!(s.snapshots.len(), 30);

        // Taken from the model rather than hardcoded: `discover()` sorts newest
        // first, so with equal timestamps the ids run descending and guessing
        // which end is which is how this test was wrong the first time.
        let first = s.snapshots.first().unwrap().id.clone();
        let last = s.snapshots.last().unwrap().id.clone();

        let text = render(&mut s, 100, 24);
        assert!(text.contains(&first), "the top row starts visible");

        // Every single step must leave the selection on screen, not just the last.
        for _ in 0..29 {
            s.on_key(key(KeyCode::Down));
            let text = render(&mut s, 100, 24);
            let id = s.selected_id().unwrap().to_string();
            assert!(text.contains(&id), "the selected row {id} is off-screen");
        }

        assert_eq!(s.selected_id(), Some(last.as_str()), "at the end");
        let text = render(&mut s, 100, 24);
        assert!(
            !text.contains(&first),
            "the first row should have scrolled away"
        );
        // A scrolled list says so, or it looks like the whole store.
        assert!(text.contains("of 30"), "the title should give the range");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Scrolling back up has to work too, and land flush at the top.
    #[test]
    fn the_list_scrolls_back_to_the_top() {
        let root = fixture_many("scroll-back", 30);
        let paths = Paths::under(&root);
        let mut s = SnapshotScreen::with_runner(paths, Box::new(write::Recording::default()));
        let first = s.snapshots.first().unwrap().id.clone();
        let last = s.snapshots.last().unwrap().id.clone();

        s.on_key(key(KeyCode::End));
        assert_eq!(s.selected_id(), Some(last.as_str()));
        assert!(render(&mut s, 100, 24).contains(&last));

        s.on_key(key(KeyCode::Home));
        let text = render(&mut s, 100, 24);
        assert_eq!(s.selected_id(), Some(first.as_str()));
        assert!(text.contains(&first), "back at the top");
        assert_eq!(s.list_off, 0, "the window scrolled back to the start");
        // The range stays — there are still 25 rows below — but it must start at
        // the first row again. Only a list that fits drops the range entirely.
        assert!(
            text.contains("SNAP  1-"),
            "the range should start at the first row again"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A list that fits must not scroll at all, and must not claim a range.
    #[test]
    fn a_short_list_does_not_scroll() {
        let (mut s, root) = screen("no-scroll");
        s.on_key(key(KeyCode::End));
        let text = render(&mut s, 120, 30);
        assert!(text.contains("snap-aaaaaaaa"), "no scrolling was needed");
        assert!(text.contains("snap-bbbbbbbb"));
        assert!(text.contains("SNAP─"), "no range on a short list");
        assert_eq!(s.list_off, 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A key that opens a confirmation must not have written anything yet.
    #[test]
    fn hide_needs_confirmation_before_anything_happens() {
        let (mut s, root) = screen("confirm");
        let id = s.selected_id().unwrap().to_string();

        assert!(matches!(
            s.on_key(key(KeyCode::Char('h'))),
            Outcome::Consumed
        ));
        assert!(s.is_confirming(), "a popup is up");
        assert!(s.modal(), "and the shell must not steal keys while it is");
        assert_eq!(s.pending, Some(Pending::Hide(id.clone())));

        // Not confirmed: the manifest is still in place, unrenamed.
        let snap = root.join(&id);
        assert!(snap.join("manifest.yaml").is_file());
        assert!(!snap.join("manifest.yaml.hidden").exists());

        s.on_key(key(KeyCode::Esc));
        assert!(!s.is_confirming());
        assert!(snap.join("manifest.yaml").is_file(), "cancel wrote nothing");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn confirming_a_hide_renames_the_manifest() {
        let (mut s, root) = screen("apply");
        let id = s.selected_id().unwrap().to_string();

        s.request(Pending::Hide(id.clone()));
        s.confirm();

        assert!(s.error.is_none(), "unexpected error: {:?}", s.error);
        assert!(
            root.join(&id).join("manifest.yaml.hidden").is_file(),
            "the manifest should be renamed"
        );
        assert!(!root.join(&id).join("manifest.yaml").exists());
        // ...and the pane now shows it as hidden (it is still listed).
        assert_eq!(s.snapshots.len(), 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The guards must reach the pane: hiding the booted snapshot is refused
    /// with an explanation, not applied.
    #[test]
    fn a_refused_action_surfaces_an_error_and_writes_nothing() {
        let (mut s, root) = screen("refuse");
        let id = s.selected_id().unwrap().to_string();

        // Make the selected snapshot look booted.
        std::fs::write(
            root.join("proc/cmdline"),
            format!("BOOT_IMAGE=/boot/vmlinuz-{id} root=UUID=x\n"),
        )
        .unwrap();
        s.refresh();

        s.request(Pending::Hide(id.clone()));
        s.confirm();

        let err = s.error.as_deref().expect("an error is surfaced");
        assert!(err.contains("booted snapshot"), "{err}");
        assert!(
            root.join(&id).join("manifest.yaml").is_file(),
            "a refusal must not have renamed anything"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The popup swallows everything it does not act on — a stray key must never
    /// answer a question about writing to /boot — but `q` backs out, like APU's
    /// and MEM's popups.
    #[test]
    fn a_popup_swallows_stray_keys_but_q_cancels() {
        let (mut s, root) = screen("popup-keys");
        let id = s.selected_id().unwrap().to_string();
        s.on_key(key(KeyCode::Char('h')));
        assert!(s.is_confirming());

        // A stray digit must not switch tabs out from under the question.
        assert!(matches!(
            s.on_key(key(KeyCode::Char('3'))),
            Outcome::Consumed
        ));
        assert!(s.is_confirming(), "still asking");
        assert!(matches!(
            s.on_key(key(KeyCode::Char('q'))),
            Outcome::Consumed
        ));
        assert!(!s.is_confirming(), "q cancels");
        assert!(
            root.join(&id).join("manifest.yaml").is_file(),
            "nothing written"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unrelated_keys_are_left_for_the_shell() {
        let (mut s, root) = screen("ignored");
        assert!(matches!(
            s.on_key(key(KeyCode::Char('9'))),
            Outcome::Ignored
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Every other pane quits on a bare `q`. This one used to answer only to
    /// Ctrl-Q, which made the last tab the odd one out.
    #[test]
    fn bare_q_quits_like_the_other_tabs() {
        let (mut s, root) = screen("quit-q");
        assert!(matches!(s.on_key(key(KeyCode::Char('q'))), Outcome::Quit));
        assert!(matches!(s.on_key(key(KeyCode::Esc)), Outcome::Quit));
        assert!(matches!(s.on_key(ctrl('c')), Outcome::Quit));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The shell's globals must still get through: tab switching, F-keys, and
    /// Ctrl/Alt chords — including Ctrl-Q, the always-available quit.
    #[test]
    fn the_shell_keeps_its_global_keys() {
        let (mut s, root) = screen("handback");
        for k in [
            ctrl('q'),
            ctrl('1'),
            alt('5'),
            key(KeyCode::F(5)),
            key(KeyCode::Char('1')),
            key(KeyCode::Char('5')),
            ctrl('p'),
            ctrl('h'),
        ] {
            assert!(
                matches!(s.on_key(k), Outcome::Ignored),
                "the shell should have seen {k:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The reason modified keys are handed back *before* the pane's own bindings:
    /// `Char('p')` matches Ctrl-P too, and arming a pin from a stray chord would
    /// put a write-to-/boot confirmation in front of someone who pressed Ctrl-P.
    #[test]
    fn a_modified_letter_does_not_arm_an_action() {
        let (mut s, root) = screen("chord");
        for k in [ctrl('p'), ctrl('h'), alt('p'), alt('H')] {
            s.on_key(k);
            assert!(!s.is_confirming(), "{k:?} armed a confirmation");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tab_moves_focus_between_the_two_regions() {
        let (mut s, root) = screen("focus");
        assert_eq!(s.focus(), Focus::List);
        assert!(matches!(s.on_key(key(KeyCode::Tab)), Outcome::Consumed));
        assert_eq!(s.focus(), Focus::Provenance);
        s.on_key(key(KeyCode::BackTab));
        assert_eq!(s.focus(), Focus::List);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `Tab` only changes a colour unless the pane says which region has focus, so
    /// the marker has to actually appear.
    #[test]
    fn the_focused_panel_is_marked() {
        let (mut s, root) = screen("marker");
        let text = render(&mut s, 120, 30);
        assert!(text.contains("▸ SNAP"), "the list should be marked");

        s.on_key(key(KeyCode::Tab));
        let text = render(&mut s, 120, 30);
        assert!(!text.contains("▸ SNAP"), "the list gave up focus");
        assert!(text.contains("▸ provenance"), "the panel should be marked");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The arrows act on whichever region has focus — the WIKI/APU habit.
    #[test]
    fn arrows_navigate_the_focused_region() {
        let (mut s, root) = screen("navigate");

        // List focused: the arrows move the selection, not the panel.
        s.on_key(key(KeyCode::Down));
        assert_eq!(s.selected_id(), Some("snap-bbbbbbbb"));
        assert_eq!(s.detail_scroll, 0);

        // Panel focused: the same key scrolls instead.
        s.on_key(key(KeyCode::Tab));
        s.on_key(key(KeyCode::Down));
        assert_eq!(s.detail_scroll, 1);
        assert_eq!(s.selected_id(), Some("snap-bbbbbbbb"), "selection held");

        // `k`/`j` are the same keys as the arrows, everywhere in the suite.
        s.on_key(key(KeyCode::Char('k')));
        assert_eq!(s.detail_scroll, 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn paging_moves_by_a_page_in_the_focused_region() {
        let (mut s, root) = screen("page");
        s.on_key(key(KeyCode::PageDown));
        assert_eq!(s.selected_id(), Some("snap-bbbbbbbb"), "one row is the end");

        s.on_key(key(KeyCode::Tab));
        s.on_key(key(KeyCode::PageDown));
        assert_eq!(s.detail_scroll, PAGE as usize);
        s.on_key(key(KeyCode::PageUp));
        assert_eq!(s.detail_scroll, 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn home_and_end_jump_the_list() {
        let (mut s, root) = screen("jump");
        s.on_key(key(KeyCode::End));
        assert_eq!(s.selected_id(), Some("snap-bbbbbbbb"));
        s.on_key(key(KeyCode::Home));
        assert_eq!(s.selected_id(), Some("snap-aaaaaaaa"));
        // `g`/`G` are the same jumps, as in APU's popup.
        s.on_key(key(KeyCode::Char('G')));
        assert_eq!(s.selected_id(), Some("snap-bbbbbbbb"));
        s.on_key(key(KeyCode::Char('g')));
        assert_eq!(s.selected_id(), Some("snap-aaaaaaaa"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `End` in the panel must not overflow the scroll arithmetic on the way back.
    #[test]
    fn jumping_to_the_end_of_the_panel_is_reversible() {
        let (mut s, root) = screen("panel-end");
        s.on_key(key(KeyCode::Tab));
        s.on_key(key(KeyCode::End));
        s.on_key(key(KeyCode::Up));
        assert!(s.detail_scroll > 0, "still near the bottom");
        s.on_key(key(KeyCode::Home));
        assert_eq!(s.detail_scroll, 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn enter_expands_the_panel_and_focuses_it() {
        let (mut s, root) = screen("enter");
        assert_eq!(s.focus(), Focus::List);
        s.on_key(key(KeyCode::Enter));
        assert!(!s.detail, "enter collapsed the panel");
        assert_eq!(s.focus(), Focus::List, "collapsing returns to the list");
        s.on_key(key(KeyCode::Enter));
        assert!(s.detail, "enter expands it again");
        assert_eq!(s.focus(), Focus::Provenance, "expanding means reading it");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn detail_toggles() {
        let (mut s, root) = screen("detail");
        assert!(s.detail, "provenance starts visible");
        s.toggle_detail();
        assert!(!s.detail);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The provenance panel must answer questions, not list keys in file order.
    #[test]
    fn provenance_is_sectioned() {
        let (mut s, root) = screen("provenance");
        let text = render(&mut s, 120, 40);

        for heading in ["identity", "origin", "contents", "disk", "boot"] {
            assert!(
                text.contains(&format!("── {heading}")),
                "missing section: {heading}"
            );
        }
        // Fields appear under their section, dotted, as recorded.
        assert!(text.contains("snapshot.description"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Sections are assigned by top-level block, so a field inside a known block
    /// lands correctly without being named.
    #[test]
    fn fields_are_grouped_by_block() {
        assert_eq!(section_for("snapshot.id"), Section::Identity);
        assert_eq!(section_for("source_tree.mode"), Section::Origin);
        assert_eq!(section_for("module.srcversion"), Section::Contents);
        assert_eq!(section_for("patches.files[0].markers"), Section::Contents);
        assert_eq!(section_for("test_results.boot_test"), Section::BootRecord);
        // Unknown blocks default to provenance rather than being dropped.
        assert_eq!(section_for("something_new"), Section::Origin);
    }

    /// A dev snapshot's manifest must be *readable in the pane*, not merely
    /// well-formed on disk.
    ///
    /// The panel iterates the manifest's own fields, so a key renamed in
    /// `register` would go on parsing, go on being written, and quietly stop being
    /// displayed — which is the failure this test exists to catch, and the reason
    /// it goes through the real `register::manifest_text` rather than a literal
    /// that could drift from it.
    #[test]
    fn a_dev_snapshot_shows_its_provenance() {
        let root = temp_root("devprovenance");
        let id = "snap-3f9a1c2b";
        let snap = root.join(id);
        std::fs::create_dir_all(snap.join("modules")).unwrap();

        let req = crate::register::Request {
            id: id.to_string(),
            description: "printf marker, iteration 3".to_string(),
            module: snap.join("modules/module.ko.zst"),
            initramfs: root.join("work/dev.img"),
            vmlinuz_source: root.join("work/vmlinuz"),
            kernel_version: "7.2.9-1-cachyos-bore".to_string(),
            cmdline: "BOOT_IMAGE=/boot/vmlinuz-snap-3f9a1c2b root=UUID=abc rw".to_string(),
            root_uuid: "757f018a-71b0-44fc-aac8-c5b2ed165923".to_string(),
            parent: Some("linux-cachyos-bore".to_string()),
            tree: Some(PathBuf::from("/home/nightcarnage/tree")),
            rebuilt_modules: vec!["drivers/gpu/drm/amd/amdgpu".to_string()],
            changed_files: None,
            module_md5: "016a410f8b86e1b5d10efbaa856ce22f".to_string(),
            module_size: 7358583,
            module_srcversion: Some("A6388DF88F71A0EE65E1EED".to_string()),
            module_vermagic: Some("7.2.9-1-cachyos-bore SMP preempt mod_unload".to_string()),
            store_root: root.clone(),
            boot: root.join("boot"),
            acpi_override: false,
        };
        std::fs::write(
            snap.join("manifest.yaml"),
            crate::register::manifest_text(&req, "2026-10-09T14:36:13Z"),
        )
        .unwrap();

        std::fs::create_dir_all(root.join("boot")).unwrap();
        for name in ["vmlinuz", "initramfs", "cmdline"] {
            let path = match name {
                "vmlinuz" => root.join(format!("boot/vmlinuz-{id}")),
                "initramfs" => root.join(format!("boot/initramfs-{id}.img")),
                _ => root.join(format!("boot/{id}.cmdline")),
            };
            std::fs::write(path, b"x").unwrap();
        }
        std::fs::create_dir_all(root.join("etc/grub.d")).unwrap();
        std::fs::create_dir_all(root.join("proc")).unwrap();
        std::fs::write(root.join("proc/cmdline"), "BOOT_IMAGE=x\n").unwrap();
        std::fs::write(
            root.join("etc/grub.d/40_custom"),
            format!("#!/bin/sh\nmenuentry 'x' --id '{id}' {{\n}}\n"),
        )
        .unwrap();

        let mut s =
            SnapshotScreen::with_runner(Paths::under(&root), Box::new(write::Recording::default()));

        // Assert on the pane's full line list rather than on a rendered frame: the
        // frame is a viewport, and a field pushed below the fold is still a field
        // the pane shows. Asserting on the frame made this test fail for the wrong
        // reason the first time it ran.
        let lines: String = s
            .provenance_lines()
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        // Identity and origin: what it is, and where it came from.
        for field in [
            "snapshot.id",
            "snapshot.description",
            "snapshot.status",
            "snapshot.parent",
            "kernel.version",
            "source_tree.mode",
            "source_tree.path",
        ] {
            assert!(lines.contains(field), "{field} should be shown:\n{lines}");
        }
        // Contents: the module's identity, which is what makes two dev snapshots
        // distinguishable from one another.
        for field in [
            "module.path",
            "module.srcversion",
            "module.md5",
            "module.size",
        ] {
            assert!(lines.contains(field), "{field} should be shown:\n{lines}");
        }
        // Boot record: the paths the entry actually uses.
        for field in [
            "boot_files.vmlinuz",
            "boot_files.initramfs",
            "grub.root_uuid",
        ] {
            assert!(lines.contains(field), "{field} should be shown:\n{lines}");
        }

        // And the sections really do lay out in a rendered frame.
        let text = render(&mut s, 120, 50);
        for heading in ["identity", "origin", "contents"] {
            assert!(
                text.contains(&format!("── {heading}")),
                "missing section {heading}:\n{text}"
            );
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scrolling_the_provenance_panel_is_clamped_to_zero() {
        let (mut s, root) = screen("scroll");
        s.scroll_detail(-5);
        assert_eq!(s.detail_scroll, 0, "cannot scroll above the first line");
        s.scroll_detail(4);
        assert_eq!(s.detail_scroll, 4);
        // Changing selection resets it, or the next snapshot opens half-read.
        s.select_next();
        assert_eq!(s.detail_scroll, 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_store_can_be_pointed_at_a_fixture() {
        assert_eq!(
            paths_for_env(Some("/some/fixture")).root,
            PathBuf::from("/some/fixture")
        );
        assert_eq!(
            paths_for_env(None).root,
            PathBuf::from(crate::store::DEFAULT_ROOT),
            "unset means the real blade layout"
        );
        assert_eq!(
            paths_for_env(Some("")).root,
            PathBuf::from(crate::store::DEFAULT_ROOT),
            "an empty variable must not mean an empty root"
        );
    }

    /// Not an assertion — a way to look at the pane without a terminal.
    ///
    /// `cargo test -p snapshot dump_pane -- --ignored --nocapture` prints the
    /// rendered frame, which is how the layout gets reviewed after a change
    /// instead of guessing at column widths.
    #[test]
    #[ignore = "prints the pane for review; run with --ignored --nocapture"]
    fn dump_pane() {
        let (mut s, root) = screen("dump");
        println!("\n{}", render(&mut s, 120, 30));
        s.select_next();
        println!("\n{}", render(&mut s, 120, 30));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The overflowing case, which the two-snapshot fixture cannot show.
    ///
    /// Worth having as a review aid: the whole point of the viewport is that the
    /// selected row is on screen, and that is easier to see than to assert.
    #[test]
    #[ignore = "prints the pane for review; run with --ignored --nocapture"]
    fn dump_scrolled_pane() {
        let root = fixture_many("dump-scroll", 26);
        let paths = Paths::under(&root);
        let mut s = SnapshotScreen::with_runner(paths, Box::new(write::Recording::default()));
        printed(
            "a fresh pane -- the top of the list, and the range in the title",
            &mut s,
        );
        for _ in 0..12 {
            s.on_key(key(KeyCode::Down));
        }
        printed(
            "after twelve downs -- the window has followed the selection",
            &mut s,
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn printed(what: &str, s: &mut SnapshotScreen) {
        println!("\n--- {what} ---\n{}", render(s, 120, 30));
    }

    // ---- rendering ---------------------------------------------------------
    //
    // Asserted against ratatui's TestBackend rather than a pty: this is the same
    // widget tree the shell drives, rendered into a buffer the test can read, so
    // a broken layout fails here instead of needing an operator at a console.

    fn render(screen: &mut SnapshotScreen, width: u16, height: u16) -> String {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test backend");
        terminal
            .draw(|frame| {
                let area = frame.area();
                screen.draw(frame, area);
            })
            .expect("draws without error");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn the_pane_renders_its_title_and_rows() {
        let (mut s, root) = screen("render");
        let text = render(&mut s, 120, 30);

        assert!(text.contains("SNAP"), "panel title missing");
        assert!(text.contains("snap-aaaaaaaa"), "first row missing");
        assert!(text.contains("snap-bbbbbbbb"), "second row missing");
        assert!(text.contains("2 snapshot(s)"), "status line missing");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The confirmation popup must show what would happen *before* it happens —
    /// that is the whole reason for planning separately from applying.
    #[test]
    fn the_confirm_popup_renders_the_plan() {
        let (mut s, root) = screen("render-popup");
        let id = s.selected_id().unwrap().to_string();
        s.request(Pending::Hide(id.clone()));

        let text = render(&mut s, 100, 24);
        assert!(text.contains("confirm"), "popup frame missing");
        assert!(
            text.contains(&format!("hide {id}")),
            "the plan summary should name the action"
        );
        assert!(
            text.contains("y to apply"),
            "the popup must say how to proceed"
        );
        // ...and nothing has happened yet.
        assert!(root.join(&id).join("manifest.yaml").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A refusal is shown, not swallowed, and the rows stay drawn.
    #[test]
    fn an_error_is_rendered_in_the_status_area() {
        let (mut s, root) = screen("render-error");
        let id = s.selected_id().unwrap().to_string();
        std::fs::write(
            root.join("proc/cmdline"),
            format!("BOOT_IMAGE=/boot/vmlinuz-{id} root=UUID=x\n"),
        )
        .unwrap();
        s.refresh();
        s.request(Pending::Hide(id));
        s.confirm();

        let text = render(&mut s, 120, 24);
        assert!(
            text.contains("booted snapshot"),
            "the reason should be on screen"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
