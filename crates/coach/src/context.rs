//! Context injected at the start of every driver message.
//!
//! The block is always in English (it is read by Claude, not spoken); the
//! system prompt tells Claude which language to answer in.

/// Where the driver is when they speak.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PilotContext {
    pub lap: Option<i32>,
    /// Position on the lap, 0..1.
    pub lap_pct: Option<f64>,
    pub last_corner: Option<String>,
    pub next_corner: Option<String>,
    /// Live delta to the reference, s.
    pub live_delta: Option<f64>,
    /// Most recent cues spoken (most recent last).
    pub recent_consignes: Vec<String>,
}

/// `[Context] …` followed by the driver's message.
pub fn format_prompt(ctx: &PilotContext, message: &str) -> String {
    let mut parts = vec![];
    if let Some(l) = ctx.lap {
        parts.push(format!("lap {l}"));
    }
    if let Some(p) = ctx.lap_pct {
        parts.push(format!("position {:.0} %", p * 100.0));
    }
    if let Some(c) = &ctx.last_corner {
        parts.push(format!("last corner {c}"));
    }
    if let Some(c) = &ctx.next_corner {
        parts.push(format!("next corner {c}"));
    }
    if let Some(d) = ctx.live_delta {
        parts.push(format!("delta {d:+.2} s"));
    }
    let mut out = String::from("[Context] ");
    out.push_str(if parts.is_empty() { "no telemetry" } else { "" });
    out.push_str(&parts.join(", "));
    if !ctx.recent_consignes.is_empty() {
        out.push_str("\nRecent cues: ");
        out.push_str(
            &ctx.recent_consignes
                .iter()
                .map(|c| format!("\"{c}\""))
                .collect::<Vec<_>>()
                .join("; "),
        );
    }
    out.push_str("\n[Driver] ");
    out.push_str(message.trim());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_context_block() {
        let ctx = PilotContext {
            lap: Some(5),
            lap_pct: Some(0.432),
            last_corner: Some("T4".into()),
            next_corner: Some("T5".into()),
            live_delta: Some(0.31),
            recent_consignes: vec!["Hairpin, be patient on the throttle.".into()],
        };
        let p = format_prompt(&ctx, " no rotation in T5 ");
        assert_eq!(
            p,
            "[Context] lap 5, position 43 %, last corner T4, next corner T5, delta +0.31 s\n\
             Recent cues: \"Hairpin, be patient on the throttle.\"\n\
             [Driver] no rotation in T5"
        );
        assert!(
            format_prompt(&PilotContext::default(), "hi").starts_with("[Context] no telemetry")
        );
    }
}
