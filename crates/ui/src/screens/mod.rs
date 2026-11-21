//! The three screens and the tab bar.

mod chat;
mod coaching;
mod setup;

use gpui::{Context, FontWeight, div, prelude::*, px};

use crate::widgets::*;
use crate::{CoachView, Screen};

impl CoachView {
    pub(crate) fn render_tabs(&self, cx: &mut Context<Self>) -> gpui::Div {
        let tab = |id: &'static str, label: &'static str, screen: Screen| {
            let active = self.screen == screen;
            div()
                .id(id)
                .flex()
                .flex_col()
                .gap(px(3.))
                .px_3()
                .pt_1()
                .cursor_pointer()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::BOLD)
                        .text_color(color(if active { theme::TEXT } else { theme::MUTED }))
                        .child(label.to_uppercase()),
                )
                // Underline in the accent color for the active screen.
                .child(
                    div()
                        .h(px(2.))
                        .bg(color(if active { theme::ACCENT } else { theme::BG })),
                )
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.screen = screen;
                    view.device_menu = None;
                    cx.notify();
                }))
        };
        let brand = div()
            .flex()
            .items_center()
            .gap_1()
            .pr_3()
            .child(div().w(px(3.)).h(px(14.)).bg(color(theme::ACCENT)))
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::EXTRA_BOLD)
                    .text_color(color(theme::ACCENT))
                    .child("SIMCOACH"),
            );
        let widget = button(
            "widget",
            if self.widget_open() {
                "Close widget"
            } else {
                "Widget"
            },
        )
        .on_click(cx.listener(|view, _, _, cx| {
            view.toggle_widget(cx);
            cx.notify();
        }));
        div()
            .flex()
            .items_center()
            .gap_1()
            .pb_1()
            .border_b_1()
            .border_color(color(theme::BORDER))
            .child(brand)
            .child(tab("tab-coaching", "Coaching", Screen::Coaching))
            .child(tab("tab-chat", "Chat", Screen::Chat))
            .child(tab("tab-setup", "Setup", Screen::Setup))
            .child(div().flex_1())
            .child(widget)
    }
}
