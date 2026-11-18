//! The car's line: path rebuilt in meters, and lateral offset against the
//! reference line in a corner.
//!
//! The path comes from `Lat`/`Lon` when present (.ibt files, Garage61
//! references), otherwise from dead reckoning: speed integrated along the
//! heading (`Yaw`, or `YawRate` integrated). Dead reckoning drifts over a
//! lap but is accurate over the few seconds of a corner.
//!
//! Without two GPS lines, the lap line is aligned on the reference over the
//! straight before the corner (rotation and translation, least squares): the
//! offsets are then relative to the approach line, not to the track edges.

use telemetry::{Corner, Direction, Sample};

use crate::zones::CornerZone;

/// Meters per degree of latitude.
const M_PER_DEG: f64 = 111_320.0;
/// Approach used to align the lap line on the reference, m before the corner.
const FIT_FROM_M: f64 = 150.0;
const FIT_TO_M: f64 = 10.0;
/// Share of samples that must carry the channel for it to be used.
const COVERAGE: f64 = 0.9;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineSource {
    Gps,
    DeadReckoning,
}

/// Path on the lap grid: `x[i]`, `y[i]` in meters at position `i / n`.
#[derive(Debug, Clone, PartialEq)]
pub struct LinePath {
    pub source: LineSource,
    pub x: Vec<f64>,
    pub y: Vec<f64>,
}

/// Lateral offset of the lap line against the reference, m; positive =
/// towards the inside of the corner.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LineComparison {
    /// Both lines from GPS: absolute offsets. Otherwise relative to the
    /// approach line (the lines coincide on average over the straight before).
    pub absolute: bool,
    /// At the reference turn-in point.
    pub turn_in_m: f64,
    /// At the reference apex (tightest point).
    pub apex_m: f64,
    /// At the corner exit.
    pub exit_m: f64,
}

fn coverage(samples: &[Sample], ok: impl Fn(&Sample) -> bool) -> bool {
    !samples.is_empty()
        && samples.iter().filter(|s| ok(s)).count() as f64 >= COVERAGE * samples.len() as f64
}

/// `(pct, x, y)` for each usable sample.
fn gps_points(samples: &[Sample]) -> Vec<(f64, f64, f64)> {
    let first = samples
        .iter()
        .find(|s| s.lat.is_finite() && s.lon.is_finite());
    let Some(o) = first else { return vec![] };
    let cos = o.lat.to_radians().cos();
    samples
        .iter()
        .filter(|s| s.lat.is_finite() && s.lon.is_finite())
        .map(|s| {
            (
                s.lap_dist_pct as f64,
                (s.lon - o.lon) * M_PER_DEG * cos,
                (s.lat - o.lat) * M_PER_DEG,
            )
        })
        .collect()
}

/// Unwraps an angle series (removes ±2π jumps).
fn unwrap(v: &[f64]) -> Vec<f64> {
    use std::f64::consts::{PI, TAU};
    let mut out = Vec::with_capacity(v.len());
    let mut offset = 0.0;
    for (i, &a) in v.iter().enumerate() {
        if i > 0 {
            let d = a - v[i - 1];
            if d > PI {
                offset -= TAU;
            } else if d < -PI {
                offset += TAU;
            }
        }
        out.push(a + offset);
    }
    out
}

/// Pearson correlation.
fn correlation(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len().min(b.len()) as f64;
    if n < 3.0 {
        return 0.0;
    }
    let (ma, mb) = (a.iter().sum::<f64>() / n, b.iter().sum::<f64>() / n);
    let (mut sab, mut saa, mut sbb) = (0.0, 0.0, 0.0);
    for (x, y) in a.iter().zip(b) {
        sab += (x - ma) * (y - mb);
        saa += (x - ma).powi(2);
        sbb += (y - mb).powi(2);
    }
    if saa <= 0.0 || sbb <= 0.0 {
        0.0
    } else {
        sab / (saa * sbb).sqrt()
    }
}

