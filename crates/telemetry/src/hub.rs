//! Fast telemetry loop: a dedicated thread reads the source at 60 Hz, splits
//! laps, detects corners and publishes a snapshot for the UI.
//!
//! Real-time consumers (trigger engine, milestone 3) subscribe to samples via
//! [`TelemetryHub::subscribe_samples`]; the UI reads [`LiveSnapshot`] at its
//! own pace without ever blocking the loop.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, unbounded};

use crate::corners::{Corner, CornerDetectionParams, detect_corners};
use crate::laps::{Lap, LapEvent, LapFlags, LapRecorder, LapRecorderConfig};
use crate::sample::Sample;
use crate::session::SessionInfo;
use crate::source::{SourceEvent, TelemetrySource};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ConnectionState {
    #[default]
    Waiting,
    Connected,
    Ended,
    Error(String),
}

#[derive(Debug, Clone)]
pub enum TelemetryEvent {
    Connection(ConnectionState),
    Session(SessionInfo),
    LapCompleted(Arc<Lap>),
    Corners(Arc<Vec<Corner>>),
}

/// Current state, copied by the UI.
#[derive(Debug, Clone, Default)]
pub struct LiveSnapshot {
    pub source: String,
    pub state: ConnectionState,
    pub session: Option<SessionInfo>,
    pub sample: Option<Sample>,
    pub current_lap_elapsed: Option<f64>,
    pub current_lap_flags: Option<LapFlags>,
    /// Completed laps, oldest to most recent.
    pub laps: Vec<Arc<Lap>>,
    pub best_lap: Option<Arc<Lap>>,
    pub corners: Arc<Vec<Corner>>,
    /// Measured sample rate, Hz.
    pub sample_rate_hz: f64,
    pub samples_received: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TelemetryHubConfig {
    pub laps: LapRecorderConfig,
    pub corners: CornerDetectionParams,
}

/// Shared access to the snapshot, usable from other threads.
#[derive(Clone)]
pub struct SnapshotHandle(Arc<Mutex<LiveSnapshot>>);

impl SnapshotHandle {
    pub fn get(&self) -> LiveSnapshot {
        self.0.lock().unwrap().clone()
    }
}

pub struct TelemetryHub {
    snapshot: Arc<Mutex<LiveSnapshot>>,
    events: Receiver<TelemetryEvent>,
    sample_subs: Arc<Mutex<Vec<Sender<Sample>>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl TelemetryHub {
    pub fn spawn(source: Box<dyn TelemetrySource>, cfg: TelemetryHubConfig) -> Self {
        let snapshot = Arc::new(Mutex::new(LiveSnapshot {
            source: source.describe(),
            ..Default::default()
        }));
        let (tx, rx) = unbounded();
        let sample_subs: Arc<Mutex<Vec<Sender<Sample>>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let snapshot = snapshot.clone();
            let subs = sample_subs.clone();
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("telemetry".into())
                .spawn(move || run(source, cfg, snapshot, tx, subs, stop))
                .expect("starting the telemetry thread")
        };
        TelemetryHub {
            snapshot,
            events: rx,
            sample_subs,
            stop,
            thread: Some(thread),
        }
    }

    pub fn snapshot(&self) -> LiveSnapshot {
        self.snapshot.lock().unwrap().clone()
    }

    pub fn snapshot_handle(&self) -> SnapshotHandle {
        SnapshotHandle(self.snapshot.clone())
    }

    pub fn events(&self) -> &Receiver<TelemetryEvent> {
        &self.events
    }

    /// Stream of all samples. A subscriber that is too slow loses samples
    /// rather than slowing down the loop (queue bounded at 2 s).
    pub fn subscribe_samples(&self) -> Receiver<Sample> {
        let (tx, rx) = bounded(120);
        self.sample_subs.lock().unwrap().push(tx);
        rx
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for TelemetryHub {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(
    mut source: Box<dyn TelemetrySource>,
    cfg: TelemetryHubConfig,
    snapshot: Arc<Mutex<LiveSnapshot>>,
    events: Sender<TelemetryEvent>,
    subs: Arc<Mutex<Vec<Sender<Sample>>>>,
    stop: Arc<AtomicBool>,
) {
    let mut rec = LapRecorder::new(cfg.laps);
    let mut corners_from: Option<f64> = None; // lap time of the lap used for corners
    let mut rate_window: VecDeque<Instant> = VecDeque::with_capacity(128);
    let publish_every = Duration::from_millis(16);
    let mut last_publish = Instant::now();
    let mut received: u64 = 0;

    let set_state = |state: ConnectionState| {
        snapshot.lock().unwrap().state = state.clone();
        let _ = events.send(TelemetryEvent::Connection(state));
    };

    while !stop.load(Ordering::Relaxed) {
        let ev = match source.next_event(Duration::from_millis(50)) {
            Ok(Some(ev)) => ev,
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!("telemetry: {e}");
                set_state(ConnectionState::Error(e.to_string()));
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        match ev {
            SourceEvent::Connected(info) => {
                rec.reset();
                corners_from = None;
                {
                    let mut s = snapshot.lock().unwrap();
                    s.laps.clear();
                    s.best_lap = None;
                    s.corners = Arc::default();
                    s.session = Some(info.clone());
                    s.sample = None;
                }
                set_state(ConnectionState::Connected);
                let _ = events.send(TelemetryEvent::Session(info));
            }
            SourceEvent::SessionInfoChanged(info) => {
                snapshot.lock().unwrap().session = Some(info.clone());
                let _ = events.send(TelemetryEvent::Session(info));
            }
            SourceEvent::Disconnected => set_state(ConnectionState::Waiting),
            SourceEvent::Ended => set_state(ConnectionState::Ended),
            SourceEvent::Sample(sample) => {
                received += 1;
                let now = Instant::now();
                rate_window.push_back(now);
                while rate_window
                    .front()
                    .is_some_and(|t| now.duration_since(*t) > Duration::from_secs(1))
                {
                    rate_window.pop_front();
                }
                subs.lock().unwrap().retain(|tx| {
                    !matches!(tx.try_send(sample), Err(TrySendError::Disconnected(_)))
                });

                if let Some(LapEvent::Completed(lap)) = rec.push(sample) {
                    let _ = events.send(TelemetryEvent::LapCompleted(lap.clone()));
                    // Corners: detected on the best valid lap, recomputed if it improves.
                    let best = rec.best_lap().cloned();
                    let mut new_corners = None;
                    if let Some(best) = &best
                        && corners_from.is_none_or(|t| best.lap_time < t)
                    {
                        let cs = Arc::new(detect_corners(best, &cfg.corners));
                        corners_from = Some(best.lap_time);
                        let _ = events.send(TelemetryEvent::Corners(cs.clone()));
                        new_corners = Some(cs);
                    }
                    let mut s = snapshot.lock().unwrap();
                    s.laps = rec.history().cloned().collect();
                    s.best_lap = best;
                    if let Some(cs) = new_corners {
                        s.corners = cs;
                    }
                }
                if now.duration_since(last_publish) >= publish_every {
                    last_publish = now;
                    let mut s = snapshot.lock().unwrap();
                    s.sample = Some(sample);
                    s.current_lap_elapsed = rec.current_lap_elapsed();
                    s.current_lap_flags = rec.current_lap_flags();
                    s.sample_rate_hz = rate_window.len() as f64;
                    s.samples_received = received;
                }
            }
        }
    }
}
