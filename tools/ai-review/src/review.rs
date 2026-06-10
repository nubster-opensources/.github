use std::fmt::Write as _;

use crate::types::{
    Agent, Finding, FindingVerdict, ReviewResponse, Severity, SynthFinding, Verdict,
};

/// Findings that become inline PR comments: critical severity AND specific line.
pub fn inline_findings(response: &ReviewResponse) -> Vec<&Finding> {
    response
        .findings
        .iter()
        .filter(|f| f.severity == Severity::Critical && f.line > 0)
        .collect()
}

/// Findings that go into the global comment: minor OR no specific line.
pub fn global_findings(response: &ReviewResponse) -> Vec<&Finding> {
    response
        .findings
        .iter()
        .filter(|f| !(f.severity == Severity::Critical && f.line > 0))
        .collect()
}

/// Render the global markdown comment (with hidden upsert marker).
pub fn render_global_comment(
    response: &ReviewResponse,
    file_count: usize,
    model: &str,
    truncated: bool,
    marker: &str,
    label: &str,
) -> String {
    let mut md = format!("{marker}\n## {label} IA\n\n");

    write!(md, "### Vue d'ensemble\n{}\n\n", response.summary).unwrap();

    if !response.strengths.is_empty() {
        md.push_str("### Points forts\n");
        for s in &response.strengths {
            writeln!(md, "- {s}").unwrap();
        }
        md.push('\n');
    }

    let globals = global_findings(response);
    if !globals.is_empty() {
        md.push_str("### Suggestions\n");
        for f in globals {
            writeln!(md, "- **{}** : {}", f.file, f.message).unwrap();
        }
        md.push('\n');
    }

    if !response.security.starts_with("N/A") {
        write!(md, "### Sécurité\n{}\n\n", response.security).unwrap();
    }

    md.push_str("---\n");
    if truncated {
        md.push_str("⚠️ *Diff tronqué (trop volumineux) — review partielle.*  \n");
    }
    write!(md, "*Modèle : {model} · Diff : {file_count} fichier(s)*").unwrap();

    md
}

pub fn has_bot_marker(body: &str, marker: &str) -> bool {
    body.contains(marker)
}

/// Input bundle for [`render_team_comment`], grouped to keep the argument count low.
pub struct TeamCommentView<'a> {
    pub executive_summary: &'a str,
    pub strengths: &'a [String],
    pub scored: &'a [(SynthFinding, FindingVerdict)],
    pub verdict: Verdict,
    pub file_count: usize,
    pub agents_ok: &'a [Agent],
    pub agents_failed: &'a [Agent],
    pub raw_count: usize,
    pub dedup_count: usize,
    pub capped: usize,
    pub model: &'a str,
}

/// Renders the team-mode global comment (with hidden upsert marker).
#[must_use]
pub fn render_team_comment(view: &TeamCommentView) -> String {
    let mut md = "<!-- ai-team-bot -->\n## Team Review IA\n\n".to_string();

    let verdict_line = match view.verdict {
        Verdict::Ship => "**Verdict : SHIP ✅**",
        Verdict::NeedsWork => "**Verdict : NEEDS_WORK ⚠️**",
        Verdict::Discuss => "**Verdict : DISCUSS 💬**",
    };
    writeln!(md, "{verdict_line}\n").unwrap();
    writeln!(md, "### Vue d'ensemble\n{}\n", view.executive_summary).unwrap();

    if !view.strengths.is_empty() {
        md.push_str("### Points forts\n");
        for s in view.strengths {
            writeln!(md, "- {s}").unwrap();
        }
        md.push('\n');
    }

    let confirmed: Vec<&(SynthFinding, FindingVerdict)> =
        view.scored.iter().filter(|(_, v)| !v.contested).collect();
    let contested: Vec<&(SynthFinding, FindingVerdict)> =
        view.scored.iter().filter(|(_, v)| v.contested).collect();

    if !confirmed.is_empty() {
        md.push_str("### Findings confirmés\n");
        for (f, _) in &confirmed {
            writeln!(md, "{}", render_finding_line(f)).unwrap();
        }
        md.push('\n');
    }

    if !contested.is_empty() {
        md.push_str("### ⚠️ Contestés (vérification adversariale)\n");
        for (f, v) in &contested {
            writeln!(md, "{}", render_finding_line(f)).unwrap();
            if let Some(reason) = v.reasons.first() {
                writeln!(md, "  - _{reason}_").unwrap();
            }
        }
        md.push('\n');
    }

    if confirmed.is_empty() && contested.is_empty() {
        md.push_str("Aucun problème remonté par les agents.\n\n");
    }

    md.push_str("---\n");
    let ok_labels: Vec<&str> = view.agents_ok.iter().map(|a| a.label()).collect();
    write!(md, "*Agents : {}", ok_labels.join(", ")).unwrap();
    if !view.agents_failed.is_empty() {
        let failed_labels: Vec<&str> = view.agents_failed.iter().map(|a| a.label()).collect();
        write!(md, " · échoués : {}", failed_labels.join(", ")).unwrap();
    }
    writeln!(md).unwrap();
    write!(
        md,
        "Findings : {} bruts → {} après fusion",
        view.raw_count, view.dedup_count
    )
    .unwrap();
    if view.capped > 0 {
        write!(md, " → {} non vérifiés (plafond)", view.capped).unwrap();
    }
    writeln!(md).unwrap();
    write!(
        md,
        "Modèle : {} · Diff : {} fichier(s)*",
        view.model, view.file_count
    )
    .unwrap();

    md
}