/// Heading of each sample, rad: `Yaw` if it matches `YawRate` (its sign
/// convention is checked against it), otherwise `YawRate` integrated.
fn headings(samples: &[Sample]) -> Option<Vec<f64>> {
    let rate_ok = coverage(samples, |s| s.yaw_rate.is_finite());
    if coverage(samples, |s| s.yaw.is_finite()) {
        let yaw = unwrap(
            &samples
                .iter()
                .map(|s| if s.yaw.is_finite() { s.yaw as f64 } else { 0.0 })
                .collect::<Vec<_>>(),
        );
        if !rate_ok {
            return Some(yaw);
        }
        // d(Yaw)/dt against YawRate: same sign convention, or opposite.
        let (mut dy, mut rate) = (vec![], vec![]);
        for w in 1..samples.len() {
            let dt = samples[w].session_time - samples[w - 1].session_time;
            let r = samples[w].yaw_rate as f64;
            if dt > 0.0 && r.is_finite() {
                dy.push((yaw[w] - yaw[w - 1]) / dt);
                rate.push(r);
            }
        }
        let c = correlation(&dy, &rate);
        if c > 0.7 {
            return Some(yaw);
        }
        if c < -0.7 {
            return Some(yaw.iter().map(|a| -a).collect());
        }
    }
    if !rate_ok {
        return None;
    }
    let mut h = 0.0;
    let mut out = Vec::with_capacity(samples.len());
    for (i, s) in samples.iter().enumerate() {
        if i > 0 {
            let dt = s.session_time - samples[i - 1].session_time;
            let r = (s.yaw_rate as f64 + samples[i - 1].yaw_rate as f64) / 2.0;
            if dt > 0.0 && dt < 1.0 && r.is_finite() {
                h += r * dt;
            }
        }
        out.push(h);
    }
    Some(out)
}

fn reckoned_points(samples: &[Sample]) -> Vec<(f64, f64, f64)> {
    let Some(h) = headings(samples) else {
        return vec![];
    };
    let (mut x, mut y) = (0.0, 0.0);
    let mut out = Vec::with_capacity(samples.len());
    for (i, s) in samples.iter().enumerate() {
        if i > 0 {
            let p = &samples[i - 1];
            let dt = s.session_time - p.session_time;
            let v = (s.speed as f64 + p.speed as f64) / 2.0;
            if dt > 0.0 && dt < 1.0 && v.is_finite() {
                let a = (h[i] + h[i - 1]) / 2.0;
                x += v * dt * a.cos();
                y += v * dt * a.sin();
            }
        }
        out.push((s.lap_dist_pct as f64, x, y));
    }
    out
}

/// Linear interpolation of `(pct, x, y)` points on the grid `i / n`.
fn on_grid(points: &[(f64, f64, f64)], n: usize) -> Option<(Vec<f64>, Vec<f64>)> {
    let mut pts: Vec<(f64, f64, f64)> = Vec::with_capacity(points.len());
    for &p in points {
        if p.0.is_finite() && pts.last().is_none_or(|l| p.0 > l.0) {
            pts.push(p);
        }
    }
    if pts.len() < 10 {
        return None;
    }
    let (mut xs, mut ys) = (Vec::with_capacity(n), Vec::with_capacity(n));
    let mut j = 0;
    for i in 0..n {
        let g = i as f64 / n as f64;
        while j + 2 < pts.len() && pts[j + 1].0 < g {
            j += 1;
        }
        let (a, b) = (pts[j], pts[j + 1]);
        let t = ((g - a.0) / (b.0 - a.0)).clamp(0.0, 1.0);
        xs.push(a.1 + (b.1 - a.1) * t);
        ys.push(a.2 + (b.2 - a.2) * t);
    }
    Some((xs, ys))
}

/// Path of a lap on the grid of `n` points, or `None` without GPS, heading
/// or yaw rate.
pub fn line_path(samples: &[Sample], n: usize) -> Option<LinePath> {
    let has_gps = coverage(samples, |s| {
        s.lat.is_finite() && s.lon.is_finite() && (s.lat != 0.0 || s.lon != 0.0)
    });
    let (source, points) = if has_gps {
        (LineSource::Gps, gps_points(samples))
    } else {
        (LineSource::DeadReckoning, reckoned_points(samples))
    };
    let (x, y) = on_grid(&points, n)?;
    Some(LinePath { source, x, y })
}

/// Grid indices from `from_m` to `to_m` meters relative to `pct` (negative = before).
fn span(pct: f64, from_m: f64, to_m: f64, n: usize, length_m: f64) -> Vec<usize> {
    let step = length_m / n as f64;
    let center = (pct.rem_euclid(1.0) * n as f64).round() as isize;
    let (a, b) = (
        (from_m / step).round() as isize,
        (to_m / step).round() as isize,
    );
    (a..=b)
        .map(|k| (center + k).rem_euclid(n as isize) as usize)
        .collect()
}

