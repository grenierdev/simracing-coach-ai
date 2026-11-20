//! Runs the cue engine over a recorded session, without audio, and reports
//! what happens to each cue: spoken in full, cut off, or dropped.
//!
//! ```text
//! cargo run -p triggers --example cue_replay -- <session.ibt> <reference.csv|.ibt> [plan.toml] [--baseline]
//! ```
//!
//! `--baseline`: without the reference timing and without moving cues that
//! do not fit (the behavior of v0.4.0).
//!
//! Without a plan, one cue per braking corner is placed the way the race
//! engineer places them: 300 m before the reference braking point, about 50
//! characters long. Speech durations use the same estimate as the speaker
//! (0.35 s + 1/16 s per character).

use std::collections::HashMap;

use analysis::Analyzer;
use telemetry::{CornerDetectionParams, IbtFile};
use triggers::engine::{Command, TickContext, TriggerEngine};
use triggers::plan::{Plan, TrackContext, resolve};
use triggers::{TimeProfile, estimate_speech_s as estimate_duration, fit_positions};

#[derive(Default, Debug)]
struct Outcome {
    spoken: u32,
    cut: u32,
}

fn main() -> anyhow::Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let baseline = args.iter().any(|a| a == "--baseline");
    args.retain(|a| a != "--baseline");
    let (Some(session), Some(reference)) = (args.first(), args.get(1)) else {
        anyhow::bail!("cue_replay <session.ibt> <reference.csv|.ibt> [plan.toml]");
    };
    let ibt = IbtFile::open(session)?;
    let r = reference::load(reference)?;
    let length = ibt.session.track_length_m.or(r.track_length_m);
    let analyzer = Analyzer::new(&r.lap, length, &CornerDetectionParams::default());
    let ctx = TrackContext {
        model: &analyzer.model,
        reference: &analyzer.reference_metrics,
    };
    let plan = match args.get(2) {
        Some(p) => Plan::load(p)?,
        None => {
            let mut text = String::new();
            for (c, m) in analyzer
                .model
                .corners
                .iter()
                .zip(&analyzer.reference_metrics)
            {
                if m.brake_point_pct.is_some() {
                    text += &format!(
                        "[[trigger]]\nid = \"auto-{l}\"\ncorner = \"{l}\"\nanchor = \"braking\"\n\
                         offset_m = -300\ntext = \"{l}: brake ten meters later, then release slowly.\"\n\n",
                        l = c.label()
                    );
                }
            }
            Plan::parse(&text)?
        }
    };
    let mut triggers = resolve(&plan, Some(&ctx))?;
    let quiet = ctx.quiet_zones(&plan.settings);
    let profile = TimeProfile::from_analyzer(&analyzer);
    let mut moved = std::collections::HashMap::new();
    if let (Some(p), false) = (&profile, baseline) {
        for f in fit_positions(
            &mut triggers,
            &quiet,
            p,
            analyzer.model.length_m,
            &mut |t| estimate_duration(t),
        ) {
            if let Some(problem) = f.problem() {
                println!("  ! {problem}");
            }
            moved.insert(f.id.clone(), f.moved_m);
        }
    }
    println!(
        "track {:.0} m, {} corners, {} quiet zones, {} cues",
        analyzer.model.length_m,
        analyzer.model.corners.len(),
        quiet.len(),
        triggers.len()
    );
    for t in &triggers {
        let fit = profile.as_ref().map(|p| p.time_to_quiet(t.at_pct, &quiet));
        println!(
            "  {:<10} at {:.3}  needs {:.1} s  room before quiet zone {}  moved {:.0} m earlier",
            t.id,
            t.at_pct,
            estimate_duration(&t.text) + 0.4,
            fit.map(|f| format!("{f:.1} s")).unwrap_or("?".into()),
            moved.get(&t.id).copied().unwrap_or(0.0)
        );
    }

    let mut engine = TriggerEngine::new(triggers, quiet);
    if let (Some(p), false) = (profile, baseline) {
        engine.set_time_profile(p);
    }
    let mut outcomes: HashMap<String, Outcome> = HashMap::new();
    let mut speaking: Option<(String, f64)> = None;
    let mut laps = std::collections::BTreeSet::new();
    for s in ibt.samples() {
        let now = s.session_time;
        if let Some((_, until)) = &speaking
            && now >= *until
        {
            speaking = None;
        }
        if s.is_on_track {
            laps.insert(s.lap);
        }
        let tick = TickContext {
            live_delta: None,
            speaking: speaking.is_some(),
            track_length_m: analyzer.model.length_m,
        };
        for c in engine.tick(&s, &tick, &mut |t| estimate_duration(t)) {
            match c {
                Command::Say {
                    trigger_id, text, ..
                } => {
                    outcomes.entry(trigger_id.clone()).or_default().spoken += 1;
                    speaking = Some((trigger_id, now + estimate_duration(&text)));
                }
                Command::Stop { .. } => {
                    if let Some((id, _)) = speaking.take() {
                        outcomes.entry(id).or_default().cut += 1;
                    }
                }
            }
        }
    }
    println!("{} laps on track", laps.len());
    println!("  cue         spoken  cut  dropped");
    let (mut said, mut cut, mut dropped) = (0, 0, 0);
    for v in engine.views() {
        let o = outcomes.remove(&v.trigger.id).unwrap_or_default();
        println!(
            "  {:<10} {:>6} {:>4} {:>8}",
            v.trigger.id, o.spoken, o.cut, v.dropped
        );
        said += o.spoken;
        cut += o.cut;
        dropped += v.dropped;
    }
    println!(
        "total: {said} started, {cut} cut off, {dropped} dropped -> {} heard in full",
        said - cut
    );
    Ok(())
}
