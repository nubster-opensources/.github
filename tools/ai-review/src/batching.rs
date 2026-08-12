use std::collections::HashSet;

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
    let Some(spec) = parse_hunk_spec(hunk_header) else {
        gaps.push(CoverageGap {
            kind: CoverageGapKind::MalformedPatch,
            file: file.path.clone(),
            detail: "oversized patch contains an invalid hunk header".to_string(),
        });
        return;
    };

    let mut cursor = HunkCursor {
        old_line: spec.old_start,
        new_line: spec.new_start,
    };
    let mut fragment_start = cursor;
    let mut old_count = 0;
    let mut new_count = 0;
    let mut body = String::new();
    for line in lines {
        let (old_delta, new_delta) = line_deltas(line);
        let next_old_count = old_count + old_delta;
        let next_new_count = new_count + new_delta;
        if !fragment_fits(
            file_header,
            &spec.section,
            fragment_start,
            next_old_count,
            next_new_count,
            body.len() + line.len(),
            line.ends_with('\n'),
            max_batch_bytes,
        ) && !body.is_empty()
        {
            push_hunk_fragment(
                file,
                file_header,
                &spec.section,
                fragment_start,
                old_count,
                new_count,
                &body,
                units,
            );
            body.clear();
            fragment_start = cursor;
            old_count = 0;
            new_count = 0;
        }

        if !fragment_fits(
            file_header,
            &spec.section,
            fragment_start,
            old_count + old_delta,
            new_count + new_delta,
            body.len() + line.len(),
            line.ends_with('\n'),
            max_batch_bytes,
        ) {
            gaps.push(CoverageGap {
                kind: CoverageGapKind::OversizedLine,
                file: file.path.clone(),
                detail: "one diff line exceeds the batch budget".to_string(),
            });
            cursor.advance(old_delta, new_delta);
            fragment_start = cursor;
            continue;
        }

        body.push_str(line);
        old_count += old_delta;
        new_count += new_delta;
        cursor.advance(old_delta, new_delta);
    }
    if !body.is_empty() {
        push_hunk_fragment(
            file,
            file_header,
            &spec.section,
            fragment_start,
            old_count,
            new_count,
            &body,
            units,
        );
    }
}

#[derive(Clone, Copy)]
struct HunkCursor {
    old_line: u64,
    new_line: u64,
}

impl HunkCursor {
    fn advance(&mut self, old_delta: u64, new_delta: u64) {
        self.old_line = self.old_line.saturating_add(old_delta);
        self.new_line = self.new_line.saturating_add(new_delta);
    }
}

struct HunkSpec {
    old_start: u64,
    new_start: u64,
    section: String,
}

fn parse_hunk_spec(header: &str) -> Option<HunkSpec> {
    let body = header.trim_end().strip_prefix("@@ ")?;
    let (ranges, section) = body.split_once(" @@")?;
    let mut ranges = ranges.split_whitespace();
    let (old_start, _) = parse_range(ranges.next()?, '-')?;
    let (new_start, _) = parse_range(ranges.next()?, '+')?;
    if ranges.next().is_some() {
        return None;
    }
    Some(HunkSpec {
        old_start,
        new_start,
        section: section.trim().to_string(),
    })
}

fn parse_range(range: &str, prefix: char) -> Option<(u64, u64)> {
    let range = range.strip_prefix(prefix)?;
    match range.split_once(',') {
        Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
        None => Some((range.parse().ok()?, 1)),
    }
}

fn line_deltas(line: &str) -> (u64, u64) {
    if line.starts_with('+') {
        (0, 1)
    } else if line.starts_with('-') {
        (1, 0)
    } else if line.starts_with('\\') {
        (0, 0)
    } else {
        (1, 1)
    }
}

fn fragment_header(section: &str, start: HunkCursor, old_count: u64, new_count: u64) -> String {
    let section = if section.is_empty() {
        "[review fragment]".to_string()
    } else {
        format!("{section} [review fragment]")
    };
    format!(
        "@@ -{},{} +{},{} @@ {section}",
        start.old_line, old_count, start.new_line, new_count
    )
}

