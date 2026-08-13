# GitHub Action Pinning Policy

Every third-party GitHub Action used by an organization workflow must be pinned
to a full, immutable commit SHA and followed by a comment that records the
human-readable release version. For example:

```yaml
- uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7
```

Tags such as `@v7`, branches such as `@main`, and unannotated SHA references
are not permitted for third-party actions. The version comment keeps reviews
auditable and lets Dependabot update both the SHA and the displayed version.

Local actions (`./.github/actions/...`) and reusable workflows published by
`nubster-opensources/.github` are exempt: they are not third-party action code
and cannot be pinned with the same syntax.

Each repository must invoke the reusable
`verify-action-pinning.yml` workflow. It checks `.github/workflows` on pull
requests and pushes to `main`, and fails when an external action does not meet
this policy.
