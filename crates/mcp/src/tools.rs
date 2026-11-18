//! Tool responses, independent of the MCP protocol.
//!
//! Units exposed to Claude: km/h, %, degrees, g, meters, seconds. Values are
//! rounded to keep token counts low.

use std::sync::Arc;

use analysis::{AlignedLap, Analyzer, CornerComparison, CornerMetrics, CornerZone, line};
use serde::Serialize;
use serde_json::{Value, json};
use telemetry::{Lap, LiveSnapshot};

/// Maximum number of points per channel in a trace.
pub const MAX_TRACE_POINTS: usize = 100;
/// Laps analyzed by default.
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

/// Finds a corner: "T5", "t5", "5", "turn 5".
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
                "unknown corner: \"{label}\" (corners: T1 to T{})",
                analyzer.model.corners.len()
            ))
        })
}

fn require_analyzer(a: Option<&Analyzer>) -> Result<&Analyzer, ToolError> {
    a.ok_or_else(|| ToolError("no reference lap loaded".into()))
}

/// Complete laps, most recent first. `numbers`: requested laps.
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
        "pit"
    } else if f.discontinuity {
        "interrupted"
    } else if f.off_track > 0 {
        "off_track"
    } else {
        "valid"
    }
}

#[derive(Serialize)]
struct CornerLoss {
    corner: String,
    /// Mean time lost against the reference, s (negative = gained).
    loss_s: Option<f64>,
    brake_point_m: Option<f64>,
    min_speed_kmh: Option<f64>,
    throttle_on_m: Option<f64>,
    /// Tightest path radius relative to the reference (< 1: tighter line).
    radius_ratio: Option<f64>,
    /// Peak steering relative to the reference (> 1: more steering).
    steering_ratio: Option<f64>,
}

fn mean(v: impl Iterator<Item = f64>) -> Option<f64> {
    let (s, n) = v
        .filter(|x| x.is_finite())
        .fold((0.0, 0), |(s, n), x| (s + x, n + 1));
    (n > 0).then(|| s / n as f64)
}

/// `get_session_overview`: laps, delta to the reference, time lost per corner (ranked).
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
                "lap": l.number,
                "time": lap_time(l.lap_time),
                "delta_ref_s": analyzer.and_then(|a| r3(l.lap_time - a.reference.lap_time)),
                "status": lap_status(l),
            })
        })
        .collect();
    let mut out = json!({
        "track": session.map(|s| s.track_label()),
        "car": session.map(|s| s.car_screen_name.clone()),
        "current_lap": snap.sample.map(|s| s.lap),
        "position_pct": snap.sample.and_then(|s| r1(s.lap_dist_pct as f64 * 100.0)),
        "best_lap": snap.best_lap.as_ref().map(|l| json!({"lap": l.number, "time": lap_time(l.lap_time)})),
        "recent_laps": laps,
    });
    let Some(a) = analyzer else {
        out["reference"] = json!(null);
        out["note"] = json!("no reference lap loaded: no per-corner comparison");
        return out;
    };
    out["reference"] =
        json!({ "time": lap_time(a.reference.lap_time), "corners": a.model.corners.len() });
    let comps: Vec<_> = select_laps(snap, None, DEFAULT_LAPS)
        .into_iter()
        .map(|l| a.compare(l))
        .collect();
    if comps.is_empty() {
        out["time_lost_by_corner"] = json!([]);
        out["note"] = json!("no complete lap yet");
        return out;
    }
    let mut losses: Vec<CornerLoss> = (0..a.model.zones.len())
        .map(|i| {
            let cs: Vec<&CornerComparison> = comps.iter().map(|c| &c.corners[i]).collect();
            CornerLoss {
                corner: a.model.corners[i].label(),
                loss_s: mean(cs.iter().map(|c| c.time_delta)).and_then(r3),
                brake_point_m: mean(cs.iter().filter_map(|c| c.brake_point_delta_m)).and_then(r1),
                min_speed_kmh: mean(cs.iter().map(|c| c.min_speed_delta * 3.6)).and_then(r1),
                throttle_on_m: mean(cs.iter().filter_map(|c| c.throttle_on_delta_m)).and_then(r1),
                radius_ratio: mean(cs.iter().filter_map(|c| c.min_radius_ratio)).and_then(r2),
                steering_ratio: mean(cs.iter().filter_map(|c| c.steering_peak_ratio)).and_then(r2),
            }
        })
        .collect();
    losses.sort_by(|a, b| b.loss_s.unwrap_or(0.0).total_cmp(&a.loss_s.unwrap_or(0.0)));
    out["analyzed_laps"] = json!(comps.iter().map(|c| c.lap_number).collect::<Vec<_>>());
    out["time_lost_by_corner"] = json!(losses);
    out["conventions"] = json!(
        "loss_s > 0: time lost; brake_point_m and throttle_on_m > 0: later than the reference; min_speed_kmh < 0: slower; radius_ratio < 1: tighter line than the reference; steering_ratio > 1: more steering"
    );
    out
}

