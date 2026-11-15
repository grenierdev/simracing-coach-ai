//! Sources de télémétrie : live (mémoire partagée iRacing) ou rejeu d'un .ibt.

use std::time::{Duration, Instant};

use thiserror::Error;

use crate::ibt::IbtFile;
use crate::sample::Sample;
use crate::session::SessionInfo;

#[derive(Debug, Clone, PartialEq)]
pub enum SourceEvent {
    /// Connexion établie (ou début du rejeu).
    Connected(SessionInfo),
    /// Le YAML de session a changé (nouvelle session, changement de voiture...).
    SessionInfoChanged(SessionInfo),
    Sample(Sample),
    /// iRacing fermé ou plus de données.
    Disconnected,
    /// Fin du fichier de rejeu.
    Ended,
}

#[derive(Debug, Error)]
pub enum SourceError {
    #[error("{0}")]
    Ibt(#[from] crate::ibt::IbtError),
    #[error("{0}")]
    Format(#[from] crate::irsdk::FormatError),
    #[error("source live indisponible : {0}")]
    Unavailable(String),
}

pub trait TelemetrySource: Send {
    /// Attend au plus `timeout` le prochain événement. `Ok(None)` : rien de neuf.
    fn next_event(&mut self, timeout: Duration) -> Result<Option<SourceEvent>, SourceError>;
    fn describe(&self) -> String;
}

/// Rejoue un .ibt comme s'il était live, au rythme `tickRate × speed`.
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
    /// `speed` : 1.0 = temps réel ; `f64::INFINITY` = aussi vite que possible.
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
                // Un nouveau passage est une nouvelle « connexion » : l'historique
                // de tours n'a pas de sens à travers la boucle.
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
            format!("rejeu {} (×{})", self.label, self.speed)
        } else {
            format!("rejeu {}", self.label)
        }
    }
}
