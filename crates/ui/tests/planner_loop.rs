//! End-to-end turn-by-turn coaching: every turn is analyzed as soon as the
//! car leaves it, the turns queue up while the race engineer (fake agent)
//! works and are sent to it in batches, its `update_plan` call goes through
//! the real MCP server, and a pass that starts from a reset inside a turn is
//! analyzed too, its time spoken at once.

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
use mcp::{CoachData, CoachingLink, CoachingStatus, McpServerHandle};
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use telemetry::synthetic::SyntheticTrack;
use telemetry::{CornerDetectionParams, Lap, LapFlags, LiveSnapshot, Sample, TrackSurface};
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
    triggers: Arc<TriggerRuntime>,
    coaching: CoachingLink,
}
impl CoachData for Data {
    fn snapshot(&self) -> LiveSnapshot {
        LiveSnapshot::default()
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
    fn coaching(&self) -> Option<CoachingStatus> {
        Some(self.coaching.lock().unwrap().clone())
    }
}

fn lap(samples: Vec<Sample>, number: i32) -> Lap {
    Lap {
        number,
        start_time: samples[0].session_time,
        lap_time: samples.len() as f64 / 60.0,
        flags: LapFlags {
            complete: true,
            ..Default::default()
        },
        samples,
    }
}

/// A slow race engineer (300 ms per turn, so that turns queue up): it places
/// a cue on T4 whenever T4 is in the message.
fn analyst(prompts: Arc<Mutex<Vec<String>>>) -> impl agent_client_protocol::ConnectTo<Client> {
    let mcp: Arc<Mutex<Option<(String, String)>>> = Arc::default();
    let mcp2 = mcp.clone();
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
            async move |req: NewSessionRequest,
                        r: Responder<NewSessionResponse, V1>,
                        _cx: ConnectionTo<Client>| {
                if let Some(McpServer::Http(h)) = req.mcp_servers.first() {
                    let token = h.headers[0].value.trim_start_matches("Bearer ").to_string();
                    *mcp2.lock().unwrap() = Some((h.url.clone(), token));
                }
                r.respond(NewSessionResponse::new("analysis"))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: PromptRequest,
                        r: Responder<PromptResponse, V1>,
                        cx: ConnectionTo<Client>| {
                let prompt = match req.prompt.first() {
                    Some(ContentBlock::Text(t)) => t.text.clone(),
                    _ => String::new(),
                };
                prompts.lock().unwrap().push(prompt.clone());
                let (url, token) = mcp.lock().unwrap().clone().unwrap();
                let sid = req.session_id.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    let summary = if prompt.contains("T4, pass") {
                        let transport = StreamableHttpClientTransport::from_config(
                            StreamableHttpClientTransportConfig::with_uri(url).auth_header(token),
                        );
                        let client = ().serve(transport).await.unwrap();
                        let args = serde_json::json!({
                            "mode": "add",
                            "triggers": [{
                                "id": "auto-T4", "corner": "T4", "anchor": "braking",
                                "offset_m": -300, "text": "Brake later."
                            }]
                        });
                        let res = client
                            .call_tool(
                                CallToolRequestParams::new("update_plan")
                                    .with_arguments(args.as_object().unwrap().clone()),
                            )
                            .await
                            .unwrap();
                        assert_ne!(res.is_error, Some(true), "{res:?}");
                        let _ = client.cancel().await;
                        "T4: braking too early, cue set."
                    } else {
                        "No change."
                    };
                    let u = SessionUpdate::AgentMessageChunk(ContentChunk::new(
                        ContentBlock::Text(TextContent::new(format!("Done.\n{summary}"))),
                    ));
                    let _ = cx.send_notification(SessionNotification::new(sid, u));
                    let _ = r.respond(PromptResponse::new(StopReason::EndTurn));
                });
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
}

fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ok() {
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn on_track(mut s: Sample, lap: i32, t0: f64) -> Sample {
    s.is_on_track = true;
    s.surface = TrackSurface::OnTrack;
    s.lap = lap;
    s.session_time += t0;
    s
}

#[test]
fn every_turn_is_analyzed_queued_and_batched_also_after_a_reset() {
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
    let turns = analyzer.model.corners.len();
    let (_stx, srx) = crossbeam_channel::bounded(10);
    let triggers = Arc::new(TriggerRuntime::spawn(srx, Box::new(Mute)));
    triggers.set_reference(Some(analyzer.clone()));
    triggers.set_plan(
        "driver",
        Plan::parse("[[trigger]]\nid = \"a\"\nposition = 0.1\ntext = \"Driver.\"").unwrap(),
    );

    let coaching = CoachingLink::default();
    let server = rt
        .block_on(McpServerHandle::start(Arc::new(Data {
            triggers: triggers.clone(),
            coaching: coaching.clone(),
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
    let (_ltx, lrx) = crossbeam_channel::unbounded();
    let (sample_tx, sample_rx) = crossbeam_channel::unbounded();
    let planner = Planner::spawn(
        PlannerDeps {
            agent,
            model: "sonnet".into(),
            laps: lrx,
            samples: sample_rx,
            triggers: triggers.clone(),
            link: coaching.clone(),
            language: voice::Language::English,
        },
        PlannerPolicy::default(),
        true,
    );

    // 1. Two laps at a slower pace, sent at once: every turn is analyzed,
    // the ones that finish while the race engineer works wait and go together.
    let lap_s = track.simulate_lap(0.95, 0.0, 0.0);
    let lap_time = lap_s.len() as f64 / 60.0;
    for n in 0..2 {
        for s in &lap_s {
            sample_tx
                .send(on_track(*s, n + 1, 100.0 + n as f64 * lap_time))
                .unwrap();
        }
    }
    wait_for("every turn analyzed", || {
        let c = planner.status().coaching;
        c.turns.len() == turns
            && c.turns
                .iter()
                .all(|t| t.passes >= 1 && !t.queued && !t.analyzing)
            && planner.status().running.is_none()
    });
    let all = prompts.lock().unwrap().clone();
    for t in &analyzer.model.corners {
        let needle = format!("{}, pass", t.label());
        assert!(
            all.iter().any(|p| p.contains(&needle)),
            "{} never analyzed: {all:?}",
            t.label()
        );
    }
    assert!(all.iter().all(|p| p.starts_with("[Turns]")));
    assert!(
        all.iter().any(|p| p.matches(", pass ").count() >= 2),
        "turns batched while the race engineer works: {all:?}"
    );
    assert!(all.len() < 2 * turns, "{} messages", all.len());
    let t4 = &planner.status().coaching.turns[3];
    assert!(t4.last_s.unwrap() > 0.0 && t4.best_s.is_some(), "{t4:?}");

    // The cue placed by the race engineer is in effect, next to the driver's.
    wait_for("cue in effect", || {
        triggers
            .status()
            .triggers
            .iter()
            .any(|v| v.trigger.id == "coach:auto-T4" && v.trigger.text == "Brake later.")
    });
    assert_eq!(triggers.current_plan().unwrap().1.triggers.len(), 1);

    // 2. A reset into T4's zone, before its braking point: a pass after the
    // reset, its time spoken at once, sent to the race engineer marked as such.
    let zone = analyzer.model.zones[3].clone();
    let brake = analyzer.reference_metrics[3].brake_point_pct.unwrap();
    let fwd = |a: f64, b: f64| (b - a).rem_euclid(1.0);
    let halfway = (zone.start_pct + fwd(zone.start_pct, brake) / 2.0).rem_euclid(1.0);
    let from = lap_s
        .iter()
        .position(|s| {
            let u = fwd(zone.start_pct, s.lap_dist_pct as f64);
            u >= fwd(zone.start_pct, halfway) && u < fwd(zone.start_pct, halfway) + 0.02
        })
        .unwrap();
    let before = prompts.lock().unwrap().len();
    let passes_before = planner.status().coaching.turns[3].passes;
    let t0 = 100.0 + 3.0 * lap_time;
    for s in &lap_s[from..] {
        sample_tx.send(on_track(*s, 3, t0)).unwrap();
    }
    wait_for("pass after the reset analyzed", || {
        planner.status().coaching.turns[3].passes > passes_before
            && prompts.lock().unwrap().len() > before
            && planner.status().running.is_none()
    });
    let reset_prompt = prompts.lock().unwrap()[before..]
        .iter()
        .find(|p| p.contains("T4, pass"))
        .cloned()
        .expect("T4 sent again");
    assert!(reset_prompt.contains("after a reset"), "{reset_prompt}");
    wait_for("time spoken", || {
        triggers
            .status()
            .triggers
            .iter()
            .any(|v| v.trigger.id.starts_with("say-") && v.trigger.text.starts_with("Turn 4."))
    });
}
