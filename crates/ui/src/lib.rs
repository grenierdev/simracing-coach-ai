//! Interface GPUI du coach (jalon 1) : état des connexions, session, valeurs
//! live, tours enregistrés et virages détectés.
//!
//! L'interface ne fait que lire l'instantané publié par le hub de télémétrie,
//! à ~30 Hz ; elle ne bloque jamais la boucle rapide.

pub mod format;

use std::time::Duration;

use gpui::{
    App, Bounds, Context, FontWeight, Hsla, IntoElement, Render, SharedString, Task,
    TitlebarOptions, Window, WindowBounds, WindowOptions, div, prelude::*, px, relative, rgb, size,
};
use telemetry::{ConnectionState, LiveSnapshot, TelemetryHub};

const REFRESH: Duration = Duration::from_millis(33);

mod theme {
    pub const BG: u32 = 0x15171c;
    pub const PANEL: u32 = 0x1e2128;
    pub const BORDER: u32 = 0x2c313a;
    pub const TEXT: u32 = 0xe6e8ec;
    pub const MUTED: u32 = 0x8a919e;
    pub const OK: u32 = 0x3fb950;
    pub const WARN: u32 = 0xd29922;
    pub const ERR: u32 = 0xf85149;
    pub const IDLE: u32 = 0x59606b;
    pub const THROTTLE: u32 = 0x3fb950;
    pub const BRAKE: u32 = 0xf85149;
    pub const BEST: u32 = 0xbc8cff;
}

/// Lance l'application. `make_hub` est appelé une fois la fenêtre prête.
pub fn run(title: &str, make_hub: impl FnOnce() -> TelemetryHub + 'static) {
    let title: SharedString = title.to_string().into();
    gpui_platform::application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1100.), px(760.)), cx);
        let hub = make_hub();
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(title),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_, cx| cx.new(|cx| CoachView::new(hub, cx)),
        )
        .expect("ouverture de la fenêtre");
        cx.activate(true);
    });
}

pub struct CoachView {
    hub: TelemetryHub,
    snap: LiveSnapshot,
    _refresh: Task<()>,
}

