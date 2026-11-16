//! Trigger engine, run on every sample (60 Hz).
//!
//! A trigger is "armed" when the car crosses its position and its condition
//! holds. It is spoken as soon as a slot opens: nobody is speaking, the car is
//! not in a quiet zone, and the sentence has time to be spoken before the next
//! quiet zone. Past its maximum delay, it is dropped.
//!
//! Hard rule: no speech in a braking zone. The engine starts nothing while
//! braking (pedal above 5 %) or in a quiet zone, and cuts off the current
//! speech if it enters one.

use telemetry::Sample;

use crate::plan::{QuietZone, Repeat, Trigger};

/// Brake pressure above which the driver is considered to be braking.
pub const BRAKE_SILENCE: f32 = 0.05;
/// Safety margin between the end of a sentence and the quiet zone, s.
const FIT_MARGIN_S: f64 = 0.4;
/// Position jump beyond which a position is not considered crossed
/// (teleport, reset).
const MAX_STEP_PCT: f64 = 0.05;

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Say {
        trigger_id: String,
        text: String,
        priority: u8,
    },
    Stop {
        reason: StopReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Braking,
    QuietZone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerState {
    Waiting,
    Queued,
    Spoken,
    Expired,
}

#[derive(Debug, Clone)]
pub struct TriggerView {
    pub trigger: Trigger,
    pub state: TriggerState,
    pub times_spoken: u32,
    pub last_spoken_lap: Option<i32>,
    pub dropped: u32,
}

/// Context information provided on each tick.
#[derive(Debug, Clone, Copy, Default)]
pub struct TickContext {
    /// Live delta to the reference, s.
    pub live_delta: Option<f64>,
    /// A sentence is currently playing.
    pub speaking: bool,
    /// Track length, m (to compute the available time).
    pub track_length_m: f64,
}

struct Slot {
    trigger: Trigger,
    times_spoken: u32,
    last_spoken_lap: Option<i32>,
    armed_lap: Option<i32>,
    expired: bool,
    dropped: u32,
    /// Lap on which the trigger was activated (basis for its expiry).
    base_lap: Option<i32>,
}

struct Pending {
    slot: usize,
    armed_pct: f64,
}

pub struct TriggerEngine {
    slots: Vec<Slot>,
    quiet: Vec<QuietZone>,
    pending: Vec<Pending>,
    prev: Option<Sample>,
    /// Injected triggers to arm on the next sample.
    to_arm: Vec<usize>,
    stop_sent: bool,
}

fn forward(a: f64, b: f64) -> f64 {
    (b - a).rem_euclid(1.0)
}

/// Is `at` crossed when moving from `prev` to `cur` (forward)?
fn crossed(prev: f64, cur: f64, at: f64) -> bool {
    let step = forward(prev, cur);
    step > 0.0 && step < MAX_STEP_PCT && forward(prev, at) > 0.0 && forward(prev, at) <= step
}

impl TriggerEngine {
    pub fn new(triggers: Vec<Trigger>, quiet: Vec<QuietZone>) -> Self {
        TriggerEngine {
            slots: triggers
                .into_iter()
                .map(|trigger| Slot {
                    trigger,
                    times_spoken: 0,
                    last_spoken_lap: None,
                    armed_lap: None,
                    expired: false,
                    dropped: 0,
                    base_lap: None,
                })
                .collect(),
            quiet,
            pending: vec![],
            prev: None,
            to_arm: vec![],
            stop_sent: false,
        }
    }

    /// Carries over the state of unchanged triggers from a previous engine (same
    /// id, same text, same position): an updated plan does not repeat a "once"
    /// trigger already spoken, nor restart its expiry.
    pub fn carry_state_from(&mut self, old: &TriggerEngine) {
        for slot in &mut self.slots {
            if let Some(o) = old.slots.iter().find(|o| {
                o.trigger.id == slot.trigger.id
                    && o.trigger.text == slot.trigger.text
                    && (o.trigger.at_pct - slot.trigger.at_pct).abs() < 1e-9
            }) {
                slot.times_spoken = o.times_spoken;
                slot.last_spoken_lap = o.last_spoken_lap;
                slot.armed_lap = o.armed_lap;
                slot.expired = o.expired;
                slot.dropped = o.dropped;
                slot.base_lap = o.base_lap;
            }
        }
        self.prev = old.prev;
    }

    /// Adds a one-off trigger (spoken only once). `arm_now`: armed right away at
    /// the current position, otherwise when `at_pct` is crossed.
    pub fn inject(&mut self, mut trigger: Trigger, arm_now: bool) {
        trigger.repeat = Repeat::Once;
        self.slots.push(Slot {
            trigger,
            times_spoken: 0,
            last_spoken_lap: None,
            armed_lap: None,
            expired: false,
            dropped: 0,
            base_lap: None,
        });
        if arm_now {
            self.to_arm.push(self.slots.len() - 1);
        }
    }

    pub fn quiet_zones(&self) -> &[QuietZone] {
        &self.quiet
    }

    pub fn in_quiet_zone(&self, pct: f64) -> bool {
        self.quiet.iter().any(|z| z.contains(pct))
    }

    /// Distance (pct) to the start of the next quiet zone.
    fn distance_to_quiet(&self, pct: f64) -> f64 {
        self.quiet
            .iter()
            .map(|z| forward(pct, z.start))
            .fold(1.0, f64::min)
    }

    pub fn views(&self) -> Vec<TriggerView> {
        self.slots
            .iter()
            .enumerate()
            .map(|(i, s)| TriggerView {
                trigger: s.trigger.clone(),
                state: if s.expired {
                    TriggerState::Expired
                } else if self.pending.iter().any(|p| p.slot == i) {
                    TriggerState::Queued
                } else if s.times_spoken > 0 {
                    TriggerState::Spoken
                } else {
                    TriggerState::Waiting
                },
                times_spoken: s.times_spoken,
                last_spoken_lap: s.last_spoken_lap,
                dropped: s.dropped,
            })
            .collect()
    }

    fn eligible(&self, slot: &Slot, lap: i32) -> bool {
        if slot.expired || slot.armed_lap == Some(lap) {
            return false;
        }
        match slot.trigger.repeat {
            Repeat::Once => slot.times_spoken == 0,
            Repeat::EveryLap => slot.last_spoken_lap != Some(lap),
            Repeat::Laps(n) => slot.times_spoken < n && slot.last_spoken_lap != Some(lap),
        }
    }

    /// One tick: `duration` gives the duration (s) of a sentence once spoken.
    pub fn tick(
        &mut self,
        s: &Sample,
        ctx: &TickContext,
        duration: &mut dyn FnMut(&str) -> f64,
    ) -> Vec<Command> {
        let mut out = vec![];
        let pct = s.lap_dist_pct as f64;
        let Some(prev) = self.prev.replace(*s) else {
            for slot in &mut self.slots {
                slot.base_lap.get_or_insert(s.lap);
            }
            return out;
        };
        if !pct.is_finite() || !s.is_on_track {
            self.pending.clear();
            return out;
        }

        // Expiry relative to each trigger's activation.
        for slot in &mut self.slots {
            let base = *slot.base_lap.get_or_insert(s.lap);
            if let Some(n) = slot.trigger.expire_after_laps
                && s.lap - base >= n as i32
            {
                slot.expired = true;
            }
        }

        // Hard rule: silence while braking and in quiet zones.
        let braking = s.brake >= BRAKE_SILENCE;
        let quiet = self.in_quiet_zone(pct);
        if ctx.speaking && (braking || quiet) {
            if !self.stop_sent {
                out.push(Command::Stop {
                    reason: if braking {
                        StopReason::Braking
                    } else {
                        StopReason::QuietZone
                    },
                });
                self.stop_sent = true;
            }
        } else if !ctx.speaking {
            self.stop_sent = false;
        }

        // Injected triggers: armed at the current position.
        for i in std::mem::take(&mut self.to_arm) {
            self.slots[i].armed_lap = Some(s.lap);
            self.pending.push(Pending {
                slot: i,
                armed_pct: pct,
            });
        }

        // Arm crossed triggers.
        let prev_pct = prev.lap_dist_pct as f64;
        for i in 0..self.slots.len() {
            let slot = &self.slots[i];
            let at = slot.trigger.at_pct;
            if !crossed(prev_pct, pct, at) || !self.eligible(slot, s.lap) {
                continue;
            }
            if let Some(c) = &slot.trigger.condition
                && !c.holds(s.speed, ctx.live_delta, s.lap)
            {
                continue;
            }
            self.slots[i].armed_lap = Some(s.lap);
            self.pending.push(Pending {
                slot: i,
                armed_pct: at,
            });
        }

        // Drop triggers that are too late.
        let slots = &mut self.slots;
        self.pending.retain(|p| {
            let keep = forward(p.armed_pct, pct) <= slots[p.slot].trigger.max_delay_pct;
            if !keep {
                slots[p.slot].dropped += 1;
            }
            keep
        });

        // Speech slot.
        if ctx.speaking || braking || quiet || self.pending.is_empty() {
            return out;
        }
        let speed = (s.speed as f64).max(5.0);
        let available_s = self.distance_to_quiet(pct) * ctx.track_length_m / speed;
        let mut order: Vec<usize> = (0..self.pending.len()).collect();
        order.sort_by(|&a, &b| {
            let (sa, sb) = (
                &self.slots[self.pending[a].slot],
                &self.slots[self.pending[b].slot],
            );
            sb.trigger.priority.cmp(&sa.trigger.priority).then(
                forward(self.pending[b].armed_pct, pct)
                    .total_cmp(&forward(self.pending[a].armed_pct, pct)),
            )
        });
        for k in order {
            let slot_idx = self.pending[k].slot;
            let text = self.slots[slot_idx].trigger.text.clone();
            let d = duration(&text);
            if self.quiet.is_empty() || d + FIT_MARGIN_S <= available_s {
                self.pending.remove(k);
                let slot = &mut self.slots[slot_idx];
                slot.times_spoken += 1;
                slot.last_spoken_lap = Some(s.lap);
                if slot.trigger.repeat == Repeat::Once
                    || matches!(slot.trigger.repeat, Repeat::Laps(n) if slot.times_spoken >= n)
                {
                    slot.expired = true;
                }
                out.push(Command::Say {
                    trigger_id: slot.trigger.id.clone(),
                    text,
                    priority: slot.trigger.priority,
                });
                break;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::Condition;
    use telemetry::TrackSurface;

    fn trig(id: &str, at: f64, priority: u8) -> Trigger {
        Trigger {
            id: id.into(),
            text: format!("trigger {id}"),
            at_pct: at,
            corner: None,
            priority,
            repeat: Repeat::EveryLap,
            condition: None,
            expire_after_laps: None,
            max_delay_pct: 0.05,
        }
    }

    fn sample(pct: f64, lap: i32, brake: f32) -> Sample {
        Sample {
            lap_dist_pct: pct as f32,
            lap,
            speed: 50.0,
            brake,
            is_on_track: true,
            surface: TrackSurface::OnTrack,
            ..Default::default()
        }
    }

    /// Simulation: the car advances by `step` per tick over `laps` laps.
    /// Speech lasts `speech_s`; braking is active in `brake_zones`.
    struct Sim {
        engine: TriggerEngine,
        speaking_until: f64,
        t: f64,
        log: Vec<(i32, f64, Command)>,
    }

    impl Sim {
        fn new(engine: TriggerEngine) -> Self {
            Sim {
                engine,
                speaking_until: 0.0,
                t: 0.0,
                log: vec![],
            }
        }

        fn run(&mut self, laps: i32, speech_s: f64, brake_zones: &[(f64, f64)]) {
            let step = 0.001; // 1000 ticks per lap, 1/60 s each
            for lap in 1..=laps {
                for i in 0..1000 {
                    let pct = i as f64 * step;
                    let braking = brake_zones.iter().any(|&(a, b)| pct >= a && pct <= b);
                    let s = sample(pct, lap, if braking { 0.8 } else { 0.0 });
                    self.t += 1.0 / 60.0;
                    let ctx = TickContext {
                        live_delta: Some(0.3),
                        speaking: self.t < self.speaking_until,
                        track_length_m: 3000.0,
                    };
                    let cmds = self.engine.tick(&s, &ctx, &mut |_| speech_s);
                    for c in cmds {
                        match &c {
                            Command::Say { .. } => self.speaking_until = self.t + speech_s,
                            Command::Stop { .. } => self.speaking_until = self.t,
                        }
                        self.log.push((lap, pct, c));
                    }
                }
            }
        }

        fn said(&self) -> Vec<(i32, f64, String)> {
            self.log
                .iter()
                .filter_map(|(l, p, c)| match c {
                    Command::Say { trigger_id, .. } => Some((*l, *p, trigger_id.clone())),
                    _ => None,
                })
                .collect()
        }
    }

    #[test]
    fn fires_once_per_lap_at_position() {
        let mut sim = Sim::new(TriggerEngine::new(vec![trig("a", 0.3, 5)], vec![]));
        sim.run(3, 1.0, &[]);
        let said = sim.said();
        assert_eq!(said.len(), 3);
        for (lap, (l, p, _)) in said.iter().enumerate() {
            assert_eq!(*l, lap as i32 + 1);
            assert!((p - 0.3).abs() < 0.002, "{p}");
        }
    }

    #[test]
    fn once_and_limited_repeats() {
        let mut once = trig("once", 0.2, 5);
        once.repeat = Repeat::Once;
        let mut twice = trig("twice", 0.6, 5);
        twice.repeat = Repeat::Laps(2);
        let mut sim = Sim::new(TriggerEngine::new(vec![once, twice], vec![]));
        sim.run(4, 1.0, &[]);
        let ids: Vec<String> = sim.said().into_iter().map(|x| x.2).collect();
        assert_eq!(ids, vec!["once", "twice", "twice"]);
        let views = sim.engine.views();
        assert!(views.iter().all(|v| v.state == TriggerState::Expired));
    }

    #[test]
    fn never_starts_speaking_while_braking() {
        // Trigger placed in the middle of braking: it waits for the end of braking.
        let mut t = trig("a", 0.40, 5);
        t.max_delay_pct = 0.10;
        let mut sim = Sim::new(TriggerEngine::new(vec![t], vec![]));
        sim.run(1, 1.0, &[(0.38, 0.45)]);
        let said = sim.said();
        assert_eq!(said.len(), 1);
        assert!(said[0].1 > 0.45, "spoken at {}", said[0].1);
    }

    #[test]
    fn dropped_when_too_late() {
        let t = trig("a", 0.40, 5); // max delay 5 %
        let mut sim = Sim::new(TriggerEngine::new(vec![t], vec![]));
        sim.run(1, 1.0, &[(0.38, 0.50)]);
        assert!(sim.said().is_empty());
        assert_eq!(sim.engine.views()[0].dropped, 1);
    }

    #[test]
    fn waits_for_a_gap_long_enough_before_quiet_zone() {
        // Quiet zone at 0.30; trigger at 0.27. At 50 m/s on 3000 m, 0.03 lap
        // = 90 m = 1.8 s: a 3 s sentence does not fit, it goes after the zone.
        let quiet = vec![QuietZone {
            start: 0.30,
            end: 0.35,
        }];
        let mut t = trig("a", 0.27, 5);
        t.max_delay_pct = 0.2;
        let mut sim = Sim::new(TriggerEngine::new(vec![t], quiet));
        sim.run(1, 3.0, &[]);
        let said = sim.said();
        assert_eq!(said.len(), 1);
        assert!(said[0].1 > 0.35, "spoken at {}", said[0].1);
        // A short sentence (1 s) fits before the zone.
        let mut short = trig("b", 0.27, 5);
        short.max_delay_pct = 0.2;
        let mut sim = Sim::new(TriggerEngine::new(
            vec![short],
            vec![QuietZone {
                start: 0.30,
                end: 0.35,
            }],
        ));
        sim.run(1, 1.0, &[]);
        assert!(sim.said()[0].1 < 0.30);
    }

    #[test]
    fn stops_speech_when_braking_starts() {
        let mut sim = Sim::new(TriggerEngine::new(vec![trig("a", 0.10, 5)], vec![]));
        // Very long sentence (10 s): braking at 0.15 cuts it off.
        sim.run(1, 10.0, &[(0.15, 0.20)]);
        let stops: Vec<_> = sim
            .log
            .iter()
            .filter(|(_, _, c)| {
                matches!(
                    c,
                    Command::Stop {
                        reason: StopReason::Braking
                    }
                )
            })
            .collect();
        assert_eq!(stops.len(), 1);
        assert!((stops[0].1 - 0.15).abs() < 0.002);
    }

    #[test]
    fn priority_wins_when_both_pending() {
        // Two triggers armed while a sentence is playing: the higher priority one goes first.
        let mut first = trig("first", 0.10, 5);
        first.text = "long".into();
        let mut low = trig("low", 0.11, 1);
        low.max_delay_pct = 0.3;
        let mut high = trig("high", 0.12, 9);
        high.max_delay_pct = 0.3;
        let mut sim = Sim::new(TriggerEngine::new(vec![first, low, high], vec![]));
        sim.run(1, 2.0, &[]);
        let ids: Vec<String> = sim.said().into_iter().map(|x| x.2).collect();
        assert_eq!(ids, vec!["first", "high", "low"]);
    }

    #[test]
    fn condition_gates_arming() {
        let mut t = trig("a", 0.5, 5);
        t.condition = Some(Condition::DeltaAbove(1.0)); // simulated delta: 0.3 s
        let mut u = trig("b", 0.6, 5);
        u.condition = Some(Condition::DeltaAbove(0.2));
        let mut sim = Sim::new(TriggerEngine::new(vec![t, u], vec![]));
        sim.run(1, 1.0, &[]);
        let ids: Vec<String> = sim.said().into_iter().map(|x| x.2).collect();
        assert_eq!(ids, vec!["b"]);
    }

    #[test]
    fn expires_after_laps() {
        let mut t = trig("a", 0.5, 5);
        t.expire_after_laps = Some(2);
        let mut sim = Sim::new(TriggerEngine::new(vec![t], vec![]));
        sim.run(4, 1.0, &[]);
        assert_eq!(sim.said().len(), 2);
    }

    #[test]
    fn injected_message_waits_for_end_of_braking() {
        let mut engine = TriggerEngine::new(vec![], vec![]);
        let mut injected = trig("say", f64::NAN, 9);
        injected.max_delay_pct = 0.3;
        let mut sim = Sim::new(engine);
        // A first lap to get a sample, then injection during braking.
        sim.run(1, 1.0, &[]);
        engine = std::mem::replace(&mut sim.engine, TriggerEngine::new(vec![], vec![]));
        engine.inject(injected, true);
        sim.engine = engine;
        sim.run(1, 1.0, &[(0.0, 0.05)]);
        let said = sim.said();
        assert_eq!(said.len(), 1);
        assert!(said[0].1 > 0.05, "spoken at {}", said[0].1);
        assert_eq!(sim.engine.views()[0].state, TriggerState::Expired);
    }

    #[test]
    fn carried_state_keeps_once_triggers_silent() {
        let mut once = trig("once", 0.2, 5);
        once.repeat = Repeat::Once;
        let mut sim = Sim::new(TriggerEngine::new(vec![once.clone()], vec![]));
        sim.run(1, 1.0, &[]);
        assert_eq!(sim.said().len(), 1);
        // Updated plan: the same trigger plus a new one.
        let mut engine = TriggerEngine::new(vec![once, trig("new", 0.5, 5)], vec![]);
        engine.carry_state_from(&sim.engine);
        let mut sim2 = Sim::new(engine);
        sim2.run(1, 1.0, &[]);
        let ids: Vec<String> = sim2.said().into_iter().map(|x| x.2).collect();
        assert_eq!(ids, vec!["new"]);
    }

    #[test]
    fn position_crossing_wraps_the_line() {
        assert!(crossed(0.998, 0.001, 0.0));
        assert!(crossed(0.998, 0.001, 0.999));
        assert!(!crossed(0.998, 0.001, 0.5));
        assert!(!crossed(0.2, 0.6, 0.4), "teleport");
        assert!(!crossed(0.3, 0.3, 0.3));
    }
}
