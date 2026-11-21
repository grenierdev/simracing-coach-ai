//! Conversation loop, outside the UI: push-to-talk, microphone, speech
//! recognition, Claude, reply spoken sentence by sentence.
//!
//! - press: the coach goes quiet, the current reply is interrupted, the microphone records;
//! - release: the recording goes to transcription;
//! - recognized text: sent to Claude with the driver context;
//! - each sentence of the reply goes through the cue engine (`say`), hence
//!   through the quiet-while-braking rule.
//!
//! Latency measured per voice turn: end of speech → text → Claude's first
//! word → first sound.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use coach::{AgentStatus, CoachAgent, CoachEvent, SentenceSplitter, format_prompt};
use crossbeam_channel::{Receiver, Sender, select, unbounded};
use input::{InputService, PttEvent};
use telemetry::SnapshotHandle;
use triggers::{SayWhen, TriggerRuntime};
use voice::{Listener, Microphone, Speaker};

use crate::chat::driver_context;

/// Latency of a voice turn, ms since the end of speech.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct VoiceLatency {
    pub transcript_ms: Option<f64>,
    pub first_word_ms: Option<f64>,
    pub first_sound_ms: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConvEvent {
    Agent(CoachEvent),
    /// Driver message sent to Claude (`voice`: dictated).
    Driver {
        turn: u64,
        text: String,
        voice: bool,
    },
    Recording(bool),
    /// Nothing intelligible in the recording.
    NotUnderstood,
    Latency(VoiceLatency),
}

pub struct ConversationDeps {
    pub agent: Option<Arc<CoachAgent>>,
    pub speaker: Arc<Speaker>,
    pub triggers: Arc<TriggerRuntime>,
    pub snapshot: SnapshotHandle,
    pub input: Option<Arc<InputService>>,
    pub recorder: Option<Arc<dyn Microphone>>,
    pub listener: Option<Arc<Listener>>,
    /// Turn-by-turn coaching state, for the driver context.
    pub coaching: Option<mcp::CoachingLink>,
}

pub struct Conversation {
    events: Receiver<ConvEvent>,
    typed: Sender<String>,
    cancel: Sender<()>,
    speak_replies: Arc<AtomicBool>,
    pub deps: Arc<ConversationDeps>,
}

impl Conversation {
    pub fn spawn(deps: ConversationDeps, speak_replies: bool) -> Self {
        let deps = Arc::new(deps);
        let (etx, erx) = unbounded();
        let (ttx, trx) = unbounded();
        let (ctx, crx) = unbounded();
        let speak = Arc::new(AtomicBool::new(speak_replies));
        {
            let (deps, speak) = (deps.clone(), speak.clone());
            std::thread::Builder::new()
                .name("conversation".into())
                .spawn(move || run(deps, etx, trx, crx, speak))
                .expect("starting the conversation thread");
        }
        Conversation {
            events: erx,
            typed: ttx,
            cancel: ctx,
            speak_replies: speak,
            deps,
        }
    }

    pub fn events(&self) -> &Receiver<ConvEvent> {
        &self.events
    }

    /// Message typed on the keyboard.
    pub fn send_text(&self, text: impl Into<String>) {
        let _ = self.typed.send(text.into());
    }

    /// Interrupts the current reply and the coach's speech.
    pub fn cancel(&self) {
        let _ = self.cancel.send(());
    }

    pub fn set_speak_replies(&self, on: bool) {
        self.speak_replies.store(on, Ordering::Relaxed);
    }
}

struct VoiceTurn {
    turn: u64,
    released: Instant,
    latency: VoiceLatency,
    first_word_at: Option<Instant>,
}

