use std::fmt::Write as _;

use crate::types::{Finding, ReviewResponse, Severity};

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
}
