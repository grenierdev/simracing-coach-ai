//! Trigger engine, run on every sample (60 Hz).
//!
//! A trigger is "armed" when the car crosses its position and its condition
//! holds, then spoken as soon as nobody is speaking. Past its maximum delay,
//! it is dropped. Cues are placed to end before the braking zone (see
//! [`crate::timing`]); the pedals do not hold them back.
//!
//! A trigger is spoken once per pass, not per lap number: the line starts a
//! new pass, and so does a jump (reset to a save point, teleport). After a
//! reset that puts the car between a cue and its corner, the cue is spoken at
//! once.
//!
//! Optional rule (`silence_in_braking`): no speech in a braking zone. The
//! engine then starts nothing while braking (pedal above 5 %) or in a quiet
//! zone, waits for a gap long enough before the next quiet zone, and cuts off
//! the current speech if it enters one.

use telemetry::Sample;

use crate::plan::{QuietZone, Repeat, Trigger};
use crate::timing::{FIT_MARGIN_S, TimeProfile};

/// Brake pressure above which the driver is considered to be braking.
pub const BRAKE_SILENCE: f32 = 0.05;
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

#[derive(Clone)]
struct Slot {
    trigger: Trigger,
    times_spoken: u32,
    /// Lap number when last spoken (display).
    last_spoken_lap: Option<i32>,
    /// Pass on which the trigger was last armed, and last spoken.
    armed_pass: Option<u64>,
    spoken_pass: Option<u64>,
    expired: bool,
    dropped: u32,
    /// Lap on which the trigger was activated (basis for its expiry).
    base_lap: Option<i32>,
}

impl Slot {
    fn new(trigger: Trigger) -> Self {
        Slot {
            trigger,
            times_spoken: 0,
            last_spoken_lap: None,
            armed_pass: None,
            spoken_pass: None,
            expired: false,
            dropped: 0,
            base_lap: None,
        }
    }
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
    /// Reference timing: speaking time left before the next quiet zone.
    profile: Option<TimeProfile>,
    /// Current pass: a new one starts at the line and after every jump
    /// (reset, teleport, back on track). A trigger is spoken once per pass.
    pass: u64,
    /// Hold speech while braking and in quiet zones (off by default).
    silence_in_braking: bool,
}

