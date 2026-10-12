//! Committed baseline of the benchmark and the gate comparing a measure to it.

use std::path::Path;

use anyhow::Context;

use crate::runner::BenchScore;

/// Reference measure and tolerances the gate compares a run against.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct Baseline {
    /// Number of lines around a bug within which a finding still validates it.
    pub window: u32,
    /// Median number of expected bugs found by the reference run.
    pub found: usize,
    /// Median number of false alerts of the reference run.
    pub false_alerts: usize,
    /// Drop of found bugs from which the gate reports a regression.
    pub found_drop_tolerance: usize,
    /// Rise of false alerts from which the gate reports a regression.
    pub false_alert_rise_tolerance: usize,
}

impl Baseline {
    /// Reads and parses the baseline file; a missing or malformed file is an error.
    ///
    /// # Errors
    /// Returns an error when the file cannot be read or is not a valid baseline.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read baseline {}", path.display()))?;
        Self::from_toml(&text).with_context(|| format!("invalid baseline {}", path.display()))
    }

    /// Parses a baseline from TOML text.
    ///
    /// # Errors
    /// Returns an error when the text is not a valid baseline.
    pub fn from_toml(text: &str) -> anyhow::Result<Self> {
        Ok(toml::from_str(text)?)
    }

    /// Serializes the baseline as TOML text.
    #[must_use]
    pub fn to_toml(&self) -> String {
        format!(
            "window = {}\nfound = {}\nfalse_alerts = {}\nfound_drop_tolerance = {}\nfalse_alert_rise_tolerance = {}\n",
            self.window,
            self.found,
            self.false_alerts,
            self.found_drop_tolerance,
            self.false_alert_rise_tolerance
        )
    }
}

/// Outcome of the comparison between a measure and the baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateVerdict {
    /// The measure stays within the tolerances.
    Pass,
    /// The measure is better than the baseline; a new baseline is suggested.
    Improved {
        /// Baseline built from the measured medians.
        suggested: Baseline,
    },
    /// The measure crosses a tolerance.
    Regression {
        /// Number of found bugs lost against the baseline.
        found_drop: usize,
        /// Number of false alerts gained against the baseline.
        false_alert_rise: usize,
    },
}

/// Compares the median of a measure to the baseline.
#[must_use]
pub fn compare(score: &BenchScore, baseline: &Baseline) -> GateVerdict {
    let found_drop = baseline.found.saturating_sub(score.median_found);
    let false_alert_rise = score
        .median_false_alerts
        .saturating_sub(baseline.false_alerts);
    if found_drop >= baseline.found_drop_tolerance
        || false_alert_rise >= baseline.false_alert_rise_tolerance
    {
        return GateVerdict::Regression {
            found_drop,
            false_alert_rise,
        };
    }
    if score.median_found > baseline.found || score.median_false_alerts < baseline.false_alerts {
        return GateVerdict::Improved {
            suggested: Baseline {
                found: score.median_found,
                false_alerts: score.median_false_alerts,
                ..baseline.clone()
            },
        };
    }
    GateVerdict::Pass
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::BenchScore;
    use std::path::Path;

    fn baseline() -> Baseline {
        Baseline {
            window: 3,
            found: 10,
            false_alerts: 4,
            found_drop_tolerance: 2,
            false_alert_rise_tolerance: 3,
        }
    }

    fn score(found: usize, false_alerts: usize) -> BenchScore {
        BenchScore {
            runs: Vec::new(),
            median_found: found,
            median_false_alerts: false_alerts,
            expected_bugs: 12,
        }
    }

    #[test]
    fn a_drop_of_one_found_bug_still_passes() {
        assert_eq!(compare(&score(9, 4), &baseline()), GateVerdict::Pass);
    }

    #[test]
    fn a_drop_of_two_found_bugs_is_a_regression() {
        assert_eq!(
            compare(&score(8, 4), &baseline()),
            GateVerdict::Regression {
                found_drop: 2,
                false_alert_rise: 0
            }
        );
    }

    #[test]
    fn a_rise_of_three_false_alerts_is_a_regression() {
        assert_eq!(
            compare(&score(10, 7), &baseline()),
            GateVerdict::Regression {
                found_drop: 0,
                false_alert_rise: 3
            }
        );
    }

    #[test]
    fn more_found_bugs_suggest_a_new_baseline() {
        let GateVerdict::Improved { suggested } = compare(&score(11, 4), &baseline()) else {
            panic!("expected an improvement");
        };
        assert_eq!(suggested.found, 11);
        assert_eq!(suggested.window, 3);
    }

    #[test]
    fn a_baseline_survives_a_toml_round_trip() {
        let text = baseline().to_toml();
        let parsed = Baseline::from_toml(&text).expect("toml");
        assert_eq!(parsed, baseline());
    }

    #[test]
    fn a_missing_baseline_is_an_error() {
        assert!(Baseline::load(Path::new("does-not-exist/baseline.toml")).is_err());
    }

    #[test]
    fn a_malformed_baseline_is_an_error() {
        assert!(Baseline::from_toml("window = \"three\"\nfound = 1").is_err());
    }
}
