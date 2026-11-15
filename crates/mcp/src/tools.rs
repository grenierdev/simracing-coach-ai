//! Calcul des réponses des outils, indépendant du protocole MCP.
//!
//! Unités exposées à Claude : km/h, %, degrés, g, mètres, secondes. Les valeurs
//! sont arrondies pour limiter les tokens.

use std::sync::Arc;

use analysis::{AlignedLap, Analyzer, CornerComparison, CornerMetrics, CornerZone};
use serde::Serialize;
use serde_json::{Value, json};
use telemetry::{Lap, LiveSnapshot};

/// Nombre maximal de points par canal dans une trace.
pub const MAX_TRACE_POINTS: usize = 100;
/// Tours analysés par défaut.
pub const DEFAULT_LAPS: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError(pub String);

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn err<T>(msg: impl Into<String>) -> Result<T, ToolError> {
    Err(ToolError(msg.into()))
}

fn r1(x: f64) -> Option<f64> {
    x.is_finite().then(|| (x * 10.0).round() / 10.0)
}
fn r2(x: f64) -> Option<f64> {
    x.is_finite().then(|| (x * 100.0).round() / 100.0)
}
fn r3(x: f64) -> Option<f64> {
    x.is_finite().then(|| (x * 1000.0).round() / 1000.0)
}
fn kmh(mps: f64) -> Option<f64> {
    r1(mps * 3.6)
}

fn lap_time(secs: f64) -> String {
    let ms = (secs * 1000.0).round() as u64;
    format!("{}:{:02}.{:03}", ms / 60_000, (ms / 1000) % 60, ms % 1000)
}

/// Retrouve un virage : « T5 », « t5 », « 5 », « virage 5 ».
pub fn find_corner(analyzer: &Analyzer, label: &str) -> Result<usize, ToolError> {
    let digits: String = label.chars().filter(|c| c.is_ascii_digit()).collect();
    let n: usize = digits.parse().unwrap_or(0);
    analyzer
        .model
        .corners
        .iter()
        .position(|c| c.number as usize == n)
        .ok_or_else(|| {
            ToolError(format!(
                "virage inconnu : « {label} » (virages : T1 à T{})",
                analyzer.model.corners.len()
            ))
        })
}

fn require_analyzer(a: Option<&Analyzer>) -> Result<&Analyzer, ToolError> {
    a.ok_or_else(|| ToolError("aucun tour de référence chargé".into()))
}

/// Tours complets, du plus récent au plus ancien. `numbers` : tours demandés.
fn select_laps<'a>(
    snap: &'a LiveSnapshot,
    numbers: Option<&[i32]>,
    count: usize,
) -> Vec<&'a Arc<Lap>> {
    let complete = snap.laps.iter().rev().filter(|l| l.flags.complete);
    match numbers {
        Some(ns) if !ns.is_empty() => complete.filter(|l| ns.contains(&l.number)).collect(),
        _ => complete.take(count).collect(),
    }
}

fn lap_status(l: &Lap) -> &'static str {
    let f = &l.flags;
    if f.pit {
        "stands"
    } else if f.discontinuity {
        "interrompu"
    } else if f.off_track > 0 {
        "hors piste"
    } else {
        "valide"
    }
}

#[derive(Serialize)]
struct CornerLoss {
    virage: String,
    /// Temps perdu moyen face à la référence, s (négatif = gagné).
    perte_s: Option<f64>,
    freinage_m: Option<f64>,
    vitesse_mini_kmh: Option<f64>,
    reprise_gaz_m: Option<f64>,
}

fn mean(v: impl Iterator<Item = f64>) -> Option<f64> {
    let (s, n) = v
        .filter(|x| x.is_finite())
        .fold((0.0, 0), |(s, n), x| (s + x, n + 1));
    (n > 0).then(|| s / n as f64)
}

