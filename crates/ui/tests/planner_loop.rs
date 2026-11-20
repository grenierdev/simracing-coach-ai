//! End-to-end focus coaching: lap completed → the race engineer (fake agent)
//! chooses a corner through the real MCP server (`set_focus`, `update_plan`)
//! → a pass through the corner is analyzed right after it → the attempt goes
//! back to the race engineer → `next_focus` starts a new choice.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    AgentCapabilities, ContentBlock, ContentChunk, InitializeRequest, InitializeResponse,
    McpServer, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    SessionNotification, SessionUpdate, StopReason, TextContent,
};
use agent_client_protocol::{Agent, Client, Responder, V1};
use analysis::Analyzer;
use coach::{AgentConfig, CoachAgent, McpEndpoint};
use mcp::{CoachData, FocusCommand, FocusLink, FocusPhase, McpServerHandle};
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use telemetry::synthetic::SyntheticTrack;
use telemetry::{CornerDetectionParams, Lap, LapFlags, LiveSnapshot, TelemetryEvent, TrackSurface};
use triggers::{Plan, SayWhen, SpeechOutput, TriggerRuntime};
use ui::planner::{Planner, PlannerDeps, PlannerPolicy};

struct Mute;
impl SpeechOutput for Mute {
    fn say(&mut self, _: &str) {}
    fn stop(&mut self) {}
    fn is_speaking(&self) -> bool {
        false
    }
    fn duration(&mut self, _: &str) -> f64 {
        1.0
    }
}

struct Data {
    snap: LiveSnapshot,
    triggers: Arc<TriggerRuntime>,
    focus: FocusLink,
}
impl CoachData for Data {
    fn snapshot(&self) -> LiveSnapshot {
        self.snap.clone()
    }
    fn analyzer(&self) -> Option<Arc<Analyzer>> {
        self.triggers.analyzer()
    }
    fn coach_plan(&self) -> Option<Plan> {
        self.triggers.coach_plan()
    }
    fn apply_coach_plan(&self, plan: Plan) {
        self.triggers.set_coach_plan(plan)
    }
    fn say(&self, t: String, w: SayWhen) {
        self.triggers.say(t, w)
    }
    fn focus(&self) -> Option<FocusLink> {
        Some(self.focus.clone())
    }
}

fn lap(samples: Vec<telemetry::Sample>, number: i32) -> Arc<Lap> {
    Arc::new(Lap {
        number,
        start_time: samples[0].session_time,
        lap_time: samples.len() as f64 / 60.0,
        flags: LapFlags {
            complete: true,
            ..Default::default()
        },
        samples,
    })
}

