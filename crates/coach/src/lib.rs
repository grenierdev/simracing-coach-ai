//! ACP client of the coach.
//!
//! The program launches the Claude adapter (`@agentclientprotocol/claude-agent-acp`)
//! as a subprocess and talks JSON-RPC to it over stdio. A session is opened at
//! startup and kept warm; each driver message becomes a `session/prompt`, and the
//! `agent_message_chunk`s received are relayed as they stream in (to the UI, then
//! the voice).
//!
//! Session configuration (see `docs/verifications.md`):
//! - Claude Code system prompt replaced by the coach's (`override`);
//! - built-in tools removed (`tools: []`), only our MCP tools allowed;
//! - the machine's Claude Code settings ignored (`settingSources: []`);
//! - model chosen by `ANTHROPIC_MODEL`, thinking disabled for the conversation;
//! - CLI subscription: `ANTHROPIC_API_KEY` is not passed on.

pub mod context;
pub mod speech;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    CancelNotification, ContentBlock, HttpHeader, InitializeRequest, McpServer, McpServerHttp,
    Meta, NewSessionRequest, PermissionOptionKind, PromptRequest, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome,
    SessionNotification, SessionUpdate, SystemPrompt, SystemPromptMode, TextContent,
    ToolCallStatus,
};
use agent_client_protocol::{AcpAgent, AcpAgentConfig, Agent, Client, ConnectTo};
use crossbeam_channel::{Receiver, Sender, unbounded};
use serde_json::json;
use tokio::sync::mpsc;

pub use context::{PilotContext, format_prompt};
pub use speech::SentenceSplitter;

/// Default coach system prompt (copied as `CLAUDE.md` into the session directory).
pub const DEFAULT_SYSTEM_PROMPT: &str = include_str!("../prompts/CLAUDE.md");

/// System prompt of the analysis session (planning the next lap).
pub const DEFAULT_ANALYSIS_PROMPT: &str = include_str!("../prompts/ANALYSIS.md");

/// Prefix of our MCP server's tools in Claude Code.
pub fn mcp_tool_prefix(server: &str) -> String {
    format!("mcp__{server}__")
}

#[derive(Debug, Clone)]
pub struct McpEndpoint {
    pub name: String,
    pub url: String,
    pub token: String,
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Role name ("conversation", "analysis"), for display.
    pub label: String,
    /// Adapter program and its arguments.
    pub command: Vec<String>,
    /// Model (`ANTHROPIC_MODEL`): `haiku`, `sonnet`, `opus` or a full identifier.
    pub model: Option<String>,
    /// Disables thinking (`MAX_THINKING_TOKENS=0`): faster replies.
    pub disable_thinking: bool,
    /// Extra environment variables for the adapter.
    pub env: Vec<(String, String)>,
    /// Session working directory (holds the coach's `CLAUDE.md`).
    pub cwd: PathBuf,
    pub system_prompt: String,
    pub mcp: Option<McpEndpoint>,
}

