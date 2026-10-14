//! Matching of confirmed findings to expected bugs.

use ai_review::SynthFinding;

use crate::case::ExpectedBug;

/// Outcome of matching the confirmed findings of one case against its expected bugs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CaseScore {
    /// Indexes of the expected bugs a confirmed finding matched.
    pub found: Vec<usize>,
    /// Indexes of the expected bugs no finding matched.
    pub missed: Vec<usize>,
    /// Confirmed findings matching no expected bug.
    pub false_alerts: usize,
    /// Confirmed findings matching only bugs another finding already took.
    pub duplicates: usize,
}

/// Matches confirmed findings to expected bugs with a maximum bipartite matching.
#[must_use]
pub fn match_findings(
    findings: &[SynthFinding],
    expected: &[ExpectedBug],
    window: u32,
) -> CaseScore {
    let eligibility: Vec<Vec<usize>> = findings
        .iter()
        .map(|finding| {
            expected
                .iter()
                .enumerate()
                .filter(|(_, bug)| is_eligible(finding, bug, window))
                .map(|(bug_index, _)| bug_index)
                .collect()
        })
        .collect();

    let mut owner_of_bug: Vec<Option<usize>> = vec![None; expected.len()];
    for finding_index in 0..findings.len() {
        let mut visited = vec![false; expected.len()];
        try_augment(finding_index, &eligibility, &mut owner_of_bug, &mut visited);
    }

    let mut score = CaseScore::default();
    for (bug_index, owner) in owner_of_bug.iter().enumerate() {
        if owner.is_some() {
            score.found.push(bug_index);
        } else {
            score.missed.push(bug_index);
        }
    }
    let mut is_matched = vec![false; findings.len()];
    for owner in owner_of_bug.iter().flatten() {
        is_matched[*owner] = true;
    }
    for (finding_index, bugs) in eligibility.iter().enumerate() {
        if is_matched[finding_index] {
            continue;
        }
        if bugs.is_empty() {
            score.false_alerts += 1;
        } else {
            score.duplicates += 1;
        }
    }
    score
}

fn try_augment(
    finding_index: usize,
    eligibility: &[Vec<usize>],
    owner_of_bug: &mut [Option<usize>],
    visited: &mut [bool],
) -> bool {
    for &bug_index in &eligibility[finding_index] {
        if visited[bug_index] {
            continue;
        }
        visited[bug_index] = true;
        let can_take = match owner_of_bug[bug_index] {
            None => true,
            Some(current_owner) => try_augment(current_owner, eligibility, owner_of_bug, visited),
        };
        if can_take {
            owner_of_bug[bug_index] = Some(finding_index);
            return true;
        }
    }
    false
}

fn is_eligible(finding: &SynthFinding, bug: &ExpectedBug, window: u32) -> bool {
    if finding.file != bug.file || finding.line == 0 {
        return false;
    }
    let lowest = bug.first_line.saturating_sub(window);
    let highest = bug.last_line.saturating_add(window);
    if finding.line < lowest || finding.line > highest {
        return false;
    }
    let message = finding.message.to_lowercase();
    let message_fr = finding.message_fr.to_lowercase();
    bug.keywords.iter().any(|keyword| {
        let keyword = keyword.to_lowercase();
        message.contains(&keyword) || message_fr.contains(&keyword)
    })
}

#[cfg(test)]
mod tests {
    use ai_review::SynthFinding;

    use super::*;
    use crate::case::ExpectedBug;

    fn finding(file: &str, line: u32, message: &str) -> SynthFinding {
        serde_json::from_value(serde_json::json!({
            "file": file, "line": line, "severity": "critical", "category": "bug", "message": message
        }))
        .expect("finding")
    }

    fn bug(file: &str, first_line: u32, last_line: u32, keywords: &[&str]) -> ExpectedBug {
        ExpectedBug {
            file: file.to_string(),
            first_line,
            last_line,
            keywords: keywords.iter().map(ToString::to_string).collect(),
        }
    }

    #[test]
    fn a_line_exactly_on_the_window_edge_matches() {
        let score = match_findings(
            &[finding("a.rs", 13, "may panic")],
            &[bug("a.rs", 10, 10, &["panic"])],
            3,
        );
        assert_eq!(score.found, vec![0]);
        assert_eq!(score.false_alerts, 0);
    }

    #[test]
    fn a_line_one_past_the_window_is_a_false_alert() {
        let score = match_findings(
            &[finding("a.rs", 14, "may panic")],
            &[bug("a.rs", 10, 10, &["panic"])],
            3,
        );
        assert_eq!(score.missed, vec![0]);
        assert_eq!(score.false_alerts, 1);
    }

    #[test]
    fn keywords_match_whatever_their_case() {
        let score = match_findings(
            &[finding("a.rs", 10, "May PANIC on empty input")],
            &[bug("a.rs", 10, 10, &["panic"])],
            0,
        );
        assert_eq!(score.found, vec![0]);
    }

    #[test]
    fn a_finding_on_the_right_line_with_the_wrong_subject_does_not_match() {
        let score = match_findings(
            &[finding("a.rs", 10, "naming is unclear")],
            &[bug("a.rs", 10, 10, &["panic"])],
            0,
        );
        assert_eq!(score.missed, vec![0]);
        assert_eq!(score.false_alerts, 1);
    }

    #[test]
    fn one_finding_eligible_for_two_bugs_validates_only_one() {
        let score = match_findings(
            &[finding("a.rs", 10, "panic and overflow")],
            &[
                bug("a.rs", 10, 10, &["panic"]),
                bug("a.rs", 11, 11, &["overflow"]),
            ],
            2,
        );
        assert_eq!(score.found.len(), 1);
        assert_eq!(score.missed.len(), 1);
    }

    #[test]
    fn two_findings_on_the_same_bug_count_it_once_and_flag_a_duplicate() {
        let score = match_findings(
            &[
                finding("a.rs", 10, "may panic"),
                finding("a.rs", 11, "panic again"),
            ],
            &[bug("a.rs", 10, 10, &["panic"])],
            2,
        );
        assert_eq!(score.found, vec![0]);
        assert_eq!(score.duplicates, 1);
        assert_eq!(score.false_alerts, 0);
    }

    #[test]
    fn the_matching_is_maximal_not_greedy() {
        let score = match_findings(
            &[
                finding("a.rs", 10, "panic and overflow"),
                finding("a.rs", 10, "may panic"),
            ],
            &[
                bug("a.rs", 10, 10, &["panic"]),
                bug("a.rs", 10, 10, &["overflow"]),
            ],
            0,
        );
        assert_eq!(score.found, vec![0, 1]);
        assert_eq!(score.duplicates, 0);
    }

    #[test]
    fn a_finding_without_a_line_matches_nothing() {
        let score = match_findings(
            &[finding("a.rs", 0, "may panic")],
            &[bug("a.rs", 1, 1, &["panic"])],
            5,
        );
        assert_eq!(score.false_alerts, 1);
    }

    #[test]
    fn a_case_without_expected_bugs_turns_every_finding_into_a_false_alert() {
        let score = match_findings(&[finding("a.rs", 3, "x"), finding("b.rs", 4, "y")], &[], 3);
        assert_eq!(score.false_alerts, 2);
        assert!(score.found.is_empty() && score.missed.is_empty());
    }

    #[test]
    fn the_french_message_is_searched_too() {
        let mut french = finding("a.rs", 10, "unclear");
        french.message_fr = "peut paniquer".to_string();
        let score = match_findings(&[french], &[bug("a.rs", 10, 10, &["paniquer"])], 0);
        assert_eq!(score.found, vec![0]);
    }
}
