//! Minimal extraction of session information (iRacing YAML).
//!
//! iRacing's YAML is not always valid (unescaped names), hence a line-by-line
//! read of the few useful keys rather than a full parser.

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionInfo {
    pub track_id: Option<i64>,
    pub track_name: String,
    pub track_display_name: String,
    pub track_config: String,
    /// Track length, m.
    pub track_length_m: Option<f64>,
    pub car_screen_name: String,
    pub driver_car_idx: Option<i64>,
}

fn value_of<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let t = line.trim_start().trim_start_matches("- ");
    let rest = t.strip_prefix(key)?.strip_prefix(':')?;
    Some(rest.trim())
}

fn parse_length_m(v: &str) -> Option<f64> {
    let mut it = v.split_whitespace();
    let n: f64 = it.next()?.parse().ok()?;
    match it.next() {
        Some("km") => Some(n * 1000.0),
        Some("mi") => Some(n * 1609.344),
        Some("m") | None => Some(n),
        _ => None,
    }
}

impl SessionInfo {
    pub fn parse(yaml: &str) -> Self {
        let mut s = SessionInfo::default();
        let mut section = "";
        for line in yaml.lines() {
            if !line.starts_with(' ') && line.ends_with(':') {
                section = line.trim_end_matches(':');
                continue;
            }
            if section == "WeekendInfo" {
                if let Some(v) = value_of(line, "TrackID") {
                    s.track_id = v.parse().ok();
                } else if let Some(v) = value_of(line, "TrackName") {
                    s.track_name = v.to_string();
                } else if let Some(v) = value_of(line, "TrackDisplayName") {
                    s.track_display_name = v.to_string();
                } else if let Some(v) = value_of(line, "TrackConfigName") {
                    s.track_config = v.to_string();
                } else if let Some(v) = value_of(line, "TrackLength") {
                    s.track_length_m = parse_length_m(v);
                }
            } else if section == "DriverInfo"
                && let Some(v) = value_of(line, "DriverCarIdx")
            {
                s.driver_car_idx = v.parse().ok();
            }
        }
        // Second pass: player's car name in the driver list.
        if let Some(idx) = s.driver_car_idx {
            let mut in_player = false;
            for line in yaml.lines() {
                if let Some(v) = value_of(line, "CarIdx") {
                    in_player = v.parse::<i64>().ok() == Some(idx);
                } else if in_player && let Some(v) = value_of(line, "CarScreenName") {
                    s.car_screen_name = v.to_string();
                    break;
                }
            }
        }
        s
    }

    pub fn track_label(&self) -> String {
        match (
            self.track_display_name.is_empty(),
            self.track_config.is_empty(),
        ) {
            (true, _) => self.track_name.clone(),
            (false, true) => self.track_display_name.clone(),
            (false, false) => format!("{} – {}", self.track_display_name, self.track_config),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const YAML: &str = "---
WeekendInfo:
 TrackName: spa 2024 up
 TrackID: 524
 TrackLength: 6.93 km
 TrackDisplayName: Circuit de Spa-Francorchamps
 TrackConfigName: Grand Prix Pits
DriverInfo:
 DriverCarIdx: 1
 Drivers:
 - CarIdx: 0
   CarScreenName: Pace Car
 - CarIdx: 1
   UserName: Test Driver
   CarScreenName: Porsche 911 GT3 R (992)
";

    #[test]
    fn parse_session_info() {
        let s = SessionInfo::parse(YAML);
        assert_eq!(s.track_id, Some(524));
        assert_eq!(s.track_length_m, Some(6930.0));
        assert_eq!(s.car_screen_name, "Porsche 911 GT3 R (992)");
        assert_eq!(
            s.track_label(),
            "Circuit de Spa-Francorchamps – Grand Prix Pits"
        );
    }
}
