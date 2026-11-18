//! Synthetic track and telemetry: tests, demo and development without iRacing.
//!
//! A track is a sequence of straights and arcs. The speed profile is limited by
//! lateral grip in corners and by acceleration and braking capability (forward
//! and backward passes, like a simple lap time calculation).

use crate::corners::Direction;
use crate::ibt::{IbtWriter, Value};
use crate::irsdk::VarType;
use crate::sample::{Sample, TrackSurface};

#[derive(Debug, Clone, Copy)]
pub enum TrackSegment {
    Straight(f64),
    Left { radius: f64, angle_deg: f64 },
    Right { radius: f64, angle_deg: f64 },
}

impl TrackSegment {
    fn length(&self) -> f64 {
        match *self {
            TrackSegment::Straight(l) => l,
            TrackSegment::Left { radius, angle_deg }
            | TrackSegment::Right { radius, angle_deg } => radius * angle_deg.to_radians(),
        }
    }

    /// Signed curvature (positive to the left), 1/m.
    fn curvature(&self) -> f64 {
        match *self {
            TrackSegment::Straight(_) => 0.0,
            TrackSegment::Left { radius, .. } => 1.0 / radius,
            TrackSegment::Right { radius, .. } => -1.0 / radius,
        }
    }
}

const DS: f64 = 0.5;
const V_MAX: f64 = 75.0;
const A_LAT: f64 = 24.0;
const A_BRAKE: f64 = 16.0;
const WHEELBASE: f64 = 2.7;
const STEERING_RATIO: f64 = 13.0;

/// Deterministic pseudo-random generator (LCG), for reproducible data.
#[derive(Debug, Clone)]
pub struct Lcg(u64);

impl Lcg {
    pub fn new(seed: u64) -> Self {
        Lcg(seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407))
    }
    /// Uniform in [-1, 1].
    pub fn next_signed(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
    }
}

#[derive(Debug, Clone)]
pub struct SyntheticTrack {
    segments: Vec<TrackSegment>,
    /// Curvature at each `DS` meter step.
    kappa: Vec<f64>,
    x: Vec<f64>,
    y: Vec<f64>,
    /// Heading at each step, rad.
    heading: Vec<f64>,
}

impl SyntheticTrack {
    pub fn new(segments: Vec<TrackSegment>) -> Self {
        let mut kappa = vec![];
        for seg in &segments {
            let n = (seg.length() / DS).round() as usize;
            kappa.extend(std::iter::repeat_n(seg.curvature(), n));
        }
        let (mut x, mut y, mut h) = (0.0, 0.0, 0.0f64);
        let mut xs = Vec::with_capacity(kappa.len());
        let mut ys = Vec::with_capacity(kappa.len());
        let mut hs = Vec::with_capacity(kappa.len());
        for k in &kappa {
            xs.push(x);
            ys.push(y);
            hs.push(h);
            h += k * DS;
            x += h.cos() * DS;
            y += h.sin() * DS;
        }
        SyntheticTrack {
            segments,
            kappa,
            x: xs,
            y: ys,
            heading: hs,
        }
    }

    /// ~2.9 km demo track (~55 s lap), six corners: R, L, R, R, R, R.
    pub fn demo() -> Self {
        use TrackSegment::*;
        Self::new(vec![
            Straight(600.0),
            Right {
                radius: 60.0,
                angle_deg: 90.0,
            },
            Straight(300.0),
            Left {
                radius: 120.0,
                angle_deg: 45.0,
            },
            Right {
                radius: 120.0,
                angle_deg: 45.0,
            },
            Straight(400.0),
            Right {
                radius: 40.0,
                angle_deg: 120.0,
            },
            Straight(250.0),
            Right {
                radius: 170.0,
                angle_deg: 60.0,
            },
            Straight(500.0),
            Right {
                radius: 80.0,
                angle_deg: 90.0,
            },
            Straight(200.0),
        ])
    }

    pub fn length_m(&self) -> f64 {
        self.kappa.len() as f64 * DS
    }

    /// Expected corners: (direction, pct of the arc midpoint).
    pub fn expected_corners(&self) -> Vec<(Direction, f64)> {
        let total = self.length_m();
        let mut d = 0.0;
        let mut out = vec![];
        for seg in &self.segments {
            let l = seg.length();
            match seg {
                TrackSegment::Left { .. } => out.push((Direction::Left, (d + l / 2.0) / total)),
                TrackSegment::Right { .. } => out.push((Direction::Right, (d + l / 2.0) / total)),
                TrackSegment::Straight(_) => {}
            }
            d += l;
        }
        out
    }

