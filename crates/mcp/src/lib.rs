//! Serveur MCP embarqué dans le programme : expose la télémétrie et le plan
//! de consignes à Claude.
//!
//! Transport : HTTP « streamable » sur `127.0.0.1`, port éphémère, protégé par
//! un jeton Bearer aléatoire. L'URL et le jeton sont passés à l'agent dans
//! `mcpServers` lors du `session/new`.
//!
//! Outils :
//! - `get_session_overview()` : tours, delta, temps perdu par virage (classé) ;
//! - `get_corner_summary(corner, laps)` : métriques du virage face à la référence ;
//! - `get_corner_trace(corner, channels, lap)` : traces rééchantillonnées (≤ 100 points) ;
//! - `update_plan(consignes, mode)` : remplace ou complète les consignes à venir ;
//! - `say(texte, quand)` : phrase ponctuelle (maintenant, prochaine ligne droite, position).

pub mod server;
pub mod tools;

use std::sync::Arc;

use analysis::Analyzer;
use telemetry::LiveSnapshot;
use triggers::{Plan, SayWhen};

pub use server::{CoachMcp, McpServerHandle, SERVER_NAME};

/// Appel d'outil reçu, pour le journal de l'interface.
#[derive(Debug, Clone)]
pub struct ToolCallLog {
    /// Session qui a appelé l'outil (« conversation », « analyse »).
    pub source: String,
    pub name: String,
    pub ok: bool,
    pub millis: f64,
}

/// Accès du serveur MCP à l'état du programme.
pub trait CoachData: Send + Sync + 'static {
    fn snapshot(&self) -> LiveSnapshot;
    fn analyzer(&self) -> Option<Arc<Analyzer>>;
    /// Couche de consignes du coach (le plan du pilote n'est jamais modifié par Claude).
    fn coach_plan(&self) -> Option<Plan>;
    fn apply_coach_plan(&self, plan: Plan);
    fn say(&self, text: String, when: SayWhen);
    /// Notification d'un appel d'outil (journal de l'interface).
    fn tool_called(&self, _name: &str, _ok: bool, _millis: f64) {}
}
