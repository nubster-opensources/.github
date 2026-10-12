//! Command line entry point of the benchmark.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use ai_review::MistralHttp;
use review_bench::baseline::{compare, Baseline, GateVerdict};
use review_bench::case::load_cases;
use review_bench::report::render_summary;
use review_bench::runner::run_bench;

const USAGE: &str = "usage: review-bench --cases <dir> --baseline <file> [--runs N]";
const DEFAULT_RUNS: usize = 3;
const MEASURE_IMPOSSIBLE: u8 = 2;

struct Arguments {
    cases: PathBuf,
    baseline: PathBuf,
    runs: usize,
}

fn parse_arguments(mut args: impl Iterator<Item = String>) -> Option<Arguments> {
    let mut cases = None;
    let mut baseline = None;
    let mut runs = DEFAULT_RUNS;
    while let Some(flag) = args.next() {
        let value = args.next()?;
        match flag.as_str() {
            "--cases" => cases = Some(PathBuf::from(value)),
            "--baseline" => baseline = Some(PathBuf::from(value)),
            "--runs" => runs = value.parse().ok().filter(|count| *count > 0)?,
            _ => return None,
        }
    }
    Some(Arguments {
        cases: cases?,
        baseline: baseline?,
        runs,
    })
}

fn append_to_step_summary(summary: &str) {
    let Some(path) = std::env::var_os("GITHUB_STEP_SUMMARY") else {
        return;
    };
    let result = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
        .and_then(|mut file| file.write_all(summary.as_bytes()));
    if let Err(error) = result {
        eprintln!("cannot write the step summary: {error}");
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let Some(arguments) = parse_arguments(std::env::args().skip(1)) else {
        eprintln!("{USAGE}");
        return ExitCode::from(MEASURE_IMPOSSIBLE);
    };
    let api_key = std::env::var("MISTRAL_API_KEY").unwrap_or_default();
    if api_key.trim().is_empty() {
        println!("not measured: MISTRAL_API_KEY is missing");
        return ExitCode::from(MEASURE_IMPOSSIBLE);
    }
    let baseline = match Baseline::load(&arguments.baseline) {
        Ok(baseline) => baseline,
        Err(error) => {
            eprintln!("{error:#}");
            return ExitCode::from(MEASURE_IMPOSSIBLE);
        }
    };
    let cases = match load_cases(&arguments.cases) {
        Ok(cases) => cases,
        Err(error) => {
            eprintln!("{error:#}");
            return ExitCode::from(MEASURE_IMPOSSIBLE);
        }
    };
    let model = match MistralHttp::new(api_key) {
        Ok(model) => Arc::new(model),
        Err(error) => {
            eprintln!("{error:#}");
            return ExitCode::from(MEASURE_IMPOSSIBLE);
        }
    };
    let score = match run_bench(&model, &cases, arguments.runs, baseline.window).await {
        Ok(score) => score,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(MEASURE_IMPOSSIBLE);
        }
    };
    let verdict = compare(&score, &baseline);
    let summary = render_summary(&score, &baseline, &verdict);
    println!("{summary}");
    append_to_step_summary(&summary);
    match verdict {
        GateVerdict::Regression { .. } => ExitCode::from(1),
        GateVerdict::Pass | GateVerdict::Improved { .. } => ExitCode::SUCCESS,
    }
}
