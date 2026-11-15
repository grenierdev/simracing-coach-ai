//! De bout en bout : session synthétique écrite en .ibt, relue, rejouée dans le hub.

use std::time::{Duration, Instant};

use telemetry::synthetic::SyntheticTrack;
use telemetry::{
    ConnectionState, IbtFile, ReplaySource, TelemetryEvent, TelemetryHub, TelemetryHubConfig,
};

fn synthetic_file(laps: usize) -> (SyntheticTrack, IbtFile) {
    let track = SyntheticTrack::demo();
    let bytes = track.session_ibt(laps, 7).to_bytes();
    (
        track,
        IbtFile::from_bytes(bytes).expect("ibt synthétique lisible"),
    )
}

#[test]
fn ibt_roundtrip_preserves_header_and_session() {
    let (track, file) = synthetic_file(2);
    assert_eq!(file.tick_rate(), 60.0);
    assert!(file.len() > 60 * 60);
    assert_eq!(file.session.track_display_name, "Circuit synthétique");
    assert_eq!(file.session.car_screen_name, "Voiture synthétique");
    let len = file.session.track_length_m.unwrap();
    assert!((len - track.length_m()).abs() < 10.0);
    let first = file.sample(0).unwrap();
    assert!(first.lap_dist_pct >= 0.7);
    assert!(first.lat.is_finite() && first.yaw_rate.is_finite());
    assert!(file.vars.get("IsOnTrackCar").is_some());
}

#[test]
fn replay_through_hub_records_laps_and_corners() {
    let (track, file) = synthetic_file(3);
    let source = ReplaySource::new(file, "synthétique", f64::INFINITY, false);
    let hub = TelemetryHub::spawn(Box::new(source), TelemetryHubConfig::default());
    let samples = hub.subscribe_samples();

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut laps = vec![];
    let mut corners = None;
    let mut ended = false;
    let mut n_samples = 0usize;
    while !ended && Instant::now() < deadline {
        while samples.try_recv().is_ok() {
            n_samples += 1;
        }
        match hub.events().recv_timeout(Duration::from_millis(100)) {
            Ok(TelemetryEvent::LapCompleted(l)) => laps.push(l),
            Ok(TelemetryEvent::Corners(c)) => corners = Some(c),
            Ok(TelemetryEvent::Connection(ConnectionState::Ended)) => ended = true,
            _ => {}
        }
    }
    assert!(ended, "le rejeu doit se terminer");
    assert!(n_samples > 0);
    // Tour de sortie partiel + 3 tours ; le dernier tour se termine au dernier
    // échantillon sans repasser la ligne, il reste donc en cours.
    assert_eq!(
        laps.len(),
        3,
        "{:?}",
        laps.iter().map(|l| l.flags).collect::<Vec<_>>()
    );
    assert!(!laps[0].flags.complete);
    assert!(laps[1].is_valid() && laps[2].is_valid());
    for l in &laps[1..] {
        assert!((45.0..90.0).contains(&l.lap_time), "temps {}", l.lap_time);
    }
    let corners = corners.expect("virages détectés");
    assert_eq!(corners.len(), track.expected_corners().len());

    let snap = hub.snapshot();
    assert_eq!(snap.laps.len(), 3);
    assert!(snap.best_lap.is_some());
    assert_eq!(snap.corners.len(), corners.len());
}
