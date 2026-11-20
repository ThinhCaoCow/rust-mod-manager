//! AI apps' sessions over MCP, kept as chats in the instance (`ai/chats/<id>.json`) so a later
//! session can read what an earlier one did (`list_chats`, `read_chat`, `save_chat_note`).
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// What happened in a session.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    /// A user's message (chats from rmm's former built-in AI have them).
    User {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role: Option<String>,
    },
    /// The AI's words: for an app, the notes it saved.
    Text { text: String },
    /// A tool call.
    Call { name: String, args: Value },
    /// Its answer, cut short.
    Result { name: String, ok: bool, text: String },
    /// About the run itself.
    Notice { text: String },
}

pub(crate) fn cut(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.into();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[cut: {} more bytes]", &s[..end], s.len() - end)
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// The provider of a chat that logs an AI app's session over MCP; its model is the app's name.
pub const MCP: &str = "mcp";

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Chat {
    pub id: String,
    pub title: String,
    pub provider: String,
    pub model: String,
    /// Seconds since 1970.
    pub updated: u64,
    #[serde(default)]
    pub log: Vec<Event>,
}

impl Chat {
    /// An AI app's session over MCP: its tool calls and the notes it saves.
    pub fn external(app: &str) -> Chat {
        // Two sessions opened in the same millisecond would share a file: each id is later than
        // the last.
        static LAST: std::sync::Mutex<u64> = std::sync::Mutex::new(0);
        let id = {
            let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
            *last = now_ms().max(*last + 1);
            *last
        };
        Chat { id: id.to_string(), title: format!("{app} over MCP"), provider: MCP.into(), model: app.into(), updated: now_ms() / 1000, ..Default::default() }
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        let f = chat_file(root, &self.id)?;
        std::fs::create_dir_all(chats_dir(root))?;
        std::fs::write(f, serde_json::to_string(self)?)?;
        Ok(())
    }

    /// As text for an AI to read: the words in full, calls and answers shortened. Over `max`
    /// bytes the end is kept, where the latest state and an app's notes are.
    pub fn transcript(&self, max: usize) -> String {
        let who = if self.provider == MCP {
            format!("{}, an AI app connected over MCP", self.model)
        } else {
            format!("rmm's former built-in AI, {} {}", self.provider, self.model)
        };
        let lines: Vec<String> = self
            .log
            .iter()
            .map(|e| match e {
                Event::User { text, role } => format!("User{}: {text}", role.as_deref().map(|r| format!(" ({r})")).unwrap_or_default()),
                Event::Text { text } => format!("AI: {text}"),
                Event::Call { name, args } => format!("Call {name} {}", cut(&args.to_string(), 300)),
                Event::Result { ok: true, text, .. } => format!("  -> {}", cut(text, 500)),
                Event::Result { text, .. } => format!("  -> failed: {}", cut(text, 500)),
                Event::Notice { text } => format!("({text})"),
            })
            .collect();
        let mut size: usize = lines.iter().map(|l| l.len() + 1).sum();
        let mut skip = 0;
        while size > max && skip < lines.len() {
            size -= lines[skip].len() + 1;
            skip += 1;
        }
        let left_out = if skip > 0 { format!("[{skip} earlier entries left out]\n") } else { String::new() };
        format!("{who}: \"{}\"\n{left_out}{}", self.title, lines[skip..].join("\n"))
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ChatInfo {
    pub id: String,
    pub title: String,
    pub provider: String,
    pub model: String,
    pub updated: u64,
}

fn chats_dir(root: &Path) -> PathBuf {
    root.join("ai").join("chats")
}

fn chat_file(root: &Path, id: &str) -> Result<PathBuf> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
        bail!("no chat {id:?}");
    }
    Ok(chats_dir(root).join(format!("{id}.json")))
}

/// The instance's chats, newest first.
// ponytail: reads every chat whole; an index file if lists get slow.
pub fn chats(root: &Path) -> Result<Vec<ChatInfo>> {
    let mut out: Vec<ChatInfo> = match std::fs::read_dir(chats_dir(root)) {
        Ok(rd) => rd.flatten().filter_map(|e| serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok()).collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
        Err(e) => return Err(e.into()),
    };
    out.sort_by_key(|c| std::cmp::Reverse(c.updated));
    Ok(out)
}

pub fn load_chat(root: &Path, id: &str) -> Result<Chat> {
    let f = chat_file(root, id)?;
    serde_json::from_slice(&std::fs::read(&f).with_context(|| format!("no chat {id}"))?).with_context(|| format!("chat {id} is damaged"))
}