#[allow(clippy::too_many_arguments)]
fn fragment_fits(
    file_header: &str,
    section: &str,
    start: HunkCursor,
    old_count: u64,
    new_count: u64,
    body_bytes: usize,
    body_ends_with_newline: bool,
    max_batch_bytes: usize,
) -> bool {
    let header = fragment_header(section, start, old_count, new_count);
    let final_newline = usize::from(!body_ends_with_newline);
    file_header.len() + header.len() + 1 + body_bytes + final_newline <= max_batch_bytes
}

#[allow(clippy::too_many_arguments)]
fn push_hunk_fragment(
    file: &ChangedFile,
    file_header: &str,
    section: &str,
    start: HunkCursor,
    old_count: u64,
    new_count: u64,
    body: &str,
    units: &mut Vec<ReviewUnit>,
) {
    let header = fragment_header(section, start, old_count, new_count);
    let mut content = format!("{file_header}{header}\n{body}");
    if !content.ends_with('\n') {
        content.push('\n');
    }
    units.push(ReviewUnit {
        file: file.path.clone(),
        content,
    });
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fmt::Write as _;

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

    fn assert_fragment_coordinates(plan: &BatchPlan, old_start: u64, new_start: u64) {
        let mut expected_old = old_start;
        let mut expected_new = new_start;
        for batch in &plan.batches {
            let mut lines = batch.content.lines();
            let _file_header = lines.next().expect("file header");
            let header = lines.next().expect("hunk header");
            let spec = parse_hunk_spec(header).expect("valid fragment header");
            let body = header
                .strip_prefix("@@ ")
                .and_then(|value| value.split_once(" @@"))
                .map(|(ranges, _)| ranges)
                .expect("fragment header ranges");
            let mut ranges = body.split_whitespace();
            let (_, declared_old_count) =
                parse_range(ranges.next().expect("old range"), '-').expect("valid old range");
            let (_, declared_new_count) =
                parse_range(ranges.next().expect("new range"), '+').expect("valid new range");
            assert_eq!(spec.old_start, expected_old, "{}", batch.content);
            assert_eq!(spec.new_start, expected_new, "{}", batch.content);
            let (old_count, new_count) = lines
                .map(line_deltas)
                .fold((0, 0), |(old, new), (old_delta, new_delta)| {
                    (old + old_delta, new + new_delta)
                });
            assert_eq!(declared_old_count, old_count, "{}", batch.content);
            assert_eq!(declared_new_count, new_count, "{}", batch.content);
            expected_old += old_count;
            expected_new += new_count;
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
        let mut patch = "@@ -0,0 +1,20 @@\n".to_string();
        for index in 0..20 {
            writeln!(patch, "+ligne {index} 🙂").expect("write fixture");
        }
        let expected_added = crate::github::parse_added_lines(&patch);
        let plan = build_review_batches(
            &[changed(
                "unicode.rs",
                PatchAvailability::Present(patch.clone()),
            )],
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
        assert_fragment_coordinates(&plan, 0, 1);
        assert!(!plan.batches[1].content.contains("@@ -0,0 +1,20 @@"));
        let actual_added: BTreeSet<_> = plan
            .batches
            .iter()
            .flat_map(|batch| crate::github::parse_added_lines(&batch.content))
            .collect();
        assert_eq!(actual_added, expected_added);
    }

    #[test]
    fn recalculates_fragment_coordinates_for_context_and_deletions() {
        let patch = "@@ -10,5 +20,5 @@ fn demo\n context-a\n-old-a\n+new-a\n context-b\n-old-b\n+new-b\n context-c\n";
        let plan = build_review_batches(
            &[changed(
                "mixed.rs",
                PatchAvailability::Present(patch.to_string()),
            )],
            90,
            20,
        );
        assert!(plan.batches.len() > 1);
        assert_fragment_coordinates(&plan, 10, 20);
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
