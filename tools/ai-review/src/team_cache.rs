use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;

use crate::github::DiffContext;
use crate::types::{FindingVerdict, PatchAvailability, Severity, SynthFinding};

const CACHE_SCHEMA: u8 = 1;
const CACHE_PREFIX: &str = "<!-- ai-team-cache:v1:";
const CACHE_SUFFIX: &str = " -->";
/// Leaves ample room below GitHub's comment limit for the rendered report.
pub const MAX_CACHE_MARKER_BYTES: usize = 16_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewCache {
    schema: u8,
    reviewer_revision: String,
    diff_hash: String,
    entries: Vec<CachedVerdict>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct CachedVerdict {
    finding_hash: String,
    context_hash: String,
    verdict: FindingVerdict,
}

impl ReviewCache {
    #[must_use]
    pub fn new(reviewer_revision: &str, diff_hash: String) -> Self {
        Self {
            schema: CACHE_SCHEMA,
            reviewer_revision: reviewer_revision.to_string(),
            diff_hash,
            entries: Vec::new(),
        }
    }

    #[must_use]
    pub fn matches_run(&self, reviewer_revision: &str, diff_hash: &str) -> bool {
        self.schema == CACHE_SCHEMA
            && self.reviewer_revision == reviewer_revision
            && self.diff_hash == diff_hash
    }

    #[must_use]
    pub fn supports_revision(&self, reviewer_revision: &str) -> bool {
        self.schema == CACHE_SCHEMA && self.reviewer_revision == reviewer_revision
    }

    #[must_use]
    pub fn lookup(&self, ctx: &DiffContext, finding: &SynthFinding) -> Option<FindingVerdict> {
        let finding_hash = finding_hash(finding);
        let context_hash = context_hash(ctx, finding);
        self.entries
            .iter()
            .find(|entry| entry.finding_hash == finding_hash && entry.context_hash == context_hash)
            .map(|entry| entry.verdict.clone())
    }

    pub fn record(&mut self, ctx: &DiffContext, finding: &SynthFinding, verdict: &FindingVerdict) {
        self.entries.push(CachedVerdict {
            finding_hash: finding_hash(finding),
            context_hash: context_hash(ctx, finding),
            verdict: verdict.clone(),
        });
    }

    pub fn encode_bounded(&self) -> anyhow::Result<String> {
        let mut bounded = self.clone();
        loop {
            let json = serde_json::to_vec(&bounded)?;
            let marker = format!(
                "{CACHE_PREFIX}{}{CACHE_SUFFIX}",
                URL_SAFE_NO_PAD.encode(json)
            );
            if marker.len() <= MAX_CACHE_MARKER_BYTES {
                return Ok(marker);
            }
            anyhow::ensure!(
                !bounded.entries.is_empty(),
                "team cache metadata exceeds the marker size limit"
            );
            bounded.entries.pop();
        }
    }

    pub fn from_comment(body: &str) -> anyhow::Result<Option<Self>> {
        let Some(start) = body.find(CACHE_PREFIX) else {
            return Ok(None);
        };
        let encoded_start = start + CACHE_PREFIX.len();
        let suffix_offset = body[encoded_start..]
            .find(CACHE_SUFFIX)
            .ok_or_else(|| anyhow::anyhow!("team cache marker has no closing delimiter"))?;
        let encoded = &body[encoded_start..encoded_start + suffix_offset];
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|error| anyhow::anyhow!("invalid team cache encoding: {error}"))?;
        let cache: Self = serde_json::from_slice(&bytes)
            .map_err(|error| anyhow::anyhow!("invalid team cache payload: {error}"))?;
        if cache.schema != CACHE_SCHEMA {
            anyhow::bail!("unsupported team cache schema: {}", cache.schema);
        }
        Ok(Some(cache))
    }
}

#[must_use]
pub fn append_marker(body: &str, marker: &str) -> String {
    format!("{body}\n\n{marker}")
}

