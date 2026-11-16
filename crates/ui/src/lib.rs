//! GPUI interface of the coach, in three screens:
//!
//! - **Coaching**: compact, meant to stay on top of the game: coach state
//!   lights, voice levels, live delta, per-corner analysis and cues;
//! - **Chat**: conversation with the coach, its analyses, tool calls and the
//!   detailed corner metrics;
//! - **Setup**: session, reference, plan, voice, devices and connections.
//!
//! The UI only reads the snapshot published by the telemetry hub, at ~30 Hz;
//! it never blocks the fast loop.

pub mod chat;
pub mod conversation;
pub mod format;
mod panels;
pub mod planner;
mod screens;
pub mod settings;
pub mod state;
mod widgets;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use coach::CoachAgent;
use gpui::{
    App, Bounds, Context, FocusHandle, IntoElement, KeyDownEvent, PathPromptOptions, Render,
    SharedString, Task, TitlebarOptions, Window, WindowBounds, WindowOptions, div, prelude::*, px,
    size,
};
use mcp::ToolCallLog;
use reference::ReferenceLap;
use telemetry::{LiveSnapshot, TelemetryHub};
use triggers::{Plan, RuntimeStatus, TriggerRuntime};
use voice::{Speaker, SpeakerStatus};

use crate::chat::Chat;
use crate::conversation::{ConvEvent, Conversation, VoiceLatency};
use crate::planner::Planner;
use crate::settings::Settings;
use crate::state::{Comparisons, ReferenceState};
use crate::widgets::{color, theme};
use input::{CaptureResult, InputService};
use voice::{Listener, Recorder};

const REFRESH: Duration = Duration::from_millis(33);
/// Level history shown by the meters (~2 s at the refresh rate).
const WAVE_LEN: usize = 60;
/// Window size of the pinned (always on top) coaching screen.
const COMPACT: (f32, f32) = (460., 680.);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Screen {
    Coaching,
    Chat,
    Setup,
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
        let bounds = Bounds::centered(None, size(px(1180.), px(820.)), cx);
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Activity {
    pub listening: bool,
    pub thinking: bool,
    pub speaking: bool,
    pub analyzing: bool,
}

impl Activity {
    pub fn idle(&self) -> bool {
        !(self.listening || self.thinking || self.speaking || self.analyzing)
    }
}

/// Window placement that GPUI does not expose.
mod platform {
    use gpui::Window;

    /// Always-on-top: Win32 `SetWindowPos(HWND_TOPMOST)`.
    #[cfg(windows)]
    pub fn set_topmost(window: &Window, on: bool) -> Result<(), String> {
        topmost(window, on)
    }

    #[cfg(windows)]
    fn topmost(window: &impl raw_window_handle::HasWindowHandle, on: bool) -> Result<(), String> {
        use raw_window_handle::RawWindowHandle;
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{
            HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowPos,
        };
        let handle = window.window_handle().map_err(|e| e.to_string())?;
        let RawWindowHandle::Win32(h) = handle.as_raw() else {
            return Err("not a Win32 window".into());
        };
        let hwnd = HWND(h.hwnd.get() as *mut core::ffi::c_void);
        let after = if on { HWND_TOPMOST } else { HWND_NOTOPMOST };
        // SAFETY: the handle comes from the live window, used on its thread.
        unsafe {
            SetWindowPos(
                hwnd,
                Some(after),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            )
        }
        .map_err(|e| e.to_string())
    }

    #[cfg(not(windows))]
    pub fn set_topmost(_window: &Window, _on: bool) -> Result<(), String> {
        Err("only available on Windows".into())
    }

    pub const SUPPORTED: bool = cfg!(windows);
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
    screen: Screen,
    /// Recent levels (0..1) of the coach's voice and of the microphone.
    coach_wave: VecDeque<f32>,
    mic_wave: VecDeque<f32>,
    /// Chat log scroll, kept at the bottom when something new arrives.
    chat_scroll: gpui::ScrollHandle,
    chat_seen: (usize, usize),
    /// Last analysis already added to the chat.
    analysis_seen: Option<i32>,
    /// The saved "always on top" setting still has to be applied to the window.
    apply_on_top: bool,
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
                    view.sample_levels();
                    view.sync_analysis();
                    let seen = (
                        view.chat.entries.len(),
                        view.chat.entries.last().map(|e| e.text.len()).unwrap_or(0),
                    );
                    if seen != view.chat_seen {
                        view.chat_seen = seen;
                        view.chat_scroll.scroll_to_bottom();
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
            screen: Screen::Coaching,
            coach_wave: VecDeque::from(vec![0.0; WAVE_LEN]),
            mic_wave: VecDeque::from(vec![0.0; WAVE_LEN]),
            analysis_seen: None,
            chat_scroll: gpui::ScrollHandle::new(),
            chat_seen: (0, 0),
            apply_on_top: false,
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
        view.apply_on_top = view.settings.always_on_top;
        view
    }

    fn sample_levels(&mut self) {
        let push = |wave: &mut VecDeque<f32>, v: f32| {
            wave.push_back(if v.is_finite() {
                v.clamp(0.0, 1.0)
            } else {
                0.0
            });
            while wave.len() > WAVE_LEN {
                wave.pop_front();
            }
        };
        push(&mut self.coach_wave, self.speaker.level());
        let mic = self.recorder.as_ref().map(|r| r.level()).unwrap_or(0.0);
        push(&mut self.mic_wave, mic);
    }

    /// Adds each new analysis summary to the chat, once.
    fn sync_analysis(&mut self) {
        let Some(p) = &self.planner else { return };
        if let Some((lap, summary, secs)) = p.status().last
            && self.analysis_seen != Some(lap)
        {
            self.analysis_seen = Some(lap);
            self.chat.analysis_message(lap, &summary, secs);
        }
    }

    /// What the coach is doing now, for the state lights.
    pub(crate) fn coach_activity(&self) -> Activity {
        Activity {
            listening: self.recording,
            thinking: self.chat.busy(),
            speaking: self.speaker.is_speaking(),
            analyzing: self
                .planner
                .as_ref()
                .is_some_and(|p| p.status().running.is_some()),
        }
    }

    /// Pins the window on top of the game, in a compact size; unpinning
    /// keeps the size.
    fn set_on_top(&mut self, on: bool, window: &mut Window) {
        match platform::set_topmost(window, on) {
            Ok(()) => {
                if on {
                    window.resize(size(px(COMPACT.0), px(COMPACT.1)));
                    self.screen = Screen::Coaching;
                }
                self.settings.always_on_top = on;
                let _ = self.settings.save();
            }
            Err(e) => self.chat.system_message(format!("Always on top: {e}")),
        }
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

impl Render for CoachView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.apply_on_top) {
            cx.defer_in(window, |view, window, cx| {
                view.set_on_top(true, window);
                cx.notify();
            });
        }
        let body = match self.screen {
            Screen::Coaching => self.render_coaching_screen(cx).into_any_element(),
            Screen::Chat => self.render_chat_screen(window, cx).into_any_element(),
            Screen::Setup => self.render_setup_screen(cx).into_any_element(),
        };
        div()
            .flex()
            .flex_col()
            .size_full()
            .gap_2()
            .p_2()
            .bg(color(theme::BG))
            .text_color(color(theme::TEXT))
            .child(self.render_tabs(cx))
            .child(div().flex_1().min_h_0().flex().flex_col().child(body))
    }
}
