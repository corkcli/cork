//! A small blocking client over tungstenite, for synchronous callers such as
//! the hooks: connect + hello, then `write`, `check`, `lease`, `promote`, each
//! waiting for its answer. Frames that arrive meanwhile (deltas, messages)
//! wait in an inbox until `drain` or `recv`.

use std::collections::VecDeque;
use std::net::TcpStream;
use std::time::{Duration, Instant};

use serde_json::Value;
use thiserror::Error;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message as WsMessage, WebSocket};

use crate::client::{Client, ClientConfig};
use crate::frames::*;

#[derive(Debug, Error)]
pub enum WsError {
    #[error("websocket: {0}")]
    Ws(Box<tungstenite::Error>),
    #[error("timed out waiting for the server")]
    Timeout,
    #[error("the server closed the connection")]
    Closed,
    #[error("server error {}: {}", .0.code, .0.message)]
    Server(ErrorFrame),
    #[error("undecodable frame: {0}")]
    Decode(String),
}

impl From<tungstenite::Error> for WsError {
    fn from(e: tungstenite::Error) -> Self {
        WsError::Ws(Box::new(e))
    }
}

pub struct BlockingClient {
    ws: WebSocket<MaybeTlsStream<TcpStream>>,
    url: String,
    pub client: Client,
    inbox: VecDeque<ServerFrame>,
    epoch: Instant,
    epoch_ms: u64,
}

/// Called with the server's url and each frame's text as it is sent: the
/// hooks keep a log of what left the machine (`cork audit`).
static TAP: std::sync::OnceLock<fn(&str, &str)> = std::sync::OnceLock::new();

/// Install the outbound tap, once per process; later calls are ignored.
pub fn set_tap(tap: fn(&str, &str)) {
    let _ = TAP.set(tap);
}

fn wall_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn set_timeout(ws: &mut WebSocket<MaybeTlsStream<TcpStream>>, t: Option<Duration>) {
    let t = t.map(|t| t.max(Duration::from_millis(1)));
    #[allow(unreachable_patterns)]
    match ws.get_mut() {
        MaybeTlsStream::Plain(s) => {
            let _ = s.set_read_timeout(t);
        }
        #[cfg(feature = "ws-client-blocking-tls")]
        MaybeTlsStream::Rustls(s) => {
            let _ = s.sock.set_read_timeout(t);
        }
        _ => {}
    }
}

impl BlockingClient {
    /// Connect to `url` (`ws://` or, with the tls feature, `wss://`), say
    /// hello and wait for `welcome`.
    pub fn connect(url: &str, cfg: ClientConfig, resume: bool, timeout: Duration) -> Result<(Self, Welcome), WsError> {
        let (ws, _) = tungstenite::connect(url)?;
        let mut me = BlockingClient {
            ws, url: url.to_string(), client: Client::new(cfg), inbox: VecDeque::new(), epoch: Instant::now(), epoch_ms: wall_ms(),
        };
        let hello = me.client.hello(resume);
        me.send(&hello)?;
        let w = me.wait(timeout, |f| match f {
            ServerFrame::Welcome(w) => Some(Ok(w.clone())),
            ServerFrame::Error(e) => Some(Err(e.clone())),
            _ => None,
        })?;
        match w {
            Ok(w) => Ok((me, w)),
            Err(e) => Err(WsError::Server(e)),
        }
    }

    pub fn now_ms(&self) -> u64 {
        self.epoch_ms + self.epoch.elapsed().as_millis() as u64
    }

    pub fn send(&mut self, f: &ClientFrame) -> Result<(), WsError> {
        let text = f.encode();
        if let Some(tap) = TAP.get() {
            tap(&self.url, &text);
        }
        self.ws.send(WsMessage::Text(text))?;
        Ok(())
    }

    fn read(&mut self, timeout: Duration) -> Result<Option<ServerFrame>, WsError> {
        set_timeout(&mut self.ws, Some(timeout));
        match self.ws.read() {
            Ok(WsMessage::Text(t)) => {
                let f = ServerFrame::decode(&t).map_err(|e| WsError::Decode(e.to_string()))?;
                let now = self.now_ms();
                self.client.receive(&f, now);
                Ok(Some(f))
            }
            Ok(WsMessage::Close(_)) => Err(WsError::Closed),
            Ok(_) => Ok(None),
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => Ok(None),
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => Err(WsError::Closed),
            Err(e) => Err(e.into()),
        }
    }

