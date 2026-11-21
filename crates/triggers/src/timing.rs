//! Speaking time available on the track, measured on the reference lap.
//!
//! The engine needs to know whether a sentence ends before the next quiet
//! zone. The car's current speed is a poor guide: on a corner exit the car is
//! slow and accelerating, so the remaining distance goes by much faster than
//! the current speed suggests. The reference lap gives the time to cover each
//! stretch at race pace, which errs on the safe side for a slower driver.

use std::sync::Arc;

use analysis::{AlignedLap, Analyzer};

use crate::plan::{PlanSettings, QuietZone, TrackContext, Trigger};

/// Safety margin between the end of a sentence and the quiet zone, s.
pub const FIT_MARGIN_S: f64 = 0.4;

/// Same estimate as `voice::estimate_duration`, for callers without the
/// speech engine (the MCP server reports whether a cue fits).
pub fn estimate_speech_s(text: &str) -> f64 {
    0.35 + text.chars().count() as f64 / 16.0
}

fn forward(a: f64, b: f64) -> f64 {
    (b - a).rem_euclid(1.0)
}

/// Reference time along the lap.
#[derive(Debug, Clone)]
pub struct TimeProfile {
    reference: Arc<AlignedLap>,
}

impl TimeProfile {
    /// `None` when the reference timing is unusable (no lap time).
    pub fn from_analyzer(analyzer: &Analyzer) -> Option<Self> {
        let r = &analyzer.reference;
        (r.lap_time.is_finite() && r.lap_time > 1.0).then(|| TimeProfile {
            reference: r.clone(),
        })
    }

    /// Time to drive from `a` to `b` at reference pace, forward, s.
    pub fn time_between(&self, a: f64, b: f64) -> f64 {
        let (a, b) = (a.rem_euclid(1.0), b.rem_euclid(1.0));
        let (ta, tb) = (self.reference.time_at(a), self.reference.time_at(b));
        if b >= a {
            tb - ta
        } else {
            self.reference.lap_time - ta + tb
        }
    }

    /// Time before the next quiet zone starts, s. Zero inside a quiet zone,
    /// infinite without quiet zones.
    pub fn time_to_quiet(&self, pct: f64, quiet: &[QuietZone]) -> f64 {
        if quiet.iter().any(|z| z.contains(pct)) {
            return 0.0;
        }
        quiet
            .iter()
            .map(|z| self.time_between(pct, z.start))
            .fold(f64::INFINITY, f64::min)
    }
}

/// End of the quiet zone that precedes `pct` (the start of its straight).
fn previous_quiet_end(pct: f64, quiet: &[QuietZone]) -> Option<f64> {
    quiet
        .iter()
        .filter(|z| !z.contains(pct))
        .min_by(|a, b| forward(a.end, pct).total_cmp(&forward(b.end, pct)))
        .map(|z| z.end)
}

/// Start of the next quiet zone after `pct` (the end of its straight).
fn next_quiet_start(pct: f64, quiet: &[QuietZone]) -> Option<f64> {
    quiet
        .iter()
        .filter(|z| !z.contains(pct))
        .min_by(|a, b| forward(pct, a.start).total_cmp(&forward(pct, b.start)))
        .map(|z| z.start)
}

/// Where a cue ended up after [`fit_positions`].
#[derive(Debug, Clone, PartialEq)]
pub struct Fit {
    pub id: String,
    /// Speaking time the sentence needs, margin included, s.
    pub needed_s: f64,
    /// Time from the moment the cue can start (its position, or the end of the
    /// cue before it on the same straight) to the next quiet zone, s.
    pub room_s: f64,
    /// Length of the whole straight the cue is spoken on, s.
    pub straight_s: f64,
    /// Other cues spoken on the same straight.
    pub shared_with: Vec<String>,
    /// Distance the cue was moved earlier, m (0: unchanged).
    pub moved_m: f64,
}

impl Fit {
    pub fn fits(&self) -> bool {
        self.room_s >= self.needed_s
    }

    /// One line for logs and for Claude when the cue does not fit.
    pub fn problem(&self) -> Option<String> {
        if self.fits() {
            return None;
        }
        let shared = if self.shared_with.is_empty() {
            String::new()
        } else {
            format!(", shared with {}", self.shared_with.join(", "))
        };
        Some(format!(
            "cue \"{}\" needs {:.1} s of speech but only {:.1} s are free before the next braking \
             zone (straight of {:.1} s{shared}): shorten it to about {} characters or drop a cue",
            self.id,
            self.needed_s,
            self.room_s.max(0.0),
            self.straight_s,
            max_chars(self.room_s)
        ))
    }
}

