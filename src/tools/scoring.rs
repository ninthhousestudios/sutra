pub fn normalize(raw: f64, ceiling: f64) -> f64 {
    (raw / ceiling).min(1.0)
}

pub fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

pub struct Signal {
    pub weight: f64,
    pub score: f64,
}

pub fn weighted_score(signals: &[Signal]) -> f64 {
    signals
        .iter()
        .map(|s| s.weight * s.score)
        .sum::<f64>()
        .min(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_clamps_to_one() {
        assert!((normalize(100.0, 50.0) - 1.0).abs() < f64::EPSILON);
        assert!((normalize(25.0, 50.0) - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn weighted_score_sums_and_clamps() {
        let signals = vec![
            Signal {
                weight: 0.5,
                score: 0.8,
            },
            Signal {
                weight: 0.5,
                score: 0.6,
            },
        ];
        let s = weighted_score(&signals);
        assert!((s - 0.7).abs() < f64::EPSILON);

        let extreme = vec![
            Signal {
                weight: 0.6,
                score: 1.0,
            },
            Signal {
                weight: 0.6,
                score: 1.0,
            },
        ];
        assert!((weighted_score(&extreme) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn round3_rounds_correctly() {
        assert!((round3(0.12345) - 0.123).abs() < f64::EPSILON);
        assert!((round3(0.9999) - 1.0).abs() < f64::EPSILON);
    }
}
