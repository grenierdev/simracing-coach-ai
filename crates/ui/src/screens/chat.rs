//! Chat screen: what a chat interface would show. The conversation with the
//! coach, the analysis session's summaries and tool calls, laps and the
//! detailed corner metrics.

use gpui::{Context, Window, div, prelude::*, px};

use crate::CoachView;
use crate::widgets::*;

impl CoachView {
    pub(crate) fn render_chat_screen(&self, window: &Window, cx: &mut Context<Self>) -> gpui::Div {
        let ptt = self
            .settings
            .ptt
            .as_ref()
            .map(|b| format!("Push to talk: {}", b.label()))
            .unwrap_or_else(|| "Push to talk: not set (Setup)".into());
        let latency = self.voice_latency.map(|l| {
            let f = |ms: Option<f64>| {
                ms.map(|v| format!("{:.2} s", v / 1000.0))
                    .unwrap_or_else(|| "…".into())
            };
            format!(
                "Last voice turn: text {} → first word {} → first sound {}",
                f(l.transcript_ms),
                f(l.first_word_ms),
                f(l.first_sound_ms)
            )
        });
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .gap_2()
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .items_center()
                    .child(self.render_agent_pill())
                    .child(self.render_stt_pill())
                    .child(div().text_xs().text_color(color(theme::MUTED)).child(ptt))
                    .children(
                        latency.map(|l| div().text_xs().text_color(color(theme::MUTED)).child(l)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .child(self.render_chat(window, cx)),
                    )
                    .child(
                        div()
                            .w(px(380.))
                            .flex_none()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .min_h_0()
                            .children(
                                self.render_planner(cx)
                                    .map(|p| panel("Analysis session").child(p)),
                            )
                            .child(div().flex_1().min_h_0().flex().child(self.render_laps())),
                    ),
            )
            .child(
                div()
                    .h(px(260.))
                    .flex_none()
                    .flex()
                    .child(self.render_corners()),
            )
    }
}
