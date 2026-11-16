//! Analysis of a lap against the reference: alignment on `LapDistPct`, corner
//! zones, per-corner metrics (braking, minimum speed, throttle pickup,
//! coasting, rotation) and deltas.
//!
//! Corners are detected on the reference lap: it defines the track map, so
//! that numbering stays stable from one lap to the next.

pub mod align;
pub mod metrics;
pub mod zones;

use std::sync::Arc;

use telemetry::corners::detect_corners;
use telemetry::trace::distance_m;
use telemetry::{Corner, CornerDetectionParams, Lap};

pub use align::{AlignedLap, delta_trace, grid_size};
pub use metrics::{CornerMetrics, corner_metrics};
pub use zones::{CornerZone, build_zones};

/// Track map derived from the reference.
#[derive(Debug, Clone)]
pub struct TrackModel {
    pub length_m: f64,
    pub corners: Vec<Corner>,
    pub zones: Vec<CornerZone>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CornerComparison {
    pub zone: CornerZone,
    pub lap: CornerMetrics,
    pub reference: CornerMetrics,
    /// Time lost (+) or gained (−) in the zone, s.
    pub time_delta: f64,
    /// Braking later (+) or earlier (−) than the reference, m.
    pub brake_point_delta_m: Option<f64>,
    /// Minimum speed difference, m/s (− = slower).
    pub min_speed_delta: f64,
    /// Throttle pickup later (+) or earlier (−), m.
    pub throttle_on_delta_m: Option<f64>,
    /// Rotation per unit of steering relative to the reference (< 1: less rotation).
    pub rotation_entry_ratio: Option<f64>,
    pub rotation_exit_ratio: Option<f64>,
}

impl CornerComparison {
    pub fn label(&self) -> String {
        self.zone.corner.label()
    }
}

#[derive(Debug, Clone)]
pub struct LapComparison {
    pub lap_number: i32,
    pub lap_time: f64,
    pub reference_time: f64,
    pub corners: Vec<CornerComparison>,
}

impl LapComparison {
    pub fn total_delta(&self) -> f64 {
        self.lap_time - self.reference_time
    }

    /// Corners sorted from most costly to most profitable.
    pub fn ranked(&self) -> Vec<&CornerComparison> {
        let mut v: Vec<_> = self.corners.iter().collect();
        v.sort_by(|a, b| b.time_delta.total_cmp(&a.time_delta));
        v
    }
}

/// Signed position difference, wrapped into [-0.5, 0.5) lap, in meters.
fn signed_m(a: f64, b: f64, length_m: f64) -> f64 {
    ((a - b + 0.5).rem_euclid(1.0) - 0.5) * length_m
}

fn ratio(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(a), Some(b)) if b.abs() > 1e-9 => Some(a / b),
        _ => None,
    }
}

/// Prepared reference: aligned, split, metrics precomputed.
#[derive(Debug, Clone)]
pub struct Analyzer {
    pub model: Arc<TrackModel>,
    pub reference: Arc<AlignedLap>,
    pub reference_metrics: Vec<CornerMetrics>,
}

impl Analyzer {
    /// `track_length_m`: known length (iRacing YAML); otherwise, distance
    /// integrated over the reference.
    pub fn new(
        reference: &Lap,
        track_length_m: Option<f64>,
        params: &CornerDetectionParams,
    ) -> Self {
        let length_m = track_length_m
            .filter(|l| l.is_finite() && *l > 0.0)
            .unwrap_or_else(|| {
                distance_m(&reference.samples)
                    .last()
                    .copied()
                    .unwrap_or(0.0)
            });
        let corners = detect_corners(reference, params);
        let zones = build_zones(&corners);
        let aligned = AlignedLap::new(reference, grid_size(Some(length_m)));
        let reference_metrics = zones
            .iter()
            .map(|z| corner_metrics(&aligned, z, length_m))
            .collect();
        Analyzer {
            model: Arc::new(TrackModel {
                length_m,
                corners,
                zones,
            }),
            reference: Arc::new(aligned),
            reference_metrics,
        }
    }

