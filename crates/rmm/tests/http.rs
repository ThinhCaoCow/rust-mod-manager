use rmm_core::{Game, Instance};
use serde_json::{Value, json};
use std::io::{Read, Write};

/// One HTTP request; returns the status, the session header and the JSON body.
fn send(addr: &str, method: &str, headers: &[(&str, &str)], body: &Value) -> (u16, Option<String>, Value) {
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    let body = if body.is_null() { String::new() } else { body.to_string() };
    let mut req = format!("{method} /mcp HTTP/1.1\r\nContent-Length: {}\r\n", body.len());
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    write!(s, "{req}\r\n{body}").unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let (head, body) = out.split_once("\r\n\r\n").unwrap();
    let status = head[9..12].parse().unwrap();
    let sid = head.lines().find_map(|l| l.strip_prefix("Mcp-Session-Id: ")).map(str::to_owned);
    (status, sid, serde_json::from_str(body).unwrap_or(Value::Null))
}

#[test]
fn mcp_over_http() {
    let tmp = tempfile::tempdir().unwrap();
    let game = tmp.path().join("game");
    std::fs::create_dir_all(game.join("Data")).unwrap();
    std::fs::write(game.join("SkyrimSE.exe"), "exe").unwrap();
    let root = tmp.path().join("inst");
    Instance::create(&root, Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();

    let server = rmm::http::start(root.clone(), "127.0.0.1:0", "t0ken".into()).unwrap();
    let addr = server.addr.to_string();
    let host = ("Host", addr.as_str());
    let auth = ("Authorization", "Bearer t0ken");
    let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "1" } } });

    assert_eq!(send(&addr, "POST", &[host], &init).0, 401, "no token");
    assert_eq!(send(&addr, "POST", &[host, ("Authorization", "Bearer t0kem")], &init).0, 401, "wrong token");
    assert_eq!(send(&addr, "POST", &[host, auth, ("Origin", "https://example.com")], &init).0, 403, "from a web page");
    assert_eq!(send(&addr, "POST", &[("Host", "evil.example:80"), auth], &init).0, 403, "rebound DNS name");
    assert_eq!(send(&addr, "GET", &[host, auth], &Value::Null).0, 405, "no event stream");
    assert_eq!(send(&addr, "POST", &[host, auth], &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })).0, 400, "no session");

    let (status, sid, reply) = send(&addr, "POST", &[host, auth], &init);
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["result"]["serverInfo"]["name"], "rmm");
    let sid = sid.expect("a session id");
    let session = ("Mcp-Session-Id", sid.as_str());
    assert_eq!(send(&addr, "POST", &[host, auth, session], &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).0, 202);

    let (_, _, tools) = send(&addr, "POST", &[host, auth, session], &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }));
    let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.contains(&"list_mods") && names.contains(&"deploy"), "{names:?}");

    let (status, _, r) = send(&addr, "POST", &[host, auth, session], &json!({ "jsonrpc": "2.0", "id": "c1", "method": "tools/call", "params": { "name": "list_mods", "arguments": {} } }));
    assert_eq!(status, 200);
    assert_eq!(r["id"], "c1");
    assert_eq!(r["result"]["isError"], false, "{r}");

    // The session is kept as a chat: its calls, and the note it saves (not logged as a call).
    let note = json!({ "jsonrpc": "2.0", "id": "c2", "method": "tools/call", "params": { "name": "save_chat_note", "arguments": { "text": "Checked the mods.\nNothing left." } } });
    let (_, _, r) = send(&addr, "POST", &[host, auth, session], &note);
    assert_eq!(r["result"]["isError"], false, "{r}");
    let chats = rmm::agent::chats(&root).unwrap();
    assert_eq!(chats.len(), 1);
    assert_eq!((chats[0].title.as_str(), chats[0].provider.as_str(), chats[0].model.as_str()), ("Checked the mods.", "mcp", "test"));
    let text = rmm::agent::load_chat(&root, &chats[0].id).unwrap().transcript(10_000);
    assert!(text.contains("Call list_mods {}") && text.contains("AI: Checked the mods.\nNothing left.") && !text.contains("save_chat_note"), "{text}");

    assert_eq!(send(&addr, "DELETE", &[host, auth, session], &Value::Null).0, 200);
    assert_eq!(send(&addr, "POST", &[host, auth, session], &json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list" })).0, 404, "ended");
    drop(server);
    assert!(std::net::TcpStream::connect(&addr).is_err(), "stopped");
}