/// Checks where `triggers` would be spoken on the reference lap, with the
/// estimated speech durations (for callers without the speech engine).
pub fn check_fit(triggers: &[Trigger], analyzer: &Analyzer, settings: &PlanSettings) -> Vec<Fit> {
    let Some(profile) = TimeProfile::from_analyzer(analyzer) else {
        return vec![];
    };
    let ctx = TrackContext {
        model: &analyzer.model,
        reference: &analyzer.reference_metrics,
    };
    let quiet = ctx.quiet_zones(settings);
    let mut t = triggers.to_vec();
    fit_positions(
        &mut t,
        &quiet,
        &profile,
        analyzer.model.length_m,
        &mut |s| estimate_speech_s(s),
    )
}

/// Speaking time on the straight before each corner's braking zone, s, at
/// reference pace (`None` for a corner the reference takes without braking).
pub fn speaking_windows(
    analyzer: &Analyzer,
    settings: &PlanSettings,
) -> Vec<(String, Option<f64>)> {
    let ctx = TrackContext {
        model: &analyzer.model,
        reference: &analyzer.reference_metrics,
    };
    let quiet = ctx.quiet_zones(settings);
    let profile = TimeProfile::from_analyzer(analyzer);
    let lead = settings.quiet_lead_m.unwrap_or(30.0) / analyzer.model.length_m;
    analyzer
        .model
        .corners
        .iter()
        .zip(&analyzer.reference_metrics)
        .map(|(c, m)| {
            let w = match (&profile, m.brake_point_pct) {
                (Some(p), Some(brake)) => {
                    let start = (brake - lead).rem_euclid(1.0);
                    previous_quiet_end(start - 1e-6, &quiet).map(|from| p.time_between(from, start))
                }
                _ => None,
            };
            (c.label(), w)
        })
        .collect()
}

/// Longest sentence (characters) that can be spoken in `seconds`.
pub fn max_chars(seconds: f64) -> usize {
    ((seconds - FIT_MARGIN_S - 0.35).max(0.0) * 16.0).floor() as usize
}

