//! The reference transport: the Engine behind tokio-tungstenite. One task per
//! connection, the engine behind one mutex (it never blocks on IO except the
//! log append), a timer for grace expiry. `cork-protocol-server` runs this;
//! cork-rs wires the same Engine into axum instead.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::engine::{ConnId, Engine, Outgoing};
use crate::frames::ServerFrame;

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

struct Shared {
    engine: Engine,
    conns: HashMap<ConnId, UnboundedSender<(String, bool)>>,
}

/// A running server's engine, for inspection (tests, metrics).
#[derive(Clone)]
pub struct Handle(Arc<Mutex<Shared>>);

impl Handle {
    pub fn with_engine<R>(&self, f: impl FnOnce(&mut Engine) -> R) -> R {
        f(&mut self.0.lock().unwrap().engine)
    }
}

fn dispatch(shared: &Shared, outs: Vec<Outgoing>) {
    for o in outs {
        let fatal = matches!(&o.frame, ServerFrame::Error(e) if e.fatal);
        if let Some(tx) = shared.conns.get(&o.conn) {
            let _ = tx.send((o.frame.encode(), fatal));
        }
    }
}

/// Serve until the listener fails. Returns a handle immediately through
/// `spawn`; `serve` itself runs forever.
pub async fn serve(listener: TcpListener, engine: Engine) -> std::io::Result<()> {
    let (handle, fut) = spawn(listener, engine);
    let _ = handle;
    fut.await
}

/// Start serving; the handle sees the engine, the future runs the server.
pub fn spawn(listener: TcpListener, engine: Engine) -> (Handle, impl std::future::Future<Output = std::io::Result<()>>) {
    let shared = Arc::new(Mutex::new(Shared { engine, conns: HashMap::new() }));
    let handle = Handle(shared.clone());
    let fut = async move {
        let ticker = shared.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let s = &mut *ticker.lock().unwrap();
                let now = now_ms();
                if s.engine.next_deadline().is_some_and(|d| d <= now) {
                    let outs = s.engine.tick(now);
                    dispatch(s, outs);
                }
            }
        });
        let next = Arc::new(AtomicU64::new(1));
        loop {
            let (stream, _) = listener.accept().await?;
            let id = next.fetch_add(1, Ordering::Relaxed);
            tokio::spawn(connection(shared.clone(), id, stream));
        }
    };
    (handle, fut)
}

async fn connection(shared: Arc<Mutex<Shared>>, id: ConnId, stream: TcpStream) {
    let _ = stream.set_nodelay(true);
    let Ok(ws) = tokio_tungstenite::accept_async(stream).await else { return };
    let (mut sink, mut source) = ws.split();
    let (tx, mut rx) = unbounded_channel::<(String, bool)>();
    {
        let s = &mut *shared.lock().unwrap();
        s.conns.insert(id, tx);
        let outs = s.engine.connected(id, now_ms());
        dispatch(s, outs);
    }
    let writer = tokio::spawn(async move {
        while let Some((text, fatal)) = rx.recv().await {
            if sink.send(WsMessage::Text(text)).await.is_err() {
                break;
            }
            if fatal {
                let _ = sink.send(WsMessage::Close(None)).await;
                break;
            }
        }
    });
    while let Some(msg) = source.next().await {
        match msg {
            Ok(WsMessage::Text(t)) => {
                let s = &mut *shared.lock().unwrap();
                let outs = s.engine.handle_text(id, &t, now_ms());
                dispatch(s, outs);
            }
            Ok(WsMessage::Close(_)) | Err(_) => break,
            Ok(_) => {}
        }
        if writer.is_finished() {
            break;
        }
    }
    let s = &mut *shared.lock().unwrap();
    s.conns.remove(&id);
    let outs = s.engine.disconnected(id, now_ms());
    dispatch(s, outs);
}
