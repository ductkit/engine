# engine

The ductkit engine, organised as a multi-crate Rust workspace.

## Layout

| Crate | Kind | Purpose |
|-------|------|---------|
| [`engine-core`](crates/engine-core) | library | Core engine logic |
| [`engine-cli`](crates/engine-cli) | binary | Command-line entry point |

## Getting started

Requires a recent stable Rust toolchain (edition 2024).

```sh
cargo build          # build every crate
cargo test           # run all tests
cargo run -p engine-cli
```

## License

Dual-licensed under MIT or Apache-2.0, at your option.
