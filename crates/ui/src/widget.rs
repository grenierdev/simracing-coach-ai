//! Coaching widget: a small window with no title bar, border, shadow or
//! taskbar entry, kept above the game. Three lines, what matters while
//! driving: the coach state (one at a time) with the turns that have the most
//! time to gain, the next cue, and the last sentence spoken. A bar on the
//! left edge takes the color of the state. Drag it anywhere on its surface to
//! move it, and by the grip in the bottom-right corner to resize it; it
//! reopens where and as it was left.
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

use crate::widgets::{color, label, theme};
use crate::{Activity, CoachView};

/// Default size, px: three lines.
pub const SIZE: (f32, f32) = (380., 72.);
/// Smallest size the grip allows, px.
const MIN: (f32, f32) = (220., 56.);

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
        ("Analyzing", theme::ACCENT)
    } else {
        ("Idle", theme::MUTED)
    }
}

/// "T4 +0.31 · T2 +0.20": the turns with the most time to gain.
pub(crate) fn gains_text(gains: &[(String, f64)]) -> Option<String> {
    (!gains.is_empty()).then(|| {
        gains
            .iter()
            .map(|(c, s)| format!("{c} +{s:.2}"))
            .collect::<Vec<_>>()
            .join(" · ")
    })
}

impl Render for WidgetView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let v = self.coach.read(cx);
        let (state, tone) = state_of(&v.coach_activity());
        // Per-turn passes when the race engineer runs, the last lap otherwise.
        let gains = match v.planner.as_ref() {
            Some(p) => p.status().coaching.top_gains(3),
            None => v
                .comparisons
                .last
                .as_ref()
                .map(|c| {
                    c.ranked()
                        .into_iter()
                        .filter(|c| c.time_delta >= 0.05)
                        .take(3)
                        .map(|c| (c.label(), c.time_delta))
                        .collect()
                })
                .unwrap_or_default(),
        };
        let gains = gains_text(&gains).unwrap_or_default();
        let (next, next_tone) = v.next_cue().unwrap_or_else(|| ("–".into(), theme::IDLE));
        let said = v.last_said().unwrap_or_else(|| "–".into());

        let line = |name: &'static str, text: String, tone: u32| {
            div()
                .flex()
                .items_center()
                .gap_2()
                .min_w_0()
                .child(label(name).w(px(34.)).flex_none())
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
        };
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
            .flex()
            .bg(color(theme::BG))
            .text_color(color(theme::TEXT))
            .font_family(theme::FONT)
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
            // The state, as a bar along the left edge.
            .child(div().w(px(4.)).h_full().flex_none().bg(color(tone)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .gap(px(2.))
                    .px_2()
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
                                    .text_sm()
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(color(tone))
                                    .child(state.to_uppercase()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_sm()
                                    .font_weight(FontWeight::BOLD)
                                    .text_right()
                                    .whitespace_nowrap()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .text_color(color(theme::BEST))
                                    .child(gains),
                            ),
                    )
                    .child(line("NEXT", next, next_tone))
                    .child(line("SAID", said, theme::OK)),
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
        plain(window)
    }

    /// Generic over the handle: `gpui::Window` has its own `window_handle()`
    /// (GPUI's handle), which hides the `raw-window-handle` trait method.
    #[cfg(windows)]
    fn plain(window: &impl raw_window_handle::HasWindowHandle) -> Result<(), String> {
        use raw_window_handle::RawWindowHandle;
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
    fn gains_line() {
        assert_eq!(gains_text(&[]), None);
        assert_eq!(
            gains_text(&[("T4".into(), 0.312), ("T2".into(), 0.2)]).unwrap(),
            "T4 +0.31 · T2 +0.20"
        );
    }
}