impl CoachView {
    fn new(hub: TelemetryHub, cx: &mut Context<Self>) -> Self {
        let refresh = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH).await;
                let alive = this.update(cx, |view, cx| {
                    view.snap = view.hub.snapshot();
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        CoachView {
            snap: hub.snapshot(),
            hub,
            _refresh: refresh,
        }
    }
}

fn color(c: u32) -> Hsla {
    rgb(c).into()
}

fn panel(title: &'static str) -> gpui::Div {
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

fn status_pill(
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

fn bar(label: &'static str, value: f32, fill: u32) -> gpui::Div {
    let v = if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    };
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .w(px(60.))
                .text_xs()
                .text_color(color(theme::MUTED))
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .h(px(10.))
                .rounded_sm()
                .bg(color(theme::BG))
                .child(div().h_full().w(relative(v)).rounded_sm().bg(color(fill))),
        )
        .child(
            div()
                .w(px(40.))
                .text_xs()
                .child(format!("{:.0} %", v * 100.0)),
        )
}

fn big_value(label: &'static str, value: impl Into<SharedString>) -> gpui::Div {
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

fn row(cells: Vec<(f32, SharedString)>, text: u32) -> gpui::Div {
    div()
        .flex()
        .gap_2()
        .py(px(2.))
        .text_sm()
        .text_color(color(text))
        .children(cells.into_iter().map(|(w, s)| div().w(px(w)).child(s)))
}

impl CoachView {
    fn render_status(&self) -> gpui::Div {
        let s = &self.snap;
        let (c, detail): (u32, SharedString) = match &s.state {
            ConnectionState::Waiting => (theme::WARN, "en attente".into()),
            ConnectionState::Connected => (
                theme::OK,
                format!("{} · {:.0} Hz", s.source, s.sample_rate_hz).into(),
            ),
            ConnectionState::Ended => (theme::IDLE, "rejeu terminé".into()),
            ConnectionState::Error(e) => (theme::ERR, e.clone().into()),
        };
        div()
            .flex()
            .flex_wrap()
            .gap_2()
            .child(status_pill("Télémétrie", c, detail))
            .child(status_pill("STT", theme::IDLE, "jalon 5"))
            .child(status_pill("TTS", theme::IDLE, "jalon 3"))
            .child(status_pill("Agent ACP", theme::IDLE, "jalon 4"))
            .child(status_pill("Serveur MCP", theme::IDLE, "jalon 4"))
    }

    fn render_session(&self) -> gpui::Div {
        let s = &self.snap;
        let (track, car) = match &s.session {
            Some(info) => (info.track_label(), info.car_screen_name.clone()),
            None => ("–".into(), "–".into()),
        };
        let length = s
            .session
            .as_ref()
            .and_then(|i| i.track_length_m)
            .map(|m| format!("{:.2} km", m / 1000.0))
            .unwrap_or_default();
        panel("Session")
            .child(div().text_lg().child(track))
            .child(
                div()
                    .text_sm()
                    .text_color(color(theme::MUTED))
                    .child(format!("{car}   {length}")),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(color(theme::MUTED))
                    .child(format!("Source : {}", s.source)),
            )
    }

    fn render_live(&self) -> gpui::Div {
        let s = &self.snap;
        let smp = s.sample.unwrap_or_default();
        let has = s.sample.is_some();
        let lap = if has {
            smp.lap.to_string()
        } else {
            "–".into()
        };
        let pct = if has {
            format!("{:.1} %", smp.lap_dist_pct * 100.0)
        } else {
            "–".into()
        };
        let elapsed = s
            .current_lap_elapsed
            .map(format::lap_time)
            .unwrap_or_else(|| format::lap_time(f64::NAN));
        panel("En piste")
            .child(
                div()
                    .flex()
                    .gap_6()
                    .child(big_value("Tour", lap))
                    .child(big_value("Chrono", elapsed))
                    .child(big_value("Position", pct))
                    .child(big_value("km/h", format::kmh(smp.speed)))
                    .child(big_value("Rapport", format::gear(smp.gear)))
                    .child(big_value(
                        "tr/min",
                        if smp.rpm.is_finite() {
                            format!("{:.0}", smp.rpm)
                        } else {
                            "–".into()
                        },
                    )),
            )
            .child(bar("Gaz", smp.throttle, theme::THROTTLE))
            .child(bar("Frein", smp.brake, theme::BRAKE))
    }

    fn render_laps(&self) -> impl IntoElement {
        let s = &self.snap;
        let best = s.best_lap.as_ref().map(|l| l.lap_time);
        let header = row(
            vec![
                (50., "Tour".into()),
                (90., "Temps".into()),
                (80., "Écart".into()),
                (90., "État".into()),
            ],
            theme::MUTED,
        );
        let rows = s.laps.iter().rev().map(|l| {
            let is_best = best == Some(l.lap_time) && l.is_valid();
            let d = match best {
                Some(b) if l.flags.complete => format::delta(l.lap_time - b),
                _ => String::new(),
            };
            row(
                vec![
                    (50., l.number.to_string().into()),
                    (90., format::lap_time(l.lap_time).into()),
                    (80., d.into()),
                    (90., format::lap_status(&l.flags).into()),
                ],
                if is_best {
                    theme::BEST
                } else if l.is_valid() {
                    theme::TEXT
                } else {
                    theme::MUTED
                },
            )
        });
        panel("Tours").flex_1().child(header).child(
            div()
                .id("laps")
                .flex()
                .flex_col()
                .overflow_y_scroll()
                .children(rows),
        )
    }

    fn render_corners(&self) -> impl IntoElement {
        let s = &self.snap;
        let current = s.sample.map(|x| x.lap_dist_pct as f64);
        let header = row(
            vec![
                (50., "Virage".into()),
                (70., "Sens".into()),
                (120., "Zone".into()),
                (90., "V. mini".into()),
                (90., "Rayon mini".into()),
            ],
            theme::MUTED,
        );
        let rows = s.corners.iter().map(|c| {
            let here = current.is_some_and(|p| c.contains(p));
            row(
                vec![
                    (50., c.label().into()),
                    (70., format::direction(c.direction).into()),
                    (
                        120.,
                        format!("{:.1}–{:.1} %", c.start_pct * 100.0, c.end_pct * 100.0).into(),
                    ),
                    (
                        90.,
                        format!("{} km/h", format::kmh(c.min_speed as f32)).into(),
                    ),
                    (90., format!("{:.0} m", c.min_radius_m).into()),
                ],
                if here { theme::WARN } else { theme::TEXT },
            )
        });
        let empty = s.corners.is_empty().then(|| {
            div()
                .text_sm()
                .text_color(color(theme::MUTED))
                .child("Détectés après le premier tour valide.")
        });
        panel("Virages (meilleur tour)")
            .flex_1()
            .child(header)
            .children(empty)
            .child(
                div()
                    .id("corners")
                    .flex()
                    .flex_col()
                    .overflow_y_scroll()
                    .children(rows),
            )
    }
}

impl Render for CoachView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .gap_3()
            .p_3()
            .bg(color(theme::BG))
            .text_color(color(theme::TEXT))
            .child(self.render_status())
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(div().w(px(340.)).child(self.render_session()))
                    .child(div().flex_1().child(self.render_live())),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .gap_3()
                    .min_h_0()
                    .child(div().w(px(380.)).flex().child(self.render_laps()))
                    .child(div().flex_1().flex().child(self.render_corners())),
            )
    }
}
