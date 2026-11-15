//! Mise en place de Claude : serveur MCP embarqué et session ACP de conversation.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use analysis::Analyzer;
use coach::{AgentConfig, CoachAgent, McpEndpoint};
use mcp::{CoachData, McpServerHandle, ToolCallLog};
use telemetry::{LiveSnapshot, SnapshotHandle};
use triggers::{Plan, SayWhen, TriggerRuntime};

/// Journal des appels d'outils, partagé avec l'interface.
pub type ToolLog = Arc<Mutex<VecDeque<ToolCallLog>>>;

/// État du programme vu par le serveur MCP.
pub struct AppData {
    pub snapshot: SnapshotHandle,
    pub triggers: Arc<TriggerRuntime>,
    pub tool_log: ToolLog,
}

impl CoachData for AppData {
    fn snapshot(&self) -> LiveSnapshot {
        self.snapshot.get()
    }
    fn analyzer(&self) -> Option<Arc<Analyzer>> {
        self.triggers.analyzer()
    }
    fn current_plan(&self) -> Option<(String, Plan)> {
        self.triggers.current_plan()
    }
    fn apply_plan(&self, name: String, plan: Plan) {
        self.triggers.set_plan(name, plan);
    }
    fn say(&self, text: String, when: SayWhen) {
        self.triggers.say(text, when);
    }
    fn tool_called(&self, name: &str, ok: bool, millis: f64) {
        let mut log = self.tool_log.lock().unwrap();
        if log.len() == 100 {
            log.pop_front();
        }
        log.push_back(ToolCallLog {
            name: name.to_string(),
            ok,
            millis,
        });
    }
}

/// Options de la session de conversation.
pub struct AgentOptions {
    pub enabled: bool,
    pub model: String,
}

/// Résultat du démarrage : ce que l'interface affiche et pilote.
pub struct AgentServices {
    pub agent: Option<Arc<CoachAgent>>,
    pub mcp_url: Option<String>,
    pub mcp_error: Option<String>,
    pub tool_log: ToolLog,
    /// Garde le runtime et le serveur en vie.
    pub keep_alive: Box<dyn std::any::Any>,
}

/// Dossier de la session : contient le `CLAUDE.md` du coach, modifiable par le pilote.
fn session_dir(role: &str) -> PathBuf {
    directories::ProjectDirs::from("", "", "simcoach")
        .map(|p| p.data_local_dir().join("sessions").join(role))
        .unwrap_or_else(|| PathBuf::from("sessions").join(role))
}

/// Lit le prompt du coach ; écrit la version par défaut s'il n'existe pas.
fn system_prompt(dir: &std::path::Path) -> String {
    let path = dir.join("CLAUDE.md");
    if let Ok(text) = std::fs::read_to_string(&path) {
        return text;
    }
    if std::fs::create_dir_all(dir).is_ok() {
        let _ = std::fs::write(&path, coach::DEFAULT_SYSTEM_PROMPT);
    }
    coach::DEFAULT_SYSTEM_PROMPT.to_string()
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
                mcp_url: None,
                mcp_error: Some(format!("runtime : {e}")),
                tool_log,
                keep_alive: Box::new(()),
            };
        }
    };
    let data = Arc::new(AppData {
        snapshot,
        triggers,
        tool_log: tool_log.clone(),
    });
    let server = match rt.block_on(McpServerHandle::start(data)) {
        Ok(s) => s,
        Err(e) => {
            return AgentServices {
                agent: None,
                mcp_url: None,
                mcp_error: Some(e.to_string()),
                tool_log,
                keep_alive: Box::new(rt),
            };
        }
    };
    tracing::info!("serveur MCP : {}", server.url);
    let agent = options.enabled.then(|| {
        let cwd = session_dir("conversation");
        let cfg = AgentConfig {
            label: "conversation".into(),
            command: AgentConfig::default_command(),
            model: Some(options.model.clone()),
            disable_thinking: true,
            env: vec![],
            system_prompt: system_prompt(&cwd),
            cwd,
            mcp: Some(McpEndpoint {
                name: mcp::SERVER_NAME.into(),
                url: server.url.clone(),
                token: server.token.clone(),
            }),
        };
        tracing::info!("adaptateur ACP : {}", cfg.command.join(" "));
        Arc::new(CoachAgent::spawn(cfg, rt.handle()))
    });
    AgentServices {
        agent,
        mcp_url: Some(server.url.clone()),
        mcp_error: None,
        tool_log,
        keep_alive: Box::new((rt, server)),
    }
}