fn metrics_json(m: &CornerMetrics, length_m: f64, corner_start: f64) -> Value {
    // Positions in meters from the geometric start of the corner.
    let pos = |p: Option<f64>| {
        p.and_then(|p| r1(((p - corner_start + 0.5).rem_euclid(1.0) - 0.5) * length_m))
    };
    json!({
        "zone_time_s": r3(m.zone_time),
        "brake_point_m": pos(m.brake_point_pct),
        "brake_peak_pct": r1(m.brake_peak * 100.0),
        "brake_release_m": pos(m.brake_release_pct),
        "trail_brake_m": r1(m.trail_brake_m),
        "min_speed_kmh": kmh(m.min_speed),
        "apex_m": pos(Some(m.min_speed_pct)),
        "throttle_on_m": pos(m.throttle_on_pct),
        "full_throttle_m": pos(m.full_throttle_pct),
        "coasting_s": r2(m.coasting_s),
        "peak_lat_g": r2(m.peak_lat_accel / 9.81),
        "rotation_entry": r3(m.rotation_entry.unwrap_or(f64::NAN)),
        "rotation_exit": r3(m.rotation_exit.unwrap_or(f64::NAN)),
        "turn_in_m": pos(m.turn_in_pct),
        "min_radius_m": m.min_radius_m.and_then(r1),
        "tightest_point_m": pos(m.min_radius_pct),
        "core_radius_m": m.core_radius_m.and_then(r1),
        "steering_peak_deg": r1(m.steering_peak.to_degrees()),
        "steering_mean_deg": r1(m.steering_mean.to_degrees()),
    })
}

/// `get_corner_summary`: corner metrics for the requested laps, against the reference.
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
        return err("no matching complete lap");
    }
    let per_lap: Vec<Value> = selected
        .iter()
        .map(|lap| {
            let cmp = a.compare(lap);
            let cc = &cmp.corners[i];
            json!({
                "lap": lap.number,
                "status": lap_status(lap),
                "loss_s": r3(cc.time_delta),
                "brake_point_delta_m": cc.brake_point_delta_m.and_then(r1),
                "min_speed_delta_kmh": r1(cc.min_speed_delta * 3.6),
                "throttle_on_delta_m": cc.throttle_on_delta_m.and_then(r1),
                "rotation_ratio_entry": cc.rotation_entry_ratio.and_then(r2),
                "rotation_ratio_exit": cc.rotation_exit_ratio.and_then(r2),
                "turn_in_delta_m": cc.turn_in_delta_m.and_then(r1),
                "min_radius_ratio": cc.min_radius_ratio.and_then(r2),
                "core_radius_ratio": cc.core_radius_ratio.and_then(r2),
                "tightest_point_delta_m": cc.tightest_point_delta_m.and_then(r1),
                "steering_peak_ratio": cc.steering_peak_ratio.and_then(r2),
                "steering_mean_ratio": cc.steering_mean_ratio.and_then(r2),
                "line_offset_m": cc.line.map(|o| json!({
                    "turn_in": r1(o.turn_in_m),
                    "apex": r1(o.apex_m),
                    "exit": r1(o.exit_m),
                    "basis": if o.absolute { "absolute (GPS)" } else { "relative to the approach line" },
                })),
                "metrics": metrics_json(&cc.lap, l, c.start_pct),
            })
        })
        .collect();
    Ok(json!({
        "corner": c.label(),
        "direction": match c.direction { telemetry::Direction::Left => "left", telemetry::Direction::Right => "right" },
        "min_radius_m": r1(c.min_radius_m),
        "length_m": r1(c.length_m),
        "reference": metrics_json(&a.reference_metrics[i], l, c.start_pct),
        "laps": per_lap,
        "conventions": "positions in m from the corner entry (negative = before); deltas > 0: later; rotation ratio < 1: less rotation than the reference for the same steering (understeer); min_radius_ratio and core_radius_ratio < 1: tighter path than the reference; steering ratios > 1: more steering; line_offset_m > 0: towards the inside of the corner (turn_in and apex at the reference's points). With basis \"relative to the approach line\", both lines are made to coincide on the straight before the corner, so a constant offset on the approach is not visible.",
    }))
}