    /// Read until `pick` returns Some; other frames go to the inbox.
    fn wait<T>(&mut self, timeout: Duration, mut pick: impl FnMut(&ServerFrame) -> Option<T>) -> Result<T, WsError> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(WsError::Timeout);
            }
            if let Some(f) = self.read(left)? {
                match pick(&f) {
                    Some(t) => return Ok(t),
                    None => self.inbox.push_back(f),
                }
            }
        }
    }

    /// The next frame: from the inbox, else from the socket within `timeout`.
    pub fn recv(&mut self, timeout: Duration) -> Result<Option<ServerFrame>, WsError> {
        if let Some(f) = self.inbox.pop_front() {
            return Ok(Some(f));
        }
        self.read(timeout)
    }

    /// Everything waiting, without blocking for more than a moment.
    pub fn drain(&mut self) -> Vec<ServerFrame> {
        while let Ok(Some(f)) = self.read(Duration::from_millis(1)) {
            self.inbox.push_back(f);
        }
        self.inbox.drain(..).collect()
    }

    /// Send one op and wait for its ack or reject.
    pub fn write(&mut self, op: Op, timeout: Duration) -> Result<Result<Ack, Reject>, WsError> {
        let id = op.op_id.clone();
        let now = self.now_ms();
        for f in self.client.submit(op, now) {
            self.send(&f)?;
        }
        self.wait(timeout, |f| match f {
            ServerFrame::Ack(a) if a.op_id == id => Some(Ok(Ok(a.clone()))),
            ServerFrame::Reject(r) if r.op_id == id => Some(Ok(Err(r.clone()))),
            ServerFrame::Error(e) if e.req.as_deref() == Some(id.as_str()) => Some(Err(e.clone())),
            _ => None,
        })?
        .map_err(WsError::Server)
    }

    fn req_of(f: &ClientFrame) -> Option<String> {
        match f {
            ClientFrame::Lease(l) => l.req.clone(),
            ClientFrame::Check(c) => c.req.clone(),
            ClientFrame::Promote(p) => p.req.clone(),
            ClientFrame::Release(r) => r.req.clone(),
            _ => None,
        }
    }

    /// The pre-write gate for `keys` from this client's bases.
    pub fn check(&mut self, keys: &[&str], kind: Option<&str>, timeout: Duration) -> Result<CheckResult, WsError> {
        let f = self.client.check(keys, kind);
        let req = Self::req_of(&f);
        self.send(&f)?;
        self.wait(timeout, |f| match f {
            ServerFrame::CheckResult(c) if c.req == req => Some(Ok(c.clone())),
            ServerFrame::Error(e) if e.req == req => Some(Err(e.clone())),
            _ => None,
        })?
        .map_err(WsError::Server)
    }

    pub fn lease(
        &mut self,
        keys: &[&str],
        kinds: &[&str],
        intent: Option<Intent>,
        timeout: Duration,
    ) -> Result<Result<LeaseGranted, LeaseDenied>, WsError> {
        let f = self.client.lease(keys, kinds, intent);
        let req = Self::req_of(&f);
        self.send(&f)?;
        self.wait(timeout, |f| match f {
            ServerFrame::LeaseGranted(g) if g.req == req => Some(Ok(Ok(g.clone()))),
            ServerFrame::LeaseDenied(d) if d.req == req => Some(Ok(Err(d.clone()))),
            ServerFrame::Error(e) if e.req == req => Some(Err(e.clone())),
            _ => None,
        })?
        .map_err(WsError::Server)
    }

    pub fn promote(&mut self, op_ids: Vec<String>, evidence: Value, timeout: Duration) -> Result<Promoted, WsError> {
        let f = self.client.promote(op_ids, evidence);
        let req = Self::req_of(&f);
        self.send(&f)?;
        self.wait(timeout, |f| match f {
            ServerFrame::Promoted(p) if p.req == req => Some(Ok(p.clone())),
            ServerFrame::Error(e) if e.req == req => Some(Err(e.clone())),
            _ => None,
        })?
        .map_err(WsError::Server)
    }

    pub fn close(mut self) {
        let _ = self.ws.close(None);
        let _ = self.ws.flush();
    }
}
