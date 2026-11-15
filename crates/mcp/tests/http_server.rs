//! Le serveur MCP servi en HTTP, interrogé par un vrai client MCP.

use std::sync::{Arc, Mutex};

use analysis::Analyzer;
use mcp::{CoachData, McpServerHandle};
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
    plan: Mutex<Option<(String, Plan)>>,
    said: Mutex<Vec<String>>,
    calls: Mutex<Vec<String>>,
}

impl CoachData for Fake {
    fn snapshot(&self) -> LiveSnapshot {
        self.snap.clone()
    }
    fn analyzer(&self) -> Option<Arc<Analyzer>> {
        self.analyzer.clone()
    }
    fn current_plan(&self) -> Option<(String, Plan)> {
        self.plan.lock().unwrap().clone()
    }
    fn apply_plan(&self, name: String, plan: Plan) {
        *self.plan.lock().unwrap() = Some((name, plan));
    }
    fn say(&self, text: String, _when: SayWhen) {
        self.said.lock().unwrap().push(text);
    }
    fn tool_called(&self, name: &str, _ok: bool, _millis: f64) {
        self.calls.lock().unwrap().push(name.to_string());
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

    // Sans jeton : refusé.
    let resp = reqwest_status(&server.url, None).await;
    assert_eq!(resp, 401);

    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(server.url.clone())
            .auth_header(server.token.clone()),
    );
    let client = ().serve(transport).await.expect("connexion MCP");
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
    assert_eq!(v["temps_perdu_par_virage"].as_array().unwrap().len(), 6);

    let args = |v: Value| v.as_object().unwrap().clone();
    let r = client
        .call_tool(
            CallToolRequestParams::new("get_corner_trace").with_arguments(args(json!({
                "corner": "T4", "channels": ["vitesse", "delta"]
            }))),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&text_of(&r)).unwrap();
    assert!(v["points"].as_u64().unwrap() <= 100);

    // Erreur métier : résultat d'erreur lisible, pas une erreur de protocole.
    let r = client
        .call_tool(
            CallToolRequestParams::new("get_corner_summary")
                .with_arguments(args(json!({"corner": "T42"}))),
        )
        .await
        .unwrap();
    assert_eq!(r.is_error, Some(true));
    assert!(text_of(&r).contains("virage inconnu"));

    let r = client
        .call_tool(CallToolRequestParams::new("update_plan").with_arguments(args(json!({
            "consignes": [{"id": "t5", "virage": "T5", "decalage_m": -200, "texte": "Garde de la vitesse."}],
            "mode": "remplacer"
        }))))
        .await
        .unwrap();
    assert_ne!(r.is_error, Some(true), "{}", text_of(&r));
    assert_eq!(
        data.plan
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .1
            .consignes
            .len(),
        1
    );

    let r = client
        .call_tool(
            CallToolRequestParams::new("update_plan").with_arguments(args(json!({
                "consignes": [{"virage": "T99", "texte": "x"}]
            }))),
        )
        .await
        .unwrap();
    assert_eq!(r.is_error, Some(true));
    assert_eq!(
        data.plan
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .1
            .consignes
            .len(),
        1,
        "plan inchangé"
    );

    client
        .call_tool(
            CallToolRequestParams::new("say")
                .with_arguments(args(json!({"texte": "Je regarde le T5."}))),
        )
        .await
        .unwrap();
    assert_eq!(*data.said.lock().unwrap(), vec!["Je regarde le T5."]);
    assert!(data.calls.lock().unwrap().len() >= 5);
    client.cancel().await.unwrap();
}

async fn reqwest_status(url: &str, token: Option<&str>) -> u16 {
    // Requête HTTP brute (initialize) pour vérifier l'authentification.
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
        texte: t.into(),
        virage: None,
        ancre: None,
        decalage_m: None,
        position: Some(0.1),
        priorite: None,
        repetition: None,
        condition: None,
        expire_apres_tours: None,
        retard_max_m: None,
    };
    let p = merge_plan(
        None,
        vec![spec("a", "1"), spec("b", "2")],
        PlanMode::Ajouter,
    );
    let p = merge_plan(Some(p), vec![spec("a", "nouveau")], PlanMode::Ajouter);
    assert_eq!(p.consignes.len(), 2);
    assert_eq!(p.consignes[0].texte, "nouveau");
    let p = merge_plan(Some(p), vec![spec("c", "3")], PlanMode::Remplacer);
    assert_eq!(p.consignes.len(), 1);
}