/// Channels available to `get_corner_trace`.
pub const TRACE_CHANNELS: &[&str] = &[
    "speed",
    "throttle",
    "brake",
    "steering",
    "yaw_rate",
    "lat_accel",
    "gear",
    "delta",
    "radius",
    "line_offset",
];

/// Path radius `Speed / |YawRate|`, m, smoothed over ±`half` points; `NaN`
/// on straights and at low speed.
fn radius(lap: &AlignedLap, half: usize) -> Vec<f64> {
    let n = lap.n;
    let k: Vec<f64> = (0..n)
        .map(|i| {
            let (v, y) = (lap.speed[i], lap.yaw_rate[i]);
            if v > 10.0 && y.is_finite() {
                y.abs() / v
            } else {
                f64::NAN
            }
        })
        .collect();
    (0..n)
        .map(|i| {
            let (s, c) = (0..=2 * half)
                .map(|j| k[(i + n + j - half) % n])
                .filter(|x| x.is_finite())
                .fold((0.0, 0usize), |(s, c), x| (s + x, c + 1));
            let mean = if c > half { s / c as f64 } else { f64::NAN };
            if mean > 1.0 / 2000.0 {
                1.0 / mean
            } else {
                f64::NAN
            }
        })
        .collect()
}

fn channel_values(lap: &AlignedLap, name: &str) -> Option<Vec<f64>> {
    let scale = |v: &[f64], k: f64| v.iter().map(|x| x * k).collect();
    Some(match name {
        "speed" => scale(&lap.speed, 3.6),
        "throttle" => scale(&lap.throttle, 100.0),
        "brake" => scale(&lap.brake, 100.0),
        "steering" => scale(&lap.steering, 180.0 / std::f64::consts::PI),
        "yaw_rate" => scale(&lap.yaw_rate, 180.0 / std::f64::consts::PI),
        "lat_accel" => scale(&lap.lat_accel, 1.0 / 9.81),
        "gear" => lap.gear.clone(),
        "radius" => radius(lap, 6),
        _ => return None,
    })
}

/// Evenly spaced indices, at most `max`.
fn pick(len: usize, max: usize) -> Vec<usize> {
    if len <= max {
        return (0..len).collect();
    }
    (0..max)
        .map(|k| (k as f64 * (len - 1) as f64 / (max - 1) as f64).round() as usize)
        .collect()
}

