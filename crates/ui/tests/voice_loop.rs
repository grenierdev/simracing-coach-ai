//! Full voice loop without hardware: push-to-talk press (simulated source),
//! recording (simulated microphone), transcription, in-process ACP agent,
//! reply spoken sentence by sentence through the cue engine.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    AgentCapabilities, ContentBlock, ContentChunk, InitializeRequest, InitializeResponse,
    NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse, SessionNotification,
    SessionUpdate, StopReason, TextContent,
};
use agent_client_protocol::{Agent, Client, Responder, V1};
use coach::{AgentConfig, CoachAgent};
use input::{Backend, Binding, InputService};
use telemetry::synthetic::SyntheticTrack;
use telemetry::{IbtFile, ReplaySource, TelemetryHub, TelemetryHubConfig};
use triggers::{SpeechOutput, TriggerRuntime};
use ui::conversation::{ConvEvent, Conversation, ConversationDeps};
use voice::{Audio, Listener, Microphone, NullSink, SilentTts, Speaker, Stt, Tts, VoiceError};

struct FakeKeys(Arc<Mutex<HashSet<Binding>>>);
impl Backend for FakeKeys {
    fn poll(&mut self, out: &mut HashSet<Binding>) {
        out.extend(self.0.lock().unwrap().iter().cloned());
    }
}

struct FakeMic;
impl Microphone for FakeMic {
    fn start(&self) {}
    fn stop(&self) -> Audio {
        Audio {
            samples: vec![0.0; 16_000],
            sample_rate: 16_000,
        }
    }
    fn level(&self) -> f32 {
        0.0
    }
}

struct FixedStt;
impl Stt for FixedStt {
    fn name(&self) -> String {
        "fixed".into()
    }
    fn transcribe(&mut self, _: &Audio) -> Result<String, VoiceError> {
        Ok("no rotation in T5".into())
    }
}

struct Out(Arc<Speaker>);
impl SpeechOutput for Out {
    fn say(&mut self, t: &str) {
        self.0.say(t)
    }
    fn stop(&mut self) {
        self.0.stop()
    }
    fn is_speaking(&self) -> bool {
        self.0.is_speaking()
    }
    fn duration(&mut self, t: &str) -> f64 {
        self.0.duration(t)
    }
}

