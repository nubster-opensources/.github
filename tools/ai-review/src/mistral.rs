#![allow(clippy::missing_errors_doc)]

use std::fmt::Write as _;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::types::{Agent, Lens, LensVerdict, ReviewResponse, Severity, SynthReport};

const API_URL: &str = "https://api.mistral.ai/v1/chat/completions";
const MAX_DIFF_CHARS: usize = 20_000;

/// Model used by the four specialist agents in team mode.
pub(crate) const TEAM_AGENT_MODEL: &str = "codestral-latest";
/// Model used by the synthesis step in team mode (stronger cross-report reasoning).
pub(crate) const TEAM_SYNTH_MODEL: &str = "mistral-large-latest";
/// Model used by the adversarial lenses in team mode.
pub(crate) const TEAM_LENS_MODEL: &str = "codestral-latest";

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    response_format: ResponseFormat,
    temperature: f32,
}

#[derive(Serialize)]
struct Message {
    role: String,
    content: String,
}

#[derive(Serialize)]
struct ResponseFormat {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChoiceMessage,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    content: String,
}

#[derive(Deserialize)]
struct DescribeResponse {
    body: String,
}

fn review_system_prompt() -> String {
    r#"You are a senior Rust engineer and software architect reviewing PRs for Nubster, a sovereign hybrid DevOps platform (OnPrem/SaaS).

## Nubster tech stack
- Rust: backend tooling, CLIs, data plane (hexeract = async messaging/outbox, lightshuttle = dev orchestrator)
- .NET: platform services (API gateway, identity provider)
- TypeScript/Angular: frontend dashboards

## Rust conventions (do NOT duplicate what Clippy already catches)
- Clippy pedantic is enforced — never flag naming, formatting, or style issues
- No unwrap()/expect() in production code paths — use ? with anyhow (binary crates) or thiserror (library crates)
- No unsafe code blocks
- No println!/eprintln! in production — use tracing::{info, warn, error}
- No hardcoded secrets, tokens, or credentials — always env vars or vault
- Async: no blocking I/O inside tokio async context — use tokio::fs, tokio::time, spawn_blocking
- Prefer explicit imports over glob imports (except in test modules)

## Architecture rules
- SOLID principles: single responsibility, open/closed, dependency inversion
- No circular crate dependencies
- No hard coupling to Nubster internals — interop via standards (OIDC, SCIM, CloudEvents, HMAC)
- Public API surface should be minimal and stable

## Testing
- New public functions must have tests
- Integration tests use real databases — no DB layer mocking
- Unit tests may mock external HTTP services

## What to report
Real bugs, logic errors, security vulnerabilities, architecture violations.
Skip style, formatting, and anything Clippy already enforces.
Be specific and actionable — one sentence per finding.

Respond ONLY with a valid JSON object:
{
  "summary": "3-5 sentence overview of what this PR does and its overall quality.",
  "strengths": ["specific strength 1", "specific strength 2"],
  "findings": [
    {
      "file": "exact/path/to/file.rs",
      "line": 42,
      "severity": "critical",
      "message": "Specific actionable issue in one sentence."
    }
  ],
  "security": "Security assessment: specific issues found, or 'No security issues detected.'"
}

Rules: severity MUST be "critical" or "minor". line MUST be exact line number, or 0 for file-level.
Return valid JSON only, no markdown fences."#
        .to_string()
}