    /// Speed profile (m/s) at each step, for a `pace` (1.0 = limit).
    fn speed_profile(&self, pace: f64) -> Vec<f64> {
        let n = self.kappa.len();
        let a_lat = A_LAT * pace * pace;
        let vlim: Vec<f64> = self
            .kappa
            .iter()
            .map(|k| {
                if k.abs() < 1e-9 {
                    V_MAX
                } else {
                    (a_lat / k.abs()).sqrt().min(V_MAX)
                }
            })
            .collect();
        let mut v = vlim.clone();
        let a_acc = |v: f64| (9.0 * (1.0 - v / (V_MAX * 1.05))).max(0.3);
        // Two rounds of forward then backward passes: the profile loops.
        for _ in 0..2 {
            for i in 0..n {
                let prev = v[(i + n - 1) % n];
                let cap = (prev * prev + 2.0 * a_acc(prev) * DS).sqrt();
                v[i] = v[i].min(cap);
            }
        }
        for _ in 0..2 {
            for i in (0..n).rev() {
                let next = v[(i + 1) % n];
                let cap = (next * next + 2.0 * A_BRAKE * pace * DS).sqrt();
                v[i] = v[i].min(cap);
            }
        }
        v
    }

    /// Simulates a complete lap at 60 Hz, from pct 0 to pct 1.
    /// `t0`: `SessionTime` of the first sample; `noise`: noise on `YawRate` (rad/s).
    pub fn simulate_lap(&self, pace: f64, t0: f64, noise: f64) -> Vec<Sample> {
        self.simulate_lap_seeded(pace, t0, noise, 1)
    }

    pub fn simulate_lap_seeded(&self, pace: f64, t0: f64, noise: f64, seed: u64) -> Vec<Sample> {
        let n = self.kappa.len();
        let v = self.speed_profile(pace);
        let total = self.length_m();
        // Cumulative time at each step.
        let mut t = Vec::with_capacity(n + 1);
        t.push(0.0);
        for i in 0..n {
            let vn = v[(i + 1) % n];
            t.push(t[i] + 2.0 * DS / (v[i] + vn));
        }
        let lap_time = t[n];
        let mut rng = Lcg::new(seed);
        let mut out = vec![];
        let mut j = 0;
        let mut k = 0usize;
        loop {
            let tk = k as f64 / 60.0;
            if tk >= lap_time {
                break;
            }
            while j + 1 < n && t[j + 1] <= tk {
                j += 1;
            }
            let frac = (tk - t[j]) / (t[j + 1] - t[j]);
            let d = (j as f64 + frac) * DS;
            let speed = v[j] + (v[(j + 1) % n] - v[j]) * frac;
            let kap = self.kappa[j];
            // Longitudinal acceleration: dv/dt = v · dv/ds.
            let dvds = (v[(j + 1) % n] - v[j]) / DS;
            let long_accel = speed * dvds;
            let (throttle, brake) = if long_accel < -2.0 {
                (0.0, (-long_accel / (A_BRAKE * pace)).clamp(0.0, 1.0))
            } else if long_accel > 0.5 {
                (1.0, 0.0)
            } else {
                (0.35, 0.0)
            };
            let gear = match speed {
                s if s < 18.0 => 1,
                s if s < 28.0 => 2,
                s if s < 38.0 => 3,
                s if s < 48.0 => 4,
                s if s < 60.0 => 5,
                _ => 6,
            };
            let ratios = [0.0, 18.0, 28.0, 38.0, 48.0, 60.0, 76.0];
            let rpm = 3500.0 + 5000.0 * (speed / ratios[gear as usize]).min(1.0);
            let yaw = speed * kap + noise * rng.next_signed();
            out.push(Sample {
                session_time: t0 + tk,
                tick: (t0 * 60.0) as i32 + k as i32,
                lap: 0,
                lap_dist_pct: (d / total) as f32,
                lap_dist: d as f32,
                lap_current_time: tk as f32,
                lap_last_time: f32::NAN,
                speed: speed as f32,
                throttle: throttle as f32,
                brake: brake as f32,
                clutch: 1.0,
                gear,
                rpm: rpm as f32,
                steering: ((kap * WHEELBASE).atan() * STEERING_RATIO) as f32,
                yaw_rate: yaw as f32,
                yaw: wrap_angle(self.heading[j]) as f32,
                lat_accel: (speed * speed * kap + noise * 20.0 * rng.next_signed()) as f32,
                long_accel: long_accel as f32,
                lat: 48.0 + self.y[j] / 111_320.0,
                lon: -79.0 + self.x[j] / (111_320.0 * 48f64.to_radians().cos()),
                is_on_track: true,
                on_pit_road: false,
                surface: TrackSurface::OnTrack,
            });
            k += 1;
        }
        out
    }

