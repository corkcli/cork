//! Replays every golden scenario in `schema/vectors/` against the Engine.

use std::path::Path;

use cork_protocol_conformance::{load_vectors, run_vector};

#[test]
fn every_vector_replays() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../schema/vectors");
    let vectors = load_vectors(&dir);
    assert!(vectors.len() >= 8, "expected the golden vectors in {dir:?}");
    let mut failed = Vec::new();
    for (name, v) in &vectors {
        if let Err(e) = run_vector(v) {
            failed.push(format!("{name}: {e}"));
        }
    }
    assert!(failed.is_empty(), "\n{}", failed.join("\n\n"));
}
