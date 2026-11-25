//! Generates the Stripe types from the vendored OpenAPI spec and checks that the committed
//! files in `src/stripe/generated/` are up to date (design section 6.2).
//!
//! With `DUCTKIT_REGEN=1` the test writes the files instead of comparing them.

mod codegen;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const REGENERATE: &str = "DUCTKIT_REGEN=1 cargo test -p ductkit-stripe-mock --test openapi_codegen";

fn generated_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/stripe/generated")
}

fn load_spec() -> serde_json::Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/openapi/spec3.json");
    let bytes = std::fs::read(path).expect("vendored spec is missing");
    serde_json::from_slice(&bytes).expect("vendored spec is not JSON")
}

/// Relative paths (with `/`) of every file under `dir`.
fn files_under(dir: &Path) -> BTreeSet<String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeSet<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let rel = path.strip_prefix(root).unwrap();
                let parts: Vec<_> = rel.iter().map(|p| p.to_string_lossy()).collect();
                out.insert(parts.join("/"));
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(dir, dir, &mut out);
    out
}

fn write_all(dir: &Path, files: &BTreeMap<String, String>) {
    for stale in files_under(dir) {
        if !files.contains_key(&stale) {
            std::fs::remove_file(dir.join(&stale)).unwrap();
        }
    }
    for (rel, text) in files {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let unchanged = std::fs::read(&path).is_ok_and(|old| old == text.as_bytes());
        if !unchanged {
            std::fs::write(&path, text).unwrap();
        }
    }
}

#[test]
fn generated_code_is_fresh() {
    let files = codegen::generate(&load_spec()).unwrap();
    let dir = generated_dir();
    if std::env::var_os("DUCTKIT_REGEN").is_some_and(|v| v == "1") {
        write_all(&dir, &files);
        return;
    }
    let on_disk = files_under(&dir);
    let mut problems = Vec::new();
    for rel in &on_disk {
        if !files.contains_key(rel) {
            problems.push(format!("  {rel}: not generated (stale file)"));
        }
    }
    for (rel, text) in &files {
        match std::fs::read(dir.join(rel)) {
            Err(_) => problems.push(format!("  {rel}: missing")),
            Ok(bytes) if bytes != text.as_bytes() => {
                problems.push(format!("  {rel}: differs"));
            }
            Ok(_) => {}
        }
    }
    assert!(
        problems.is_empty(),
        "src/stripe/generated/ is out of date with openapi/spec3.json:\n{}\nregenerate with: {REGENERATE}",
        problems.join("\n")
    );
}
