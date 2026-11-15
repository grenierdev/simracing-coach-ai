//! Interface GPUI du coach : état des connexions, session, référence, valeurs
//! live, tours enregistrés, virages et deltas face à la référence.
//!
//! L'interface ne fait que lire l'instantané publié par le hub de télémétrie,
//! à ~30 Hz ; elle ne bloque jamais la boucle rapide.

pub mod format;
pub mod state;

use std::path::PathBuf;
use std::time::Duration;

use analysis::{Analyzer, CornerComparison};
use gpui::{
    App, Bounds, Context, FontWeight, Hsla, IntoElement, PathPromptOptions, Render, SharedString,
    Task, TitlebarOptions, Window, WindowBounds, WindowOptions, div, prelude::*, px, relative, rgb,
    size,
};
use telemetry::{ConnectionState, CornerDetectionParams, LiveSnapshot, TelemetryHub};

use crate::state::{Comparisons, ReferenceState};

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

/// Lance l'application. `make_hub` est appelé une fois la fenêtre prête ;
/// `reference` : fichier de référence à charger au démarrage.
pub fn run(
    title: &str,
    reference: Option<PathBuf>,
    make_hub: impl FnOnce() -> TelemetryHub + 'static,
) {
    let title: SharedString = title.to_string().into();
    gpui_platform::application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1280.), px(820.)), cx);
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
            |_, cx| cx.new(|cx| CoachView::new(hub, reference, cx)),
        )
        .expect("ouverture de la fenêtre");
        cx.activate(true);
    });
}

enum ReferenceStatus {
    None,
    Loading(String),
    Ready(Box<ReferenceState>),
    Failed(String),
}

pub struct CoachView {
    hub: TelemetryHub,
    snap: LiveSnapshot,
    reference: ReferenceStatus,
    comparisons: Comparisons,
    _refresh: Task<()>,
}

impl CoachView {
    fn new(hub: TelemetryHub, reference: Option<PathBuf>, cx: &mut Context<Self>) -> Self {
        let refresh = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH).await;
                let alive = this.update(cx, |view, cx| {
                    view.snap = view.hub.snapshot();
                    if let ReferenceStatus::Ready(r) = &view.reference {
                        view.comparisons.update(&view.snap, &r.analyzer);
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        let mut view = CoachView {
            snap: hub.snapshot(),
            hub,
            reference: ReferenceStatus::None,
            comparisons: Comparisons::default(),
            _refresh: refresh,
        };
        if let Some(path) = reference {
            view.load_reference(path, cx);
        }
        view
    }

    fn pick_reference(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Tour de référence (.csv Garage61 ou .ibt)".into()),
        });
        cx.spawn(async move |this, cx| match rx.await {
            Ok(Ok(Some(paths))) => {
                if let Some(path) = paths.into_iter().next() {
                    let _ = this.update(cx, |view, cx| view.load_reference(path, cx));
                }
            }
            Ok(Err(e)) => {
                let _ = this.update(cx, |view, cx| {
                    view.reference = ReferenceStatus::Failed(format!("sélecteur de fichier : {e}"));
                    cx.notify();
                });
            }
            _ => {}
        })
        .detach();
    }

    fn load_reference(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.reference = ReferenceStatus::Loading(name);
        self.comparisons.clear();
        let session_length = self.snap.session.as_ref().and_then(|s| s.track_length_m);
        let task = cx.background_executor().spawn(async move {
            reference::load(&path).map(|r| {
                let analyzer = Analyzer::new(
                    &r.lap,
                    r.track_length_m.or(session_length),
                    &CornerDetectionParams::default(),
                );
                ReferenceState {
                    reference: r,
                    analyzer,
                }
            })
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |view, cx| {
                view.reference = match result {
                    Ok(state) => {
                        view.comparisons.update(&view.snap, &state.analyzer);
                        ReferenceStatus::Ready(Box::new(state))
                    }
                    Err(e) => ReferenceStatus::Failed(e.to_string()),
                };
                cx.notify();
            });
        })
        .detach();
    }

    fn ready_reference(&self) -> Option<&ReferenceState> {
        match &self.reference {
            ReferenceStatus::Ready(r) => Some(r),
            _ => None,
        }
    }
}

