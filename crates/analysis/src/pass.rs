//! One pass through a few corners, compared to the reference without waiting
//! for the end of the lap.
//!
//! The coach works on one or two corners at a time. As soon as the car leaves
//! the last of them, the pass is compared to the reference: [`PassCapture`]
//! records the samples between the start of the first corner zone and the end
//! of the last one, and [`Analyzer::compare_pass`] splices them into the
//! reference lap so that the usual per-corner metrics apply. A pass can follow
//! a reset in the sim: it only needs to cover the window without a jump.

use telemetry::{Sample, TrackSurface};

use crate::zones::forward;
use crate::{AlignedLap, Analyzer, LapComparison};

/// Position step beyond which the car is considered to have jumped (reset,
/// teleport), fraction of a lap.
const MAX_STEP_PCT: f64 = 0.02;
/// Time gap beyond which the recording is considered interrupted, s.
const MAX_GAP_S: f64 = 0.5;
/// Tolerance on the coverage of the window ends, fraction of a lap.
const EDGE_PCT: f64 = 0.005;

/// A pass recorded through a window of the lap.
#[derive(Debug, Clone)]
pub struct Pass {
    /// Window `(start, end)` in pct; `start > end` when it crosses the line.
    pub window: (f64, f64),
    pub samples: Vec<Sample>,
    /// The car went off track during the pass.
    pub off_track: bool,
    /// Lap number when the pass ended.
    pub lap: i32,
}

/// Records passes through a window, one sample at a time.
#[derive(Debug, Clone)]
pub struct PassCapture {
    window: (f64, f64),
    prev: Option<Sample>,
    current: Option<Pass>,
}

fn crossed(prev: f64, cur: f64, at: f64) -> bool {
    let step = forward(prev, cur);
    step > 0.0 && step < MAX_STEP_PCT && forward(prev, at) > 0.0 && forward(prev, at) <= step
}

impl PassCapture {
    pub fn new(window: (f64, f64)) -> Self {
        PassCapture {
            window: (window.0.rem_euclid(1.0), window.1.rem_euclid(1.0)),
            prev: None,
            current: None,
        }
    }

    pub fn window(&self) -> (f64, f64) {
        self.window
    }

    /// A pass is being recorded (the car is inside the window).
    pub fn in_progress(&self) -> bool {
        self.current.is_some()
    }

    /// Adds a sample; returns the pass when the car leaves the window.
    pub fn push(&mut self, s: &Sample) -> Option<Pass> {
        let pct = s.lap_dist_pct as f64;
        if !s.is_on_track || s.on_pit_road || !pct.is_finite() || !s.session_time.is_finite() {
            self.prev = None;
            self.current = None;
            return None;
        }
        let prev = self.prev.replace(*s)?;
        let p0 = prev.lap_dist_pct as f64;
        let dt = s.session_time - prev.session_time;
        if forward(p0, pct) >= MAX_STEP_PCT || dt > MAX_GAP_S || dt < 0.0 {
            // Reset, teleport or a gap in the data: this pass is lost.
            self.current = None;
            return None;
        }
        if let Some(pass) = self.current.as_mut() {
            pass.samples.push(*s);
            pass.off_track |= s.surface == TrackSurface::OffTrack;
            if crossed(p0, pct, self.window.1) {
                let mut done = self.current.take()?;
                done.lap = s.lap;
                return Some(done);
            }
        } else if crossed(p0, pct, self.window.0) {
            self.current = Some(Pass {
                window: self.window,
                samples: vec![*s],
                off_track: s.surface == TrackSurface::OffTrack,
                lap: s.lap,
            });
        }
        None
    }
}

/// Linear interpolation in increasing `x`; clamped at the ends.
fn interp(pts: &[(f64, f64)], x: f64) -> f64 {
    match pts.binary_search_by(|p| p.0.total_cmp(&x)) {
        Ok(i) => pts[i].1,
        Err(0) => pts[0].1,
        Err(i) if i >= pts.len() => pts[pts.len() - 1].1,
        Err(i) => {
            let ((x0, y0), (x1, y1)) = (pts[i - 1], pts[i]);
            if x1 > x0 {
                y0 + (y1 - y0) * (x - x0) / (x1 - x0)
            } else {
                y0
            }
        }
    }
}

