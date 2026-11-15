//! Serveur rmcp : déclaration des outils et service HTTP.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock as Content};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use triggers::{Plan, SayWhen, TrackContext, TriggerSpec, resolve};

use crate::CoachData;
use crate::tools::{self, ToolError};

/// Nom du serveur dans `mcpServers` : les outils apparaissent chez Claude sous
/// `mcp__coach__<outil>`.
pub const SERVER_NAME: &str = "coach";

#[derive(Clone)]
pub struct CoachMcp {
    data: Arc<dyn CoachData>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CornerSummaryArgs {
    /// Virage : « T5 » (ou « 5 »).
    pub corner: String,
    /// Numéros des tours à analyser ; par défaut les 3 derniers tours complets.
    #[serde(default)]
    pub laps: Option<Vec<i32>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CornerTraceArgs {
    /// Virage : « T5 » (ou « 5 »).
    pub corner: String,
    /// Canaux : vitesse, gaz, frein, volant, lacet, acc_lat, rapport, delta.
    /// Par défaut : vitesse, gaz, frein.
    #[serde(default)]
    pub channels: Vec<String>,
    /// Numéro du tour ; par défaut le dernier tour complet.
    #[serde(default)]
    pub lap: Option<i32>,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanMode {
    /// Remplace tout le plan.
    Remplacer,
    /// Ajoute les consignes ; une consigne de même `id` est remplacée.
    #[default]
    Ajouter,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdatePlanArgs {
    /// Consignes à placer sur le tour.
    pub consignes: Vec<TriggerSpec>,
    /// « remplacer » ou « ajouter » (défaut).
    #[serde(default)]
    pub mode: PlanMode,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum When {
    /// Dès que possible, hors zone de freinage.
    #[default]
    Maintenant,
    /// Sur la prochaine portion où la phrase a le temps d'être dite.
    ProchaineLigneDroite,
    /// À une position : `virage` (+ `ancre`, `decalage_m`) ou `position`.
    Position,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SayArgs {
    /// Phrase à dire : courte, en français, sans mise en forme.
    pub texte: String,
    #[serde(default)]
    pub quand: When,
    /// Pour `quand = position` : virage de repère (« T5 »).
    #[serde(default)]
    pub virage: Option<String>,
    /// Pour `quand = position` : freinage, entree, apex ou sortie.
    #[serde(default)]
    pub ancre: Option<triggers::Anchor>,
    /// Pour `quand = position` : décalage en mètres (négatif = avant).
    #[serde(default)]
    pub decalage_m: Option<f64>,
    /// Pour `quand = position` : position absolue (LapDistPct, 0 à 1).
    #[serde(default)]
    pub position: Option<f64>,
}

fn ok_json(v: Value) -> Result<CallToolResult, ErrorData> {
    Ok(CallToolResult::success(vec![Content::text(v.to_string())]))
}

fn tool_err(e: ToolError) -> Result<CallToolResult, ErrorData> {
    Ok(CallToolResult::error(vec![Content::text(e.0)]))
}

/// Applique `consignes` au plan courant (`mode`) et vérifie qu'il se résout sur la carte.
pub fn merge_plan(current: Option<Plan>, specs: Vec<TriggerSpec>, mode: PlanMode) -> Plan {
    let mut plan = match mode {
        PlanMode::Remplacer => Plan {
            reglages: current.map(|p| p.reglages).unwrap_or_default(),
            consignes: vec![],
        },
        PlanMode::Ajouter => current.unwrap_or_default(),
    };
    for spec in specs {
        if let Some(id) = &spec.id
            && let Some(existing) = plan
                .consignes
                .iter_mut()
                .find(|c| c.id.as_deref() == Some(id.as_str()))
        {
            *existing = spec;
            continue;
        }
        plan.consignes.push(spec);
    }
    plan
}

#[tool_router(server_handler)]
impl CoachMcp {
    #[tool(
        description = "Vue d'ensemble de la session : derniers tours, delta à la référence et temps perdu par virage, classé du plus coûteux au moins coûteux."
    )]
    async fn get_session_overview(&self) -> Result<CallToolResult, ErrorData> {
        let t = Instant::now();
        let a = self.data.analyzer();
        let v = tools::session_overview(&self.data.snapshot(), a.as_deref());
        self.data.tool_called(
            "get_session_overview",
            true,
            t.elapsed().as_secs_f64() * 1e3,
        );
        ok_json(v)
    }

    #[tool(
        description = "Métriques d'un virage face à la référence, par tour : temps perdu, point de freinage, trail braking, vitesse mini, reprise des gaz, roue libre, accélération latérale, rapports de rotation (entrée et sortie)."
    )]
    async fn get_corner_summary(
        &self,
        Parameters(args): Parameters<CornerSummaryArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let t = Instant::now();
        let a = self.data.analyzer();
        let r = tools::corner_summary(
            &self.data.snapshot(),
            a.as_deref(),
            &args.corner,
            args.laps.as_deref(),
        );
        self.data.tool_called(
            "get_corner_summary",
            r.is_ok(),
            t.elapsed().as_secs_f64() * 1e3,
        );
        r.map_or_else(tool_err, ok_json)
    }

    #[tool(
        description = "Traces d'un virage (au plus 100 points par canal), pour un tour et pour la référence, en fonction de la distance depuis l'entrée du virage."
    )]
    async fn get_corner_trace(
        &self,
        Parameters(args): Parameters<CornerTraceArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let t = Instant::now();
        let a = self.data.analyzer();
        let r = tools::corner_trace(
            &self.data.snapshot(),
            a.as_deref(),
            &args.corner,
            &args.channels,
            args.lap,
        );
        self.data.tool_called(
            "get_corner_trace",
            r.is_ok(),
            t.elapsed().as_secs_f64() * 1e3,
        );
        r.map_or_else(tool_err, ok_json)
    }

