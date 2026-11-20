//! Setup screen: session, reference, plan, voice and devices, coaching
//! options, connections.

use gpui::{Context, div, prelude::*, px};

use crate::CoachView;
use crate::widgets::*;

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
                    "Focus coaching: work on one or two corners at a time, each pass analyzed ({})",
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
        let on_top = Some(option(
            "Coaching widget on top of the game",
            button("widget-setup", on_off(self.widget_open())).on_click(cx.listener(
                |view, _, _, cx| {
                    view.toggle_widget(cx);
                    cx.notify();
                },
            )),
        ));
        let with_iracing = Some(option(
            "Open the widget automatically while iRacing is running",
            button(
                "widget-with-iracing",
                on_off(self.settings.widget_with_iracing),
            )
            .on_click(cx.listener(|view, _, _, cx| {
                view.toggle_widget_with_iracing();
                cx.notify();
            })),
        ));
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