/// A backward step smaller than this is noise, not a reset, fraction of a lap.
const JITTER_PCT: f64 = 0.002;

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
            slots: triggers.into_iter().map(Slot::new).collect(),
            quiet,
            pending: vec![],
            prev: None,
            to_arm: vec![],
            stop_sent: false,
            profile: None,
            pass: 0,
            silence_in_braking: false,
        }
    }

    /// Measures the time left before a quiet zone on the reference lap rather
    /// than from the current speed alone.
    pub fn set_time_profile(&mut self, profile: TimeProfile) {
        self.profile = Some(profile);
    }

    /// Holds speech while braking and in the quiet zones, and cuts a sentence
    /// when braking starts. Off by default: a cue is spoken where it is
    /// placed (before the braking zone), whatever the pedals do.
    pub fn set_silence_in_braking(&mut self, on: bool) {
        self.silence_in_braking = on;
    }

    /// Carries over the state of unchanged triggers from a previous engine (same
    /// id, same text): an updated plan does not repeat a "once" trigger already
    /// spoken, nor restart its expiry. The position is not compared: adding a
    /// cue on the same straight can move the others to make room. One-off
    /// sentences not spoken yet (`inject`) are carried over too, with their
    /// place in the queue.
    pub fn carry_state_from(&mut self, old: &TriggerEngine) {
        for (i, o) in old.slots.iter().enumerate() {
            let one_off = !o.trigger.at_pct.is_finite() || o.trigger.id.starts_with("say-");
            if !one_off || o.expired || self.slots.iter().any(|s| s.trigger.id == o.trigger.id) {
                continue;
            }
            self.slots.push(o.clone());
            let j = self.slots.len() - 1;
            if let Some(p) = old.pending.iter().find(|p| p.slot == i) {
                self.pending.push(Pending {
                    slot: j,
                    armed_pct: p.armed_pct,
                });
            }
            if old.to_arm.contains(&i) {
                self.to_arm.push(j);
            }
        }
        for slot in &mut self.slots {
            if let Some(o) = old
                .slots
                .iter()
                .find(|o| o.trigger.id == slot.trigger.id && o.trigger.text == slot.trigger.text)
            {
                slot.times_spoken = o.times_spoken;
                slot.last_spoken_lap = o.last_spoken_lap;
                slot.armed_pass = o.armed_pass;
                slot.spoken_pass = o.spoken_pass;
                slot.expired = o.expired;
                slot.dropped = o.dropped;
                slot.base_lap = o.base_lap;
            }
        }
        self.prev = old.prev;
        self.pass = old.pass;
    }

    /// Adds a one-off trigger (spoken only once). `arm_now`: armed right away at
    /// the current position, otherwise when `at_pct` is crossed.
    pub fn inject(&mut self, mut trigger: Trigger, arm_now: bool) {
        trigger.repeat = Repeat::Once;
        self.slots.push(Slot::new(trigger));
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

    fn eligible(&self, slot: &Slot) -> bool {
        if slot.expired || slot.armed_pass == Some(self.pass) {
            return false;
        }
        match slot.trigger.repeat {
            Repeat::Once => slot.times_spoken == 0,
            Repeat::EveryLap => slot.spoken_pass != Some(self.pass),
            Repeat::Laps(n) => slot.times_spoken < n && slot.spoken_pass != Some(self.pass),
        }
    }

    fn arm(&mut self, i: usize, armed_pct: f64) {
        self.slots[i].armed_pass = Some(self.pass);
        self.pending.push(Pending { slot: i, armed_pct });
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

        // Optional rule: silence while braking and in quiet zones.
        let braking = self.silence_in_braking && s.brake >= BRAKE_SILENCE;
        let quiet = self.silence_in_braking && self.in_quiet_zone(pct);
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

        // Passes: the line starts a new one, and so does a jump (reset to a
        // save point, teleport, back on track), so that a cue already spoken
        // is spoken again on the next try. Tiny backward steps are noise.
        let prev_pct = prev.lap_dist_pct as f64;
        let step = forward(prev_pct, pct);
        let back_on_track = !prev.is_on_track || !prev_pct.is_finite();
        let jitter = step > 1.0 - JITTER_PCT;
        let jumped = back_on_track || (step >= MAX_STEP_PCT && !jitter);
        if jumped || crossed(prev_pct, pct, 0.0) {
            self.pass += 1;
        }

        // Injected triggers: armed at the current position.
        for i in std::mem::take(&mut self.to_arm) {
            self.arm(i, pct);
        }

        let holds = |slot: &Slot| {
            slot.trigger
                .condition
                .as_ref()
                .is_none_or(|c| c.holds(s.speed, ctx.live_delta, s.lap))
        };
        if jumped {
            // After a reset between a cue and its corner, the cue is spoken
            // now: the car will not cross its position before the corner.
            for i in 0..self.slots.len() {
                let slot = &self.slots[i];
                let (at, Some(anchor)) = (slot.trigger.at_pct, slot.trigger.anchor_pct) else {
                    continue;
                };
                let into = forward(at, pct);
                if at.is_finite()
                    && into > 0.0
                    && into < forward(at, anchor)
                    && self.eligible(slot)
                    && holds(slot)
                {
                    self.arm(i, pct);
                }
            }
        } else {
            // Arm crossed triggers.
            for i in 0..self.slots.len() {
                let slot = &self.slots[i];
                if crossed(prev_pct, pct, slot.trigger.at_pct) && self.eligible(slot) && holds(slot)
                {
                    let at = slot.trigger.at_pct;
                    self.arm(i, at);
                }
            }
        }

        // Drop triggers that are too late.
        let slots = &mut self.slots;
        self.pending.retain(|p| {
            let keep = forward(p.armed_pct, pct) <= slots[p.slot].trigger.max_delay_pct;
            if !keep {
                slots[p.slot].dropped += 1;
                tracing::info!(
                    "cue \"{}\" dropped: no slot to speak it within {:.0} % of a lap",
                    slots[p.slot].trigger.id,
                    slots[p.slot].trigger.max_delay_pct * 100.0
                );
            }
            keep
        });

        // Speech slot.
        if ctx.speaking || braking || quiet || self.pending.is_empty() {
            return out;
        }
        let check_fit = self.silence_in_braking && !self.quiet.is_empty();
        // The current speed overestimates the time left on a corner exit,
        // where the car accelerates: the reference timing bounds it.
        let speed = (s.speed as f64).max(5.0);
        let by_speed = self.distance_to_quiet(pct) * ctx.track_length_m / speed;
        let available_s = match &self.profile {
            Some(p) => by_speed.min(p.time_to_quiet(pct, &self.quiet)),
            None => by_speed,
        };
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
            let fits = !check_fit || duration(&text) + FIT_MARGIN_S <= available_s;
            if fits {
                self.pending.remove(k);
                let pass = self.pass;
                let slot = &mut self.slots[slot_idx];
                slot.times_spoken += 1;
                slot.last_spoken_lap = Some(s.lap);
                slot.spoken_pass = Some(pass);
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
            anchor_pct: None,
        }
    }

    /// The engine with the optional braking rule on.
    fn braking_rule(mut e: TriggerEngine) -> TriggerEngine {
        e.set_silence_in_braking(true);
        e
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
            for lap in 1..=laps {
                self.drive(0, 1000, lap, speech_s, brake_zones);
            }
        }

        /// Drives from `from` to `to` thousandths of a lap (1/60 s each).
        fn drive(
            &mut self,
            from: usize,
            to: usize,
            lap: i32,
            speech_s: f64,
            brake_zones: &[(f64, f64)],
        ) {
            for i in from..to {
                let pct = i as f64 * 0.001;
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
    fn spoken_where_placed_whatever_the_pedals() {
        // Default: braking neither holds nor cuts a cue.
        let quiet = vec![QuietZone {
            start: 0.38,
            end: 0.45,
        }];
        let mut sim = Sim::new(TriggerEngine::new(vec![trig("a", 0.40, 5)], quiet));
        sim.run(1, 10.0, &[(0.38, 0.45)]);
        let said = sim.said();
        assert_eq!(said.len(), 1);
        assert!((said[0].1 - 0.40).abs() < 0.002, "spoken at {}", said[0].1);
        assert!(
            !sim.log
                .iter()
                .any(|(_, _, c)| matches!(c, Command::Stop { .. })),
            "not cut off by braking"
        );
    }

    #[test]
    fn spoken_again_after_a_reset_on_the_same_lap() {
        // Drive to 0.5, reset back to 0.25 (same lap number), drive again:
        // the cue at 0.3 is spoken on both tries.
        let mut sim = Sim::new(TriggerEngine::new(vec![trig("a", 0.3, 5)], vec![]));
        sim.drive(0, 500, 1, 1.0, &[]);
        sim.drive(250, 500, 1, 1.0, &[]);
        let at: Vec<f64> = sim.said().into_iter().map(|x| x.1).collect();
        assert_eq!(at.len(), 2, "{at:?}");
        assert!(at.iter().all(|p| (p - 0.3).abs() < 0.002), "{at:?}");
    }

    #[test]
    fn reset_between_cue_and_corner_speaks_it_at_once() {
        // Cue at 0.30 for a corner braking at 0.40; reset to 0.35: the car
        // never crosses 0.30 again before the corner, the cue goes now.
        let mut t = trig("a", 0.30, 5);
        t.anchor_pct = Some(0.40);
        let mut other = trig("b", 0.10, 5);
        other.anchor_pct = Some(0.20);
        let mut sim = Sim::new(TriggerEngine::new(vec![t, other], vec![]));
        sim.drive(0, 500, 1, 1.0, &[]);
        sim.drive(350, 500, 1, 1.0, &[]);
        let said = sim.said();
        let ids: Vec<&str> = said.iter().map(|x| x.2.as_str()).collect();
        assert_eq!(
            ids,
            vec!["b", "a", "a"],
            "the corner behind is not repeated"
        );
        assert!((said[2].1 - 0.35).abs() < 0.002, "spoken at {}", said[2].1);
    }

    #[test]
    fn small_backward_steps_are_noise() {
        let mut sim = Sim::new(TriggerEngine::new(vec![trig("a", 0.3, 5)], vec![]));
        sim.drive(0, 302, 1, 1.0, &[]);
        // One thousandth back, then on: not a reset, not spoken twice.
        sim.drive(301, 400, 1, 1.0, &[]);
        assert_eq!(sim.said().len(), 1);
    }

    #[test]
    fn never_starts_speaking_while_braking() {
        // Trigger placed in the middle of braking: it waits for the end of braking.
        let mut t = trig("a", 0.40, 5);
        t.max_delay_pct = 0.10;
        let mut sim = Sim::new(braking_rule(TriggerEngine::new(vec![t], vec![])));
        sim.run(1, 1.0, &[(0.38, 0.45)]);
        let said = sim.said();
        assert_eq!(said.len(), 1);
        assert!(said[0].1 > 0.45, "spoken at {}", said[0].1);
    }

    #[test]
    fn dropped_when_too_late() {
        let t = trig("a", 0.40, 5); // max delay 5 %
        let mut sim = Sim::new(braking_rule(TriggerEngine::new(vec![t], vec![])));
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
        let mut sim = Sim::new(braking_rule(TriggerEngine::new(vec![t], quiet)));
        sim.run(1, 3.0, &[]);
        let said = sim.said();
        assert_eq!(said.len(), 1);
        assert!(said[0].1 > 0.35, "spoken at {}", said[0].1);
        // A short sentence (1 s) fits before the zone.
        let mut short = trig("b", 0.27, 5);
        short.max_delay_pct = 0.2;
        let mut sim = Sim::new(braking_rule(TriggerEngine::new(
            vec![short],
            vec![QuietZone {
                start: 0.30,
                end: 0.35,
            }],
        )));
        sim.run(1, 1.0, &[]);
        assert!(sim.said()[0].1 < 0.30);
    }

    #[test]
    fn stops_speech_when_braking_starts() {
        let mut sim = Sim::new(braking_rule(TriggerEngine::new(
            vec![trig("a", 0.10, 5)],
            vec![],
        )));
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
        let mut engine = braking_rule(TriggerEngine::new(vec![], vec![]));
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
    fn a_plan_update_keeps_the_sentences_not_spoken_yet() {
        let mut sim = Sim::new(TriggerEngine::new(vec![trig("a", 0.5, 5)], vec![]));
        sim.drive(0, 100, 1, 1.0, &[]);
        let mut said = trig("say-1", f64::NAN, 9);
        said.max_delay_pct = 0.3;
        sim.engine.inject(said, true);
        // The plan changes before the next sample: a new engine.
        let mut engine = TriggerEngine::new(vec![trig("a", 0.5, 5), trig("b", 0.6, 5)], vec![]);
        engine.carry_state_from(&sim.engine);
        sim.engine = engine;
        sim.drive(100, 200, 1, 1.0, &[]);
        let ids: Vec<String> = sim.said().into_iter().map(|x| x.2).collect();
        assert_eq!(ids, vec!["say-1"]);
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