fn delta_color(d: f64) -> u32 {
    if !d.is_finite() || d.abs() < 0.0005 {
        theme::TEXT
    } else if d > 0.0 {
        theme::ERR
    } else {
        theme::OK
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
        .children(
            cells
                .into_iter()
                .map(|(w, s)| div().w(px(w)).whitespace_nowrap().child(s)),
        )
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

    fn render_session(&self, cx: &mut Context<Self>) -> gpui::Div {
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
            .child(self.render_reference(cx))
    }

    fn render_reference(&self, cx: &mut Context<Self>) -> gpui::Div {
        let session_length = self.snap.session.as_ref().and_then(|i| i.track_length_m);
        let (line, detail, tone): (String, Option<String>, u32) = match &self.reference {
            ReferenceStatus::None => ("Aucune référence".into(), None, theme::MUTED),
            ReferenceStatus::Loading(n) => (format!("Chargement de {n}…"), None, theme::WARN),
            ReferenceStatus::Failed(e) => {
                ("Échec du chargement".into(), Some(e.clone()), theme::ERR)
            }
            ReferenceStatus::Ready(r) => {
                let rot = r.reference.rotation;
                let mut detail = format!(
                    "{} virages · rotation : {}",
                    r.analyzer.model.corners.len(),
                    rot.yaw_rate.label()
                );
                let tone = match r.length_mismatch(session_length) {
                    Some(w) => {
                        detail = format!("{detail}\n{w}");
                        theme::WARN
                    }
                    None => theme::TEXT,
                };
                (r.reference.label.clone(), Some(detail), tone)
            }
        };
        let button = div()
            .id("pick-reference")
            .px_2()
            .py_1()
            .rounded_md()
            .bg(color(theme::BORDER))
            .text_xs()
            .cursor_pointer()
            .child("Choisir…")
            .on_click(cx.listener(|view, _, _, cx| view.pick_reference(cx)));
        div()
            .flex()
            .flex_col()
            .gap_1()
            .pt_2()
            .border_t_1()
            .border_color(color(theme::BORDER))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .text_xs()
                            .text_color(color(theme::MUTED))
                            .child("RÉFÉRENCE"),
                    )
                    .child(button),
            )
            .child(div().text_sm().text_color(color(tone)).child(line))
            .children(detail.map(|d| div().text_xs().text_color(color(theme::MUTED)).child(d)))
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
        let live_delta = match (self.ready_reference(), s.sample, s.current_lap_elapsed) {
            (Some(r), Some(smp), Some(el)) if s.current_lap_flags.is_some_and(|f| f.complete) => {
                Some(r.analyzer.live_delta(smp.lap_dist_pct as f64, el))
            }
            _ => None,
        };
        let delta_box = div()
            .flex()
            .flex_col()
            .child(
                div()
                    .text_xs()
                    .text_color(color(theme::MUTED))
                    .child("Δ réf"),
            )
            .child(
                div()
                    .text_2xl()
                    .font_weight(FontWeight::BOLD)
                    .text_color(color(live_delta.map(delta_color).unwrap_or(theme::MUTED)))
                    .child(match live_delta {
                        Some(d) => format::delta(d),
                        None => "–".into(),
                    }),
            );
        panel("En piste")
            .child(
                div()
                    .flex()
                    .gap_6()
                    .child(big_value("Tour", lap))
                    .child(big_value("Chrono", elapsed))
                    .child(delta_box)
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
                (70., "Écart".into()),
                (70., "Δ réf".into()),
                (80., "État".into()),
            ],
            theme::MUTED,
        );
        let ref_time = self
            .ready_reference()
            .map(|r| r.analyzer.reference.lap_time);
        let rows = s.laps.iter().rev().map(|l| {
            let dr = match ref_time {
                Some(t) if l.flags.complete => format::delta(l.lap_time - t),
                _ => String::new(),
            };
            let is_best = best == Some(l.lap_time) && l.is_valid();
            let d = match best {
                Some(b) if l.flags.complete => format::delta(l.lap_time - b),
                _ => String::new(),
            };
            row(
                vec![
                    (50., l.number.to_string().into()),
                    (90., format::lap_time(l.lap_time).into()),
                    (70., d.into()),
                    (70., dr.into()),
                    (80., format::lap_status(&l.flags).into()),
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

    fn render_corners(&self) -> gpui::AnyElement {
        match self.ready_reference() {
            Some(r) => self.render_corner_deltas(r).into_any_element(),
            None => self.render_detected_corners().into_any_element(),
        }
    }

    fn render_corner_deltas(&self, r: &ReferenceState) -> impl IntoElement {
        let current = self.snap.sample.map(|x| x.lap_dist_pct as f64);
        let last = self.comparisons.last.as_ref();
        let best = self.comparisons.best.as_ref();
        let header = row(
            vec![
                (50., "Virage".into()),
                (72., "Δ dernier".into()),
                (78., "Δ meilleur".into()),
                (96., "V mini / réf".into()),
                (60., "Frein".into()),
                (60., "Gaz".into()),
                (60., "Trail".into()),
                (96., "Rotation E/S".into()),
                (76., "Roue libre".into()),
            ],
            theme::MUTED,
        );
        let cell_delta = |c: Option<&CornerComparison>| -> (String, u32) {
            match c {
                Some(c) => (format::delta(c.time_delta), delta_color(c.time_delta)),
                None => ("–".into(), theme::MUTED),
            }
        };
        let rows = r.analyzer.model.zones.iter().enumerate().map(|(i, zone)| {
            let lc = last.and_then(|l| l.corners.get(i));
            let bc = best.and_then(|b| b.corners.get(i));
            let reference = &r.analyzer.reference_metrics[i];
            let here = current.is_some_and(|p| zone.contains(p));
            let (dl, dl_color) = cell_delta(lc);
            let (db, db_color) = cell_delta(bc);
            let vmin = match lc {
                Some(c) => format!(
                    "{} / {}",
                    format::kmh(c.lap.min_speed as f32),
                    format::kmh(reference.min_speed as f32)
                ),
                None => format!("– / {}", format::kmh(reference.min_speed as f32)),
            };
            let label_color = if here { theme::WARN } else { theme::TEXT };
            let cell =
                |w: f32, text: String, c: u32| div().w(px(w)).text_color(color(c)).child(text);
            div()
                .flex()
                .gap_2()
                .py(px(2.))
                .text_sm()
                .child(cell(50., zone.corner.label(), label_color))
                .child(cell(72., dl, dl_color))
                .child(cell(78., db, db_color))
                .child(cell(96., vmin, theme::TEXT))
                .child(cell(
                    60.,
                    format::meters(lc.and_then(|c| c.brake_point_delta_m)),
                    theme::TEXT,
                ))
                .child(cell(
                    60.,
                    format::meters(lc.and_then(|c| c.throttle_on_delta_m)),
                    theme::TEXT,
                ))
                .child(cell(
                    60.,
                    lc.map(|c| format!("{:.0} m", c.lap.trail_brake_m))
                        .unwrap_or_else(|| "–".into()),
                    theme::TEXT,
                ))
                .child(cell(
                    96.,
                    match lc {
                        Some(c) => format!(
                            "{} / {}",
                            format::ratio(c.rotation_entry_ratio),
                            format::ratio(c.rotation_exit_ratio)
                        ),
                        None => "–".into(),
                    },
                    theme::TEXT,
                ))
                .child(cell(
                    76.,
                    lc.map(|c| format!("{:.2} s", c.lap.coasting_s))
                        .unwrap_or_else(|| "–".into()),
                    theme::TEXT,
                ))
        });
        let summary = match (last, best) {
            (Some(l), _) => {
                let worst = l
                    .ranked()
                    .first()
                    .map(|c| {
                        format!(
                            " · plus coûteux : {} ({})",
                            c.label(),
                            format::delta(c.time_delta)
                        )
                    })
                    .unwrap_or_default();
                format!(
                    "Dernier tour {} : {}{worst}",
                    l.lap_number,
                    format::delta(l.total_delta())
                )
            }
            (None, _) => "Comparaison après le premier tour complet.".into(),
        };
        panel("Virages vs référence")
            .flex_1()
            .child(div().text_sm().text_color(color(theme::MUTED)).child(summary))
            .child(header)
            .child(
                div()
                    .id("corner-deltas")
                    .flex()
                    .flex_col()
                    .overflow_y_scroll()
                    .children(rows),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(color(theme::MUTED))
                    .child("Frein/Gaz : + plus tard que la référence. Rotation : < 1 moins de rotation pour le même braquage."),
            )
    }

    fn render_detected_corners(&self) -> impl IntoElement {
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
                    .child(div().w(px(360.)).child(self.render_session(cx)))
                    .child(div().flex_1().child(self.render_live())),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .gap_3()
                    .min_h_0()
                    .child(div().w(px(400.)).flex().child(self.render_laps()))
                    .child(div().flex_1().flex().child(self.render_corners())),
            )
    }
}
