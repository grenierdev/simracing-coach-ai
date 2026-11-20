//! Coaching screen: coach state, voice levels, focus corners, reference, live
//! delta, per-corner deltas and cues. The small window kept on top of the game
//! while driving is the [`crate::widget`].

use analysis::CornerComparison;
use gpui::{Context, FontWeight, div, prelude::*, px};
use triggers::TriggerState;

use crate::state::ReferenceState;
use crate::widgets::*;
use crate::{CoachView, ReferenceStatus, format};

impl CoachView {
    pub(crate) fn render_coaching_screen(&self, cx: &mut Context<Self>) -> gpui::Div {
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .gap_2()
            .child(self.render_coach_state())
            .children(self.render_focus(cx))
            .child(self.render_reference_bar(cx))
            .child(self.render_live_strip())
            .child(self.render_compact_corners())
            .child(self.render_compact_cues())
    }

    fn render_coach_state(&self) -> gpui::Div {
        let a = self.coach_activity();
        panel("Coach")
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_1()
                    .child(light("Listening", a.listening, theme::ERR))
                    .child(light("Thinking", a.thinking, theme::WARN))
                    .child(light("Speaking", a.speaking, theme::OK))
                    .child(light("Analyzing", a.analyzing, theme::BEST))
                    .child(light("Idle", a.idle(), theme::MUTED)),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(wave("Coach", &self.coach_wave, theme::OK))
                    .child(wave("Mic", &self.mic_wave, theme::ERR)),
            )
    }

    /// Focus coaching: the corners worked on, the latest attempts, and the
    /// button to move on.
    pub(crate) fn render_focus(&self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let st = self.planner.as_ref()?.status();
        let line = st.focus.line()?;
        let practicing = st.focus.phase == mcp::FocusPhase::Practicing;
        let tone = if practicing {
            theme::TEXT
        } else {
            theme::MUTED
        };
        let attempts = st.focus.attempts.iter().rev().take(6).rev().map(|a| {
            let tone = if a.off_track {
                theme::MUTED
            } else {
                delta_color(a.loss_s)
            };
            div()
                .px_1()
                .rounded_sm()
                .bg(color(theme::BORDER))
                .text_xs()
                .text_color(color(tone))
                .child(format::delta(a.loss_s))
        });
        Some(
            panel("Focus")
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_sm()
                                .whitespace_nowrap()
                                .overflow_hidden()
                                .text_ellipsis()
                                .text_color(color(tone))
                                .child(line),
                        )
                        .when(practicing, |d| {
                            d.child(button("focus-next", "Next").on_click(cx.listener(
                                |view, _, _, cx| {
                                    if let Some(p) = &view.planner {
                                        p.next_focus();
                                    }
                                    cx.notify();
                                },
                            )))
                        }),
                )
                .when(practicing, |d| {
                    d.child(div().flex().flex_wrap().gap_1().children(attempts))
                }),
        )
    }

    /// Reference in one line, with the quick load button.
    fn render_reference_bar(&self, cx: &mut Context<Self>) -> gpui::Div {
        let (text, tone) = match &self.reference {
            ReferenceStatus::None => ("No reference lap".to_string(), theme::MUTED),
            ReferenceStatus::Loading(n) => (format!("Loading {n}…"), theme::WARN),
            ReferenceStatus::Failed(e) => (e.clone(), theme::ERR),
            ReferenceStatus::Ready(r) => (r.reference.label.clone(), theme::TEXT),
        };
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(color(theme::MUTED))
                    .child("REF"),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_color(color(tone))
                    .child(text),
            )
            .child(
                button("quick-reference", "Load lap…")
                    .on_click(cx.listener(|view, _, _, cx| view.pick_reference(cx))),
            )
    }

    fn render_live_strip(&self) -> gpui::Div {
        let s = &self.snap;
        let lap = s
            .sample
            .map(|x| x.lap.to_string())
            .unwrap_or_else(|| "–".into());
        let pct = s
            .sample
            .map(|x| format!("{:.0}%", x.lap_dist_pct * 100.0))
            .unwrap_or_else(|| "–".into());
        let live_delta = self.live_delta();
        let last = self.comparisons.last.as_ref().map(|l| l.total_delta());
        div()
            .flex()
            .items_end()
            .gap_4()
            .px_2()
            .child(big_value("Lap", lap))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_xs()
                            .text_color(color(theme::MUTED))
                            .child("Δ ref"),
                    )
                    .child(
                        div()
                            .text_3xl()
                            .font_weight(FontWeight::BOLD)
                            .text_color(color(live_delta.map(delta_color).unwrap_or(theme::MUTED)))
                            .child(live_delta.map(format::delta).unwrap_or_else(|| "–".into())),
                    ),
            )
            .child(big_value("Track", pct))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_xs()
                            .text_color(color(theme::MUTED))
                            .child("Last lap"),
                    )
                    .child(
                        div()
                            .text_lg()
                            .text_color(color(last.map(delta_color).unwrap_or(theme::MUTED)))
                            .child(last.map(format::delta).unwrap_or_else(|| "–".into())),
                    ),
            )
    }

    /// Per-corner deltas: corner, last lap, best lap, active cue.
    fn render_compact_corners(&self) -> gpui::Div {
        let Some(r) = self.ready_reference() else {
            return panel("Corners").child(
                div()
                    .text_sm()
                    .text_color(color(theme::MUTED))
                    .child("Load a reference lap to compare corner by corner."),
            );
        };
        let header = row(
            vec![
                (44., "".into()),
                (64., "Δ last".into()),
                (64., "Δ best".into()),
                (40., "Cue".into()),
            ],
            theme::MUTED,
        );
        let rows = self.compact_corner_rows(r);
        let summary = self.comparisons.last.as_ref().and_then(|l| {
            l.ranked().first().map(|c| {
                format!(
                    "Costliest last lap: {} ({})",
                    c.label(),
                    format::delta(c.time_delta)
                )
            })
        });
        panel("Corners")
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .children(summary.map(|t| div().text_xs().text_color(color(theme::MUTED)).child(t)))
            .child(header)
            .child(
                div()
                    .id("compact-corners")
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .overflow_y_scroll()
                    .children(rows),
            )
    }

    fn compact_corner_rows(&self, r: &ReferenceState) -> Vec<gpui::Div> {
        let current = self.snap.sample.map(|x| x.lap_dist_pct as f64);
        let last = self.comparisons.last.as_ref();
        let best = self.comparisons.best.as_ref();
        let cell = |c: Option<&CornerComparison>| match c {
            Some(c) => (format::delta(c.time_delta), delta_color(c.time_delta)),
            None => ("–".to_string(), theme::MUTED),
        };
        r.analyzer
            .model
            .zones
            .iter()
            .enumerate()
            .map(|(i, zone)| {
                let label = zone.corner.label();
                let here = current.is_some_and(|p| zone.contains(p));
                let (dl, cl) = cell(last.and_then(|l| l.corners.get(i)));
                let (db, cb) = cell(best.and_then(|b| b.corners.get(i)));
                let cue = self
                    .coach_status
                    .triggers
                    .iter()
                    .find(|v| {
                        v.trigger.corner.as_deref() == Some(label.as_str())
                            && v.state != TriggerState::Expired
                    })
                    .map(|v| v.trigger.text.clone())
                    .unwrap_or_default();
                div()
                    .flex()
                    .gap_2()
                    .py(px(2.))
                    .px_1()
                    .rounded_sm()
                    .text_sm()
                    .when(here, |d| d.bg(color(theme::BORDER)))
                    .child(
                        div()
                            .w(px(44.))
                            .text_color(color(if here { theme::WARN } else { theme::TEXT }))
                            .child(label),
                    )
                    .child(div().w(px(64.)).text_color(color(cl)).child(dl))
                    .child(div().w(px(64.)).text_color(color(cb)).child(db))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_color(color(theme::MUTED))
                            .child(cue),
                    )
            })
            .collect()
    }

    /// Upcoming cues and the last ones spoken.
    fn render_compact_cues(&self) -> gpui::Div {
        let st = &self.coach_status;
        let pending = st
            .triggers
            .iter()
            .filter(|v| matches!(v.state, TriggerState::Waiting | TriggerState::Queued))
            .take(3)
            .map(|v| {
                let place = v
                    .trigger
                    .corner
                    .clone()
                    .unwrap_or_else(|| format!("{:.0}%", v.trigger.at_pct * 100.0));
                let fits = st
                    .fits
                    .iter()
                    .find(|f| f.id == v.trigger.id)
                    .is_none_or(|f| f.fits());
                let tone = if !fits {
                    theme::ERR
                } else if v.state == TriggerState::Queued {
                    theme::WARN
                } else {
                    theme::TEXT
                };
                div()
                    .flex()
                    .gap_2()
                    .text_sm()
                    .child(
                        div()
                            .w(px(44.))
                            .flex_none()
                            .text_color(color(theme::MUTED))
                            .child(place),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_color(color(tone))
                            .child(v.trigger.text.clone()),
                    )
            });
        let spoken = st.log.iter().rev().take(1).map(|e| {
            div()
                .flex()
                .gap_2()
                .text_xs()
                .child(
                    div()
                        .w(px(44.))
                        .flex_none()
                        .text_color(color(theme::MUTED))
                        .child(format!("L{}", e.lap)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_color(color(theme::OK))
                        .child(e.text.clone()),
                )
        });
        let quiet = st.quiet_now.then(|| {
            div()
                .text_xs()
                .text_color(color(theme::WARN))
                .child("Quiet: braking zone")
        });
        let analysis = self.planner.as_ref().and_then(|p| {
            let st = p.status();
            st.last.map(|last| {
                div()
                    .text_xs()
                    .text_color(color(theme::MUTED))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(format!("{}: {}", last.title, last.text))
            })
        });
        panel("Cues")
            .children(quiet)
            .children(pending)
            .child(div().h(px(1.)).bg(color(theme::BORDER)))
            .children(spoken)
            .children(analysis)
    }
}
