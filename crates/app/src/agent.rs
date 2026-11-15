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
    /// Session servie par ce point d'accès MCP (journal des outils).
    pub source: &'static str,
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
    fn coach_plan(&self) -> Option<Plan> {
        self.triggers.coach_plan()
    }
    fn apply_coach_plan(&self, plan: Plan) {
        self.triggers.set_coach_plan(plan);
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
            source: self.source.to_string(),
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
    /// Session d'analyse (planification du tour suivant).
    pub analysis: bool,
    pub analysis_model: String,
}

/// Résultat du démarrage : ce que l'interface affiche et pilote.
pub struct AgentServices {
    pub agent: Option<Arc<CoachAgent>>,
    pub analysis: Option<Arc<CoachAgent>>,
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
fn system_prompt(dir: &std::path::Path, default: &str) -> String {
    let path = dir.join("CLAUDE.md");
    if let Ok(text) = std::fs::read_to_string(&path) {
        return text;
    }
    if std::fs::create_dir_all(dir).is_ok() {
        let _ = std::fs::write(&path, default);
    }
    default.to_string()
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
                mcp_error: Some(format!("runtime : {e}")),
                tool_log,
                keep_alive: Box::new(()),
            };
        }
    };
    // Un point d'accès MCP par session, pour savoir qui appelle quel outil.
    let data = |source: &'static str| {
        Arc::new(AppData {
            source,
            snapshot: snapshot.clone(),
            triggers: triggers.clone(),
            tool_log: tool_log.clone(),
        })
    };
    let analysis_server = (options.enabled && options.analysis)
        .then(|| rt.block_on(McpServerHandle::start(data("analyse"))).ok())
        .flatten();
    let server = match rt.block_on(McpServerHandle::start(data("conversation"))) {
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
    tracing::info!("serveur MCP : {}", server.url);
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
            system_prompt: system_prompt(&cwd, coach::DEFAULT_SYSTEM_PROMPT),
            cwd,
            mcp: Some(endpoint.clone()),
        };
        tracing::info!("adaptateur ACP : {}", cfg.command.join(" "));
        Arc::new(CoachAgent::spawn(cfg, rt.handle()))
    });
    // Session d'analyse : modèle plus fort, réflexion permise (hors boucle rapide).
    let analysis = analysis_server.as_ref().map(|srv| {
        let cwd = session_dir("analyse");
        let cfg = AgentConfig {
            label: "analyse".into(),
            command: AgentConfig::default_command(),
            model: Some(options.analysis_model.clone()),
            disable_thinking: false,
            env: vec![],
            system_prompt: system_prompt(&cwd, coach::DEFAULT_ANALYSIS_PROMPT),
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
