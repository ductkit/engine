# Vendored Stripe OpenAPI spec

`spec3.json` is the schema the mock generates its Rust types from.

- Source repo: `stripe/openapi`
- Path: `latest/openapi.spec3.json`
- Commit: `0f6d4dfafe157c10df393ce8a27d00b186ab3955`
- Size: 5,190,895 bytes
- SHA-256: `e1fd0c9ed163b6ec53594ac30d60b251d7cef5a0ee911760bc89cdceafbe227b`
- `info.version`: `2026-11-18.endive`

Do not take the file from tag `v2602`. It is a different (older) schema and
does not match `PINNED_API_VERSION`.

## Updating

1. Replace `spec3.json` with the new file from `stripe/openapi`.
2. Update the size and SHA-256 in `tests/openapi_vendored.rs` and in this file.
3. Regenerate: `DUCTKIT_REGEN=1 cargo test -p ductkit-stripe-mock --test openapi_codegen`.
4. Fix whatever breaks.
5. Bump `PINNED_API_VERSION` and `docs/stripe-mock-spec.md` section 7.1.
