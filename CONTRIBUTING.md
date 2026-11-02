# Contributing to FluxBridge

Thanks for your interest in improving FluxBridge. This document describes the
branching model, versioning scheme and commit conventions the project follows.

## Prerequisites

- Rust 1.75 or newer (developed against rustc/cargo 1.92).
- In the sandbox / CI, export a writable cargo home first:
  `export CARGO_HOME=/tmp/cargo-home`.

Before opening a pull request, make sure the workspace builds, tests pass and
clippy is clean:

```sh
cargo build  --workspace
cargo test   --workspace
cargo clippy --workspace
cargo fmt    --all
```

## Branching strategy

FluxBridge uses a three-tier branching model:

- **`main`** - always stable and releasable. Only fast-forward or reviewed merges
  from `dev` or from a `release/v*` branch land here. Every commit on `main`
  corresponds to a tagged release.
- **`dev`** - the active integration branch. Feature branches are merged here
  first. `dev` should always compile and pass tests.
- **`release/v*`** - release stabilization branches, for example
  `release/v1.2`. Cut from `dev` when preparing a release; only bug fixes and
  release-prep commits are allowed. When ready, the branch is merged to `main`
  and tagged, then merged back to `dev`.

### Feature and fix branches

Branch off `dev` using a short, descriptive prefix:

- `feat/<short-description>` for new features.
- `fix/<short-description>` for bug fixes.
- `docs/<short-description>`, `chore/<short-description>`, `refactor/<...>` as
  appropriate.

Open the pull request against `dev` (or against the relevant `release/v*` branch
for release-only fixes).

## Semantic Versioning

Releases follow [Semantic Versioning](https://semver.org/) with a `v` prefix on
the git tag: **`vX.Y.Z`**.

- **MAJOR (`X`)** - incompatible protocol or public-API changes.
- **MINOR (`Y`)** - backward-compatible functionality.
- **PATCH (`Z`)** - backward-compatible bug fixes.

Example tags: `v0.1.0`, `v0.2.0`, `v1.0.0`. The wire-protocol version byte (see
[`docs/PROTOCOL.md`](docs/PROTOCOL.md)) is bumped independently when the packet
format changes incompatibly, and such a change always implies a MAJOR release.

Pushing a tag matching `v*` triggers the CI release pipeline (added in a later
phase) which builds the optimized binaries and publishes a GitHub Release.

## Commit messages

Use [Conventional Commits](https://www.conventionalcommits.org/):

```
<type>(<optional scope>): <description>

[optional body]
[optional footer]
```

Common types: `feat`, `fix`, `docs`, `refactor`, `test`, `chore`, `perf`,
`ci`, `build`. Examples:

- `feat(protocol): add TouchEvent packet type`
- `fix(decoder): handle zero-length frames`
- `docs(readme): document CARGO_HOME requirement`

Keep commits focused and the subject line under ~72 characters. Do not use
em dashes in commit messages or documentation.

## Code style

- Run `cargo fmt --all` before committing.
- Keep the `fluxbridge-core` crate free of OS-specific and vendor-library
  dependencies; model such capabilities as traits with `#[cfg]`-gated stubs.
- Public items in `fluxbridge-core` are documented (`#![warn(missing_docs)]`).
