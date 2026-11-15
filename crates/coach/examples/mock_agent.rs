//! Faux agent ACP sur stdio, pour développer sans Claude.
//!
//! Il répond comme le coach : un accusé, un vrai appel à `get_session_overview`
//! sur le serveur MCP du programme, puis une phrase sur le virage le plus coûteux.
//!
//! ```sh
//! cargo build -p coach --example mock_agent
//! SIMCOACH_ACP_COMMAND=target/debug/examples/mock_agent simcoach --synthetic
//! ```

use std::sync::{Arc, Mutex};

use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    AgentCapabilities, ContentBlock, ContentChunk, Implementation, InitializeRequest,
    InitializeResponse, McpServer, NewSessionRequest, NewSessionResponse, PromptRequest,
    PromptResponse, SessionNotification, SessionUpdate, StopReason, TextContent, ToolCall,
};
use agent_client_protocol::{Agent, Client, Responder, Stdio, V1};
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;

fn chunk(t: &str) -> SessionUpdate {
    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(t))))
}

async fn overview(url: String, token: String) -> Result<serde_json::Value, String> {
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(url).auth_header(token),
    );
    let client = ().serve(transport).await.map_err(|e| e.to_string())?;
    let r = client
        .call_tool(CallToolRequestParams::new("get_session_overview"))
        .await
        .map_err(|e| e.to_string())?;
    let body = serde_json::to_value(&r.content).map_err(|e| e.to_string())?;
    let _ = client.cancel().await;
    serde_json::from_str(body[0]["text"].as_str().unwrap_or("{}")).map_err(|e| e.to_string())
}

#[tokio::main]
async fn main() -> agent_client_protocol::Result<()> {
    let mcp: Arc<Mutex<Option<(String, String)>>> = Arc::default();
    let mcp_new = mcp.clone();
    Agent
        .v1()
        .on_receive_request(
            async move |req: InitializeRequest, responder: Responder<InitializeResponse, V1>, _cx: ConnectionTo<Client>| {
                responder.respond(
                    InitializeResponse::new(req.protocol_version)
                        .agent_capabilities(AgentCapabilities::new())
                        .agent_info(Implementation::new("faux-coach", "0.1")),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: NewSessionRequest, responder: Responder<NewSessionResponse, V1>, _cx: ConnectionTo<Client>| {
                if let Some(McpServer::Http(h)) = req.mcp_servers.first() {
                    let token = h
                        .headers
                        .first()
                        .map(|h| h.value.trim_start_matches("Bearer ").to_string())
                        .unwrap_or_default();
                    *mcp_new.lock().unwrap() = Some((h.url.clone(), token));
                }
                responder.respond(NewSessionResponse::new("faux"))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: PromptRequest, responder: Responder<PromptResponse, V1>, cx: ConnectionTo<Client>| {
                let endpoint = mcp.lock().unwrap().clone();
                let sid = req.session_id.clone();
                tokio::spawn(async move {
                    let send = |u| cx.send_notification(SessionNotification::new(sid.clone(), u));
                    let _ = send(chunk("Je regarde. "));
                    let reply = match endpoint {
                        Some((url, token)) => {
                            let _ = send(SessionUpdate::ToolCall(ToolCall::new("t1", "mcp__coach__get_session_overview")));
                            match overview(url, token).await {
                                Ok(v) => match v["temps_perdu_par_virage"].get(0) {
                                    Some(w) => format!(
                                        "Le plus coûteux, c'est {} : {:.0} centièmes perdus en moyenne.",
                                        w["virage"].as_str().unwrap_or("?"),
                                        w["perte_s"].as_f64().unwrap_or(0.0) * 100.0
                                    ),
                                    None => "Pas encore de tour complet à comparer.".into(),
                                },
                                Err(e) => format!("Télémétrie indisponible : {e}"),
                            }
                        }
                        None => "Pas de serveur MCP.".into(),
                    };
                    // Envoi par petits morceaux, comme un vrai flux.
                    for word in reply.split_inclusive(' ') {
                        let _ = send(chunk(word));
                        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                    }
                    let _ = responder.respond(PromptResponse::new(StopReason::EndTurn));
                });
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_to(Stdio::new())
        .await
}
