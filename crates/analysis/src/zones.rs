//! Splitting of the lap into zones, one per corner.
//!
//! Each zone runs from the middle of the previous straight to the middle of the
//! next one: zones cover the whole lap without overlapping, so the sum of the
//! per-corner deltas equals the lap delta.

use telemetry::Corner;

#[derive(Debug, Clone, PartialEq)]
pub struct CornerZone {
    pub corner: Corner,
    /// Zone start, pct (inclusive).
    pub start_pct: f64,
    /// Zone end, pct; equal to the start of the next zone.
    pub end_pct: f64,
}

impl CornerZone {
    /// Grid indices of the zone, in the driving direction (end inclusive).
    pub fn indices(&self, n: usize) -> Vec<usize> {
        let a = ((self.start_pct * n as f64).round() as usize) % n;
        let b = ((self.end_pct * n as f64).round() as usize) % n;
        let len = (b + n - a) % n;
        let len = if len == 0 { n } else { len };
        (0..=len).map(|k| (a + k) % n).collect()
    }

    pub fn contains(&self, pct: f64) -> bool {
        let p = pct.rem_euclid(1.0);
        if self.start_pct <= self.end_pct {
            p >= self.start_pct && p < self.end_pct
        } else {
            p >= self.start_pct || p < self.end_pct
        }
    }
}

/// Circular distance from `a` to `b` in the driving direction, in pct.
pub fn forward(a: f64, b: f64) -> f64 {
    (b - a).rem_euclid(1.0)
}

/// Corner zones (in track order).
pub fn build_zones(corners: &[Corner]) -> Vec<CornerZone> {
    let m = corners.len();
    if m == 0 {
        return vec![];
    }
    if m == 1 {
        let c = &corners[0];
        let mid = (c.end_pct + forward(c.end_pct, c.start_pct) / 2.0).rem_euclid(1.0);
        return vec![CornerZone {
            corner: c.clone(),
            start_pct: mid,
            end_pct: mid,
        }];
    }
    // Boundary between corner i and the next one: middle of the straight.
    let boundary = |i: usize| {
        let a = &corners[i];
        let b = &corners[(i + 1) % m];
        (a.end_pct + forward(a.end_pct, b.start_pct) / 2.0).rem_euclid(1.0)
    };
    (0..m)
        .map(|i| CornerZone {
            corner: corners[i].clone(),
            start_pct: boundary((i + m - 1) % m),
            end_pct: boundary(i),
        })
        .collect()
}
