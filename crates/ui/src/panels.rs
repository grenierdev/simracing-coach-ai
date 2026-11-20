//! Panels shared by the screens.

use analysis::CornerComparison;
use coach::AgentStatus;
use gpui::{Context, IntoElement, MouseButton, SharedString, Window, div, prelude::*, px};
use telemetry::ConnectionState;
use triggers::TriggerState;
use voice::SpeakerState;

use crate::chat::Who;
use crate::state::ReferenceState;
use crate::widgets::*;
use crate::{CoachView, PlanStatus, ReferenceStatus, format, settings};

impl CoachView {
    pub(crate) fn render_status(&self) -> gpui::Div {
        let s = &self.snap;
        let (c, detail): (u32, SharedString) = match &s.state {
            ConnectionState::Waiting => (theme::WARN, "waiting".into()),
            ConnectionState::Connected => (
                theme::OK,
                format!("{} · {:.0} Hz", s.source, s.sample_rate_hz).into(),
            ),
            ConnectionState::Ended => (theme::IDLE, "replay ended".into()),
            ConnectionState::Error(e) => (theme::ERR, e.clone().into()),
        };
        div()
            .flex()
            .flex_wrap()
            .gap_2()
            .child(status_pill("Telemetry", c, detail))
            .child(self.render_stt_pill())
            .child(self.render_tts_pill())
            .child(self.render_agent_pill())
            .child(match (&self.mcp_url, &self.mcp_error) {
                (Some(url), _) => status_pill("MCP server", theme::OK, url.clone()),
                (None, Some(e)) => status_pill("MCP server", theme::ERR, e.clone()),
                (None, None) => status_pill("MCP server", theme::IDLE, "stopped"),
            })
    }

    pub(crate) fn render_stt_pill(&self) -> gpui::Div {
        if self.recording {
            let level = self
                .recorder
                .as_ref()
                .map(|r| (r.level() * 100.0).min(100.0))
                .unwrap_or(0.0);
            return status_pill("STT", theme::ERR, format!("● recording {level:.0}%"));
        }
        let Some(l) = &self.listener else {
            return status_pill("STT", theme::IDLE, "disabled");
        };
        let st = l.status();
        let (c, detail) = match &st.state {
            SpeakerState::Loading => (theme::WARN, st.detail.unwrap_or_else(|| "loading".into())),
            SpeakerState::Ready => match (&self.settings.ptt, &self.recorder) {
                (_, None) => (theme::WARN, "no microphone".into()),
                (_, Some(r)) if r.device().is_none() => (theme::WARN, "no microphone".into()),
                (None, _) => (theme::WARN, "push-to-talk button not set".into()),
                (Some(b), Some(_)) => (theme::OK, b.label()),
            },
            SpeakerState::Error(e) => (theme::ERR, e.clone()),
        };
        status_pill("STT", c, detail)
    }

