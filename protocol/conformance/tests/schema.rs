//! frames.schema.json against reality: every frame a vector sends, and every
//! frame the engine sends while replaying them, must validate; and the
//! schema names exactly the frame tags the Rust types have.

use std::collections::BTreeSet;
use std::path::Path;

use cork_protocol_conformance::{load_vectors, run_vector_collect, substitute, validate};
use serde_json::Value;

fn schema() -> Value {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../schema/frames.schema.json");
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

#[test]
fn vector_frames_validate_both_ways() {
    let s = schema();
    let client = s["$defs"]["ClientFrame"].clone();
    let server = s["$defs"]["ServerFrame"].clone();
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../schema/vectors");
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut sent: BTreeSet<String> = BTreeSet::new();
    for (name, v) in load_vectors(&dir) {
        for (i, step) in v["steps"].as_array().unwrap().iter().enumerate() {
            if let Some(f) = step.get("send") {
                // $ref placeholders stand for hashes: validate with a string there
                let f = substitute(f, &Default::default())
                    .unwrap_or_else(|_| serde_json::from_str(&f.to_string().replace("\"$ref:", "\"ref-")).unwrap());
                validate(&client, &s, &f, &format!("{name} step {i} send")).unwrap();
                sent.insert(f["t"].as_str().unwrap().to_string());
            }
        }
        for (j, f) in run_vector_collect(&v).unwrap().iter().enumerate() {
            validate(&server, &s, f, &format!("{name} recv {j}")).unwrap();
            seen.insert(f["t"].as_str().unwrap().to_string());
        }
    }
    let want: BTreeSet<String> =
        s["$defs"].as_object().unwrap().keys().filter_map(|k| k.strip_prefix("server_").map(str::to_string)).collect();
    assert_eq!(seen, want, "the vectors exercise every server frame");
    assert!(sent.len() >= 8, "client frames exercised: {sent:?}");
}

#[test]
fn schema_tags_match_the_rust_frames() {
    use cork_protocol::*;
    let s = schema();
    let tags = |prefix: &str| -> BTreeSet<String> {
        s["$defs"].as_object().unwrap().keys().filter_map(|k| k.strip_prefix(prefix).map(str::to_string)).collect()
    };
    let client: BTreeSet<String> = [
        ClientFrame::Hello(Hello {
            proto: 1,
            session: "s".into(),
            agent_id: "a".into(),
            person: "p".into(),
            scope: "x".into(),
            token: None,
            resume: false,
        }),
        ClientFrame::Op(Op::default()),
        ClientFrame::Batch(Batch { batch_id: "b".into(), atomic: true, ops: vec![] }),
        ClientFrame::Lease(Lease::default()),
        ClientFrame::Release(Release::default()),
        ClientFrame::Subscribe(Subscribe::default()),
        ClientFrame::Unsubscribe(Unsubscribe::default()),
        ClientFrame::Check(Check::default()),
        ClientFrame::Promote(Promote::default()),
        ClientFrame::Message(SendMessage::default()),
    ]
    .iter()
    .map(|f| serde_json::to_value(f).unwrap()["t"].as_str().unwrap().to_string())
    .collect();
    assert_eq!(client, tags("client_"));
    let e = ServerFrame::Error(ErrorFrame { code: "c".into(), message: "m".into(), fatal: false, req: None });
    assert!(tags("server_").contains(e.tag()));
    assert_eq!(tags("server_").len(), 11);
}
