//! Moteur de consignes du coach (boucle rapide).
//!
//! - [`plan`] : fichier de plan (TOML), résolution des positions par virage,
//!   zones de silence tirées des freinages de la référence ;
//! - [`engine`] : déclenchement par position, condition, priorité, expiration,
//!   règle dure « aucune parole en zone de freinage » ;
//! - [`runtime`] : thread qui relie la télémétrie, le moteur et la parole.

pub mod engine;
pub mod plan;
pub mod runtime;

pub use engine::{Command, StopReason, TickContext, TriggerEngine, TriggerState, TriggerView};
pub use plan::{
    Anchor, Condition, Plan, PlanError, QuietZone, Repeat, TrackContext, Trigger, resolve,
};
pub use runtime::{LogEntry, RuntimeStatus, SpeechOutput, TriggerRuntime};
