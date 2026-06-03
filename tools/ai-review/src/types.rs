use serde::Deserialize;

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Critical,
    Minor,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Finding {
    pub file: String,
    pub line: u32,
    pub severity: Severity,
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub struct ReviewResponse {
    pub summary: String,
    pub strengths: Vec<String>,
    pub findings: Vec<Finding>,
    pub security: String,
}

#[allow(dead_code)]
pub enum Mode {
    Review,
    Describe,
    Security,
    Architecture,
    Performance,
    Product,
}

#[allow(dead_code)]
impl Mode {
    pub fn from_env() -> anyhow::Result<Self> {
        match std::env::var("AI_MODE").as_deref() {
            Ok("review") | Err(_) => Ok(Self::Review),
            Ok("describe") => Ok(Self::Describe),
            Ok("security") => Ok(Self::Security),
            Ok("architecture") => Ok(Self::Architecture),
            Ok("performance") => Ok(Self::Performance),
            Ok("product") => Ok(Self::Product),
            Ok(other) => anyhow::bail!("unknown AI_MODE: {other}"),
        }
    }

    pub fn mistral_model(&self) -> &'static str {
        match self {
            Self::Review | Self::Security | Self::Architecture | Self::Performance => {
                "codestral-latest"
            }
            Self::Describe | Self::Product => "mistral-small-latest",
        }
    }

    pub fn comment_marker(&self) -> &'static str {
        match self {
            Self::Review => "<!-- ai-review-bot -->",
            Self::Security => "<!-- ai-security-bot -->",
            Self::Architecture => "<!-- ai-architecture-bot -->",
            Self::Performance => "<!-- ai-performance-bot -->",
            Self::Product => "<!-- ai-product-bot -->",
            Self::Describe => "",
        }
    }

    pub fn display_label(&self) -> &'static str {
        match self {
            Self::Review => "Code Review",
            Self::Security => "Security Review",
            Self::Architecture => "Architecture Review",
            Self::Performance => "Performance Review",
            Self::Product => "Product Review",
            Self::Describe => "Description",
        }
    }
}

/// One of the four specialised review angles run in parallel in team mode.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub enum Agent {
    Correctness,
    Security,
    Architecture,
    Performance,
}

#[allow(dead_code)]
impl Agent {
    /// Returns the human-readable label for this agent.
    pub fn label(self) -> &'static str {
        match self {
            Self::Correctness => "Correctness",
            Self::Security => "Security",
            Self::Architecture => "Architecture",
            Self::Performance => "Performance",
        }
    }
}

/// One adversarial lens applied to a synthesised finding during verification.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub enum Lens {
    CodeConfirms,
    RealImpact,
    FalsePositive,
}

#[allow(dead_code)]
impl Lens {
    /// Returns the human-readable label for this lens.
    pub fn label(self) -> &'static str {
        match self {
            Self::CodeConfirms => "code-confirms",
            Self::RealImpact => "real-impact",
            Self::FalsePositive => "false-positive",
        }
    }
}

/// A finding merged by the synthesis agent, tracking which agents raised it.
#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct SynthFinding {
    pub file: String,
    #[serde(default)]
    pub line: u32,
    #[serde(default = "default_severity")]
    pub severity: Severity,
    pub message: String,
    #[serde(default)]
    pub sources: Vec<String>,
}

/// Output of the synthesis agent: a merged, deduplicated cross-agent review.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct SynthReport {
    pub executive_summary: String,
    #[serde(default)]
    pub strengths: Vec<String>,
    #[serde(default)]
    pub findings: Vec<SynthFinding>,
}

/// Verdict returned by one adversarial lens for a single finding.
#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct LensVerdict {
    #[serde(deserialize_with = "de_bool_loose")]
    pub contested: bool,
    pub reason: String,
}

/// Aggregated verdict for one finding after the multi-lens vote.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct FindingVerdict {
    pub contested: bool,
    pub reasons: Vec<String>,
}

