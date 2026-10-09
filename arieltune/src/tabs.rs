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
                    tell: tell_of(patch.tell),
                })
                .collect(),
        })
        .collect()
}

/// Translate a patch's runtime tell into the form the SNAP pane can check against a
/// *stored module*.
///
/// APU's tells are about a running kernel; a snapshot has no running kernel, so what
/// crosses the boundary is only what a built module can still be asked about — a
/// debugfs node name and a module parameter are string literals in the object, and the
/// clock and CU-count tells are not answerable from a file at all.
///
/// **Exhaustive, with no `_` arm.** A new tell in APU's series must stop this
/// compiling, because the alternative is that it arrives here as "no tell" and every
/// patch using it silently shows as unprovable when it is not.
fn tell_of(tell: apu::patches::Tell) -> snapshot::screen::Tell {
    use apu::patches::Tell as Apu;
    use snapshot::screen::Tell as Snap;
    match tell {
        Apu::Debugfs(name) => Snap::Debugfs(name.to_string()),
        Apu::ModParam(name) => Snap::ModParam(name.to_string()),
        Apu::SclkMax(mhz) => Snap::Runtime(format!("pp_dpm_sclk >= {mhz} MHz")),
        Apu::CuCount(cus) => Snap::Runtime(format!(">= {cus} active CUs")),
        Apu::Bundled => Snap::Bundled,
    }
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

    /// The boundary this injection sits on: the series is the *applied* series, not
    /// every patch on disk. Patch 19 was retired from it by 26+27 and its module
    /// parameter is deliberately absent from every module built since, so listing it
    /// would put a permanent red `NOT in this snapshot's module` against a patch that
    /// is not supposed to be there.
    #[test]
    fn the_series_handed_over_is_the_applied_one_not_every_patch_on_disk() {
        let series = kernel_series();
        let v729 = series
            .iter()
            .find(|s| s.release == apu::kernel::Kernel::Bore729.release())
            .expect("7.2.9 has a series");

        assert!(
            !v729.entries.iter().any(|e| e.id == "19"),
            "retired patch 19 must not be offered as part of the series: {:?}",
            v729.entries.iter().map(|e| &e.id).collect::<Vec<_>>()
        );
        assert_eq!(
            v729.entries.len(),
            apu::patches::count(apu::kernel::Kernel::Bore729),
            "the injected list is the same list the CLI counts"
        );
    }

    /// Every patch's tell must arrive as one of the four forms, and a tell that can be
    /// seen in a file must arrive as one this tool can look for. A tell that silently
    /// became `Bundled` would show as unprovable when it is in fact checkable.
    #[test]
    fn every_tell_crosses_the_boundary_as_the_right_kind() {
        let mut checkable = 0;
        for kernel in apu::kernel::Kernel::ALL {
            for patch in apu::patches::series(kernel) {
                let crossed = tell_of(patch.tell);
                match (patch.tell, &crossed) {
                    (apu::patches::Tell::Debugfs(n), snapshot::screen::Tell::Debugfs(m)) => {
                        assert_eq!(n, m, "patch {}", patch.id);
                        checkable += 1;
                    }
                    (apu::patches::Tell::ModParam(n), snapshot::screen::Tell::ModParam(m)) => {
                        assert_eq!(n, m, "patch {}", patch.id);
                        checkable += 1;
                    }
                    (apu::patches::Tell::SclkMax(_), snapshot::screen::Tell::Runtime(_)) => {}
                    (apu::patches::Tell::CuCount(_), snapshot::screen::Tell::Runtime(_)) => {}
                    (apu::patches::Tell::Bundled, snapshot::screen::Tell::Bundled) => {}
                    (from, to) => panic!("patch {} crossed as {to:?}, not from {from:?}", patch.id),
                }
            }
        }
        assert!(
            checkable > 0,
            "no patch has a tell a module could be asked about, which would make the \
             popup's whole count meaningless"
        );
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