#[must_use]
pub fn diff_hash(ctx: &DiffContext) -> String {
    let mut hasher = Sha256::new();
    hash_part(&mut hasher, b"ai-team-diff-v1");
    hash_part(&mut hasher, &ctx.file_count.to_le_bytes());
    for file in &ctx.files {
        hash_part(&mut hasher, file.path.as_bytes());
        hash_part(&mut hasher, format!("{:?}", file.status).as_bytes());
        hash_part(
            &mut hasher,
            file.previous_path.as_deref().unwrap_or_default().as_bytes(),
        );
        hash_part(&mut hasher, &file.additions.to_le_bytes());
        hash_part(&mut hasher, &file.deletions.to_le_bytes());
        match &file.patch {
            PatchAvailability::Present(patch) => {
                hash_part(&mut hasher, b"present");
                hash_part(&mut hasher, patch.as_bytes());
            }
            PatchAvailability::Missing => hash_part(&mut hasher, b"missing"),
        }
    }
    for gap in &ctx.coverage_gaps {
        hash_part(&mut hasher, format!("{:?}", gap.kind).as_bytes());
        hash_part(&mut hasher, gap.file.as_bytes());
        hash_part(&mut hasher, gap.detail.as_bytes());
    }
    hex_digest(hasher)
}

fn finding_hash(finding: &SynthFinding) -> String {
    let mut hasher = Sha256::new();
    hash_part(&mut hasher, b"ai-team-finding-v1");
    hash_part(&mut hasher, finding.file.as_bytes());
    hash_part(&mut hasher, &finding.line.to_le_bytes());
    let severity = match finding.severity {
        Severity::Critical => b"critical".as_slice(),
        Severity::Minor => b"minor".as_slice(),
    };
    hash_part(&mut hasher, severity);
    hash_part(&mut hasher, finding.category.label().as_bytes());
    hash_part(&mut hasher, finding.message.trim().as_bytes());
    hex_digest(hasher)
}

fn context_hash(ctx: &DiffContext, finding: &SynthFinding) -> String {
    let mut hasher = Sha256::new();
    hash_part(&mut hasher, b"ai-team-context-v1");
    hash_part(&mut hasher, ctx.lens_context(finding).as_bytes());
    hex_digest(hasher)
}

fn hash_part(hasher: &mut Sha256, value: &[u8]) {
    hasher.update(value.len().to_le_bytes());
    hasher.update(value);
}

