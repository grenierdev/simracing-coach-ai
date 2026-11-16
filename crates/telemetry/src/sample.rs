//! Decoded telemetry sample: the channels useful for coaching.

use crate::irsdk::{VarHeader, VarTable, read_value};

/// Surface under the car (`irsdk_TrkLoc`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrackSurface {
    #[default]
    NotInWorld,
    OffTrack,
    InPitStall,
    ApproachingPits,
    OnTrack,
}

impl TrackSurface {
    pub fn from_raw(v: i32) -> Self {
        match v {
            0 => TrackSurface::OffTrack,
            1 => TrackSurface::InPitStall,
            2 => TrackSurface::ApproachingPits,
            3 => TrackSurface::OnTrack,
            _ => TrackSurface::NotInWorld,
        }
    }

    pub fn to_raw(self) -> i32 {
        match self {
            TrackSurface::NotInWorld => -1,
            TrackSurface::OffTrack => 0,
            TrackSurface::InPitStall => 1,
            TrackSurface::ApproachingPits => 2,
            TrackSurface::OnTrack => 3,
        }
    }
}

/// One sample at 60 Hz. SI units, like iRacing.
/// Channels missing from the source are `NaN` (floats) or their default.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Sample {
    /// `SessionTime`, s.
    pub session_time: f64,
    /// `SessionTick`.
    pub tick: i32,
    /// `Lap`: current lap number.
    pub lap: i32,
    /// `LapDistPct`: 0..1 from the start line.
    pub lap_dist_pct: f32,
    /// `LapDist`, m.
    pub lap_dist: f32,
    /// `LapCurrentLapTime`, s (iRacing estimate).
    pub lap_current_time: f32,
    /// `LapLastLapTime`, s.
    pub lap_last_time: f32,
    /// `Speed`, m/s.
    pub speed: f32,
    /// `Throttle`, 0..1.
    pub throttle: f32,
    /// `Brake`, 0..1.
    pub brake: f32,
    /// `Clutch`, 0..1 (1 = clutch released in iRacing).
    pub clutch: f32,
    /// `Gear`: -1 reverse, 0 neutral.
    pub gear: i32,
    /// `RPM`.
    pub rpm: f32,
    /// `SteeringWheelAngle`, rad, positive to the left.
    pub steering: f32,
    /// `YawRate`, rad/s.
    pub yaw_rate: f32,
    /// `LatAccel`, m/s².
    pub lat_accel: f32,
    /// `LongAccel`, m/s².
    pub long_accel: f32,
    /// `Lat`, degrees (often present only in .ibt files).
    pub lat: f64,
    /// `Lon`, degrees.
    pub lon: f64,
    /// `IsOnTrack` (or `IsOnTrackCar` in .ibt).
    pub is_on_track: bool,
    /// `OnPitRoad`.
    pub on_pit_road: bool,
    /// `PlayerTrackSurface`.
    pub surface: TrackSurface,
}

/// Mapping between [`Sample`] fields and source variables.
/// Resolved once per connection (the variable table does not change during a session).
#[derive(Debug, Clone, Default)]
pub struct ChannelMap {
    session_time: Option<VarHeader>,
    tick: Option<VarHeader>,
    lap: Option<VarHeader>,
    lap_dist_pct: Option<VarHeader>,
    lap_dist: Option<VarHeader>,
    lap_current_time: Option<VarHeader>,
    lap_last_time: Option<VarHeader>,
    speed: Option<VarHeader>,
    throttle: Option<VarHeader>,
    brake: Option<VarHeader>,
    clutch: Option<VarHeader>,
    gear: Option<VarHeader>,
    rpm: Option<VarHeader>,
    steering: Option<VarHeader>,
    yaw_rate: Option<VarHeader>,
    lat_accel: Option<VarHeader>,
    long_accel: Option<VarHeader>,
    lat: Option<VarHeader>,
    lon: Option<VarHeader>,
    is_on_track: Option<VarHeader>,
    on_pit_road: Option<VarHeader>,
    surface: Option<VarHeader>,
}

/// Channels without which lap splitting is impossible.
pub const REQUIRED_CHANNELS: &[&str] = &["SessionTime", "Lap", "LapDistPct", "Speed"];

impl ChannelMap {
    pub fn new(table: &VarTable) -> Self {
        let g = |names: &[&str]| names.iter().find_map(|n| table.get(n).cloned());
        ChannelMap {
            session_time: g(&["SessionTime"]),
            tick: g(&["SessionTick"]),
            lap: g(&["Lap"]),
            lap_dist_pct: g(&["LapDistPct"]),
            lap_dist: g(&["LapDist"]),
            lap_current_time: g(&["LapCurrentLapTime"]),
            lap_last_time: g(&["LapLastLapTime"]),
            speed: g(&["Speed"]),
            throttle: g(&["Throttle"]),
            brake: g(&["Brake"]),
            clutch: g(&["Clutch"]),
            gear: g(&["Gear"]),
            rpm: g(&["RPM"]),
            steering: g(&["SteeringWheelAngle"]),
            yaw_rate: g(&["YawRate"]),
            lat_accel: g(&["LatAccel"]),
            long_accel: g(&["LongAccel"]),
            lat: g(&["Lat"]),
            lon: g(&["Lon"]),
            is_on_track: g(&["IsOnTrack", "IsOnTrackCar"]),
            on_pit_road: g(&["OnPitRoad"]),
            surface: g(&["PlayerTrackSurface"]),
        }
    }

    /// Names of missing required channels.
    pub fn missing_required(table: &VarTable) -> Vec<&'static str> {
        REQUIRED_CHANNELS
            .iter()
            .copied()
            .filter(|n| table.get(n).is_none())
            .collect()
    }

    pub fn decode(&self, row: &[u8]) -> Sample {
        let f = |v: &Option<VarHeader>| {
            v.as_ref()
                .and_then(|v| read_value(row, v))
                .unwrap_or(f64::NAN)
        };
        let i = |v: &Option<VarHeader>, default: i32| {
            v.as_ref()
                .and_then(|v| read_value(row, v))
                .map(|x| x as i32)
                .unwrap_or(default)
        };
        let b = |v: &Option<VarHeader>, default: bool| {
            v.as_ref()
                .and_then(|v| read_value(row, v))
                .map(|x| x != 0.0)
                .unwrap_or(default)
        };
        Sample {
            session_time: f(&self.session_time),
            tick: i(&self.tick, 0),
            lap: i(&self.lap, 0),
            lap_dist_pct: f(&self.lap_dist_pct) as f32,
            lap_dist: f(&self.lap_dist) as f32,
            lap_current_time: f(&self.lap_current_time) as f32,
            lap_last_time: f(&self.lap_last_time) as f32,
            speed: f(&self.speed) as f32,
            throttle: f(&self.throttle) as f32,
            brake: f(&self.brake) as f32,
            clutch: f(&self.clutch) as f32,
            gear: i(&self.gear, 0),
            rpm: f(&self.rpm) as f32,
            steering: f(&self.steering) as f32,
            yaw_rate: f(&self.yaw_rate) as f32,
            lat_accel: f(&self.lat_accel) as f32,
            long_accel: f(&self.long_accel) as f32,
            lat: f(&self.lat),
            lon: f(&self.lon),
            is_on_track: b(&self.is_on_track, true),
            on_pit_road: b(&self.on_pit_road, false),
            surface: self
                .surface
                .as_ref()
                .and_then(|v| read_value(row, v))
                .map(|x| TrackSurface::from_raw(x as i32))
                .unwrap_or(TrackSurface::OnTrack),
        }
    }
}