impl Analyzer {
    /// Compares a pass to the reference. The corners inside the window carry
    /// the pass's metrics; the others are equal to the reference. `None` when
    /// the samples do not cover the window.
    pub fn compare_pass(&self, pass: &Pass) -> Option<LapComparison> {
        let (w0, w1) = pass.window;
        let span = forward(w0, w1);
        // Positions unwrapped from the window start, so that a window crossing
        // the line stays increasing.
        let u = |p: f64| forward(w0, p);
        let mut last = -1.0;
        let pts: Vec<(f64, &Sample)> = pass
            .samples
            .iter()
            .filter_map(|s| {
                let x = u(s.lap_dist_pct as f64);
                (x > last && x <= span + EDGE_PCT).then(|| {
                    last = x;
                    (x, s)
                })
            })
            .collect();
        if pts.len() < 10 || pts[0].0 > EDGE_PCT || last < span - EDGE_PCT {
            return None;
        }
        let channel = |f: fn(&Sample) -> f64| -> Vec<(f64, f64)> {
            pts.iter()
                .map(|(x, s)| (*x, f(s)))
                .filter(|(_, v)| v.is_finite())
                .collect()
        };
        let time = channel(|s| s.session_time);
        let speed = channel(|s| s.speed as f64);
        let throttle = channel(|s| s.throttle as f64);
        let brake = channel(|s| s.brake as f64);
        let steering = channel(|s| s.steering as f64);
        let yaw_rate = channel(|s| s.yaw_rate as f64);
        let lat_accel = channel(|s| s.lat_accel as f64);
        let gear = channel(|s| s.gear as f64);

        let r = &self.reference;
        let n = r.n;
        let mut lap: AlignedLap = (**r).clone();
        lap.line = None;
        let inside = |i: usize| u(i as f64 / n as f64) <= span;
        let fill = |dst: &mut Vec<f64>, src: &[(f64, f64)]| {
            if src.is_empty() {
                return;
            }
            for (i, v) in dst.iter_mut().enumerate() {
                if inside(i) {
                    *v = interp(src, u(i as f64 / n as f64));
                }
            }
        };
        fill(&mut lap.speed, &speed);
        fill(&mut lap.throttle, &throttle);
        fill(&mut lap.brake, &brake);
        fill(&mut lap.steering, &steering);
        fill(&mut lap.yaw_rate, &yaw_rate);
        fill(&mut lap.lat_accel, &lat_accel);
        fill(&mut lap.gear, &gear);

        // Time: increments of the pass inside the window, of the reference
        // outside, accumulated from the line.
        let ref_time = |i: usize| if i < n { r.time[i] } else { r.lap_time };
        let pass_time = |i: usize| interp(&time, u((i % n) as f64 / n as f64).min(span));
        let step = |i: usize| {
            // From grid point i to i + 1.
            let both = inside(i) && inside((i + 1) % n);
            let mut d = if both {
                let mut a = pass_time(i);
                let b = pass_time(i + 1);
                if b < a {
                    a = b;
                }
                b - a
            } else {
                ref_time(i + 1) - ref_time(i)
            };
            if !d.is_finite() || d < 0.0 {
                d = ref_time(i + 1) - ref_time(i);
            }
            d
        };
        let mut t = 0.0;
        for (i, slot) in lap.time.iter_mut().enumerate() {
            *slot = t;
            t += step(i);
        }
        lap.lap_time = t;
        Some(self.compare_aligned(&lap, pass.lap))
    }

