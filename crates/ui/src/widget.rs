//! Coaching widget: a small window with no title bar, border or taskbar entry,
//! kept above the game. It shows what matters while driving and nothing else:
//! the coach state, the live delta, the focus corners, the next cue and the
//! last sentence spoken. Drag it anywhere on its surface to move it; it opens
//! where it was last left.
//!
//! On Windows, `WindowKind::PopUp` makes it a topmost tool window. It works
//! over a borderless or windowed game, not over exclusive fullscreen.

use gpui::{
    App, Bounds, Context, Entity, FontWeight, IntoElement, MouseButton, Pixels, Point, Render,
    Subscription, Window, WindowBounds, WindowControlArea, WindowHandle, WindowKind, WindowOptions,
    div, point, prelude::*, px, size,
};
use triggers::TriggerState;

use crate::CoachView;
use crate::format;
use crate::widgets::{color, delta_color, theme};

/// Widget size, px.
pub const SIZE: (f32, f32) = (420., 200.);

pub struct WidgetView {
    coach: Entity<CoachView>,
    _observe: Subscription,
}

impl WidgetView {
    fn new(coach: Entity<CoachView>, cx: &mut Context<Self>) -> Self {
        // The main view refreshes at ~30 Hz: redraw with it.
        let observe = cx.observe(&coach, |_, _, cx| cx.notify());
        WidgetView {
            coach,
            _observe: observe,
        }
    }
}

/// Opens the widget at `origin` (top-left corner, px), or at the top center
/// of the primary display.
pub(crate) fn open(
    coach: Entity<CoachView>,
    origin: Option<(f32, f32)>,
    cx: &mut App,
) -> Result<WindowHandle<WidgetView>, String> {
    let origin = match origin {
        Some((x, y)) => point(px(x), px(y)),
        None => default_origin(cx),
    };
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                origin,
                size(px(SIZE.0), px(SIZE.1)),
            ))),
            titlebar: None,
            focus: false,
            show: true,
            kind: WindowKind::PopUp,
            is_movable: true,
            is_resizable: false,
            is_minimizable: false,
            ..Default::default()
        },
        |_, cx| cx.new(|cx| WidgetView::new(coach, cx)),
    )
    .map_err(|e| e.to_string())
}

fn default_origin(cx: &App) -> Point<Pixels> {
    match cx.primary_display() {
        Some(d) => {
            let b = d.bounds();
            point(
                b.origin.x + (b.size.width - px(SIZE.0)) / 2.0,
                b.origin.y + px(24.),
            )
        }
        None => point(px(100.), px(100.)),
    }
}

/// Coach state dot with a short label.
fn dot(label: &'static str, on: bool, on_color: u32) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap_1()
        .child(div().size(px(9.)).rounded_full().bg(color(if on {
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

/// One line: a short muted label, then the text.
fn line(label: &'static str, text: String, tone: u32) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .w(px(42.))
                .flex_none()
                .text_xs()
                .text_color(color(theme::MUTED))
                .child(label),
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
}

impl Render for WidgetView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let v = self.coach.read(cx);
        let a = v.coach_activity();
        let delta = v.live_delta();
        let focus = v
            .planner
            .as_ref()
            .and_then(|p| p.status().focus.line())
            .unwrap_or_else(|| "–".into());
        let st = &v.coach_status;
        let pct = v.snap.sample.map(|s| s.lap_dist_pct as f64);
        // Next cue: one waiting to be spoken first, then the nearest ahead.
        let next = st
            .triggers
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
            });
        let said = st.log.last().map(|e| e.text.clone());

        div()
            .size_full()
            .flex()
            .flex_col()
            .justify_between()
            .p_3()
            .bg(color(theme::BG))
            .text_color(color(theme::TEXT))
            // The whole surface moves the window (no title bar to grab).
            .window_control_area(WindowControlArea::Drag)
            .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .gap_3()
                            .child(dot("Listen", a.listening, theme::ERR))
                            .child(dot("Think", a.thinking, theme::WARN))
                            .child(dot("Speak", a.speaking, theme::OK))
                            .child(dot("Analyze", a.analyzing, theme::BEST)),
                    )
                    .child(
                        div()
                            .text_2xl()
                            .font_weight(FontWeight::BOLD)
                            .text_color(color(delta.map(delta_color).unwrap_or(theme::MUTED)))
                            .child(delta.map(format::delta).unwrap_or_else(|| "–".into())),
                    ),
            )
            .child(line("Focus", focus, theme::TEXT))
            .child(match next {
                Some((text, tone)) => line("Next", text, tone),
                None => line("Next", "–".into(), theme::MUTED),
            })
            .child(line("Said", said.unwrap_or_else(|| "–".into()), theme::OK))
    }
}
