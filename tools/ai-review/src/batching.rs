use std::collections::HashSet;
use std::fmt::Write as _;

use crate::types::{ChangedFile, CoverageGap, CoverageGapKind, PatchAvailability, ReviewBatch};

/// Leaves room below the Mistral request limit for prompt framing.
pub const DEFAULT_BATCH_BYTES: usize = 18_000;
/// Hard safety bound on specialist calls for unusually large pull requests.
pub const DEFAULT_MAX_BATCHES: usize = 8;

/// Result of converting changed files into bounded, traceable model inputs.
pub struct BatchPlan {
    pub batches: Vec<ReviewBatch>,
    pub gaps: Vec<CoverageGap>,
}

struct ReviewUnit {
    file: String,
    content: String,
}

/// Builds batches without cutting a UTF-8 codepoint, line, or hunk when the
/// hunk itself fits. Anything that cannot be represented is returned as an
/// explicit coverage gap.
#[must_use]
pub fn build_review_batches(
    files: &[ChangedFile],
    max_batch_bytes: usize,
    max_batches: usize,
) -> BatchPlan {
    let mut gaps = Vec::new();
    let mut units = Vec::new();

    for file in files {
        match &file.patch {
            PatchAvailability::Present(patch) => {
                units.extend(units_for_patch(file, patch, max_batch_bytes, &mut gaps));
            }
            PatchAvailability::Missing => gaps.push(CoverageGap {
                kind: CoverageGapKind::PatchUnavailable,
                file: file.path.clone(),
                detail: "GitHub did not provide a textual patch; binary and oversized patches cannot be distinguished safely from this response"
                    .to_string(),
            }),
        }
    }

    let mut batches = Vec::new();
    let mut current = String::new();
    let mut current_files = Vec::new();
    let mut omitted_files = HashSet::new();

    for unit in units {
        if !current.is_empty() && current.len() + unit.content.len() > max_batch_bytes {
            push_batch(&mut batches, &mut current, &mut current_files);
        }

        if batches.len() >= max_batches {
            if omitted_files.insert(unit.file.clone()) {
                gaps.push(CoverageGap {
                    kind: CoverageGapKind::BatchBudgetExceeded,
                    file: unit.file,
                    detail: format!("review exceeded the {max_batches}-batch safety limit"),
                });
            }
            continue;
        }

        if !current_files.contains(&unit.file) {
            current_files.push(unit.file);
        }
        current.push_str(&unit.content);
    }
    if !current.is_empty() && batches.len() < max_batches {
        push_batch(&mut batches, &mut current, &mut current_files);
    }

    BatchPlan { batches, gaps }
}

fn push_batch(batches: &mut Vec<ReviewBatch>, content: &mut String, files: &mut Vec<String>) {
    batches.push(ReviewBatch {
        id: batches.len() + 1,
        content: std::mem::take(content),
        files: std::mem::take(files),
    });
}

fn units_for_patch(
    file: &ChangedFile,
    patch: &str,
    max_batch_bytes: usize,
    gaps: &mut Vec<CoverageGap>,
) -> Vec<ReviewUnit> {
    let header = format!("--- {}\n", file.path);
    if header.len() >= max_batch_bytes {
        gaps.push(CoverageGap {
            kind: CoverageGapKind::OversizedLine,
            file: file.path.clone(),
            detail: "file header alone exceeds the batch budget".to_string(),
        });
        return Vec::new();
    }

    if header.len() + patch.len() < max_batch_bytes {
        return vec![ReviewUnit {
            file: file.path.clone(),
            content: format!("{header}{patch}\n"),
        }];
    }

    let mut units = Vec::new();
    for hunk in split_hunks(patch) {
        split_hunk_units(file, &header, &hunk, max_batch_bytes, &mut units, gaps);
    }
    units
}

fn split_hunks(patch: &str) -> Vec<String> {
    let mut hunks = Vec::new();
    let mut current = String::new();
    for line in patch.split_inclusive('\n') {
        if line.starts_with("@@") && !current.is_empty() {
            hunks.push(std::mem::take(&mut current));
        }
        current.push_str(line);
    }
    if !current.is_empty() {
        hunks.push(current);
    }
    hunks
}

