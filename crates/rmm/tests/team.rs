//! The mod team: a lead hands steps to the AI each role was given in Settings, on a provider or on
//! the team board for the AI app over MCP. Its own test binary, since it points rmm's settings at a
//! folder of its own through the environment.
mod common;
use common::{Screen, fake_provider, instance};
use rmm::agent::{self, Agent, Conn, Event};
use serde_json::{Value, json};
use std::time::Duration;

#[test]
fn lead_hands_steps_to_each_roles_ai() {
    let tmp = tempfile::tempdir().unwrap();
    // SAFETY: the only test in this binary, before any thread reads the environment.
    unsafe {
        std::env::set_var("RMM_CONFIG_DIR", tmp.path().join("config"));
        for v in ["ANTHROPIC_API_KEY", "DEEPSEEK_API_KEY"] {
            std::env::remove_var(v);
        }
    }
    let root = instance(tmp.path());
    let tool = |id: &str, name: &str, input: Value| json!({ "stop_reason": "tool_use", "content": [{ "type": "tool_use", "id": id, "name": name, "input": input }] });
    let said = |text: &str| json!({ "stop_reason": "end_turn", "content": [{ "type": "text", "text": text }] });
    let deepseek = |text: &str| json!({ "choices": [{ "finish_reason": "stop", "message": { "role": "assistant", "content": text } }] });
    let (base, seen) = fake_provider(vec![
        tool("t1", "ask_teammate", json!({ "role": "researcher", "task": "find snow mods" })),
        deepseek("Snow Plus scores 90."),
        said("The researcher found Snow Plus."),
        tool("t2", "ask_teammate", json!({ "role": "tester", "task": "try Snow Plus in Whiterun" })),
        said("The tester's step waits for your AI app."),
        deepseek("Install Snow Plus, then sort."),
    ]);
    // Claude leads (the provider in use), DeepSeek researches, the AI app over MCP tests.
    for p in ["claude", "deepseek"] {
        agent::configure(p, None, Some(&base), false).unwrap();
        rmm_core::write_private(&rmm_core::config_file(&format!("ai-{p}.key")).unwrap(), b"k3y").unwrap();
    }
    agent::configure("claude", None, None, true).unwrap();
    agent::set_seat("researcher", "deepseek", "deepseek-test").unwrap();
    agent::set_seat("tester", "mcp", "").unwrap();
    assert!(agent::set_seat("cook", "deepseek", "").is_err());
    assert_eq!(agent::seat("lead").provider, "claude", "an unset role follows the provider in use");

    let mut lead = Agent::new(Conn::for_seat(&agent::seat("lead")).unwrap(), root.clone()).unwrap();
    let mut screen = Screen::default();
    lead.send(Some("lead"), "snow that piles up", &[], &mut screen).unwrap();
    {
        let seen = seen.lock().unwrap();
        let names = |i: usize| -> Vec<String> { seen[i].2["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str().or(t["function"]["name"].as_str())).map(String::from).collect() };
        assert!(names(0).iter().any(|n| n == "ask_teammate") && !names(0).iter().any(|n| n == "finish_team_task"), "the lead hands work on");
        assert!(!names(1).iter().any(|n| n == "ask_teammate"), "a teammate doesn't");
        assert!(seen[0].2["messages"][0].to_string().contains("You lead rmm's mod team"), "the lead role by its name");
        assert_eq!((seen[1].0.as_str(), &seen[1].2["model"]), ("/chat/completions", &json!("deepseek-test")));
        let asked = seen[1].2["messages"][1]["content"].to_string();
        assert!(asked.contains("You are the researcher") && asked.contains("find snow mods"), "{asked}");
        let back = &seen[2].2["messages"][2]["content"][0];
        assert_eq!((&back["tool_use_id"], &back["content"][0]["text"]), (&json!("t1"), &json!("Snow Plus scores 90.")));
    }
    assert!(screen.events.iter().any(|e| matches!(e, Event::Notice { text } if text.contains("Researcher (DeepSeek · deepseek-test)"))), "{:?}", screen.events);
    let chats = agent::chats(&root).unwrap();
    assert!(chats.iter().any(|c| c.title == "Researcher: find snow mods" && c.provider == "deepseek"), "{chats:?}");

    // The tester is the AI app over MCP: the step waits on the board for it.
    lead.send(None, "now test it", &[], &mut screen).unwrap();
    let tasks = agent::tasks(&root).unwrap();
    assert_eq!((tasks.len(), tasks[0].role.as_str(), tasks[0].state.as_str(), tasks[0].from.as_str()), (1, "tester", "waiting", lead.chat.id.as_str()));
    agent::finish_task(&root, 1, "Works in Whiterun.").unwrap();
    assert!(agent::finish_task(&root, 1, "again").is_err(), "only a waiting task");
    assert_eq!(agent::tasks(&root).unwrap()[0].answer, "Works in Whiterun.");
    assert!(agent::hand_to_app(&root, "test_mods", "try the snow at night").unwrap().contains("team task 2"));

    // An app leading over MCP: a role on a provider works in the background and changes nothing.
    assert!(agent::start_teammate(root.clone(), "tester", "x", "Claude").is_err(), "the app's own role");
    assert!(agent::start_teammate(root.clone(), "lead", "x", "Claude").is_err());
    let msg = agent::start_teammate(root.clone(), "researcher", "what goes with Snow Plus", "Claude").unwrap();
    assert!(msg.starts_with("Team task 3"), "{msg}");
    let done = (0..150)
        .find_map(|_| {
            std::thread::sleep(Duration::from_millis(100));
            agent::tasks(&root).unwrap().into_iter().find(|t| t.n == 3 && t.state != "running")
        })
        .expect("the teammate finishes");
    assert_eq!((done.state.as_str(), done.answer.as_str(), done.from.as_str()), ("done", "Install Snow Plus, then sort.", "Claude"));
    assert!(seen.lock().unwrap()[5].2["messages"][1]["content"].to_string().contains("can't ask the user to approve"));
}
