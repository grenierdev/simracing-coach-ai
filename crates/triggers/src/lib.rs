//! Coach trigger engine (fast loop).
//!
//! - [`plan`]: plan file (TOML), per-corner position resolution, quiet zones
//!   taken from the reference braking zones;
//! - [`engine`]: firing by position, condition, priority, expiry, hard rule
//!   "no speech in a braking zone";
//! - [`runtime`]: thread that connects telemetry, the engine and speech.

pub mod engine;
pub mod plan;
pub mod runtime;

pub use engine::{Command, StopReason, TickContext, TriggerEngine, TriggerState, TriggerView};
pub use plan::{
    Anchor, Condition, Plan, PlanError, QuietZone, Repeat, TrackContext, Trigger, TriggerSpec,
    resolve,
};
pub use runtime::{COACH_PREFIX, LogEntry, RuntimeStatus, SayWhen, SpeechOutput, TriggerRuntime};
