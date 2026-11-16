//! GPUI interface of the coach: connection status, session, reference, live
//! values, recorded laps, corners and deltas to the reference.
//!
//! The UI only reads the snapshot published by the telemetry hub, at ~30 Hz;
//! it never blocks the fast loop.

pub mod chat;
pub mod conversation;
pub mod format;
pub mod planner;
pub mod settings;
pub mod state;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use analysis::CornerComparison;
use coach::{AgentStatus, CoachAgent};
use gpui::{
    App, Bounds, Context, FocusHandle, FontWeight, Hsla, IntoElement, KeyDownEvent, MouseButton,
    PathPromptOptions, Render, SharedString, Task, TitlebarOptions, Window, WindowBounds,
    WindowOptions, div, prelude::*, px, relative, rgb, size,
};
use mcp::ToolCallLog;
use reference::ReferenceLap;
use telemetry::{ConnectionState, LiveSnapshot, TelemetryHub};
use triggers::{Plan, RuntimeStatus, TriggerRuntime, TriggerState};
use voice::{Speaker, SpeakerState, SpeakerStatus};

use crate::chat::{Chat, Who};
use crate::conversation::{ConvEvent, Conversation, VoiceLatency};
use crate::planner::Planner;
use crate::settings::Settings;
use crate::state::{Comparisons, ReferenceState};
use input::{CaptureResult, InputService};
use voice::{Listener, Recorder};

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

/// Services started by the application and driven by the UI.
pub struct Services {
    pub hub: TelemetryHub,
    pub triggers: Arc<TriggerRuntime>,
    pub speaker: Arc<Speaker>,
    /// Conversation session with Claude (absent with `--no-agent`).
    pub agent: Option<Arc<CoachAgent>>,
    pub mcp_url: Option<String>,
    pub mcp_error: Option<String>,
    pub tool_log: Arc<Mutex<VecDeque<ToolCallLog>>>,
    /// Keeps the async runtime and the MCP server alive.
    pub keep_alive: Box<dyn std::any::Any>,
    /// Conversation loop (push-to-talk, STT, spoken replies).
    pub conversation: Conversation,
    pub input: Option<Arc<InputService>>,
    pub listener: Option<Arc<Listener>>,
    pub recorder: Option<Arc<Recorder>>,
    /// Switchable output of the coach's voice (absent with `--no-voice`).
    pub output: Option<Arc<voice::SwitchableSink>>,
    /// Startup warnings shown in the conversation (device not found…).
    pub notes: Vec<String>,
    pub settings: Settings,
    /// Voice language in use (settings or `--language`); a change in the
    /// settings applies on the next launch.
    pub language: voice::Language,
    /// Automatic planning of the next lap (absent with `--no-planning`).
    pub planner: Option<Planner>,
}

pub enum ReferenceSource {
    File(PathBuf),
    Preloaded(Box<ReferenceLap>),
}

#[derive(Default)]
pub struct StartOptions {
    pub reference: Option<ReferenceSource>,
    pub plan: Option<PathBuf>,
}

/// Starts the application. `make_services` is called once the UI is ready.
pub fn run(title: &str, options: StartOptions, make_services: impl FnOnce() -> Services + 'static) {
    let title: SharedString = title.to_string().into();
    gpui_platform::application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1320.), px(920.)), cx);
        let services = make_services();
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(title),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_, cx| cx.new(|cx| CoachView::new(services, options, cx)),
        )
        .expect("opening the window");
        cx.activate(true);
    });
}

enum ReferenceStatus {
    None,
    Loading(String),
    Ready(Box<ReferenceState>),
    Failed(String),
}

enum PlanStatus {
    None,
    Loaded(String),
    Failed(String),
}