/// Rotation + translation (least squares) that brings `lap` points onto
/// `reference` points, both taken at `idx`.
fn fit(lap: &LinePath, reference: &LinePath, idx: &[usize]) -> Option<(f64, f64, f64, f64, f64)> {
    let pairs: Vec<((f64, f64), (f64, f64))> = idx
        .iter()
        .map(|&i| ((lap.x[i], lap.y[i]), (reference.x[i], reference.y[i])))
        .filter(|((a, b), (c, d))| a.is_finite() && b.is_finite() && c.is_finite() && d.is_finite())
        .collect();
    if pairs.len() < 10 {
        return None;
    }
    let n = pairs.len() as f64;
    let (mut px, mut py, mut qx, mut qy) = (0.0, 0.0, 0.0, 0.0);
    for ((a, b), (c, d)) in &pairs {
        px += a;
        py += b;
        qx += c;
        qy += d;
    }
    let (px, py, qx, qy) = (px / n, py / n, qx / n, qy / n);
    let (mut dot, mut cross) = (0.0, 0.0);
    for ((a, b), (c, d)) in &pairs {
        let (ux, uy, vx, vy) = (a - px, b - py, c - qx, d - qy);
        dot += ux * vx + uy * vy;
        cross += ux * vy - uy * vx;
    }
    Some((cross.atan2(dot), px, py, qx, qy))
}

/// Offset towards the inside of `corner` (m) of the lap line against the
/// reference at each index of `idx` (`NaN` where unknown), and whether it is
/// absolute.
pub fn inside_offsets(
    lap: &LinePath,
    reference: &LinePath,
    corner: &Corner,
    idx: &[usize],
    length_m: f64,
) -> Option<(bool, Vec<f64>)> {
    let n = reference.x.len();
    if lap.x.len() != n {
        return None;
    }
    let absolute = lap.source == LineSource::Gps && reference.source == LineSource::Gps;
    let transform: Box<dyn Fn(usize) -> (f64, f64)> = if absolute {
        Box::new(|i| (lap.x[i], lap.y[i]))
    } else {
        let window = span(corner.start_pct, -FIT_FROM_M, -FIT_TO_M, n, length_m);
        let (a, px, py, qx, qy) = fit(lap, reference, &window)?;
        let (c, s) = (a.cos(), a.sin());
        Box::new(move |i| {
            let (u, v) = (lap.x[i] - px, lap.y[i] - py);
            (qx + c * u - s * v, qy + s * u + c * v)
        })
    };
    let inside = match corner.direction {
        Direction::Left => 1.0,
        Direction::Right => -1.0,
    };
    let offsets = idx
        .iter()
        .map(|&i| {
            let (prev, next) = ((i + n - 1) % n, (i + 1) % n);
            let (tx, ty) = (
                reference.x[next] - reference.x[prev],
                reference.y[next] - reference.y[prev],
            );
            let norm = tx.hypot(ty);
            let (lx, ly) = transform(i);
            let (dx, dy) = (lx - reference.x[i], ly - reference.y[i]);
            if norm > 1e-6 && dx.is_finite() && dy.is_finite() {
                inside * (tx * dy - ty * dx) / norm
            } else {
                f64::NAN
            }
        })
        .collect();
    Some((absolute, offsets))
}