fn mock_agent(prompts: Arc<Mutex<Vec<String>>>) -> impl agent_client_protocol::ConnectTo<Client> {
    Agent
        .v1()
        .on_receive_request(
            async move |req: InitializeRequest,
                        r: Responder<InitializeResponse, V1>,
                        _cx: ConnectionTo<Client>| {
                r.respond(
                    InitializeResponse::new(req.protocol_version)
                        .agent_capabilities(AgentCapabilities::new()),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_req: NewSessionRequest,
                        r: Responder<NewSessionResponse, V1>,
                        _cx: ConnectionTo<Client>| {
                r.respond(NewSessionResponse::new("s"))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: PromptRequest,
                        r: Responder<PromptResponse, V1>,
                        cx: ConnectionTo<Client>| {
                if let Some(ContentBlock::Text(t)) = req.prompt.first() {
                    prompts.lock().unwrap().push(t.text.clone());
                }
                let sid = req.session_id.clone();
                tokio::spawn(async move {
                    for part in ["Looking at ", "T5. Release the brake ", "later."] {
                        let u = SessionUpdate::AgentMessageChunk(ContentChunk::new(
                            ContentBlock::Text(TextContent::new(part)),
                        ));
                        let _ = cx.send_notification(SessionNotification::new(sid.clone(), u));
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    let _ = r.respond(PromptResponse::new(StopReason::EndTurn));
                });
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
}

#[test]
fn push_to_talk_round_trip() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let prompts: Arc<Mutex<Vec<String>>> = Arc::default();
    let cfg = AgentConfig {
        label: "conversation".into(),
        command: vec![],
        model: None,
        disable_thinking: true,
        env: vec![],
        cwd: std::env::temp_dir(),
        system_prompt: String::new(),
        mcp: None,
    };
    let agent = Arc::new(CoachAgent::spawn_with(
        mock_agent(prompts.clone()),
        cfg,
        rt.handle(),
    ));

    let file = IbtFile::from_bytes(SyntheticTrack::demo().session_ibt(2, 1).to_bytes()).unwrap();
    let hub = TelemetryHub::spawn(
        Box::new(ReplaySource::new(file, "test", 1.0, true)),
        TelemetryHubConfig::default(),
    );
    let speaker = Arc::new(Speaker::spawn(
        |_| Ok(Box::new(SilentTts::default()) as Box<dyn Tts>),
        Arc::new(NullSink::default()),
    ));
    let triggers = Arc::new(TriggerRuntime::spawn(
        hub.subscribe_samples(),
        Box::new(Out(speaker.clone())),
    ));

    let keys: Arc<Mutex<HashSet<Binding>>> = Arc::default();
    let k = keys.clone();
    let input = Arc::new(InputService::spawn_with(move || {
        vec![Box::new(FakeKeys(k)) as Box<dyn Backend>]
    }));
    let f9 = Binding::Key { key: "F9".into() };
    input.set_ptt(Some(f9.clone()));

    let conv = Conversation::spawn(
        ConversationDeps {
            agent: Some(agent),
            speaker: speaker.clone(),
            triggers: triggers.clone(),
            snapshot: hub.snapshot_handle(),
            input: Some(input),
            recorder: Some(Arc::new(FakeMic)),
            listener: Some(Arc::new(Listener::spawn(|_| {
                Ok(Box::new(FixedStt) as Box<dyn Stt>)
            }))),
        },
        true,
    );

    let mut log = vec![];
    let wait = |pred: &dyn Fn(&ConvEvent) -> bool, log: &mut Vec<ConvEvent>| {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if let Ok(e) = conv.events().recv_timeout(Duration::from_millis(50)) {
                let ok = pred(&e);
                log.push(e);
                if ok {
                    return;
                }
            }
        }
        panic!("expected event not received: {log:#?}");
    };
    wait(
        &|e| {
            matches!(
                e,
                ConvEvent::Agent(coach::CoachEvent::Status(coach::AgentStatus::Ready { .. }))
            )
        },
        &mut log,
    );

    keys.lock().unwrap().insert(f9.clone());
    wait(&|e| *e == ConvEvent::Recording(true), &mut log);
    std::thread::sleep(Duration::from_millis(100));
    keys.lock().unwrap().clear();
    wait(&|e| *e == ConvEvent::Recording(false), &mut log);
    wait(
        &|e| matches!(e, ConvEvent::Driver { voice: true, .. }),
        &mut log,
    );
    wait(
        &|e| matches!(e, ConvEvent::Latency(l) if l.first_sound_ms.is_some()),
        &mut log,
    );

    let p = prompts.lock().unwrap().clone();
    assert_eq!(p.len(), 1);
    assert!(p[0].starts_with("[Context] lap"), "{}", p[0]);
    assert!(p[0].ends_with("[Driver] no rotation in T5"), "{}", p[0]);

    let lat = log
        .iter()
        .rev()
        .find_map(|e| match e {
            ConvEvent::Latency(l) => Some(*l),
            _ => None,
        })
        .unwrap();
    let (t, w, s) = (
        lat.transcript_ms.unwrap(),
        lat.first_word_ms.unwrap(),
        lat.first_sound_ms.unwrap(),
    );
    assert!(t <= w && w <= s, "{lat:?}");

    // Both sentences go through the cue engine, in order.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let said: Vec<String> = triggers
            .status()
            .log
            .iter()
            .map(|e| e.text.clone())
            .collect();
        if said.len() >= 2 {
            assert_eq!(said, vec!["Looking at T5.", "Release the brake later."]);
            break;
        }
        assert!(Instant::now() < deadline, "spoken sentences: {said:?}");
        std::thread::sleep(Duration::from_millis(100));
    }
}
