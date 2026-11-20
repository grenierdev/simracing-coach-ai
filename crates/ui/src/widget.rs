//! Coaching widget: a small window with no title bar, border, shadow or
//! taskbar entry, kept above the game. Four lines, what matters while
//! driving: the coach state (one at a time) with the focus corners, the time
//! still to gain, the next cue and the last sentence spoken. Drag it anywhere
//! on its surface to move it, and by the grip in the bottom-right corner to
//! resize it; it reopens where and as it was left.
//!
//! On Windows, `WindowKind::PopUp` makes it a topmost tool window; its style
//! is then reduced to a plain popup so that the compositor draws no frame or
//! shadow. It works over a borderless or windowed game, not over exclusive
//! fullscreen.

use gpui::{
    App, Bounds, Context, CursorStyle, DragMoveEvent, Entity, FontWeight, IntoElement, MouseButton,
    MouseDownEvent, Pixels, Point, Render, Size, Subscription, Window, WindowBounds,
    WindowControlArea, WindowHandle, WindowKind, WindowOptions, div, point, prelude::*, px, size,
};
use mcp::{FocusPhase, FocusStatus};
use triggers::TriggerState;

use crate::widgets::{color, theme};
use crate::{Activity, CoachView};

/// Default size, px: four lines.
pub const SIZE: (f32, f32) = (380., 92.);
/// Smallest size the grip allows, px.
const MIN: (f32, f32) = (220., 64.);

pub struct WidgetView {
    coach: Entity<CoachView>,
    /// Resize in progress: mouse position and window size when it started.
    resize_from: Option<(Point<Pixels>, Size<Pixels>)>,
    _observe: Subscription,
}

/// Drag value of the resize grip.
struct Grip;

/// What follows the mouse while resizing: nothing visible.
struct GripPreview;

impl Render for GripPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

impl WidgetView {
    fn new(coach: Entity<CoachView>, cx: &mut Context<Self>) -> Self {
        // The main view refreshes at ~30 Hz: redraw with it.
        let observe = cx.observe(&coach, |_, _, cx| cx.notify());
        WidgetView {
            coach,
            resize_from: None,
            _observe: observe,
        }
    }
}

/// Opens the widget at `origin` (top-left corner, px), or at the top center
/// of the primary display, with `size` or the default size.
pub(crate) fn open(
    coach: Entity<CoachView>,
    origin: Option<(f32, f32)>,
    saved_size: Option<(f32, f32)>,
    cx: &mut App,
) -> Result<WindowHandle<WidgetView>, String> {
    let (w, h) = saved_size.unwrap_or(SIZE);
    let window_size = size(px(w.max(MIN.0)), px(h.max(MIN.1)));
    let origin = match origin {
        Some((x, y)) => point(px(x), px(y)),
        None => default_origin(cx, window_size.width),
    };
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(origin, window_size))),
            titlebar: None,
            focus: false,
            show: true,
            kind: WindowKind::PopUp,
            is_movable: true,
            // Resized by its own grip: the platform resize border would take
            // the top edge away from the drag area.
            is_resizable: false,
            is_minimizable: false,
            ..Default::default()
        },
        |window, cx| {
            if let Err(e) = platform::plain_popup(window) {
                tracing::warn!("widget frame: {e}");
            }
            cx.new(|cx| WidgetView::new(coach, cx))
        },
    )
    .map_err(|e| e.to_string())
}

fn default_origin(cx: &App, width: Pixels) -> Point<Pixels> {
    match cx.primary_display() {
        Some(d) => {
            let b = d.bounds();
            point(
                b.origin.x + (b.size.width - width) / 2.0,
                b.origin.y + px(24.),
            )
        }
        None => point(px(100.), px(100.)),
    }
}

/// The one state to show: the driver speaking wins, then the coach speaking,
/// thinking, analyzing.
pub(crate) fn state_of(a: &Activity) -> (&'static str, u32) {
    if a.listening {
        ("Listening", theme::ERR)
    } else if a.speaking {
        ("Speaking", theme::OK)
    } else if a.thinking {
        ("Thinking", theme::WARN)
    } else if a.analyzing {
        ("Analyzing", theme::BEST)
    } else {
        ("Idle", theme::MUTED)
    }
}

