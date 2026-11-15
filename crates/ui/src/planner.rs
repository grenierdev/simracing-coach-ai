//! Planification automatique du tour suivant par la session d'analyse.
//!
//! À chaque tour complet et valide, le bilan (vue d'ensemble de la session et
//! consignes actives du coach) part à une session ACP distincte de la
//! conversation, avec un modèle plus fort. Claude analyse les virages
//! coûteux avec les outils MCP, puis remplace la couche de consignes du coach
//! (`update_plan`). Le plan du pilote n'est jamais modifié. L'analyse ne
//! bloque ni la boucle rapide ni la conversation ; un tour terminé pendant
//! une analyse est ignoré.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use coach::{AgentStatus, CoachAgent, CoachEvent};
use crossbeam_channel::{Receiver, select};
use telemetry::{Lap, LiveSnapshot, TelemetryEvent};
use triggers::{Plan, TriggerRuntime};

#[derive(Debug, Clone, Copy)]
pub struct PlannerPolicy {
    /// Une analyse tous les N tours valides.
    pub every_n_laps: u32,
}

impl Default for PlannerPolicy {
    fn default() -> Self {
        PlannerPolicy { every_n_laps: 1 }
    }
}

/// Faut-il analyser ce tour ? `valid_since_last` : tours valides depuis la
/// dernière analyse, celui-ci compris.
pub fn should_analyze(
    lap: &Lap,
    valid_since_last: u32,
    busy: bool,
    policy: &PlannerPolicy,
) -> bool {
    lap.is_valid() && !busy && valid_since_last >= policy.every_n_laps.max(1)
}

fn lap_time(secs: f64) -> String {
    let ms = (secs * 1000.0).round() as u64;
    format!("{}:{:02}.{:03}", ms / 60_000, (ms / 1000) % 60, ms % 1000)
}

/// Message envoyé à la session d'analyse.
pub fn analysis_prompt(
    lap: &Lap,
    overview: &serde_json::Value,
    coach_plan: Option<&Plan>,
) -> String {
    let active = match coach_plan {
        Some(p) if !p.consignes.is_empty() => p
            .consignes
            .iter()
            .map(|c| {
                format!(
                    "{} ({}) : « {} »",
                    c.id.as_deref().unwrap_or("?"),
                    c.virage.as_deref().unwrap_or("position"),
                    c.texte
                )
            })
            .collect::<Vec<_>>()
            .join(" ; "),
        _ => "aucune".into(),
    };
    format!(
        "[Analyse] Tour {} terminé en {}.\n\
         Vue d'ensemble (get_session_overview) : {}\n\
         Consignes du coach actives : {}\n\
         Prépare les consignes du tour suivant (update_plan, mode remplacer, au plus trois), puis résume en une phrase.",
        lap.number,
        lap_time(lap.lap_time),
        overview,
        active
    )
}

#[derive(Debug, Clone, Default)]
pub struct PlannerStatus {
    pub enabled: bool,
    pub model: String,
    /// Tour en cours d'analyse.
    pub running: Option<i32>,
    /// Dernière analyse : tour, résumé, durée (s).
    pub last: Option<(i32, String, f64)>,
    pub analyses: u32,
    pub error: Option<String>,
}

pub struct Planner {
    status: Arc<Mutex<PlannerStatus>>,
    enabled: Arc<AtomicBool>,
}

pub type SnapshotFn = Arc<dyn Fn() -> LiveSnapshot + Send + Sync>;

impl Planner {
    pub fn spawn(
        agent: Arc<CoachAgent>,
        model: String,
        laps: Receiver<TelemetryEvent>,
        snapshot: SnapshotFn,
        triggers: Arc<TriggerRuntime>,
        policy: PlannerPolicy,
        enabled: bool,
    ) -> Self {
        let status = Arc::new(Mutex::new(PlannerStatus {
            enabled,
            model,
            ..Default::default()
        }));
        let flag = Arc::new(AtomicBool::new(enabled));
        {
            let (status, flag) = (status.clone(), flag.clone());
            std::thread::Builder::new()
                .name("planner".into())
                .spawn(move || run(agent, laps, snapshot, triggers, policy, status, flag))
                .expect("démarrage du planificateur");
        }
        Planner {
            status,
            enabled: flag,
        }
    }