/// Offsets at the reference turn-in, apex and exit of a corner.
pub fn compare_line(
    lap: &LinePath,
    reference: &LinePath,
    zone: &CornerZone,
    turn_in_pct: f64,
    apex_pct: f64,
    length_m: f64,
) -> Option<LineComparison> {
    let n = reference.x.len();
    let corner = &zone.corner;
    let at = |pct: f64| ((pct.rem_euclid(1.0) * n as f64).round() as usize) % n;
    let points = [at(turn_in_pct), at(apex_pct), at(corner.end_pct)];
    let (absolute, o) = inside_offsets(lap, reference, corner, &points, length_m)?;
    o.iter().all(|x| x.is_finite()).then_some(LineComparison {
        absolute,
        turn_in_m: o[0],
        apex_m: o[1],
        exit_m: o[2],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use telemetry::synthetic::SyntheticTrack;

    fn without_gps(mut samples: Vec<Sample>) -> Vec<Sample> {
        for s in &mut samples {
            s.lat = f64::NAN;
            s.lon = f64::NAN;
        }
        samples
    }

    #[test]
    fn dead_reckoning_matches_the_gps_line() {
        let track = SyntheticTrack::demo();
        let samples = track.simulate_lap(1.0, 0.0, 0.0);
        let n = 2900;
        let gps = line_path(&samples, n).unwrap();
        assert_eq!(gps.source, LineSource::Gps);
        // Yaw heading.
        let reck = line_path(&without_gps(samples.clone()), n).unwrap();
        assert_eq!(reck.source, LineSource::DeadReckoning);
        // Integrated YawRate only.
        let mut no_yaw = without_gps(samples);
        for s in &mut no_yaw {
            s.yaw = f32::NAN;
        }
        let rate = line_path(&no_yaw, n).unwrap();
        let corners = telemetry::corners::detect_corners(
            &telemetry::Lap {
                number: 1,
                start_time: 0.0,
                lap_time: 60.0,
                flags: Default::default(),
                samples: track.simulate_lap(1.0, 0.0, 0.0),
            },
            &Default::default(),
        );
        for path in [&reck, &rate] {
            for c in &corners {
                let idx = span(c.start_pct, -50.0, c.length_m + 50.0, n, track.length_m());
                let (abs, o) = inside_offsets(path, &gps, c, &idx, track.length_m()).unwrap();
                assert!(!abs);
                let worst = o.iter().fold(0.0f64, |m, x| m.max(x.abs()));
                assert!(worst < 1.0, "{}: {worst} m", c.label());
            }
        }
    }

    /// Reference: an arc of radius 60 m after a straight; lap: same entry, a
    /// tighter arc (shifted towards the inside by up to `shift` m).
    fn arc_paths(shift: f64, lap_source: LineSource) -> (LinePath, LinePath, Corner) {
        let n = 1000;
        let (mut rx, mut ry, mut lx, mut ly) = (vec![], vec![], vec![], vec![]);
        for i in 0..n {
            let d = i as f64;
            // Straight along +x up to 400 m, then left arc of 60 m over 94 m (90°).
            let (x, y, nx, ny, bump) = if d < 400.0 {
                (d, 0.0, 0.0, 1.0, 0.0)
            } else if d < 494.0 {
                let a = (d - 400.0) / 60.0;
                let b = (std::f64::consts::PI * (d - 400.0) / 94.0).sin();
                (
                    400.0 + 60.0 * a.sin(),
                    60.0 - 60.0 * a.cos(),
                    -a.sin(),
                    a.cos(),
                    b,
                )
            } else {
                (460.0, 60.0 + (d - 494.0), -1.0, 0.0, 0.0)
            };
            rx.push(x);
            ry.push(y);
            // Left normal: towards the inside of a left turn.
            lx.push(x + shift * bump * nx + 3.0);
            ly.push(y + shift * bump * ny - 7.0);
        }
        let corner = Corner {
            number: 1,
            direction: Direction::Left,
            start_pct: 0.4,
            end_pct: 0.494,
            apex_pct: 0.447,
            min_speed: 20.0,
            min_radius_m: 60.0,
            length_m: 94.0,
        };
        let reference = LinePath {
            source: LineSource::Gps,
            x: rx,
            y: ry,
        };
        let lap = LinePath {
            source: lap_source,
            x: lx,
            y: ly,
        };
        (lap, reference, corner)
    }

    #[test]
    fn tighter_line_reads_as_inside_at_the_apex() {
        let (lap, reference, corner) = arc_paths(4.0, LineSource::DeadReckoning);
        let zone = CornerZone {
            corner: corner.clone(),
            start_pct: 0.3,
            end_pct: 0.55,
        };
        let c = compare_line(&lap, &reference, &zone, 0.4, 0.447, 1000.0).unwrap();
        assert!(!c.absolute);
        // The constant shift (3, -7) of the dead-reckoned line is removed by the fit.
        assert!(c.turn_in_m.abs() < 0.3, "{c:?}");
        assert!((c.apex_m - 4.0).abs() < 0.3, "{c:?}");
        assert!(c.exit_m.abs() < 0.3, "{c:?}");
    }

    #[test]
    fn two_gps_lines_are_compared_as_is() {
        let (lap, reference, corner) = arc_paths(0.0, LineSource::Gps);
        let zone = CornerZone {
            corner,
            start_pct: 0.3,
            end_pct: 0.55,
        };
        let c = compare_line(&lap, &reference, &zone, 0.4, 0.447, 1000.0).unwrap();
        assert!(c.absolute);
        // Shift (3, -7): 7 m to the right on the straight (outside of a left turn).
        assert!((c.turn_in_m + 7.0).abs() < 0.5, "{c:?}");
    }
}
