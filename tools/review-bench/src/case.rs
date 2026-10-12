//! Loading of benchmark cases from disk.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path};

use ai_review::{ChangedFile, ChangedFileStatus};
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

/// Where a benchmark case comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaseOrigin {
    /// A bug that really shipped and was fixed later.
    Real {
        /// Reference of the pull request that introduced the bug.
        pull_request: String,
        /// Commit that fixed the bug.
        fix_commit: String,
    },
    /// A bug injected on purpose into a clean change.
    Injected,
}

/// A bug the review is expected to find.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedBug {
    /// Path of the file holding the bug.
    pub file: String,
    /// First line of the bug, inclusive.
    pub first_line: u32,
    /// Last line of the bug, inclusive.
    pub last_line: u32,
    /// Words of which at least one must appear in a matching finding.
    pub keywords: Vec<String>,
}

/// One benchmark case: a change set and the bugs hidden in it.
#[derive(Debug, Clone)]
pub struct BenchCase {
    /// Case identifier, taken from the directory name.
    pub id: String,
    /// Origin of the case.
    pub origin: CaseOrigin,
    /// Files changed by the case.
    pub files: Vec<ChangedFile>,
    /// Content of the files at the head commit, by path.
    pub head_files: HashMap<String, String>,
    /// Bugs the review is expected to find.
    pub expected_bugs: Vec<ExpectedBug>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaseManifest {
    origin: String,
    pull_request: Option<String>,
    fix_commit: Option<String>,
    #[serde(default)]
    files: Vec<FileEntry>,
    #[serde(default)]
    expected_bugs: Vec<BugEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileEntry {
    path: String,
    status: String,
    patch: Option<String>,
    head: Option<String>,
    additions: u64,
    deletions: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BugEntry {
    file: String,
    lines: Vec<u32>,
    keywords: Vec<String>,
}

fn parse_status(value: &str) -> Result<ChangedFileStatus> {
    Ok(match value {
        "added" => ChangedFileStatus::Added,
        "removed" => ChangedFileStatus::Removed,
        "modified" => ChangedFileStatus::Modified,
        "renamed" => ChangedFileStatus::Renamed,
        "copied" => ChangedFileStatus::Copied,
        "changed" => ChangedFileStatus::Changed,
        "unchanged" => ChangedFileStatus::Unchanged,
        other => bail!("unknown file status `{other}`"),
    })
}

fn ensure_inside_case(reference: &str) -> Result<()> {
    let path = Path::new(reference);
    let escapes = path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        });
    if escapes {
        bail!("file reference `{reference}` points outside the case directory (outside the case)");
    }
    Ok(())
}

fn build_origin(manifest: &CaseManifest) -> Result<CaseOrigin> {
    match manifest.origin.as_str() {
        "injected" => Ok(CaseOrigin::Injected),
        "real" => {
            let pull_request = manifest
                .pull_request
                .clone()
                .ok_or_else(|| anyhow!("origin `real` requires `pull_request`"))?;
            let fix_commit = manifest
                .fix_commit
                .clone()
                .ok_or_else(|| anyhow!("origin `real` requires `fix_commit`"))?;
            Ok(CaseOrigin::Real {
                pull_request,
                fix_commit,
            })
        }
        other => bail!("unknown origin `{other}`, expected `real` or `injected`"),
    }
}

fn validate(manifest: &CaseManifest) -> Result<()> {
    build_origin(manifest)?;
    if manifest.files.is_empty() {
        bail!("at least one entry in `files` is required");
    }
    let mut paths = HashSet::new();
    for entry in &manifest.files {
        if !paths.insert(entry.path.as_str()) {
            bail!("duplicate file path `{}`", entry.path);
        }
        parse_status(&entry.status).with_context(|| format!("file `{}`", entry.path))?;
        for reference in [&entry.patch, &entry.head].into_iter().flatten() {
            ensure_inside_case(reference)?;
        }
    }
    for bug in &manifest.expected_bugs {
        match bug.lines.as_slice() {
            [first, last] if *first >= 1 && first <= last => {}
            _ => bail!(
                "expected bug on `{}`: `lines` must hold two values, first >= 1 and first <= last",
                bug.file
            ),
        }
        if bug.keywords.is_empty() {
            bail!(
                "expected bug on `{}`: `keywords` must not be empty",
                bug.file
            );
        }
        if !paths.contains(bug.file.as_str()) {
            bail!(
                "expected bug targets unknown file `{}` which the case does not change",
                bug.file
            );
        }
    }
    Ok(())
}

fn parse_case(id: &str, text: &str) -> Result<CaseManifest> {
    let manifest: CaseManifest =
        toml::from_str(text).with_context(|| format!("case {id}: invalid case.toml"))?;
    validate(&manifest).with_context(|| format!("case {id}"))?;
    Ok(manifest)
}

/// Loads one case from its directory; the directory name is the case id.
///
/// # Errors
///
/// Fails when `case.toml` is missing or invalid, or when a referenced
/// patch or head file cannot be read.
pub fn load_case(directory: &Path) -> Result<BenchCase> {
    let id = directory
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            anyhow!(
                "case directory `{}` has no usable name",
                directory.display()
            )
        })?
        .to_string();
    let text = std::fs::read_to_string(directory.join("case.toml"))
        .with_context(|| format!("case {id}: cannot read case.toml"))?;
    let manifest = parse_case(&id, &text)?;
    let origin = build_origin(&manifest).with_context(|| format!("case {id}"))?;

    let mut files = Vec::with_capacity(manifest.files.len());
    let mut head_files = HashMap::new();
    for entry in &manifest.files {
        let status = parse_status(&entry.status).with_context(|| format!("case {id}"))?;
        let patch = match &entry.patch {
            Some(reference) => Some(
                std::fs::read_to_string(directory.join(reference))
                    .with_context(|| format!("case {id}: cannot read patch `{reference}`"))?,
            ),
            None => None,
        };
        if let Some(reference) = &entry.head {
            let content = std::fs::read_to_string(directory.join(reference))
                .with_context(|| format!("case {id}: cannot read head file `{reference}`"))?;
            head_files.insert(entry.path.clone(), content);
        }
        files.push(ChangedFile::from_patch(
            entry.path.clone(),
            status,
            patch,
            entry.additions,
            entry.deletions,
        ));
    }
    let expected_bugs = manifest
        .expected_bugs
        .iter()
        .map(|bug| ExpectedBug {
            file: bug.file.clone(),
            first_line: bug.lines[0],
            last_line: bug.lines[1],
            keywords: bug.keywords.clone(),
        })
        .collect();

    Ok(BenchCase {
        id,
        origin,
        files,
        head_files,
        expected_bugs,
    })
}

