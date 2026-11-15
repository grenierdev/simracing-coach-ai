//! Client ACP face à un faux agent en processus : configuration de session,
//! flux de réponse, appel réel à notre serveur MCP, permissions, latence.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    AgentCapabilities, ContentBlock, ContentChunk, InitializeRequest, InitializeResponse,
    McpServer, NewSessionRequest, NewSessionResponse, PermissionOption, PermissionOptionKind,
    PromptRequest, PromptResponse, RequestPermissionOutcome, RequestPermissionRequest,
    SessionNotification, SessionUpdate, StopReason, SystemPromptMode, TextContent, ToolCall,
    ToolCallUpdate, ToolCallUpdateFields,
};
use agent_client_protocol::{Agent, Client, Responder, V1};
use analysis::Analyzer;
use coach::{AgentConfig, AgentStatus, CoachAgent, CoachEvent, McpEndpoint};
use mcp::{CoachData, McpServerHandle};
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use telemetry::synthetic::SyntheticTrack;
use telemetry::{CornerDetectionParams, Lap, LapFlags, LiveSnapshot};
use triggers::{Plan, SayWhen};

struct Data {
    snap: LiveSnapshot,
    analyzer: Arc<Analyzer>,
}

impl CoachData for Data {
    fn snapshot(&self) -> LiveSnapshot {
        self.snap.clone()
    }
    fn analyzer(&self) -> Option<Arc<Analyzer>> {
        Some(self.analyzer.clone())
    }
    fn coach_plan(&self) -> Option<Plan> {
        None
    }
    fn apply_coach_plan(&self, _: Plan) {}
    fn say(&self, _: String, _: SayWhen) {}
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

/// Ce que le faux agent a reçu au `session/new`.
#[derive(Default, Debug)]
struct Seen {
    system_prompt_override: bool,
    system_prompt: String,
    meta: Option<serde_json::Value>,
    mcp: Option<(String, String, String)>,
    prompts: Vec<String>,
}

fn text(t: &str) -> SessionUpdate {
    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(t))))
}

fn permission(
    session: agent_client_protocol::v1::schema::SessionId,
    tool: &str,
) -> RequestPermissionRequest {
    RequestPermissionRequest::new(
        session,
        ToolCallUpdate::new("p1", ToolCallUpdateFields::new().name(tool.to_string())),
        vec![
            PermissionOption::new("ok", "Autoriser", PermissionOptionKind::AllowOnce),
            PermissionOption::new("non", "Refuser", PermissionOptionKind::RejectOnce),
        ],
    )
}

