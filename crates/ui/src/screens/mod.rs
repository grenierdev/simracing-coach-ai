//! The three screens and the tab bar.

mod chat;
mod coaching;
mod setup;

use gpui::{Context, div, prelude::*};

use crate::widgets::*;
use crate::{CoachView, Screen, platform};

impl CoachView {
    pub(crate) fn render_tabs(&self, cx: &mut Context<Self>) -> gpui::Div {
        let tab = |id: &'static str, label: &'static str, screen: Screen| {
            let active = self.screen == screen;
            div()
                .id(id)
                .px_3()
                .py_1()
                .rounded_md()
                .text_sm()
                .cursor_pointer()
                .bg(color(if active { theme::PANEL } else { theme::BG }))
                .text_color(color(if active { theme::TEXT } else { theme::MUTED }))
                .child(label)
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.screen = screen;
                    view.device_menu = None;
                    cx.notify();
                }))
        };
        let pin = platform::SUPPORTED.then(|| {
            let on = self.settings.always_on_top;
            button("pin", if on { "Unpin" } else { "Pin on top" }).on_click(cx.listener(
                move |view, _, window, cx| {
                    view.set_on_top(!on, window);
                    cx.notify();
                },
            ))
        });
        div()
            .flex()
            .items_center()
            .gap_1()
            .child(tab("tab-coaching", "Coaching", Screen::Coaching))
            .child(tab("tab-chat", "Chat", Screen::Chat))
            .child(tab("tab-setup", "Setup", Screen::Setup))
            .child(div().flex_1())
            .children(pin)
    }
}
