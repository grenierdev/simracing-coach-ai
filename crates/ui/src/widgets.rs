//! Shared widgets and colors.

use gpui::{FontWeight, Hsla, SharedString, div, prelude::*, px, rgb};

pub(crate) mod theme {
    pub const BG: u32 = 0x15171c;
    pub const PANEL: u32 = 0x1e2128;
    pub const BORDER: u32 = 0x2c313a;
    pub const TEXT: u32 = 0xe6e8ec;
    pub const MUTED: u32 = 0x8a919e;
    pub const OK: u32 = 0x3fb950;
    pub const WARN: u32 = 0xd29922;
    pub const ERR: u32 = 0xf85149;
    pub const IDLE: u32 = 0x59606b;
    pub const BEST: u32 = 0xbc8cff;
}

pub(crate) fn delta_color(d: f64) -> u32 {
    if !d.is_finite() || d.abs() < 0.0005 {
        theme::TEXT
    } else if d > 0.0 {
        theme::ERR
    } else {
        theme::OK
    }
}

pub(crate) fn color(c: u32) -> Hsla {
    rgb(c).into()
}

pub(crate) fn panel(title: &'static str) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .p_3()
        .bg(color(theme::PANEL))
        .border_1()
        .border_color(color(theme::BORDER))
        .rounded_md()
        .child(
            div()
                .text_xs()
                .text_color(color(theme::MUTED))
                .font_weight(FontWeight::BOLD)
                .child(title.to_uppercase()),
        )
}

pub(crate) fn status_pill(
    label: &'static str,
    state_color: u32,
    detail: impl Into<SharedString>,
) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .px_2()
        .py_1()
        .rounded_md()
        .bg(color(theme::BG))
        .child(div().size(px(8.)).rounded_full().bg(color(state_color)))
        .child(div().text_sm().child(label))
        .child(
            div()
                .text_xs()
                .text_color(color(theme::MUTED))
                .child(detail.into()),
        )
}

pub(crate) fn big_value(label: &'static str, value: impl Into<SharedString>) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .child(div().text_xs().text_color(color(theme::MUTED)).child(label))
        .child(
            div()
                .text_2xl()
                .font_weight(FontWeight::BOLD)
                .child(value.into()),
        )
}

pub(crate) fn row(cells: Vec<(f32, SharedString)>, text: u32) -> gpui::Div {
    div()
        .flex()
        .gap_2()
        .py(px(2.))
        .text_sm()
        .text_color(color(text))
        .children(
            cells
                .into_iter()
                .map(|(w, s)| div().w(px(w)).whitespace_nowrap().child(s)),
        )
}

/// Small clickable button (the caller adds `on_click`).
pub(crate) fn button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_2()
        .py_1()
        .flex_none()
        .rounded_md()
        .bg(color(theme::BORDER))
        .text_xs()
        .cursor_pointer()
        .child(label.into())
}

/// State light: a dot and a label, bright when on.
pub(crate) fn light(label: &'static str, on: bool, on_color: u32) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap_1()
        .px_2()
        .py_1()
        .rounded_md()
        .bg(color(if on { theme::PANEL } else { theme::BG }))
        .child(div().size(px(10.)).rounded_full().bg(color(if on {
            on_color
        } else {
            theme::BORDER
        })))
        .child(
            div()
                .text_xs()
                .text_color(color(if on { theme::TEXT } else { theme::IDLE }))
                .child(label),
        )
}

/// Rolling level meter: one bar per refresh, mirrored around the middle like
/// a waveform envelope.
pub(crate) fn wave(
    label: &'static str,
    levels: &std::collections::VecDeque<f32>,
    tone: u32,
) -> gpui::Div {
    const H: f32 = 28.;
    let bars = levels.iter().map(move |v| {
        // Speech RMS rarely exceeds 0.3: stretch it so it reads well.
        let h = ((v * 3.0).sqrt().min(1.0) * H).max(1.);
        div()
            .flex_1()
            .min_w(px(1.))
            .max_w(px(6.))
            .h(px(h))
            .rounded_sm()
            .bg(color(if *v > 0.003 { tone } else { theme::BORDER }))
    });
    div()
        .flex()
        .flex_1()
        .min_w_0()
        .items_center()
        .gap_2()
        .child(
            div()
                .w(px(40.))
                .flex_none()
                .text_xs()
                .text_color(color(theme::MUTED))
                .child(label),
        )
        .child(
            div()
                .flex()
                .flex_1()
                .min_w_0()
                .h(px(H))
                .items_center()
                .gap(px(1.))
                .overflow_hidden()
                .children(bars),
        )
}
