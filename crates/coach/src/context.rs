//! Contexte injecté au début de chaque message du pilote.

/// Où en est le pilote au moment où il parle.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PilotContext {
    pub lap: Option<i32>,
    /// Position sur le tour, 0..1.
    pub lap_pct: Option<f64>,
    pub last_corner: Option<String>,
    pub next_corner: Option<String>,
    /// Delta live face à la référence, s.
    pub live_delta: Option<f64>,
    /// Dernières consignes dites (la plus récente en dernier).
    pub recent_consignes: Vec<String>,
}

/// `[Contexte] …` puis le message du pilote.
pub fn format_prompt(ctx: &PilotContext, message: &str) -> String {
    let mut parts = vec![];
    if let Some(l) = ctx.lap {
        parts.push(format!("tour {l}"));
    }
    if let Some(p) = ctx.lap_pct {
        parts.push(format!("position {:.0} %", p * 100.0));
    }
    if let Some(c) = &ctx.last_corner {
        parts.push(format!("dernier virage {c}"));
    }
    if let Some(c) = &ctx.next_corner {
        parts.push(format!("prochain virage {c}"));
    }
    if let Some(d) = ctx.live_delta {
        parts.push(format!("delta {d:+.2} s"));
    }
    let mut out = String::from("[Contexte] ");
    out.push_str(if parts.is_empty() {
        "pas de télémétrie"
    } else {
        ""
    });
    out.push_str(&parts.join(", "));
    if !ctx.recent_consignes.is_empty() {
        out.push_str("\nDernières consignes : ");
        out.push_str(
            &ctx.recent_consignes
                .iter()
                .map(|c| format!("« {c} »"))
                .collect::<Vec<_>>()
                .join(" ; "),
        );
    }
    out.push_str("\n[Pilote] ");
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
            recent_consignes: vec!["Épingle, patiente avant les gaz.".into()],
        };
        let p = format_prompt(&ctx, " je n'ai plus de rotation au T5 ");
        assert_eq!(
            p,
            "[Contexte] tour 5, position 43 %, dernier virage T4, prochain virage T5, delta +0.31 s\n\
             Dernières consignes : « Épingle, patiente avant les gaz. »\n\
             [Pilote] je n'ai plus de rotation au T5"
        );
        assert!(
            format_prompt(&PilotContext::default(), "salut")
                .starts_with("[Contexte] pas de télémétrie")
        );
    }
}