/// Loads every case directory under `root`, sorted by id.
///
/// # Errors
///
/// Fails on the first case that cannot be loaded.
pub fn load_cases(root: &Path) -> Result<Vec<BenchCase>> {
    let mut directories = Vec::new();
    for entry in std::fs::read_dir(root)
        .with_context(|| format!("cannot list cases under `{}`", root.display()))?
    {
        let path = entry?.path();
        if path.is_dir() && path.join("case.toml").is_file() {
            directories.push(path);
        }
    }
    directories.sort();
    directories
        .iter()
        .map(|directory| load_case(directory))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::parse_case;

    #[test]
    fn an_inverted_line_range_is_rejected() {
        let error = parse_case("bad", "origin = \"injected\"\n[[files]]\npath = \"a.rs\"\nstatus = \"modified\"\nadditions = 1\ndeletions = 0\n[[expected_bugs]]\nfile = \"a.rs\"\nlines = [5, 2]\nkeywords = [\"x\"]\n")
            .expect_err("must fail");
        assert!(format!("{error:#}").contains("lines"));
    }

    #[test]
    fn an_expected_bug_without_keywords_is_rejected() {
        let error = parse_case("bad", "origin = \"injected\"\n[[files]]\npath = \"a.rs\"\nstatus = \"modified\"\nadditions = 1\ndeletions = 0\n[[expected_bugs]]\nfile = \"a.rs\"\nlines = [1, 1]\nkeywords = []\n")
            .expect_err("must fail");
        assert!(format!("{error:#}").contains("keywords"));
    }
}
