# CLAUDE.md

Guidance for Claude Code when working in this repository.

## Project

`ductkit/engine` — a Rust (edition 2024) Cargo workspace. All crates live under `crates/` and are picked up automatically by the `crates/*` member glob.

- `crates/engine-core` — library with the core logic. Other crates depend on it.
- `crates/engine-cli` — binary that wraps `engine-core`.

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
- New crates: `cargo new --lib --vcs none crates/<name>`, then switch its metadata to workspace inheritance.
- Keep logic in libraries; binaries should stay thin.