    pub(crate) fn render_voice_settings(&self, cx: &mut Context<Self>) -> gpui::Div {
        let capturing = self.capture.is_some();
        let ptt = if capturing {
            "Press a key or a button… (Esc to cancel)".to_string()
        } else {
            self.settings
                .ptt
                .as_ref()
                .map(|b| b.label())
                .unwrap_or_else(|| "no button".into())
        };
        let button = |id: &'static str, label: String| {
            div()
                .id(id)
                .px_2()
                .py_1()
                .flex_none()
                .rounded_md()
                .bg(color(theme::BORDER))
                .text_xs()
                .cursor_pointer()
                .child(label)
        };
        let latency = self.voice_latency.map(|l| {
            let f = |ms: Option<f64>| {
                ms.map(|v| format!("{:.2} s", v / 1000.0))
                    .unwrap_or_else(|| "…".into())
            };
            format!(
                "End of speech → text {} → first word {} → first sound {}",
                f(l.transcript_ms),
                f(l.first_word_ms),
                f(l.first_sound_ms)
            )
        });
        div()
            .flex()
            .flex_col()
            .gap_1()
            .pb_2()
            .border_b_1()
            .border_color(color(theme::BORDER))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .text_xs()
                    .child(
                        div()
                            .flex_none()
                            .text_color(color(theme::MUTED))
                            .child("Push to talk:"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .whitespace_normal()
                            .text_color(color(if capturing { theme::WARN } else { theme::TEXT }))
                            .child(ptt),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        button(
                            "ptt-bind",
                            if capturing {
                                "Cancel".into()
                            } else {
                                "Change…".into()
                            },
                        )
                        .on_click(cx.listener(|view, _, _, cx| {
                            if view.capture.is_some() {
                                if let Some(i) = &view.input {
                                    i.cancel_capture();
                                }
                            } else {
                                view.start_capture();
                            }
                            cx.notify();
                        })),
                    )
                    .child(
                        button(
                            "speak-replies",
                            if self.settings.speak_replies {
                                "Spoken replies: on".into()
                            } else {
                                "Spoken replies: off".into()
                            },
                        )
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.toggle_speak_replies();
                            cx.notify();
                        })),
                    )
                    .child(
                        button("language", format!("Voice: {}", self.settings.language)).on_click(
                            cx.listener(|view, _, _, cx| {
                                view.cycle_language();
                                cx.notify();
                            }),
                        ),
                    ),
            )
            .children((self.settings.language != self.language).then(|| {
                div()
                    .text_xs()
                    .text_color(color(theme::WARN))
                    .child(format!(
                        "Voice language {} after restart (current: {})",
                        self.settings.language, self.language
                    ))
            }))
            .child(
                self.render_device_row(
                    voice::Direction::Input,
                    "Microphone",
                    self.recorder
                        .as_ref()
                        .and_then(|r| r.device())
                        .unwrap_or_else(|| "none".into()),
                    self.settings.microphone.is_none(),
                    cx,
                ),
            )
            .children(self.output.as_ref().map(|o| {
                self.render_device_row(
                    voice::Direction::Output,
                    "Coach output",
                    voice::AudioSink::name(o.as_ref()),
                    self.settings.output.is_none(),
                    cx,
                )
            }))
            .children(self.render_device_menu(cx))
            .children(latency.map(|l| div().text_xs().text_color(color(theme::MUTED)).child(l)))
    }

    pub(crate) fn render_device_row(
        &self,
        direction: voice::Direction,
        label: &'static str,
        current: String,
        is_default: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let open = matches!(&self.device_menu, Some((d, _)) if *d == direction);
        let id = match direction {
            voice::Direction::Input => "device-in",
            voice::Direction::Output => "device-out",
        };
        let suffix = if is_default { " (default)" } else { "" };
        div()
            .flex()
            .gap_2()
            .items_center()
            .text_xs()
            .child(
                div()
                    .flex_none()
                    .text_color(color(theme::MUTED))
                    .child(format!("{label}:")),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(format!("{current}{suffix}")),
            )
            .child(
                div()
                    .id(id)
                    .px_2()
                    .py_1()
                    .flex_none()
                    .rounded_md()
                    .bg(color(theme::BORDER))
                    .cursor_pointer()
                    .child(if open { "Close" } else { "Change…" })
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.toggle_device_menu(direction);
                        cx.notify();
                    })),
            )
    }

    /// List of devices under the row that opened it.
    pub(crate) fn render_device_menu(&self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let (direction, devices) = self.device_menu.clone()?;
        let selected = match direction {
            voice::Direction::Input => settings::DeviceChoice::id(&self.settings.microphone),
            voice::Direction::Output => settings::DeviceChoice::id(&self.settings.output),
        }
        .map(str::to_string);
        let mut entries: Vec<(String, Option<voice::AudioDevice>)> =
            vec![("System default".into(), None)];
        entries.extend(devices.into_iter().map(|d| (d.name.clone(), Some(d))));
        let rows = entries.into_iter().enumerate().map(|(i, (name, device))| {
            let active = device.as_ref().map(|d| d.id.clone()) == selected;
            div()
                .id(("device", i))
                .px_2()
                .py_1()
                .rounded_md()
                .whitespace_nowrap()
                .overflow_hidden()
                .text_ellipsis()
                .cursor_pointer()
                .text_color(color(if active { theme::BEST } else { theme::TEXT }))
                .hover(|s| s.bg(color(theme::BORDER)))
                .child(if active { format!("● {name}") } else { name })
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.select_device(direction, device.clone());
                    cx.notify();
                }))
        });
        Some(
            div()
                .flex()
                .flex_col()
                .text_xs()
                .p_1()
                .rounded_md()
                .border_1()
                .border_color(color(theme::BORDER))
                .children(rows),
        )
    }

    pub(crate) fn render_agent_pill(&self) -> gpui::Div {
        let latency = self
            .chat
            .last_latency
            .and_then(|l| l.first_chunk_ms)
            .map(|ms| format!(" · first word {:.2} s", ms / 1000.0))
            .unwrap_or_default();
        let (c, detail): (u32, String) = match (&self.agent, &self.chat.status) {
            (None, _) => (theme::IDLE, "disabled".into()),
            (Some(_), None | Some(AgentStatus::Starting)) => (theme::WARN, "starting".into()),
            (Some(a), Some(AgentStatus::Ready { .. })) => {
                (theme::OK, format!("{}{latency}", a.label()))
            }
            (Some(_), Some(AgentStatus::Busy)) => (theme::WARN, format!("thinking{latency}")),
            (Some(_), Some(AgentStatus::Failed(e))) => (theme::ERR, e.clone()),
            (Some(_), Some(AgentStatus::Stopped)) => (theme::IDLE, "stopped".into()),
        };
        status_pill("ACP agent", c, detail)
    }

    pub(crate) fn render_chat(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entries = self.chat.entries.iter().map(|e| {
            let (who, tone) = match e.who {
                Who::Driver => ("Driver", theme::BEST),
                Who::Coach => ("Coach", theme::TEXT),
                Who::Tool => ("tool", theme::MUTED),
                Who::System => ("", theme::WARN),
                Who::Analysis => ("Analysis", theme::BEST),
            };
            let small = matches!(e.who, Who::Tool | Who::System);
            div()
                .flex()
                .w_full()
                .gap_2()
                .when(small, |d| d.text_xs())
                .when(!small, |d| d.text_sm())
                .child(
                    div()
                        .w(px(60.))
                        .flex_none()
                        .text_color(color(theme::MUTED))
                        .child(who),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .whitespace_normal()
                        .text_color(color(tone))
                        .child(e.text.clone()),
                )
        });
        let focused = self.input_focus.is_focused(window);
        let placeholder = self.chat.input.is_empty();
        let input_text = if placeholder {
            "Message the coach… (Enter: send, Esc: interrupt)".to_string()
        } else if focused {
            format!("{}▏", self.chat.input)
        } else {
            self.chat.input.clone()
        };
        let input = div()
            .id("chat-input")
            .track_focus(&self.input_focus)
            .on_key_down(cx.listener(Self::on_key))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, _, window, cx| window.focus(&view.input_focus, cx)),
            )
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(color(if focused { theme::BEST } else { theme::BORDER }))
            .bg(color(theme::BG))
            .text_sm()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .text_color(color(if placeholder {
                theme::MUTED
            } else {
                theme::TEXT
            }))
            .child(input_text);
        panel("Conversation")
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .child(
                div()
                    .id("chat-log")
                    .track_scroll(&self.chat_scroll)
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(entries),
            )
            .child(input)
    }

    pub(crate) fn render_tts_pill(&self) -> gpui::Div {
        let st = &self.speaker_status;
        let (c, detail): (u32, String) = match &st.state {
            SpeakerState::Loading => (
                theme::WARN,
                st.detail.clone().unwrap_or_else(|| "loading".into()),
            ),
            SpeakerState::Ready => {
                let mut d = st.engine.clone();
                if let Some(ms) = st.last_latency_ms {
                    d.push_str(&format!(" · {ms:.0} ms"));
                }
                (theme::OK, d)
            }
            SpeakerState::Error(e) => (theme::ERR, e.clone()),
        };
        status_pill("TTS", c, detail)
    }

    pub(crate) fn render_session(&self, cx: &mut Context<Self>) -> gpui::Div {
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
                    .child(format!("Source: {}", s.source)),
            )
            .child(self.render_reference(cx))
            .child(self.render_plan_line(cx))
    }

    pub(crate) fn render_plan_line(&self, cx: &mut Context<Self>) -> gpui::Div {
        let (line, tone) = match (&self.plan, &self.coach_status.plan_error) {
            (PlanStatus::None, _) => ("No cue plan".to_string(), theme::MUTED),
            (PlanStatus::Failed(e), _) => (e.clone(), theme::ERR),
            (PlanStatus::Loaded(_), Some(e)) => (e.clone(), theme::ERR),
            (PlanStatus::Loaded(n), None) => (
                {
                    let coach = self
                        .coach_status
                        .triggers
                        .iter()
                        .filter(|v| v.trigger.id.starts_with(triggers::COACH_PREFIX))
                        .count();
                    let own = self.coach_status.triggers.len() - coach;
                    let coach = if coach > 0 {
                        format!(" + {coach} from the coach")
                    } else {
                        String::new()
                    };
                    format!(
                        "{n} · {own} cues{coach} · {} quiet zones",
                        self.coach_status.quiet_zones
                    )
                },
                theme::TEXT,
            ),
        };
        let button = div()
            .id("pick-plan")
            .px_2()
            .py_1()
            .rounded_md()
            .bg(color(theme::BORDER))
            .text_xs()
            .cursor_pointer()
            .child("Choose…")
            .on_click(cx.listener(|view, _, _, cx| view.pick_plan(cx)));
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
                            .child("PLAN"),
                    )
                    .child(button),
            )
            .child(div().text_sm().text_color(color(tone)).child(line))
    }

    pub(crate) fn render_planner(&self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let p = self.planner.as_ref()?;
        let st = p.status();
        let (line, tone) = if !st.enabled {
            ("Focus coaching disabled".to_string(), theme::MUTED)
        } else if let Some(what) = &st.running {
            (format!("Analyzing {what} ({})…", st.model), theme::WARN)
        } else if let Some(e) = &st.error {
            (format!("Analysis: {e}"), theme::ERR)
        } else if let Some(last) = &st.last {
            (
                format!(
                    "{} analyzed in {:.0} s: {}",
                    last.title, last.secs, last.text
                ),
                theme::TEXT,
            )
        } else {
            (
                format!("Focus coaching ({}) after the next valid lap", st.model),
                theme::MUTED,
            )
        };
        let toggle = div()
            .id("auto-planning")
            .px_2()
            .py_1()
            .flex_none()
            .rounded_md()
            .bg(color(theme::BORDER))
            .text_xs()
            .cursor_pointer()
            .child(if st.enabled { "Auto: on" } else { "Auto: off" })
            .on_click(cx.listener(|view, _, _, cx| {
                view.toggle_auto_planning();
                cx.notify();
            }));
        Some(
            div()
                .flex()
                .items_center()
                .gap_2()
                .text_xs()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .whitespace_normal()
                        .text_color(color(tone))
                        .child(line),
                )
                .child(toggle),
        )
    }

    pub(crate) fn render_reference(&self, cx: &mut Context<Self>) -> gpui::Div {
        let session_length = self.snap.session.as_ref().and_then(|i| i.track_length_m);
        let (line, detail, tone): (String, Option<String>, u32) = match &self.reference {
            ReferenceStatus::None => ("No reference".into(), None, theme::MUTED),
            ReferenceStatus::Loading(n) => (format!("Loading {n}…"), None, theme::WARN),
            ReferenceStatus::Failed(e) => ("Loading failed".into(), Some(e.clone()), theme::ERR),
            ReferenceStatus::Ready(r) => {
                let rot = r.reference.rotation;
                let mut detail = format!(
                    "{} corners · rotation: {}",
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
            .child("Choose…")
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
                            .child("REFERENCE"),
                    )
                    .child(button),
            )
            .child(div().text_sm().text_color(color(tone)).child(line))
            .children(detail.map(|d| div().text_xs().text_color(color(theme::MUTED)).child(d)))
    }

    pub(crate) fn render_laps(&self) -> impl IntoElement {
        let s = &self.snap;
        let best = s.best_lap.as_ref().map(|l| l.lap_time);
        let header = row(
            vec![
                (50., "Lap".into()),
                (90., "Time".into()),
                (70., "Gap".into()),
                (70., "Δ ref".into()),
                (80., "Status".into()),
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
        panel("Laps")
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .child(header)
            .child(
                div()
                    .id("laps")
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .overflow_y_scroll()
                    .children(rows),
            )
    }

    pub(crate) fn render_corners(&self) -> gpui::AnyElement {
        match self.ready_reference() {
            Some(r) => self.render_corner_deltas(r).into_any_element(),
            None => self.render_detected_corners().into_any_element(),
        }
    }

    pub(crate) fn render_corner_deltas(&self, r: &ReferenceState) -> impl IntoElement {
        let current = self.snap.sample.map(|x| x.lap_dist_pct as f64);
        let last = self.comparisons.last.as_ref();
        let best = self.comparisons.best.as_ref();
        let header = row(
            vec![
                (50., "Corner".into()),
                (68., "Δ last".into()),
                (72., "Δ best".into()),
                (90., "Min V / ref".into()),
                (56., "Brake".into()),
                (64., "Throttle".into()),
                (56., "Trail".into()),
                (88., "Rot. in/out".into()),
                (72., "Coasting".into()),
                (60., "Turn-in".into()),
                (56., "Radius".into()),
                (52., "Steer".into()),
                (64., "Line".into()),
                (110., "Cue".into()),
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
            let label = zone.corner.label();
            let cue = self
                .coach_status
                .triggers
                .iter()
                .find(|v| {
                    v.trigger.corner.as_deref() == Some(label.as_str())
                        && v.state != TriggerState::Expired
                })
                .map(|v| v.trigger.text.clone())
                .unwrap_or_default();
            let cell =
                |w: f32, text: String, c: u32| div().w(px(w)).text_color(color(c)).child(text);
            div()
                .flex()
                .gap_2()
                .py(px(2.))
                .text_sm()
                .child(cell(50., zone.corner.label(), label_color))
                .child(cell(68., dl, dl_color))
                .child(cell(72., db, db_color))
                .child(cell(90., vmin, theme::TEXT))
                .child(cell(
                    56.,
                    format::meters(lc.and_then(|c| c.brake_point_delta_m)),
                    theme::TEXT,
                ))
                .child(cell(
                    64.,
                    format::meters(lc.and_then(|c| c.throttle_on_delta_m)),
                    theme::TEXT,
                ))
                .child(cell(
                    56.,
                    lc.map(|c| format!("{:.0} m", c.lap.trail_brake_m))
                        .unwrap_or_else(|| "–".into()),
                    theme::TEXT,
                ))
                .child(cell(
                    88.,
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
                    72.,
                    lc.map(|c| format!("{:.2} s", c.lap.coasting_s))
                        .unwrap_or_else(|| "–".into()),
                    theme::TEXT,
                ))
                .child(cell(
                    60.,
                    format::meters(lc.and_then(|c| c.turn_in_delta_m)),
                    theme::TEXT,
                ))
                .child(cell(
                    56.,
                    format::percent_change(lc.and_then(|c| c.min_radius_ratio)),
                    theme::TEXT,
                ))
                .child(cell(
                    52.,
                    format::percent_change(lc.and_then(|c| c.steering_peak_ratio)),
                    theme::TEXT,
                ))
                .child(cell(
                    64.,
                    lc.and_then(|c| c.line)
                        .map(|l| format!("{:+.1} m", l.apex_m))
                        .unwrap_or_else(|| "–".into()),
                    theme::TEXT,
                ))
                .child(
                    div()
                        .w(px(110.))
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_color(color(theme::MUTED))
                        .child(cue),
                )
        });
        let summary = match (last, best) {
            (Some(l), _) => {
                let worst = l
                    .ranked()
                    .first()
                    .map(|c| {
                        format!(
                            " · costliest: {} ({})",
                            c.label(),
                            format::delta(c.time_delta)
                        )
                    })
                    .unwrap_or_default();
                format!(
                    "Last lap {}: {}{worst}",
                    l.lap_number,
                    format::delta(l.total_delta())
                )
            }
            (None, _) => "Comparison after the first complete lap.".into(),
        };
        panel("Corners vs reference")
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .child(div().text_sm().text_color(color(theme::MUTED)).child(summary))
            .child(header)
            .child(
                div()
                    .id("corner-deltas")
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .overflow_y_scroll()
                    .children(rows),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(color(theme::MUTED))
                    .child("Brake, throttle, turn-in: + later than the reference. Rotation: < 1 less rotation for the same steering. Radius: − tighter path. Steer: + more steering. Line: offset at the apex, + towards the inside."),
            )
    }

    pub(crate) fn render_detected_corners(&self) -> impl IntoElement {
        let s = &self.snap;
        let current = s.sample.map(|x| x.lap_dist_pct as f64);
        let header = row(
            vec![
                (50., "Corner".into()),
                (70., "Direction".into()),
                (120., "Zone".into()),
                (90., "Min speed".into()),
                (90., "Min radius".into()),
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
                        format!("{:.1}–{:.1}%", c.start_pct * 100.0, c.end_pct * 100.0).into(),
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
                .child("Detected after the first valid lap.")
        });
        panel("Corners (best lap)")
            .flex_1()
            .child(header)
            .children(empty)
            .child(
                div()
                    .id("corners")
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .overflow_y_scroll()
                    .children(rows),
            )
    }
}
