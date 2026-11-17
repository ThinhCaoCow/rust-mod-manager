//! The MCP tools over streamable HTTP on 127.0.0.1, for clients that connect to a URL. The window
//! hosts it too; it notices the AI's changes like any other outside change.
//!
//! A subset of the streamable HTTP transport (MCP 2025-06-18): each POST carries one JSON-RPC
//! message and gets a JSON answer (requests) or 202 (notifications, responses). There is no SSE
//! stream, so GET gets 405, which the spec allows. `initialize` starts a session
//! (`Mcp-Session-Id`); DELETE ends it. Every request needs `Authorization: Bearer <token>`, and a
//! browser's `Origin` or a Host other than the loopback address is refused (DNS rebinding).
use crate::mcp::Rmm;
use anyhow::{Context, Result, bail};
use rmcp::ServiceExt;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

const MAX_HEAD: usize = 64 << 10;
const MAX_BODY: usize = 16 << 20;

/// The bearer token clients send, kept in the user's config folder so client settings stay valid
/// across restarts. Made on first use.
pub fn token() -> Result<String> {
    let path = rmm_core::config_file("mcp-token").context("no config folder to keep the MCP token in")?;
    if let Ok(t) = std::fs::read_to_string(&path)
        && t.trim().len() >= 32
    {
        return Ok(t.trim().to_string());
    }
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("no randomness for the MCP token: {e}"))?;
    let t: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    rmm_core::write_private(&path, t.as_bytes())?;
    Ok(t)
}

