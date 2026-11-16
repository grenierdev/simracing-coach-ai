//! Corner detection from a lap.
//!
//! Method: path curvature `κ = YawRate / Speed` (rad/m), falling back to
//! `LatAccel / Speed²` if `YawRate` is missing. The curvature is resampled on a
//! regular distance grid, smoothed, then thresholded with hysteresis (entry
//! below radius `enter_radius_m`, exit above `exit_radius_m`). Neighboring
//! segments in the same direction are merged, segments that are too short are
//! discarded, and a corner that straddles the finish line is stitched back.
//!
//! Sign convention: positive `YawRate` = rotation to the left.

use crate::laps::Lap;
use crate::sample::Sample;
use crate::trace::distance_m;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Corner {
    /// Displayed number (T1, T2...), starting at 1 in track order.
    pub number: u32,
    pub direction: Direction,
    /// Corner start, pct. If `start_pct > end_pct`, the corner straddles the line.
    pub start_pct: f64,
    pub end_pct: f64,
    /// Minimum speed point (apex marker for coaching), pct.
    pub apex_pct: f64,
    /// Minimum speed in the corner, m/s.
    pub min_speed: f64,
    /// Minimum path radius, m.
    pub min_radius_m: f64,
    /// Corner length, m.
    pub length_m: f64,
}

impl Corner {
    pub fn label(&self) -> String {
        format!("T{}", self.number)
    }