    /// Full session: end of an out lap (from pct 0.7) then `laps` laps at
    /// varying pace. Replayable as an .ibt.
    pub fn session_ibt(&self, laps: usize, seed: u64) -> IbtWriter {
        let yaml = format!(
            "---\nWeekendInfo:\n TrackName: synthetic\n TrackID: 0\n TrackLength: {:.2} km\n TrackDisplayName: Synthetic Circuit\n TrackConfigName: Demo\nDriverInfo:\n DriverCarIdx: 0\n Drivers:\n - CarIdx: 0\n   UserName: Driver\n   CarScreenName: Synthetic car\n",
            self.length_m() / 1000.0
        );
        let mut w = IbtWriter::new(SYNTHETIC_CHANNELS, &yaml, 60);
        let mut rng = Lcg::new(seed);
        let mut t = 0.0;
        let mut last_lap_time = f32::NAN;
        let out_lap: Vec<Sample> = self
            .simulate_lap_seeded(0.85, 0.0, 0.01, seed)
            .into_iter()
            .filter(|s| s.lap_dist_pct >= 0.7)
            .collect();
        let mut push_lap = |samples: Vec<Sample>, lap: i32, w: &mut IbtWriter, t: &mut f64| {
            let base = samples.first().map(|s| s.session_time).unwrap_or(0.0);
            let dur = samples.len() as f64 / 60.0;
            for s in samples {
                let st = *t + (s.session_time - base);
                w.push(&[
                    Value::Double(st),
                    Value::Int((st * 60.0).round() as i32),
                    Value::Int(lap),
                    Value::Float(s.lap_dist_pct),
                    Value::Float(s.lap_dist),
                    Value::Float(s.lap_current_time),
                    Value::Float(last_lap_time),
                    Value::Float(s.speed),
                    Value::Float(s.throttle),
                    Value::Float(s.brake),
                    Value::Float(s.clutch),
                    Value::Int(s.gear),
                    Value::Float(s.rpm),
                    Value::Float(s.steering),
                    Value::Float(s.yaw_rate),
                    Value::Float(s.yaw),
                    Value::Float(s.lat_accel),
                    Value::Float(s.long_accel),
                    Value::Double(s.lat),
                    Value::Double(s.lon),
                    Value::Bool(true),
                    Value::Bool(false),
                    Value::Int(TrackSurface::OnTrack.to_raw()),
                ]);
            }
            *t += dur;
            last_lap_time = dur as f32;
        };
        push_lap(out_lap, 0, &mut w, &mut t);
        for lap in 1..=laps {
            let pace = 0.96 + 0.03 * (rng.next_signed() * 0.5 + 0.5);
            let samples = self.simulate_lap_seeded(pace, 0.0, 0.01, seed + lap as u64);
            push_lap(samples, lap as i32, &mut w, &mut t);
        }
        w
    }
}

/// Angle in (-π, π], like iRacing's `Yaw`.
fn wrap_angle(a: f64) -> f64 {
    use std::f64::consts::{PI, TAU};
    let w = a.rem_euclid(TAU);
    if w > PI { w - TAU } else { w }
}

/// Channels written by [`SyntheticTrack::session_ibt`], iRacing names and types.
pub const SYNTHETIC_CHANNELS: &[(&str, VarType, &str)] = &[
    ("SessionTime", VarType::Double, "s"),
    ("SessionTick", VarType::Int, ""),
    ("Lap", VarType::Int, ""),
    ("LapDistPct", VarType::Float, "%"),
    ("LapDist", VarType::Float, "m"),
    ("LapCurrentLapTime", VarType::Float, "s"),
    ("LapLastLapTime", VarType::Float, "s"),
    ("Speed", VarType::Float, "m/s"),
    ("Throttle", VarType::Float, "%"),
    ("Brake", VarType::Float, "%"),
    ("Clutch", VarType::Float, "%"),
    ("Gear", VarType::Int, ""),
    ("RPM", VarType::Float, "revs/min"),
    ("SteeringWheelAngle", VarType::Float, "rad"),
    ("YawRate", VarType::Float, "rad/s"),
    ("Yaw", VarType::Float, "rad"),
    ("LatAccel", VarType::Float, "m/s^2"),
    ("LongAccel", VarType::Float, "m/s^2"),
    ("Lat", VarType::Double, "deg"),
    ("Lon", VarType::Double, "deg"),
    ("IsOnTrackCar", VarType::Bool, ""),
    ("OnPitRoad", VarType::Bool, ""),
    ("PlayerTrackSurface", VarType::Int, "irsdk_TrkLoc"),
];
