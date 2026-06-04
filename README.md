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

### Calling the reusable workflow

Default per-PR setup, the multi-agent team review plus the PR description.
The `permissions` block is required: a reusable workflow cannot escalate
beyond the ceiling set by its caller.

```yaml
on:
  pull_request:
    types: [opened, synchronize]
    branches: [main]

permissions:
  contents: read
  pull-requests: write

jobs:
  team:
    uses: nubster-opensources/.github/.github/workflows/ai-review.yml@main
    with:
      pr-number: ${{ github.event.pull_request.number }}
      mode: team
    secrets:
      mistral-api-key: ${{ secrets.MISTRAL_API_KEY }}

  describe:
    uses: nubster-opensources/.github/.github/workflows/ai-review.yml@main
    with:
      pr-number: ${{ github.event.pull_request.number }}
      mode: describe
    secrets:
      mistral-api-key: ${{ secrets.MISTRAL_API_KEY }}
```

Any other mode (`review`, `security`, `architecture`, `performance`,
`product`) can be invoked the same way by passing it as the `mode` input.
