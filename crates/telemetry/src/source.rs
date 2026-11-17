//! Telemetry sources: live (iRacing shared memory) or replay of an .ibt.

use std::time::{Duration, Instant};

use thiserror::Error;

use crate::ibt::IbtFile;
use crate::sample::Sample;
use crate::session::SessionInfo;

#[derive(Debug, Clone, PartialEq)]
pub enum SourceEvent {
    /// Connection established (or replay started).
    Connected(SessionInfo),
    /// The session YAML changed (new session, car change...).
    SessionInfoChanged(SessionInfo),
    Sample(Sample),
    /// iRacing closed or no more data.
    Disconnected,
    /// End of the replay file.
    Ended,
}

#[derive(Debug, Error)]
pub enum SourceError {
    #[error("{0}")]
    Ibt(#[from] crate::ibt::IbtError),
    #[error("{0}")]
    Format(#[from] crate::irsdk::FormatError),
    #[error("live source unavailable: {0}")]
    Unavailable(String),
}

pub trait TelemetrySource: Send {
    /// Waits at most `timeout` for the next event. `Ok(None)`: nothing new.
    fn next_event(&mut self, timeout: Duration) -> Result<Option<SourceEvent>, SourceError>;
    fn describe(&self) -> String;
    /// The source is the running game (not a replay).
    fn is_live(&self) -> bool {
        false
    }
}

/// Replays an .ibt as if it were live, at the rate `tickRate × speed`.
pub struct ReplaySource {
    file: IbtFile,
    label: String,
    speed: f64,
    looping: bool,
    index: usize,
    started: Option<Instant>,
    sent_connected: bool,
    ended: bool,
}

impl ReplaySource {
    /// `speed`: 1.0 = real time; `f64::INFINITY` = as fast as possible.
    pub fn new(file: IbtFile, label: impl Into<String>, speed: f64, looping: bool) -> Self {
        ReplaySource {
            file,
            label: label.into(),
            speed: if speed > 0.0 { speed } else { 1.0 },
            looping,
            index: 0,
            started: None,
            sent_connected: false,
            ended: false,
        }
    }

    pub fn file(&self) -> &IbtFile {
        &self.file
    }
}

impl TelemetrySource for ReplaySource {
    fn next_event(&mut self, timeout: Duration) -> Result<Option<SourceEvent>, SourceError> {
        if !self.sent_connected {
            self.sent_connected = true;
            return Ok(Some(SourceEvent::Connected(self.file.session.clone())));
        }
        if self.ended {
            std::thread::sleep(timeout);
            return Ok(None);
        }
        if self.index >= self.file.len() {
            if self.looping && !self.file.is_empty() {
                self.index = 0;
                self.started = None;
                // A new pass is a new "connection": the lap history makes no
                // sense across the loop.
                return Ok(Some(SourceEvent::Connected(self.file.session.clone())));
            }
            self.ended = true;
            return Ok(Some(SourceEvent::Ended));
        }
        if self.speed.is_finite() {
            let started = *self.started.get_or_insert_with(Instant::now);
            let due = started
                + Duration::from_secs_f64(self.index as f64 / (self.file.tick_rate() * self.speed));
            let now = Instant::now();
            if due > now {
                let wait = due - now;
                if wait > timeout {
                    std::thread::sleep(timeout);
                    return Ok(None);
                }
                std::thread::sleep(wait);
            }
        }
        let s = self.file.sample(self.index);
        self.index += 1;
        Ok(s.map(SourceEvent::Sample))
    }

    fn describe(&self) -> String {
        if self.speed.is_finite() && self.speed != 1.0 {
            format!("replay {} (×{})", self.label, self.speed)
        } else {
            format!("replay {}", self.label)
        }
    }
}
