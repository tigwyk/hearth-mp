//! Glicko-2 (Glickman, "Example of the Glicko-2 system", 2013). Ported from the
//! FFNY relay's `accounts/rating.go`, so a ladder moved here keeps its numbers.

pub const START: f64 = 1500.0;
pub const DEVIATION_START: f64 = 350.0;
pub const VOLATILITY_START: f64 = 0.06;
/// A settled player's rating must still be able to move.
pub const DEVIATION_FLOOR: f64 = 45.0;
const SCALE: f64 = 173.7178;
const TAU: f64 = 0.5;
const EPSILON: f64 = 0.000001;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rating {
    pub rating: f64,
    pub deviation: f64,
    pub volatility: f64,
}

impl Default for Rating {
    fn default() -> Self {
        Rating {
            rating: START,
            deviation: DEVIATION_START,
            volatility: VOLATILITY_START,
        }
    }
}

fn g(phi: f64) -> f64 {
    1.0 / (1.0 + 3.0 * phi * phi / (std::f64::consts::PI * std::f64::consts::PI)).sqrt()
}

fn e(mu: f64, muj: f64, phij: f64) -> f64 {
    1.0 / (1.0 + (-g(phij) * (mu - muj)).exp())
}

impl Rating {
    /// The rating after one period with these `(opponent, score)` games
    /// (1 win, 0 loss, 0.5 draw).
    pub fn update(&self, games: &[(Rating, f64)]) -> Rating {
        let mu = (self.rating - START) / SCALE;
        let phi = self.deviation / SCALE;
        let sigma = self.volatility;
        if games.is_empty() {
            let phi = (phi * phi + sigma * sigma)
                .sqrt()
                .min(DEVIATION_START / SCALE);
            return Rating {
                deviation: phi * SCALE,
                ..*self
            };
        }
        let (mut v_inv, mut delta_sum) = (0.0, 0.0);
        for (opp, score) in games {
            let muj = (opp.rating - START) / SCALE;
            let phij = opp.deviation / SCALE;
            let ej = e(mu, muj, phij);
            let gj = g(phij);
            v_inv += gj * gj * ej * (1.0 - ej);
            delta_sum += gj * (score - ej);
        }
        let v = 1.0 / v_inv;
        let delta = v * delta_sum;

        // New volatility: the Illinois algorithm (step 5 of the paper).
        let a = (sigma * sigma).ln();
        let f = |x: f64| {
            let ex = x.exp();
            let d = phi * phi + v + ex;
            ex * (delta * delta - phi * phi - v - ex) / (2.0 * d * d) - (x - a) / (TAU * TAU)
        };
        let mut big_a = a;
        let mut big_b = if delta * delta > phi * phi + v {
            (delta * delta - phi * phi - v).ln()
        } else {
            let mut k = 1.0;
            while f(a - k * TAU) < 0.0 {
                k += 1.0;
            }
            a - k * TAU
        };
        let (mut fa, mut fb) = (f(big_a), f(big_b));
        let mut i = 0;
        while (big_b - big_a).abs() > EPSILON && i < 100 {
            let c = big_a + (big_a - big_b) * fa / (fb - fa);
            let fc = f(c);
            if fc * fb <= 0.0 {
                big_a = big_b;
                fa = fb;
            } else {
                fa /= 2.0;
            }
            big_b = c;
            fb = fc;
            i += 1;
        }
        let new_sigma = (big_a / 2.0).exp();
        let phi_star = (phi * phi + new_sigma * new_sigma).sqrt();
        let new_phi = 1.0 / (1.0 / (phi_star * phi_star) + 1.0 / v).sqrt();
        let new_mu = mu + new_phi * new_phi * delta_sum;
        Rating {
            rating: new_mu * SCALE + START,
            deviation: (new_phi * SCALE).max(DEVIATION_FLOOR),
            volatility: new_sigma,
        }
    }

    /// Both ratings after one game, `score_a` being player A's score.
    pub fn game(a: Rating, b: Rating, score_a: f64) -> (Rating, Rating) {
        (a.update(&[(b, score_a)]), b.update(&[(a, 1.0 - score_a)]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The worked example in Glickman's paper.
    #[test]
    fn matches_the_papers_example() {
        let me = Rating {
            rating: 1500.0,
            deviation: 200.0,
            volatility: 0.06,
        };
        let opp = |rating, deviation| Rating {
            rating,
            deviation,
            volatility: 0.06,
        };
        let out = me.update(&[
            (opp(1400.0, 30.0), 1.0),
            (opp(1550.0, 100.0), 0.0),
            (opp(1700.0, 300.0), 0.0),
        ]);
        assert!((out.rating - 1464.06).abs() < 0.01, "{out:?}");
        assert!((out.deviation - 151.52).abs() < 0.01, "{out:?}");
        assert!((out.volatility - 0.05999).abs() < 0.0001, "{out:?}");
    }

    #[test]
    fn winner_gains_loser_drops() {
        let (a, b) = Rating::game(Rating::default(), Rating::default(), 1.0);
        assert!(a.rating > START && b.rating < START);
    }
}