/// Focus corners and stage, short: "T5, T6 · 3 tries".
pub(crate) fn focus_short(f: &FocusStatus) -> String {
    match f.phase {
        FocusPhase::Off => String::new(),
        FocusPhase::WaitingForLap => "waiting for a clean lap".into(),
        FocusPhase::Choosing => "choosing corners".into(),
        FocusPhase::Practicing => match f.attempts.len() {
            0 => f.corners.join(", "),
            1 => format!("{} · 1 try", f.corners.join(", ")),
            n => format!("{} · {n} tries", f.corners.join(", ")),
        },
    }
}

/// Time still to gain over the focus corners: the last clean attempt, or the
/// starting time before the first one.
pub(crate) fn focus_gain(f: &FocusStatus) -> Option<String> {
    if f.phase != FocusPhase::Practicing {
        return None;
    }
    let now = f
        .attempts
        .iter()
        .rev()
        .find(|a| !a.off_track)
        .map(|a| a.loss_s)
        .or(f.baseline_s)?;
    let corners = f.corners.join(", ");
    if now <= 0.0 {
        return Some(format!("{corners}: gap closed"));
    }
    let best = f
        .best_s()
        .filter(|b| (b - now).abs() > 0.005)
        .map(|b| format!(" (best {:.2})", b.max(0.0)))
        .unwrap_or_default();
    Some(format!("{now:.2} s on {corners}{best}"))
}

/// The corners where the most time is lost, from a lap comparison:
/// "T5 0.31 · T2 0.22 · T9 0.15".
pub(crate) fn top_gains(cmp: &analysis::LapComparison, n: usize) -> Option<String> {
    let parts: Vec<String> = cmp
        .ranked()
        .into_iter()
        .filter(|c| c.time_delta >= 0.05)
        .take(n)
        .map(|c| format!("{} {:.2}", c.label(), c.time_delta))
        .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn line(label: &'static str, text: String, tone: u32, bold: bool) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .min_w_0()
        .child(
            div()
                .w(px(34.))
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
                .when(bold, |d| d.font_weight(FontWeight::BOLD))
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
        let (state, state_tone) = state_of(&v.coach_activity());
        let focus = v.planner.as_ref().map(|p| p.status().focus);
        let stage = focus.as_ref().map(focus_short).unwrap_or_default();
        let gain = focus
            .as_ref()
            .and_then(focus_gain)
            .or_else(|| {
                v.comparisons
                    .last
                    .as_deref()
                    .or(v.comparisons.best.as_deref())
                    .and_then(|c| top_gains(c, 3))
            })
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

        let grip = div()
            .id("widget-grip")
            .absolute()
            .right_0()
            .bottom_0()
            .size(px(16.))
            .p(px(3.))
            .flex()
            .items_end()
            .justify_end()
            // In front of the drag area: a plain client area that resizes.
            .occlude()
            .cursor(CursorStyle::ResizeUpLeftDownRight)
            .child(
                div()
                    .size(px(8.))
                    .border_r_2()
                    .border_b_2()
                    .border_color(color(theme::IDLE)),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &MouseDownEvent, window, _| {
                    this.resize_from = Some((e.position, window.bounds().size));
                }),
            )
            .on_drag(Grip, |_, _, _, cx| cx.new(|_| GripPreview));

        div()
            .id("widget")
            .size_full()
            .relative()
            .bg(color(theme::BG))
            .text_color(color(theme::TEXT))
            .on_drag_move(cx.listener(|this, e: &DragMoveEvent<Grip>, window, cx| {
                let Some((from, start)) = this.resize_from else {
                    return;
                };
                let p = e.event.position;
                let w = (start.width + (p.x - from.x)).max(px(MIN.0));
                let h = (start.height + (p.y - from.y)).max(px(MIN.1));
                window.resize(size(w, h));
                cx.notify();
            }))
            .child(
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .gap(px(2.))
                    .px_2()
                    .py_1()
                    // The whole surface moves the window (no title bar).
                    .window_control_area(WindowControlArea::Drag)
                    .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .min_w_0()
                            .child(
                                div()
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(div().size(px(9.)).rounded_full().bg(color(state_tone)))
                                    .child(
                                        div().text_sm().text_color(color(state_tone)).child(state),
                                    ),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_xs()
                                    .text_right()
                                    .whitespace_nowrap()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .text_color(color(theme::MUTED))
                                    .child(stage),
                            ),
                    )
                    .child(line("Gain", gain, theme::BEST, true))
                    .child(match next {
                        Some((text, tone)) => line("Next", text, tone, false),
                        None => line("Next", "–".into(), theme::MUTED, false),
                    })
                    .child(line(
                        "Said",
                        said.unwrap_or_else(|| "–".into()),
                        theme::OK,
                        false,
                    )),
            )
            .child(grip)
    }
}

