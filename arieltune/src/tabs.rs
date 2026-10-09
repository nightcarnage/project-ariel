// SPDX-License-Identifier: GPL-2.0-only
//! Tab set. Each real screen is live now: WIKI (M1), BIOS (M3), APU (M5), MEM
//! (M4). `StubScreen` is retained only as a hermetic Screen double for the shell
//! routing tests (`crate::shell` tests construct it), so it is `dead_code` in a
//! production (non-test) build -- allowed below rather than removed.

use arieltune_tui_kit::{theme, widgets, Outcome, Screen};
use crossterm::event::KeyEvent;
use ratatui::layout::{Alignment, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

/// A no-op Screen (returns `Ignored` for every key) used only by the shell tests
/// to exercise the global switch/quit routing independent of the real tab set.
#[allow(dead_code)]
pub struct StubScreen {
    title: &'static str,
    blurb: &'static str,
    milestone: &'static str,
}

#[allow(dead_code)]
impl StubScreen {
    pub fn new(title: &'static str, blurb: &'static str, milestone: &'static str) -> Self {
        StubScreen {
            title,
            blurb,
            milestone,
        }
    }
}

impl Screen for StubScreen {
    fn title(&self) -> &'static str {
        self.title
    }

    fn draw(&mut self, f: &mut Frame, area: Rect) {
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(self.title, theme::header())),
            Line::from(""),
            Line::from(Span::raw(self.blurb)),
            Line::from(""),
            Line::from(Span::styled(
                format!("stub tab -- real screen lands in {}", self.milestone),
                theme::dim(),
            )),
        ];
        let p = Paragraph::new(lines)
            .block(widgets::panel(self.title, true))
            .alignment(Alignment::Center);
        f.render_widget(p, area);
    }

    fn on_key(&mut self, _key: KeyEvent) -> Outcome {
        Outcome::Ignored
    }

    fn status_hint(&self) -> Option<String> {
        Some(format!("{} (stub)", self.title))
    }
}

/// The five tabs in fixed order WIKI | BIOS | APU | MEM | SNAP.
///
/// Order is the shell's business, not each screen's: it is what the `1`-`5` keys
/// and `--tab` resolve against, so appending here is what makes a tab reachable.
pub fn screens() -> Vec<Box<dyn Screen>> {
    vec![
        Box::new(wiki::screen()),
        Box::new(bios::screen()),
        Box::new(apu::screen()),
        Box::new(mem::screen()),
        Box::new(snapshot::screen().with_series(kernel_series())),
    ]
}

/// The liberation series for every supported kernel, in the shape the SNAP pane
/// takes.
///
/// Built here rather than inside the pane: the series is APU's reference data — it
/// owns the patch bodies and the runtime tells — and the pane's job is boot
/// configuration, so reaching across for it would put the GPU stack behind a tool
/// that never touches the GPU. The shell is the one place that already depends on
/// both, and this is the single copy of the series the suite uses.
fn kernel_series() -> Vec<snapshot::screen::KernelSeries> {
    apu::kernel::Kernel::ALL
        .iter()
        .map(|kernel| snapshot::screen::KernelSeries {
            release: kernel.release().to_string(),
            entries: apu::patches::series(*kernel)
                .map(|patch| snapshot::screen::SeriesEntry {
                    id: patch.id.to_string(),
                    title: patch.title.to_string(),
                    touches: patch.touches.to_string(),
                })
                .collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The injection is the one thing here that can fail silently: if it produced an
    /// empty series, the pane would say "no curated series is known for this kernel"
    /// and look correct while telling the operator nothing. So the shape is asserted,
    /// not assumed.
    #[test]
    fn every_supported_kernel_gets_its_series() {
        let series = kernel_series();
        assert_eq!(
            series.len(),
            apu::kernel::Kernel::ALL.len(),
            "one series per supported kernel"
        );

        for (supplied, kernel) in series.iter().zip(apu::kernel::Kernel::ALL) {
            // The releases must match exactly, or the pane's lookup — which is an
            // equality test against what a snapshot records — would never hit.
            assert_eq!(supplied.release, kernel.release());
            assert!(
                !supplied.entries.is_empty(),
                "{} has an empty series",
                supplied.release
            );
            // And the entries must be named, or the popup would list blanks.
            for entry in &supplied.entries {
                assert!(!entry.id.is_empty());
                assert!(!entry.title.is_empty());
                assert!(!entry.touches.is_empty());
            }
        }
    }

    /// The suite builds the SNAP pane with the series attached; the standalone binary
    /// builds it without. This is the suite's, so it must have them.
    #[test]
    fn the_snap_pane_is_built_with_the_series() {
        let pane = snapshot::screen().with_series(kernel_series());
        for kernel in apu::kernel::Kernel::ALL {
            assert!(
                pane.knows_series_for(kernel.release()),
                "the pane should recognise {}",
                kernel.release()
            );
        }
    }
}
