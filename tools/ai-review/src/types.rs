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
}

#[allow(dead_code)]
impl Mode {
    pub fn from_env() -> anyhow::Result<Self> {
        match std::env::var("AI_MODE").as_deref() {
            Ok("review") | Err(_) => Ok(Self::Review),
            Ok("describe")        => Ok(Self::Describe),
            Ok(other)             => anyhow::bail!("unknown AI_MODE: {other}"),
        }
    }

    pub fn mistral_model(&self) -> &'static str {
        match self {
            Self::Review   => "codestral-latest",
            Self::Describe => "mistral-small-latest",
        }
    }
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
}
