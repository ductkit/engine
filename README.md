# engine

The ductkit engine, organised as a multi-crate Rust workspace.

## Layout

| Crate | Kind | Purpose |
|-------|------|---------|
| [`ductkit-core`](crates/core) | library | Core engine logic |
| [`ductkit-sync`](crates/sync) | binary | `sync` command-line tool |

## Getting started

Requires a recent stable Rust toolchain (edition 2024).

```sh
cargo build          # build every crate
cargo test           # run all tests
cargo run -p ductkit-sync
```

## License

Dual-licensed under MIT or Apache-2.0, at your option.