pub struct CoachView {
    hub: TelemetryHub,
    coach: Arc<TriggerRuntime>,
    speaker: Arc<Speaker>,
    agent: Option<Arc<CoachAgent>>,
    mcp_url: Option<String>,
    mcp_error: Option<String>,
    tool_log: Arc<Mutex<VecDeque<ToolCallLog>>>,
    chat: Chat,
    input_focus: FocusHandle,
    _keep_alive: Box<dyn std::any::Any>,
    conversation: Conversation,
    input: Option<Arc<InputService>>,
    listener: Option<Arc<Listener>>,
    recorder: Option<Arc<Recorder>>,
    output: Option<Arc<voice::SwitchableSink>>,
    /// Open device list: direction and devices found when it was opened.
    device_menu: Option<(voice::Direction, Vec<voice::AudioDevice>)>,
    settings: Settings,
    language: voice::Language,
    planner: Option<Planner>,
    capture: Option<crossbeam_channel::Receiver<CaptureResult>>,
    recording: bool,
    voice_latency: Option<VoiceLatency>,
    snap: LiveSnapshot,
    coach_status: RuntimeStatus,
    speaker_status: SpeakerStatus,
    reference: ReferenceStatus,
    plan: PlanStatus,
    comparisons: Comparisons,
    _refresh: Task<()>,
}

impl CoachView {
    fn new(services: Services, options: StartOptions, cx: &mut Context<Self>) -> Self {
        let Services {
            hub,
            triggers: coach,
            speaker,
            agent,
            mcp_url,
            mcp_error,
            tool_log,
            keep_alive,
            conversation,
            input,
            listener,
            recorder,
            output,
            notes,
            settings,
            language,
            planner,
        } = services;
        let refresh = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH).await;
                let alive = this.update(cx, |view, cx| {
                    view.snap = view.hub.snapshot();
                    view.coach_status = view.coach.status();
                    view.speaker_status = view.speaker.status();
                    while let Ok(e) = view.conversation.events().try_recv() {
                        view.apply_conversation(e);
                    }
                    if let Some(rx) = &view.capture
                        && let Ok(result) = rx.try_recv()
                    {
                        view.capture = None;
                        if let CaptureResult::Bound(b) = result {
                            view.settings.ptt = Some(b.clone());
                            if let Some(i) = &view.input {
                                i.set_ptt(Some(b));
                            }
                            if let Err(e) = view.settings.save() {
                                view.chat.system_message(format!("Settings not saved: {e}"));
                            }
                        }
                    }
                    let tools: Vec<ToolCallLog> =
                        view.tool_log.lock().unwrap().iter().cloned().collect();
                    view.chat.sync_tools(&tools);
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
            coach_status: coach.status(),
            speaker_status: speaker.status(),
            hub,
            coach,
            speaker,
            agent,
            mcp_url,
            mcp_error,
            tool_log,
            chat: Chat::default(),
            input_focus: cx.focus_handle(),
            _keep_alive: keep_alive,
            conversation,
            input,
            listener,
            recorder,
            output,
            device_menu: None,
            settings,
            language,
            planner,
            capture: None,
            recording: false,
            voice_latency: None,
            reference: ReferenceStatus::None,
            plan: PlanStatus::None,
            comparisons: Comparisons::default(),
            _refresh: refresh,
        };
        match options.reference {
            Some(ReferenceSource::File(path)) => view.load_reference(path, cx),
            Some(ReferenceSource::Preloaded(r)) => {
                let state = ReferenceState::new(*r, None);
                view.set_reference(state);
            }
            None => {}
        }
        if let Some(path) = options.plan {
            view.load_plan(path, cx);
        }
        for n in notes {
            view.chat.system_message(n);
        }
        view
    }

    fn toggle_device_menu(&mut self, direction: voice::Direction) {
        self.device_menu = match &self.device_menu {
            Some((d, _)) if *d == direction => None,
            _ => Some((direction, voice::devices::list(direction))),
        };
    }