    #[tool(
        description = "Remplace ou complète les consignes du coach dites automatiquement aux tours suivants (le plan du pilote n'est pas modifié). Chaque consigne est placée par virage (ancre + décalage en mètres) ou par position absolue. Le coach se tait de toute façon en zone de freinage."
    )]
    async fn update_plan(
        &self,
        Parameters(args): Parameters<UpdatePlanArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let t = Instant::now();
        let plan = merge_plan(self.data.coach_plan(), args.consignes, args.mode);
        let analyzer = self.data.analyzer();
        let ctx = analyzer.as_ref().map(|a| TrackContext {
            model: &a.model,
            reference: &a.reference_metrics,
        });
        let result = resolve(&plan, ctx.as_ref());
        self.data.tool_called(
            "update_plan",
            result.is_ok(),
            t.elapsed().as_secs_f64() * 1e3,
        );
        match result {
            Ok(resolved) => {
                let summary: Vec<Value> = resolved
                    .iter()
                    .map(|t| {
                        json!({
                            "id": t.id,
                            "virage": t.corner,
                            "position_pct": (t.at_pct * 1000.0).round() / 10.0,
                            "texte": t.text,
                        })
                    })
                    .collect();
                self.data.apply_coach_plan(plan);
                ok_json(json!({ "consignes_actives": summary }))
            }
            Err(e) => tool_err(ToolError(format!("plan refusé : {e}"))),
        }
    }

    #[tool(
        description = "Dit une phrase au pilote : maintenant (dès que possible hors freinage), à la prochaine ligne droite, ou à une position du tour."
    )]
    async fn say(
        &self,
        Parameters(args): Parameters<SayArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let t = Instant::now();
        let text = args.texte.trim().to_string();
        if text.is_empty() {
            return tool_err(ToolError("texte vide".into()));
        }
        let when = match args.quand {
            When::Maintenant => SayWhen::Now,
            When::ProchaineLigneDroite => SayWhen::NextStraight,
            When::Position => {
                if args.virage.is_none() && args.position.is_none() {
                    return tool_err(ToolError(
                        "position : indiquer « virage » ou « position »".into(),
                    ));
                }
                SayWhen::At(Box::new(TriggerSpec {
                    id: None,
                    texte: text.clone(),
                    virage: args.virage,
                    ancre: args.ancre,
                    decalage_m: args.decalage_m,
                    position: args.position,
                    priorite: Some(8),
                    repetition: None,
                    condition: None,
                    expire_apres_tours: None,
                    retard_max_m: None,
                }))
            }
        };
        self.data.say(text, when);
        self.data
            .tool_called("say", true, t.elapsed().as_secs_f64() * 1e3);
        ok_json(json!({ "programme": true }))
    }
}

/// Serveur en cours d'exécution.
pub struct McpServerHandle {
    pub url: String,
    pub token: String,
    pub addr: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl McpServerHandle {
    /// Démarre le serveur sur `127.0.0.1:<port libre>`.
    pub async fn start(data: Arc<dyn CoachData>) -> std::io::Result<Self> {
        let mut raw = [0u8; 24];
        getrandom::fill(&mut raw).map_err(|e| std::io::Error::other(e.to_string()))?;
        let token: String = raw.iter().map(|b| format!("{b:02x}")).collect();

        let service = StreamableHttpService::new(
            move || Ok(CoachMcp { data: data.clone() }),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default(),
        );
        let expected = format!("Bearer {token}");
        let auth = move |req: Request, next: Next| {
            let expected = expected.clone();
            async move {
                let ok = req
                    .headers()
                    .get(axum::http::header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    == Some(expected.as_str());
                if ok {
                    next.run(req).await
                } else {
                    Response::from(StatusCode::UNAUTHORIZED.into_response())
                }
            }
        };
        let app = axum::Router::new()
            .nest_service("/mcp", service)
            .layer(axum::middleware::from_fn(auth));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let r = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
            if let Err(e) = r {
                tracing::warn!("serveur MCP : {e}");
            }
        });
        Ok(McpServerHandle {
            url: format!("http://{addr}/mcp"),
            token,
            addr,
            shutdown: Some(tx),
        })
    }
}

impl Drop for McpServerHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}
