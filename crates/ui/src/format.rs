//! Formatting of displayed values.

use telemetry::{Direction, LapFlags};

/// `83.456` → `1:23.456`.
pub fn lap_time(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "–:––.–––".into();
    }
    let total_ms = (secs * 1000.0).round() as u64;
    let m = total_ms / 60_000;
    let s = (total_ms / 1000) % 60;
    let ms = total_ms % 1000;
    format!("{m}:{s:02}.{ms:03}")
}

/// Signed gap: `+0.123`, `-1.045`.
pub fn delta(secs: f64) -> String {
    if !secs.is_finite() {
        return String::new();
    }
    if secs.abs() < 0.0005 {
        return "±0.000".into();
    }
    format!("{secs:+.3}")
}

/// Position gap in metres: `+12 m`.
pub fn meters(d: Option<f64>) -> String {
    match d {
        Some(d) if d.is_finite() => format!("{d:+.0} m"),
        _ => "–".into(),
    }
}

/// Unitless ratio: `0.92`.
pub fn ratio(r: Option<f64>) -> String {
    match r {
        Some(r) if r.is_finite() => format!("{r:.2}"),
        _ => "–".into(),
    }
}

pub fn kmh(mps: f32) -> String {
    if mps.is_finite() {
        format!("{:.0}", mps * 3.6)
    } else {
        "–".into()
    }
}

pub fn gear(g: i32) -> String {
    match g {
        -1 => "R".into(),
        0 => "N".into(),
        g => g.to_string(),
    }
}

pub fn direction(d: Direction) -> &'static str {
    match d {
        Direction::Left => "left",
        Direction::Right => "right",
    }
}

pub fn lap_status(f: &LapFlags) -> &'static str {
    if !f.complete {
        "partial"
    } else if f.pit {
        "pit"
    } else if f.discontinuity {
        "interrupted"
    } else if f.off_track > 0 {
        "off track"
    } else {
        "valid"
    }
}

/// Ratio shown as a change: 0.88 → "−12%", 1.2 → "+20%".
pub fn percent_change(r: Option<f64>) -> String {
    match r {
        Some(r) if r.is_finite() => {
            let p = (r - 1.0) * 100.0;
            if p.abs() < 0.5 {
                "0%".into()
            } else if p > 0.0 {
                format!("+{p:.0}%")
            } else {
                format!("−{:.0}%", -p)
            }
        }
        _ => "–".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_lap_time() {
        assert_eq!(lap_time(83.4564), "1:23.456");
        assert_eq!(lap_time(59.9996), "1:00.000");
        assert_eq!(lap_time(f64::NAN), "–:––.–––");
    }

    #[test]
    fn formats_delta() {
        assert_eq!(delta(0.1234), "+0.123");
        assert_eq!(delta(-1.0449), "-1.045");
        assert_eq!(delta(0.0001), "±0.000");
    }

    #[test]
    fn percent_change_formats_ratios() {
        assert_eq!(percent_change(Some(0.88)), "−12%");
        assert_eq!(percent_change(Some(1.2)), "+20%");
        assert_eq!(percent_change(Some(1.001)), "0%");
        assert_eq!(percent_change(None), "–");
    }
}
