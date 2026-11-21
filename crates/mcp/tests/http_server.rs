//! The MCP server served over HTTP, queried by a real MCP client.

use std::sync::{Arc, Mutex};

use analysis::Analyzer;
use mcp::{CoachData, CoachingStatus, McpServerHandle, TurnStatus};
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::{Value, json};
use telemetry::synthetic::SyntheticTrack;
use telemetry::{CornerDetectionParams, Lap, LapFlags, LiveSnapshot};
use triggers::{Plan, SayWhen};

#[derive(Default)]
struct Fake {
    snap: LiveSnapshot,
    analyzer: Option<Arc<Analyzer>>,
    plan: Mutex<Option<Plan>>,
    said: Mutex<Vec<String>>,
    calls: Mutex<Vec<String>>,
    coaching: Option<CoachingStatus>,
}

impl CoachData for Fake {
    fn snapshot(&self) -> LiveSnapshot {
        self.snap.clone()
    }
    fn analyzer(&self) -> Option<Arc<Analyzer>> {
        self.analyzer.clone()
    }
    fn coach_plan(&self) -> Option<Plan> {
        self.plan.lock().unwrap().clone()
    }
    fn apply_coach_plan(&self, plan: Plan) {
        *self.plan.lock().unwrap() = Some(plan);
    }
    fn say(&self, text: String, _when: SayWhen) {
        self.said.lock().unwrap().push(text);
    }
    fn tool_called(&self, name: &str, _ok: bool, _millis: f64) {
        self.calls.lock().unwrap().push(name.to_string());
    }
    fn coaching(&self) -> Option<CoachingStatus> {
        self.coaching.clone()
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

fn fake() -> Arc<Fake> {
    fake_with(None)
}

fn fake_with(coaching: Option<CoachingStatus>) -> Arc<Fake> {
    let track = SyntheticTrack::demo();
    let reference = lap(track.simulate_lap(1.0, 0.0, 0.0), 0);
    let analyzer = Analyzer::new(
        &reference,
        Some(track.length_m()),
        &CornerDetectionParams::default(),
    );
    Arc::new(Fake {
        snap: LiveSnapshot {
            laps: vec![lap(track.simulate_lap(0.96, 10.0, 0.0), 1)],
            ..Default::default()
        },
        analyzer: Some(Arc::new(analyzer)),
        coaching,
        ..Default::default()
    })
}

fn text_of(r: &rmcp::model::CallToolResult) -> String {
    serde_json::to_value(&r.content).unwrap()[0]["text"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tools_over_http_with_bearer_token() {
    let data = fake();
    let server = McpServerHandle::start(data.clone()).await.unwrap();
    assert!(server.url.starts_with("http://127.0.0.1:"));

    // Without a token: rejected.
    let resp = reqwest_status(&server.url, None).await;
    assert_eq!(resp, 401);

    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(server.url.clone())
            .auth_header(server.token.clone()),
    );
    let client = ().serve(transport).await.expect("MCP connection");
    let tools = client.list_all_tools().await.unwrap();
    let mut names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "get_corner_summary",
            "get_corner_trace",
            "get_session_overview",
            "say",
            "update_plan"
        ]
    );

    let r = client
        .call_tool(CallToolRequestParams::new("get_session_overview"))
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&text_of(&r)).unwrap();
    assert_eq!(v["time_lost_by_corner"].as_array().unwrap().len(), 6);

    let args = |v: Value| v.as_object().unwrap().clone();
    let r = client
        .call_tool(
            CallToolRequestParams::new("get_corner_trace").with_arguments(args(json!({
                "corner": "T4", "channels": ["speed", "delta"]
            }))),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&text_of(&r)).unwrap();
    assert!(v["points"].as_u64().unwrap() <= 100);

    // Domain error: readable error result, not a protocol error.
    let r = client
        .call_tool(
            CallToolRequestParams::new("get_corner_summary")
                .with_arguments(args(json!({"corner": "T42"}))),
        )
        .await
        .unwrap();
    assert_eq!(r.is_error, Some(true));
    assert!(text_of(&r).contains("unknown corner"));

