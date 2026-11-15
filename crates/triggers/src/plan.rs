//! Plan de consignes : format de fichier (TOML) et résolution des positions.
//!
//! ```toml
//! [reglages]
//! marge_silence_m = 30          # silence à partir de 30 m avant le freinage
//! silence_jusqu_a = "apex"      # "relachement", "apex" ou "sortie"
//!
//! [[consigne]]
//! virage = "T4"                 # virage de la carte de la piste
//! ancre = "freinage"            # "freinage", "entree", "apex" ou "sortie"
//! decalage_m = -250             # 250 m avant l'ancre
//! texte = "Épingle : freine au panneau de cent."
//! priorite = 7                  # 0 à 9
//! repetition = "chaque_tour"    # "une_fois", "chaque_tour" ou { tours = 3 }
//! condition = { delta_au_dessus = 0.2 }
//!
//! [[consigne]]
//! position = 0.02               # position absolue (LapDistPct), sans virage
//! texte = "Nouveau tour."
//! ```

use analysis::{CornerMetrics, TrackModel};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PlanError {
    #[error("lecture du plan : {0}")]
    Io(#[from] std::io::Error),
    #[error("plan invalide : {0}")]
    Parse(#[from] toml::de::Error),
    #[error("consigne « {id} » : {msg}")]
    Invalid { id: String, msg: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum Anchor {
    /// Début du freinage de la référence (début du virage s'il n'y a pas de freinage).
    #[default]
    Freinage,
    /// Début géométrique du virage.
    Entree,
    /// Point de vitesse minimale.
    Apex,
    /// Fin géométrique du virage.
    Sortie,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum QuietUntil {
    /// Jusqu'au relâchement du frein.
    Relachement,
    /// Jusqu'au point de vitesse minimale.
    #[default]
    Apex,
    /// Jusqu'à la fin du virage.
    Sortie,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum Repeat {
    UneFois,
    #[default]
    ChaqueTour,
    /// Au plus `tours` fois.
    Tours(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Condition {
    VitesseAuDessusKmh(f64),
    VitesseEnDessousKmh(f64),
    /// Delta live face à la référence au-dessus de N secondes (en retard).
    DeltaAuDessus(f64),
    /// Delta live en dessous de N secondes (en avance si négatif).
    DeltaEnDessous(f64),
    /// À partir du tour N.
    TourMinimum(i32),
}

impl Condition {
    pub fn holds(&self, speed_mps: f32, live_delta: Option<f64>, lap: i32) -> bool {
        match *self {
            Condition::VitesseAuDessusKmh(v) => speed_mps as f64 * 3.6 > v,
            Condition::VitesseEnDessousKmh(v) => (speed_mps as f64 * 3.6) < v,
            Condition::DeltaAuDessus(d) => live_delta.is_some_and(|x| x > d),
            Condition::DeltaEnDessous(d) => live_delta.is_some_and(|x| x < d),
            Condition::TourMinimum(n) => lap >= n,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, Default)]
pub struct PlanSettings {
    /// Marge de silence avant le point de freinage, m.
    pub marge_silence_m: Option<f64>,
    pub silence_jusqu_a: Option<QuietUntil>,
    /// Distance maximale après la position prévue pour dire une consigne, m.
    pub retard_max_m: Option<f64>,
}

/// Une consigne du plan.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct TriggerSpec {
    /// Identifiant (facultatif) ; une consigne de même id est remplacée.
    pub id: Option<String>,
    /// Phrase dite au pilote : courte, en français, sans mise en forme.
    pub texte: String,
    /// Virage de la carte (« T5 »). Ignoré si `position` est donnée.
    pub virage: Option<String>,
    /// Point du virage servant de repère (défaut : freinage).
    pub ancre: Option<Anchor>,
    /// Décalage par rapport à l'ancre, m ; négatif = avant (défaut : -150).
    pub decalage_m: Option<f64>,
    /// Position absolue sur le tour (LapDistPct, 0 à 1).
    pub position: Option<f64>,
    /// Priorité 0 à 9 (défaut : 5).
    pub priorite: Option<u8>,
    /// Répétition (défaut : chaque tour).
    pub repetition: Option<Repeat>,
    /// Condition facultative, évaluée au passage de la position.
    pub condition: Option<Condition>,
    /// Expire après N tours à compter de l'activation du plan.
    pub expire_apres_tours: Option<u32>,
    /// Distance maximale après la position prévue pour dire la consigne, m.
    pub retard_max_m: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, Default)]
pub struct Plan {
    #[serde(default)]
    pub reglages: PlanSettings,
    #[serde(default, rename = "consigne")]
    pub consignes: Vec<TriggerSpec>,
}

impl Plan {
    pub fn parse(text: &str) -> Result<Self, PlanError> {
        Ok(toml::from_str(text)?)
    }

    pub fn load(path: impl AsRef<std::path::Path>) -> Result<Self, PlanError> {
        Self::parse(&std::fs::read_to_string(path)?)
    }
}

/// Consigne résolue, positions en pct.
#[derive(Debug, Clone, PartialEq)]
pub struct Trigger {
    pub id: String,
    pub text: String,
    pub at_pct: f64,
    pub corner: Option<String>,
    pub priority: u8,
    pub repeat: Repeat,
    pub condition: Option<Condition>,
    pub expire_after_laps: Option<u32>,
    /// Retard maximal toléré, en pct de tour.
    pub max_delay_pct: f64,
}

/// Zone où le coach se tait, en pct (`start > end` : chevauche la ligne).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QuietZone {
    pub start: f64,
    pub end: f64,
}

impl QuietZone {
    pub fn contains(&self, pct: f64) -> bool {
        let p = pct.rem_euclid(1.0);
        if self.start <= self.end {
            p >= self.start && p <= self.end
        } else {
            p >= self.start || p <= self.end
        }
    }
}

const DEFAULT_OFFSET_M: f64 = -150.0;
const DEFAULT_QUIET_LEAD_M: f64 = 30.0;
const DEFAULT_MAX_DELAY_M: f64 = 150.0;

/// Carte nécessaire à la résolution : la piste et les métriques de la référence.
pub struct TrackContext<'a> {
    pub model: &'a TrackModel,
    pub reference: &'a [CornerMetrics],
}

fn wrap(p: f64) -> f64 {
    p.rem_euclid(1.0)
}

impl<'a> TrackContext<'a> {
    fn corner_index(&self, label: &str) -> Option<usize> {
        self.model
            .corners
            .iter()
            .position(|c| c.label().eq_ignore_ascii_case(label.trim()))
    }

    fn anchor_pct(&self, i: usize, anchor: Anchor) -> f64 {
        let c = &self.model.corners[i];
        let m = self.reference.get(i);
        match anchor {
            Anchor::Freinage => m.and_then(|m| m.brake_point_pct).unwrap_or(c.start_pct),
            Anchor::Entree => c.start_pct,
            Anchor::Apex => m.map(|m| m.min_speed_pct).unwrap_or(c.apex_pct),
            Anchor::Sortie => c.end_pct,
        }
    }

    /// Zones de silence : de `marge` avant le freinage jusqu'à l'apex (par défaut),
    /// pour chaque virage où la référence freine.
    pub fn quiet_zones(&self, settings: &PlanSettings) -> Vec<QuietZone> {
        let lead = settings.marge_silence_m.unwrap_or(DEFAULT_QUIET_LEAD_M) / self.model.length_m;
        let until = settings.silence_jusqu_a.unwrap_or_default();
        self.model
            .corners
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                let m = self.reference.get(i)?;
                let brake = m.brake_point_pct?;
                let end = match until {
                    QuietUntil::Relachement => m.brake_release_pct.unwrap_or(m.min_speed_pct),
                    QuietUntil::Apex => m.min_speed_pct,
                    QuietUntil::Sortie => c.end_pct,
                };
                Some(QuietZone {
                    start: wrap(brake - lead),
                    end,
                })
            })
            .collect()
    }
}

/// Résout les consignes du plan. Sans carte de piste, seules les consignes à
/// position absolue sont acceptées.
pub fn resolve(plan: &Plan, track: Option<&TrackContext>) -> Result<Vec<Trigger>, PlanError> {
    let length = track.map(|t| t.model.length_m);
    plan.consignes
        .iter()
        .enumerate()
        .map(|(n, spec)| {
            let id = spec.id.clone().unwrap_or_else(|| format!("c{}", n + 1));
            let invalid = |msg: String| PlanError::Invalid {
                id: id.clone(),
                msg,
            };
            if spec.texte.trim().is_empty() {
                return Err(invalid("texte vide".into()));
            }
            let (at_pct, corner) = match (spec.position, &spec.virage) {
                (Some(p), _) => {
                    if !(0.0..1.0).contains(&p) {
                        return Err(invalid(format!("position {p} hors de [0, 1)")));
                    }
                    (p, spec.virage.clone())
                }
                (None, Some(label)) => {
                    let t = track.ok_or_else(|| {
                        invalid("virage indiqué mais aucune référence chargée".into())
                    })?;
                    let i = t
                        .corner_index(label)
                        .ok_or_else(|| invalid(format!("virage inconnu : {label}")))?;
                    let base = t.anchor_pct(i, spec.ancre.unwrap_or_default());
                    let offset = spec.decalage_m.unwrap_or(DEFAULT_OFFSET_M) / t.model.length_m;
                    (wrap(base + offset), Some(t.model.corners[i].label()))
                }
                (None, None) => return Err(invalid("ni « virage » ni « position »".into())),
            };
            let max_delay_m = spec
                .retard_max_m
                .or(plan.reglages.retard_max_m)
                .unwrap_or(DEFAULT_MAX_DELAY_M);
            Ok(Trigger {
                id: id.clone(),
                text: spec.texte.trim().to_string(),
                at_pct,
                corner,
                priority: spec.priorite.unwrap_or(5).min(9),
                repeat: spec.repetition.unwrap_or_default(),
                condition: spec.condition,
                expire_after_laps: spec.expire_apres_tours,
                // Sans longueur de piste connue : 3 % du tour.
                max_delay_pct: length.map(|l| max_delay_m / l).unwrap_or(0.03),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use analysis::Analyzer;
    use telemetry::synthetic::SyntheticTrack;
    use telemetry::{CornerDetectionParams, Lap, LapFlags};

    fn analyzer() -> Analyzer {
        let track = SyntheticTrack::demo();
        let samples = track.simulate_lap(1.0, 0.0, 0.0);
        let lap = Lap {
            number: 1,
            start_time: 0.0,
            lap_time: samples.len() as f64 / 60.0,
            flags: LapFlags {
                complete: true,
                ..Default::default()
            },
            samples,
        };
        Analyzer::new(
            &lap,
            Some(track.length_m()),
            &CornerDetectionParams::default(),
        )
    }

    #[test]
    fn parses_full_plan() {
        let plan = Plan::parse(
            r#"
            [reglages]
            marge_silence_m = 40
            silence_jusqu_a = "sortie"

            [[consigne]]
            id = "t4"
            virage = "T4"
            ancre = "apex"
            decalage_m = -300
            texte = "  Épingle.  "
            priorite = 12
            repetition = { tours = 3 }
            condition = { delta_au_dessus = 0.2 }
            expire_apres_tours = 5

            [[consigne]]
            position = 0.5
            texte = "Mi-tour."
            repetition = "une_fois"
            "#,
        )
        .unwrap();
        assert_eq!(plan.consignes.len(), 2);
        assert_eq!(plan.reglages.silence_jusqu_a, Some(QuietUntil::Sortie));
        let a = analyzer();
        let ctx = TrackContext {
            model: &a.model,
            reference: &a.reference_metrics,
        };
        let t = resolve(&plan, Some(&ctx)).unwrap();
        assert_eq!(t[0].text, "Épingle.");
        assert_eq!(t[0].priority, 9, "priorité bornée à 9");
        assert_eq!(t[0].repeat, Repeat::Tours(3));
        assert_eq!(t[0].condition, Some(Condition::DeltaAuDessus(0.2)));
        let apex = a.reference_metrics[3].min_speed_pct;
        let expected = apex - 300.0 / a.model.length_m;
        assert!((t[0].at_pct - expected).abs() < 1e-9);
        assert_eq!(t[1].id, "c2");
        assert_eq!(t[1].at_pct, 0.5);
    }

    #[test]
    fn brake_anchor_uses_reference_brake_point() {
        let a = analyzer();
        let ctx = TrackContext {
            model: &a.model,
            reference: &a.reference_metrics,
        };
        let plan =
            Plan::parse("[[consigne]]\nvirage = \"t1\"\ndecalage_m = 0\ntexte = \"x\"").unwrap();
        let t = resolve(&plan, Some(&ctx)).unwrap();
        assert_eq!(Some(t[0].at_pct), a.reference_metrics[0].brake_point_pct);
        assert_eq!(t[0].corner.as_deref(), Some("T1"));
    }

    #[test]
    fn invalid_plans_are_reported() {
        let a = analyzer();
        let ctx = TrackContext {
            model: &a.model,
            reference: &a.reference_metrics,
        };
        for (src, needle) in [
            (
                "[[consigne]]\nvirage = \"T42\"\ntexte = \"x\"",
                "virage inconnu",
            ),
            ("[[consigne]]\ntexte = \"x\"", "ni « virage »"),
            ("[[consigne]]\nposition = 1.5\ntexte = \"x\"", "hors de"),
            ("[[consigne]]\nposition = 0.1\ntexte = \" \"", "texte vide"),
        ] {
            let err = resolve(&Plan::parse(src).unwrap(), Some(&ctx)).unwrap_err();
            assert!(err.to_string().contains(needle), "{err}");
        }
        assert!(Plan::parse("[[consigne]]\nposition = \"a\"").is_err());
    }

    #[test]
    fn quiet_zones_cover_braking_corners() {
        let a = analyzer();
        let ctx = TrackContext {
            model: &a.model,
            reference: &a.reference_metrics,
        };
        let zones = ctx.quiet_zones(&PlanSettings::default());
        let braking: Vec<_> = a
            .reference_metrics
            .iter()
            .filter(|m| m.brake_point_pct.is_some())
            .collect();
        assert_eq!(zones.len(), braking.len());
        for (z, m) in zones.iter().zip(&braking) {
            assert!(z.contains(m.brake_point_pct.unwrap()));
            assert!(z.contains(m.min_speed_pct));
        }
    }
}
