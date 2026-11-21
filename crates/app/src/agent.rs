//! Claude setup: embedded MCP server and ACP conversation session.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use analysis::Analyzer;
use coach::{AgentConfig, CoachAgent, McpEndpoint};
use mcp::{CoachData, CoachingLink, CoachingStatus, McpServerHandle, ToolCallLog};
use telemetry::{LiveSnapshot, SnapshotHandle};
use triggers::{Plan, SayWhen, TriggerRuntime};

/// Tool call log, shared with the UI.
pub type ToolLog = Arc<Mutex<VecDeque<ToolCallLog>>>;

/// Program state as seen by the MCP server.
pub struct AppData {
    /// Session served by this MCP endpoint (tool log).
    pub source: &'static str,
    /// May speak to the driver (`say`): the conversation only. The race
    /// engineer updates cues, spoken on the approach to their turn.
    pub speaks: bool,
    pub snapshot: SnapshotHandle,
    pub triggers: Arc<TriggerRuntime>,
    pub tool_log: ToolLog,
    /// Turn-by-turn coaching (with the analysis session only).
    pub coaching: Option<CoachingLink>,
}

impl CoachData for AppData {
    fn snapshot(&self) -> LiveSnapshot {
        self.snapshot.get()
    }
    fn analyzer(&self) -> Option<Arc<Analyzer>> {
        self.triggers.analyzer()
    }
    fn coach_plan(&self) -> Option<Plan> {
        self.triggers.coach_plan()
    }
    fn apply_coach_plan(&self, plan: Plan) {
        self.triggers.set_coach_plan(plan);
    }
    fn say(&self, text: String, when: SayWhen) {
        self.triggers.say(text, when);
    }
    fn can_speak(&self) -> bool {
        self.speaks
    }
    fn coaching(&self) -> Option<CoachingStatus> {
        self.coaching.as_ref().map(|c| c.lock().unwrap().clone())
    }
    fn tool_called(&self, name: &str, ok: bool, millis: f64) {
        let mut log = self.tool_log.lock().unwrap();
        if log.len() == 100 {
            log.pop_front();
        }
        log.push_back(ToolCallLog {
            source: self.source.to_string(),
            name: name.to_string(),
            ok,
            millis,
        });
    }
}

/// Conversation session options.
pub struct AgentOptions {
    pub enabled: bool,
    pub model: String,
    /// Analysis session (planning of the next lap).
    pub analysis: bool,
    pub analysis_model: String,
    /// Language of the replies.
    pub language: voice::Language,
    /// Turn-by-turn coaching, driven by the analysis session.
    pub coaching: CoachingLink,
}

/// Startup result: what the UI displays and drives.
pub struct AgentServices {
    pub agent: Option<Arc<CoachAgent>>,
    pub analysis: Option<Arc<CoachAgent>>,
    pub mcp_url: Option<String>,
    pub mcp_error: Option<String>,
    pub tool_log: ToolLog,
    /// Keeps the runtime and the server alive.
    pub keep_alive: Box<dyn std::any::Any>,
}

/// Session folder: holds the coach's `CLAUDE.md`, editable by the driver.
fn session_dir(role: &str) -> PathBuf {
    directories::ProjectDirs::from("", "", "simcoach")
        .map(|p| p.data_local_dir().join("sessions").join(role))
        .unwrap_or_else(|| PathBuf::from("sessions").join(role))
}

/// Reads the coach prompt of a session folder. A missing copy, or an unedited
/// default of an earlier version, is (re)written with the current default.
/// A copy edited by the driver is kept; the current default is then written
/// next to it as `CLAUDE.default.md`. The French prompt of v0.1.0 is replaced
/// and kept as a backup.
fn system_prompt(dir: &std::path::Path, default: &str, previous: &[&str]) -> String {
    let path = dir.join("CLAUDE.md");
    let mut existing = std::fs::read_to_string(&path).ok();
    if existing
        .as_deref()
        .is_some_and(|t| t.starts_with("# Coach de pilotage"))
    {
        let _ = std::fs::rename(&path, dir.join("CLAUDE.v0.1.md"));
        existing = None;
    }
    let _ = std::fs::create_dir_all(dir);
    match coach::prompt_copy(existing.as_deref(), default, previous) {
        coach::PromptCopy::WriteDefault => {
            let _ = std::fs::write(&path, default);
            let _ = std::fs::remove_file(dir.join("CLAUDE.default.md"));
            default.to_string()
        }
        coach::PromptCopy::Keep => {
            let _ = std::fs::write(dir.join("CLAUDE.default.md"), default);
            tracing::info!(
                "{}: edited prompt kept; current default in CLAUDE.default.md",
                path.display()
            );
            existing.unwrap_or_else(|| default.to_string())
        }
    }
}