fn run(
    deps: Arc<ConversationDeps>,
    out: Sender<ConvEvent>,
    typed: Receiver<String>,
    cancel: Receiver<()>,
    speak: Arc<AtomicBool>,
) {
    let ptt_rx = deps
        .input
        .as_ref()
        .map(|i| i.ptt_events().clone())
        .unwrap_or_else(crossbeam_channel::never);
    let stt_rx = deps
        .listener
        .as_ref()
        .map(|l| l.results().clone())
        .unwrap_or_else(crossbeam_channel::never);
    let agent_rx = deps
        .agent
        .as_ref()
        .map(|a| a.events().clone())
        .unwrap_or_else(crossbeam_channel::never);

    let mut recording_id: u64 = 0;
    let mut pending_release: Option<(u64, Instant)> = None;
    let mut voice_turn: Option<VoiceTurn> = None;
    let mut splitter = SentenceSplitter::default();
    let mut busy = false;

    let send_prompt = |text: &str| -> Option<u64> {
        let agent = deps.agent.as_ref()?;
        let snap = deps.snapshot.get();
        let analyzer = deps.triggers.analyzer();
        let status = deps.triggers.status();
        let turns = deps
            .coaching
            .as_ref()
            .and_then(|c| c.lock().unwrap().line());
        let ctx = driver_context(&snap, analyzer.as_deref(), &status, turns);
        Some(agent.prompt(format_prompt(&ctx, text)))
    };
    let interrupt = |busy: bool| {
        deps.speaker.stop();
        if busy && let Some(a) = &deps.agent {
            a.cancel();
        }
    };

    loop {
        select! {
            recv(ptt_rx) -> e => match e {
                Ok(PttEvent::Pressed(_)) => {
                    interrupt(busy);
                    splitter = SentenceSplitter::default();
                    if let Some(r) = &deps.recorder {
                        r.start();
                        let _ = out.send(ConvEvent::Recording(true));
                    }
                }
                Ok(PttEvent::Released(t)) => {
                    if let (Some(r), Some(l)) = (&deps.recorder, &deps.listener) {
                        let audio = r.stop();
                        recording_id += 1;
                        pending_release = Some((recording_id, t));
                        l.transcribe(recording_id, audio);
                        let _ = out.send(ConvEvent::Recording(false));
                    }
                }
                Err(_) => break,
            },
            recv(stt_rx) -> t => match t {
                Ok(t) => {
                    let released = match pending_release {
                        Some((id, at)) if id == t.id => at,
                        _ => Instant::now(),
                    };
                    if t.text.trim().is_empty() {
                        let _ = out.send(ConvEvent::NotUnderstood);
                        continue;
                    }
                    let latency = VoiceLatency {
                        transcript_ms: Some(released.elapsed().as_secs_f64() * 1e3),
                        ..Default::default()
                    };
                    if let Some(turn) = send_prompt(&t.text) {
                        voice_turn = Some(VoiceTurn { turn, released, latency, first_word_at: None });
                        let _ = out.send(ConvEvent::Driver { turn, text: t.text.clone(), voice: true });
                        let _ = out.send(ConvEvent::Latency(latency));
                    }
                }
                Err(_) => break,
            },
            recv(typed) -> t => match t {
                Ok(text) => {
                    if let Some(turn) = send_prompt(&text) {
                        let _ = out.send(ConvEvent::Driver { turn, text, voice: false });
                    }
                }
                Err(_) => break,
            },
            recv(cancel) -> c => {
                if c.is_err() { break; }
                interrupt(busy);
            },
            recv(agent_rx) -> e => match e {
                Ok(e) => {
                    match &e {
                        CoachEvent::Status(s) => busy = matches!(s, AgentStatus::Busy),
                        CoachEvent::Chunk { turn, text } => {
                            if let Some(v) = voice_turn.as_mut()
                                && v.turn == *turn
                                && v.first_word_at.is_none()
                            {
                                v.first_word_at = Some(Instant::now());
                                v.latency.first_word_ms = Some(v.released.elapsed().as_secs_f64() * 1e3);
                                let _ = out.send(ConvEvent::Latency(v.latency));
                            }
                            if speak.load(Ordering::Relaxed) {
                                for s in splitter.push(text) {
                                    deps.triggers.say(s, SayWhen::Now);
                                }
                            }
                        }
                        CoachEvent::TurnEnd { .. } => {
                            if speak.load(Ordering::Relaxed) && let Some(rest) = splitter.finish() {
                                deps.triggers.say(rest, SayWhen::Now);
                            }
                        }
                        _ => {}
                    }
                    let _ = out.send(ConvEvent::Agent(e));
                }
                Err(_) => break,
            },
            default(Duration::from_millis(20)) => {}
        }
        // First sound of the voice turn: first audio enqueue after the first word.
        if let Some(v) = voice_turn.as_mut()
            && v.latency.first_sound_ms.is_none()
            && let (Some(fw), Some(played)) = (v.first_word_at, deps.speaker.status().last_play_at)
            && played >= fw
        {
            v.latency.first_sound_ms = Some(played.duration_since(v.released).as_secs_f64() * 1e3);
            let _ = out.send(ConvEvent::Latency(v.latency));
        }
    }
}

/// Last known latency, for display.
pub type SharedLatency = Arc<Mutex<Option<VoiceLatency>>>;