/// Window details GPUI does not expose.
mod platform {
    use gpui::Window;

    /// Turns the popup into a plain `WS_POPUP` window: GPUI creates it with
    /// style 0 (`WS_OVERLAPPED`), for which the compositor draws a frame
    /// shadow; rounded corners (Windows 11) are turned off as well.
    #[cfg(windows)]
    pub fn plain_popup(window: &Window) -> Result<(), String> {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        use windows::Win32::Foundation::HWND;
        use windows::Win32::Graphics::Dwm::{
            DWMNCRP_DISABLED, DWMWA_NCRENDERING_POLICY, DWMWA_WINDOW_CORNER_PREFERENCE,
            DWMWCP_DONOTROUND, DwmSetWindowAttribute,
        };
        use windows::Win32::UI::WindowsAndMessaging::{
            GWL_STYLE, GetWindowLongPtrW, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
            SWP_NOZORDER, SetWindowLongPtrW, SetWindowPos, WS_CAPTION, WS_MAXIMIZEBOX,
            WS_MINIMIZEBOX, WS_POPUP, WS_SYSMENU, WS_THICKFRAME,
        };

        let handle = window.window_handle().map_err(|e| e.to_string())?;
        let RawWindowHandle::Win32(h) = handle.as_raw() else {
            return Err("not a Win32 window".into());
        };
        let hwnd = HWND(h.hwnd.get() as *mut core::ffi::c_void);
        let frame = (WS_CAPTION | WS_THICKFRAME | WS_SYSMENU | WS_MAXIMIZEBOX | WS_MINIMIZEBOX).0;
        // SAFETY: the handle comes from the live window, used on its thread;
        // the attribute values are plain integers that outlive the calls.
        unsafe {
            let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
            SetWindowLongPtrW(
                hwnd,
                GWL_STYLE,
                (style & !(frame as isize)) | WS_POPUP.0 as isize,
            );
            let policy = DWMNCRP_DISABLED;
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_NCRENDERING_POLICY,
                &policy as *const _ as *const core::ffi::c_void,
                std::mem::size_of_val(&policy) as u32,
            )
            .map_err(|e| e.to_string())?;
            // Windows 11 only: an error on Windows 10 is expected.
            let corner = DWMWCP_DONOTROUND;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &corner as *const _ as *const core::ffi::c_void,
                std::mem::size_of_val(&corner) as u32,
            );
            SetWindowPos(
                hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    #[cfg(not(windows))]
    pub fn plain_popup(_window: &Window) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp::FocusAttempt;

    fn attempt(n: u32, loss: f64, off: bool) -> FocusAttempt {
        FocusAttempt {
            number: n,
            lap: 3,
            loss_s: loss,
            corners: vec![],
            off_track: off,
        }
    }

    #[test]
    fn one_state_at_a_time() {
        let mut a = Activity {
            listening: true,
            thinking: true,
            speaking: true,
            analyzing: true,
        };
        assert_eq!(state_of(&a).0, "Listening");
        a.listening = false;
        assert_eq!(state_of(&a).0, "Speaking");
        a.speaking = false;
        assert_eq!(state_of(&a).0, "Thinking");
        a.thinking = false;
        assert_eq!(state_of(&a).0, "Analyzing");
        a.analyzing = false;
        assert_eq!(state_of(&a).0, "Idle");
    }

    #[test]
    fn gain_follows_the_last_clean_attempt() {
        let mut f = FocusStatus {
            phase: FocusPhase::Practicing,
            corners: vec!["T5".into(), "T6".into()],
            baseline_s: Some(0.30),
            ..Default::default()
        };
        assert_eq!(focus_gain(&f).unwrap(), "0.30 s on T5, T6");
        assert_eq!(focus_short(&f), "T5, T6");
        f.attempts = vec![attempt(1, 0.15, false), attempt(2, 0.21, false)];
        assert_eq!(focus_gain(&f).unwrap(), "0.21 s on T5, T6 (best 0.15)");
        assert_eq!(focus_short(&f), "T5, T6 · 2 tries");
        // Off track: the last clean attempt counts.
        f.attempts.push(attempt(3, 0.9, true));
        assert_eq!(focus_gain(&f).unwrap(), "0.21 s on T5, T6 (best 0.15)");
        f.attempts.push(attempt(4, -0.02, false));
        assert_eq!(focus_gain(&f).unwrap(), "T5, T6: gap closed");
        f.phase = FocusPhase::Choosing;
        assert_eq!(focus_gain(&f), None);
    }
}