fn analyst(prompts: Arc<Mutex<Vec<String>>>) -> impl agent_client_protocol::ConnectTo<Client> {
    let mcp: Arc<Mutex<Option<(String, String)>>> = Arc::default();
    let mcp2 = mcp.clone();
    Agent
        .v1()
        .on_receive_request(
            async move |req: InitializeRequest, r: Responder<InitializeResponse, V1>, _cx: ConnectionTo<Client>| {
                r.respond(InitializeResponse::new(req.protocol_version).agent_capabilities(AgentCapabilities::new()))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: NewSessionRequest, r: Responder<NewSessionResponse, V1>, _cx: ConnectionTo<Client>| {
                if let Some(McpServer::Http(h)) = req.mcp_servers.first() {
                    let token = h.headers[0].value.trim_start_matches("Bearer ").to_string();
                    *mcp2.lock().unwrap() = Some((h.url.clone(), token));
                }
                r.respond(NewSessionResponse::new("analysis"))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: PromptRequest, r: Responder<PromptResponse, V1>, cx: ConnectionTo<Client>| {
                if let Some(ContentBlock::Text(t)) = req.prompt.first() {
                    prompts.lock().unwrap().push(t.text.clone());
                }
                let (url, token) = mcp.lock().unwrap().clone().unwrap();
                let sid = req.session_id.clone();
                let prompt = match req.prompt.first() {
                    Some(ContentBlock::Text(t)) => t.text.clone(),
                    _ => String::new(),
                };
                tokio::spawn(async move {
                    let transport = StreamableHttpClientTransport::from_config(
                        StreamableHttpClientTransportConfig::with_uri(url).auth_header(token),
                    );
                    let client = ().serve(transport).await.unwrap();
                    let call = |name: &'static str, args: serde_json::Value| {
                        let client = &client;
                        async move {
                            let res = client
                                .call_tool(
                                    CallToolRequestParams::new(name)
                                        .with_arguments(args.as_object().unwrap().clone()),
                                )
                                .await
                                .unwrap();
                            assert_ne!(res.is_error, Some(true), "{name}: {res:?}");
                        }
                    };
                    let summary = if prompt.starts_with("[Focus]") {
                        call("set_focus", serde_json::json!({
                            "corners": ["T4"], "announcement": "Hairpin focus: brake later."
                        }))
                        .await;
                        call("update_plan", serde_json::json!({
                            "mode": "replace",
                            "triggers": [{
                                "id": "auto-T4", "corner": "T4", "anchor": "braking", "offset_m": -300,
                                "text": "Brake later.", "priority": 6
                            }]
                        }))
                        .await;
                        "T4: braking too early, focus set."
                    } else {
                        call("say", serde_json::json!({"text": "Release slower.", "when": "now"})).await;
                        "T4 attempt: release slower."
                    };
                    let _ = client.cancel().await;
                    let u = SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                        TextContent::new(format!("Done.\n{summary}")),
                    )));
                    let _ = cx.send_notification(SessionNotification::new(sid, u));
                    let _ = r.respond(PromptResponse::new(StopReason::EndTurn));
                });
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
}

fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ok() {
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn focus_loop_chooses_analyzes_passes_and_moves_on() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let track = SyntheticTrack::demo();
    let reference = lap(track.simulate_lap(1.0, 0.0, 0.0), 0);
    let analyzer = Arc::new(Analyzer::new(
        &reference,
        Some(track.length_m()),
        &CornerDetectionParams::default(),
    ));
    let (_stx, srx) = crossbeam_channel::bounded(10);
    let triggers = Arc::new(TriggerRuntime::spawn(srx, Box::new(Mute)));
    triggers.set_reference(Some(analyzer));
    triggers.set_plan(
        "driver",
        Plan::parse("[[trigger]]\nid = \"a\"\nposition = 0.1\ntext = \"Driver.\"").unwrap(),
    );

    let done = lap(track.simulate_lap(0.95, 100.0, 0.0), 3);
    let snap = LiveSnapshot {
        laps: vec![done.clone()],
        ..Default::default()
    };
    let (link, commands) = FocusLink::new();
    let server = rt
        .block_on(McpServerHandle::start(Arc::new(Data {
            snap: snap.clone(),
            triggers: triggers.clone(),
            focus: link.clone(),
        })))
        .unwrap();

    let prompts: Arc<Mutex<Vec<String>>> = Arc::default();
    let cfg = AgentConfig {
        label: "analysis".into(),
        command: vec![],
        model: Some("sonnet".into()),
        disable_thinking: false,
        env: vec![],
        cwd: std::env::temp_dir(),
        system_prompt: coach::DEFAULT_ANALYSIS_PROMPT.into(),
        mcp: Some(McpEndpoint {
            name: mcp::SERVER_NAME.into(),
            url: server.url.clone(),
            token: server.token.clone(),
        }),
    };
    let agent = Arc::new(CoachAgent::spawn_with(
        analyst(prompts.clone()),
        cfg,
        rt.handle(),
    ));
    let (ltx, lrx) = crossbeam_channel::unbounded();
    let (sample_tx, sample_rx) = crossbeam_channel::unbounded();
    let planner = Planner::spawn(
        PlannerDeps {
            agent,
            model: "sonnet".into(),
            laps: lrx,
            samples: sample_rx,
            snapshot: Arc::new(move || snap.clone()),
            triggers: triggers.clone(),
            link: link.clone(),
            commands,
            language: voice::Language::English,
        },
        PlannerPolicy::default(),
        true,
    );
    wait_for("loop waiting for a lap", || {
        planner.status().focus.phase == FocusPhase::WaitingForLap
    });

    // 1. A valid lap: the race engineer chooses T4.
    ltx.send(TelemetryEvent::LapCompleted(done)).unwrap();
    wait_for("focus on T4", || {
        let f = planner.status().focus;
        f.phase == FocusPhase::Practicing && f.corners == ["T4"]
    });
    let p = prompts.lock().unwrap()[0].clone();
    assert!(
        p.starts_with("[Focus] Choose the corners to work on."),
        "{p}"
    );
    assert!(p.contains("Lap 3 completed in"), "{p}");
    assert!(p.contains("time_lost_by_corner"), "overview attached");
    assert!(p.contains("Speaking time"), "{p}");
    let base = planner
        .status()
        .focus
        .baseline_s
        .expect("baseline from lap 3");
    assert!(base > 0.0, "{base}");
    wait_for("choice analyzed", || planner.status().analyses == 1);
    assert_eq!(
        planner.status().last.unwrap().text,
        "T4: braking too early, focus set."
    );

    // 2. A pass through T4: analyzed right after the corner, before the end
    // of the lap.
    let pass = track.simulate_lap(0.97, 300.0, 0.0);
    for mut s in pass {
        s.is_on_track = true;
        s.surface = TrackSurface::OnTrack;
        s.lap = 4;
        sample_tx.send(s).unwrap();
    }
    wait_for("attempt recorded", || {
        !planner.status().focus.attempts.is_empty()
    });
    let attempt = planner.status().focus.attempts[0].clone();
    assert_eq!(attempt.number, 1);
    assert!(attempt.loss_s > 0.0 && attempt.loss_s < base, "{attempt:?}");
    wait_for("attempt prompt", || prompts.lock().unwrap().len() == 2);
    let p = prompts.lock().unwrap()[1].clone();
    assert!(p.starts_with("[Attempt 1 on T4]"), "{p}");
    assert!(p.contains("brake point"), "{p}");
    assert!(p.contains("Start: +"), "{p}");
    wait_for("attempt analyzed", || planner.status().analyses == 2);

    // The announcement, the time of the pass and the advice were spoken (or
    // queued: the trigger thread gets no samples here).
    wait_for("sentences queued", || {
        let texts: Vec<String> = triggers
            .status()
            .triggers
            .iter()
            .map(|v| v.trigger.text.clone())
            .collect();
        ["Hairpin focus: brake later.", "tenth", "Release slower."]
            .iter()
            .all(|w| texts.iter().any(|t| t.contains(w)))
    });
    let ids: Vec<String> = triggers
        .status()
        .triggers
        .iter()
        .map(|v| v.trigger.id.clone())
        .filter(|id| !id.starts_with("say-"))
        .collect();
    assert_eq!(ids, vec!["a", "coach:auto-T4"]);
    assert_eq!(
        triggers.current_plan().unwrap().1.triggers.len(),
        1,
        "driver plan untouched"
    );

    // 3. The driver asks to move on: T4 is set aside and a new choice starts.
    link.commands
        .send(FocusCommand::Next {
            reason: "the driver asked".into(),
        })
        .unwrap();
    wait_for("new choice", || prompts.lock().unwrap().len() == 3);
    let p = prompts.lock().unwrap()[2].clone();
    assert!(p.starts_with("[Focus]"), "{p}");
    assert!(p.contains("T4: +"), "finished corners listed: {p}");
    assert!(p.contains("(the driver asked)"), "{p}");
}
