//! Markdown summary of a benchmark measure.

use std::fmt::Write;

use crate::baseline::{Baseline, GateVerdict};
use crate::runner::BenchScore;

fn signed_gap(measured: usize, reference: usize) -> String {
    if measured >= reference {
        format!("+{}", measured - reference)
    } else {
        format!("-{}", reference - measured)
    }
}

/// Renders the verdict, the global table, the per-case table of the median run
/// and, on improvement, the suggested baseline.
#[must_use]
pub fn render_summary(score: &BenchScore, baseline: &Baseline, verdict: &GateVerdict) -> String {
    let mut out = String::new();
    let verdict_line = match verdict {
        GateVerdict::Pass => "Verdict: pass".to_string(),
        GateVerdict::Improved { .. } => {
            "Verdict: improved, a new baseline is suggested".to_string()
        }
        GateVerdict::Regression {
            found_drop,
            false_alert_rise,
        } => format!(
            "Verdict: regression (found bugs -{found_drop}, false alerts +{false_alert_rise})"
        ),
    };
    let _ = writeln!(out, "{verdict_line}\n");
    let _ = writeln!(out, "| mesure | médiane | référence | écart |");
    let _ = writeln!(out, "| --- | --- | --- | --- |");
    let _ = writeln!(
        out,
        "| bugs trouvés (sur {}) | {} | {} | {} |",
        score.expected_bugs,
        score.median_found,
        baseline.found,
        signed_gap(score.median_found, baseline.found)
    );
    let _ = writeln!(
        out,
        "| fausses alertes | {} | {} | {} |\n",
        score.median_false_alerts,
        baseline.false_alerts,
        signed_gap(score.median_false_alerts, baseline.false_alerts)
    );
    if let Some(run) = score
        .runs
        .iter()
        .find(|run| run.found == score.median_found)
    {
        let _ = writeln!(
            out,
            "| cas | bugs trouvés / attendus | ratés | fausses alertes | doublons |"
        );
        let _ = writeln!(out, "| --- | --- | --- | --- | --- |");
        for (case_id, case) in &run.per_case {
            let _ = writeln!(
                out,
                "| {case_id} | {} / {} | {} | {} | {} |",
                case.found.len(),
                case.found.len() + case.missed.len(),
                case.missed.len(),
                case.false_alerts,
                case.duplicates
            );
        }
        out.push('\n');
    }
    if let GateVerdict::Improved { suggested } = verdict {
        let _ = write!(
            out,
            "Suggested baseline:\n\n```toml\n{}```\n",
            suggested.to_toml()
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::baseline::{Baseline, GateVerdict};
    use crate::matching::CaseScore;
    use crate::runner::{BenchScore, RunScore};

    #[test]
    fn the_summary_names_the_verdict_and_each_case() {
        let per_case = vec![(
            "case-a".to_string(),
            CaseScore {
                found: vec![0],
                missed: vec![1],
                false_alerts: 2,
                duplicates: 1,
            },
        )];
        let run = RunScore {
            per_case,
            found: 1,
            false_alerts: 2,
        };
        let score = BenchScore {
            runs: vec![run],
            median_found: 1,
            median_false_alerts: 2,
            expected_bugs: 2,
        };
        let baseline = Baseline {
            window: 3,
            found: 1,
            false_alerts: 2,
            found_drop_tolerance: 2,
            false_alert_rise_tolerance: 3,
        };
        let summary = render_summary(&score, &baseline, &GateVerdict::Pass);
        assert!(summary.contains("case-a"));
        assert!(summary.contains("1 / 2"));
        assert!(summary.to_lowercase().contains("pass"));
    }
}