    /// Window covering the zones of `corners` (indices into the track map),
    /// from the start of the first zone to the end of the last, in driving
    /// order. `None` for an empty list.
    pub fn window_for(&self, corners: &[usize]) -> Option<(f64, f64)> {
        let zones = &self.model.zones;
        let mut idx: Vec<usize> = corners
            .iter()
            .copied()
            .filter(|&i| i < zones.len())
            .collect();
        if idx.is_empty() {
            return None;
        }
        idx.sort_unstable();
        idx.dedup();
        // Start at the zone after the largest gap, so that the window is the
        // shortest stretch covering them all (handles the line).
        let k = idx.len();
        let start = (0..k)
            .max_by(|&a, &b| {
                let gap = |j: usize| {
                    let prev = idx[(j + k - 1) % k];
                    forward(zones[prev].end_pct, zones[idx[j]].start_pct)
                };
                gap(a).total_cmp(&gap(b))
            })
            .unwrap_or(0);
        let first = idx[start];
        let last = idx[(start + k - 1) % k];
        Some((zones[first].start_pct, zones[last].end_pct))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use telemetry::synthetic::SyntheticTrack;
    use telemetry::{CornerDetectionParams, Lap, LapFlags};

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

    fn setup() -> (SyntheticTrack, Analyzer) {
        let track = SyntheticTrack::demo();
        let reference = lap_from(track.simulate_lap(1.0, 0.0, 0.0));
        let analyzer = Analyzer::new(
            &reference,
            Some(track.length_m()),
            &CornerDetectionParams::default(),
        );
        (track, analyzer)
    }

    fn on_track(mut s: Sample) -> Sample {
        s.is_on_track = true;
        s.surface = TrackSurface::OnTrack;
        s
    }

    /// Two laps in a row, so that windows crossing the line are covered.
    fn two_laps(track: &SyntheticTrack, pace: f64) -> Vec<Sample> {
        let a = track.simulate_lap(pace, 100.0, 0.0);
        let t1 = a.last().unwrap().session_time + 1.0 / 60.0;
        let mut b = track.simulate_lap(pace, t1, 0.0);
        for s in &mut b {
            s.lap += 1;
        }
        a.into_iter().chain(b).map(on_track).collect()
    }

    fn capture(window: (f64, f64), samples: &[Sample]) -> Vec<Pass> {
        let mut c = PassCapture::new(window);
        samples.iter().filter_map(|s| c.push(s)).collect()
    }

    #[test]
    fn pass_metrics_match_the_full_lap() {
        let (track, a) = setup();
        let samples = two_laps(&track, 0.95);
        let full = a.compare(&lap_from(track.simulate_lap(0.95, 0.0, 0.0)));
        for i in 0..a.model.zones.len() {
            let window = a.window_for(&[i]).unwrap();
            let passes = capture(window, &samples);
            assert!(!passes.is_empty(), "zone {i}: no pass");
            let cmp = a.compare_pass(&passes[0]).expect("covered");
            let (p, f) = (&cmp.corners[i], &full.corners[i]);
            assert!(
                (p.time_delta - f.time_delta).abs() < 0.03,
                "{}: pass {} vs lap {}",
                p.label(),
                p.time_delta,
                f.time_delta
            );
            assert!((p.min_speed_delta - f.min_speed_delta).abs() < 0.5);
            // The other corners are equal to the reference.
            for (j, c) in cmp.corners.iter().enumerate() {
                if j != i {
                    assert!(c.time_delta.abs() < 0.02, "{}: {}", c.label(), c.time_delta);
                }
            }
        }
    }

    #[test]
    fn two_consecutive_corners_share_one_window() {
        let (track, a) = setup();
        let samples = two_laps(&track, 0.95);
        let last = a.model.zones.len() - 1;
        // The last corner and the first one: the window crosses the line.
        let w = a.window_for(&[last, 0]).unwrap();
        assert_eq!(w.0, a.model.zones[last].start_pct);
        assert_eq!(w.1, a.model.zones[0].end_pct);
        let passes = capture(w, &samples);
        assert_eq!(passes.len(), 1, "one crossing of the line in two laps");
        assert!(a.compare_pass(&passes[0]).is_some());
    }

    #[test]
    fn a_reset_loses_the_pass_unless_it_goes_back_before_the_window() {
        let (track, a) = setup();
        let lap: Vec<Sample> = track
            .simulate_lap(1.0, 0.0, 0.0)
            .into_iter()
            .map(on_track)
            .collect();
        let w = a.window_for(&[3]).unwrap();
        let span = forward(w.0, w.1);
        // First sample at `x` (fraction of a lap) past the window start.
        let at = |x: f64| {
            lap.iter()
                .position(|s| {
                    let u = forward(w.0, s.lap_dist_pct as f64);
                    u >= x && u <= span
                })
                .unwrap()
        };
        // From the middle of the window back to `to`, then on to the end.
        let reset_to = |to: usize| -> Vec<Sample> {
            let mid = at(span / 2.0);
            let mut out: Vec<Sample> = lap[..mid].to_vec();
            let shift = out.last().unwrap().session_time + 1.0 / 60.0 - lap[to].session_time;
            out.extend(lap[to..].iter().map(|s| Sample {
                session_time: s.session_time + shift,
                ..*s
            }));
            out
        };
        // Back to a quarter of the window: still inside, the pass is lost.
        assert!(capture(w, &reset_to(at(span / 4.0))).is_empty());
        // Back to 200 samples before the window: a full pass after the reset.
        let before = at(0.0) - 200;
        let passes = capture(w, &reset_to(before));
        assert_eq!(passes.len(), 1);
        assert!(a.compare_pass(&passes[0]).is_some());
    }
}
