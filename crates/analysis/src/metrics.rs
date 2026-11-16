//! Metrics of an aligned lap within a corner zone.

use telemetry::Corner;

use crate::align::AlignedLap;
use crate::zones::CornerZone;

/// Pedal detection thresholds (0..1).
pub const BRAKE_ON: f64 = 0.10;
pub const BRAKE_OFF: f64 = 0.05;
pub const THROTTLE_ON: f64 = 0.20;
pub const THROTTLE_FULL: f64 = 0.95;
pub const PEDAL_IDLE: f64 = 0.05;
/// Steering angle below which the rotation ratio is not computed, rad.
const MIN_STEER: f64 = 0.03;
const MIN_SPEED: f64 = 10.0;
/// Margin trimmed from each edge of the corner core for rotation, m.
const CORE_TRIM_M: f64 = 15.0;
/// Minimum number of points for a rotation median.
const MIN_ROTATION_POINTS: usize = 5;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct CornerMetrics {
    /// Time spent in the corner zone, s.
    pub zone_time: f64,
    /// Start of braking, pct. `None`: corner without braking.
    pub brake_point_pct: Option<f64>,
    /// Maximum brake pressure, 0..1.
    pub brake_peak: f64,
    /// Full brake release, pct.
    pub brake_release_pct: Option<f64>,
    /// Distance braked after the start of the corner (trail braking), m.
    pub trail_brake_m: f64,
    /// Minimum speed, m/s, and its position (pct).
    pub min_speed: f64,
    pub min_speed_pct: f64,
    /// Throttle pickup (crossing above 20 %), pct. `None`: flat-out corner.
    pub throttle_on_pct: Option<f64>,
    /// Full throttle (above 95 %) after the pickup, pct.
    pub full_throttle_pct: Option<f64>,
    /// Time with neither brake nor throttle in the zone, s.
    pub coasting_s: f64,
    /// Maximum lateral acceleration in the corner, m/s².
    pub peak_lat_accel: f64,
    /// Rotation obtained per unit of steering, `YawRate / (Speed × steering)`,
    /// median over the entry (start → minimum speed) and the exit (minimum speed → end).
    /// To compare with the reference: a lower ratio = less rotation for the
    /// same steering (understeer), higher = more rotation (oversteer).
    pub rotation_entry: Option<f64>,
    pub rotation_exit: Option<f64>,
}

fn median(mut v: Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    Some(v[v.len() / 2])
}

/// Position (index in `idx`) of the grid index closest to `pct`.
fn position_of(idx: &[usize], n: usize, pct: f64) -> usize {
    let target = ((pct.rem_euclid(1.0) * n as f64).round() as usize) % n;
    idx.iter().position(|&i| i == target).unwrap_or_else(|| {
        // Outside the zone (should not happen): closest point.
        idx.iter()
            .enumerate()
            .min_by_key(|(_, i)| {
                let d = (**i as isize - target as isize).unsigned_abs();
                d.min(n - d)
            })
            .map(|(k, _)| k)
            .unwrap_or(0)
    })
}