    /// Switches the microphone or the output to `device` (`None`: system
    /// default) and saves the choice if it worked.
    fn select_device(&mut self, direction: voice::Direction, device: Option<voice::AudioDevice>) {
        self.device_menu = None;
        let id = device.as_ref().map(|d| d.id.as_str());
        let result = match direction {
            voice::Direction::Input => match &self.recorder {
                Some(r) => r.select(id).map(|_| ()),
                None => Ok(()),
            },
            voice::Direction::Output => match &self.output {
                Some(o) => voice::CpalSink::open_device(id).map(|sink| o.replace(Arc::new(sink))),
                None => Ok(()),
            },
        };
        if let Err(e) = result {
            self.chat.system_message(format!("Device not changed: {e}"));
            return;
        }
        let choice = device.map(|d| settings::DeviceChoice {
            id: d.id,
            name: d.name,
        });
        match direction {
            voice::Direction::Input => self.settings.microphone = choice,
            voice::Direction::Output => self.settings.output = choice,
        }
        if let Err(e) = self.settings.save() {
            self.chat.system_message(format!("Settings not saved: {e}"));
        }
    }

    fn send_message(&mut self) {
        let text = self.chat.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        if self.agent.is_none() {
            self.chat
                .system_message("No agent: start without --no-agent.");
            return;
        }
        self.conversation.send_text(text);
        self.chat.input.clear();
    }

    fn apply_conversation(&mut self, e: ConvEvent) {
        match e {
            ConvEvent::Agent(e) => self.chat.apply(e),
            ConvEvent::Driver { turn, text, voice } => {
                self.chat.driver_message(&text, turn);
                if voice {
                    self.voice_latency = None;
                }
            }
            ConvEvent::Recording(r) => self.recording = r,
            ConvEvent::NotUnderstood => self.chat.system_message("Didn't catch that, say again."),
            ConvEvent::Latency(l) => self.voice_latency = Some(l),
        }
    }

    fn start_capture(&mut self) {
        if let Some(i) = &self.input {
            self.capture = Some(i.capture());
        }
    }

    fn toggle_auto_planning(&mut self) {
        self.settings.auto_planning = !self.settings.auto_planning;
        if let Some(p) = &self.planner {
            p.set_enabled(self.settings.auto_planning);
        }
        let _ = self.settings.save();
    }

    fn cycle_language(&mut self) {
        self.settings.language = self.settings.language.next();
        let _ = self.settings.save();
    }

    fn toggle_speak_replies(&mut self) {
        self.settings.speak_replies = !self.settings.speak_replies;
        self.conversation
            .set_speak_replies(self.settings.speak_replies);
        let _ = self.settings.save();
    }