pub fn start(
    snapshot: SnapshotHandle,
    triggers: Arc<TriggerRuntime>,
    options: &AgentOptions,
) -> AgentServices {
    let tool_log: ToolLog = Arc::default();
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("coach-async")
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            return AgentServices {
                agent: None,
                analysis: None,
                mcp_url: None,
                mcp_error: Some(format!("runtime: {e}")),
                tool_log,
                keep_alive: Box::new(()),
            };
        }
    };
    // One MCP endpoint per session, to know who calls which tool.
    let with_coaching = options.enabled && options.analysis;
    let data = |source: &'static str, speaks: bool| {
        Arc::new(AppData {
            source,
            speaks,
            snapshot: snapshot.clone(),
            triggers: triggers.clone(),
            tool_log: tool_log.clone(),
            coaching: with_coaching.then(|| options.coaching.clone()),
        })
    };
    let analysis_server = (options.enabled && options.analysis)
        .then(|| {
            rt.block_on(McpServerHandle::start(data("analysis", false)))
                .ok()
        })
        .flatten();
    let server = match rt.block_on(McpServerHandle::start(data("conversation", true))) {
        Ok(s) => s,
        Err(e) => {
            return AgentServices {
                agent: None,
                analysis: None,
                mcp_url: None,
                mcp_error: Some(e.to_string()),
                tool_log,
                keep_alive: Box::new(rt),
            };
        }
    };
    tracing::info!("MCP server: {}", server.url);
    let endpoint = McpEndpoint {
        name: mcp::SERVER_NAME.into(),
        url: server.url.clone(),
        token: server.token.clone(),
    };
    let agent = options.enabled.then(|| {
        let cwd = session_dir("conversation");
        let cfg = AgentConfig {
            label: "conversation".into(),
            command: AgentConfig::default_command(),
            model: Some(options.model.clone()),
            disable_thinking: true,
            env: vec![],
            system_prompt: coach::with_language(
                &system_prompt(
                    &cwd,
                    coach::DEFAULT_SYSTEM_PROMPT,
                    coach::PREVIOUS_SYSTEM_PROMPTS,
                ),
                options.language.name(),
            ),
            cwd,
            mcp: Some(endpoint.clone()),
        };
        tracing::info!("ACP adapter: {}", cfg.command.join(" "));
        Arc::new(CoachAgent::spawn(cfg, rt.handle()))
    });
    // Analysis session: stronger model, thinking allowed (outside the fast loop).
    let analysis = analysis_server.as_ref().map(|srv| {
        let cwd = session_dir("analysis");
        let cfg = AgentConfig {
            label: "analysis".into(),
            command: AgentConfig::default_command(),
            model: Some(options.analysis_model.clone()),
            disable_thinking: false,
            env: vec![],
            system_prompt: coach::with_language(
                &system_prompt(
                    &cwd,
                    coach::DEFAULT_ANALYSIS_PROMPT,
                    coach::PREVIOUS_ANALYSIS_PROMPTS,
                ),
                options.language.name(),
            ),
            cwd,
            mcp: Some(McpEndpoint {
                name: mcp::SERVER_NAME.into(),
                url: srv.url.clone(),
                token: srv.token.clone(),
            }),
        };
        Arc::new(CoachAgent::spawn(cfg, rt.handle()))
    });
    AgentServices {
        agent,
        analysis,
        mcp_url: Some(server.url.clone()),
        mcp_error: None,
        tool_log,
        keep_alive: Box::new((rt, server, analysis_server)),
    }
}