fn mock_agent(seen: Arc<Mutex<Seen>>) -> impl agent_client_protocol::ConnectTo<Client> {
    let seen_new = seen.clone();
    let seen_prompt = seen.clone();
    Agent
        .v1()
        .on_receive_request(
            async move |req: InitializeRequest,
                        responder: Responder<InitializeResponse, V1>,
                        _cx: ConnectionTo<Client>| {
                responder.respond(
                    InitializeResponse::new(req.protocol_version)
                        .agent_capabilities(AgentCapabilities::new()),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: NewSessionRequest,
                        responder: Responder<NewSessionResponse, V1>,
                        _cx: ConnectionTo<Client>| {
                let mut s = seen_new.lock().unwrap();
                if let Some(sp) = &req.system_prompt {
                    s.system_prompt_override = sp.mode == SystemPromptMode::Override;
                    if let Some(ContentBlock::Text(t)) = sp.content.first() {
                        s.system_prompt = t.text.clone();
                    }
                }
                s.meta = req.meta.clone().map(serde_json::Value::Object);
                if let Some(McpServer::Http(h)) = req.mcp_servers.first() {
                    s.mcp = Some((
                        h.name.clone(),
                        h.url.clone(),
                        h.headers
                            .first()
                            .map(|h| h.value.clone())
                            .unwrap_or_default(),
                    ));
                }
                responder.respond(NewSessionResponse::new("session-test"))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: PromptRequest,
                        responder: Responder<PromptResponse, V1>,
                        cx: ConnectionTo<Client>| {
                let prompt = match req.prompt.first() {
                    Some(ContentBlock::Text(t)) => t.text.clone(),
                    _ => String::new(),
                };
                let mcp = {
                    let mut s = seen_prompt.lock().unwrap();
                    s.prompts.push(prompt.clone());
                    s.mcp.clone()
                };
                let sid = req.session_id.clone();
                // Le travail se fait hors de la boucle de distribution.
                tokio::spawn(async move {
                    let send = |u: SessionUpdate| {
                        cx.send_notification(SessionNotification::new(sid.clone(), u))
                    };
                    send(text("Je regarde. ")).unwrap();
                    // Permissions : outil du coach puis outil intégré.
                    let ok = cx
                        .send_request(permission(sid.clone(), "mcp__coach__get_session_overview"))
                        .block_task()
                        .await
                        .unwrap();
                    let bash = cx
                        .send_request(permission(sid.clone(), "Bash"))
                        .block_task()
                        .await
                        .unwrap();
                    let selected = |o: &RequestPermissionOutcome| match o {
                        RequestPermissionOutcome::Selected(s) => s.option_id.to_string(),
                        _ => "annulé".into(),
                    };
                    let perms = format!("[{}/{}] ", selected(&ok.outcome), selected(&bash.outcome));
                    send(text(&perms)).unwrap();
                    // Vrai appel MCP vers le serveur du programme.
                    if let Some((_, url, header)) = mcp {
                        send(SessionUpdate::ToolCall(ToolCall::new(
                            "t1",
                            "mcp__coach__get_session_overview",
                        )))
                        .unwrap();
                        let token = header.trim_start_matches("Bearer ").to_string();
                        let transport = StreamableHttpClientTransport::from_config(
                            StreamableHttpClientTransportConfig::with_uri(url).auth_header(token),
                        );
                        let client = ().serve(transport).await.unwrap();
                        let r = client
                            .call_tool(CallToolRequestParams::new("get_session_overview"))
                            .await
                            .unwrap();
                        let body = serde_json::to_value(&r.content).unwrap()[0]["text"]
                            .as_str()
                            .unwrap()
                            .to_string();
                        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
                        let worst = v["temps_perdu_par_virage"][0]["virage"]
                            .as_str()
                            .unwrap()
                            .to_string();
                        client.cancel().await.unwrap();
                        send(text(&format!("Le plus coûteux : {worst}."))).unwrap();
                    }
                    responder
                        .respond(PromptResponse::new(StopReason::EndTurn))
                        .unwrap();
                });
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
}

fn wait_for(agent: &CoachAgent, pred: impl Fn(&CoachEvent) -> bool, log: &mut Vec<CoachEvent>) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Ok(e) = agent.events().recv_timeout(Duration::from_millis(100)) {
            let done = pred(&e);
            log.push(e);
            if done {
                return;
            }
        }
    }
    panic!("événement attendu non reçu ; reçus : {log:#?}");
}

#[test]
fn full_turn_with_mcp_call_and_permissions() {
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
    let data = Arc::new(Data {
        snap: LiveSnapshot {
            laps: vec![lap(track.simulate_lap(0.95, 10.0, 0.0), 1)],
            ..Default::default()
        },
        analyzer,
    });
    let server = rt.block_on(McpServerHandle::start(data)).unwrap();

    let seen: Arc<Mutex<Seen>> = Arc::default();
    let cfg = AgentConfig {
        label: "conversation".into(),
        command: vec![],
        model: Some("haiku".into()),
        disable_thinking: true,
        env: vec![],
        cwd: std::env::temp_dir(),
        system_prompt: coach::DEFAULT_SYSTEM_PROMPT.into(),
        mcp: Some(McpEndpoint {
            name: mcp::SERVER_NAME.into(),
            url: server.url.clone(),
            token: server.token.clone(),
        }),
    };
    let agent = CoachAgent::spawn_with(mock_agent(seen.clone()), cfg, rt.handle());
    let mut log = vec![];
    wait_for(
        &agent,
        |e| matches!(e, CoachEvent::Status(AgentStatus::Ready { .. })),
        &mut log,
    );

    {
        let s = seen.lock().unwrap();
        assert!(s.system_prompt_override, "prompt système en override");
        assert!(s.system_prompt.contains("Coach de pilotage"));
        let meta = s.meta.as_ref().unwrap();
        let opts = &meta["claudeCode"]["options"];
        assert_eq!(opts["tools"], serde_json::json!([]));
        assert_eq!(opts["allowedTools"], serde_json::json!(["mcp__coach"]));
        assert_eq!(opts["settingSources"], serde_json::json!([]));
        let (name, url, header) = s.mcp.clone().unwrap();
        assert_eq!(name, "coach");
        assert_eq!(url, server.url);
        assert_eq!(header, format!("Bearer {}", server.token));
    }

    let turn = agent.prompt("[Contexte] tour 2\n[Pilote] où je perds du temps ?");
    wait_for(
        &agent,
        |e| matches!(e, CoachEvent::TurnEnd { .. }),
        &mut log,
    );

    let reply: String = log
        .iter()
        .filter_map(|e| match e {
            CoachEvent::Chunk { turn: t, text } if *t == turn => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(reply.starts_with("Je regarde. "), "{reply}");
    assert!(
        reply.contains("[ok/non]"),
        "coach autorisé, Bash refusé : {reply}"
    );
    assert!(reply.contains("Le plus coûteux : T"), "{reply}");
    let perms: Vec<(String, bool)> = log
        .iter()
        .filter_map(|e| match e {
            CoachEvent::Permission { tool, allowed } => Some((tool.clone(), *allowed)),
            _ => None,
        })
        .collect();
    assert_eq!(
        perms,
        vec![
            ("mcp__coach__get_session_overview".into(), true),
            ("Bash".into(), false)
        ]
    );
    assert!(log.iter().any(|e| matches!(e, CoachEvent::ToolCall { name, .. } if name == "mcp__coach__get_session_overview")));
    let latency = log
        .iter()
        .find_map(|e| match e {
            CoachEvent::TurnEnd {
                latency,
                stop_reason,
                ..
            } => {
                assert_eq!(stop_reason, "EndTurn");
                Some(*latency)
            }
            _ => None,
        })
        .unwrap();
    let first = latency.first_chunk_ms.expect("latence du premier morceau");
    assert!(first <= latency.total_ms);
    assert_eq!(seen.lock().unwrap().prompts.len(), 1);
}

#[test]
fn missing_adapter_reports_failure() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let cfg = AgentConfig {
        label: "conversation".into(),
        command: vec!["programme-qui-n-existe-pas-acp".into()],
        model: None,
        disable_thinking: false,
        env: vec![],
        cwd: std::env::temp_dir(),
        system_prompt: String::new(),
        mcp: None,
    };
    let agent = CoachAgent::spawn(cfg, rt.handle());
    let mut log = vec![];
    wait_for(
        &agent,
        |e| matches!(e, CoachEvent::Status(AgentStatus::Failed(_))),
        &mut log,
    );
}
