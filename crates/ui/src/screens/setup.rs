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
        let option = |text: &'static str, control: gpui::Stateful<gpui::Div>| {
            div()
                .flex()
                .items_center()
                .gap_2()
                .text_sm()
                .child(div().flex_1().child(text))
                .child(control)
        };
        let on_off = |on: bool| if on { "On" } else { "Off" };
        let on_top = platform::SUPPORTED.then(|| {
            option(
                "Keep the compact coaching window on top of the game",
                button("on-top-setup", on_off(self.settings.always_on_top)).on_click(cx.listener(
                    |view, _, window, cx| {
                        view.toggle_pin(window);
                        cx.notify();
                    },
                )),
            )
        });
        let with_iracing = platform::SUPPORTED.then(|| {
            option(
                "Pin it on top automatically while iRacing is running",
                button("pin-with-iracing", on_off(self.settings.pin_with_iracing)).on_click(
                    cx.listener(|view, _, _, cx| {
                        view.toggle_pin_with_iracing();
                        cx.notify();
                    }),
                ),
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
                    .child(
                        panel("Coaching")
                            .children(planning)
                            .children(on_top)
                            .children(with_iracing),
                    ),
            )
    }
}
