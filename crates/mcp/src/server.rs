//! rmcp server: tool declarations and HTTP service.

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

use crate::tools::{self, ToolError};
use crate::{CoachData, FocusCommand};

/// Server name in `mcpServers`: Claude sees the tools as `mcp__coach__<tool>`.
pub const SERVER_NAME: &str = "coach";

#[derive(Clone)]
pub struct CoachMcp {
    data: Arc<dyn CoachData>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CornerSummaryArgs {
    /// Corner: "T5" (or "5").
    pub corner: String,
    /// Lap numbers to analyze; defaults to the last 3 complete laps.
    #[serde(default)]
    pub laps: Option<Vec<i32>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CornerTraceArgs {
    /// Corner: "T5" (or "5").
    pub corner: String,
    /// Channels: speed, throttle, brake, steering, yaw_rate, lat_accel, gear,
    /// delta, radius (path radius), line_offset (lateral offset of the line
    /// against the reference, > 0 = inside).
    /// Default: speed, throttle, brake.
    #[serde(default)]
    pub channels: Vec<String>,
    /// Lap number; defaults to the last complete lap.
    #[serde(default)]
    pub lap: Option<i32>,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanMode {
    /// Replaces the whole coach plan.
    Replace,
    /// Adds the triggers; a trigger with the same `id` is replaced.
    #[default]
    Add,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdatePlanArgs {
    /// Triggers to place on the lap.
    pub triggers: Vec<TriggerSpec>,
    /// "replace" or "add" (default).
    #[serde(default)]
    pub mode: PlanMode,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetFocusArgs {
    /// One corner, or two consecutive corners: ["T5"] or ["T5", "T6"].
    pub corners: Vec<String>,
    /// Short sentence telling the driver what to work on, spoken on the next
    /// straight ("Let's work on turns five and six: the exit of five sets up six.").
    #[serde(default)]
    pub announcement: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NextFocusArgs {
    /// Why: "the driver asked", "gap closed", "no more progress".
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum When {
    /// As soon as possible, outside braking zones.
    #[default]
    Now,
    /// On the next stretch where the sentence has time to be spoken.
    NextStraight,
    /// At a position: `corner` (+ `anchor`, `offset_m`) or `position`.
    Position,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SayArgs {
    /// Sentence to speak: short, plain, in the voice language.
    pub text: String,
    #[serde(default)]
    pub when: When,
    /// For `when = position`: reference corner ("T5").
    #[serde(default)]
    pub corner: Option<String>,
    /// For `when = position`: braking, entry, apex or exit.
    #[serde(default)]
    pub anchor: Option<triggers::Anchor>,
    /// For `when = position`: offset in meters (negative = before).
    #[serde(default)]
    pub offset_m: Option<f64>,
    /// For `when = position`: absolute position (LapDistPct, 0 to 1).
    #[serde(default)]
    pub position: Option<f64>,
}

fn ok_json(v: Value) -> Result<CallToolResult, ErrorData> {
    Ok(CallToolResult::success(vec![Content::text(v.to_string())]))
}

fn tool_err(e: ToolError) -> Result<CallToolResult, ErrorData> {
    Ok(CallToolResult::error(vec![Content::text(e.0)]))
}

/// Applies `specs` to the current coach plan according to `mode`.
pub fn merge_plan(current: Option<Plan>, specs: Vec<TriggerSpec>, mode: PlanMode) -> Plan {
    let mut plan = match mode {
        PlanMode::Replace => Plan {
            settings: current.map(|p| p.settings).unwrap_or_default(),
            triggers: vec![],
        },
        PlanMode::Add => current.unwrap_or_default(),
    };
    for spec in specs {
        if let Some(id) = &spec.id
            && let Some(existing) = plan
                .triggers
                .iter_mut()
                .find(|c| c.id.as_deref() == Some(id.as_str()))
        {
            *existing = spec;
            continue;
        }
        plan.triggers.push(spec);
    }
    plan
}

#[tool_router(server_handler)]
impl CoachMcp {
    #[tool(
        description = "Session overview: recent laps, delta to the reference and time lost per corner, ranked from most to least costly."
    )]
    async fn get_session_overview(&self) -> Result<CallToolResult, ErrorData> {
        let t = Instant::now();
        let a = self.data.analyzer();
        let mut v = tools::session_overview(&self.data.snapshot(), a.as_deref());
        if let Some(focus) = self.data.focus() {
            v["focus"] = json!(focus.status());
        }
        self.data.tool_called(
            "get_session_overview",
            true,
            t.elapsed().as_secs_f64() * 1e3,
        );
        ok_json(v)
    }

    #[tool(
        description = "Corner metrics against the reference, per lap: time lost, braking point, trail braking, minimum speed, throttle pickup, coasting, lateral acceleration, rotation ratios (entry and exit), turn-in point, path radius (tightest and in the core), steering angle (peak and mean), and the line's lateral offset at turn-in, apex and exit."
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
        description = "Corner traces (at most 100 points per channel) for a lap and for the reference, as a function of distance from the corner entry."
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
        description = "Replaces or extends the coach triggers spoken automatically on the next laps (the driver's own plan is never modified). Each trigger is placed by corner (anchor + offset in meters) or by absolute position. The coach always stays silent in braking zones."
    )]
    async fn update_plan(
        &self,
        Parameters(args): Parameters<UpdatePlanArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let t = Instant::now();
        let plan = merge_plan(self.data.coach_plan(), args.triggers, args.mode);
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
                let fits = analyzer
                    .as_deref()
                    .map(|a| triggers::check_fit(&resolved, a, &plan.settings))
                    .unwrap_or_default();
                let summary: Vec<Value> = resolved
                    .iter()
                    .map(|t| {
                        let fit = fits.iter().find(|f| f.id == t.id);
                        json!({
                            "id": t.id,
                            "corner": t.corner,
                            "position_pct": (t.at_pct * 1000.0).round() / 10.0,
                            "text": t.text,
                            "speech_s": fit.map(|f| (f.needed_s * 10.0).round() / 10.0),
                            "room_s": fit
                                .filter(|f| f.room_s.is_finite())
                                .map(|f| (f.room_s * 10.0).round() / 10.0),
                        })
                    })
                    .collect();
                let warnings: Vec<String> = fits.iter().filter_map(|f| f.problem()).collect();
                self.data.apply_coach_plan(plan);
                ok_json(json!({ "active_triggers": summary, "warnings": warnings }))
            }
            Err(e) => tool_err(ToolError(format!("plan rejected: {e}"))),
        }
    }

    #[tool(
        description = "Speaks a sentence to the driver: now (as soon as possible outside braking), on the next straight, or at a position on the lap."
    )]
    async fn say(
        &self,
        Parameters(args): Parameters<SayArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let t = Instant::now();
        let text = args.text.trim().to_string();
        if text.is_empty() {
            return tool_err(ToolError("empty text".into()));
        }
        let when = match args.when {
            When::Now => SayWhen::Now,
            When::NextStraight => SayWhen::NextStraight,
            When::Position => {
                if args.corner.is_none() && args.position.is_none() {
                    return tool_err(ToolError(
                        "position: give \"corner\" or \"position\"".into(),
                    ));
                }
                SayWhen::At(Box::new(TriggerSpec {
                    id: None,
                    text: text.clone(),
                    corner: args.corner,
                    anchor: args.anchor,
                    offset_m: args.offset_m,
                    position: args.position,
                    priority: Some(8),
                    repeat: None,
                    condition: None,
                    expire_after_laps: None,
                    max_delay_m: None,
                }))
            }
        };
        self.data.say(text, when);
        self.data
            .tool_called("say", true, t.elapsed().as_secs_f64() * 1e3);
        ok_json(json!({ "scheduled": true }))
    }

