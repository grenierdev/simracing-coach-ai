//! Rééchantillonnage d'un tour sur une grille régulière de `LapDistPct`.
//!
//! Base de l'alignement entre tours : deux tours rééchantillonnés sur la même
//! grille se comparent point à point.

use crate::laps::Lap;
use crate::sample::Sample;

/// Un tour sous forme de couples (pct, valeur) strictement croissants en pct.
fn monotonic_points<'a>(
    samples: &'a [Sample],
    value: impl Fn(&Sample) -> f64 + 'a,
) -> impl Iterator<Item = (f64, f64)> + 'a {
    let mut last = f64::NEG_INFINITY;
    samples.iter().filter_map(move |s| {
        let p = s.lap_dist_pct as f64;
        let v = value(s);
        // On écarte les reculs (voiture arrêtée qui dérive, bruit) pour garder
        // une fonction de pct ; les valeurs non finies sont ignorées.
        if p > last && v.is_finite() {
            last = p;
            Some((p, v))
        } else {
            None
        }
    })
}

/// Valeurs de `value` interpolées linéairement aux positions `grid` (pct croissants).
/// Hors de la couverture du tour, la valeur la plus proche est reprise.
pub fn resample(samples: &[Sample], value: impl Fn(&Sample) -> f64, grid: &[f64]) -> Vec<f64> {
    let pts: Vec<(f64, f64)> = monotonic_points(samples, value).collect();
    if pts.is_empty() {
        return vec![f64::NAN; grid.len()];
    }
    let mut out = Vec::with_capacity(grid.len());
    let mut j = 0;
    for &g in grid {
        while j + 1 < pts.len() && pts[j + 1].0 < g {
            j += 1;
        }
        let v = if g <= pts[0].0 {
            pts[0].1
        } else if j + 1 >= pts.len() {
            pts[pts.len() - 1].1
        } else {
            let (p0, v0) = pts[j];
            let (p1, v1) = pts[j + 1];
            v0 + (v1 - v0) * (g - p0) / (p1 - p0)
        };
        out.push(v);
    }
    out
}

/// Grille régulière de `n` points dans [0, 1).
pub fn uniform_grid(n: usize) -> Vec<f64> {
    (0..n).map(|i| i as f64 / n as f64).collect()
}

/// Temps écoulé depuis le début du tour, rééchantillonné sur `grid`.
/// Sert au calcul du delta entre deux tours : `delta(p) = t_a(p) - t_b(p)`.
pub fn elapsed_time(lap: &Lap, grid: &[f64]) -> Vec<f64> {
    resample(&lap.samples, |s| s.session_time - lap.start_time, grid)
}

/// Distance parcourue (m) en fonction de pct : `LapDist` si présent,
/// sinon intégrale de la vitesse.
pub fn distance_m(samples: &[Sample]) -> Vec<f64> {
    if samples.iter().all(|s| s.lap_dist.is_finite()) {
        return samples.iter().map(|s| s.lap_dist as f64).collect();
    }
    let mut d = 0.0;
    let mut out = Vec::with_capacity(samples.len());
    for (i, s) in samples.iter().enumerate() {
        if i > 0 {
            let p = &samples[i - 1];
            let dt = s.session_time - p.session_time;
            d += 0.5 * (s.speed + p.speed) as f64 * dt;
        }
        out.push(d);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(pct: f32, speed: f32, t: f64) -> Sample {
        Sample {
            lap_dist_pct: pct,
            speed,
            session_time: t,
            lap_dist: f32::NAN,
            ..Default::default()
        }
    }

    fn assert_close(a: &[f64], b: &[f64]) {
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b) {
            assert!((x - y).abs() < 1e-4, "{a:?} != {b:?}");
        }
    }

    #[test]
    fn linear_interpolation_and_clamping() {
        let samples = [s(0.1, 10.0, 0.0), s(0.3, 30.0, 1.0), s(0.5, 10.0, 2.0)];
        let out = resample(
            &samples,
            |x| x.speed as f64,
            &[0.0, 0.1, 0.2, 0.4, 0.5, 0.9],
        );
        assert_close(&out, &[10.0, 10.0, 20.0, 20.0, 10.0, 10.0]);
    }

    #[test]
    fn ignores_backward_samples() {
        let samples = [s(0.1, 10.0, 0.0), s(0.09, 99.0, 0.5), s(0.2, 20.0, 1.0)];
        let out = resample(&samples, |x| x.speed as f64, &[0.15]);
        assert_close(&out, &[15.0]);
    }

    #[test]
    fn same_lap_at_different_rates_aligns() {
        // Même tour échantillonné à deux fréquences : les courbes rééchantillonnées coïncident.
        let lap = |n: usize| -> Vec<Sample> {
            (0..n)
                .map(|i| {
                    let p = i as f64 / n as f64;
                    s(p as f32, (50.0 + 20.0 * (p * 12.0).sin()) as f32, p * 90.0)
                })
                .collect()
        };
        let grid = uniform_grid(200);
        let a = resample(&lap(5400), |x| x.speed as f64, &grid);
        let b = resample(&lap(3000), |x| x.speed as f64, &grid);
        let max_err = a
            .iter()
            .zip(&b)
            .map(|(x, y)| (x - y).abs())
            .fold(0.0, f64::max);
        assert!(max_err < 0.05, "écart max {max_err}");
    }

    #[test]
    fn distance_from_speed() {
        let samples = [s(0.0, 10.0, 0.0), s(0.1, 10.0, 1.0), s(0.2, 20.0, 2.0)];
        assert_eq!(distance_m(&samples), vec![0.0, 10.0, 25.0]);
    }
}
