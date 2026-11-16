//! State of the comparison to the reference, independent of GPUI.

use std::sync::Arc;

use analysis::{Analyzer, LapComparison};
use reference::ReferenceLap;
use telemetry::{Lap, LiveSnapshot};

pub struct ReferenceState {
    pub reference: ReferenceLap,
    pub analyzer: Arc<Analyzer>,
}

impl ReferenceState {
    pub fn new(reference: ReferenceLap, session_length_m: Option<f64>) -> Self {
        let analyzer = Analyzer::new(
            &reference.lap,
            reference.track_length_m.or(session_length_m),
            &telemetry::CornerDetectionParams::default(),
        );
        ReferenceState {
            reference,
            analyzer: Arc::new(analyzer),
        }
    }
}

impl ReferenceState {
    /// Relative gap between the session track length and the reference one:
    /// a notable gap signals another track or layout.
    pub fn length_mismatch(&self, session_length_m: Option<f64>) -> Option<String> {
        let s = session_length_m?;
        let r = self.analyzer.model.length_m;
        let rel = (s - r).abs() / s;
        (rel > 0.03).then(|| {
            format!(
                "Track length differs: session {:.2} km, reference {:.2} km",
                s / 1000.0,
                r / 1000.0
            )
        })
    }
}

/// Comparisons of the last complete lap and of the best lap, recomputed only
/// when those laps change.
#[derive(Default)]
pub struct Comparisons {
    pub last: Option<Arc<LapComparison>>,
    pub best: Option<Arc<LapComparison>>,
    last_key: Option<u64>,
    best_key: Option<u64>,
}

fn key(lap: &Lap) -> u64 {
    lap.start_time.to_bits()
}

impl Comparisons {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn update(&mut self, snap: &LiveSnapshot, analyzer: &Analyzer) {
        let last = snap.laps.iter().rev().find(|l| l.flags.complete);
        match last {
            Some(l) if self.last_key != Some(key(l)) => {
                self.last_key = Some(key(l));
                self.last = Some(Arc::new(analyzer.compare(l)));
            }
            None => {
                self.last_key = None;
                self.last = None;
            }
            _ => {}
        }
        match &snap.best_lap {
            Some(l) if self.best_key != Some(key(l)) => {
                self.best_key = Some(key(l));
                self.best = Some(Arc::new(analyzer.compare(l)));
            }
            None => {
                self.best_key = None;
                self.best = None;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use telemetry::synthetic::SyntheticTrack;
    use telemetry::{CornerDetectionParams, IbtFile, LapRecorder};

    #[test]
    fn comparisons_follow_snapshot() {
        let track = SyntheticTrack::demo();
        let file = IbtFile::from_bytes(track.session_ibt(3, 5).to_bytes()).unwrap();
        let reference = reference::from_ibt(&file, "ref.ibt").unwrap();
        let analyzer = Analyzer::new(
            &reference.lap,
            reference.track_length_m,
            &CornerDetectionParams::default(),
        );
        let mut rec = LapRecorder::default();
        for s in file.samples() {
            rec.push(s);
        }
        let snap = LiveSnapshot {
            laps: rec.history().cloned().collect(),
            best_lap: rec.best_lap().cloned(),
            ..Default::default()
        };
        let mut c = Comparisons::default();
        c.update(&snap, &analyzer);
        let best = c.best.clone().unwrap();
        // The best lap of the file is the reference: zero delta.
        assert!(best.total_delta().abs() < 1e-9);
        assert!(c.last.is_some());
        let before = Arc::as_ptr(&best);
        c.update(&snap, &analyzer);
        assert_eq!(
            Arc::as_ptr(c.best.as_ref().unwrap()),
            before,
            "no recomputation"
        );

        let state = ReferenceState {
            reference,
            analyzer: Arc::new(analyzer),
        };
        assert!(state.length_mismatch(Some(track.length_m())).is_none());
        assert!(state.length_mismatch(Some(7000.0)).is_some());
    }
}
