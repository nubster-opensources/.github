#![allow(clippy::missing_errors_doc)]

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::types::ReviewResponse;

const API_URL: &str = "https://api.mistral.ai/v1/chat/completions";
const MAX_DIFF_CHARS: usize = 20_000;

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
    r#"You are a security engineer auditing a pull request for Nubster, a sovereign hybrid DevOps platform. Focus exclusively on security vulnerabilities.

## Nubster security context
- Identity provider handling OIDC/OAuth2/SAML/SCIM — auth/authz bugs are critical
- Multi-tenant: data isolation between organizations is mandatory (tenant data leakage = critical)
- Sovereign platform: EU data residency enforced, no data exfiltration
- Secrets: GITHUB_TOKEN, MISTRAL_API_KEY, DB credentials — must never appear in logs or error messages

## Security checklist
1. Injection: SQL injection, command injection (std::process::Command with user input), path traversal, SSRF
2. Auth/Authz: missing permission checks, privilege escalation, insecure token handling, JWT alg confusion
3. Secrets exposure: hardcoded credentials, secrets in logs, secrets in error messages or debug output
4. Cryptography: weak algorithms (MD5/SHA1 for integrity), IV reuse, predictable PRNG for security-sensitive values
5. Race conditions: TOCTOU vulnerabilities, concurrent state mutation without synchronization
6. Integer handling: overflow in security-sensitive arithmetic (use checked_add, saturating_add)
7. Rust-specific: unsafe blocks with justification missing, transmute misuse, raw pointer lifetime issues
8. Input validation: missing bounds checks on external inputs, deserializing untrusted data without limits
9. Multi-tenancy: missing tenant scoping in queries, cross-tenant data access
10. Data leakage: PII in logs, internal paths/structure in error responses returned to clients

Respond ONLY with a valid JSON object:
{
  "summary": "Overall security posture of this PR in 2-4 sentences.",
  "strengths": ["security practice done well"],
  "findings": [
    {
      "file": "exact/path/to/file.rs",
      "line": 42,
      "severity": "critical",
      "message": "Vulnerability class + how it could be exploited + recommended fix, in one sentence."
    }
  ],
  "security": "SECURE | CONCERNS | CRITICAL_ISSUES — with one sentence justification."
}

severity: "critical" = exploitable vulnerability; "minor" = hardening suggestion.
line: exact line, 0 for file-level. Return valid JSON only, no markdown fences."#
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
