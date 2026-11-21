//! Checks the per-pass analysis of the turn-by-turn coaching on a recorded
//! session: every pass through the chosen corners is compared to the
//! reference, and set against the same corners of the full laps.
//!
//! ```text
//! cargo run -p analysis --example focus_passes -- <session.ibt> <reference.csv|.ibt> [T5 [T6]]
//! ```
//!
//! Without corners, the most costly corner of the best valid lap is used.

use analysis::{Analyzer, PassCapture};
use telemetry::{CornerDetectionParams, IbtFile, LapEvent, LapRecorder};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(session), Some(reference)) = (args.first(), args.get(1)) else {
        return Err("focus_passes <session.ibt> <reference.csv|.ibt> [T5 [T6]]".into());
    };
    let ibt = IbtFile::open(session)?;
    let r = reference::load(reference)?;
    let length = ibt.session.track_length_m.or(r.track_length_m);
    let a = Analyzer::new(&r.lap, length, &CornerDetectionParams::default());

    let mut laps = LapRecorder::default();
    let mut valid = vec![];
    for s in ibt.samples() {
        if let Some(LapEvent::Completed(l)) = laps.push(s)
            && l.is_valid()
        {
            valid.push(l);
        }
    }
    println!("{} valid laps", valid.len());
    let corners: Vec<usize> = if args.len() > 2 {
        args[2..]
            .iter()
            .filter_map(|l| {
                a.model
                    .corners
                    .iter()
                    .position(|c| c.label().eq_ignore_ascii_case(l))
            })
            .collect()
    } else {
        let best = valid
            .iter()
            .min_by(|x, y| x.lap_time.total_cmp(&y.lap_time))
            .ok_or("no valid lap")?;
        let cmp = a.compare(best);
        let top = cmp.ranked()[0].label();
        vec![
            a.model
                .corners
                .iter()
                .position(|c| c.label() == top)
                .unwrap(),
        ]
    };
    let labels: Vec<String> = corners
        .iter()
        .map(|&i| a.model.corners[i].label())
        .collect();
    let window = a.window_for(&corners).ok_or("no corner")?;
    println!(
        "focus {} window {:.3} → {:.3}",
        labels.join(", "),
        window.0,
        window.1
    );

    // Full laps: loss over the focus corners.
    let mut full = std::collections::HashMap::new();
    for l in &valid {
        let cmp = a.compare(l);
        let loss: f64 = corners.iter().map(|&i| cmp.corners[i].time_delta).sum();
        full.insert(l.number, loss);
    }

    let mut capture = PassCapture::new(window);
    println!("  lap   pass loss   full-lap loss   off track");
    for s in ibt.samples() {
        let Some(pass) = capture.push(&s) else {
            continue;
        };
        let Some(cmp) = a.compare_pass(&pass) else {
            println!("  {:>3}   not covered", pass.lap);
            continue;
        };
        let loss: f64 = corners.iter().map(|&i| cmp.corners[i].time_delta).sum();
        // The pass ends on the lap where the window ends; a window crossing
        // the line belongs to the lap that ends there.
        let lap_of_window = if window.0 > window.1 {
            pass.lap - 1
        } else {
            pass.lap
        };
        let f = full
            .get(&lap_of_window)
            .map(|x| format!("{x:+.3}"))
            .unwrap_or_else(|| "(invalid lap)".into());
        println!(
            "  {:>3}   {:+.3}       {:>13}   {}",
            pass.lap,
            loss,
            f,
            if pass.off_track { "yes" } else { "" }
        );
    }
    Ok(())
}
