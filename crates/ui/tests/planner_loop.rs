//! Planification de bout en bout : tour terminé → session d'analyse (faux
//! agent) → vrai appel MCP `update_plan` → consigne dans la couche du coach.

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
use mcp::{CoachData, McpServerHandle};
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use telemetry::synthetic::SyntheticTrack;
use telemetry::{CornerDetectionParams, Lap, LapFlags, LiveSnapshot, TelemetryEvent};
use triggers::{Plan, SayWhen, SpeechOutput, TriggerRuntime};
use ui::planner::{Planner, PlannerPolicy};

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
                r.respond(NewSessionResponse::new("analyse"))
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
                tokio::spawn(async move {
                    let transport = StreamableHttpClientTransport::from_config(
                        StreamableHttpClientTransportConfig::with_uri(url).auth_header(token),
                    );
                    let client = ().serve(transport).await.unwrap();
                    let args = serde_json::json!({
                        "mode": "replace",
                        "triggers": [{
                            "id": "auto-T6", "corner": "T6", "anchor": "braking", "offset_m": -300,
                            "text": "Dernier virage, freine plus tard.", "priority": 6, "expire_after_laps": 2
                        }]
                    });
                    let res = client
                        .call_tool(CallToolRequestParams::new("update_plan").with_arguments(args.as_object().unwrap().clone()))
                        .await
                        .unwrap();
                    assert_ne!(res.is_error, Some(true));
                    let _ = client.cancel().await;
                    let u = SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
                        "Analyse faite.\nT6 : freinage trop tôt, consigne ajoutée.",
                    ))));
                    let _ = cx.send_notification(SessionNotification::new(sid, u));
                    let _ = r.respond(PromptResponse::new(StopReason::EndTurn));
                });
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
}

#[test]
fn completed_lap_triggers_analysis_and_coach_plan() {
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
        "pilote",
        Plan::parse("[[trigger]]\nid = \"a\"\nposition = 0.1\ntext = \"Pilote.\"").unwrap(),
    );

    let done = lap(track.simulate_lap(0.95, 100.0, 0.0), 3);
    let snap = LiveSnapshot {
        laps: vec![done.clone()],
        ..Default::default()
    };
    let server = rt
        .block_on(McpServerHandle::start(Arc::new(Data {
            snap: snap.clone(),
            triggers: triggers.clone(),
        })))
        .unwrap();

    let prompts: Arc<Mutex<Vec<String>>> = Arc::default();
    let cfg = AgentConfig {
        label: "analyse".into(),
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
    let planner = Planner::spawn(
        agent,
        "sonnet".into(),
        lrx,
        Arc::new(move || snap.clone()),
        triggers.clone(),
        PlannerPolicy::default(),
        true,
    );
    std::thread::sleep(Duration::from_millis(300));
    ltx.send(TelemetryEvent::LapCompleted(done)).unwrap();

    let deadline = Instant::now() + Duration::from_secs(20);
    while planner.status().analyses == 0 {
        assert!(
            Instant::now() < deadline,
            "analyse non terminée : {:?}",
            planner.status()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let st = planner.status();
    let (lap_no, summary, _) = st.last.unwrap();
    assert_eq!(lap_no, 3);
    assert_eq!(summary, "T6 : freinage trop tôt, consigne ajoutée.");

    let p = prompts.lock().unwrap()[0].clone();
    assert!(p.starts_with("[Analysis] Lap 3 completed in"), "{p}");
    assert!(p.contains("time_lost_by_corner"), "vue d'ensemble jointe");

    std::thread::sleep(Duration::from_millis(300));
    let ids: Vec<String> = triggers
        .status()
        .triggers
        .iter()
        .map(|v| v.trigger.id.clone())
        .collect();
    assert_eq!(ids, vec!["a", "coach:auto-T6"]);
    assert_eq!(
        triggers.current_plan().unwrap().1.triggers.len(),
        1,
        "plan du pilote intact"
    );
}