/// Final overall verdict computed deterministically after verification.
#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(dead_code)]
pub enum Verdict {
    Ship,
    NeedsWork,
    Discuss,
}

/// Default severity for a finding whose severity field is missing or unparseable.
fn default_severity() -> Severity {
    Severity::Minor
}

/// Deserializes a boolean from `true`/`false`, `"true"`/`"false"`, `"yes"`/`"no"`,
/// or `0`/`1`, defaulting to `true` (the sceptical stance) when the value cannot
/// be interpreted. Language models are inconsistent about boolean encoding.
fn de_bool_loose<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Loose {
        Bool(bool),
        Int(i64),
        Str(String),
    }

    let parsed = match Loose::deserialize(deserializer)? {
        Loose::Bool(b) => b,
        Loose::Int(n) => n != 0,
        Loose::Str(s) => !matches!(s.trim().to_ascii_lowercase().as_str(), "false" | "0" | "no"),
    };
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_full_review_response() {
        let json = r#"{
            "summary": "This PR adds manifest parsing.",
            "strengths": ["Good error handling", "Tests included"],
            "findings": [
                {"file": "src/lib.rs", "line": 42, "severity": "critical", "message": "unwrap() can panic"},
                {"file": "src/lib.rs", "line": 0,  "severity": "minor",    "message": "High cyclomatic complexity"}
            ],
            "security": "No issues detected."
        }"#;
        let r: ReviewResponse = serde_json::from_str(json).unwrap();
        assert_eq!(r.summary, "This PR adds manifest parsing.");
        assert_eq!(r.strengths.len(), 2);
        assert_eq!(r.findings.len(), 2);
        assert_eq!(r.findings[0].severity, Severity::Critical);
        assert_eq!(r.findings[1].line, 0);
    }

    #[test]
    fn deserializes_empty_findings() {
        let json = r#"{
            "summary": "Minor tweak.",
            "strengths": [],
            "findings": [],
            "security": "No issues."
        }"#;
        let r: ReviewResponse = serde_json::from_str(json).unwrap();
        assert!(r.findings.is_empty());
    }

    #[test]
    fn synth_report_defaults_missing_collections() {
        let json = r#"{ "executive_summary": "Solid PR." }"#;
        let r: SynthReport = serde_json::from_str(json).unwrap();
        assert_eq!(r.executive_summary, "Solid PR.");
        assert!(r.strengths.is_empty());
        assert!(r.findings.is_empty());
    }

    #[test]
    fn synth_finding_defaults_line_severity_and_sources() {
        let json = r#"{ "file": "src/a.rs", "message": "Possible panic." }"#;
        let f: SynthFinding = serde_json::from_str(json).unwrap();
        assert_eq!(f.line, 0);
        assert_eq!(f.severity, Severity::Minor);
        assert!(f.sources.is_empty());
    }

    #[test]
    fn synth_finding_keeps_sources_and_severity() {
        let json = r#"{
            "file": "src/a.rs", "line": 12, "severity": "critical",
            "message": "Tenant scoping missing.", "sources": ["security", "correctness"]
        }"#;
        let f: SynthFinding = serde_json::from_str(json).unwrap();
        assert_eq!(f.severity, Severity::Critical);
        assert_eq!(f.sources, vec!["security", "correctness"]);
    }

    #[test]
    fn lens_verdict_parses_loose_booleans() {
        let cases = [
            (r#"{"contested": true,    "reason": "x"}"#, true),
            (r#"{"contested": false,   "reason": "x"}"#, false),
            (r#"{"contested": "true",  "reason": "x"}"#, true),
            (r#"{"contested": "false", "reason": "x"}"#, false),
            (r#"{"contested": 1,       "reason": "x"}"#, true),
            (r#"{"contested": 0,       "reason": "x"}"#, false),
            (r#"{"contested": "maybe", "reason": "x"}"#, true),
        ];
        for (json, expected) in cases {
            let v: LensVerdict = serde_json::from_str(json).unwrap();
            assert_eq!(v.contested, expected, "input was {json}");
        }
    }
}