    fn on_key(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let ctrl = ks.modifiers.control || ks.modifiers.platform;
        match ks.key.as_str() {
            "enter" => self.send_message(),
            "escape" => self.conversation.cancel(),
            "backspace" => {
                self.chat.input.pop();
            }
            "v" if ctrl => {
                if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                    self.chat.input.push_str(&text.replace(['\n', '\r'], " "));
                }
            }
            _ if !ctrl => {
                if let Some(c) = &ks.key_char {
                    self.chat.input.push_str(c);
                }
            }
            _ => {}
        }
        cx.notify();
    }

    fn set_reference(&mut self, state: ReferenceState) {
        self.comparisons.clear();
        self.comparisons.update(&self.snap, &state.analyzer);
        self.coach.set_reference(Some(state.analyzer.clone()));
        self.reference = ReferenceStatus::Ready(Box::new(state));
    }

    fn pick_plan(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Cue plan (.toml)".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(path) = paths.into_iter().next()
            {
                let _ = this.update(cx, |view, cx| view.load_plan(path, cx));
            }
        })
        .detach();
    }

    fn load_plan(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.plan = match Plan::load(&path) {
            Ok(plan) => {
                self.coach.set_plan(name.clone(), plan);
                PlanStatus::Loaded(name)
            }
            Err(e) => PlanStatus::Failed(format!("{name}: {e}")),
        };
        cx.notify();
    }

    fn pick_reference(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Reference lap (Garage61 .csv or .ibt)".into()),
        });
        cx.spawn(async move |this, cx| match rx.await {
            Ok(Ok(Some(paths))) => {
                if let Some(path) = paths.into_iter().next() {
                    let _ = this.update(cx, |view, cx| view.load_reference(path, cx));
                }
            }
            Ok(Err(e)) => {
                let _ = this.update(cx, |view, cx| {
                    view.reference = ReferenceStatus::Failed(format!("file picker: {e}"));
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
            reference::load(&path).map(|r| ReferenceState::new(r, session_length))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |view, cx| {
                match result {
                    Ok(state) => view.set_reference(state),
                    Err(e) => view.reference = ReferenceStatus::Failed(e.to_string()),
                }
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
                .child(format!("{:.0}%", v * 100.0)),
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

    fn render_stt_pill(&self) -> gpui::Div {
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

    fn render_voice_settings(&self, cx: &mut Context<Self>) -> gpui::Div {
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
                            .child("Talk:"),
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

    fn render_device_row(
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
    fn render_device_menu(&self, cx: &mut Context<Self>) -> Option<gpui::Div> {
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

    fn render_agent_pill(&self) -> gpui::Div {
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

    fn render_chat(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entries = self.chat.entries.iter().rev().take(40).rev().map(|e| {
            let (who, tone) = match e.who {
                Who::Driver => ("Driver", theme::BEST),
                Who::Coach => ("Coach", theme::TEXT),
                Who::Tool => ("tool", theme::MUTED),
                Who::System => ("", theme::WARN),
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
                        .w(px(48.))
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
            .child(self.render_voice_settings(cx))
            .child(
                div()
                    .id("chat-log")
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

    fn render_tts_pill(&self) -> gpui::Div {
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
                    .child(format!("Source: {}", s.source)),
            )
            .child(self.render_reference(cx))
            .child(self.render_plan_line(cx))
    }

    fn render_plan_line(&self, cx: &mut Context<Self>) -> gpui::Div {
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

    fn render_planner(&self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let p = self.planner.as_ref()?;
        let st = p.status();
        let (line, tone) = if !st.enabled {
            ("Automatic planning disabled".to_string(), theme::MUTED)
        } else if let Some(lap) = st.running {
            (format!("Analyzing lap {lap} ({})…", st.model), theme::WARN)
        } else if let Some(e) = &st.error {
            (format!("Analysis: {e}"), theme::ERR)
        } else if let Some((lap, summary, secs)) = &st.last {
            (
                format!("Lap {lap} analyzed in {secs:.0} s: {summary}"),
                theme::TEXT,
            )
        } else {
            (
                format!("Analysis ({}) after the next valid lap", st.model),
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
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_color(color(tone))
                        .child(line),
                )
                .child(toggle),
        )
    }

    fn render_coach(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let st = &self.coach_status;
        let rows = st.triggers.iter().map(|v| {
            let (state, tone) = match v.state {
                TriggerState::Waiting => ("waiting".to_string(), theme::MUTED),
                TriggerState::Queued => ("queued".to_string(), theme::WARN),
                TriggerState::Spoken => (
                    format!(
                        "spoken ×{} (lap {})",
                        v.times_spoken,
                        v.last_spoken_lap.unwrap_or(0)
                    ),
                    theme::OK,
                ),
                TriggerState::Expired => (format!("done ×{}", v.times_spoken), theme::IDLE),
            };
            let coach = v.trigger.id.starts_with(triggers::COACH_PREFIX);
            let place = v
                .trigger
                .corner
                .clone()
                .unwrap_or_else(|| format!("{:.1}%", v.trigger.at_pct * 100.0));
            let place = if coach { format!("{place} ·C") } else { place };
            div()
                .flex()
                .gap_2()
                .py(px(2.))
                .text_sm()
                .child(div().w(px(56.)).whitespace_nowrap().child(place))
                .child(
                    div()
                        .w(px(140.))
                        .whitespace_nowrap()
                        .text_color(color(tone))
                        .child(state),
                )
                .child(
                    div()
                        .flex_1()
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(v.trigger.text.clone()),
                )
        });
        let log = st.log.iter().rev().take(2).map(|e| {
            div()
                .flex()
                .gap_2()
                .text_sm()
                .child(
                    div()
                        .w(px(96.))
                        .whitespace_nowrap()
                        .text_color(color(theme::MUTED))
                        .child(format!("Lap {} · {:.0}%", e.lap, e.pct * 100.0)),
                )
                .child(
                    div()
                        .flex_1()
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(format!("Coach: {}", e.text)),
                )
        });
        let quiet = st.quiet_now.then(|| {
            div()
                .text_xs()
                .text_color(color(theme::WARN))
                .child("quiet: braking zone")
        });
        let planner = self.render_planner(cx);
        panel("Cues")
            .flex_1()
            .min_w_0()
            .children(planner)
            .children(quiet)
            .child(
                div()
                    .id("triggers")
                    .h(px(80.))
                    .flex_none()
                    .flex()
                    .flex_col()
                    .overflow_y_scroll()
                    .children(rows),
            )
            .child(
                div()
                    .pt_2()
                    .border_t_1()
                    .border_color(color(theme::BORDER))
                    .text_xs()
                    .text_color(color(theme::MUTED))
                    .child("LOG"),
            )
            .children(log)
    }

    fn render_reference(&self, cx: &mut Context<Self>) -> gpui::Div {
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
            format!("{:.1}%", smp.lap_dist_pct * 100.0)
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
                    .child("Δ ref"),
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
        panel("On track")
            .child(
                div()
                    .flex()
                    .gap_6()
                    .child(big_value("Lap", lap))
                    .child(big_value("Time", elapsed))
                    .child(delta_box)
                    .child(big_value("Position", pct))
                    .child(big_value("km/h", format::kmh(smp.speed)))
                    .child(big_value("Gear", format::gear(smp.gear)))
                    .child(big_value(
                        "rpm",
                        if smp.rpm.is_finite() {
                            format!("{:.0}", smp.rpm)
                        } else {
                            "–".into()
                        },
                    )),
            )
            .child(bar("Throttle", smp.throttle, theme::THROTTLE))
            .child(bar("Brake", smp.brake, theme::BRAKE))
    }

    fn render_laps(&self) -> impl IntoElement {
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
                (50., "Corner".into()),
                (68., "Δ last".into()),
                (72., "Δ best".into()),
                (90., "Min V / ref".into()),
                (56., "Brake".into()),
                (64., "Throttle".into()),
                (56., "Trail".into()),
                (88., "Rot. in/out".into()),
                (72., "Coasting".into()),
                (120., "Cue".into()),
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
                .child(
                    div()
                        .w(px(120.))
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
                    .child("Brake/Throttle: + later than the reference. Rotation: < 1 less rotation for the same steering."),
            )
    }

    fn render_detected_corners(&self) -> impl IntoElement {
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

impl Render for CoachView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
                    .flex_1()
                    .gap_3()
                    .min_h_0()
                    // Left column: session, reference, plan, laps.
                    .child(
                        div()
                            .w(px(380.))
                            .flex()
                            .flex_col()
                            .gap_3()
                            .min_h_0()
                            .child(self.render_session(cx))
                            .child(div().h(px(150.)).flex().child(self.render_laps()))
                            .child(
                                div()
                                    .flex_1()
                                    .flex()
                                    .min_h_0()
                                    .child(self.render_chat(window, cx)),
                            ),
                    )
                    // Right column: live, corners, cues.
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .min_h_0()
                            .min_w_0()
                            .child(self.render_live())
                            .child(div().flex_1().flex().min_h_0().child(self.render_corners()))
                            .child(div().h(px(262.)).flex().child(self.render_coach(cx))),
                    ),
            )
    }
}