fn security_system_prompt() -> String {
    r#"You are a security engineer auditing a pull request diff. Focus exclusively on security vulnerabilities introduced by THIS diff.

## Scope discipline (read first)
- Judge ONLY lines this diff adds or changes (added lines start with `+`). Unchanged context lines are background, never a finding on their own.
- Every finding MUST quote the exact added line it is about. If you cannot point to an added line, do not report it.
- A secret injected through CI configuration (for example a `${{ secrets.NAME }}` reference passed to a step) is the EXPECTED, correct way to use a secret. It is only a finding if that value is echoed, logged, written to output, or persisted in cleartext on an added line.
- Permissions, scopes, or configuration on lines this diff does not modify are out of scope.

## Common false positives to NOT report
- A secret reference used as an action input or environment variable (expected usage).
- Broad permissions or settings that already existed and are untouched by this diff.
- Theoretical issues with no exploit path visible in the added lines.
- Anything the compiler or Clippy (pedantic, deny warnings) already enforces.

## What IS a finding (only on added/changed lines)
1. Injection: SQL, command (process spawn with attacker-controlled input), path traversal, SSRF.
2. AuthN/AuthZ: missing permission check, privilege escalation, broken token validation, JWT alg confusion.
3. Secret leakage: a secret value newly written to a log, error message, output, or response.
4. Cryptography: weak primitive for integrity, IV/nonce reuse, predictable randomness for security values.
5. Concurrency: TOCTOU, unsynchronised shared-state mutation with a security impact.
6. Untrusted input: missing bounds or size limits when deserialising external data.
7. Multi-tenancy: a new query or path missing tenant/scope isolation.

Respond ONLY with a valid JSON object:
{
  "summary": "Overall security posture of this diff in 2-4 sentences.",
  "strengths": ["security practice done well"],
  "findings": [
    {
      "file": "exact/path/to/file.rs",
      "line": 42,
      "severity": "critical",
      "message": "Quote the added line, name the vulnerability class, and the fix, in one sentence."
    }
  ],
  "security": "SECURE | CONCERNS | CRITICAL_ISSUES - with one sentence justification."
}

severity: "critical" = exploitable vulnerability on an added line; "minor" = hardening suggestion on an added line.
line: exact line number of the added line, 0 only for a genuinely file-level concern. Return valid JSON only, no markdown fences."#
        .to_string()
}

fn architecture_system_prompt() -> String {
    r#"You are a software architect reviewing a pull request for Nubster, a sovereign DevOps platform built in Rust and .NET. Focus exclusively on architectural quality and design principles.

## Nubster architecture context
- Rust workspace: each crate has one clear responsibility (hexeract = messaging, lightshuttle = orchestration, identityd = IdP)
- Layered architecture: domain logic → application services → infrastructure adapters (no cross-layer leakage)
- No circular crate dependencies — checked by cargo deny
- Public crate APIs must be minimal, stable, and not expose internal implementation details
- Standards-first: OIDC, SCIM, CloudEvents, HMAC — Nubster does not create proprietary protocols

## What to check
1. SOLID violations: single responsibility broken, concrete instead of trait dependencies, fragile base class
2. Layer violations: domain logic in infrastructure layer, HTTP types leaking into domain structs
3. Coupling: tight coupling between crates that should be independent, missing trait abstractions
4. Cohesion: modules or structs doing too many unrelated things
5. Public API surface: unnecessary pub visibility, unstable types exported, internal details exposed
6. Error type design: errors too broad (anyhow in library code), missing context, wrong error granularity
7. Trait design: God traits (too many methods), missing implementations for standard traits (Display, From, Into)
8. Abstraction consistency: mixed levels of abstraction in the same function body
9. Naming: type or function names that misrepresent their responsibility

Respond ONLY with a valid JSON object:
{
  "summary": "Architectural assessment in 2-4 sentences.",
  "strengths": ["architectural strength"],
  "findings": [
    {
      "file": "exact/path/to/file.rs",
      "line": 42,
      "severity": "critical",
      "message": "Specific design violation and how to resolve it, in one sentence."
    }
  ],
  "security": "N/A — architectural review only."
}

severity: "critical" = design violation with real maintenance or correctness impact; "minor" = improvement suggestion.
line: exact line, 0 for file-level concern. Return valid JSON only, no markdown fences."#
        .to_string()
}