fn render_finding_line(f: &SynthFinding) -> String {
    let sev = match f.severity {
        Severity::Critical => "🔴",
        Severity::Minor => "🟡",
    };
    let location = if f.line > 0 {
        format!("{}:{}", f.file, f.line)
    } else {
        f.file.clone()
    };
    let sources = if f.sources.is_empty() {
        String::new()
    } else {
        format!(" _(via {})_", f.sources.join(", "))
    };
    format!(
        "- {sev} `{}` **{}** : {}{}",
        f.category.label(),
        location,
        f.message,
        sources
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Finding, ReviewResponse, Severity};

    fn make_response() -> ReviewResponse {
        ReviewResponse {
            summary: "Adds manifest parsing.".into(),
            strengths: vec!["Good tests".into()],
            findings: vec![
                Finding {
                    file: "src/a.rs".into(),
                    line: 10,
                    severity: Severity::Critical,
                    message: "unwrap".into(),
                },
                Finding {
                    file: "src/b.rs".into(),
                    line: 0,
                    severity: Severity::Critical,
                    message: "complexity".into(),
                },
                Finding {
                    file: "src/c.rs".into(),
                    line: 5,
                    severity: Severity::Minor,
                    message: "naming".into(),
                },
            ],
            security: "No issues.".into(),
        }
    }

    #[test]
    fn routes_critical_with_line_to_inline() {
        let r = make_response();
        let inline = inline_findings(&r);
        assert_eq!(inline.len(), 1);
        assert_eq!(inline[0].file, "src/a.rs");
    }

    #[test]
    fn routes_minor_and_no_line_to_global() {
        let r = make_response();
        let global = global_findings(&r);
        assert_eq!(global.len(), 2);
    }

    #[test]
    fn renders_marker_in_global_comment() {
        let r = make_response();
        let comment = render_global_comment(
            &r,
            3,
            "codestral-latest",
            false,
            "<!-- ai-review-bot -->",
            "Code Review",
        );
        assert!(comment.starts_with("<!-- ai-review-bot -->"));
        assert!(comment.contains("Code Review IA"));
        assert!(comment.contains("Adds manifest parsing."));
        assert!(comment.contains("Points forts"));
        assert!(comment.contains("Sécurité"));
        assert!(comment.contains("codestral-latest"));
    }

    #[test]
    fn renders_security_mode_marker() {
        let r = make_response();
        let comment = render_global_comment(
            &r,
            2,
            "codestral-latest",
            false,
            "<!-- ai-security-bot -->",
            "Security Review",
        );
        assert!(comment.starts_with("<!-- ai-security-bot -->"));
        assert!(comment.contains("Security Review IA"));
    }

    #[test]
    fn renders_truncation_warning_when_truncated() {
        let r = make_response();
        let comment = render_global_comment(
            &r,
            50,
            "codestral-latest",
            true,
            "<!-- ai-review-bot -->",
            "Code Review",
        );
        assert!(comment.contains("Diff tronqué"));
    }

    #[test]
    fn detects_bot_marker() {
        assert!(has_bot_marker(
            "<!-- ai-review-bot -->\n## Review IA",
            "<!-- ai-review-bot -->"
        ));
        assert!(!has_bot_marker(
            "Normal comment body",
            "<!-- ai-review-bot -->"
        ));
        assert!(has_bot_marker(
            "<!-- ai-security-bot -->\n## Security",
            "<!-- ai-security-bot -->"
        ));
    }

    #[test]
    fn renders_team_comment_with_verdict_and_sections() {
        use crate::types::Category;

        let scored = vec![
            (
                SynthFinding {
                    file: "a.rs".to_string(),
                    line: 10,
                    severity: Severity::Critical,
                    category: Category::Bug,
                    message: "panic in handler".to_string(),
                    message_fr: "panique dans le handler".to_string(),
                    sources: vec!["correctness".to_string()],
                },
                FindingVerdict {
                    contested: false,
                    reasons: vec![],
                    reasons_fr: vec![],
                },
            ),
            (
                SynthFinding {
                    file: "b.rs".to_string(),
                    line: 0,
                    severity: Severity::Minor,
                    category: Category::Design,
                    message: "tight coupling".to_string(),
                    message_fr: "couplage fort".to_string(),
                    sources: vec![],
                },
                FindingVerdict {
                    contested: true,
                    reasons: vec!["already handled elsewhere".to_string()],
                    reasons_fr: vec!["deja gere ailleurs".to_string()],
                },
            ),
        ];
        let ok = [Agent::Correctness, Agent::Security];
        let failed = [Agent::Performance];
        let strengths = ["clear separation".to_string()];
        let view = TeamCommentView {
            executive_summary: "Adds the handler.",
            strengths: &strengths,
            scored: &scored,
            verdict: Verdict::NeedsWork,
            file_count: 3,
            agents_ok: &ok,
            agents_failed: &failed,
            raw_count: 5,
            dedup_count: 2,
            capped: 0,
            model: "codestral-latest + mistral-large-latest",
        };
        let md = render_team_comment(&view);

        assert!(md.starts_with("<!-- ai-team-bot -->"));
        assert!(md.contains("NEEDS_WORK"));
        assert!(md.contains("Findings confirmés"));
        assert!(md.contains("panic in handler"));
        assert!(md.contains("Contestés"));
        assert!(md.contains("already handled elsewhere"));
        assert!(md.contains("échoués : Performance"));
        assert!(md.contains("5 bruts → 2 après fusion"));
    }
}
