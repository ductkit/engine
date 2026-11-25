# CLAUDE.md

Guidance for Claude Code when working in this repository.

## Project

`ductkit/engine` — a Rust (edition 2024) Cargo workspace. All crates live under `crates/` and are picked up automatically by the `crates/*` member glob.

- `crates/core` (package `ductkit-core`, imported as `ductkit_core`) — library with the core logic. Other crates depend on it.
- `crates/sync` (package `ductkit-sync`) — the `sync` CLI binary, built on `ductkit-core`.
- `crates/stripe-mock` (package `ductkit-stripe-mock`) — the Stripe mock test fixture; see `docs/stripe-mock-spec.md`.

Packages are prefixed `ductkit-` because bare `core` clashes with Rust's built-in `core` crate; directory names stay short.

## Design docs

Specs live in `docs/`. The Stripe mock test fixture (planned crate `crates/stripe-mock`) is specified in `docs/stripe-mock-spec.md`; read it before working on the mock or on sync tests.

## Commands

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

Run `cargo fmt --all` and `cargo clippy` before committing; keep clippy warning-free.

## Conventions

- Shared metadata (version, edition, license, repository) lives in the root `[workspace.package]`; crates inherit it with `*.workspace = true`.
- Declare dependency versions once in the root `[workspace.dependencies]` and reference them from crates with `dep.workspace = true`.
- New crates: `cargo new --lib --vcs none --name ductkit-<name> crates/<name>`, then switch its metadata to workspace inheritance.
- Keep logic in libraries; binaries should stay thin.