    /// Compares a complete lap to the reference.
    pub fn compare(&self, lap: &Lap) -> LapComparison {
        let aligned = AlignedLap::new(lap, self.reference.n);
        let l = self.model.length_m;
        let corners = self
            .model
            .zones
            .iter()
            .zip(&self.reference_metrics)
            .map(|(zone, r)| {
                let m = corner_metrics(&aligned, zone, l);
                let pos_delta = |a: Option<f64>, b: Option<f64>| match (a, b) {
                    (Some(a), Some(b)) => Some(signed_m(a, b, l)),
                    _ => None,
                };
                CornerComparison {
                    zone: zone.clone(),
                    time_delta: m.zone_time - r.zone_time,
                    brake_point_delta_m: pos_delta(m.brake_point_pct, r.brake_point_pct),
                    min_speed_delta: m.min_speed - r.min_speed,
                    throttle_on_delta_m: pos_delta(m.throttle_on_pct, r.throttle_on_pct),
                    rotation_entry_ratio: ratio(m.rotation_entry, r.rotation_entry),
                    rotation_exit_ratio: ratio(m.rotation_exit, r.rotation_exit),
                    lap: m,
                    reference: r.clone(),
                }
            })
            .collect();
        LapComparison {
            lap_number: lap.number,
            lap_time: lap.lap_time,
            reference_time: self.reference.lap_time,
            corners,
        }
    }

    /// Live delta: time elapsed in the current lap minus the reference time at
    /// the same position. Positive = behind.
    pub fn live_delta(&self, pct: f64, elapsed: f64) -> f64 {
        elapsed - self.reference.time_at(pct)
    }