impl AgentConfig {
    /// Default command: `SIMCOACH_ACP_COMMAND` if set, otherwise
    /// `claude-agent-acp` if it is in the PATH, otherwise `npx`.
    pub fn default_command() -> Vec<String> {
        if let Ok(cmd) = std::env::var("SIMCOACH_ACP_COMMAND") {
            let parts: Vec<String> = cmd.split_whitespace().map(String::from).collect();
            if !parts.is_empty() {
                return parts;
            }
        }
        let exe = if cfg!(windows) {
            "claude-agent-acp.exe"
        } else {
            "claude-agent-acp"
        };
        let in_path = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).any(|d| d.join(exe).is_file()))
            .unwrap_or(false);
        if in_path {
            vec![exe.into()]
        } else {
            let npx = if cfg!(windows) { "npx.cmd" } else { "npx" };
            vec![
                npx.into(),
                "-y".into(),
                "@agentclientprotocol/claude-agent-acp@0.107.0".into(),
            ]
        }
    }

    /// Environment passed to the adapter.
    pub fn process_env(&self) -> Vec<(String, String)> {
        let mut env = self.env.clone();
        if let Some(m) = &self.model {
            env.push(("ANTHROPIC_MODEL".into(), m.clone()));
        }
        if self.disable_thinking {
            env.push(("MAX_THINKING_TOKENS".into(), "0".into()));
        }
        env
    }

    /// Claude Code options passed in `_meta.claudeCode.options`.
    pub fn session_meta(&self) -> Meta {
        let allowed: Vec<String> = self
            .mcp
            .iter()
            .map(|m| format!("mcp__{}", m.name))
            .collect();
        let v = json!({
            "claudeCode": {
                "options": {
                    "tools": [],
                    "allowedTools": allowed,
                    "settingSources": [],
                    "promptSuggestions": false,
                }
            }
        });
        v.as_object().cloned().unwrap_or_default()
    }

    pub fn session_request(&self) -> NewSessionRequest {
        let servers = self
            .mcp
            .iter()
            .map(|m| {
                McpServer::Http(
                    McpServerHttp::new(m.name.clone(), m.url.clone()).headers(vec![
                        HttpHeader::new("Authorization", format!("Bearer {}", m.token)),
                    ]),
                )
            })
            .collect();
        NewSessionRequest::new(self.cwd.clone())
            .mcp_servers(servers)
            .system_prompt(
                SystemPrompt::new(vec![ContentBlock::Text(TextContent::new(
                    self.system_prompt.clone(),
                ))])
                .mode(SystemPromptMode::Override),
            )
            .meta(self.session_meta())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentStatus {
    Starting,
    Ready { agent: String },
    Busy,
    Failed(String),
    Stopped,
}

/// Latency of a conversation turn.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TurnLatency {
    /// From sending the prompt to the first `agent_message_chunk`, ms.
    pub first_chunk_ms: Option<f64>,
    /// From sending the prompt to the end of the turn, ms.
    pub total_ms: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CoachEvent {
    Status(AgentStatus),
    /// Reply chunk, to display and speak as it streams in.
    Chunk {
        turn: u64,
        text: String,
    },
    ToolCall {
        turn: u64,
        id: String,
        name: String,
        status: String,
    },
    /// Permission request (auto-approved or denied).
    Permission {
        tool: String,
        allowed: bool,
    },
    TurnEnd {
        turn: u64,
        stop_reason: String,
        latency: TurnLatency,
    },
    Error(String),
}

enum Cmd {
    Prompt { turn: u64, text: String },
    Cancel,
}

#[derive(Default)]
struct TurnState {
    turn: u64,
    started: Option<Instant>,
    first_chunk_ms: Option<f64>,
}

/// ACP session held by a task on the tokio runtime.
pub struct CoachAgent {
    label: String,
    tx: mpsc::UnboundedSender<Cmd>,
    events: Receiver<CoachEvent>,
    next_turn: Mutex<u64>,
}

/// Decision on a permission request: our MCP tools yes, everything else no.
pub fn permission_allowed(req: &RequestPermissionRequest, mcp_server: Option<&str>) -> bool {
    let Some(server) = mcp_server else {
        return false;
    };
    let prefix = mcp_tool_prefix(server);
    let f = &req.tool_call.fields;
    f.name
        .as_deref()
        .or(f.title.as_deref())
        .is_some_and(|n| n.starts_with(&prefix))
}

fn permission_response(req: &RequestPermissionRequest, allow: bool) -> RequestPermissionResponse {
    let kinds: &[PermissionOptionKind] = if allow {
        &[
            PermissionOptionKind::AllowOnce,
            PermissionOptionKind::AllowAlways,
        ]
    } else {
        &[
            PermissionOptionKind::RejectOnce,
            PermissionOptionKind::RejectAlways,
        ]
    };
    let chosen = kinds.iter().find_map(|k| {
        req.options
            .iter()
            .find(|o| o.kind == *k)
            .map(|o| o.option_id.clone())
    });
    RequestPermissionResponse::new(match chosen {
        Some(id) => RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id)),
        None => RequestPermissionOutcome::Cancelled,
    })
}

impl CoachAgent {
    /// Launches the adapter described by `cfg.command`.
    pub fn spawn(cfg: AgentConfig, rt: &tokio::runtime::Handle) -> Self {
        let mut acp = AcpAgentConfig::new(cfg.command.first().cloned().unwrap_or_default());
        acp = acp.args(cfg.command.iter().skip(1).cloned());
        for (k, v) in cfg.process_env() {
            acp = acp.env(k, v);
        }
        Self::spawn_with(AcpAgent::new(acp), cfg, rt)
    }