    pub fn contains(&self, pct: f64) -> bool {
        if self.start_pct <= self.end_pct {
            (self.start_pct..=self.end_pct).contains(&pct)
        } else {
            pct >= self.start_pct || pct <= self.end_pct
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CornerDetectionParams {
    /// Distance grid step, m.
    pub step_m: f64,
    /// Smoothing window (centered moving average), m.
    pub smooth_m: f64,
    /// Radius below which a corner starts, m.
    pub enter_radius_m: f64,
    /// Radius above which it ends, m.
    pub exit_radius_m: f64,
    /// Minimum corner length, m.
    pub min_length_m: f64,
    /// Maximum gap between two merged segments in the same direction, m.
    pub merge_gap_m: f64,
    /// Speed below which curvature is not computed (noise), m/s.
    pub min_speed: f64,
}

impl Default for CornerDetectionParams {
    fn default() -> Self {
        CornerDetectionParams {
            step_m: 2.0,
            smooth_m: 30.0,
            enter_radius_m: 200.0,
            exit_radius_m: 320.0,
            min_length_m: 20.0,
            merge_gap_m: 40.0,
            min_speed: 8.0,
        }
    }
}

fn curvature(s: &Sample, min_speed: f64) -> Option<f64> {
    let v = s.speed as f64;
    if !v.is_finite() || v < min_speed {
        return None;
    }
    if s.yaw_rate.is_finite() {
        Some(s.yaw_rate as f64 / v)
    } else if s.lat_accel.is_finite() {
        Some(s.lat_accel as f64 / (v * v))
    } else {
        None
    }
}

/// Linear interpolation of `ys` (at increasing abscissas `xs`) at `x`.
fn interp(xs: &[f64], ys: &[f64], x: f64, j: &mut usize) -> f64 {
    while *j + 1 < xs.len() && xs[*j + 1] < x {
        *j += 1;
    }
    if x <= xs[0] {
        return ys[0];
    }
    if *j + 1 >= xs.len() {
        return ys[xs.len() - 1];
    }
    let (x0, x1) = (xs[*j], xs[*j + 1]);
    if x1 <= x0 {
        return ys[*j];
    }
    ys[*j] + (ys[*j + 1] - ys[*j]) * (x - x0) / (x1 - x0)
}

/// Centered moving average, circular (the lap loops).
fn smooth_circular(v: &[f64], half: usize) -> Vec<f64> {
    let n = v.len();
    if n == 0 || half == 0 {
        return v.to_vec();
    }
    let w = (2 * half + 1) as f64;
    let mut out = vec![0.0; n];
    let mut acc: f64 = (0..=2 * half).map(|k| v[(k + n - half) % n]).sum();
    for i in 0..n {
        out[i] = acc / w;
        acc -= v[(i + n - half) % n];
        acc += v[(i + half + 1) % n];
    }
    out
}

#[derive(Debug, Clone, Copy)]
struct Segment {
    start: usize,
    end: usize, // inclusive
    dir: Direction,
}

/// Detects the corners of a complete lap.
pub fn detect_corners(lap: &Lap, params: &CornerDetectionParams) -> Vec<Corner> {
    detect_corners_in(&lap.samples, params)
}

pub fn detect_corners_in(samples: &[Sample], params: &CornerDetectionParams) -> Vec<Corner> {
    // Points strictly increasing in distance.
    let dist = distance_m(samples);
    let mut xs = Vec::with_capacity(samples.len());
    let mut kappa = Vec::with_capacity(samples.len());
    let mut speed = Vec::with_capacity(samples.len());
    let mut pct = Vec::with_capacity(samples.len());
    let mut last = f64::NEG_INFINITY;
    for (s, &d) in samples.iter().zip(&dist) {
        if d > last {
            last = d;
            xs.push(d);
            kappa.push(curvature(s, params.min_speed).unwrap_or(0.0));
            speed.push(s.speed as f64);
            pct.push(s.lap_dist_pct as f64);
        }
    }
    if xs.len() < 10 {
        return vec![];
    }
    let (d0, d1) = (xs[0], xs[xs.len() - 1]);
    let n = ((d1 - d0) / params.step_m).floor() as usize + 1;
    if n < 10 {
        return vec![];
    }
    let (mut jk, mut js, mut jp) = (0, 0, 0);
    let grid: Vec<f64> = (0..n).map(|i| d0 + i as f64 * params.step_m).collect();
    let k_raw: Vec<f64> = grid
        .iter()
        .map(|&x| interp(&xs, &kappa, x, &mut jk))
        .collect();
    let v_grid: Vec<f64> = grid
        .iter()
        .map(|&x| interp(&xs, &speed, x, &mut js))
        .collect();
    let p_grid: Vec<f64> = grid
        .iter()
        .map(|&x| interp(&xs, &pct, x, &mut jp))
        .collect();
    let half = ((params.smooth_m / params.step_m) / 2.0).round() as usize;
    let k = smooth_circular(&k_raw, half);

    // Thresholding with hysteresis.
    let k_enter = 1.0 / params.enter_radius_m;
    let k_exit = 1.0 / params.exit_radius_m;
    let mut segs: Vec<Segment> = vec![];
    let mut open: Option<Segment> = None;
    for (i, &ki) in k.iter().enumerate() {
        let dir = if ki >= 0.0 {
            Direction::Left
        } else {
            Direction::Right
        };
        match open.as_mut() {
            Some(seg) => {
                if ki.abs() < k_exit || dir != seg.dir {
                    segs.push(*seg);
                    open = None;
                    if ki.abs() >= k_enter {
                        open = Some(Segment {
                            start: i,
                            end: i,
                            dir,
                        });
                    }
                } else {
                    seg.end = i;
                }
            }
            None => {
                if ki.abs() >= k_enter {
                    open = Some(Segment {
                        start: i,
                        end: i,
                        dir,
                    });
                }
            }
        }
    }
    if let Some(seg) = open {
        segs.push(seg);
    }

    // Merge nearby segments in the same direction.
    let gap_pts = (params.merge_gap_m / params.step_m) as usize;
    let mut merged: Vec<Segment> = vec![];
    for seg in segs {
        if let Some(last) = merged.last_mut()
            && last.dir == seg.dir
            && seg.start.saturating_sub(last.end) <= gap_pts
        {
            last.end = seg.end;
            continue;
        }
        merged.push(seg);
    }

    // Corner straddling the line: the last and first segments join.
    let mut wrapped: Option<(Segment, Segment)> = None;
    if merged.len() >= 2 {
        let first = merged[0];
        let last = merged[merged.len() - 1];
        if first.dir == last.dir && first.start + (n - 1 - last.end) <= gap_pts {
            merged.remove(0);
            merged.pop();
            wrapped = Some((last, first));
        }
    }

    let make = |parts: &[Segment]| -> Option<Corner> {
        let idx: Vec<usize> = parts.iter().flat_map(|s| s.start..=s.end).collect();
        let length_m = idx.len() as f64 * params.step_m;
        if length_m < params.min_length_m {
            return None;
        }
        let apex = *idx
            .iter()
            .min_by(|&&a, &&b| v_grid[a].total_cmp(&v_grid[b]))?;
        let max_k = idx.iter().map(|&i| k[i].abs()).fold(0.0, f64::max);
        Some(Corner {
            number: 0,
            direction: parts[0].dir,
            start_pct: p_grid[parts[0].start],
            end_pct: p_grid[parts[parts.len() - 1].end],
            apex_pct: p_grid[apex],
            min_speed: v_grid[apex],
            min_radius_m: if max_k > 0.0 {
                1.0 / max_k
            } else {
                f64::INFINITY
            },
            length_m,
        })
    };

    let mut corners: Vec<Corner> = merged.iter().filter_map(|s| make(&[*s])).collect();
    if let Some((last, first)) = wrapped
        && let Some(c) = make(&[last, first])
    {
        // Numbering: a corner that starts before the line stays last.
        corners.push(c);
    }
    for (i, c) in corners.iter_mut().enumerate() {
        c.number = i as u32 + 1;
    }
    corners
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synthetic::{SyntheticTrack, TrackSegment};

    fn dirs(cs: &[Corner]) -> Vec<Direction> {
        cs.iter().map(|c| c.direction).collect()
    }

    #[test]
    fn detects_corners_of_demo_track() {
        let track = SyntheticTrack::demo();
        let lap = track.simulate_lap(1.0, 0.0, 0.0);
        let corners = detect_corners_in(&lap, &CornerDetectionParams::default());
        let expected = track.expected_corners();
        assert_eq!(
            dirs(&corners),
            expected.iter().map(|e| e.0).collect::<Vec<_>>(),
            "{corners:#?}"
        );
        for (c, (_, mid_pct)) in corners.iter().zip(&expected) {
            assert!(
                c.contains(*mid_pct),
                "{} [{:.3}, {:.3}] does not contain {mid_pct:.3}",
                c.label(),
                c.start_pct,
                c.end_pct
            );
        }
    }

    #[test]
    fn robust_to_noise() {
        let track = SyntheticTrack::demo();
        let lap = track.simulate_lap(1.0, 0.0, 0.02);
        let corners = detect_corners_in(&lap, &CornerDetectionParams::default());
        assert_eq!(corners.len(), track.expected_corners().len());
    }

    #[test]
    fn lat_accel_fallback_when_no_yaw_rate() {
        let track = SyntheticTrack::demo();
        let mut lap = track.simulate_lap(1.0, 0.0, 0.0);
        for s in &mut lap {
            s.yaw_rate = f32::NAN;
        }
        let corners = detect_corners_in(&lap, &CornerDetectionParams::default());
        assert_eq!(corners.len(), track.expected_corners().len());
    }

    #[test]
    fn opposite_directions_stay_separate() {
        // Tight right-left chicane: two corners.
        let track = SyntheticTrack::new(vec![
            TrackSegment::Straight(800.0),
            TrackSegment::Right {
                radius: 50.0,
                angle_deg: 60.0,
            },
            TrackSegment::Left {
                radius: 50.0,
                angle_deg: 60.0,
            },
            TrackSegment::Straight(800.0),
            TrackSegment::Right {
                radius: 100.0,
                angle_deg: 180.0,
            },
            TrackSegment::Straight(400.0),
            TrackSegment::Right {
                radius: 100.0,
                angle_deg: 180.0,
            },
            TrackSegment::Straight(400.0),
        ]);
        let lap = track.simulate_lap(1.0, 0.0, 0.0);
        let corners = detect_corners_in(&lap, &CornerDetectionParams::default());
        assert_eq!(
            dirs(&corners),
            vec![
                Direction::Right,
                Direction::Left,
                Direction::Right,
                Direction::Right
            ]
        );
    }

    #[test]
    fn corner_across_start_line_is_merged() {
        let track = SyntheticTrack::new(vec![
            TrackSegment::Right {
                radius: 80.0,
                angle_deg: 45.0,
            },
            TrackSegment::Straight(900.0),
            TrackSegment::Right {
                radius: 80.0,
                angle_deg: 135.0,
            },
            TrackSegment::Straight(900.0),
            TrackSegment::Right {
                radius: 80.0,
                angle_deg: 135.0,
            },
            TrackSegment::Straight(300.0),
            TrackSegment::Right {
                radius: 80.0,
                angle_deg: 45.0,
            },
        ]);
        let lap = track.simulate_lap(1.0, 0.0, 0.0);
        let corners = detect_corners_in(&lap, &CornerDetectionParams::default());
        assert_eq!(corners.len(), 3, "{corners:#?}");
        let last = corners.last().unwrap();
        assert!(last.start_pct > last.end_pct, "{last:?}");
        assert!(last.contains(0.0) && last.contains(0.999));
    }

    #[test]
    fn contains_handles_wrap() {
        let c = Corner {
            number: 1,
            direction: Direction::Left,
            start_pct: 0.95,
            end_pct: 0.03,
            apex_pct: 0.99,
            min_speed: 20.0,
            min_radius_m: 50.0,
            length_m: 100.0,
        };
        assert!(c.contains(0.97) && c.contains(0.01));
        assert!(!c.contains(0.5));
    }
}
