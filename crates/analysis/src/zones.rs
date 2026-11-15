//! Découpage du tour en zones, une par virage.
//!
//! Chaque zone va du milieu de la ligne droite précédente au milieu de la
//! suivante : les zones couvrent tout le tour sans se chevaucher, donc la somme
//! des deltas par virage égale le delta du tour.

use telemetry::Corner;

#[derive(Debug, Clone, PartialEq)]
pub struct CornerZone {
    pub corner: Corner,
    /// Début de zone, pct (inclus).
    pub start_pct: f64,
    /// Fin de zone, pct ; égale au début de la zone suivante.
    pub end_pct: f64,
}

impl CornerZone {
    /// Indices de grille de la zone, dans le sens de la marche (fin incluse).
    pub fn indices(&self, n: usize) -> Vec<usize> {
        let a = ((self.start_pct * n as f64).round() as usize) % n;
        let b = ((self.end_pct * n as f64).round() as usize) % n;
        let len = (b + n - a) % n;
        let len = if len == 0 { n } else { len };
        (0..=len).map(|k| (a + k) % n).collect()
    }

    pub fn contains(&self, pct: f64) -> bool {
        let p = pct.rem_euclid(1.0);
        if self.start_pct <= self.end_pct {
            p >= self.start_pct && p < self.end_pct
        } else {
            p >= self.start_pct || p < self.end_pct
        }
    }
}

/// Distance circulaire de `a` vers `b` dans le sens de la marche, en pct.
pub fn forward(a: f64, b: f64) -> f64 {
    (b - a).rem_euclid(1.0)
}

/// Zones des virages (dans l'ordre de la piste).
pub fn build_zones(corners: &[Corner]) -> Vec<CornerZone> {
    let m = corners.len();
    if m == 0 {
        return vec![];
    }
    if m == 1 {
        let c = &corners[0];
        let mid = (c.end_pct + forward(c.end_pct, c.start_pct) / 2.0).rem_euclid(1.0);
        return vec![CornerZone {
            corner: c.clone(),
            start_pct: mid,
            end_pct: mid,
        }];
    }
    // Frontière entre le virage i et le suivant : milieu de la ligne droite.
    let boundary = |i: usize| {
        let a = &corners[i];
        let b = &corners[(i + 1) % m];
        (a.end_pct + forward(a.end_pct, b.start_pct) / 2.0).rem_euclid(1.0)
    };
    (0..m)
        .map(|i| CornerZone {
            corner: corners[i].clone(),
            start_pct: boundary((i + m - 1) % m),
            end_pct: boundary(i),
        })
        .collect()
}