/// `get_corner_trace`: resampled traces over the corner zone, lap and reference.
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
    .ok_or_else(|| ToolError("lap not found (or incomplete)".into()))?;
    let channels: Vec<&str> = if channels.is_empty() {
        vec!["speed", "throttle", "brake"]
    } else {
        channels.iter().map(|s| s.as_str()).collect()
    };
    if let Some(bad) = channels.iter().find(|c| !TRACE_CHANNELS.contains(c)) {
        return err(format!(
            "unknown channel: {bad} (available: {})",
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
        let (mine, refv): (Vec<Option<f64>>, Vec<Option<f64>>) = if *ch == "line_offset" {
            let offsets = match (&aligned.line, &a.reference.line) {
                (Some(m), Some(r)) => {
                    line::inside_offsets(m, r, &a.model.corners[i], &picks, l).map(|(_, o)| o)
                }
                _ => None,
            }
            .ok_or_else(|| ToolError("no line available for this lap or the reference".into()))?;
            (offsets.into_iter().map(r1).collect(), vec![])
        } else if *ch == "delta" {
            // Cumulative delta from the zone entry, s.
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
        obj.insert("lap".into(), json!(mine));
        if !refv.is_empty() {
            obj.insert("reference".into(), json!(refv));
        }
        series.insert((*ch).to_string(), Value::Object(obj));
    }
    Ok(json!({
        "corner": a.model.corners[i].label(),
        "lap": lap.number,
        "points": picks.len(),
        "distance_m": dist,
        "units": "speed km/h, throttle and brake %, steering deg, yaw_rate deg/s, lat_accel g, delta s, radius m (path radius, null on straights), line_offset m (> 0: inside of the reference line); distance from the corner entry",
        "channels": series,
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
        let losses = v["time_lost_by_corner"].as_array().unwrap();
        assert_eq!(losses.len(), 6);
        let p: Vec<f64> = losses
            .iter()
            .map(|l| l["loss_s"].as_f64().unwrap())
            .collect();
        assert!(p.windows(2).all(|w| w[0] >= w[1]), "{p:?}");
        assert!(p[0] > 0.0);
        assert_eq!(v["analyzed_laps"], json!([2, 1]));
        assert_eq!(v["recent_laps"][0]["lap"], 2);
    }

    #[test]
    fn overview_without_reference_explains() {
        let (snap, _) = fixture();
        let v = session_overview(&snap, None);
        assert!(v["note"].as_str().unwrap().contains("reference"));
    }

    #[test]
    fn corner_summary_accepts_loose_labels() {
        let (snap, a) = fixture();
        for label in ["T4", "t4", "4", "turn 4"] {
            let v = corner_summary(&snap, Some(&a), label, None).unwrap();
            assert_eq!(v["corner"], "T4");
            assert_eq!(v["laps"].as_array().unwrap().len(), 2);
        }
        let v = corner_summary(&snap, Some(&a), "T4", Some(&[1])).unwrap();
        assert_eq!(v["laps"][0]["lap"], 1);
        assert!(v["laps"][0]["loss_s"].as_f64().unwrap() > 0.0);
        assert!(corner_summary(&snap, Some(&a), "T42", None).is_err());
    }

    #[test]
    fn trace_is_bounded_and_aligned() {
        let (snap, a) = fixture();
        let chans = vec![
            "speed".to_string(),
            "brake".to_string(),
            "delta".to_string(),
        ];
        let v = corner_trace(&snap, Some(&a), "T1", &chans, Some(2)).unwrap();
        let n = v["points"].as_u64().unwrap() as usize;
        assert!(n <= MAX_TRACE_POINTS && n > 50);
        assert_eq!(v["distance_m"].as_array().unwrap().len(), n);
        let speed = &v["channels"]["speed"];
        assert_eq!(speed["lap"].as_array().unwrap().len(), n);
        assert_eq!(speed["reference"].as_array().unwrap().len(), n);
        // Cumulative delta: zero at the zone entry, positive at the exit (slower lap).
        let delta = v["channels"]["delta"]["lap"].as_array().unwrap();
        assert!(delta[0].as_f64().unwrap().abs() < 1e-6);
        assert!(delta[n - 1].as_f64().unwrap() > 0.0);
        // Reasonable size for a tool call.
        assert!(v.to_string().len() < 8000, "{} bytes", v.to_string().len());
        assert!(corner_trace(&snap, Some(&a), "T1", &["oil".into()], None).is_err());
    }

    #[test]
    fn corner_summary_reports_radius_steering_and_line() {
        let (snap, a) = fixture();
        let v = corner_summary(&snap, Some(&a), "T4", Some(&[1])).unwrap();
        let r = &v["reference"];
        // T4: 40 m hairpin.
        let radius = r["min_radius_m"].as_f64().unwrap();
        assert!((radius - 40.0).abs() < 4.0, "{radius}");
        assert!(r["steering_peak_deg"].as_f64().unwrap() > 10.0);
        assert!(r["turn_in_m"].as_f64().is_some());
        let lap = &v["laps"][0];
        // Same line, a bit slower: same radius and steering, no offset.
        assert!((lap["min_radius_ratio"].as_f64().unwrap() - 1.0).abs() < 0.05);
        assert!((lap["steering_peak_ratio"].as_f64().unwrap() - 1.0).abs() < 0.05);
        let line = &lap["line_offset_m"];
        assert_eq!(line["basis"], "absolute (GPS)");
        assert!(line["apex"].as_f64().unwrap().abs() < 1.0, "{line}");
        let o = session_overview(&snap, Some(&a));
        assert!(
            o["time_lost_by_corner"][0]["radius_ratio"]
                .as_f64()
                .is_some()
        );
    }

    #[test]
    fn trace_has_radius_and_line_offset() {
        let (snap, a) = fixture();
        let chans = vec!["radius".to_string(), "line_offset".to_string()];
        let v = corner_trace(&snap, Some(&a), "T4", &chans, Some(1)).unwrap();
        let radius: Vec<f64> = v["channels"]["radius"]["reference"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|x| x.as_f64())
            .collect();
        let min = radius.iter().copied().fold(f64::INFINITY, f64::min);
        assert!((min - 40.0).abs() < 4.0, "{min}");
        let offsets = v["channels"]["line_offset"]["lap"].as_array().unwrap();
        assert_eq!(offsets.len(), v["points"].as_u64().unwrap() as usize);
        assert!(v["channels"]["line_offset"].get("reference").is_none());
    }
}