/// `get_session_overview` : tours, delta à la référence, temps perdu par virage (classé).
pub fn session_overview(snap: &LiveSnapshot, analyzer: Option<&Analyzer>) -> Value {
    let session = snap.session.as_ref();
    let laps: Vec<Value> = snap
        .laps
        .iter()
        .rev()
        .filter(|l| l.flags.complete)
        .take(10)
        .map(|l| {
            json!({
                "tour": l.number,
                "temps": lap_time(l.lap_time),
                "delta_ref_s": analyzer.and_then(|a| r3(l.lap_time - a.reference.lap_time)),
                "etat": lap_status(l),
            })
        })
        .collect();
    let mut out = json!({
        "piste": session.map(|s| s.track_label()),
        "voiture": session.map(|s| s.car_screen_name.clone()),
        "tour_en_cours": snap.sample.map(|s| s.lap),
        "position_pct": snap.sample.and_then(|s| r1(s.lap_dist_pct as f64 * 100.0)),
        "meilleur_tour": snap.best_lap.as_ref().map(|l| json!({"tour": l.number, "temps": lap_time(l.lap_time)})),
        "derniers_tours": laps,
    });
    let Some(a) = analyzer else {
        out["reference"] = json!(null);
        out["note"] = json!("aucun tour de référence chargé : pas de comparaison par virage");
        return out;
    };
    out["reference"] =
        json!({ "temps": lap_time(a.reference.lap_time), "virages": a.model.corners.len() });
    let comps: Vec<_> = select_laps(snap, None, DEFAULT_LAPS)
        .into_iter()
        .map(|l| a.compare(l))
        .collect();
    if comps.is_empty() {
        out["temps_perdu_par_virage"] = json!([]);
        out["note"] = json!("aucun tour complet pour l'instant");
        return out;
    }
    let mut losses: Vec<CornerLoss> = (0..a.model.zones.len())
        .map(|i| {
            let cs: Vec<&CornerComparison> = comps.iter().map(|c| &c.corners[i]).collect();
            CornerLoss {
                virage: a.model.corners[i].label(),
                perte_s: mean(cs.iter().map(|c| c.time_delta)).and_then(r3),
                freinage_m: mean(cs.iter().filter_map(|c| c.brake_point_delta_m)).and_then(r1),
                vitesse_mini_kmh: mean(cs.iter().map(|c| c.min_speed_delta * 3.6)).and_then(r1),
                reprise_gaz_m: mean(cs.iter().filter_map(|c| c.throttle_on_delta_m)).and_then(r1),
            }
        })
        .collect();
    losses.sort_by(|a, b| {
        b.perte_s
            .unwrap_or(0.0)
            .total_cmp(&a.perte_s.unwrap_or(0.0))
    });
    out["tours_analyses"] = json!(comps.iter().map(|c| c.lap_number).collect::<Vec<_>>());
    out["temps_perdu_par_virage"] = json!(losses);
    out["conventions"] = json!(
        "perte_s > 0 : temps perdu ; freinage_m et reprise_gaz_m > 0 : plus tard que la référence ; vitesse_mini_kmh < 0 : plus lent"
    );
    out
}

fn metrics_json(m: &CornerMetrics, length_m: f64, corner_start: f64) -> Value {
    // Positions exprimées en mètres depuis le début géométrique du virage.
    let pos = |p: Option<f64>| {
        p.and_then(|p| r1(((p - corner_start + 0.5).rem_euclid(1.0) - 0.5) * length_m))
    };
    json!({
        "temps_zone_s": r3(m.zone_time),
        "freinage_m": pos(m.brake_point_pct),
        "frein_max_pct": r1(m.brake_peak * 100.0),
        "relachement_frein_m": pos(m.brake_release_pct),
        "trail_braking_m": r1(m.trail_brake_m),
        "vitesse_mini_kmh": kmh(m.min_speed),
        "apex_m": pos(Some(m.min_speed_pct)),
        "reprise_gaz_m": pos(m.throttle_on_pct),
        "pleins_gaz_m": pos(m.full_throttle_pct),
        "roue_libre_s": r2(m.coasting_s),
        "acc_lat_max_g": r2(m.peak_lat_accel / 9.81),
        "rotation_entree": r3(m.rotation_entry.unwrap_or(f64::NAN)),
        "rotation_sortie": r3(m.rotation_exit.unwrap_or(f64::NAN)),
    })
}

/// `get_corner_summary` : métriques du virage pour les tours demandés, face à la référence.
pub fn corner_summary(
    snap: &LiveSnapshot,
    analyzer: Option<&Analyzer>,
    corner: &str,
    laps: Option<&[i32]>,
) -> Result<Value, ToolError> {
    let a = require_analyzer(analyzer)?;
    let i = find_corner(a, corner)?;
    let c = &a.model.corners[i];
    let l = a.model.length_m;
    let selected = select_laps(snap, laps, DEFAULT_LAPS);
    if selected.is_empty() {
        return err("aucun tour complet correspondant");
    }
    let per_lap: Vec<Value> = selected
        .iter()
        .map(|lap| {
            let cmp = a.compare(lap);
            let cc = &cmp.corners[i];
            json!({
                "tour": lap.number,
                "etat": lap_status(lap),
                "perte_s": r3(cc.time_delta),
                "ecart_freinage_m": cc.brake_point_delta_m.and_then(r1),
                "ecart_vitesse_mini_kmh": r1(cc.min_speed_delta * 3.6),
                "ecart_reprise_gaz_m": cc.throttle_on_delta_m.and_then(r1),
                "rapport_rotation_entree": cc.rotation_entry_ratio.and_then(r2),
                "rapport_rotation_sortie": cc.rotation_exit_ratio.and_then(r2),
                "metriques": metrics_json(&cc.lap, l, c.start_pct),
            })
        })
        .collect();
    Ok(json!({
        "virage": c.label(),
        "sens": match c.direction { telemetry::Direction::Left => "gauche", telemetry::Direction::Right => "droite" },
        "rayon_mini_m": r1(c.min_radius_m),
        "longueur_m": r1(c.length_m),
        "reference": metrics_json(&a.reference_metrics[i], l, c.start_pct),
        "tours": per_lap,
        "conventions": "positions en m depuis l'entrée du virage (négatif = avant) ; écarts > 0 : plus tard ; rapport de rotation < 1 : moins de rotation que la référence pour le même braquage (sous-virage)",
    }))
}

