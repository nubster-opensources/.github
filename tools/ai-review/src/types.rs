use std::collections::BTreeSet;

use serde::Deserialize;

/// Normalised status of a file changed by a pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangedFileStatus {
    Added,
    Removed,
    Modified,
    Renamed,
    Copied,
    Changed,
    Unchanged,
}

/// Availability of the textual patch returned by GitHub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchAvailability {
    Present(String),
    /// GitHub did not provide a patch and did not expose enough metadata to
    /// distinguish a large/truncated file from another unsupported patch.
    Missing,
}

/// One changed file with the metadata required to account for review coverage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: String,
    pub status: ChangedFileStatus,
    pub previous_path: Option<String>,
    pub additions: u64,
    pub deletions: u64,
    pub patch: PatchAvailability,
    pub added_lines: BTreeSet<u32>,
}

/// Reason why part of a pull request could not be put in a model batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverageGapKind {
    PatchUnavailable,
    MalformedPatch,
    BatchBudgetExceeded,
    OversizedLine,
    AgentFailed,
    SynthesisFailed,
    GitHubFileListIncomplete,
}

/// Explicit accounting for one piece of review input that was not analysed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageGap {
    pub kind: CoverageGapKind,
    pub file: String,
    pub detail: String,
}

/// A bounded model input assembled from complete file/hunk units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewBatch {
    pub id: usize,
    pub content: String,
    pub files: Vec<String>,
}

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

pub enum Mode {
    Review,
    Describe,
    Security,
    Architecture,
    Performance,
    Product,
    Team,
}

impl Mode {
    pub fn from_env() -> anyhow::Result<Self> {
        match std::env::var("AI_MODE").as_deref() {
            Ok("review") | Err(_) => Ok(Self::Review),
            Ok("describe") => Ok(Self::Describe),
            Ok("security") => Ok(Self::Security),
            Ok("architecture") => Ok(Self::Architecture),
            Ok("performance") => Ok(Self::Performance),
            Ok("product") => Ok(Self::Product),
            Ok("team") => Ok(Self::Team),
            Ok(other) => anyhow::bail!("unknown AI_MODE: {other}"),
        }
    }

    pub fn mistral_model(&self) -> &'static str {
        match self {
            Self::Review | Self::Security | Self::Architecture | Self::Performance | Self::Team => {
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
            Self::Team => "<!-- ai-team-bot -->",
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
            Self::Team => "Team Review",
            Self::Describe => "Description",
        }
    }
}

/// One of the four specialised review angles run in parallel in team mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Agent {
    Correctness,
    Security,
    Architecture,
    Performance,
}

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
pub enum Lens {
    CodeConfirms,
    RealImpact,
    FalsePositive,
}

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

/// The kind of issue a synthesised finding describes.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Category {
    Bug,
    Security,
    Design,
    Performance,
    TestGap,
    #[serde(other)]
    Other,
}

impl Category {
    /// Returns the human-readable label for this category.
    pub fn label(self) -> &'static str {
        match self {
            Self::Bug => "bug",
            Self::Security => "security",
            Self::Design => "design",
            Self::Performance => "performance",
            Self::TestGap => "test-gap",
            Self::Other => "other",
        }
    }
}

/// A finding merged by the synthesis agent, tracking which agents raised it.
#[derive(Debug, Deserialize, Clone)]
pub struct SynthFinding {
    pub file: String,
    #[serde(default)]
    pub line: u32,
    #[serde(default = "default_severity")]
    pub severity: Severity,
    #[serde(default = "default_category")]
    pub category: Category,
    pub message: String,
    #[serde(default)]
    pub message_fr: String,
    #[serde(default)]
    pub sources: Vec<String>,
}

/// Output of the synthesis agent: a merged, deduplicated cross-agent review.
#[derive(Debug, Deserialize)]
pub struct SynthReport {
    pub executive_summary: String,
    #[serde(default)]
    pub executive_summary_fr: String,
    #[serde(default)]
    pub strengths: Vec<String>,
    #[serde(default)]
    pub strengths_fr: Vec<String>,
    #[serde(default)]
    pub findings: Vec<SynthFinding>,
}

/// Verdict returned by one adversarial lens for a single finding.
#[derive(Debug, Deserialize, Clone)]
pub struct LensVerdict {
    #[serde(deserialize_with = "de_bool_loose")]
    pub contested: bool,
    pub reason: String,
    #[serde(default)]
    pub reason_fr: String,
}

/// Aggregated verdict for one finding after the multi-lens vote.
#[derive(Debug, Clone)]
pub struct FindingVerdict {
    pub contested: bool,
    pub reasons: Vec<String>,
    pub reasons_fr: Vec<String>,
}

/// Final overall verdict computed deterministically after verification.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Verdict {
    Ship,
    NeedsWork,
    Discuss,
    Incomplete,
}

/// Default severity for a finding whose severity field is missing or unparseable.
fn default_severity() -> Severity {
    Severity::Minor
}

/// Default category for a finding whose category field is missing.
fn default_category() -> Category {
    Category::Other
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
            "file": "src/a.rs", "line": 12, "severity": "critical", "category": "security",
            "message": "Tenant scoping missing.", "sources": ["security", "correctness"]
        }"#;
        let f: SynthFinding = serde_json::from_str(json).unwrap();
        assert_eq!(f.severity, Severity::Critical);
        assert_eq!(f.category, Category::Security);
        assert_eq!(f.sources, vec!["security", "correctness"]);
    }

    #[test]
    fn synth_finding_category_parses_defaults_and_falls_back() {
        let known = r#"{"file": "a.rs", "message": "x", "category": "test-gap"}"#;
        assert_eq!(
            serde_json::from_str::<SynthFinding>(known)
                .unwrap()
                .category,
            Category::TestGap
        );

        let unknown = r#"{"file": "a.rs", "message": "x", "category": "whatever"}"#;
        assert_eq!(
            serde_json::from_str::<SynthFinding>(unknown)
                .unwrap()
                .category,
            Category::Other
        );

        let missing = r#"{"file": "a.rs", "message": "x"}"#;
        assert_eq!(
            serde_json::from_str::<SynthFinding>(missing)
                .unwrap()
                .category,
            Category::Other
        );
    }

    #[test]
    fn synth_finding_reads_french_and_defaults_it() {
        let with = r#"{"file":"a.rs","message":"Panic.","message_fr":"Panique."}"#;
        assert_eq!(
            serde_json::from_str::<SynthFinding>(with)
                .unwrap()
                .message_fr,
            "Panique."
        );
        let without = r#"{"file":"a.rs","message":"Panic."}"#;
        assert_eq!(
            serde_json::from_str::<SynthFinding>(without)
                .unwrap()
                .message_fr,
            ""
        );
    }

    #[test]
    fn synth_report_defaults_french_fields() {
        let json = r#"{"executive_summary":"Solid."}"#;
        let r: SynthReport = serde_json::from_str(json).unwrap();
        assert_eq!(r.executive_summary_fr, "");
        assert!(r.strengths_fr.is_empty());
    }

    #[test]
    fn lens_verdict_defaults_french_reason() {
        let json = r#"{"contested": true, "reason": "x"}"#;
        assert_eq!(
            serde_json::from_str::<LensVerdict>(json).unwrap().reason_fr,
            ""
        );
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
