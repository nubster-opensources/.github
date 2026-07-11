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
    let mut md = format!("{marker}\n## {label}\n\n");

    // fmt::Write to String is infallible, so the Result of write!/writeln! is deliberately ignored.
    let _ = write!(md, "### Overview\n{}\n\n", response.summary);

    if !response.strengths.is_empty() {
        md.push_str("### Strengths\n");
        for s in &response.strengths {
            let _ = writeln!(md, "- {s}");
        }
        md.push('\n');
    }

    let globals = global_findings(response);
    if !globals.is_empty() {
        md.push_str("### Suggestions\n");
        for f in globals {
            let _ = writeln!(md, "- **{}**: {}", f.file, f.message);
        }
        md.push('\n');
    }

    if !response.security.starts_with("N/A") {
        let _ = write!(md, "### Security\n{}\n\n", response.security);
    }

    md.push_str("---\n");
    if truncated {
        md.push_str("⚠️ *Diff truncated (too large): partial review.*  \n");
    }
    let _ = write!(md, "*Model: {model} · Diff: {file_count} file(s)*");

    md
}

pub fn has_bot_marker(body: &str, marker: &str) -> bool {
    body.contains(marker)
}

/// Input bundle for [`render_team_comment`], grouped to keep the argument count low.
pub struct TeamCommentView<'a> {
    pub executive_summary: &'a str,
    pub executive_summary_fr: &'a str,
    pub strengths: &'a [String],
    pub strengths_fr: &'a [String],
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

/// Language of a rendered report block.
#[derive(Clone, Copy)]
enum Lang {
    En,
    Fr,
}

/// Localized section labels for one rendered language block.
struct Labels {
    summary: &'static str,
    overview: &'static str,
    strengths: &'static str,
    confirmed: &'static str,
    minor: &'static str,
    contested: &'static str,
    nothing: &'static str,
}

impl Labels {
    fn for_lang(lang: Lang) -> Self {
        match lang {
            Lang::En => Labels {
                summary: "🇬🇧 English",
                overview: "Overview",
                strengths: "Strengths",
                confirmed: "Confirmed findings",
                minor: "Minor",
                contested: "⚠️ Contested (adversarial verification)",
                nothing: "No issues reported by the agents.",
            },
            Lang::Fr => Labels {
                summary: "🇫🇷 Français",
                overview: "Vue d'ensemble",
                strengths: "Points forts",
                confirmed: "Findings confirmés",
                minor: "Mineurs",
                contested: "⚠️ Contestés (vérification adversariale)",
                nothing: "Aucun problème remonté par les agents.",
            },
        }
    }
}

/// Picks the English text, or the French text when present (English fallback).
fn pick<'a>(en: &'a str, fr: &'a str, lang: Lang) -> &'a str {
    match lang {
        Lang::Fr if !fr.is_empty() => fr,
        Lang::En | Lang::Fr => en,
    }
}

/// Picks the English list, or the French list when non-empty (English fallback).
fn pick_list<'a>(en: &'a [String], fr: &'a [String], lang: Lang) -> &'a [String] {
    match lang {
        Lang::Fr if !fr.is_empty() => fr,
        Lang::En | Lang::Fr => en,
    }
}

/// Renders the team-mode global comment (with hidden upsert marker).
#[must_use]
pub fn render_team_comment(view: &TeamCommentView) -> String {
    let confirmed_count = view.scored.iter().filter(|(_, v)| !v.contested).count();
    let contested_count = view.scored.iter().filter(|(_, v)| v.contested).count();
    let verdict_badge = match view.verdict {
        Verdict::Ship => "SHIP ✅",
        Verdict::NeedsWork => "NEEDS_WORK ⚠️",
        Verdict::Discuss => "DISCUSS 💬",
    };

    let mut md = "<!-- ai-team-bot -->\n## Team Review\n\n".to_string();
    let _ = writeln!(
        md,
        "**Verdict: {verdict_badge}** · {confirmed_count} confirmed · {contested_count} contested\n"
    );

    md.push_str(&render_lang_block(view, Lang::En));
    md.push_str(&render_lang_block(view, Lang::Fr));

    md.push_str("---\n");
    let ok_labels: Vec<&str> = view.agents_ok.iter().map(|a| a.label()).collect();
    let _ = write!(md, "*Agents: {}", ok_labels.join(", "));
    if !view.agents_failed.is_empty() {
        let failed_labels: Vec<&str> = view.agents_failed.iter().map(|a| a.label()).collect();
        let _ = write!(md, " · failed: {}", failed_labels.join(", "));
    }
    let _ = writeln!(md);
    let _ = write!(
        md,
        "Findings: {} raw -> {} merged",
        view.raw_count, view.dedup_count
    );
    if view.capped > 0 {
        let _ = write!(md, " -> {} unverified (cap)", view.capped);
    }
    let _ = writeln!(md);
    let _ = write!(
        md,
        "Model: {} · Diff: {} file(s)*",
        view.model, view.file_count
    );

    md
}