/// A running server; dropping it stops it.
pub struct Http {
    pub addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Http {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Session {
    to_server: tokio::sync::Mutex<WriteHalf<DuplexStream>>,
    /// Requests waiting for their answer, by JSON-RPC id.
    waiting: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
}

struct Shared {
    rmm: Rmm,
    token: String,
    port: u16,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

/// Serve on `addr` (a loopback address; port 0 picks one) on a thread of its own.
pub fn start(root: PathBuf, addr: &str, token: String) -> Result<Http> {
    let listener = std::net::TcpListener::bind(addr).with_context(|| format!("cannot listen on {addr}"))?;
    let addr = listener.local_addr()?;
    if !addr.ip().is_loopback() {
        bail!("the MCP server only listens on this computer (127.0.0.1), not {}", addr.ip());
    }
    listener.set_nonblocking(true)?;
    let shared = Arc::new(Shared { rmm: Rmm::new(root), token, port: addr.port(), sessions: Default::default() });
    let (stop, mut stopped) = oneshot::channel();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let thread = std::thread::spawn(move || {
        rt.block_on(async move {
            let Ok(listener) = tokio::net::TcpListener::from_std(listener) else { return };
            loop {
                tokio::select! {
                    _ = &mut stopped => break,
                    Ok((stream, _)) = listener.accept() => {
                        let shared = shared.clone();
                        tokio::spawn(async move { let _ = connection(stream, shared).await; });
                    }
                }
            }
        });
        rt.shutdown_background();
    });
    Ok(Http { addr, stop: Some(stop), thread: Some(thread) })
}

struct Request {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

async fn read_request(stream: &mut TcpStream) -> Result<Option<Request>> {
    let mut buf = Vec::new();
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > MAX_HEAD {
            bail!("headers too long");
        }
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split_whitespace();
    let (method, path) = (first.next().unwrap_or("").to_string(), first.next().unwrap_or("").to_string());
    let headers: HashMap<String, String> =
        lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
    if headers.contains_key("transfer-encoding") {
        bail!("chunked bodies are not supported");
    }
    let len: usize = headers.get("content-length").map(|v| v.parse()).transpose()?.unwrap_or(0);
    if len > MAX_BODY {
        bail!("body too large");
    }
    let mut body = buf[end + 4..].to_vec();
    while body.len() < len {
        let mut chunk = vec![0u8; (len - body.len()).min(1 << 16)];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            bail!("body cut short");
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    Ok(Some(Request { method, path, headers, body }))
}

async fn respond(stream: &mut TcpStream, status: u16, extra: &[(&str, &str)], body: &Value) -> Result<()> {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    let body = if status == 202 { String::new() } else { body.to_string() };
    let mut head = format!("HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

fn error(msg: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32600, "message": msg } })
}

/// Same length and bytes, without stopping at the first difference.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn connection(mut stream: TcpStream, shared: Arc<Shared>) -> Result<()> {
    let req = match read_request(&mut stream).await {
        Ok(Some(r)) => r,
        Ok(None) => return Ok(()),
        Err(e) => return respond(&mut stream, 400, &[], &error(&format!("{e:#}"))).await,
    };
    let h = |k: &str| req.headers.get(k).map(String::as_str);
    if h("origin").is_some() {
        return respond(&mut stream, 403, &[], &error("requests from web pages are refused")).await;
    }
    let port = shared.port;
    if !matches!(h("host"), Some(host) if [format!("127.0.0.1:{port}"), format!("localhost:{port}"), format!("[::1]:{port}")].iter().any(|ok| host.eq_ignore_ascii_case(ok))) {
        return respond(&mut stream, 403, &[], &error("unexpected Host header")).await;
    }
    if !h("authorization").and_then(|a| a.strip_prefix("Bearer ")).is_some_and(|t| same(t.trim(), &shared.token)) {
        return respond(&mut stream, 401, &[("WWW-Authenticate", "Bearer")], &error("missing or wrong bearer token")).await;
    }
    if req.path.split('?').next() != Some("/mcp") {
        return respond(&mut stream, 404, &[], &error("the endpoint is /mcp")).await;
    }
    let sid = h("mcp-session-id").map(str::to_owned);
    match req.method.as_str() {
        "POST" => {}
        "DELETE" => {
            let gone = sid.and_then(|s| shared.sessions.lock().unwrap().remove(&s));
            if let Some(s) = &gone {
                // End of input stops its server, which ends the task reading its answers.
                let _ = s.to_server.lock().await.shutdown().await;
            }
            return respond(&mut stream, if gone.is_some() { 200 } else { 404 }, &[], &json!({})).await;
        }
        _ => return respond(&mut stream, 405, &[("Allow", "POST, DELETE")], &error("POST JSON-RPC messages; there is no event stream")).await,
    }
    let Ok(msg) = serde_json::from_slice::<Value>(&req.body) else {
        return respond(&mut stream, 400, &[], &error("the body is not JSON")).await;
    };
    if !msg.is_object() {
        return respond(&mut stream, 400, &[], &error("send one JSON-RPC message per request")).await;
    }
    let method = msg["method"].as_str().map(str::to_owned);
    let (sid, session) = if method.as_deref() == Some("initialize") {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("{e}"))?;
        let sid: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let session = open_session(&shared);
        shared.sessions.lock().unwrap().insert(sid.clone(), session.clone());
        (sid, session)
    } else {
        let Some(sid) = sid else {
            return respond(&mut stream, 400, &[], &error("missing Mcp-Session-Id: initialize first")).await;
        };
        let Some(session) = shared.sessions.lock().unwrap().get(&sid).cloned() else {
            return respond(&mut stream, 404, &[], &error("unknown session: initialize again")).await;
        };
        (sid, session)
    };
    let id = msg.get("id").filter(|_| method.is_some()).map(Value::to_string);
    let answer = id.as_ref().map(|id| {
        let (tx, rx) = oneshot::channel();
        session.waiting.lock().unwrap().insert(id.clone(), tx);
        rx
    });
    {
        let mut w = session.to_server.lock().await;
        w.write_all(format!("{msg}\n").as_bytes()).await?;
    }
    let headers = [("Mcp-Session-Id", sid.as_str())];
    let Some(rx) = answer else {
        return respond(&mut stream, 202, &headers, &Value::Null).await;
    };
    let reply = rx.await.unwrap_or_else(|_| error("the session ended"));
    respond(&mut stream, 200, &headers, &reply).await
}

/// An rmcp server on an in-memory pipe, and a task that hands its answers to the waiting requests.
fn open_session(shared: &Shared) -> Arc<Session> {
    let (ours, theirs) = tokio::io::duplex(1 << 20);
    // Each session keeps its own chat.
    let server = shared.rmm.clone().recorded_if_set();
    tokio::spawn(async move {
        if let Ok(running) = server.serve(tokio::io::split(theirs)).await {
            let _ = running.waiting().await;
        }
    });
    let (from_server, to_server) = tokio::io::split(ours);
    let waiting: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>> = Default::default();
    let pending = waiting.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(from_server).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            // Server-sent requests and notifications have nowhere to go without a stream.
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            if v.get("method").is_none()
                && let Some(tx) = v.get("id").and_then(|id| pending.lock().unwrap().remove(&id.to_string()))
            {
                let _ = tx.send(v);
            }
        }
    });
    Arc::new(Session { to_server: tokio::sync::Mutex::new(to_server), waiting })
}