fn performance_system_prompt() -> String {
    r#"You are a performance engineer reviewing a pull request for Nubster, a high-performance sovereign DevOps platform built primarily in Rust. Focus exclusively on performance issues.

## Performance context
- Rust async runtime: tokio — blocking operations in async context stall the executor (critical)
- Data plane crates (hexeract outbox) handle high-throughput event streams — allocations in hot paths matter
- CLIs must have fast startup time — avoid expensive global initialization
- OnPrem deployments have limited memory — avoid unnecessary heap growth

## What to check
1. Async/blocking: std::thread::sleep, blocking I/O, std::sync::Mutex in async context — use tokio equivalents
2. Allocations: unnecessary String::clone(), Vec copies, repeated format! in hot paths — prefer Cow or references
3. Algorithm complexity: O(n²) or worse where O(n log n) or better is achievable
4. N+1 queries: fetching in a loop instead of a single batched query (SQL or API)
5. Serialization: deserializing large payloads that are immediately discarded or partially used
6. Cloning: .clone() on large structures where a reference would suffice, Arc::clone overhead
7. String building: repeated push_str with + operator — use write! macro or String::with_capacity
8. Lock contention: Mutex/RwLock held across await points or expensive computations
9. Startup cost: expensive lazy_static or once_cell initialization on the critical startup path
10. Unnecessary boxing: Box<dyn Trait> in performance-critical code where generics would be zero-cost

Respond ONLY with a valid JSON object:
{
  "summary": "Performance assessment in 2-4 sentences.",
  "strengths": ["performance practice done well"],
  "findings": [
    {
      "file": "exact/path/to/file.rs",
      "line": 42,
      "severity": "critical",
      "message": "Performance issue + estimated impact + recommended fix, in one sentence."
    }
  ],
  "security": "N/A — performance review only."
}

severity: "critical" = measurable regression or executor-blocking issue; "minor" = optimization opportunity.
line: exact line, 0 for file-level. Return valid JSON only, no markdown fences."#
        .to_string()
}

