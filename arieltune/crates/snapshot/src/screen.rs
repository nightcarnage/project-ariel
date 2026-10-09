// SPDX-License-Identifier: GPL-2.0-only
//! The SNAPTUNE tab.
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
//! ## Why the logic is not in `draw`
//!
//! Selection, filtering and the pending-action state are plain data with plain
//! methods ([`SnapshotScreen::select_next`], [`SnapshotScreen::plan_for`], …) so
//! they can be tested without a terminal. `draw` only renders that state.

use std::path::PathBuf;
use std::time::Duration;

use arieltune_tui_kit::{theme, widgets, Outcome, Screen};
use crossterm::event::{KeyCode, KeyEvent};
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
        SnapshotScreen::with_runner(paths, Box::new(System))
    }

    /// The pane with an injected command runner, for tests.
    pub fn with_runner(paths: Paths, runner: Box<dyn Runner>) -> Self {
        let mut screen = SnapshotScreen {
            paths,
            snapshots: Vec::new(),
            pinned: None,
            selection: 0,
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
        if !self.snapshots.is_empty() {
            self.selection = (self.selection + 1).min(self.snapshots.len() - 1);
            self.detail_scroll = 0;
        }
    }

    pub fn select_prev(&mut self) {
        self.selection = self.selection.saturating_sub(1);
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

    pub fn toggle_detail(&mut self) {
        self.detail = !self.detail;
        self.detail_scroll = 0;
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
            "y to apply, n or Esc to cancel",
            theme::dim(),
        )));
        lines
    }
}

impl Screen for SnapshotScreen {
    fn title(&self) -> &'static str {
        "SNAPTUNE"
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
        Some(
            "↑↓ move · enter detail · PgUp/PgDn scroll · p/P pin/unpin · h/H hide/unhide · r refresh"
                .to_string(),
        )
    }

    fn on_key(&mut self, key: KeyEvent) -> Outcome {
        // While confirming, the popup owns the keyboard — including digits and
        // q, which is why `modal()` also reports true.
        if self.is_confirming() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.confirm();
                    Outcome::Consumed
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.cancel();
                    Outcome::Consumed
                }
                _ => Outcome::Consumed,
            }
        } else {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.select_prev();
                    Outcome::Consumed
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.select_next();
                    Outcome::Consumed
                }
                KeyCode::Enter => {
                    self.toggle_detail();
                    Outcome::Consumed
                }
                KeyCode::Char('r') => {
                    self.refresh();
                    Outcome::Consumed
                }
                KeyCode::PageDown => {
                    self.scroll_detail(5);
                    Outcome::Consumed
                }
                KeyCode::PageUp => {
                    self.scroll_detail(-5);
                    Outcome::Consumed
                }
                KeyCode::Char('p') => {
                    if let Some(id) = self.selected_id() {
                        let id = id.to_string();
                        self.request(Pending::Pin(id));
                    }
                    Outcome::Consumed
                }
                KeyCode::Char('P') => {
                    self.request(Pending::Unpin);
                    Outcome::Consumed
                }
                KeyCode::Char('h') => {
                    if let Some(id) = self.selected_id() {
                        let id = id.to_string();
                        self.request(Pending::Hide(id));
                    }
                    Outcome::Consumed
                }
                KeyCode::Char('H') => {
                    if let Some(id) = self.selected_id() {
                        let id = id.to_string();
                        self.request(Pending::Unhide(id));
                    }
                    Outcome::Consumed
                }
                _ => Outcome::Ignored,
            }
        }
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
        let provenance = if self.detail {
            (area.height * 55 / 100).max(6)
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
    fn draw_list(&self, f: &mut Frame, area: Rect) {
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

        for (index, snap) in self.snapshots.iter().enumerate() {
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

        f.render_widget(
            Paragraph::new(lines).block(widgets::panel("SNAPTUNE", true)),
            area,
        );
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
            Paragraph::new(shown).block(widgets::panel(&title, false)),
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
        let root = temp_root(tag);
        for (id, date) in [
            ("snap-aaaaaaaa", "2026-10-08"),
            ("snap-bbbbbbbb", "2026-08-25"),
        ] {
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
        for id in ["snap-aaaaaaaa", "snap-bbbbbbbb"] {
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
        std::fs::write(
            root.join("etc/grub.d/40_custom"),
            "#!/bin/sh\nexec tail -n +3 $0\n\
             # === snap-aaaaaaaa: snap-aaaaaaaa desc ===\n\
             menuentry 'x' --id 'snap-aaaaaaaa' {\n}\n",
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

    #[test]
    fn unrelated_keys_are_left_for_the_shell() {
        let (mut s, root) = screen("ignored");
        assert!(matches!(
            s.on_key(key(KeyCode::Char('9'))),
            Outcome::Ignored
        ));
        assert!(matches!(s.on_key(key(KeyCode::Tab)), Outcome::Ignored));
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

        assert!(text.contains("SNAPTUNE"), "panel title missing");
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
