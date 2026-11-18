//! Reference lap import.
//!
//! - CSV exported from Garage61 (one lap, 60 Hz, iRacing channel names).
//!   Columns are recognized by name; missing ones are `NaN`.
//! - .ibt file: the best valid lap in the file.
//!
//! Expected Garage61 columns (not checked against a real export, see
//! `docs/verifications.md`): `Speed, LapDistPct, Lat, Lon, Brake, Throttle, RPM,
//! SteeringWheelAngle, Gear, Clutch, ABSActive, DRSActive, LatAccel, LongAccel,
//! VertAccel, Yaw, YawRate, PositionType`.

pub mod estimate;

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

use telemetry::{IbtFile, Lap, LapFlags, LapRecorder, LapRecorderConfig, Sample, TrackSurface};
use thiserror::Error;

pub use estimate::{ChannelOrigin, RotationOrigin};

#[derive(Debug, Error)]
pub enum ReferenceError {
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("CSV: {0}")]
    Csv(#[from] csv::Error),
    #[error(".ibt: {0}")]
    Ibt(#[from] telemetry::ibt::IbtError),
    #[error("required column missing: {0}")]
    MissingColumn(&'static str),
    #[error("unusable lap: {0}")]
    Unusable(String),
    #[error("unrecognized format (expected .csv or .ibt): {0}")]
    UnknownFormat(String),
}

/// Metadata taken from the Garage61 file name:
/// `Garage 61 - <driver> - <car> - <track> - MM.SS.mmm - <id>.csv`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Garage61Meta {
    pub driver: Option<String>,
    pub car: Option<String>,
    pub track: Option<String>,
    pub lap_time: Option<f64>,
    pub lap_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ReferenceLap {
    /// Displayed label (driver, car, time).
    pub label: String,
    pub lap: Lap,
    pub rotation: RotationOrigin,
    pub driver: Option<String>,
    pub car: Option<String>,
    pub track: Option<String>,
    /// Known track length (.ibt YAML), m.
    pub track_length_m: Option<f64>,
}

/// Loads a reference according to the file extension.
pub fn load(path: impl AsRef<Path>) -> Result<ReferenceLap, ReferenceError> {
    let path = path.as_ref();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .as_deref()
    {
        Some("csv") => from_garage61_csv(std::fs::File::open(path)?, &name),
        Some("ibt") => from_ibt(&IbtFile::open(path)?, &name),
        _ => Err(ReferenceError::UnknownFormat(name)),
    }
}

pub fn parse_garage61_filename(name: &str) -> Garage61Meta {
    let stem = name.strip_suffix(".csv").unwrap_or(name);
    let parts: Vec<&str> = stem.split(" - ").map(str::trim).collect();
    if parts.len() < 6 || !parts[0].eq_ignore_ascii_case("Garage 61") {
        return Garage61Meta::default();
    }
    let n = parts.len();
    let lap_time = parse_lap_time(parts[n - 2]);
    Garage61Meta {
        driver: Some(parts[1].to_string()),
        car: Some(parts[2].to_string()),
        // The track name may itself contain " - ".
        track: Some(parts[3..n - 2].join(" - ")),
        lap_time,
        lap_id: Some(parts[n - 1].to_string()),
    }
}

/// `01.58.123` or `1:58.123` → 118.123 s.
fn parse_lap_time(s: &str) -> Option<f64> {
    let s = s.replace(':', ".");
    let p: Vec<&str> = s.split('.').collect();
    match p.as_slice() {
        [m, sec, ms] => {
            let m: f64 = m.parse().ok()?;
            let sec: f64 = sec.parse().ok()?;
            let frac: f64 = format!("0.{ms}").parse().ok()?;
            Some(m * 60.0 + sec + frac)
        }
        _ => None,
    }
}

fn parse_value(s: &str) -> f64 {
    let t = s.trim();
    match t {
        "true" | "True" | "TRUE" => 1.0,
        "false" | "False" | "FALSE" => 0.0,
        _ => t.parse().unwrap_or(f64::NAN),
    }
}

/// Reads a Garage61 CSV. `file_name` is used for metadata.
pub fn from_garage61_csv(
    reader: impl Read,
    file_name: &str,
) -> Result<ReferenceLap, ReferenceError> {
    const RATE: f64 = 60.0;
    let mut rdr = csv::ReaderBuilder::new().flexible(true).from_reader(reader);
    let headers: HashMap<String, usize> = rdr
        .headers()?
        .iter()
        .enumerate()
        .map(|(i, h)| (h.trim().to_string(), i))
        .collect();
    let col = |name: &str| headers.get(name).copied();
    let pct_col = col("LapDistPct").ok_or(ReferenceError::MissingColumn("LapDistPct"))?;
    let speed_col = col("Speed").ok_or(ReferenceError::MissingColumn("Speed"))?;

    let names = [
        "Throttle",
        "Brake",
        "Clutch",
        "Gear",
        "RPM",
        "SteeringWheelAngle",
        "YawRate",
        "LatAccel",
        "LongAccel",
        "Lat",
        "Lon",
        "Yaw",
        "PositionType",
    ];
    let cols: Vec<Option<usize>> = names.iter().map(|n| col(n)).collect();
    let mut rows: Vec<[f64; 15]> = vec![];
    for rec in rdr.records() {
        let rec = rec?;
        let get = |c: Option<usize>| {
            c.and_then(|c| rec.get(c))
                .map(parse_value)
                .unwrap_or(f64::NAN)
        };
        let mut r = [f64::NAN; 15];
        r[0] = get(Some(pct_col));
        r[1] = get(Some(speed_col));
        for (k, c) in cols.iter().enumerate() {
            r[2 + k] = get(*c);
        }
        rows.push(r);
    }
    if rows.len() < 60 {
        return Err(ReferenceError::Unusable(format!(
            "only {} rows",
            rows.len()
        )));
    }

    // Units: some exports give percentages 0..100.
    let max_of = |k: usize| {
        rows.iter()
            .map(|r| r[k])
            .filter(|x| x.is_finite())
            .fold(f64::NEG_INFINITY, f64::max)
    };
    let pct_scale = if max_of(0) > 1.5 { 0.01 } else { 1.0 };
    let throttle_scale = if max_of(2) > 1.5 { 0.01 } else { 1.0 };
    let brake_scale = if max_of(3) > 1.5 { 0.01 } else { 1.0 };

    let mut samples: Vec<Sample> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| Sample {
            session_time: i as f64 / RATE,
            tick: i as i32,
            lap: 1,
            lap_dist_pct: (r[0] * pct_scale) as f32,
            lap_dist: f32::NAN,
            lap_current_time: (i as f64 / RATE) as f32,
            lap_last_time: f32::NAN,
            speed: r[1] as f32,
            throttle: (r[2] * throttle_scale) as f32,
            brake: (r[3] * brake_scale) as f32,
            clutch: r[4] as f32,
            gear: if r[5].is_finite() { r[5] as i32 } else { 0 },
            rpm: r[6] as f32,
            steering: r[7] as f32,
            yaw_rate: r[8] as f32,
            yaw: f32::NAN,
            lat_accel: r[9] as f32,
            long_accel: r[10] as f32,
            lat: r[11],
            lon: r[12],
            is_on_track: true,
            on_pit_road: false,
            surface: if r[14].is_finite() {
                TrackSurface::from_raw(r[14] as i32)
            } else {
                TrackSurface::OnTrack
            },
        })
        .collect();
    let yaw: Vec<f64> = rows.iter().map(|r| r[13]).collect();

    // Lap edges: drop the end of the previous lap at the head and the start of
    // the next lap at the tail, to keep pct increasing.
    let start = samples
        .iter()
        .position(|s| s.lap_dist_pct < 0.5)
        .unwrap_or(0);
    let end = samples
        .iter()
        .rposition(|s| s.lap_dist_pct > 0.5)
        .map(|i| i + 1)
        .unwrap_or(samples.len());
    let yaw = &yaw[start..end.max(start)];
    samples.truncate(end);
    samples.drain(..start);
    if samples.len() < 60 {
        return Err(ReferenceError::Unusable("lap too short".into()));
    }
    let t0 = samples[0].session_time;
    for s in &mut samples {
        s.session_time -= t0;
        s.lap_current_time = s.session_time as f32;
    }
    let rotation = estimate::fill_rotation(&mut samples, Some(yaw));

    let meta = parse_garage61_filename(file_name);
    let lap_time = meta
        .lap_time
        .unwrap_or_else(|| extrapolated_lap_time(&samples));
    let label = match (&meta.driver, &meta.car) {
        (Some(d), Some(c)) => format!("{d} · {c} · {}", format_time(lap_time)),
        _ => format!("{file_name} · {}", format_time(lap_time)),
    };
    Ok(ReferenceLap {
        label,
        lap: Lap {
            number: 1,
            start_time: 0.0,
            lap_time,
            flags: LapFlags {
                complete: true,
                ..Default::default()
            },
            samples,
        },
        rotation,
        driver: meta.driver,
        car: meta.car,
        track: meta.track,
        track_length_m: None,
    })
}

/// Lap time when the export stops before the line: extrapolated at the
/// progress rate of the last seconds.
fn extrapolated_lap_time(samples: &[Sample]) -> f64 {
    let last = samples[samples.len() - 1];
    let back = &samples[samples.len().saturating_sub(60)];
    let dp = (last.lap_dist_pct - back.lap_dist_pct) as f64;
    let dt = last.session_time - back.session_time;
    let remaining = (1.0 - last.lap_dist_pct as f64).max(0.0);
    if dp > 0.0 && dt > 0.0 {
        last.session_time + remaining * dt / dp
    } else {
        last.session_time
    }
}

fn format_time(secs: f64) -> String {
    let ms = (secs * 1000.0).round() as u64;
    format!("{}:{:02}.{:03}", ms / 60_000, (ms / 1000) % 60, ms % 1000)
}

/// Best valid lap of an .ibt.
pub fn from_ibt(file: &IbtFile, file_name: &str) -> Result<ReferenceLap, ReferenceError> {
    let mut rec = LapRecorder::new(LapRecorderConfig {
        history: 10_000,
        ..Default::default()
    });
    for s in file.samples() {
        rec.push(s);
    }
    let best = rec
        .best_lap()
        .ok_or_else(|| ReferenceError::Unusable("no valid lap in the file".into()))?;
    let mut lap = (**best).clone();
    let rotation = estimate::fill_rotation(&mut lap.samples, None);
    let car =
        (!file.session.car_screen_name.is_empty()).then(|| file.session.car_screen_name.clone());
    let track = Some(file.session.track_label()).filter(|t| !t.is_empty());
    Ok(ReferenceLap {
        label: format!(
            "{} · lap {} · {}",
            file_name,
            lap.number,
            format_time(lap.lap_time)
        ),
        lap,
        rotation,
        driver: None,
        car,
        track,
        track_length_m: file.session.track_length_m,
    })
}

/// Writes a lap in Garage61 CSV format (tests and tools).
pub fn write_garage61_csv(samples: &[Sample], columns: &[&str]) -> String {
    let mut out = columns.join(",");
    out.push('\n');
    for s in samples {
        let vals: Vec<String> = columns
            .iter()
            .map(|c| match *c {
                "Speed" => format!("{}", s.speed),
                "LapDistPct" => format!("{}", s.lap_dist_pct),
                "Lat" => format!("{}", s.lat as f32),
                "Lon" => format!("{}", s.lon as f32),
                "Brake" => format!("{}", s.brake),
                "Throttle" => format!("{}", s.throttle),
                "RPM" => format!("{}", s.rpm),
                "SteeringWheelAngle" => format!("{}", s.steering),
                "Gear" => format!("{}", s.gear),
                "Clutch" => format!("{}", s.clutch),
                "LatAccel" => format!("{}", s.lat_accel),
                "LongAccel" => format!("{}", s.long_accel),
                "YawRate" => format!("{}", s.yaw_rate),
                "PositionType" => format!("{}", s.surface.to_raw()),
                _ => "0".to_string(),
            })
            .collect();
        out.push_str(&vals.join(","));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use telemetry::synthetic::SyntheticTrack;
    use telemetry::{CornerDetectionParams, corners::detect_corners_in};

    const ALL: &[&str] = &[
        "Speed",
        "LapDistPct",
        "Lat",
        "Lon",
        "Brake",
        "Throttle",
        "RPM",
        "SteeringWheelAngle",
        "Gear",
        "Clutch",
        "ABSActive",
        "DRSActive",
        "LatAccel",
        "LongAccel",
        "VertAccel",
        "Yaw",
        "YawRate",
        "PositionType",
    ];

    #[test]
    fn parses_filename() {
        let m = parse_garage61_filename(
            "Garage 61 - John Driver - Porsche 911 GT3 R (992) - Circuit de Spa-Francorchamps - Grand Prix Pits - 02.17.456 - 01ABC.csv",
        );
        assert_eq!(m.driver.as_deref(), Some("John Driver"));
        assert_eq!(
            m.track.as_deref(),
            Some("Circuit de Spa-Francorchamps - Grand Prix Pits")
        );
        assert!((m.lap_time.unwrap() - 137.456).abs() < 1e-9);
        assert_eq!(m.lap_id.as_deref(), Some("01ABC"));
        assert_eq!(
            parse_garage61_filename("export.csv"),
            Garage61Meta::default()
        );
    }

    #[test]
    fn imports_full_csv() {
        let track = SyntheticTrack::demo();
        let lap = track.simulate_lap(0.98, 0.0, 0.0);
        let truth = lap.len() as f64 / 60.0;
        let csv = write_garage61_csv(&lap, ALL);
        let r = from_garage61_csv(csv.as_bytes(), "lap.csv").unwrap();
        assert_eq!(r.rotation.yaw_rate, ChannelOrigin::Measured);
        assert_eq!(r.lap.samples.len(), lap.len());
        assert!((r.lap.lap_time - truth).abs() < 0.05, "{}", r.lap.lap_time);
    }

    #[test]
    fn estimates_yaw_rate_from_gps_when_missing() {
        let track = SyntheticTrack::demo();
        let lap = track.simulate_lap(1.0, 0.0, 0.0);
        let cols: Vec<&str> = ALL
            .iter()
            .copied()
            .filter(|c| !matches!(*c, "YawRate" | "LatAccel" | "Yaw"))
            .collect();
        let csv = write_garage61_csv(&lap, &cols);
        let r = from_garage61_csv(csv.as_bytes(), "lap.csv").unwrap();
        assert_eq!(r.rotation.yaw_rate, ChannelOrigin::FromPosition);
        assert_eq!(r.rotation.lat_accel, ChannelOrigin::FromYawRate);

        // In the middle of each corner, the mean estimate is close to the true
        // value. At straight/corner transitions, the synthetic curvature is
        // discontinuous and smoothing introduces a lag: they are not judged.
        let total = track.length_m();
        for (_, mid) in track.expected_corners() {
            let near = |s: &&Sample| ((s.lap_dist_pct as f64 - mid) * total).abs() < 10.0;
            let mean = |v: &[Sample]| {
                let xs: Vec<f64> = v.iter().filter(near).map(|s| s.yaw_rate as f64).collect();
                xs.iter().sum::<f64>() / xs.len() as f64
            };
            let (est, truth) = (mean(&r.lap.samples), mean(&lap));
            eprintln!("corner at {mid:.3}: estimated {est:.3}, true {truth:.3}");
            assert!(
                (est - truth).abs() < 0.1 * truth.abs(),
                "{est} vs {truth} at {mid}"
            );
        }

        let corners = detect_corners_in(&r.lap.samples, &CornerDetectionParams::default());
        assert_eq!(corners.len(), track.expected_corners().len());
    }

    #[test]
    fn trims_wrapped_edges_and_extrapolates_missing_end() {
        let track = SyntheticTrack::demo();
        let lap = track.simulate_lap(1.0, 0.0, 0.0);
        let truth = lap.len() as f64 / 60.0;
        // Two samples from the previous lap at the head, lap end cut at 98 %.
        let mut rows = vec![lap[lap.len() - 2], lap[lap.len() - 1]];
        rows.extend(lap.iter().copied().filter(|s| s.lap_dist_pct < 0.98));
        let csv = write_garage61_csv(&rows, ALL);
        let r = from_garage61_csv(csv.as_bytes(), "lap.csv").unwrap();
        assert!(r.lap.samples[0].lap_dist_pct < 0.01);
        assert!(
            (r.lap.lap_time - truth).abs() < 0.3,
            "{} vs {truth}",
            r.lap.lap_time
        );
    }

    #[test]
    fn percent_units_are_rescaled() {
        let track = SyntheticTrack::demo();
        let mut lap = track.simulate_lap(1.0, 0.0, 0.0);
        for s in &mut lap {
            s.lap_dist_pct *= 100.0;
            s.throttle *= 100.0;
            s.brake *= 100.0;
        }
        let r = from_garage61_csv(write_garage61_csv(&lap, ALL).as_bytes(), "t.csv").unwrap();
        let max_pct = r
            .lap
            .samples
            .iter()
            .map(|s| s.lap_dist_pct)
            .fold(0.0, f32::max);
        assert!(max_pct <= 1.0);
        assert!(r.lap.samples.iter().all(|s| s.throttle <= 1.0));
    }

    #[test]
    fn missing_required_column_is_an_error() {
        let err = from_garage61_csv("Speed,Throttle\n1,2\n".as_bytes(), "t.csv").unwrap_err();
        assert!(matches!(err, ReferenceError::MissingColumn("LapDistPct")));
    }

    #[test]
    fn best_lap_from_ibt() {
        let track = SyntheticTrack::demo();
        let file = IbtFile::from_bytes(track.session_ibt(4, 3).to_bytes()).unwrap();
        let r = from_ibt(&file, "s.ibt").unwrap();
        assert!(r.lap.is_valid());
        assert_eq!(r.rotation.yaw_rate, ChannelOrigin::Measured);
        assert_eq!(r.car.as_deref(), Some("Synthetic car"));
    }
}
