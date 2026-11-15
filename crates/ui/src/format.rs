//! Mise en forme des valeurs affichées.

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

/// Écart signé : `+0.123`, `-1.045`.
pub fn delta(secs: f64) -> String {
    if !secs.is_finite() {
        return String::new();
    }
    if secs.abs() < 0.0005 {
        return "±0.000".into();
    }
    format!("{secs:+.3}")
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
        Direction::Left => "gauche",
        Direction::Right => "droite",
    }
}

pub fn lap_status(f: &LapFlags) -> &'static str {
    if !f.complete {
        "partiel"
    } else if f.pit {
        "stands"
    } else if f.discontinuity {
        "interrompu"
    } else if f.off_track > 0 {
        "hors piste"
    } else {
        "valide"
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
}
