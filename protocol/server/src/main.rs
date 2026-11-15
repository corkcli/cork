//! cork-protocol-server: the reference server. The Engine with the code,
//! notes and tasks domains, an in-memory store and log, over plain WebSockets.
//!
//!     cork-protocol-server [--addr 127.0.0.1:7878] [--grace-ms 5000]
//!
//! Any path is accepted; every connection speaks the protocol. Not for
//! production: no TLS, no auth, nothing durable. cork-rs embeds the same
//! Engine behind its own auth, Postgres log and axum.

use cork_protocol::{Config, Engine};
use cork_protocol_code::CodeDomain;
use cork_protocol_notes::NotesDomain;
use cork_protocol_tasks::TasksDomain;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mut addr = "127.0.0.1:7878".to_string();
    let mut cfg = Config::default();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--addr" => addr = args.next().expect("--addr needs a value"),
            "--grace-ms" => cfg.grace_ms = args.next().and_then(|v| v.parse().ok()).expect("--grace-ms needs a number"),
            "-h" | "--help" => {
                println!("cork-protocol-server [--addr 127.0.0.1:7878] [--grace-ms 5000]");
                return Ok(());
            }
            other => panic!("unknown argument {other}"),
        }
    }
    let engine = Engine::builder().config(cfg).domain(CodeDomain::new()).domain(NotesDomain::new())
        .domain(TasksDomain::new())
        .build();
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    eprintln!("cork-protocol-server listening on ws://{addr}");
    cork_protocol::ws_server::serve(listener, engine).await
}