fn split_hunk_units(
    file: &ChangedFile,
    file_header: &str,
    hunk: &str,
    max_batch_bytes: usize,
    units: &mut Vec<ReviewUnit>,
    gaps: &mut Vec<CoverageGap>,
) {
    if file_header.len() + hunk.len() < max_batch_bytes {
        units.push(ReviewUnit {
            file: file.path.clone(),
            content: format!("{file_header}{hunk}\n"),
        });
        return;
    }

    let mut lines = hunk.split_inclusive('\n');
    let hunk_header = lines.next().unwrap_or_default();
    let prefix = format!("{file_header}{hunk_header}");
    if prefix.len() >= max_batch_bytes {
        gaps.push(CoverageGap {
            kind: CoverageGapKind::OversizedLine,
            file: file.path.clone(),
            detail: "hunk header exceeds the batch budget".to_string(),
        });
        return;
    }

    let mut content = prefix.clone();
    for line in lines {
        if prefix.len() + line.len() + 1 > max_batch_bytes {
            gaps.push(CoverageGap {
                kind: CoverageGapKind::OversizedLine,
                file: file.path.clone(),
                detail: "one diff line exceeds the batch budget".to_string(),
            });
            continue;
        }
        if content.len() + line.len() + 1 > max_batch_bytes {
            let _ = writeln!(content);
            let completed = std::mem::replace(&mut content, prefix.clone());
            units.push(ReviewUnit {
                file: file.path.clone(),
                content: completed,
            });
        }
        content.push_str(line);
    }
    if content.len() > prefix.len() {
        let _ = writeln!(content);
        units.push(ReviewUnit {
            file: file.path.clone(),
            content,
        });
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::types::ChangedFileStatus;

    fn changed(path: &str, availability: PatchAvailability) -> ChangedFile {
        ChangedFile {
            path: path.to_string(),
            status: ChangedFileStatus::Modified,
            previous_path: None,
            additions: 1,
            deletions: 0,
            patch: availability,
            added_lines: BTreeSet::new(),
        }
    }

    #[test]
    fn covers_every_text_file_across_batches() {
        let files: Vec<_> = (0..31)
            .map(|index| {
                changed(
                    &format!("src/file{index}.rs"),
                    PatchAvailability::Present(format!(
                        "@@ -0,0 +1 @@\n+const VALUE_{index}: usize = {index};"
                    )),
                )
            })
            .collect();
        let plan = build_review_batches(&files, 300, 20);
        let covered: HashSet<_> = plan
            .batches
            .iter()
            .flat_map(|batch| batch.files.iter().cloned())
            .collect();
        assert_eq!(covered.len(), 31);
        assert!(plan.batches.iter().all(|batch| batch.content.len() <= 300));
        assert!(plan.gaps.is_empty());
    }

    #[test]
    fn splits_large_unicode_hunks_only_at_line_boundaries() {
        let mut patch = "@@ -1,1 +1,20 @@\n".to_string();
        for index in 0..20 {
            writeln!(patch, "+ligne {index} 🙂").expect("write fixture");
        }
        let plan = build_review_batches(
            &[changed("unicode.rs", PatchAvailability::Present(patch))],
            120,
            20,
        );
        assert!(plan.batches.len() > 1);
        assert!(plan
            .batches
            .iter()
            .all(|batch| batch.content.is_char_boundary(batch.content.len())));
        for index in 0..20 {
            assert!(plan
                .batches
                .iter()
                .any(|batch| batch.content.contains(&format!("+ligne {index} 🙂"))));
        }
        assert!(plan.gaps.is_empty());
    }

    #[test]
    fn reports_missing_and_budget_gaps() {
        let files = vec![
            changed("missing", PatchAvailability::Missing),
            changed(
                "first",
                PatchAvailability::Present("@@ -0,0 +1 @@\n+x".to_string()),
            ),
            changed(
                "overflow",
                PatchAvailability::Present("@@ -0,0 +1 @@\n+y".to_string()),
            ),
        ];
        let plan = build_review_batches(&files, 40, 1);
        assert!(plan
            .gaps
            .iter()
            .any(|gap| gap.kind == CoverageGapKind::PatchUnavailable));
        assert!(plan
            .gaps
            .iter()
            .any(|gap| gap.kind == CoverageGapKind::BatchBudgetExceeded));
    }
}