    pub fn zone_at(&self, pct: f64) -> Option<&CornerZone> {
        self.model.zones.iter().find(|z| z.contains(pct))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use telemetry::synthetic::SyntheticTrack;
    use telemetry::{LapFlags, Sample};

    fn lap_from(samples: Vec<Sample>) -> Lap {
        Lap {
            number: 1,
            start_time: samples[0].session_time,
            lap_time: samples.len() as f64 / 60.0,
            flags: LapFlags {
                complete: true,
                ..Default::default()
            },
            samples,
        }
    }

    fn setup(pace: f64) -> (SyntheticTrack, Analyzer, Lap) {
        let track = SyntheticTrack::demo();
        let reference = lap_from(track.simulate_lap(1.0, 0.0, 0.0));
        let analyzer = Analyzer::new(
            &reference,
            Some(track.length_m()),
            &CornerDetectionParams::default(),
        );
        let lap = lap_from(track.simulate_lap(pace, 50.0, 0.0));
        (track, analyzer, lap)
    }

    #[test]
    fn zones_partition_the_lap() {
        let (track, a, _) = setup(1.0);
        assert_eq!(a.model.zones.len(), track.expected_corners().len());
        let total: f64 = a.reference_metrics.iter().map(|m| m.zone_time).sum();
        assert!((total - a.reference.lap_time).abs() < 0.02, "{total}");
        for p in [0.0, 0.1, 0.5, 0.999] {
            assert_eq!(a.model.zones.iter().filter(|z| z.contains(p)).count(), 1);
        }
    }

    #[test]
    fn corner_deltas_sum_to_lap_delta() {
        let (_, a, lap) = setup(0.97);
        let cmp = a.compare(&lap);
        let sum: f64 = cmp.corners.iter().map(|c| c.time_delta).sum();
        assert!(cmp.total_delta() > 0.3);
        assert!(
            (sum - cmp.total_delta()).abs() < 0.02,
            "{sum} vs {}",
            cmp.total_delta()
        );
        for c in &cmp.corners {
            assert!(c.time_delta > 0.0, "{}: {}", c.label(), c.time_delta);
            assert!(
                c.min_speed_delta < 0.0,
                "{}: {}",
                c.label(),
                c.min_speed_delta
            );
        }
        // Ranking: the first one is the most costly.
        let r = cmp.ranked();
        assert!(r.windows(2).all(|w| w[0].time_delta >= w[1].time_delta));
    }

    #[test]
    fn reference_against_itself_is_neutral() {
        let (_, a, lap) = setup(1.0);
        let cmp = a.compare(&lap);
        for c in &cmp.corners {
            assert!(c.time_delta.abs() < 0.01, "{} {}", c.label(), c.time_delta);
            if let Some(d) = c.brake_point_delta_m {
                assert!(d.abs() < 3.0, "{} braking {d}", c.label());
            }
            if let Some(r) = c.rotation_entry_ratio {
                assert!((r - 1.0).abs() < 0.02, "{} rotation {r}", c.label());
            }
        }
    }

    #[test]
    fn slower_lap_brakes_earlier_and_reapplies_later() {
        let (_, a, lap) = setup(0.9);
        let cmp = a.compare(&lap);
        // T4: 40 m hairpin, heavy braking.
        let t4 = &cmp.corners[3];
        assert!(t4.reference.brake_point_pct.is_some());
        let d = t4.brake_point_delta_m.unwrap();
        assert!(d < -5.0, "braking {d} m");
        assert!(t4.lap.brake_peak > 0.5);
        assert!(t4.reference.throttle_on_pct.is_some());
    }

    /// Available rotation ratios (entry and exit) of all corners.
    fn rotation_ratios(cmp: &LapComparison) -> Vec<(String, f64)> {
        cmp.corners
            .iter()
            .flat_map(|c| {
                [c.rotation_entry_ratio, c.rotation_exit_ratio]
                    .into_iter()
                    .flatten()
                    .map(move |r| (c.label(), r))
            })
            .collect()
    }

    #[test]
    fn more_steering_for_same_rotation_reads_as_understeer() {
        let (_, a, mut lap) = setup(1.0);
        // Same line but 30 % more steering: less rotation per degree.
        for s in &mut lap.samples {
            s.steering *= 1.3;
        }
        let cmp = a.compare(&lap);
        let ratios = rotation_ratios(&cmp);
        assert!(ratios.len() >= cmp.corners.len(), "{ratios:?}");
        for (label, r) in ratios {
            assert!((r - 1.0 / 1.3).abs() < 0.03, "{label}: {r}");
        }
    }

    #[test]
    fn gps_estimated_reference_gives_neutral_rotation() {
        // CSV reference without YawRate (estimated from Lat/Lon), identical measured
        // lap: smoothing in the estimate must not skew the ratios.
        let track = SyntheticTrack::demo();
        let samples = track.simulate_lap(1.0, 0.0, 0.0);
        let cols = [
            "Speed",
            "LapDistPct",
            "Lat",
            "Lon",
            "Brake",
            "Throttle",
            "SteeringWheelAngle",
            "Gear",
        ];
        let csv = reference::write_garage61_csv(&samples, &cols);
        let r = reference::from_garage61_csv(csv.as_bytes(), "ref.csv").unwrap();
        assert_eq!(r.rotation.yaw_rate, reference::ChannelOrigin::FromPosition);
        let a = Analyzer::new(
            &r.lap,
            Some(track.length_m()),
            &CornerDetectionParams::default(),
        );
        let cmp = a.compare(&lap_from(track.simulate_lap(1.0, 10.0, 0.0)));
        let ratios = rotation_ratios(&cmp);
        assert!(!ratios.is_empty());
        for (label, ratio) in ratios {
            assert!((ratio - 1.0).abs() < 0.1, "{label}: {ratio}");
        }
    }

    #[test]
    fn live_delta_tracks_progress() {
        let (_, a, lap) = setup(0.95);
        let mid = &lap.samples[lap.samples.len() / 2];
        let d = a.live_delta(mid.lap_dist_pct as f64, mid.session_time - lap.start_time);
        assert!(d > 0.0);
        assert!(a.live_delta(0.0, 0.0).abs() < 1e-9);
    }

    #[test]
    fn signed_distance_wraps() {
        assert!((signed_m(0.01, 0.99, 1000.0) - 20.0).abs() < 1e-9);
        assert!((signed_m(0.99, 0.01, 1000.0) + 20.0).abs() < 1e-9);
    }
}