pub fn corner_metrics(lap: &AlignedLap, zone: &CornerZone, length_m: f64) -> CornerMetrics {
    let n = lap.n;
    let idx = zone.indices(n);
    let corner: &Corner = &zone.corner;
    let pct = |k: usize| idx[k] as f64 / n as f64;
    let step_m = length_m / n as f64;
    let val = |v: &[f64], k: usize| v[idx[k]];

    let k_turn = position_of(&idx, n, corner.start_pct);
    let k_cend = position_of(&idx, n, corner.end_pct).max(k_turn);
    let last = idx.len() - 1;

    // Minimum speed in the corner.
    let k_min = (k_turn..=k_cend)
        .filter(|&k| val(&lap.speed, k).is_finite())
        .min_by(|&a, &b| val(&lap.speed, a).total_cmp(&val(&lap.speed, b)))
        .unwrap_or(k_turn);

    // Braking: first press from the zone start up to the minimum speed.
    let k_brake = (0..=k_min).find(|&k| val(&lap.brake, k) >= BRAKE_ON);
    let k_release = k_brake.map(|kb| {
        (kb..=last)
            .find(|&k| val(&lap.brake, k) < BRAKE_OFF)
            .unwrap_or(last)
    });
    let brake_peak = k_brake
        .map(|kb| {
            (kb..=k_release.unwrap_or(kb))
                .map(|k| val(&lap.brake, k))
                .fold(0.0, f64::max)
        })
        .unwrap_or(0.0);
    let trail_brake_m = match k_release {
        Some(kr) if kr > k_turn => (kr - k_turn) as f64 * step_m,
        _ => 0.0,
    };

    // Throttle pickup: after the lowest throttle point in the corner, last
    // drop below 20 % then first return above it.
    let k_from = k_brake.unwrap_or(0);
    let k_low = (k_from..=k_cend)
        .min_by(|&a, &b| val(&lap.throttle, a).total_cmp(&val(&lap.throttle, b)))
        .unwrap_or(k_turn);
    let min_throttle = val(&lap.throttle, k_low);
    let (throttle_on, full_throttle) = if min_throttle.is_nan() || min_throttle >= THROTTLE_ON {
        (None, None)
    } else {
        let mut k = (k_low..=k_cend)
            .rev()
            .find(|&k| val(&lap.throttle, k) < THROTTLE_ON)
            .unwrap_or(k_low);
        while k <= last && val(&lap.throttle, k) < THROTTLE_ON {
            k += 1;
        }
        if k > last {
            (None, None)
        } else {
            let full = (k..=last).find(|&j| val(&lap.throttle, j) >= THROTTLE_FULL);
            (Some(k), full)
        }
    };

    // Coasting: neither brake nor throttle, while moving.
    let coasting_s = (0..last)
        .filter(|&k| {
            val(&lap.throttle, k) < PEDAL_IDLE
                && val(&lap.brake, k) < PEDAL_IDLE
                && val(&lap.speed, k) > 5.0
        })
        .map(|k| lap.duration(idx[k], idx[k + 1]))
        .sum::<f64>()
        // An empty f64 sum is -0.0 in Rust: normalize.
        + 0.0;

    let peak_lat_accel = (k_turn..=k_cend)
        .map(|k| val(&lap.lat_accel, k).abs())
        .filter(|x| x.is_finite())
        .fold(0.0, f64::max);

    // Rotation: only in the core of the corner (at least half of the maximum
    // steering). At the edges, steering and yaw change quickly and an estimated
    // `YawRate` (smoothed, hence lagging) would skew the ratio.
    let peak_steer = (k_turn..=k_cend)
        .map(|k| val(&lap.steering, k).abs())
        .filter(|x| x.is_finite())
        .fold(0.0, f64::max);
    let steer_floor = MIN_STEER.max(0.5 * peak_steer);
    // Also trim a margin at both edges of this core, on the order of the
    // smoothing window of a GPS-estimated `YawRate`.
    let trim = (CORE_TRIM_M / step_m).round() as usize;
    let core: Vec<usize> = (k_turn..=k_cend)
        .filter(|&k| val(&lap.steering, k).abs() >= steer_floor)
        .collect();
    let (core_lo, core_hi) = match (core.first(), core.last()) {
        (Some(&a), Some(&b)) if b > a + 2 * trim => (a + trim, b - trim),
        _ => (1, 0), // core too short: no measurement
    };
    let rotation = |range: std::ops::RangeInclusive<usize>| {
        let (lo, hi) = ((*range.start()).max(core_lo), (*range.end()).min(core_hi));
        if hi < lo + MIN_ROTATION_POINTS {
            return None;
        }
        median(
            (lo..=hi)
                .filter_map(|k| {
                    let (y, v, s) = (
                        val(&lap.yaw_rate, k),
                        val(&lap.speed, k),
                        val(&lap.steering, k),
                    );
                    (y.is_finite() && v > MIN_SPEED && s.abs() > steer_floor && y * s > 0.0)
                        .then(|| y / (v * s))
                })
                .collect(),
        )
    };

    CornerMetrics {
        zone_time: lap.duration(idx[0], idx[last]),
        brake_point_pct: k_brake.map(pct),
        brake_peak,
        brake_release_pct: k_release.map(pct),
        trail_brake_m,
        min_speed: val(&lap.speed, k_min),
        min_speed_pct: pct(k_min),
        throttle_on_pct: throttle_on.map(pct),
        full_throttle_pct: full_throttle.map(pct),
        coasting_s,
        peak_lat_accel,
        rotation_entry: rotation(k_turn..=k_min),
        rotation_exit: rotation(k_min..=k_cend),
    }
}