    pub fn status(&self) -> PlannerStatus {
        self.status.lock().unwrap().clone()
    }

    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
        self.status.lock().unwrap().enabled = on;
    }
}

fn run(
    agent: Arc<CoachAgent>,
    laps: Receiver<TelemetryEvent>,
    snapshot: SnapshotFn,
    triggers: Arc<TriggerRuntime>,
    policy: PlannerPolicy,
    status: Arc<Mutex<PlannerStatus>>,
    enabled: Arc<AtomicBool>,
) {
    let events = agent.events().clone();
    let mut valid_since_last: u32 = 0;
    let mut current: Option<(u64, i32, Instant, String)> = None;
    loop {
        select! {
            recv(laps) -> e => match e {
                Ok(TelemetryEvent::LapCompleted(lap)) => {
                    if lap.is_valid() {
                        valid_since_last += 1;
                    }
                    let busy = current.is_some();
                    if !enabled.load(Ordering::Relaxed)
                        || !should_analyze(&lap, valid_since_last, busy, &policy)
                    {
                        continue;
                    }
                    let Some(analyzer) = triggers.analyzer() else {
                        status.lock().unwrap().error = Some("pas de tour de référence".into());
                        continue;
                    };
                    let overview = mcp::tools::session_overview(&snapshot(), Some(&analyzer));
                    let prompt = analysis_prompt(&lap, &overview, triggers.coach_plan().as_ref());
                    let turn = agent.prompt(prompt);
                    valid_since_last = 0;
                    current = Some((turn, lap.number, Instant::now(), String::new()));
                    let mut s = status.lock().unwrap();
                    s.running = Some(lap.number);
                    s.error = None;
                }
                Ok(_) => {}
                Err(_) => break,
            },
            recv(events) -> e => match e {
                Ok(CoachEvent::Chunk { turn, text }) => {
                    if let Some((t, _, _, summary)) = current.as_mut()
                        && *t == turn
                    {
                        summary.push_str(&text);
                    }
                }
                Ok(CoachEvent::TurnEnd { turn, .. }) => {
                    if let Some((t, lap, started, summary)) = current.take() {
                        if t != turn {
                            current = Some((t, lap, started, summary));
                            continue;
                        }
                        // Le résumé est la dernière phrase de la réponse.
                        let summary = summary
                            .trim()
                            .rsplit('\n')
                            .find(|l| !l.trim().is_empty())
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        let mut s = status.lock().unwrap();
                        s.running = None;
                        s.analyses += 1;
                        s.last = Some((lap, summary, started.elapsed().as_secs_f64()));
                    }
                }
                Ok(CoachEvent::Status(AgentStatus::Failed(err))) => {
                    let mut s = status.lock().unwrap();
                    s.running = None;
                    s.error = Some(err);
                    current = None;
                }
                Ok(CoachEvent::Error(err)) => status.lock().unwrap().error = Some(err),
                Ok(_) => {}
                Err(_) => break,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use telemetry::LapFlags;

    fn lap(valid: bool) -> Lap {
        Lap {
            number: 4,
            start_time: 0.0,
            lap_time: 83.456,
            flags: LapFlags {
                complete: true,
                off_track: if valid { 0 } else { 1 },
                ..Default::default()
            },
            samples: vec![],
        }
    }

    #[test]
    fn policy() {
        let p = PlannerPolicy { every_n_laps: 2 };
        assert!(!should_analyze(&lap(true), 1, false, &p));
        assert!(should_analyze(&lap(true), 2, false, &p));
        assert!(
            !should_analyze(&lap(true), 2, true, &p),
            "analyse déjà en cours"
        );
        assert!(!should_analyze(&lap(false), 3, false, &p), "tour invalide");
    }

    #[test]
    fn prompt_lists_overview_and_active_consignes() {
        let plan =
            Plan::parse("[[consigne]]\nid = \"auto-T4\"\nvirage = \"T4\"\ntexte = \"Patience.\"")
                .unwrap();
        let p = analysis_prompt(&lap(true), &serde_json::json!({"x": 1}), Some(&plan));
        assert!(p.starts_with("[Analyse] Tour 4 terminé en 1:23.456."));
        assert!(p.contains("{\"x\":1}"));
        assert!(p.contains("auto-T4 (T4) : « Patience. »"));
        assert!(
            analysis_prompt(&lap(true), &serde_json::json!({}), None).contains("actives : aucune")
        );
    }
}
