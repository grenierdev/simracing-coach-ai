//! Setup screen: session, reference, plan, voice and devices, coaching
//! options, connections.

use gpui::{Context, div, prelude::*, px};

use crate::widgets::*;
use crate::{CoachView, platform};

impl CoachView {
    pub(crate) fn render_setup_screen(&self, cx: &mut Context<Self>) -> gpui::Div {
        let planning = self.planner.as_ref().map(|p| {
            let st = p.status();
            div()
                .flex()
                .items_center()
                .gap_2()
                .text_sm()
                .child(div().flex_1().child(format!(
                    "Plan the next lap after each valid lap ({})",
                    st.model
                )))
                .child(
                    button("auto-planning-setup", if st.enabled { "On" } else { "Off" }).on_click(
                        cx.listener(|view, _, _, cx| {
                            view.toggle_auto_planning();
                            cx.notify();
                        }),
                    ),
                )
        });
        let on_top = platform::SUPPORTED.then(|| {
            let on = self.settings.always_on_top;
            div()
                .flex()
                .items_center()
                .gap_2()
                .text_sm()
                .child(
                    div()
                        .flex_1()
                        .child("Keep the compact coaching window on top of the game"),
                )
                .child(
                    button("on-top-setup", if on { "On" } else { "Off" }).on_click(cx.listener(
                        move |view, _, window, cx| {
                            view.set_on_top(!on, window);
                            cx.notify();
                        },
                    )),
                )
        });
        div()
            .flex()
            .flex_1()
            .min_h_0()
            .gap_2()
            .child(
                div()
                    .w(px(440.))
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(self.render_session(cx))
                    .child(panel("Connections").child(self.render_status())),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(panel("Voice").child(self.render_voice_settings(cx)))
                    .child(panel("Coaching").children(planning).children(on_top)),
            )
    }
}