/// Places the cues so that they can all be spoken. The cues spoken on the
/// same straight (between two quiet zones) are packed back to back so that the
/// last one ends before the braking zone: a cue that would start too late is
/// moved earlier on its straight. A cue inside a quiet zone is spoken when the
/// zone ends, on the next straight. When a straight is too short for its cues,
/// the cues are spread from its start and the [`Fit`] of the ones that do not
/// fit reports the shortfall.
pub fn fit_positions(
    triggers: &mut [Trigger],
    quiet: &[QuietZone],
    profile: &TimeProfile,
    length_m: f64,
    duration: &mut dyn FnMut(&str) -> f64,
) -> Vec<Fit> {
    struct Item {
        idx: usize,
        /// Straight: (start, end) in pct.
        straight: (f64, f64),
        /// Time from the start of the straight to the cue's position, s.
        at_s: f64,
        needed_s: f64,
    }
    let mut items = vec![];
    let mut fits: Vec<Option<Fit>> = vec![None; triggers.len()];
    for (idx, t) in triggers.iter().enumerate() {
        if !t.at_pct.is_finite() {
            continue;
        }
        let needed_s = duration(&t.text) + FIT_MARGIN_S;
        // Inside a quiet zone, the cue waits for the end of the zone.
        let inside = quiet.iter().find(|z| z.contains(t.at_pct));
        let from = inside.map(|z| z.end).unwrap_or(t.at_pct);
        match (
            inside
                .map(|z| z.end)
                .or_else(|| previous_quiet_end(from, quiet)),
            next_quiet_start(from, quiet),
        ) {
            (Some(start), Some(end)) => items.push(Item {
                idx,
                straight: (start, end),
                at_s: profile.time_between(start, from),
                needed_s,
            }),
            // No quiet zone: always time to speak.
            _ => {
                fits[idx] = Some(Fit {
                    id: t.id.clone(),
                    needed_s,
                    room_s: f64::INFINITY,
                    straight_s: f64::INFINITY,
                    shared_with: vec![],
                    moved_m: 0.0,
                })
            }
        }
    }
    // Group by straight, in driving order.
    items.sort_by(|a, b| {
        a.straight
            .1
            .total_cmp(&b.straight.1)
            .then(a.at_s.total_cmp(&b.at_s))
    });
    let mut i = 0;
    while i < items.len() {
        let mut j = i;
        while j < items.len() && (items[j].straight.1 - items[i].straight.1).abs() < 1e-9 {
            j += 1;
        }
        let group = &items[i..j];
        let (start, end) = group[0].straight;
        let straight_s = profile.time_between(start, end);
        // Backward pass: latest start of each cue so that all end in time.
        let mut latest = vec![0.0; group.len()];
        let mut deadline = straight_s;
        for k in (0..group.len()).rev() {
            latest[k] = deadline - group[k].needed_s;
            deadline = latest[k].min(group[k].at_s);
        }
        // Forward pass: actual start times after moving late cues earlier.
        let mut t = 0.0f64;
        for (k, it) in group.iter().enumerate() {
            let wanted = it.at_s.min(latest[k]).max(0.0);
            let begin = wanted.max(t);
            let trig = &mut triggers[it.idx];
            let mut moved_m = 0.0;
            if wanted < it.at_s - 1e-6 {
                // New position: time `wanted` after the start of the straight.
                let mut lo = 0.0;
                let mut hi = forward(start, end);
                for _ in 0..40 {
                    let mid = (lo + hi) / 2.0;
                    if profile.time_between(start, start + mid) < wanted {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                let new_pct = (start + lo).rem_euclid(1.0);
                moved_m =
                    forward(new_pct, trig.at_pct).min(forward(trig.at_pct, new_pct)) * length_m;
                trig.at_pct = new_pct;
            }
            fits[it.idx] = Some(Fit {
                id: trig.id.clone(),
                needed_s: it.needed_s,
                room_s: straight_s - begin,
                straight_s,
                shared_with: group
                    .iter()
                    .filter(|o| o.idx != it.idx)
                    .map(|o| triggers[o.idx].id.clone())
                    .collect(),
                moved_m,
            });
            t = begin + it.needed_s;
        }
        i = j;
    }
    fits.into_iter().flatten().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{Plan, TrackContext, resolve};
    use telemetry::synthetic::SyntheticTrack;
    use telemetry::{CornerDetectionParams, Lap, LapFlags};

    fn analyzer() -> Analyzer {
        let track = SyntheticTrack::demo();
        let samples = track.simulate_lap(1.0, 0.0, 0.0);
        let lap = Lap {
            number: 1,
            start_time: 0.0,
            lap_time: samples.len() as f64 / 60.0,
            flags: LapFlags {
                complete: true,
                ..Default::default()
            },
            samples,
        };
        Analyzer::new(
            &lap,
            Some(track.length_m()),
            &CornerDetectionParams::default(),
        )
    }

    #[test]
    fn time_between_wraps_the_line() {
        let a = analyzer();
        let p = TimeProfile::from_analyzer(&a).unwrap();
        let lap = a.reference.lap_time;
        let whole = p.time_between(0.9, 0.1) + p.time_between(0.1, 0.9);
        assert!((whole - lap).abs() < 1e-6, "{whole} vs {lap}");
        assert!(p.time_between(0.2, 0.3) > 0.0);
    }

    #[test]
    fn long_cue_moves_earlier_on_its_straight() {
        let a = analyzer();
        let ctx = TrackContext {
            model: &a.model,
            reference: &a.reference_metrics,
        };
        let quiet = ctx.quiet_zones(&Default::default());
        assert!(!quiet.is_empty());
        let p = TimeProfile::from_analyzer(&a).unwrap();
        // A cue 40 m before the braking point of the first braking corner:
        // inside the quiet lead or just before it, no room at all.
        let label = a
            .model
            .corners
            .iter()
            .zip(&a.reference_metrics)
            .find(|(_, m)| m.brake_point_pct.is_some())
            .map(|(c, _)| c.label())
            .unwrap();
        let plan = Plan::parse(&format!(
            "[[trigger]]\ncorner = \"{label}\"\noffset_m = -40\ntext = \"A short cue.\"\n"
        ))
        .unwrap();
        let mut t = resolve(&plan, Some(&ctx)).unwrap();
        let before = t[0].at_pct;
        let fits = fit_positions(&mut t, &quiet, &p, a.model.length_m, &mut |s| {
            estimate_speech_s(s)
        });
        assert!(fits[0].moved_m > 0.0, "{fits:?}");
        assert!(t[0].at_pct != before);
        assert!(fits[0].fits(), "{fits:?}");
        assert!(p.time_to_quiet(t[0].at_pct, &quiet) >= fits[0].needed_s);
    }

    #[test]
    fn cues_on_one_straight_are_packed_back_to_back() {
        let a = analyzer();
        let ctx = TrackContext {
            model: &a.model,
            reference: &a.reference_metrics,
        };
        let quiet = ctx.quiet_zones(&Default::default());
        let p = TimeProfile::from_analyzer(&a).unwrap();
        // The longest straight, and two cues near its end.
        let (z, room) = quiet
            .iter()
            .map(|z| (z, p.time_to_quiet(z.end + 1e-4, &quiet)))
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .unwrap();
        assert!(room > 6.0, "{room}");
        let len = a.model.length_m;
        let near_end = |m: f64| {
            // `m` meters before the end of the straight.
            let end = quiet
                .iter()
                .map(|q| q.start)
                .min_by(|x, y| forward(z.end, *x).total_cmp(&forward(z.end, *y)))
                .unwrap();
            (end - m / len).rem_euclid(1.0)
        };
        let plan = Plan::parse(&format!(
            "[[trigger]]\nid = \"a\"\nposition = {}\ntext = \"First cue, long enough to matter.\"\n\
             [[trigger]]\nid = \"b\"\nposition = {}\ntext = \"Second cue.\"\n",
            near_end(60.0),
            near_end(30.0)
        ))
        .unwrap();
        let mut t = resolve(&plan, Some(&ctx)).unwrap();
        let fits = fit_positions(&mut t, &quiet, &p, len, &mut |s| estimate_speech_s(s));
        assert!(fits.iter().all(|f| f.fits()), "{fits:?}");
        assert!(fits.iter().all(|f| f.moved_m > 0.0), "{fits:?}");
        // "a" ends before "b" starts, "b" ends before the quiet zone.
        let (ta, tb) = (t[0].at_pct, t[1].at_pct);
        assert!(p.time_between(ta, tb) >= fits[0].needed_s - 1e-3);
        assert!(p.time_to_quiet(tb, &quiet) >= fits[1].needed_s - 1e-3);
        assert_eq!(fits[0].shared_with, vec!["b".to_string()]);
    }

    #[test]
    fn too_much_on_a_short_straight_is_reported() {
        let a = analyzer();
        let ctx = TrackContext {
            model: &a.model,
            reference: &a.reference_metrics,
        };
        let quiet = ctx.quiet_zones(&Default::default());
        let p = TimeProfile::from_analyzer(&a).unwrap();
        let at = (quiet[0].start - 0.001).rem_euclid(1.0);
        // About 30 s of speech: longer than any straight of the demo track.
        let long = "far too long ".repeat(36);
        let plan = Plan::parse(&format!(
            "[[trigger]]\nid = \"x\"\nposition = {at}\ntext = \"{}\"\n",
            long.trim()
        ))
        .unwrap();
        let mut t = resolve(&plan, Some(&ctx)).unwrap();
        let fits = fit_positions(&mut t, &quiet, &p, a.model.length_m, &mut |s| {
            estimate_speech_s(s)
        });
        assert!(!fits[0].fits());
        let msg = fits[0].problem().unwrap();
        assert!(msg.contains("shorten it"), "{msg}");
    }

    #[test]
    fn cue_that_fits_is_not_moved() {
        let a = analyzer();
        let ctx = TrackContext {
            model: &a.model,
            reference: &a.reference_metrics,
        };
        let quiet = ctx.quiet_zones(&Default::default());
        let p = TimeProfile::from_analyzer(&a).unwrap();
        // Find a point with plenty of room.
        let at = (0..1000)
            .map(|i| i as f64 / 1000.0)
            .find(|&x| p.time_to_quiet(x, &quiet) > 6.0)
            .unwrap();
        let plan = Plan::parse(&format!(
            "[[trigger]]\nposition = {at}\ntext = \"Short.\"\n"
        ))
        .unwrap();
        let mut t = resolve(&plan, Some(&ctx)).unwrap();
        let fits = fit_positions(&mut t, &quiet, &p, a.model.length_m, &mut |s| {
            estimate_speech_s(s)
        });
        assert_eq!(fits[0].moved_m, 0.0);
        assert_eq!(t[0].at_pct, at);
    }
}
