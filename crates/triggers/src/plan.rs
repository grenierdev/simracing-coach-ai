//! Cue plan: file format (TOML) and position resolution.
//!
//! ```toml
//! [settings]
//! quiet_lead_m = 30          # silence from 30 m before the braking point
//! quiet_until = "apex"       # "brake_release", "apex" or "exit"
//!
//! [[trigger]]
//! corner = "T4"              # corner of the track map
//! anchor = "braking"         # "braking", "entry", "apex" or "exit"
//! offset_m = -250            # 250 m before the anchor
//! text = "Hairpin: brake at the hundred board."
//! priority = 7               # 0 to 9
//! repeat = "every_lap"       # "once", "every_lap" or { laps = 3 }
//! condition = { delta_above = 0.2 }
//!
//! [[trigger]]
//! position = 0.02            # absolute position (LapDistPct), no corner
//! text = "New lap."
//! ```

use analysis::{CornerMetrics, TrackModel};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PlanError {
    #[error("reading the plan: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid plan: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("trigger \"{id}\": {msg}")]
    Invalid { id: String, msg: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum Anchor {
    /// Reference braking point (corner entry if the reference does not brake).
    #[default]
    Braking,
    /// Geometric start of the corner.
    Entry,
    /// Minimum speed point.
    Apex,
    /// Geometric end of the corner.
    Exit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum QuietUntil {
    /// Until the brake is released.
    BrakeRelease,
    /// Until the minimum speed point.
    #[default]
    Apex,
    /// Until the end of the corner.
    Exit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum Repeat {
    Once,
    #[default]
    EveryLap,
    /// At most `laps` times.
    Laps(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Condition {
    SpeedAboveKmh(f64),
    SpeedBelowKmh(f64),
    /// Live delta to the reference above N seconds (behind).
    DeltaAbove(f64),
    /// Live delta below N seconds (ahead when negative).
    DeltaBelow(f64),
    /// From lap N onwards.
    MinLap(i32),
}

impl Condition {
    pub fn holds(&self, speed_mps: f32, live_delta: Option<f64>, lap: i32) -> bool {
        match *self {
            Condition::SpeedAboveKmh(v) => speed_mps as f64 * 3.6 > v,
            Condition::SpeedBelowKmh(v) => (speed_mps as f64 * 3.6) < v,
            Condition::DeltaAbove(d) => live_delta.is_some_and(|x| x > d),
            Condition::DeltaBelow(d) => live_delta.is_some_and(|x| x < d),
            Condition::MinLap(n) => lap >= n,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, Default)]
pub struct PlanSettings {
    /// Silence margin before the braking point, m.
    pub quiet_lead_m: Option<f64>,
    pub quiet_until: Option<QuietUntil>,
    /// Maximum distance past the planned position to still speak a trigger, m.
    pub max_delay_m: Option<f64>,
}

/// One trigger of the plan.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct TriggerSpec {
    /// Identifier (optional); a trigger with the same id is replaced.
    pub id: Option<String>,
    /// Sentence spoken to the driver: short, plain, in the voice language.
    pub text: String,
    /// Corner of the track map ("T5"). Ignored when `position` is set.
    pub corner: Option<String>,
    /// Point of the corner used as reference (default: braking).
    pub anchor: Option<Anchor>,
    /// Offset from the anchor, m; negative = before (default: -150).
    pub offset_m: Option<f64>,
    /// Absolute position on the lap (LapDistPct, 0 to 1).
    pub position: Option<f64>,
    /// Priority 0 to 9 (default: 5).
    pub priority: Option<u8>,
    /// Repetition (default: every lap).
    pub repeat: Option<Repeat>,
    /// Optional condition, evaluated when the position is crossed.
    pub condition: Option<Condition>,
    /// Expires after N laps from the moment the plan is activated.
    pub expire_after_laps: Option<u32>,
    /// Maximum distance past the planned position to still speak it, m.
    pub max_delay_m: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, Default)]
pub struct Plan {
    #[serde(default)]
    pub settings: PlanSettings,
    #[serde(default, rename = "trigger")]
    pub triggers: Vec<TriggerSpec>,
}

impl Plan {
    pub fn parse(text: &str) -> Result<Self, PlanError> {
        Ok(toml::from_str(text)?)
    }

    pub fn load(path: impl AsRef<std::path::Path>) -> Result<Self, PlanError> {
        Self::parse(&std::fs::read_to_string(path)?)
    }
}

/// Resolved trigger, positions in pct.
#[derive(Debug, Clone, PartialEq)]
pub struct Trigger {
    pub id: String,
    pub text: String,
    pub at_pct: f64,
    pub corner: Option<String>,
    pub priority: u8,
    pub repeat: Repeat,
    pub condition: Option<Condition>,
    pub expire_after_laps: Option<u32>,
    /// Maximum tolerated delay, in fraction of a lap.
    pub max_delay_pct: f64,
}

/// Zone where the coach stays silent, in pct (`start > end`: crosses the line).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QuietZone {
    pub start: f64,
    pub end: f64,
}

impl QuietZone {
    pub fn contains(&self, pct: f64) -> bool {
        let p = pct.rem_euclid(1.0);
        if self.start <= self.end {
            p >= self.start && p <= self.end
        } else {
            p >= self.start || p <= self.end
        }
    }
}

const DEFAULT_OFFSET_M: f64 = -150.0;
const DEFAULT_QUIET_LEAD_M: f64 = 30.0;
const DEFAULT_MAX_DELAY_M: f64 = 150.0;

/// Map needed to resolve a plan: the track and the reference metrics.
pub struct TrackContext<'a> {
    pub model: &'a TrackModel,
    pub reference: &'a [CornerMetrics],
}

fn wrap(p: f64) -> f64 {
    p.rem_euclid(1.0)
}

impl<'a> TrackContext<'a> {
    fn corner_index(&self, label: &str) -> Option<usize> {
        self.model
            .corners
            .iter()
            .position(|c| c.label().eq_ignore_ascii_case(label.trim()))
    }

    fn anchor_pct(&self, i: usize, anchor: Anchor) -> f64 {
        let c = &self.model.corners[i];
        let m = self.reference.get(i);
        match anchor {
            Anchor::Braking => m.and_then(|m| m.brake_point_pct).unwrap_or(c.start_pct),
            Anchor::Entry => c.start_pct,
            Anchor::Apex => m.map(|m| m.min_speed_pct).unwrap_or(c.apex_pct),
            Anchor::Exit => c.end_pct,
        }
    }

    /// Quiet zones: from `quiet_lead_m` before the braking point to the apex (by
    /// default), for every corner where the reference brakes.
    pub fn quiet_zones(&self, settings: &PlanSettings) -> Vec<QuietZone> {
        let lead = settings.quiet_lead_m.unwrap_or(DEFAULT_QUIET_LEAD_M) / self.model.length_m;
        let until = settings.quiet_until.unwrap_or_default();
        self.model
            .corners
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                let m = self.reference.get(i)?;
                let brake = m.brake_point_pct?;
                let end = match until {
                    QuietUntil::BrakeRelease => m.brake_release_pct.unwrap_or(m.min_speed_pct),
                    QuietUntil::Apex => m.min_speed_pct,
                    QuietUntil::Exit => c.end_pct,
                };
                Some(QuietZone {
                    start: wrap(brake - lead),
                    end,
                })
            })
            .collect()
    }
}

/// Resolves the plan triggers. Without a track map, only triggers with an
/// absolute position are accepted.
pub fn resolve(plan: &Plan, track: Option<&TrackContext>) -> Result<Vec<Trigger>, PlanError> {
    let length = track.map(|t| t.model.length_m);
    plan.triggers
        .iter()
        .enumerate()
        .map(|(n, spec)| {
            let id = spec.id.clone().unwrap_or_else(|| format!("t{}", n + 1));
            let invalid = |msg: String| PlanError::Invalid {
                id: id.clone(),
                msg,
            };
            if spec.text.trim().is_empty() {
                return Err(invalid("empty text".into()));
            }
            let (at_pct, corner) = match (spec.position, &spec.corner) {
                (Some(p), _) => {
                    if !(0.0..1.0).contains(&p) {
                        return Err(invalid(format!("position {p} outside [0, 1)")));
                    }
                    (p, spec.corner.clone())
                }
                (None, Some(label)) => {
                    let t = track.ok_or_else(|| {
                        invalid("corner given but no reference lap loaded".into())
                    })?;
                    let i = t
                        .corner_index(label)
                        .ok_or_else(|| invalid(format!("unknown corner: {label}")))?;
                    let base = t.anchor_pct(i, spec.anchor.unwrap_or_default());
                    let offset = spec.offset_m.unwrap_or(DEFAULT_OFFSET_M) / t.model.length_m;
                    (wrap(base + offset), Some(t.model.corners[i].label()))
                }
                (None, None) => return Err(invalid("neither \"corner\" nor \"position\"".into())),
            };
            let max_delay_m = spec
                .max_delay_m
                .or(plan.settings.max_delay_m)
                .unwrap_or(DEFAULT_MAX_DELAY_M);
            Ok(Trigger {
                id: id.clone(),
                text: spec.text.trim().to_string(),
                at_pct,
                corner,
                priority: spec.priority.unwrap_or(5).min(9),
                repeat: spec.repeat.unwrap_or_default(),
                condition: spec.condition,
                expire_after_laps: spec.expire_after_laps,
                // Unknown track length: 3 % of a lap.
                max_delay_pct: length.map(|l| max_delay_m / l).unwrap_or(0.03),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use analysis::Analyzer;
    use telemetry::synthetic::SyntheticTrack;
    use telemetry::{CornerDetectionParams, Lap, LapFlags};

    fn analyzer() -> Analyzer {
        let track = SyntheticTrack::demo();
        let samples = track.simulate_lap(1.0, 0.0, 0.0);
        let lap = Lap {
            number: 1,
            start_time: 0.0,
            lap_time: samples.len() as f64 / 60.0,
            flags: LapFlags {
                complete: true,
                ..Default::default()
            },
            samples,
        };
        Analyzer::new(
            &lap,
            Some(track.length_m()),
            &CornerDetectionParams::default(),
        )
    }

    #[test]
    fn parses_full_plan() {
        let plan = Plan::parse(
            r#"
            [settings]
            quiet_lead_m = 40
            quiet_until = "exit"

            [[trigger]]
            id = "t4"
            corner = "T4"
            anchor = "apex"
            offset_m = -300
            text = "  Hairpin.  "
            priority = 12
            repeat = { laps = 3 }
            condition = { delta_above = 0.2 }
            expire_after_laps = 5

            [[trigger]]
            position = 0.5
            text = "Half lap."
            repeat = "once"
            "#,
        )
        .unwrap();
        assert_eq!(plan.triggers.len(), 2);
        assert_eq!(plan.settings.quiet_until, Some(QuietUntil::Exit));
        let a = analyzer();
        let ctx = TrackContext {
            model: &a.model,
            reference: &a.reference_metrics,
        };
        let t = resolve(&plan, Some(&ctx)).unwrap();
        assert_eq!(t[0].text, "Hairpin.");
        assert_eq!(t[0].priority, 9, "priority capped at 9");
        assert_eq!(t[0].repeat, Repeat::Laps(3));
        assert_eq!(t[0].condition, Some(Condition::DeltaAbove(0.2)));
        let apex = a.reference_metrics[3].min_speed_pct;
        let expected = apex - 300.0 / a.model.length_m;
        assert!((t[0].at_pct - expected).abs() < 1e-9);
        assert_eq!(t[1].id, "t2");
        assert_eq!(t[1].at_pct, 0.5);
    }

    #[test]
    fn brake_anchor_uses_reference_brake_point() {
        let a = analyzer();
        let ctx = TrackContext {
            model: &a.model,
            reference: &a.reference_metrics,
        };
        let plan = Plan::parse("[[trigger]]\ncorner = \"t1\"\noffset_m = 0\ntext = \"x\"").unwrap();
        let t = resolve(&plan, Some(&ctx)).unwrap();
        assert_eq!(Some(t[0].at_pct), a.reference_metrics[0].brake_point_pct);
        assert_eq!(t[0].corner.as_deref(), Some("T1"));
    }

    #[test]
    fn invalid_plans_are_reported() {
        let a = analyzer();
        let ctx = TrackContext {
            model: &a.model,
            reference: &a.reference_metrics,
        };
        for (src, needle) in [
            (
                "[[trigger]]\ncorner = \"T42\"\ntext = \"x\"",
                "unknown corner",
            ),
            ("[[trigger]]\ntext = \"x\"", "neither \"corner\""),
            ("[[trigger]]\nposition = 1.5\ntext = \"x\"", "outside"),
            ("[[trigger]]\nposition = 0.1\ntext = \" \"", "empty text"),
        ] {
            let err = resolve(&Plan::parse(src).unwrap(), Some(&ctx)).unwrap_err();
            assert!(err.to_string().contains(needle), "{err}");
        }
        assert!(Plan::parse("[[trigger]]\nposition = \"a\"").is_err());
    }

    #[test]
    fn quiet_zones_cover_braking_corners() {
        let a = analyzer();
        let ctx = TrackContext {
            model: &a.model,
            reference: &a.reference_metrics,
        };
        let zones = ctx.quiet_zones(&PlanSettings::default());
        let braking: Vec<_> = a
            .reference_metrics
            .iter()
            .filter(|m| m.brake_point_pct.is_some())
            .collect();
        assert_eq!(zones.len(), braking.len());
        for (z, m) in zones.iter().zip(&braking) {
            assert!(z.contains(m.brake_point_pct.unwrap()));
            assert!(z.contains(m.min_speed_pct));
        }
    }
}