    let r = client
        .call_tool(CallToolRequestParams::new("update_plan").with_arguments(args(json!({
            "triggers": [{"id": "t5", "corner": "T5", "offset_m": -200, "text": "Carry more speed."}],
            "mode": "replace"
        }))))
        .await
        .unwrap();
    assert_ne!(r.is_error, Some(true), "{}", text_of(&r));
    assert_eq!(
        data.plan.lock().unwrap().as_ref().unwrap().triggers.len(),
        1
    );
    let v: Value = serde_json::from_str(&text_of(&r)).unwrap();
    assert!(v["active_triggers"][0]["speech_s"].as_f64().unwrap() > 1.0);
    assert_eq!(v["warnings"], json!([]), "a short cue fits");

    // A cue far too long for any straight: accepted, with a warning.
    let long = "Brake later and carry more speed, ".repeat(10);
    let r = client
        .call_tool(
            CallToolRequestParams::new("update_plan").with_arguments(args(json!({
                "triggers": [{"id": "t5", "corner": "T5", "text": long}],
                "mode": "replace"
            }))),
        )
        .await
        .unwrap();
    assert_ne!(r.is_error, Some(true), "{}", text_of(&r));
    let v: Value = serde_json::from_str(&text_of(&r)).unwrap();
    let w = v["warnings"][0].as_str().unwrap();
    assert!(w.contains("t5") && w.contains("shorten it"), "{w}");

    let r = client
        .call_tool(
            CallToolRequestParams::new("update_plan").with_arguments(args(json!({
                "triggers": [{"corner": "T99", "text": "x"}]
            }))),
        )
        .await
        .unwrap();
    assert_eq!(r.is_error, Some(true));
    assert_eq!(
        data.plan.lock().unwrap().as_ref().unwrap().triggers.len(),
        1,
        "plan unchanged"
    );

    client
        .call_tool(
            CallToolRequestParams::new("say")
                .with_arguments(args(json!({"text": "Looking at T5."}))),
        )
        .await
        .unwrap();
    assert_eq!(*data.said.lock().unwrap(), vec!["Looking at T5."]);
    assert!(data.calls.lock().unwrap().len() >= 5);
    client.cancel().await.unwrap();
}

async fn reqwest_status(url: &str, token: Option<&str>) -> u16 {
    // Raw HTTP request (initialize) to check authentication.
    let mut req = reqwest::Client::new()
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    req.send().await.unwrap().status().as_u16()
}

#[test]
fn merge_plan_replaces_same_id() {
    use mcp::server::{PlanMode, merge_plan};
    use triggers::TriggerSpec;
    let spec = |id: &str, t: &str| TriggerSpec {
        id: Some(id.into()),
        text: t.into(),
        corner: None,
        anchor: None,
        offset_m: None,
        position: Some(0.1),
        priority: None,
        repeat: None,
        condition: None,
        expire_after_laps: None,
        max_delay_m: None,
    };
    let p = merge_plan(None, vec![spec("a", "1"), spec("b", "2")], PlanMode::Add);
    let p = merge_plan(Some(p), vec![spec("a", "new")], PlanMode::Add);
    assert_eq!(p.triggers.len(), 2);
    assert_eq!(p.triggers[0].text, "new");
    let p = merge_plan(Some(p), vec![spec("c", "3")], PlanMode::Replace);
    assert_eq!(p.triggers.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overview_carries_the_turns() {
    let status = CoachingStatus {
        turns: vec![TurnStatus {
            corner: "T4".into(),
            passes: 3,
            last_s: Some(0.21),
            best_s: Some(0.15),
            queued: true,
            analyzing: false,
        }],
    };
    let server = McpServerHandle::start(fake_with(Some(status)))
        .await
        .unwrap();
    let client = ()
        .serve(StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(server.url.clone())
                .auth_header(server.token.clone()),
        ))
        .await
        .unwrap();
    let r = client
        .call_tool(CallToolRequestParams::new("get_session_overview"))
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&text_of(&r)).unwrap();
    assert_eq!(v["turns"][0]["corner"], "T4");
    assert_eq!(v["turns"][0]["best_s"], 0.15);
    assert_eq!(v["turns"][0]["queued"], true);
    client.cancel().await.unwrap();
}
