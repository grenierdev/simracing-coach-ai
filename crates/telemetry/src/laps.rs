//! Découpage du flux d'échantillons en tours et historique des N derniers tours.

use std::collections::VecDeque;
use std::sync::Arc;

use crate::sample::{Sample, TrackSurface};

/// Raisons pour lesquelles un tour n'est pas exploitable comme comparaison.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LapFlags {
    /// Le tour a commencé et fini sur la ligne (pas un tour de sortie partiel).
    pub complete: bool,
    /// Passage par la voie des stands.
    pub pit: bool,
    /// Nombre de sorties de piste (passages à `OffTrack`).
    pub off_track: u32,
    /// Téléportation, reset, recul, pause ou trou dans les données.
    pub discontinuity: bool,
}

impl LapFlags {
    pub fn is_valid(&self) -> bool {
        self.complete && !self.pit && self.off_track == 0 && !self.discontinuity
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Lap {
    /// Numéro iRacing (`Lap`).
    pub number: i32,
    /// Temps de session au passage de la ligne (interpolé).
    pub start_time: f64,
    /// Temps au tour, s (interpolé entre les deux échantillons encadrant la ligne).
    /// Pour un tour incomplet : durée couverte.
    pub lap_time: f64,
    pub flags: LapFlags,
    pub samples: Vec<Sample>,
}

impl Lap {
    pub fn is_valid(&self) -> bool {
        self.flags.is_valid()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct LapRecorderConfig {
    /// Nombre de tours conservés.
    pub history: usize,
    /// Saut de `LapDistPct` entre deux échantillons au-delà duquel on considère
    /// une téléportation.
    pub max_pct_jump: f32,
    /// Trou de `SessionTime` au-delà duquel on considère une discontinuité, s.
    pub max_time_gap: f64,
}

impl Default for LapRecorderConfig {
    fn default() -> Self {
        LapRecorderConfig {
            history: 50,
            max_pct_jump: 0.02,
            max_time_gap: 0.5,
        }
    }
}

struct LapBuilder {
    start_time: f64,
    flags: LapFlags,
    samples: Vec<Sample>,
    was_off_track: bool,
}

impl LapBuilder {
    fn new(start_time: f64, complete: bool) -> Self {
        LapBuilder {
            start_time,
            flags: LapFlags {
                complete,
                ..Default::default()
            },
            samples: Vec::with_capacity(60 * 150),
            was_off_track: false,
        }
    }

    fn push(&mut self, s: Sample) {
        if s.on_pit_road
            || matches!(
                s.surface,
                TrackSurface::InPitStall | TrackSurface::ApproachingPits
            )
        {
            self.flags.pit = true;
        }
        let off = s.surface == TrackSurface::OffTrack;
        if off && !self.was_off_track {
            self.flags.off_track += 1;
        }
        self.was_off_track = off;
        self.samples.push(s);
    }

    fn finish(self, end_time: f64) -> Lap {
        // Numéro du tour : valeur de `Lap` au milieu du tour, insensible au retard
        // d'un tick d'iRacing au passage de la ligne.
        let number = self
            .samples
            .get(self.samples.len() / 2)
            .map(|s| s.lap)
            .unwrap_or_default();
        Lap {
            number,
            start_time: self.start_time,
            lap_time: end_time - self.start_time,
            flags: self.flags,
            samples: self.samples,
        }
    }
}

/// Événement produit par [`LapRecorder::push`].
#[derive(Debug, Clone)]
pub enum LapEvent {
    Completed(Arc<Lap>),
    /// Tour abandonné (voiture retirée de la piste, reset au garage).
    Aborted,
}

pub struct LapRecorder {
    cfg: LapRecorderConfig,
    current: Option<LapBuilder>,
    prev: Option<Sample>,
    history: VecDeque<Arc<Lap>>,
}

impl Default for LapRecorder {
    fn default() -> Self {
        Self::new(LapRecorderConfig::default())
    }
}

impl LapRecorder {
    pub fn new(cfg: LapRecorderConfig) -> Self {
        LapRecorder {
            cfg,
            current: None,
            prev: None,
            history: VecDeque::with_capacity(cfg.history),
        }
    }

    /// Oublie le tour en cours et l'historique (nouvelle session).
    pub fn reset(&mut self) {
        self.current = None;
        self.prev = None;
        self.history.clear();
    }

    pub fn history(&self) -> impl DoubleEndedIterator<Item = &Arc<Lap>> {
        self.history.iter()
    }

    pub fn last_lap(&self) -> Option<&Arc<Lap>> {
        self.history.back()
    }

    pub fn best_lap(&self) -> Option<&Arc<Lap>> {
        self.history
            .iter()
            .filter(|l| l.is_valid())
            .min_by(|a, b| a.lap_time.total_cmp(&b.lap_time))
    }

    /// Temps écoulé dans le tour en cours, s.
    pub fn current_lap_elapsed(&self) -> Option<f64> {
        let cur = self.current.as_ref()?;
        let last = cur.samples.last()?;
        Some(last.session_time - cur.start_time)
    }

    pub fn current_lap_flags(&self) -> Option<LapFlags> {
        self.current.as_ref().map(|c| c.flags)
    }

    pub fn push(&mut self, s: Sample) -> Option<LapEvent> {
        if !s.is_on_track || !s.lap_dist_pct.is_finite() || !s.session_time.is_finite() {
            self.prev = None;
            return self.current.take().map(|_| LapEvent::Aborted);
        }
        let Some(prev) = self.prev.replace(s) else {
            // Premier échantillon en piste : tour partiel jusqu'à la ligne.
            let mut b = LapBuilder::new(s.session_time, false);
            b.push(s);
            self.current = Some(b);
            return None;
        };

        let dt = s.session_time - prev.session_time;
        if dt <= 0.0 {
            // Doublon ou retour en arrière du temps (rejeu relancé) : on ignore.
            if dt < 0.0 {
                self.prev = None;
                return self.current.take().map(|_| LapEvent::Aborted);
            }
            self.prev = Some(prev);
            return None;
        }

        let crossed = prev.lap_dist_pct > 0.9 && s.lap_dist_pct < 0.1;
        let mut event = None;
        if crossed {
            // Interpolation linéaire de l'instant de passage de la ligne.
            let before = 1.0 - prev.lap_dist_pct as f64;
            let after = s.lap_dist_pct as f64;
            let frac = if before + after > 0.0 {
                before / (before + after)
            } else {
                0.5
            };
            let t_cross = prev.session_time + frac * dt;
            if let Some(cur) = self.current.take() {
                let lap = Arc::new(cur.finish(t_cross));
                if self.history.len() == self.cfg.history {
                    self.history.pop_front();
                }
                self.history.push_back(lap.clone());
                event = Some(LapEvent::Completed(lap));
            }
            self.current = Some(LapBuilder::new(t_cross, true));
        } else {
            let jump = s.lap_dist_pct - prev.lap_dist_pct;
            let wrapped_backwards = prev.lap_dist_pct < 0.1 && s.lap_dist_pct > 0.9;
            if (wrapped_backwards || jump < -0.001 || jump > self.cfg.max_pct_jump)
                && let Some(cur) = self.current.as_mut()
            {
                cur.flags.discontinuity = true;
            }
        }
        if dt > self.cfg.max_time_gap
            && let Some(cur) = self.current.as_mut()
        {
            cur.flags.discontinuity = true;
        }
        if let Some(cur) = self.current.as_mut() {
            cur.push(s);
        }
        event
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(t: f64, pct: f32, lap: i32) -> Sample {
        Sample {
            session_time: t,
            lap,
            lap_dist_pct: pct,
            speed: 50.0,
            is_on_track: true,
            surface: TrackSurface::OnTrack,
            ..Default::default()
        }
    }

    /// Tours de `period` secondes à 60 Hz, départ à `start_pct`.
    fn drive(rec: &mut LapRecorder, start_pct: f64, seconds: f64, period: f64) -> Vec<Arc<Lap>> {
        let mut laps = vec![];
        let n = (seconds * 60.0) as usize;
        for i in 0..n {
            let t = i as f64 / 60.0;
            let dist = start_pct + t / period;
            let s = sample(t, dist.fract() as f32, dist.floor() as i32 + 1);
            if let Some(LapEvent::Completed(l)) = rec.push(s) {
                laps.push(l);
            }
        }
        laps
    }

    #[test]
    fn splits_laps_and_interpolates_time() {
        let mut rec = LapRecorder::default();
        let laps = drive(&mut rec, 0.5, 320.0, 90.0);
        // Départ à mi-tour : un tour partiel puis trois tours complets en 320 s.
        assert_eq!(laps.len(), 4);
        assert!(!laps[0].flags.complete);
        assert!((laps[0].lap_time - 45.0).abs() < 0.05);
        for l in &laps[1..] {
            assert!(l.is_valid(), "{:?}", l.flags);
            assert!((l.lap_time - 90.0).abs() < 1e-3, "temps {}", l.lap_time);
        }
        assert_eq!(laps[1].number, 2);
        assert_eq!(laps[3].number, 4);
        assert!(rec.best_lap().unwrap().is_valid());
    }

    #[test]
    fn history_is_bounded() {
        let mut rec = LapRecorder::new(LapRecorderConfig {
            history: 2,
            ..Default::default()
        });
        drive(&mut rec, 0.0, 10.0 * 20.0 + 1.0, 20.0);
        assert_eq!(rec.history().count(), 2);
        assert_eq!(rec.last_lap().unwrap().number, 10);
    }

    #[test]
    fn teleport_flags_discontinuity() {
        let mut rec = LapRecorder::default();
        let mut out = vec![];
        let mut t = 0.0;
        let mut push = |pct: f32, rec: &mut LapRecorder| {
            t += 1.0 / 60.0;
            if let Some(LapEvent::Completed(l)) = rec.push(sample(t, pct, 1)) {
                out.push(l);
            }
        };
        push(0.95, &mut rec);
        push(0.99, &mut rec);
        push(0.01, &mut rec); // ligne : début d'un tour complet
        push(0.02, &mut rec);
        push(0.40, &mut rec); // saut : reset / remorquage
        for i in 0..60 {
            push(0.40 + i as f32 * 0.01, &mut rec);
        }
        push(0.005, &mut rec);
        assert_eq!(out.len(), 2);
        assert!(out[1].flags.complete);
        assert!(out[1].flags.discontinuity);
        assert!(!out[1].is_valid());
    }

    #[test]
    fn pit_and_off_track_invalidate() {
        let mut rec = LapRecorder::default();
        let mut laps = vec![];
        for i in 0..(60 * 25) {
            let t = i as f64 / 60.0;
            let d = t / 10.0;
            let mut s = sample(t, d.fract() as f32, d as i32);
            if (12.0..12.5).contains(&t) {
                s.surface = TrackSurface::OffTrack;
            }
            if (21.0..22.0).contains(&t) {
                s.on_pit_road = true;
            }
            if let Some(LapEvent::Completed(l)) = rec.push(s) {
                laps.push(l);
            }
        }
        assert_eq!(laps.len(), 2);
        assert_eq!(laps[1].flags.off_track, 1);
        assert!(!laps[1].is_valid());
    }

    #[test]
    fn leaving_track_aborts_current_lap() {
        let mut rec = LapRecorder::default();
        rec.push(sample(0.0, 0.5, 1));
        let mut s = sample(0.1, 0.51, 1);
        s.is_on_track = false;
        assert!(matches!(rec.push(s), Some(LapEvent::Aborted)));
        assert!(rec.current_lap_elapsed().is_none());
    }
}
