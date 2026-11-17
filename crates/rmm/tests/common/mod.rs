//! A fake AI provider, a screen that records what the agent shows, and a small instance.
#![allow(dead_code)] // each test file uses some of these
use rmm::agent::{self, Answer, Event, Ui};
use rmm_core::{Game, Instance};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::{Arc, Mutex};

pub type Seen = Arc<Mutex<Vec<(String, HashMap<String, String>, Value)>>>;

/// Answers each POST with the next reply, and keeps what was sent.
pub fn fake_provider(replies: Vec<Value>) -> (String, Seen) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen: Seen = Default::default();
    let log = seen.clone();
    std::thread::spawn(move || {
        for (reply, stream) in replies.into_iter().zip(listener.incoming()) {
            let mut r = BufReader::new(stream.unwrap());
            let mut first = String::new();
            r.read_line(&mut first).unwrap();
            let mut headers = HashMap::new();
            loop {
                let mut l = String::new();
                r.read_line(&mut l).unwrap();
                let Some((k, v)) = l.trim_end().split_once(':') else { break };
                headers.insert(k.to_ascii_lowercase(), v.trim().to_string());
            }
            let mut body = vec![0; headers.get("content-length").map_or(0, |n| n.parse().unwrap())];
            r.read_exact(&mut body).unwrap();
            log.lock().unwrap().push((first.split(' ').nth(1).unwrap().to_string(), headers, serde_json::from_slice(&body).unwrap_or_default()));
            let out = reply.to_string();
            write!(r.get_mut(), "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}", out.len()).unwrap();
        }
    });
    (base, seen)
}

/// Keeps every event and request; declines every change.
#[derive(Default)]
pub struct Screen {
    pub events: Vec<Event>,
    pub asked: Vec<String>,
    pub exchanges: Vec<agent::Exchange>,
}

impl Ui for Screen {
    fn event(&mut self, e: Event) {
        self.events.push(e);
    }
    fn approve(&mut self, name: &str, _: &Value) -> Answer {
        self.asked.push(name.into());
        Answer::No
    }
    fn exchange(&mut self, x: &agent::Exchange) {
        self.exchanges.push(x.clone());
    }
}

/// A Skyrim SE instance with one mod, Snow.
pub fn instance(tmp: &std::path::Path) -> std::path::PathBuf {
    let game = tmp.join("game");
    std::fs::create_dir_all(game.join("Data")).unwrap();
    std::fs::write(game.join("SkyrimSE.exe"), "exe").unwrap();
    std::fs::create_dir_all(tmp.join("src/Snow/textures")).unwrap();
    std::fs::write(tmp.join("src/Snow/textures/snow.dds"), "x").unwrap();
    let inst = Instance::create(&tmp.join("inst"), Game::SkyrimSe, &game, &tmp.join("local")).unwrap();
    rmm_core::install::install(&inst, &tmp.join("src/Snow"), None, true, None).unwrap();
    inst.root
}