/// Canaux disponibles pour `get_corner_trace`.
pub const TRACE_CHANNELS: &[&str] = &[
    "vitesse", "gaz", "frein", "volant", "lacet", "acc_lat", "rapport", "delta",
];

fn channel_values(lap: &AlignedLap, name: &str) -> Option<Vec<f64>> {
    let scale = |v: &[f64], k: f64| v.iter().map(|x| x * k).collect();
    Some(match name {
        "vitesse" => scale(&lap.speed, 3.6),
        "gaz" => scale(&lap.throttle, 100.0),
        "frein" => scale(&lap.brake, 100.0),
        "volant" => scale(&lap.steering, 180.0 / std::f64::consts::PI),
        "lacet" => scale(&lap.yaw_rate, 180.0 / std::f64::consts::PI),
        "acc_lat" => scale(&lap.lat_accel, 1.0 / 9.81),
        "rapport" => lap.gear.clone(),
        _ => return None,
    })
}

/// Indices régulièrement espacés, au plus `max`.
fn pick(len: usize, max: usize) -> Vec<usize> {
    if len <= max {
        return (0..len).collect();
    }
    (0..max)
        .map(|k| (k as f64 * (len - 1) as f64 / (max - 1) as f64).round() as usize)
        .collect()
}

/// `get_corner_trace` : traces rééchantillonnées sur la zone du virage, tour et référence.
pub fn corner_trace(
    snap: &LiveSnapshot,
    analyzer: Option<&Analyzer>,
    corner: &str,
    channels: &[String],
    lap: Option<i32>,
) -> Result<Value, ToolError> {
    let a = require_analyzer(analyzer)?;
    let i = find_corner(a, corner)?;
    let zone: &CornerZone = &a.model.zones[i];
    let lap = match lap {
        Some(n) => snap
            .laps
            .iter()
            .rev()
            .find(|l| l.number == n && l.flags.complete),
        None => snap.laps.iter().rev().find(|l| l.flags.complete),
    }
    .ok_or_else(|| ToolError("tour introuvable (ou incomplet)".into()))?;
    let channels: Vec<&str> = if channels.is_empty() {
        vec!["vitesse", "gaz", "frein"]
    } else {
        channels.iter().map(|s| s.as_str()).collect()
    };
    if let Some(bad) = channels.iter().find(|c| !TRACE_CHANNELS.contains(c)) {
        return err(format!(
            "canal inconnu : {bad} (disponibles : {})",
            TRACE_CHANNELS.join(", ")
        ));
    }
    let n = a.reference.n;
    let aligned = AlignedLap::new(lap, n);
    let idx = zone.indices(n);
    let picks: Vec<usize> = pick(idx.len(), MAX_TRACE_POINTS)
        .into_iter()
        .map(|k| idx[k])
        .collect();
    let l = a.model.length_m;
    let start = a.model.corners[i].start_pct;
    let dist: Vec<Option<f64>> = picks
        .iter()
        .map(|&g| {
            let p = g as f64 / n as f64;
            r1(((p - start + 0.5).rem_euclid(1.0) - 0.5) * l).map(|x| x.round())
        })
        .collect();
    let mut series = serde_json::Map::new();
    for ch in &channels {
        let (mine, refv): (Vec<Option<f64>>, Vec<Option<f64>>) = if *ch == "delta" {
            // Delta cumulé depuis l'entrée de zone, s.
            let t0 = aligned.time[idx[0]] - a.reference.time[idx[0]];
            (
                picks
                    .iter()
                    .map(|&g| r3(aligned.time[g] - a.reference.time[g] - t0))
                    .collect(),
                vec![],
            )
        } else {
            let mv = channel_values(&aligned, ch).unwrap_or_default();
            let rv = channel_values(&a.reference, ch).unwrap_or_default();
            (
                picks.iter().map(|&g| r1(mv[g])).collect(),
                picks.iter().map(|&g| r1(rv[g])).collect(),
            )
        };
        let mut obj = serde_json::Map::new();
        obj.insert("tour".into(), json!(mine));
        if !refv.is_empty() {
            obj.insert("reference".into(), json!(refv));
        }
        series.insert((*ch).to_string(), Value::Object(obj));
    }
    Ok(json!({
        "virage": a.model.corners[i].label(),
        "tour": lap.number,
        "points": picks.len(),
        "distance_m": dist,
        "unites": "vitesse km/h, gaz et frein %, volant °, lacet °/s, acc_lat g, delta s ; distance depuis l'entrée du virage",
        "canaux": series,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use telemetry::synthetic::SyntheticTrack;
    use telemetry::{CornerDetectionParams, LapFlags};

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

    pub(crate) fn fixture() -> (LiveSnapshot, Analyzer) {
        let track = SyntheticTrack::demo();
        let reference = lap(track.simulate_lap(1.0, 0.0, 0.0), 0);
        let analyzer = Analyzer::new(
            &reference,
            Some(track.length_m()),
            &CornerDetectionParams::default(),
        );
        let laps = vec![
            lap(track.simulate_lap(0.97, 100.0, 0.0), 1),
            lap(track.simulate_lap(0.95, 200.0, 0.0), 2),
        ];
        let snap = LiveSnapshot {
            best_lap: Some(laps[0].clone()),
            laps,
            ..Default::default()
        };
        (snap, analyzer)
    }

    #[test]
    fn overview_ranks_corners_by_loss() {
        let (snap, a) = fixture();
        let v = session_overview(&snap, Some(&a));
        let losses = v["temps_perdu_par_virage"].as_array().unwrap();
        assert_eq!(losses.len(), 6);
        let p: Vec<f64> = losses
            .iter()
            .map(|l| l["perte_s"].as_f64().unwrap())
            .collect();
        assert!(p.windows(2).all(|w| w[0] >= w[1]), "{p:?}");
        assert!(p[0] > 0.0);
        assert_eq!(v["tours_analyses"], json!([2, 1]));
        assert_eq!(v["derniers_tours"][0]["tour"], 2);
    }

    #[test]
    fn overview_without_reference_explains() {
        let (snap, _) = fixture();
        let v = session_overview(&snap, None);
        assert!(v["note"].as_str().unwrap().contains("référence"));
    }

    #[test]
    fn corner_summary_accepts_loose_labels() {
        let (snap, a) = fixture();
        for label in ["T4", "t4", "4", "virage 4"] {
            let v = corner_summary(&snap, Some(&a), label, None).unwrap();
            assert_eq!(v["virage"], "T4");
            assert_eq!(v["tours"].as_array().unwrap().len(), 2);
        }
        let v = corner_summary(&snap, Some(&a), "T4", Some(&[1])).unwrap();
        assert_eq!(v["tours"][0]["tour"], 1);
        assert!(v["tours"][0]["perte_s"].as_f64().unwrap() > 0.0);
        assert!(corner_summary(&snap, Some(&a), "T42", None).is_err());
    }

    #[test]
    fn trace_is_bounded_and_aligned() {
        let (snap, a) = fixture();
        let chans = vec![
            "vitesse".to_string(),
            "frein".to_string(),
            "delta".to_string(),
        ];
        let v = corner_trace(&snap, Some(&a), "T1", &chans, Some(2)).unwrap();
        let n = v["points"].as_u64().unwrap() as usize;
        assert!(n <= MAX_TRACE_POINTS && n > 50);
        assert_eq!(v["distance_m"].as_array().unwrap().len(), n);
        let speed = &v["canaux"]["vitesse"];
        assert_eq!(speed["tour"].as_array().unwrap().len(), n);
        assert_eq!(speed["reference"].as_array().unwrap().len(), n);
        // Delta cumulé : nul à l'entrée de zone, positif à la sortie (tour plus lent).
        let delta = v["canaux"]["delta"]["tour"].as_array().unwrap();
        assert!(delta[0].as_f64().unwrap().abs() < 1e-6);
        assert!(delta[n - 1].as_f64().unwrap() > 0.0);
        // Taille raisonnable pour un appel d'outil.
        assert!(v.to_string().len() < 8000, "{} octets", v.to_string().len());
        assert!(corner_trace(&snap, Some(&a), "T1", &["huile".into()], None).is_err());
    }
}
