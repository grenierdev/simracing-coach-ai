//! Alignement d'un tour sur une grille régulière de `LapDistPct`.

use telemetry::Lap;
use telemetry::trace::resample;

/// Un tour rééchantillonné : chaque canal a `n` valeurs aux positions `i / n`.
#[derive(Debug, Clone)]
pub struct AlignedLap {
    pub n: usize,
    pub lap_time: f64,
    /// Temps écoulé depuis la ligne, s. `time[0] = 0` ; `lap_time` correspond à pct = 1.
    pub time: Vec<f64>,
    pub speed: Vec<f64>,
    pub throttle: Vec<f64>,
    pub brake: Vec<f64>,
    pub steering: Vec<f64>,
    pub yaw_rate: Vec<f64>,
    pub lat_accel: Vec<f64>,
    pub gear: Vec<f64>,
}

/// Taille de grille : environ un point par mètre, bornée.
pub fn grid_size(track_length_m: Option<f64>) -> usize {
    match track_length_m {
        Some(l) if l.is_finite() && l > 0.0 => (l.round() as usize).clamp(1000, 10_000),
        _ => 2000,
    }
}

impl AlignedLap {
    pub fn new(lap: &Lap, n: usize) -> Self {
        let grid: Vec<f64> = (0..n).map(|i| i as f64 / n as f64).collect();
        let s = &lap.samples;
        let ch = |f: fn(&telemetry::Sample) -> f64| resample(s, f, &grid);

        // Temps : on ancre (0, 0) et (1, lap_time) pour que les extrémités de la
        // grille ne dépendent pas de la position exacte du premier échantillon.
        let mut pts: Vec<(f64, f64)> = vec![(0.0, 0.0)];
        let mut last = 0.0;
        for x in s {
            let p = x.lap_dist_pct as f64;
            let t = x.session_time - lap.start_time;
            if p > last && p < 1.0 && t.is_finite() && t >= 0.0 && t <= lap.lap_time {
                pts.push((p, t));
                last = p;
            }
        }
        pts.push((1.0, lap.lap_time));
        let mut time = Vec::with_capacity(n);
        let mut j = 0;
        for &g in &grid {
            while j + 1 < pts.len() && pts[j + 1].0 < g {
                j += 1;
            }
            let (p0, t0) = pts[j];
            let (p1, t1) = pts[(j + 1).min(pts.len() - 1)];
            time.push(if p1 > p0 {
                t0 + (t1 - t0) * (g - p0) / (p1 - p0)
            } else {
                t0
            });
        }

        AlignedLap {
            n,
            lap_time: lap.lap_time,
            time,
            speed: ch(|x| x.speed as f64),
            throttle: ch(|x| x.throttle as f64),
            brake: ch(|x| x.brake as f64),
            steering: ch(|x| x.steering as f64),
            yaw_rate: ch(|x| x.yaw_rate as f64),
            lat_accel: ch(|x| x.lat_accel as f64),
            gear: ch(|x| x.gear as f64),
        }
    }

    pub fn index(&self, pct: f64) -> usize {
        ((pct.rem_euclid(1.0) * self.n as f64).round() as usize) % self.n
    }

    /// Temps écoulé interpolé à la position `pct`.
    pub fn time_at(&self, pct: f64) -> f64 {
        let x = pct.clamp(0.0, 1.0) * self.n as f64;
        let i = x.floor() as usize;
        if i >= self.n {
            return self.lap_time;
        }
        let next = if i + 1 < self.n {
            self.time[i + 1]
        } else {
            self.lap_time
        };
        self.time[i] + (next - self.time[i]) * (x - i as f64)
    }

    /// Durée pour aller de l'indice `a` à l'indice `b` (sens de la marche, en
    /// passant la ligne si `b < a`).
    pub fn duration(&self, a: usize, b: usize) -> f64 {
        if b >= a {
            self.time[b] - self.time[a]
        } else {
            (self.lap_time - self.time[a]) + self.time[b]
        }
    }
}

/// Delta de temps point par point : positif = `lap` est en retard sur `reference`.
pub fn delta_trace(lap: &AlignedLap, reference: &AlignedLap) -> Vec<f64> {
    assert_eq!(lap.n, reference.n, "grilles différentes");
    lap.time
        .iter()
        .zip(&reference.time)
        .map(|(a, b)| a - b)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use telemetry::synthetic::SyntheticTrack;
    use telemetry::{Lap, LapFlags};

    pub(crate) fn lap_from(samples: Vec<telemetry::Sample>) -> Lap {
        let lap_time = samples.len() as f64 / 60.0;
        Lap {
            number: 1,
            start_time: samples[0].session_time,
            lap_time,
            flags: LapFlags {
                complete: true,
                ..Default::default()
            },
            samples,
        }
    }

    #[test]
    fn same_lap_has_zero_delta() {
        let track = SyntheticTrack::demo();
        let lap = lap_from(track.simulate_lap(1.0, 100.0, 0.0));
        // Même tour, un échantillon sur deux : mêmes temps aux mêmes positions.
        let mut half = lap.clone();
        half.samples = lap.samples.iter().step_by(2).copied().collect();
        let a = AlignedLap::new(&lap, 2000);
        let b = AlignedLap::new(&half, 2000);
        let d = delta_trace(&a, &b);
        let max = d.iter().map(|x| x.abs()).fold(0.0, f64::max);
        assert!(max < 0.01, "delta max {max}");
    }

    #[test]
    fn delta_ends_at_lap_time_difference() {
        let track = SyntheticTrack::demo();
        let fast = lap_from(track.simulate_lap(1.0, 0.0, 0.0));
        let slow = lap_from(track.simulate_lap(0.95, 0.0, 0.0));
        let a = AlignedLap::new(&slow, 2000);
        let b = AlignedLap::new(&fast, 2000);
        let d = delta_trace(&a, &b);
        // Le delta croît globalement et finit près de l'écart des temps au tour.
        let expected = slow.lap_time - fast.lap_time;
        let end = a.time_at(0.9999) - b.time_at(0.9999);
        assert!(expected > 0.5);
        assert!((end - expected).abs() < 0.02, "{end} vs {expected}");
        assert!(d[d.len() / 2] > 0.0);
        assert!(a.time[0] == 0.0 && b.time[0] == 0.0);
    }
}
