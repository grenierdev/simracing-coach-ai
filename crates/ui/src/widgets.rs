//! Shared widgets and colors: a dark, racing-telemetry look (near-black
//! background, neon accents, square corners, condensed type).

use gpui::{FontWeight, Hsla, SharedString, div, prelude::*, px, rgb};

pub(crate) mod theme {
    pub const BG: u32 = 0x07090d;
    pub const PANEL: u32 = 0x0d1219;
    pub const BORDER: u32 = 0x1b2532;
    pub const TEXT: u32 = 0xe8f4ff;
    pub const MUTED: u32 = 0x6c7f94;
    pub const IDLE: u32 = 0x37445a;
    /// Neon cyan: the accent (active, selected, highlights).
    pub const ACCENT: u32 = 0x00e5ff;
    pub const OK: u32 = 0x2bff88;
    pub const WARN: u32 = 0xffb020;
    pub const ERR: u32 = 0xff3355;
    /// Time to gain, best values.
    pub const BEST: u32 = 0xd16bff;

    /// Condensed, DIN-like face shipped with Windows 10 and 11; elsewhere the
    /// system interface font.
    pub const FONT: &str = if cfg!(windows) {
        "Bahnschrift"
    } else {
        ".SystemUIFont"
    };
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

/// Small uppercase label.
pub(crate) fn label(text: impl Into<SharedString>) -> gpui::Div {
    div()
        .text_xs()
        .font_weight(FontWeight::BOLD)
        .text_color(color(theme::MUTED))
        .child(text.into())
}

/// Section: square, thin border, an accent bar on the left and an uppercase
/// title.
pub(crate) fn panel(title: &'static str) -> gpui::Div {
    div()
        .relative()
        .flex()
        .flex_col()
        .gap_2()
        .py_2()
        .pl_3()
        .pr_2()
        .bg(color(theme::PANEL))
        .border_1()
        .border_color(color(theme::BORDER))
        .child(
            div()
                .absolute()
                .left_0()
                .top_0()
                .bottom_0()
                .w(px(2.))
                .bg(color(theme::ACCENT)),
        )
        .child(label(title.to_uppercase()))
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
        .bg(color(theme::BG))
        .border_l_2()
        .border_color(color(state_color))
        .child(div().text_sm().child(label))
        .child(
            div()
                .text_xs()
                .text_color(color(theme::MUTED))
                .child(detail.into()),
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

/// Small clickable button (the caller adds `on_click`): square, outlined,
/// uppercase.
pub(crate) fn button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
) -> gpui::Stateful<gpui::Div> {
    let label: SharedString = label.into();
    div()
        .id(id)
        .px_2()
        .py_1()
        .flex_none()
        .border_1()
        .border_color(color(theme::BORDER))
        .bg(color(theme::BG))
        .text_xs()
        .font_weight(FontWeight::BOLD)
        .text_color(color(theme::ACCENT))
        .cursor_pointer()
        .hover(|s| s.border_color(color(theme::ACCENT)))
        .child(label.to_uppercase())
}

/// The coach state, one at a time: a colored bar and the name.
pub(crate) fn state_chip(name: &'static str, tone: u32) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .flex_none()
        .child(div().w(px(4.)).h(px(16.)).bg(color(tone)))
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::BOLD)
                .text_color(color(tone))
                .child(name.to_uppercase()),
        )
}

/// Rolling level meter: one bar per refresh, mirrored around the middle like
/// a waveform envelope.
pub(crate) fn wave(
    label: &'static str,
    levels: &std::collections::VecDeque<f32>,
    tone: u32,
) -> gpui::Div {
    const H: f32 = 20.;
    let bars = levels.iter().map(move |v| {
        // Speech RMS rarely exceeds 0.3: stretch it so it reads well.
        let h = ((v * 3.0).sqrt().min(1.0) * H).max(1.);
        div()
            .flex_1()
            .min_w(px(1.))
            .max_w(px(5.))
            .h(px(h))
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
                .w(px(44.))
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
