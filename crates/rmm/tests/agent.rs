//! The built-in AI against a fake provider: requests in each API's shape, tool calls through rmm's
//! MCP server, approvals, pictures.
mod common;
use common::{Screen, fake_provider, instance};
use rmm::agent::{self, Agent, Conn, Event};
use serde_json::{Value, json};

#[test]
fn claude_calls_tools_and_asks_before_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = instance(tmp.path());
    let tool = |id: &str, name: &str, input: Value| json!({ "stop_reason": "tool_use", "content": [
        { "type": "thinking", "thinking": "", "signature": "sig" },
        { "type": "tool_use", "id": id, "name": name, "input": input }] });
    let (base, seen) = fake_provider(vec![
        tool("t1", "list_mods", json!({})),
        tool("t2", "deploy", json!({})),
        tool("t3", "set_mods_enabled", json!({ "ids": ["Snow"], "enabled": false })),
        json!({ "stop_reason": "end_turn", "content": [{ "type": "text", "text": "Snow is installed." }],
            "usage": { "input_tokens": 1000, "output_tokens": 100, "cache_read_input_tokens": 2000, "cache_creation_input_tokens": 0 } }),
    ]);
    let mut ai = Agent::new(Conn::new(agent::provider("anthropic").unwrap(), "claude-opus-5-5", &base, "k3y"), root.clone()).unwrap();
    let mut screen = Screen::default();
    ai.send(None, "What's installed?", &[], &mut screen).unwrap();

    let seen = seen.lock().unwrap();
    let (path, headers, first) = &seen[0];
    assert_eq!(path, "/v1/messages");
    assert_eq!((headers["x-api-key"].as_str(), headers["anthropic-version"].as_str()), ("k3y", "2026-10-21"));
    assert_eq!(headers["anthropic-beta"], "server-side-fallback-2026-07-01");
    assert_eq!((&first["fallbacks"], &first["output_config"]["effort"]), (&json!("default"), &json!("high")));
    assert!(first["system"].as_str().unwrap().contains("rmm manages one"), "the server's instructions");
    let names: Vec<&str> = first["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.contains(&"list_mods") && names.contains(&"view_image") && !names.contains(&"save_chat_note"), "{names:?}");
    assert!(first["tools"][0]["input_schema"].get("$schema").is_none());

    let listed = &seen[1].2["messages"][2]["content"][0];
    assert_eq!((&listed["tool_use_id"], &listed["is_error"]), (&json!("t1"), &json!(false)));
    assert!(listed["content"][0]["text"].as_str().unwrap().contains("Snow"), "{listed}");
    assert_eq!(seen[1].2["messages"][1]["content"][0]["type"], "thinking", "assistant turns go back whole");
    assert_eq!(seen[2].2["messages"][4]["content"][0]["is_error"], false, "a dry run needs no yes");
    let declined = &seen[3].2["messages"][6]["content"][0];
    assert_eq!((&declined["is_error"], &declined["content"][0]["text"]), (&json!(true), &json!("The user declined this call.")));
    assert_eq!(screen.asked, ["set_mods_enabled"]);
    assert!(matches!(screen.events.last(), Some(Event::Text { text }) if text == "Snow is installed."));

    // Each request reaches the inspector, trimmed; the chat is saved with its log and its cost.
    assert_eq!(screen.exchanges.len(), 4);
    assert_eq!(screen.exchanges[0].request["tools"], format!("[{} tools]", names.len()));
    let chats = agent::chats(&root).unwrap();
    assert_eq!((chats.len(), chats[0].title.as_str()), (1, "What's installed?"));
    let chat = agent::load_chat(&root, &chats[0].id).unwrap();
    assert!(matches!(&chat.log[0], Event::User { text, .. } if text == "What's installed?"));
    assert_eq!((chat.messages.len(), chat.usage.input, chat.usage.cache_read), (8, 1000, 2000));
    // Opus 5.5: $4 in, $20 out, $0.20 cache reads, per million tokens.
    assert!((chat.usage.cost.unwrap() - 0.0064).abs() < 1e-9, "{:?}", chat.usage.cost);
    assert!(Agent::resume(Conn::new(agent::provider("openai").unwrap(), "m", &base, "k"), root.clone(), chat).is_err(), "a chat stays with its provider");
    assert!(agent::load_chat(&root, "../x").is_err());
}

#[test]
fn openai_style_tools_and_pictures() {
    let tmp = tempfile::tempdir().unwrap();
    let root = instance(tmp.path());
    let shot = tmp.path().join("shot.png");
    std::fs::write(&shot, b"\x89PNG fake").unwrap();
    let call = |id: &str, name: &str, args: Value| json!({ "id": id, "type": "function", "function": { "name": name, "arguments": args.to_string() } });
    let (base, seen) = fake_provider(vec![
        json!({ "choices": [{ "finish_reason": "tool_calls", "message": { "role": "assistant", "content": null, "reasoning_content": "think",
            "tool_calls": [call("c1", "list_mods", json!({})), call("c2", "view_image", json!({ "path": shot }))] } }] }),
        json!({ "choices": [{ "finish_reason": "stop", "message": { "role": "assistant", "content": "One mod, and the picture is a fake." } }] }),
    ]);
    let mut ai = Agent::new(Conn::new(agent::provider("deepseek").unwrap(), "deepseek-v4-pro", &base, "k3y"), root).unwrap();
    let mut screen = Screen::default();
    let pic = agent::Image::from_data_url("data:image/png;base64,iVBORw0KGgo=").unwrap();
    ai.send(None, "Look", &[pic], &mut screen).unwrap();

    let seen = seen.lock().unwrap();
    assert_eq!((seen[0].0.as_str(), seen[0].1["authorization"].as_str()), ("/chat/completions", "Bearer k3y"));
    let first = &seen[0].2;
    assert_eq!((&first["model"], &first["messages"][0]["role"]), (&json!("deepseek-v4-pro"), &json!("system")));
    assert_eq!(first["messages"][1]["content"][0]["image_url"]["url"], "data:image/png;base64,iVBORw0KGgo=");
    assert_eq!(first["tools"][0]["type"], "function");
    let m = seen[1].2["messages"].as_array().unwrap();
    assert_eq!(m[2]["reasoning_content"], "think", "DeepSeek's reasoning goes back");
    assert_eq!((&m[3]["role"], &m[3]["tool_call_id"]), (&json!("tool"), &json!("c1")));
    assert!(m[3]["content"].as_str().unwrap().contains("Snow"));
    assert_eq!(m[5]["content"][1]["image_url"]["url"], "data:image/png;base64,iVBORyBmYWtl", "view_image's picture follows the tool answers");
    assert!(screen.asked.is_empty());

    // Only pictures can be attached.
    assert!(agent::Image::from_data_url("data:text/plain;base64,eA==").is_err());
}
