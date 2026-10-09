// SPDX-License-Identifier: GPL-2.0-only
//! The SNAPSHOTS tab.
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

use crate::store::{Paths, Store};
use crate::write::{self, Plan};

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
    ids: Vec<String>,
    descriptions: Vec<String>,
    selection: usize,
    detail: bool,
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
        let mut screen = SnapshotScreen {
            paths,
            ids: Vec::new(),
            descriptions: Vec::new(),
            selection: 0,
            detail: false,
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
                // Hidden snapshots are listed here on purpose: this is the pane
                // from which you unhide them, and `--all` is the CLI's way to
                // ask the same question.
                self.ids = snapshots.iter().map(|s| s.id.clone()).collect();
                self.descriptions = snapshots
                    .iter()
                    .map(|s| {
                        format!(
                            "{}  {}  {}",
                            s.date().unwrap_or_else(|| "-".to_string()),
                            s.status().unwrap_or_else(|| "-".to_string()),
                            s.description()
                        )
                    })
                    .collect();
                if self.selection >= self.ids.len() {
                    self.selection = self.ids.len().saturating_sub(1);
                }
                self.status = format!("{} snapshot(s)", self.ids.len());
            }
            Err(err) => {
                self.error = Some(err.to_string());
                self.status = "cannot read the snapshot store".to_string();
            }
        }
    }

    pub fn selected_id(&self) -> Option<&str> {
        self.ids.get(self.selection).map(String::as_str)
    }

    /// True while the confirmation popup is up.
    pub fn is_confirming(&self) -> bool {
        self.pending.is_some()
    }

    pub fn select_next(&mut self) {
        if !self.ids.is_empty() {
            self.selection = (self.selection + 1).min(self.ids.len() - 1);
        }
    }

    pub fn select_prev(&mut self) {
        self.selection = self.selection.saturating_sub(1);
    }

    pub fn toggle_detail(&mut self) {
        self.detail = !self.detail;
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
            Ok(plan) => match write::apply(&plan, false) {
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
        "SNAPSHOTS"
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
        Some("↑↓ move · enter detail · p/P pin/unpin · h/H hide/unhide · r refresh".to_string())
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

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(6), Constraint::Length(4)])
            .split(area);

        let mut lines: Vec<Line> = Vec::new();
        if self.ids.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("No snapshots in {}", self.paths.root.display()),
                theme::dim(),
            )));
        }
        for (i, id) in self.ids.iter().enumerate() {
            let marker = if i == self.selection { "▶ " } else { "  " };
            let style = if i == self.selection {
                theme::header()
            } else {
                theme::dim()
            };
            lines.push(Line::from(Span::styled(format!("{marker}{id}"), style)));
            if self.detail {
                lines.push(Line::from(Span::raw(format!(
                    "      {}",
                    self.descriptions.get(i).cloned().unwrap_or_default()
                ))));
            }
        }
        f.render_widget(
            Paragraph::new(lines).block(widgets::panel("SNAPSHOTS", true)),
            rows[0],
        );

        let mut bottom: Vec<Line> = Vec::new();
        if let Some(err) = &self.error {
            bottom.push(Line::from(Span::styled(
                format!("! {err}"),
                theme::header(),
            )));
        } else {
            bottom.push(Line::from(Span::raw(self.status.clone())));
        }
        if let Some(id) = self.selected_id() {
            bottom.push(Line::from(Span::styled(
                format!("selected: {id}"),
                theme::dim(),
            )));
        }
        f.render_widget(
            Paragraph::new(bottom)
                .block(widgets::panel("status", false))
                .wrap(Wrap { trim: true }),
            rows[1],
        );

        if let Some(lines) = popup_lines {
            let popup = centered(area, 70, 60);
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
            std::fs::write(
                snap.join("manifest.yaml"),
                format!("snapshot:\n  id: \"{id}\"\n  timestamp: \"{date}T10:00:00Z\"\n  description: \"{id} desc\"\n"),
            )
            .unwrap();
        }
        std::fs::create_dir_all(root.join("boot")).unwrap();
        std::fs::create_dir_all(root.join("etc/grub.d")).unwrap();
        std::fs::create_dir_all(root.join("proc")).unwrap();
        std::fs::write(
            root.join("proc/cmdline"),
            "BOOT_IMAGE=/boot/vmlinuz-linux-cachyos\n",
        )
        .unwrap();
        std::fs::write(
            root.join("etc/grub.d/40_custom"),
            "#!/bin/sh\nexec tail -n +3 $0\n",
        )
        .unwrap();
        root
    }

    fn screen(tag: &str) -> (SnapshotScreen, PathBuf) {
        let root = fixture(tag);
        let paths = Paths::under(&root);
        (SnapshotScreen::with_paths(paths), root)
    }

    #[test]
    fn lists_snapshots_newest_first() {
        let (s, root) = screen("list");
        assert_eq!(s.ids.len(), 2);
        assert_eq!(s.selected_id(), Some("snap-aaaaaaaa"));
        assert_eq!(s.status, "2 snapshot(s)");
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
        assert_eq!(s.ids.len(), 2);
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
        assert!(!s.detail);
        s.toggle_detail();
        assert!(s.detail);
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
        let text = render(&mut s, 100, 20);

        assert!(text.contains("SNAPSHOTS"), "panel title missing");
        assert!(text.contains("snap-aaaaaaaa"), "first row missing");
        assert!(text.contains("snap-bbbbbbbb"), "second row missing");
        assert!(text.contains('▶'), "selection marker missing");
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