    /// Variant with any transport (in-process agent, for tests).
    pub fn spawn_with(
        transport: impl ConnectTo<Client> + 'static,
        cfg: AgentConfig,
        rt: &tokio::runtime::Handle,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let (etx, erx) = unbounded();
        let label = cfg.label.clone();
        rt.spawn(run(transport, cfg, rx, etx));
        CoachAgent {
            label,
            tx,
            events: erx,
            next_turn: Mutex::new(0),
        }
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// Sends a message; returns the conversation turn number.
    pub fn prompt(&self, text: impl Into<String>) -> u64 {
        let mut n = self.next_turn.lock().unwrap();
        *n += 1;
        let _ = self.tx.send(Cmd::Prompt {
            turn: *n,
            text: text.into(),
        });
        *n
    }

    /// Interrupts the current turn (`session/cancel`).
    pub fn cancel(&self) {
        let _ = self.tx.send(Cmd::Cancel);
    }

    pub fn events(&self) -> &Receiver<CoachEvent> {
        &self.events
    }
}

async fn run(
    transport: impl ConnectTo<Client> + 'static,
    cfg: AgentConfig,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
    events: Sender<CoachEvent>,
) {
    let _ = events.send(CoachEvent::Status(AgentStatus::Starting));
    let state: Arc<Mutex<TurnState>> = Arc::default();
    let mcp_name = cfg.mcp.as_ref().map(|m| m.name.clone());

    let ev_notif = events.clone();
    let st_notif = state.clone();
    let ev_perm = events.clone();
    let result = Client
        .v1()
        .on_receive_notification(
            async move |n: SessionNotification, _cx| {
                let mut st = st_notif.lock().unwrap();
                let turn = st.turn;
                match n.update {
                    SessionUpdate::AgentMessageChunk(chunk) => {
                        if let ContentBlock::Text(t) = chunk.content {
                            if st.first_chunk_ms.is_none()
                                && let Some(t0) = st.started
                            {
                                st.first_chunk_ms = Some(t0.elapsed().as_secs_f64() * 1e3);
                            }
                            let _ = ev_notif.send(CoachEvent::Chunk { turn, text: t.text });
                        }
                    }
                    SessionUpdate::ToolCall(tc) => {
                        let _ = ev_notif.send(CoachEvent::ToolCall {
                            turn,
                            id: tc.tool_call_id.to_string(),
                            name: tc.name.unwrap_or(tc.title),
                            status: format!("{:?}", tc.status),
                        });
                    }
                    SessionUpdate::ToolCallUpdate(u) => {
                        if let Some(status) = u.fields.status
                            && matches!(status, ToolCallStatus::Completed | ToolCallStatus::Failed)
                        {
                            let _ = ev_notif.send(CoachEvent::ToolCall {
                                turn,
                                id: u.tool_call_id.to_string(),
                                name: u.fields.name.or(u.fields.title).unwrap_or_default(),
                                status: format!("{status:?}"),
                            });
                        }
                    }
                    _ => {}
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |req: RequestPermissionRequest, responder, _cx| {
                let allow = permission_allowed(&req, mcp_name.as_deref());
                let f = &req.tool_call.fields;
                let _ = ev_perm.send(CoachEvent::Permission {
                    tool: f.name.clone().or(f.title.clone()).unwrap_or_default(),
                    allowed: allow,
                });
                responder.respond(permission_response(&req, allow))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(transport, async |cx: ConnectionTo<Agent>| {
            let init = cx
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let agent_name = init
                .agent_info
                .map(|i| i.name)
                .unwrap_or_else(|| "agent".into());
            let session = cx.send_request(cfg.session_request()).block_task().await?;
            let sid = session.session_id;
            let _ = events.send(CoachEvent::Status(AgentStatus::Ready {
                agent: agent_name.clone(),
            }));

            let mut queue: VecDeque<(u64, String)> = VecDeque::new();
            loop {
                let (turn, text) = match queue.pop_front() {
                    Some(p) => p,
                    None => match rx.recv().await {
                        Some(Cmd::Prompt { turn, text }) => (turn, text),
                        Some(Cmd::Cancel) => continue,
                        None => break,
                    },
                };
                let t0 = Instant::now();
                {
                    let mut st = state.lock().unwrap();
                    *st = TurnState {
                        turn,
                        started: Some(t0),
                        first_chunk_ms: None,
                    };
                }
                let _ = events.send(CoachEvent::Status(AgentStatus::Busy));
                let fut = cx
                    .send_request(PromptRequest::new(
                        sid.clone(),
                        vec![ContentBlock::Text(TextContent::new(text))],
                    ))
                    .block_task();
                tokio::pin!(fut);
                let outcome = loop {
                    tokio::select! {
                        r = &mut fut => break r,
                        cmd = rx.recv() => match cmd {
                            Some(Cmd::Cancel) => {
                                cx.send_notification(CancelNotification::new(sid.clone()))?;
                            }
                            Some(Cmd::Prompt { turn, text }) => queue.push_back((turn, text)),
                            None => {}
                        },
                    }
                };
                let latency = TurnLatency {
                    first_chunk_ms: state.lock().unwrap().first_chunk_ms,
                    total_ms: t0.elapsed().as_secs_f64() * 1e3,
                };
                match outcome {
                    Ok(resp) => {
                        let _ = events.send(CoachEvent::TurnEnd {
                            turn,
                            stop_reason: format!("{:?}", resp.stop_reason),
                            latency,
                        });
                    }
                    Err(e) => {
                        let _ = events.send(CoachEvent::Error(e.to_string()));
                        let _ = events.send(CoachEvent::TurnEnd {
                            turn,
                            stop_reason: "error".into(),
                            latency,
                        });
                    }
                }
                let _ = events.send(CoachEvent::Status(AgentStatus::Ready {
                    agent: agent_name.clone(),
                }));
            }
            Ok(())
        })
        .await;
    let _ = events.send(CoachEvent::Status(match result {
        Ok(()) => AgentStatus::Stopped,
        Err(e) => AgentStatus::Failed(e.to_string()),
    }));
}
