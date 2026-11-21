//! Coaching screen, kept to what matters: the coach state with the reference,
//! lap and live delta; every turn with its latest pass, its best and its cue;
//! the next cue and the last sentence spoken. The small window kept on top of
//! the game while driving is the [`crate::widget`].

use gpui::{Context, FontWeight, SharedString, div, prelude::*, px};
use triggers::TriggerState;

use crate::widget::state_of;
use crate::widgets::*;
use crate::{CoachView, ReferenceStatus, format};

/// One turn row of the table.
struct TurnRow {
    label: String,
    last: Option<f64>,
    best: Option<f64>,
    cue: Option<(String, bool)>,
    queued: bool,
    analyzing: bool,
    here: bool,
}

impl CoachView {
    pub(crate) fn render_coaching_screen(&self, cx: &mut Context<Self>) -> gpui::Div {
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .gap_2()
            .child(self.render_header(cx))
            .child(self.render_turns())
            .child(self.render_cue_lines())
    }

    /// State, reference, lap and live delta, in one strip.
    fn render_header(&self, cx: &mut Context<Self>) -> gpui::Div {
        let (state, tone) = state_of(&self.coach_activity());
        let (reference, ref_tone) = match &self.reference {
            ReferenceStatus::None => ("No reference lap".to_string(), theme::MUTED),
            ReferenceStatus::Loading(n) => (format!("Loading {n}…"), theme::WARN),
            ReferenceStatus::Failed(e) => (e.clone(), theme::ERR),
            ReferenceStatus::Ready(r) => (r.reference.label.clone(), theme::TEXT),
        };
        let lap = self
            .snap
            .sample
            .map(|x| x.lap.to_string())
            .unwrap_or_else(|| "–".into());
        let delta = self.live_delta();
        let sep = || div().w(px(1.)).h(px(18.)).bg(color(theme::BORDER));
        let stat = |name: &'static str, value: String, tone: u32, big: bool| {
            div()
                .flex()
                .items_baseline()
                .gap_1()
                .flex_none()
                .child(label(name))
                .child(
                    div()
                        .when(big, |d| d.text_2xl())
                        .when(!big, |d| d.text_lg())
                        .font_weight(FontWeight::BOLD)
                        .text_color(color(tone))
                        .child(value),
                )
        };
        div()
            .flex()
            .items_center()
            .gap_3()
            .px_3()
            .py_2()
            .bg(color(theme::PANEL))
            .border_1()
            .border_color(color(theme::BORDER))
            .child(state_chip(state, tone))
            .child(sep())
            .child(label("REF"))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_color(color(ref_tone))
                    .child(reference),
            )
            .child(
                button("quick-reference", "Load lap")
                    .on_click(cx.listener(|view, _, _, cx| view.pick_reference(cx))),
            )
            .child(sep())
            .child(stat("LAP", lap, theme::TEXT, false))
            .child(stat(
                "Δ",
                delta.map(format::delta).unwrap_or_else(|| "–".into()),
                delta.map(delta_color).unwrap_or(theme::MUTED),
                true,
            ))
    }

    fn turn_rows(&self) -> Vec<TurnRow> {
        let Some(r) = self.ready_reference() else {
            return vec![];
        };
        let coaching = self.planner.as_ref().map(|p| p.status().coaching);
        let current = self.snap.sample.map(|x| x.lap_dist_pct as f64);
        let st = &self.coach_status;
        r.analyzer
            .model
            .zones
            .iter()
            .enumerate()
            .map(|(i, zone)| {
                let label = zone.corner.label();
                // Per-turn passes when the race engineer runs, laps otherwise.
                let (last, best, queued, analyzing) =
                    match coaching.as_ref().and_then(|c| c.turns.get(i)) {
                        Some(t) => (t.last_s, t.best_s, t.queued, t.analyzing),
                        None => (
                            self.comparisons
                                .last
                                .as_ref()
                                .and_then(|l| l.corners.get(i))
                                .map(|c| c.time_delta),
                            self.comparisons
                                .best
                                .as_ref()
                                .and_then(|l| l.corners.get(i))
                                .map(|c| c.time_delta),
                            false,
                            false,
                        ),
                    };
                let cue = st
                    .triggers
                    .iter()
                    .find(|v| {
                        v.trigger.corner.as_deref() == Some(label.as_str())
                            && v.state != TriggerState::Expired
                    })
                    .map(|v| {
                        let fits = st
                            .fits
                            .iter()
                            .find(|f| f.id == v.trigger.id)
                            .is_none_or(|f| f.fits());
                        (v.trigger.text.clone(), fits)
                    });
                TurnRow {
                    label,
                    last,
                    best,
                    cue,
                    queued,
                    analyzing,
                    here: current.is_some_and(|p| zone.contains(p)),
                }
            })
            .collect()
    }

    /// Every turn: latest pass, best, cue, and whether the race engineer has
    /// it queued or is looking at it.
    fn render_turns(&self) -> gpui::Div {
        let rows = self.turn_rows();
        if rows.is_empty() {
            return panel("Turns").flex_1().child(
                div()
                    .text_sm()
                    .text_color(color(theme::MUTED))
                    .child("Load a reference lap: every turn is then compared after each pass."),
            );
        }
        let cell = |v: Option<f64>, tone: fn(f64) -> u32| match v {
            Some(d) => (format::delta(d), tone(d)),
            None => ("–".to_string(), theme::IDLE),
        };
        let header = div()
            .flex()
            .gap_3()
            .pb_1()
            .border_b_1()
            .border_color(color(theme::BORDER))
            .child(label("TURN").w(px(44.)))
            .child(label("LAST").w(px(64.)))
            .child(label("BEST").w(px(64.)))
            .child(label("CUE").flex_1());
        let body = rows.into_iter().map(|t| {
            let (last, last_tone) = cell(t.last, delta_color);
            let (best, best_tone) = cell(t.best, |_| theme::BEST);
            let (cue, cue_tone) = match t.cue {
                Some((text, true)) => (text, theme::TEXT),
                Some((text, false)) => (text, theme::ERR),
                None => ("–".into(), theme::IDLE),
            };
            let (mark, mark_tone): (SharedString, u32) = if t.analyzing {
                ("◆".into(), theme::ACCENT)
            } else if t.queued {
                ("◇".into(), theme::MUTED)
            } else {
                ("".into(), theme::MUTED)
            };
            div()
                .flex()
                .items_center()
                .gap_3()
                .py(px(3.))
                .text_sm()
                .when(t.here, |d| d.bg(color(theme::BORDER)))
                .child(
                    div()
                        .w(px(44.))
                        .pl_1()
                        .font_weight(FontWeight::BOLD)
                        .text_color(color(if t.here { theme::ACCENT } else { theme::TEXT }))
                        .child(t.label),
                )
                .child(div().w(px(64.)).text_color(color(last_tone)).child(last))
                .child(div().w(px(64.)).text_color(color(best_tone)).child(best))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_color(color(cue_tone))
                        .child(cue),
                )
                .child(
                    div()
                        .w(px(16.))
                        .flex_none()
                        .text_color(color(mark_tone))
                        .child(mark),
                )
        });
        panel("Turns")
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .child(header)
            .child(
                div()
                    .id("turns")
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .overflow_y_scroll()
                    .children(body),
            )
    }

    /// The next cue: one waiting to be spoken first, then the nearest ahead.
    pub(crate) fn next_cue(&self) -> Option<(String, u32)> {
        let st = &self.coach_status;
        let pct = self.snap.sample.map(|s| s.lap_dist_pct as f64);
        st.triggers
            .iter()
            .filter(|t| matches!(t.state, TriggerState::Waiting | TriggerState::Queued))
            .min_by(|x, y| {
                let ahead = |t: &triggers::TriggerView| match (pct, t.trigger.at_pct) {
                    (Some(p), at) if at.is_finite() => (at - p).rem_euclid(1.0),
                    _ => 0.0,
                };
                (x.state != TriggerState::Queued)
                    .cmp(&(y.state != TriggerState::Queued))
                    .then(ahead(x).total_cmp(&ahead(y)))
            })
            .map(|t| {
                let fits = st
                    .fits
                    .iter()
                    .find(|f| f.id == t.trigger.id)
                    .is_none_or(|f| f.fits());
                let place = t.trigger.corner.clone().unwrap_or_default();
                let tone = if !fits {
                    theme::ERR
                } else if t.state == TriggerState::Queued {
                    theme::WARN
                } else {
                    theme::TEXT
                };
                (
                    format!("{place}  {}", t.trigger.text).trim().to_string(),
                    tone,
                )
            })
    }

    pub(crate) fn last_said(&self) -> Option<String> {
        self.coach_status.log.last().map(|e| e.text.clone())
    }

    fn render_cue_lines(&self) -> gpui::Div {
        let line = |name: &'static str, text: String, tone: u32| {
            div()
                .flex()
                .items_center()
                .gap_3()
                .min_w_0()
                .child(label(name).w(px(64.)).flex_none())
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
        };
        let (next, next_tone) = self.next_cue().unwrap_or_else(|| ("–".into(), theme::IDLE));
        let engineer = self.planner.as_ref().map(|p| {
            let st = p.status();
            let (text, tone) = if !st.enabled {
                ("off".to_string(), theme::IDLE)
            } else if let Some(what) = st.running {
                (format!("analyzing {what}…"), theme::ACCENT)
            } else if let Some(e) = st.error {
                (e, theme::ERR)
            } else if let Some(last) = st.last {
                (format!("{}: {}", last.title, last.text), theme::MUTED)
            } else {
                ("each turn is analyzed after every pass".into(), theme::IDLE)
            };
            line("ENGINEER", text, tone)
        });
        div()
            .flex()
            .flex_col()
            .gap_1()
            .px_3()
            .py_2()
            .bg(color(theme::PANEL))
            .border_1()
            .border_color(color(theme::BORDER))
            .child(line("NEXT", next, next_tone))
            .child(line(
                "SAID",
                self.last_said().unwrap_or_else(|| "–".into()),
                theme::OK,
            ))
            .children(engineer)
    }
}
