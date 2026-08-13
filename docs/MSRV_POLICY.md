# Open source fleet MSRV policy

The Nubster open source fleet uses a shared Minimum Supported Rust Version
(MSRV) baseline. For the August 2026 to January 2027 support cycle, that baseline
is **Rust 1.89**.

## Support window and cadence

- The baseline is reviewed twice a year, in February and August.
- Each review selects the newest Rust release that has been stable for at least
  twelve months, normally about eight stable releases behind current stable.
- The baseline must never drift more than twelve stable releases behind current
  stable. Reaching that limit triggers an out-of-cycle review.
- A repository may declare a newer MSRV when a concrete language, library, or
  security requirement needs it. It may not declare an older one.
- Emergency bumps are allowed for security or correctness fixes that cannot be
  delivered on the current baseline. The same release and documentation rules
  still apply.

The next scheduled review is February 2027. Until then, Rust 1.89 remains the
fleet floor even when newer stable releases become available.

## Release contract

`package.rust-version` is the public compatibility contract. Raising it is a
possibly breaking tooling change and is released as a minor version, including
for 0.x crates. The release changelog must name the old and new MSRV. Existing
release lines keep their published MSRV; backports must not silently raise it.

Each Rust repository must:

1. declare the effective MSRV in every publishable package;
2. pin the same patch release in `rust-toolchain.toml` for local development;
3. run a required CI job on that exact toolchain and all features;
4. configure Cargo's resolver with
   `incompatible-rust-versions = "fallback"`; and
5. document any repository-specific exception and its concrete cause.

The development pin is intentionally exact and matches the public floor.
Container actions must install or select the requested toolchain explicitly;
they must not delete, rename, or bypass `rust-toolchain.toml`.

## Current fleet

| Repository | MSRV | Decision |
| --- | ---: | --- |
| `lightshuttle` | 1.89 | 2026 fleet baseline |
| `hexeract` | 1.89 | 2026 fleet baseline |
| `nubster-cli` | 1.89 | 2026 fleet baseline |
| `.github` review tool | 1.89 | 2026 fleet baseline |
| `flaps` | 1.89 | 2026 fleet baseline |
| `aerogram` | 1.89 | 2026 fleet baseline |
| `eidosdb` | 1.89 | Baseline; also enables redb 4.x |
| `isochron` | 1.89 | 2026 fleet baseline |
| `egide` | 1.94 | Higher floor required by its current dependency set |
| `nubster-oss-template` | 1.89 | Default for newly generated repositories |

## Blocked dependency decisions

- `.github`: Octocrab 0.47.1 is retained. Newer lines force an unused JWT
  backend with either an unpatched RSA advisory or an extra native toolchain.
- `eidosdb`: proceed to redb 4.x on Rust 1.89, with an explicit redb v2-to-v3
  on-disk migration path.
- `aerogram`: proceed to `cargo-deny-action` 2.1.1. Its container bootstrap
  installs the repository-pinned toolchain; the historical failed proposal
  ultimately failed on the yanked `spin` 0.9.8 lock entry, which is now fixed.

## Cargo, toolchains, and containers

These three controls serve different purposes:

- `Cargo.toml` rejects unsupported compilers for downstream consumers.
- `rust-toolchain.toml` gives contributors and ordinary CI jobs a reproducible
  default toolchain.
- The MSRV CI job independently proves the complete workspace and all features
  still compile at the declared floor.

Cargo's MSRV-aware resolver reduces accidental transitive breakage, but it is
not a substitute for the CI gate. Likewise, a container action owns its image
and target triple; it must use its supported input or bootstrap mechanism to
install the pinned version inside that environment.
