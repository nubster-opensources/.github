# nubster-opensources/.github

Organization-level tooling for nubster-opensources.

## tools/ai-review

Rust binary used in reusable GitHub Actions workflows to review pull requests
and generate PR descriptions using the Mistral API.

### Modes

The mode is selected with the `AI_MODE` environment variable, which the
reusable workflow forwards from its `mode` input.

| Mode | What it does | Model |
| --- | --- | --- |
| `review` | General code review (bugs, logic, security summary) | codestral |
| `security` | Security-focused audit | codestral |
| `architecture` | Architecture and design review | codestral |
| `performance` | Performance review | codestral |
| `product` | Product, compliance and developer-experience review | mistral-small |
| `describe` | Fills an empty PR description from the diff | mistral-small |
| `team` | Multi-agent review: four specialist agents run in parallel, a synthesis step merges and deduplicates their findings, then every finding is checked by a three-lens adversarial vote before a deterministic verdict | codestral + mistral-large |

The reviewer follows every page of GitHub's changed-file response. In `team`
mode it splits large textual patches into bounded UTF-8-safe batches without
cutting ordinary hunks or lines. Missing patches, oversized inputs, exhausted
batch budgets, and specialist failures are listed explicitly as partial
coverage in the team comment. Line-located findings are accepted only when
they point to a line actually added by the pull request.

Oversized hunks receive recalculated old/new coordinates in every fragment,
and each bounded batch is synthesised independently before reports are joined.
The collected file count is also checked against the pull request metadata so
GitHub's 3,000-file endpoint limit cannot look like complete coverage.
The deterministic team verdict is `INCOMPLETE` whenever any input gap remains
or the verification cap leaves findings unchecked; a confirmed critical still
takes precedence as `NEEDS_WORK`. Re-running a mode on the same head commit
updates its global and inline bot comments instead of publishing duplicates,
and removes inline results that are no longer confirmed by that mode.

Successful `team` reviews also keep a versioned, size-bounded cache in an
invisible HTML marker owned by the workflow bot. An identical diff reviewed by
the same workflow revision returns before any Mistral call. When only part of
the diff changes, unchanged findings reuse their previous three-lens verdict
only if their exact identity and review context still match. Invalid cache data,
workflow changes, incomplete model responses, and changed context all fall back
to fresh verification.

### Calling the reusable workflow

Default per-PR setup, the multi-agent team review plus the PR description.
The `permissions` block is required: a reusable workflow cannot escalate
beyond the ceiling set by its caller. Pin the reusable workflow to a reviewed
commit SHA. Pull requests from forks and Dependabot do not receive the Mistral
secret, so the jobs skip them explicitly.

```yaml
on:
  pull_request:
    types: [opened, synchronize]
    branches: [main]

permissions:
  contents: read
  pull-requests: write

concurrency:
  group: ai-review-${{ github.repository }}-${{ github.event.pull_request.number }}
  cancel-in-progress: true

jobs:
  team:
    if: github.event.pull_request.head.repo.full_name == github.repository
    uses: nubster-opensources/.github/.github/workflows/ai-review.yml@<reviewed-commit-sha>
    with:
      pr-number: ${{ github.event.pull_request.number }}
      mode: team
    secrets:
      mistral-api-key: ${{ secrets.MISTRAL_API_KEY }}

  describe:
    if: github.event.pull_request.head.repo.full_name == github.repository
    uses: nubster-opensources/.github/.github/workflows/ai-review.yml@<reviewed-commit-sha>
    with:
      pr-number: ${{ github.event.pull_request.number }}
      mode: describe
    secrets:
      mistral-api-key: ${{ secrets.MISTRAL_API_KEY }}
```

Any other mode (`review`, `security`, `architecture`, `performance`,
`product`) can be invoked the same way by passing it as the `mode` input.

### Dependency resolution and MSRV

The review tool currently supports Rust 1.88, as declared by its package and
verified by CI. Cargo is configured to prefer dependency versions compatible
with that declaration during updates. The blocked Octocrab 0.54.1 proposal
introduced this build dependency chain:

```text
octocrab 0.54.1
└── cargo_metadata 0.23.1
    └── cargo-platform 0.3.3 (MSRV 1.91)
```

Cargo can select `cargo-platform` 0.3.2 for Rust 1.88, but Octocrab 0.48 and
later also require `jsonwebtoken` 10 and a JWT crypto backend even though this
tool authenticates with the workflow token and never signs GitHub App JWTs.
RustCrypto introduces the unpatched RUSTSEC-2023-0071 advisory; AWS-LC requires
an additional native toolchain and fails a clean Windows build without NASM.

Octocrab 0.47.1 is therefore the newest acceptable target. It retains
`jsonwebtoken` 9, supports Rust 1.73, and builds without either extra JWT
backend. Default features are disabled, leaving only the HTTP client, Rustls,
and request timeouts. Raising the fleet MSRV is a separate policy decision
tracked in issue #28, not a dependency-update side effect.
