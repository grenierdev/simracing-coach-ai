//! Estimation des canaux de rotation absents d'un export.
//!
//! Ordre de préférence pour `YawRate` :
//! 1. dérivée temporelle de `Yaw` (cap), déroulé ;
//! 2. variation du cap de la trajectoire calculé depuis `Lat`/`Lon`.
//!
//! `LatAccel` est ensuite estimé par `YawRate × Speed` (accélération centripète,
//! sans dérive, dévers ni gravité). Positif = vers la gauche, comme `YawRate`.

use telemetry::Sample;

const EARTH_RADIUS_M: f64 = 6_371_000.0;

/// Moyenne glissante centrée (bords tronqués), qui ignore les valeurs non finies.
pub fn smooth(v: &[f64], half: usize) -> Vec<f64> {
    (0..v.len())
        .map(|i| {
            let lo = i.saturating_sub(half);
            let hi = (i + half + 1).min(v.len());
            let (sum, n) = v[lo..hi]
                .iter()
                .filter(|x| x.is_finite())
                .fold((0.0, 0usize), |(s, n), x| (s + x, n + 1));
            if n == 0 { f64::NAN } else { sum / n as f64 }
        })
        .collect()
}

/// Déroule un angle (rad) : supprime les sauts de ±2π.
pub fn unwrap_angle(v: &[f64]) -> Vec<f64> {
    let mut out = Vec::with_capacity(v.len());
    let mut offset = 0.0;
    let mut prev: Option<f64> = None;
    for &a in v {
        if let Some(p) = prev {
            let d = a - p;
            if d > std::f64::consts::PI {
                offset -= std::f64::consts::TAU;
            } else if d < -std::f64::consts::PI {
                offset += std::f64::consts::TAU;
            }
        }
        prev = Some(a);
        out.push(a + offset);
    }
    out
}

/// Dérivée centrée de `v` par rapport au temps des échantillons.
fn time_derivative(samples: &[Sample], v: &[f64]) -> Vec<f64> {
    let n = v.len();
    (0..n)
        .map(|i| {
            let (a, b) = (i.saturating_sub(1), (i + 1).min(n - 1));
            let dt = samples[b].session_time - samples[a].session_time;
            if b > a && dt > 0.0 {
                (v[b] - v[a]) / dt
            } else {
                f64::NAN
            }
        })
        .collect()
}

/// `YawRate` depuis le canal `Yaw` (rad, cap absolu).
pub fn yaw_rate_from_yaw(samples: &[Sample], yaw: &[f64]) -> Vec<f64> {
    let unwrapped = unwrap_angle(yaw);
    smooth(&time_derivative(samples, &unwrapped), 4)
}

/// `YawRate` depuis la trajectoire GPS : cap de la corde entre deux points
/// distants d'environ `span_m`, dérivé dans le temps.
pub fn yaw_rate_from_position(samples: &[Sample], span_m: f64) -> Vec<f64> {
    let n = samples.len();
    if n < 3 {
        return vec![f64::NAN; n];
    }
    let lat0 = samples
        .iter()
        .map(|s| s.lat)
        .find(|x| x.is_finite())
        .unwrap_or(0.0)
        .to_radians();
    let lon0 = samples
        .iter()
        .map(|s| s.lon)
        .find(|x| x.is_finite())
        .unwrap_or(0.0)
        .to_radians();
    // Projection équirectangulaire locale : x vers l'est, y vers le nord (m).
    let x: Vec<f64> = samples
        .iter()
        .map(|s| EARTH_RADIUS_M * lat0.cos() * (s.lon.to_radians() - lon0))
        .collect();
    let y: Vec<f64> = samples
        .iter()
        .map(|s| EARTH_RADIUS_M * (s.lat.to_radians() - lat0))
        .collect();
    // Cap de la corde centrée sur i, de longueur ~span_m (cap compté
    // antihoraire depuis l'est : un virage à gauche l'augmente).
    let heading: Vec<f64> = (0..n)
        .map(|i| {
            let mut k = 1;
            while k < n / 2 {
                let (a, b) = (i.saturating_sub(k), (i + k).min(n - 1));
                if (x[b] - x[a]).hypot(y[b] - y[a]) >= span_m {
                    break;
                }
                k += 1;
            }
            let (a, b) = (i.saturating_sub(k), (i + k).min(n - 1));
            (y[b] - y[a]).atan2(x[b] - x[a])
        })
        .collect();
    // Lissage plus large que pour `Yaw` : les positions exportées en float32 ont
    // une résolution d'environ 0,5 m, ce qui bruite le cap.
    smooth(&time_derivative(samples, &unwrap_angle(&heading)), 10)
}

/// Remplit `yaw_rate` et `lat_accel` là où ils manquent. Retourne l'origine retenue.
pub fn fill_rotation(samples: &mut [Sample], yaw: Option<&[f64]>) -> RotationOrigin {
    let has_yaw_rate = samples.iter().any(|s| s.yaw_rate.is_finite());
    let has_lat_accel = samples.iter().any(|s| s.lat_accel.is_finite());
    let has_position = samples
        .iter()
        .any(|s| s.lat.is_finite() && s.lon.is_finite());

    let yaw_rate_origin = if has_yaw_rate {
        ChannelOrigin::Measured
    } else {
        let est = match yaw {
            Some(yaw) if yaw.iter().any(|x| x.is_finite()) => {
                Some((yaw_rate_from_yaw(samples, yaw), ChannelOrigin::FromYaw))
            }
            _ if has_position => Some((
                yaw_rate_from_position(samples, 12.0),
                ChannelOrigin::FromPosition,
            )),
            _ => None,
        };
        match est {
            Some((v, origin)) => {
                for (s, r) in samples.iter_mut().zip(v) {
                    s.yaw_rate = r as f32;
                }
                origin
            }
            None => ChannelOrigin::Missing,
        }
    };
    let lat_accel_origin = if has_lat_accel {
        ChannelOrigin::Measured
    } else if yaw_rate_origin != ChannelOrigin::Missing {
        for s in samples.iter_mut() {
            s.lat_accel = s.yaw_rate * s.speed;
        }
        ChannelOrigin::FromYawRate
    } else {
        ChannelOrigin::Missing
    };
    RotationOrigin {
        yaw_rate: yaw_rate_origin,
        lat_accel: lat_accel_origin,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelOrigin {
    Measured,
    FromYaw,
    FromPosition,
    FromYawRate,
    Missing,
}

impl ChannelOrigin {
    pub fn label(self) -> &'static str {
        match self {
            ChannelOrigin::Measured => "mesuré",
            ChannelOrigin::FromYaw => "estimé (dérivée du cap)",
            ChannelOrigin::FromPosition => "estimé (trajectoire GPS)",
            ChannelOrigin::FromYawRate => "estimé (YawRate × vitesse)",
            ChannelOrigin::Missing => "absent",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RotationOrigin {
    pub yaw_rate: ChannelOrigin,
    pub lat_accel: ChannelOrigin,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwrap_removes_jumps() {
        let v = [3.0, 3.1, -3.1, -3.0];
        let u = unwrap_angle(&v);
        assert!((u[2] - (std::f64::consts::TAU - 3.1)).abs() < 1e-9);
        assert!(u.windows(2).all(|w| (w[1] - w[0]).abs() < 0.2));
    }
}
