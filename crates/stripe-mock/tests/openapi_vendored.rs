use sha2::{Digest, Sha256};

const SPEC_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/openapi/spec3.json");

#[test]
fn vendored_spec_is_the_pinned_file() {
    let bytes = std::fs::read(SPEC_PATH).expect("vendored spec is missing");
    assert_eq!(bytes.len(), 5_190_895);
    let hex: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(
        hex,
        "e1fd0c9ed163b6ec53594ac30d60b251d7cef5a0ee911760bc89cdceafbe227b"
    );
}

#[test]
fn vendored_spec_version_is_pinned() {
    let bytes = std::fs::read(SPEC_PATH).expect("vendored spec is missing");
    let spec: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        spec["info"]["version"],
        ductkit_stripe_mock::PINNED_API_VERSION
    );
}