    #[tool(
        description = "Sets the corners the driver works on now: one corner, or two consecutive corners. From then on, every pass through them is analyzed as soon as the car leaves the last one, and you receive the result. The announcement is spoken to the driver on the next straight."
    )]
    async fn set_focus(
        &self,
        Parameters(args): Parameters<SetFocusArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let t = Instant::now();
        let r = self.set_focus_inner(args);
        self.data
            .tool_called("set_focus", r.is_ok(), t.elapsed().as_secs_f64() * 1e3);
        r.map_or_else(tool_err, ok_json)
    }

    #[tool(
        description = "Leaves the current focus corners and moves on to the next finding: call it when the driver asks to work on something else, or when the corners have nothing more to give. A new choice of corners follows."
    )]
    async fn next_focus(
        &self,
        Parameters(args): Parameters<NextFocusArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let t = Instant::now();
        let r = match self.data.focus() {
            None => Err(ToolError(
                "focus coaching is not running (automatic planning off)".into(),
            )),
            Some(f) => {
                let reason = args.reason.unwrap_or_else(|| "moving on".into());
                f.commands
                    .send(FocusCommand::Next { reason })
                    .map(|_| json!({ "ok": true }))
                    .map_err(|_| ToolError("focus coaching stopped".into()))
            }
        };
        self.data
            .tool_called("next_focus", r.is_ok(), t.elapsed().as_secs_f64() * 1e3);
        r.map_or_else(tool_err, ok_json)
    }
}

impl CoachMcp {
    fn set_focus_inner(&self, args: SetFocusArgs) -> Result<Value, ToolError> {
        let focus = self.data.focus().ok_or_else(|| {
            ToolError("focus coaching is not running (automatic planning off)".into())
        })?;
        let analyzer = self
            .data
            .analyzer()
            .ok_or_else(|| ToolError("no reference lap loaded".into()))?;
        if args.corners.is_empty() || args.corners.len() > 2 {
            return Err(ToolError("give one or two corners".into()));
        }
        let mut idx = args
            .corners
            .iter()
            .map(|c| tools::find_corner(&analyzer, c))
            .collect::<Result<Vec<_>, _>>()?;
        idx.sort_unstable();
        idx.dedup();
        let labels: Vec<String> = idx
            .iter()
            .map(|&i| analyzer.model.corners[i].label())
            .collect();
        let announcement = args
            .announcement
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty());
        focus
            .commands
            .send(FocusCommand::Set {
                corners: idx,
                announcement,
            })
            .map_err(|_| ToolError("focus coaching stopped".into()))?;
        Ok(json!({ "focus": labels }))
    }
}

/// Running server.
pub struct McpServerHandle {
    pub url: String,
    pub token: String,
    pub addr: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl McpServerHandle {
    /// Starts the server on `127.0.0.1:<free port>`.
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
                tracing::warn!("MCP server: {e}");
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