fn hex_digest(hasher: Sha256) -> String {
    let digest = hasher.finalize();
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};

    use super::*;
    use crate::types::{
        Category, ChangedFile, ChangedFileStatus, CoverageGap, CoverageGapKind, PatchAvailability,
    };

    fn finding(message: &str) -> SynthFinding {
        SynthFinding {
            file: "src/lib.rs".to_string(),
            line: 2,
            severity: Severity::Critical,
            category: Category::Bug,
            message: message.to_string(),
            message_fr: String::new(),
            sources: vec!["correctness".to_string()],
        }
    }

    fn context(patch: &str) -> DiffContext {
        let file = ChangedFile {
            path: "src/lib.rs".to_string(),
            status: ChangedFileStatus::Modified,
            previous_path: None,
            additions: 1,
            deletions: 0,
            patch: PatchAvailability::Present(patch.to_string()),
            added_lines: BTreeSet::from([2]),
        };
        DiffContext {
            full: format!("--- src/lib.rs\n{patch}\n"),
            by_file: HashMap::from([("src/lib.rs".to_string(), patch.to_string())]),
            head_files: HashMap::new(),
            file_count: 1,
            files: vec![file],
            batches: vec![],
            coverage_gaps: vec![],
        }
    }

    fn verdict(reason: &str) -> FindingVerdict {
        FindingVerdict {
            contested: true,
            reasons: vec![reason.to_string()],
            reasons_fr: vec![],
        }
    }

    #[test]
    fn round_trips_an_invisible_bounded_cache() {
        let ctx = context("@@ -1 +1,2 @@\n context\n+added");
        let mut cache = ReviewCache::new("revision", diff_hash(&ctx));
        cache.record(&ctx, &finding("panic"), &verdict("not reachable"));
        let marker = cache.encode_bounded().expect("encode");
        assert!(marker.starts_with(CACHE_PREFIX));
        assert!(marker.ends_with(CACHE_SUFFIX));
        assert!(marker.len() <= MAX_CACHE_MARKER_BYTES);
        let decoded = ReviewCache::from_comment(&format!("report\n{marker}"))
            .expect("decode")
            .expect("cache");
        assert_eq!(decoded, cache);
    }

    #[test]
    fn invalidates_revision_diff_context_and_finding_changes() {
        let first = context("@@ -1 +1,2 @@\n context\n+added");
        let changed = context("@@ -1 +1,2 @@\n context\n+different");
        let original = finding("panic");
        let mut cache = ReviewCache::new("revision", diff_hash(&first));
        cache.record(&first, &original, &verdict("cached"));

        assert!(cache.matches_run("revision", &diff_hash(&first)));
        assert!(!cache.matches_run("other", &diff_hash(&first)));
        assert!(!cache.matches_run("revision", &diff_hash(&changed)));
        assert!(cache.lookup(&first, &original).is_some());
        assert!(cache.lookup(&changed, &original).is_none());
        assert!(cache
            .lookup(&first, &finding("different finding"))
            .is_none());
    }

    #[test]
    fn corrupted_or_unsupported_payloads_fail_without_panicking() {
        assert!(ReviewCache::from_comment("plain report")
            .expect("absent")
            .is_none());
        assert!(ReviewCache::from_comment("<!-- ai-team-cache:v1:not-base64 -->").is_err());

        let unsupported = serde_json::json!({
            "schema": 99,
            "reviewer_revision": "revision",
            "diff_hash": "hash",
            "entries": []
        });
        let marker = format!(
            "{CACHE_PREFIX}{}{CACHE_SUFFIX}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&unsupported).expect("JSON"))
        );
        assert!(ReviewCache::from_comment(&marker).is_err());
    }

    #[test]
    fn oversized_entries_are_dropped_until_the_marker_fits() {
        let ctx = context("@@ -1 +1,2 @@\n context\n+added");
        let mut cache = ReviewCache::new("revision", diff_hash(&ctx));
        for index in 0..100 {
            let mut item = finding(&format!("finding {index}"));
            item.line = index + 1;
            cache.record(&ctx, &item, &verdict(&"x".repeat(1_000)));
        }
        let marker = cache.encode_bounded().expect("bounded encode");
        assert!(marker.len() <= MAX_CACHE_MARKER_BYTES);
        let decoded = ReviewCache::from_comment(&marker)
            .expect("decode")
            .expect("cache");
        assert!(decoded.entries.len() < cache.entries.len());
        assert!(decoded.matches_run("revision", &diff_hash(&ctx)));
    }

    #[test]
    fn oversized_metadata_is_rejected_instead_of_exceeding_the_limit() {
        let ctx = context("@@ -1 +1,2 @@\n context\n+added");
        let cache = ReviewCache::new(&"x".repeat(MAX_CACHE_MARKER_BYTES), diff_hash(&ctx));
        assert!(cache.encode_bounded().is_err());
    }

    #[test]
    fn diff_hash_includes_missing_input_accounting() {
        let mut ctx = context("@@ -1 +1,2 @@\n context\n+added");
        let clean = diff_hash(&ctx);
        ctx.coverage_gaps.push(CoverageGap {
            kind: CoverageGapKind::PatchUnavailable,
            file: "asset.bin".to_string(),
            detail: "missing patch".to_string(),
        });
        assert_ne!(clean, diff_hash(&ctx));
    }
}