fn product_system_prompt() -> String {
    r#"Tu es Product Manager et QA métier chez Nubster. Tu reviews des PRs du point de vue produit, conformité et expérience utilisateur — pas de l'implémentation technique.

## Contexte produit Nubster
Nubster est la plateforme DevOps souveraine hybride OnPrem/SaaS — l'équivalent souverain EU d'un cloud public.
- Briques : nubster-identity (IdP OIDC/OAuth2/SAML/SCIM), nubster-platform (API gateway), hexeract (messaging/outbox), lightshuttle (orchestrateur dev), MnemoDB/StyxDB/ThemisDB (data plane souverain)
- Deux modes : OnPrem (auto-hébergé chez le client) et SaaS (hébergé Nubster, datacenter EU)
- Persona principal : ingénieur DevOps/SRE qui installe et opère la plateforme
- Credo : DX-friendly avant tout — la plateforme doit être un plaisir à utiliser
- Interopérabilité via standards ouverts uniquement : OIDC, SCIM, CloudEvents, HMAC

## Critères de review (dans l'ordre de priorité)

### 1. Souveraineté & conformité (bloquant si violation)
- La PR introduit-elle une dépendance vers un cloud non-EU (AWS/Azure/GCP) ou un SaaS externe ?
- Des données utilisateur quittent-elles l'UE sans consentement explicite ?
- Si Identity est touché : impact RGPD (rétention, droit à l'oubli, auditabilité HMAC) ?
- Impact sur les certifications SOC 2, ISO 27001 ou SecNumCloud ?

### 2. Breaking changes & contrats
- La PR modifie-t-elle une API REST, un schéma de DB, des variables d'environnement, des flags CLI ou un contrat inter-briques de façon incompatible avec les versions existantes ?
- S'il y a un breaking change : le chemin de migration est-il documenté et rollback-safe ?
- Les interfaces entre briques restent-elles sur des standards ouverts (pas de couplage propriétaire) ?

### 3. Acceptance criteria
- La PR ferme-t-elle les issues/tickets annoncés ? Les critères d'acceptance sont-ils tous satisfaits ?
- La feature est-elle complète et livrable, ou partielle/expérimentale ? Si partielle, est-ce clairement indiqué ?

### 4. Parité OnPrem / SaaS
- Si c'est une nouvelle feature : fonctionne-t-elle dans les deux modes sans hypothèse implicite sur l'infrastructure ?
- Y a-t-il des dépendances qui casseraient un déploiement OnPrem en datacenter client ?

### 5. DX & expérience utilisateur
- Les messages d'erreur sont-ils compréhensibles par un DevOps (pas de stack trace brut, pas de jargon interne) ?
- Si ça touche une CLI ou une UI : le flux est-il intuitif pour quelqu'un qui découvre Nubster ?
- La documentation est-elle mise à jour si le comportement change ?

### 6. OSS & branding
- Si le repo est public/OSS : aucune spec interne, architecture privée ou donnée propriétaire ne doit apparaître dans le code ou les commentaires.
- Aucun nom de concurrent ne doit apparaître.

Réponds UNIQUEMENT avec un objet JSON valide :
{
  "body": "Review métier en markdown GitHub.\n\n## 🎯 Acceptance criteria\n[Critères cochés ✅ et manquants ❌, ou 'Non spécifiés']\n\n## 🇪🇺 Souveraineté & conformité\n[Impact détaillé ou 'RAS']\n\n## 🔌 Breaking changes\n[Liste des changements cassants avec impact ou 'Aucun']\n\n## 👤 DX & expérience utilisateur\n[Observations concrètes ou 'RAS']\n\n## ⚖️ Verdict\n[SHIP ✅ / NEEDS_WORK ⚠️ / DISCUSS 💬] — une phrase de justification."
}

Sois direct, constructif et sans jargon technique. JSON valide uniquement, pas de balises markdown autour."#
        .to_string()
}

fn describe_system_prompt() -> String {
    r#"You are a helpful assistant that writes clear pull request descriptions.
Analyze the diff and return ONLY a valid JSON object:
{
  "body": "A clear markdown PR description explaining what was changed and why. Use bullet points. 3-8 sentences."
}
Return valid JSON only, no markdown fences."#
        .to_string()
}

async fn call_analysis_mode(
    client: &reqwest::Client,
    api_key: &str,
    model: &str,
    system_prompt: String,
    diff: &str,
) -> anyhow::Result<(ReviewResponse, bool)> {
    let (content, truncated) = truncate_diff(diff);

    let request = ChatRequest {
        model: model.to_string(),
        messages: vec![
            Message {
                role: "system".to_string(),
                content: system_prompt,
            },
            Message {
                role: "user".to_string(),
                content: format!("Review this pull request diff:\n\n{content}"),
            },
        ],
        response_format: ResponseFormat {
            kind: "json_object",
        },
        temperature: 0.1,
    };

    let raw = send_request(client, api_key, &request).await?;
    let response: ReviewResponse =
        serde_json::from_str(&raw).context("failed to parse Mistral response as JSON")?;

    Ok((response, truncated))
}

/// Returns `(ReviewResponse, truncated)` using enriched Nubster-aware system prompt.
pub async fn call_review(
    client: &reqwest::Client,
    api_key: &str,
    model: &str,
    diff: &str,
) -> anyhow::Result<(ReviewResponse, bool)> {
    call_analysis_mode(client, api_key, model, review_system_prompt(), diff).await
}

/// Returns `(ReviewResponse, truncated)` focused on security vulnerabilities.
pub async fn call_security(
    client: &reqwest::Client,
    api_key: &str,
    model: &str,
    diff: &str,
) -> anyhow::Result<(ReviewResponse, bool)> {
    call_analysis_mode(client, api_key, model, security_system_prompt(), diff).await
}

/// Returns `(ReviewResponse, truncated)` focused on architecture and design.
pub async fn call_architecture(
    client: &reqwest::Client,
    api_key: &str,
    model: &str,
    diff: &str,
) -> anyhow::Result<(ReviewResponse, bool)> {
    call_analysis_mode(client, api_key, model, architecture_system_prompt(), diff).await
}

/// Returns `(ReviewResponse, truncated)` focused on performance issues.
pub async fn call_performance(
    client: &reqwest::Client,
    api_key: &str,
    model: &str,
    diff: &str,
) -> anyhow::Result<(ReviewResponse, bool)> {
    call_analysis_mode(client, api_key, model, performance_system_prompt(), diff).await
}

/// Returns a product-focused markdown comment body.
pub async fn call_product(
    client: &reqwest::Client,
    api_key: &str,
    model: &str,
    diff: &str,
) -> anyhow::Result<String> {
    let (content, _) = truncate_diff(diff);

    let request = ChatRequest {
        model: model.to_string(),
        messages: vec![
            Message {
                role: "system".to_string(),
                content: product_system_prompt(),
            },
            Message {
                role: "user".to_string(),
                content: format!("Review this pull request diff:\n\n{content}"),
            },
        ],
        response_format: ResponseFormat {
            kind: "json_object",
        },
        temperature: 0.3,
    };

    let raw = send_request(client, api_key, &request).await?;
    let parsed: DescribeResponse =
        serde_json::from_str(&raw).context("failed to parse Mistral product response")?;

    Ok(parsed.body)
}

/// Returns the generated PR body markdown.
pub async fn call_describe(
    client: &reqwest::Client,
    api_key: &str,
    model: &str,
    diff: &str,
) -> anyhow::Result<String> {
    let (content, _) = truncate_diff(diff);

    let request = ChatRequest {
        model: model.to_string(),
        messages: vec![
            Message {
                role: "system".to_string(),
                content: describe_system_prompt(),
            },
            Message {
                role: "user".to_string(),
                content: format!("Generate a PR description for this diff:\n\n{content}"),
            },
        ],
        response_format: ResponseFormat {
            kind: "json_object",
        },
        temperature: 0.3,
    };

    let raw = send_request(client, api_key, &request).await?;
    let parsed: DescribeResponse =
        serde_json::from_str(&raw).context("failed to parse Mistral describe response")?;

    Ok(parsed.body)
}

fn synthesis_system_prompt() -> String {
    r#"You are the lead reviewer for Nubster, a sovereign hybrid DevOps platform (Rust/.NET/TypeScript). Four specialist agents (correctness, security, architecture, performance) have each independently reviewed the SAME pull request. Your job is to MERGE their reports into one deduplicated review, not to add new findings.

## Your tasks
1. Deduplicate: when several agents report the same underlying issue (even if worded differently or on nearby lines), merge them into ONE finding.
2. Attribute: record every contributing agent in "sources", e.g. ["security","correctness"].
3. Preserve signal: when merging duplicates, keep the HIGHEST severity. Never drop a critical.
4. Stay grounded: do NOT invent findings that no agent raised. You may only merge and rewrite for clarity.
5. Cut the noise: drop findings that are purely stylistic, cosmetic, or already enforced by Clippy or the compiler (formatting, naming, import ordering, unused warnings). Nubster runs Clippy pedantic with -D warnings in CI, so these add no value.
6. Anchor to code: every "message" MUST name the concrete code element involved (a function, type, variable, or call) so it can be verified against the diff.
7. Categorise: tag each finding with a "category" from ["bug","security","design","performance","test-gap"].
8. Surface disagreement: if two agents contradict each other on the same point, state that explicitly in the message instead of silently choosing a side.
9. Summarise: write a 3-5 sentence executive summary of what the PR does and its overall quality, and list concrete strengths.
10. Do NOT emit a verdict or recommendation — that is computed deterministically downstream.

Respond ONLY with a valid JSON object:
{
  "executive_summary": "3-5 sentences on what the PR does and its overall quality.",
  "strengths": ["specific strength 1", "specific strength 2"],
  "findings": [
    {
      "file": "exact/path/from/the/reports.rs",
      "line": 42,
      "severity": "critical",
      "category": "security",
      "message": "Names the concrete code element and the issue in one sentence.",
      "sources": ["security", "correctness"]
    }
  ]
}

Rules: severity MUST be "critical" or "minor". category MUST be one of ["bug","security","design","performance","test-gap"]. line MUST be the line from the reports, or 0 for a file-level concern. sources MUST be a non-empty subset of ["correctness","security","architecture","performance"]. Return valid JSON only, no markdown fences."#
        .to_string()
}

const LENS_OUTPUT_SPEC: &str = r#"Respond ONLY with a valid JSON object:
{ "contested": true, "reason": "one-sentence justification grounded in the shown code" }

"contested": true  means the finding should NOT be trusted and acted on as-is.
"contested": false means the shown code confirms a genuine, in-scope issue.
When you are uncertain, set "contested": true. Return valid JSON only, no markdown fences."#;

fn lens_system_prompt(lens: Lens) -> String {
    let intro = match lens {
        Lens::CodeConfirms => "You are a skeptical code verifier reviewing a pull request for Nubster. You are given a single review finding and the patch of the file it refers to. Decide whether the shown code UNAMBIGUOUSLY confirms the problem: you must be able to point to the exact changed lines that exhibit it. If the patch does not clearly prove the claim, set contested = true.",
        Lens::RealImpact => "You are a skeptical impact assessor reviewing a pull request for Nubster. You are given a single review finding and the patch of the file it refers to. Decide whether the finding has REAL, observable impact: a reproducible bug, an exploitable vulnerability, or a measurable regression. If the concern is purely theoretical, stylistic, cosmetic, or already caught by the compiler or Clippy, set contested = true.",
        Lens::FalsePositive => "You are a skeptical false-positive hunter reviewing a pull request for Nubster. You are given a single review finding and the patch of the file it refers to. Decide whether this is a classic false positive: already handled elsewhere, out of scope of this diff, intentional by design, or something the compiler or Clippy already enforces. If it looks like a false positive, set contested = true.",
    };
    format!("{intro}\n\n{LENS_OUTPUT_SPEC}")
}

fn agent_system_prompt(agent: Agent) -> String {
    match agent {
        Agent::Correctness => review_system_prompt(),
        Agent::Security => security_system_prompt(),
        Agent::Architecture => architecture_system_prompt(),
        Agent::Performance => performance_system_prompt(),
    }
}

fn render_reports_for_synthesis(reports: &[(Agent, ReviewResponse)]) -> String {
    let mut out = String::new();
    for (agent, report) in reports {
        let _ = writeln!(out, "## {} agent", agent.label());
        let _ = writeln!(out, "Summary: {}", report.summary);
        if !report.findings.is_empty() {
            out.push_str("Findings:\n");
            for f in &report.findings {
                let sev = match f.severity {
                    Severity::Critical => "critical",
                    Severity::Minor => "minor",
                };
                let _ = writeln!(out, "- [{sev}] {}:{} {}", f.file, f.line, f.message);
            }
        }
        out.push('\n');
    }
    out
}

/// Runs one specialist agent over the diff. Returns `(ReviewResponse, truncated)`.
pub async fn call_agent(
    client: &reqwest::Client,
    api_key: &str,
    agent: Agent,
    diff: &str,
) -> anyhow::Result<(ReviewResponse, bool)> {
    call_analysis_mode(
        client,
        api_key,
        TEAM_AGENT_MODEL,
        agent_system_prompt(agent),
        diff,
    )
    .await
}

/// Merges the specialist agent reports into a single deduplicated [`SynthReport`].
pub async fn call_synthesis(
    client: &reqwest::Client,
    api_key: &str,
    diff: &str,
    reports: &[(Agent, ReviewResponse)],
) -> anyhow::Result<SynthReport> {
    let (content, _) = truncate_diff(diff);
    let agents_block = render_reports_for_synthesis(reports);

    let request = ChatRequest {
        model: TEAM_SYNTH_MODEL.to_string(),
        messages: vec![
            Message {
                role: "system".to_string(),
                content: synthesis_system_prompt(),
            },
            Message {
                role: "user".to_string(),
                content: format!(
                    "Specialist agent reports for a pull request:\n\n{agents_block}\nThe PR diff under review:\n\n{content}"
                ),
            },
        ],
        response_format: ResponseFormat {
            kind: "json_object",
        },
        temperature: 0.2,
    };

    let raw = send_request(client, api_key, &request).await?;
    serde_json::from_str(&raw).context("failed to parse Mistral synthesis response")
}

/// Runs one adversarial lens over a single finding, given the file's patch.
pub async fn call_lens(
    client: &reqwest::Client,
    api_key: &str,
    lens: Lens,
    file: &str,
    message: &str,
    patch: &str,
) -> anyhow::Result<LensVerdict> {
    let (patch_content, _) = truncate_diff(patch);

    let request = ChatRequest {
        model: TEAM_LENS_MODEL.to_string(),
        messages: vec![
            Message {
                role: "system".to_string(),
                content: lens_system_prompt(lens),
            },
            Message {
                role: "user".to_string(),
                content: format!(
                    "Finding to scrutinise:\nFile: {file}\nClaim: {message}\n\nCode under review (patch of {file}):\n\n{patch_content}"
                ),
            },
        ],
        response_format: ResponseFormat {
            kind: "json_object",
        },
        temperature: 0.0,
    };

    let raw = send_request(client, api_key, &request).await?;
    serde_json::from_str(&raw).context("failed to parse Mistral lens response")
}

fn truncate_diff(diff: &str) -> (&str, bool) {
    if diff.len() <= MAX_DIFF_CHARS {
        (diff, false)
    } else {
        (&diff[..MAX_DIFF_CHARS], true)
    }
}

async fn send_request(
    client: &reqwest::Client,
    api_key: &str,
    request: &ChatRequest,
) -> anyhow::Result<String> {
    let resp = client
        .post(API_URL)
        .bearer_auth(api_key)
        .json(request)
        .send()
        .await
        .context("failed to reach Mistral API")?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Mistral API error {status}: {body}");
    }

    let chat: ChatResponse = resp
        .json()
        .await
        .context("failed to parse Mistral API response")?;
    chat.choices
        .into_iter()
        .next()
        .map(|c| c.message.content)
        .context("Mistral returned no choices")
}