/// Renders one language block as a `<details>` section (French opens by default).
fn render_lang_block(view: &TeamCommentView, lang: Lang) -> String {
    let labels = Labels::for_lang(lang);
    let open = if matches!(lang, Lang::Fr) {
        " open"
    } else {
        ""
    };
    let mut md = String::new();
    let _ = writeln!(md, "<details{open}><summary>{}</summary>\n", labels.summary);

    let summary = pick(view.executive_summary, view.executive_summary_fr, lang);
    let _ = writeln!(md, "**{}**\n{summary}\n", labels.overview);

    let strengths = pick_list(view.strengths, view.strengths_fr, lang);
    if !strengths.is_empty() {
        let _ = writeln!(md, "**{}**", labels.strengths);
        md.push('\n');
        for s in strengths {
            let _ = writeln!(md, "- {s}");
        }
        md.push('\n');
    }

    let confirmed_critical: Vec<&(SynthFinding, FindingVerdict)> = view
        .scored
        .iter()
        .filter(|(f, v)| !v.contested && f.severity == Severity::Critical)
        .collect();
    let confirmed_minor: Vec<&(SynthFinding, FindingVerdict)> = view
        .scored
        .iter()
        .filter(|(f, v)| !v.contested && f.severity == Severity::Minor)
        .collect();
    let contested: Vec<&(SynthFinding, FindingVerdict)> =
        view.scored.iter().filter(|(_, v)| v.contested).collect();

    if !confirmed_critical.is_empty() {
        let _ = writeln!(md, "**{}**", labels.confirmed);
        md.push('\n');
        for (f, _) in &confirmed_critical {
            let _ = writeln!(md, "{}", render_finding_line(f, lang));
        }
        md.push('\n');
    }

    if !confirmed_minor.is_empty() {
        let _ = writeln!(
            md,
            "<details><summary>{} ({})</summary>\n",
            labels.minor,
            confirmed_minor.len()
        );
        for (f, _) in &confirmed_minor {
            let _ = writeln!(md, "{}", render_finding_line(f, lang));
        }
        md.push_str("\n</details>\n\n");
    }

    if !contested.is_empty() {
        let _ = writeln!(md, "**{}**", labels.contested);
        md.push('\n');
        for (f, v) in &contested {
            let _ = writeln!(md, "{}", render_finding_line(f, lang));
            let reasons = pick_list(&v.reasons, &v.reasons_fr, lang);
            if let Some(reason) = reasons.first() {
                let _ = writeln!(md, "  - _{reason}_");
            }
        }
        md.push('\n');
    }

    if view.scored.is_empty() {
        let _ = writeln!(md, "{}\n", labels.nothing);
    }

    md.push_str("</details>\n\n");
    md
}

fn render_finding_line(f: &SynthFinding, lang: Lang) -> String {
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
    let message = pick(&f.message, &f.message_fr, lang);
    format!(
        "- {sev} `{}` **{}**: {message}{sources}",
        f.category.label(),
        location
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
        assert!(comment.contains("## Code Review\n"));
        assert!(comment.contains("Adds manifest parsing."));
        assert!(comment.contains("Strengths"));
        assert!(comment.contains("Security"));
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
        assert!(comment.contains("## Security Review\n"));
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
        assert!(comment.contains("Diff truncated"));
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
    fn renders_bilingual_team_comment_with_collapsed_minors() {
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
                    line: 3,
                    severity: Severity::Minor,
                    category: Category::Design,
                    message: "tight coupling".to_string(),
                    message_fr: "couplage fort".to_string(),
                    sources: vec![],
                },
                FindingVerdict {
                    contested: false,
                    reasons: vec![],
                    reasons_fr: vec![],
                },
            ),
            (
                SynthFinding {
                    file: "c.rs".to_string(),
                    line: 0,
                    severity: Severity::Minor,
                    category: Category::Design,
                    message: "naming".to_string(),
                    message_fr: "nommage".to_string(),
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
        let strengths_fr = ["separation claire".to_string()];
        let view = TeamCommentView {
            executive_summary: "Adds the handler.",
            executive_summary_fr: "Ajoute le handler.",
            strengths: &strengths,
            strengths_fr: &strengths_fr,
            scored: &scored,
            verdict: Verdict::NeedsWork,
            file_count: 3,
            agents_ok: &ok,
            agents_failed: &failed,
            raw_count: 5,
            dedup_count: 3,
            capped: 0,
            model: "codestral-latest + mistral-large-latest",
        };
        let md = render_team_comment(&view);

        assert!(md.starts_with("<!-- ai-team-bot -->"));
        assert!(md.contains("NEEDS_WORK"));
        assert!(md.contains("2 confirmed"));
        assert!(md.contains("1 contested"));
        assert!(md.contains("<details><summary>🇬🇧 English</summary>"));
        assert!(md.contains("<details open><summary>🇫🇷 Français</summary>"));
        assert!(md.contains("panic in handler"));
        assert!(md.contains("panique dans le handler"));
        assert!(md.contains("Minor (1)"));
        assert!(md.contains("Mineurs (1)"));
        assert!(md.contains("tight coupling"));
        assert!(md.contains("couplage fort"));
        assert!(md.contains("already handled elsewhere"));
        assert!(md.contains("deja gere ailleurs"));
        assert!(md.contains("Performance"));
        assert!(md.contains("<details><summary>Minor (1)</summary>\n\n"));
    }

    #[test]
    fn renders_nothing_reported_in_both_languages_when_empty() {
        let scored: Vec<(SynthFinding, FindingVerdict)> = vec![];
        let ok = [Agent::Correctness];
        let view = TeamCommentView {
            executive_summary: "Trivial change.",
            executive_summary_fr: "Changement trivial.",
            strengths: &[],
            strengths_fr: &[],
            scored: &scored,
            verdict: Verdict::Ship,
            file_count: 1,
            agents_ok: &ok,
            agents_failed: &[],
            raw_count: 0,
            dedup_count: 0,
            capped: 0,
            model: "codestral-latest + mistral-large-latest",
        };
        let md = render_team_comment(&view);
        assert!(md.contains("0 confirmed"));
        assert!(md.contains("No issues reported by the agents."));
        assert!(md.contains("Aucun problème remonté par les agents."));
        assert!(md.contains("<details><summary>🇬🇧 English</summary>\n\n"));
    }
}
