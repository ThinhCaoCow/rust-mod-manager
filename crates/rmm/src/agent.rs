//! rmm's own AI: its MCP tools and mod team roles, driven by a model the user picks with their own
//! API key. Claude goes through Anthropic's Messages API; OpenRouter, OpenAI, Kimi, DeepSeek and Qwen
//! through their OpenAI-compatible chat completions. Reads and dry runs run straight away; a call
//! that changes something waits for the user's yes. Chats are kept in the instance (`ai/chats/`).
use crate::mcp::Rmm;
use anyhow::{Context, Result, bail};
use base64::Engine;
use rmcp::ServiceExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Api {
    /// Anthropic's Messages API (`/v1/messages`).
    Anthropic,
    /// Chat completions (`/chat/completions`), which most providers copy from OpenAI.
    OpenAi,
}

pub struct Provider {
    pub id: &'static str,
    pub name: &'static str,
    pub api: Api,
    pub base: &'static str,
    /// The model used until the user picks another; any id from `models` works.
    pub model: &'static str,
    /// The environment variable the provider's own tools read the key from; it wins over a saved key.
    pub env: &'static str,
}

pub const PROVIDERS: &[Provider] = &[
    Provider { id: "openrouter", name: "OpenRouter", api: Api::OpenAi, base: "https://openrouter.ai/api/v1", model: "openrouter/auto", env: "OPENROUTER_API_KEY" },
    Provider { id: "claude", name: "Claude (Anthropic)", api: Api::Anthropic, base: "https://api.anthropic.com", model: "claude-opus-5-5", env: "ANTHROPIC_API_KEY" },
    Provider { id: "openai", name: "GPT (OpenAI)", api: Api::OpenAi, base: "https://api.openai.com/v1", model: "gpt-6-astra", env: "OPENAI_API_KEY" },
    Provider { id: "kimi", name: "Kimi (Moonshot AI)", api: Api::OpenAi, base: "https://api.moonshot.ai/v1", model: "kimi-k3.1-pro", env: "MOONSHOT_API_KEY" },
    Provider { id: "deepseek", name: "DeepSeek", api: Api::OpenAi, base: "https://api.deepseek.com", model: "deepseek-v4-pro", env: "DEEPSEEK_API_KEY" },
    Provider { id: "qwen", name: "Qwen (Alibaba Cloud)", api: Api::OpenAi, base: "https://dashscope-intl.aliyuncs.com/compatible-mode/v1", model: "qwen3-max", env: "DASHSCOPE_API_KEY" },
];

const ANTHROPIC_VERSION: &str = "2026-10-21";
/// Claude models that take `output_config.effort`; agent work runs at high unless the user picks.
const EFFORT: &[&str] = &["claude-opus-5", "claude-sonnet-5", "claude-haiku-5", "claude-fable-5", "claude-mythos-5"];
const CLAUDE_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];
/// Claude models with server-side fallbacks: when one declines, another carries on in the same call.
const FALLBACKS: &[&str] = &["claude-fable-5-2", "claude-fable-5-1", "claude-opus-5-5", "claude-opus-5", "claude-sonnet-5-5"];
/// Tool answers longer than this are cut before they reach the model.
const MAX_RESULT: usize = 40_000;

const SYSTEM: &str = "You are the AI inside rmm, a mod manager for Skyrim SE and Fallout 4, working on the user's \
setup through rmm's tools. Calls that only read, and dry runs, run straight away. The user approves each call that \
changes something, so make changes in small reversible steps and say what each one is for. A declined call is the \
user's decision: don't retry it; ask what they want instead. view_image shows you a picture file, such as a \
screenshot path from test_session_step. The user works in rmm's IDE: list_files, read_file and edit_file reach \
mod files, overwrite/ and the game's INIs, and the user sees each edit as a diff. ask_teammate hands a step to the \
researcher, installer or tester, each on the AI the user chose for it. Keep answers short and plain.";

/// Told to a teammate an AI app started over MCP, where nobody can approve its changes.
const UNATTENDED: &str = "The lead started you from an AI app over MCP, where rmm can't ask the user to approve \
changes: read and preview freely, and report each change you'd make, with the tool and its arguments, for the lead to make.";

pub fn provider(id: &str) -> Result<&'static Provider> {
    let id = id.trim().to_ascii_lowercase();
    let id = match id.as_str() {
        "anthropic" => "claude",
        "gpt" | "chatgpt" => "openai",
        "moonshot" => "kimi",
        "dashscope" | "alibaba" => "qwen",
        x => x,
    };
    PROVIDERS.iter().find(|p| p.id == id).with_context(|| format!("no AI provider {id:?}; the providers are {}", PROVIDERS.iter().map(|p| p.id).collect::<Vec<_>>().join(", ")))
}

/// A provider's request settings; unset ones use the provider's (or rmm's) default.
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct Options {
    /// Added to rmm's own instructions.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub instructions: String,
    /// Claude: low, medium, high (rmm's default), xhigh or max. Others: sent as `reasoning_effort`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Not for Claude, whose current models take no sampling settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

/// Which provider is in use, and models, servers and settings other than a provider's defaults.
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct Settings {
    pub provider: Option<String>,
    #[serde(default)]
    pub models: BTreeMap<String, String>,
    /// Qwen's China or US region, Kimi's China site, or a proxy.
    #[serde(default)]
    pub bases: BTreeMap<String, String>,
    #[serde(default)]
    pub options: BTreeMap<String, Options>,
    /// Who does each mod team role; a role not here goes to the provider in use.
    #[serde(default)]
    pub team: BTreeMap<String, Seat>,
}

pub fn settings() -> Settings {
    rmm_core::config_file("ai.json").and_then(|f| std::fs::read_to_string(f).ok()).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save_settings(s: &Settings) -> Result<()> {
    let f = rmm_core::config_file("ai.json").context("no config folder to keep the settings in")?;
    std::fs::create_dir_all(f.parent().unwrap_or(&f))?;
    std::fs::write(f, serde_json::to_string_pretty(s)?)?;
    Ok(())
}

/// Set `id`'s model or server (an empty one puts back its default), and with `switch` use it from now on.
pub fn configure(id: &str, model: Option<&str>, base: Option<&str>, switch: bool) -> Result<Settings> {
    let p = provider(id)?;
    let mut s = settings();
    if switch {
        s.provider = Some(p.id.into());
    }
    for (map, value) in [(&mut s.models, model), (&mut s.bases, base)] {
        match value.map(str::trim) {
            Some("") => {
                map.remove(p.id);
            }
            Some(v) => {
                map.insert(p.id.into(), v.trim_end_matches('/').into());
            }
            None => {}
        }
    }
    if let Some(b) = s.bases.get(p.id) {
        // The key goes to this server: never in the clear over the network.
        if !["https://", "http://localhost", "http://127.0.0.1", "http://[::1]"].iter().any(|ok| b.starts_with(ok)) {
            bail!("the server must be an https:// address (or http://localhost)");
        }
    }
    save_settings(&s)?;
    Ok(s)
}

/// Replace a provider's request settings.
pub fn set_options(id: &str, mut o: Options) -> Result<Settings> {
    let p = provider(id)?;
    o.instructions = o.instructions.trim().to_string();
    o.effort = o.effort.map(|e| e.trim().to_ascii_lowercase()).filter(|e| !e.is_empty());
    if let (Api::Anthropic, Some(e)) = (p.api, &o.effort)
        && !CLAUDE_EFFORTS.contains(&e.as_str())
    {
        bail!("Claude's effort is one of {}", CLAUDE_EFFORTS.join(", "));
    }
    if o.temperature.is_some_and(|t| !(0.0..=2.0).contains(&t)) {
        bail!("the temperature goes from 0 to 2");
    }
    if o.max_tokens.is_some_and(|n| n == 0 || n > 128_000) {
        bail!("the most output tokens is 1 to 128000");
    }
    let mut s = settings();
    if o == Options::default() {
        s.options.remove(p.id);
    } else {
        s.options.insert(p.id.into(), o);
    }
    save_settings(&s)?;
    Ok(s)
}

/// The mod team: each role's name in Settings, its prompt on the MCP server, and what the user sees.
pub const ROLES: &[(&str, &str, &str)] = &[("lead", "mod_team", "Lead"), ("researcher", "find_mods", "Researcher"), ("installer", "install_safely", "Installer"), ("tester", "test_mods", "Tester")];

/// A role by its name or its prompt's.
pub fn role(name: &str) -> Result<&'static (&'static str, &'static str, &'static str)> {
    ROLES.iter().find(|r| r.0 == name || r.1 == name).with_context(|| format!("no role {name:?}; the roles are lead, researcher, installer and tester"))
}

/// Who does a role: one of rmm's providers with the user's key (and a model, empty for the
/// provider's own), or `MCP`: the AI app connected to rmm over MCP.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Seat {
    pub provider: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
}

/// Who does `role` (a ROLES name; "chat" is a message without one): as set, else the provider in use.
pub fn seat(role: &str) -> Seat {
    let s = settings();
    s.team.get(role).cloned().unwrap_or_else(|| Seat { provider: s.provider.unwrap_or_default(), model: String::new() })
}

/// Give a role to a provider (with a model, or "" for its own) or to `MCP`; "" gives it back to the provider in use.
pub fn set_seat(name: &str, provider_id: &str, model: &str) -> Result<Settings> {
    let &(key, ..) = role(name)?;
    let mut s = settings();
    match provider_id.trim() {
        "" => {
            s.team.remove(key);
        }
        MCP => {
            s.team.insert(key.into(), Seat { provider: MCP.into(), model: String::new() });
        }
        id => {
            s.team.insert(key.into(), Seat { provider: provider(id)?.id.into(), model: model.trim().into() });
        }
    }
    save_settings(&s)?;
    Ok(s)
}

/// Each role and who does it; `set` is false for a role following the provider in use.
pub fn team() -> Vec<Value> {
    let s = settings();
    ROLES.iter().map(|&(key, prompt, name)| json!({ "role": key, "prompt": prompt, "name": name, "seat": seat(key), "set": s.team.contains_key(key) })).collect()
}

fn key_file(p: &Provider) -> Option<PathBuf> {
    rmm_core::config_file(&format!("ai-{}.key", p.id))
}

fn key(p: &Provider) -> Option<String> {
    std::env::var(p.env).ok().or_else(|| std::fs::read_to_string(key_file(p)?).ok()).map(|k| k.trim().to_string()).filter(|k| !k.is_empty())
}

fn base(p: &Provider, s: &Settings) -> String {
    s.bases.get(p.id).map(String::as_str).unwrap_or(p.base).trim_end_matches('/').to_string()
}

/// Check the key with the provider (by listing its models), then save it. Returns how many models it offers.
pub fn set_key(id: &str, key: &str) -> Result<usize> {
    let p = provider(id)?;
    let key = key.trim();
    if key.is_empty() || key.chars().any(|c| c.is_control() || c.is_whitespace() || c == '"' || c == '\\') {
        bail!("that doesn't look like an API key");
    }
    let n = Conn::new(p, p.model, &base(p, &settings()), key).models()?.len();
    rmm_core::write_private(&key_file(p).context("no config folder to keep the key in")?, key.as_bytes())?;
    Ok(n)
}

pub fn forget_key(id: &str) -> Result<bool> {
    match key_file(provider(id)?).map(std::fs::remove_file) {
        Some(Ok(())) => Ok(true),
        Some(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Some(Err(e)) => Err(e.into()),
        None => Ok(false),
    }
}

#[derive(Serialize, Debug)]
pub struct Status {
    pub id: &'static str,
    pub name: &'static str,
    /// "anthropic" or "openai" (chat completions).
    pub api: &'static str,
    pub model: String,
    pub base: String,
    /// "saved", or the environment variable it comes from; none without a key.
    pub key: Option<String>,
    pub current: bool,
    pub options: Options,
}

/// Every provider with its model, server, settings and whether a key is there (never the key itself).
pub fn status() -> Vec<Status> {
    let s = settings();
    PROVIDERS
        .iter()
        .map(|p| Status {
            id: p.id,
            name: p.name,
            api: if p.api == Api::Anthropic { "anthropic" } else { "openai" },
            model: s.models.get(p.id).cloned().unwrap_or_else(|| p.model.into()),
            base: base(p, &s),
            key: if std::env::var(p.env).is_ok_and(|k| !k.trim().is_empty()) {
                Some(p.env.into())
            } else {
                key_file(p).filter(|f| f.exists()).map(|_| "saved".into())
            },
            current: s.provider.as_deref() == Some(p.id),
            options: s.options.get(p.id).cloned().unwrap_or_default(),
        })
        .collect()
}

/// A provider, model, server, settings and key, ready for requests.
pub struct Conn {
    pub provider: &'static Provider,
    pub model: String,
    pub base: String,
    pub options: Options,
    key: String,
}

impl Conn {
    /// `id` (or the provider in use) with its key, and `model` or the one set for it.
    pub fn open(id: Option<&str>, model: Option<&str>) -> Result<Conn> {
        let s = settings();
        let id = id.or(s.provider.as_deref()).context("pick an AI provider first (openrouter, claude, openai, kimi, deepseek or qwen): `rmm ai use <provider>`")?;
        let p = provider(id)?;
        let key = key(p).with_context(|| format!("no {} API key: `rmm ai key {}` saves one (or set {})", p.name, p.id, p.env))?;
        let model = model.map(str::to_string).or_else(|| s.models.get(p.id).cloned()).unwrap_or_else(|| p.model.into());
        Ok(Conn { provider: p, model, base: base(p, &s), options: s.options.get(p.id).cloned().unwrap_or_default(), key })
    }

    /// The AI in a seat. The AI app over MCP can't be called from here.
    pub fn for_seat(seat: &Seat) -> Result<Conn> {
        if seat.provider == MCP {
            bail!("that role is the AI app connected over MCP, which rmm can't call");
        }
        Conn::open(Some(seat.provider.as_str()).filter(|p| !p.is_empty()), Some(seat.model.as_str()).filter(|m| !m.is_empty()))
    }

    pub fn new(provider: &'static Provider, model: &str, base: &str, key: &str) -> Conn {
        Conn { provider, model: model.into(), base: base.trim_end_matches('/').into(), options: Options::default(), key: key.into() }
    }

    fn auth(&self) -> Vec<(&'static str, String)> {
        match self.provider.api {
            Api::Anthropic => vec![("x-api-key", self.key.clone()), ("anthropic-version", ANTHROPIC_VERSION.into())],
            Api::OpenAi => vec![("Authorization", format!("Bearer {}", self.key))],
        }
    }

    /// The model ids the key can use.
    pub fn models(&self) -> Result<Vec<String>> {
        let url = match self.provider.api {
            Api::Anthropic => format!("{}/v1/models?limit=1000", self.base),
            Api::OpenAi => format!("{}/models", self.base),
        };
        let v = http(self.provider, &url, &self.auth(), None)?;
        let mut ids: Vec<String> = v["data"].as_array().into_iter().flatten().filter_map(|m| m["id"].as_str().map(String::from)).collect();
        ids.sort();
        Ok(ids)
    }
}

fn message_of(body: &str) -> String {
    let v: Value = serde_json::from_str(body).unwrap_or_default();
    [&v["error"]["message"], &v["error"], &v["message"]]
        .iter()
        .find_map(|m| m.as_str().map(str::to_owned))
        .unwrap_or_else(|| body.chars().take(300).collect())
}

/// Deletes the request body once curl has sent it.
struct TempFile(PathBuf);
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// One request through curl, as for Nexus. The key goes to curl on stdin and the body through a file,
/// so neither is on its command line (which is also too short for a body with pictures).
fn http(p: &Provider, url: &str, headers: &[(&str, String)], body: Option<&Value>) -> Result<Value> {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut config = format!("user-agent = \"rmm/{}\"\n", env!("CARGO_PKG_VERSION"));
    for (k, v) in headers {
        config += &format!("header = \"{k}: {v}\"\n");
    }
    let file = match body {
        Some(b) => {
            let f = TempFile(std::env::temp_dir().join(format!("rmm-ai-{}-{}.json", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed))));
            std::fs::write(&f.0, b.to_string())?;
            Some(f)
        }
        None => None,
    };
    let data = file.as_ref().map(|f| format!("@{}", f.0.display()));
    for attempt in 0..3 {
        let mut args = vec!["-sS", "--connect-timeout", "30", "--max-time", "900", "-w", "\n%{http_code}", "-K", "-"];
        if let Some(d) = &data {
            args.extend(["-H", "Content-Type: application/json", "-H", "Expect:", "--data-binary", d]);
        }
        args.extend(["--", url]);
        let mut child = Command::new("curl").args(&args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().context("cannot run curl")?;
        child.stdin.take().context("no stdin for curl")?.write_all(config.as_bytes())?;
        let out = child.wait_with_output()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let (body, code) = text.rsplit_once('\n').unwrap_or(("", &text));
        let code = code.trim();
        // Busy or rate limited: wait and try again (5 s, then 20 s).
        if matches!(code, "429" | "500" | "502" | "503" | "504" | "529") && attempt < 2 {
            std::thread::sleep(std::time::Duration::from_secs(5 << (2 * attempt)));
            continue;
        }
        return match code {
            "200" => serde_json::from_str(body).with_context(|| format!("{} sent something that isn't JSON", p.name)),
            "401" => bail!("{} refused the API key ({}); save it again with `rmm ai key {}`", p.name, message_of(body), p.id),
            "000" | "" => bail!("couldn't reach {}: {}", p.name, String::from_utf8_lossy(&out.stderr).trim()),
            c => bail!("{} answered {c}: {}", p.name, message_of(body)),
        };
    }
    unreachable!()
}

/// Tokens one request used, and what it cost when that's known.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// US dollars: OpenRouter reports it; Claude's comes from Anthropic's list prices.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
}

impl Usage {
    fn add(&mut self, o: &Usage) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
        self.cost = match (self.cost, o.cost) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
        };
    }

    fn of(api: Api, model: &str, v: &Value) -> Usage {
        let u = &v["usage"];
        let n = |x: &Value| x.as_u64().unwrap_or(0);
        match api {
            Api::Anthropic => {
                let mut usage = Usage { input: n(&u["input_tokens"]), output: n(&u["output_tokens"]), cache_read: n(&u["cache_read_input_tokens"]), cache_write: n(&u["cache_creation_input_tokens"]), cost: None };
                usage.cost = claude_cost(model, &usage);
                usage
            }
            Api::OpenAi => {
                // Cached prompt tokens are part of the prompt's count (DeepSeek reports them its own way).
                let cached = u["prompt_tokens_details"]["cached_tokens"].as_u64().or(u["prompt_cache_hit_tokens"].as_u64()).unwrap_or(0);
                Usage { input: n(&u["prompt_tokens"]).saturating_sub(cached), output: n(&u["completion_tokens"]), cache_read: cached, cache_write: 0, cost: u["cost"].as_f64() }
            }
        }
    }
}

/// Anthropic's list prices in dollars per million tokens (input, output, cache reads); cache writes
/// cost 1.25 times input. Other models: no cost shown.
fn claude_cost(model: &str, u: &Usage) -> Option<f64> {
    let prompt = u.input + u.cache_read + u.cache_write;
    let (i, o, r) = match model {
        "claude-fable-5-2" => (8.0, 40.0, 0.20),
        "claude-fable-5-1" | "claude-mythos-5-1" => (10.0, 50.0, 0.25),
        "claude-fable-5" | "claude-mythos-5" => (10.0, 50.0, 1.0),
        "claude-opus-5-5" => (4.0, 20.0, 0.20),
        "claude-opus-5" => (5.0, 25.0, 0.50),
        "claude-sonnet-5-5" => (2.0, 10.0, 0.10),
        "claude-sonnet-5" => (2.0, 10.0, 0.20),
        "claude-haiku-5-5" if prompt <= 100_000 => (0.10, 0.50, 0.01),
        "claude-haiku-5-5" => (0.50, 2.50, 0.05),
        _ => return None,
    };
    Some((u.input as f64 * i + u.output as f64 * o + u.cache_read as f64 * r + u.cache_write as f64 * i * 1.25) / 1e6)
}

/// One request and its answer, for the inspector. The request is trimmed: the tools are counted,
/// only the last messages are kept, and pictures are left out.
#[derive(Serialize, Clone, Debug)]
pub struct Exchange {
    /// Milliseconds since 1970.
    pub at: u64,
    pub model: String,
    pub url: String,
    pub ms: u64,
    pub request: Value,
    pub response: Value,
    pub usage: Usage,
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// Pictures in base64 replaced by their size.
fn elide(v: &mut Value) {
    match v {
        Value::Object(o) => {
            for (k, x) in o.iter_mut() {
                match x {
                    Value::String(s) if (k == "data" && s.len() > 256) || (k == "url" && s.starts_with("data:")) => *x = format!("[picture, {} KB]", s.len() * 3 / 4 / 1024).into(),
                    _ => elide(x),
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(elide),
        _ => {}
    }
}

fn trimmed(body: &Value) -> Value {
    let mut b = body.clone();
    if let Some(t) = b.get_mut("tools") {
        *t = format!("[{} tools]", t.as_array().map_or(0, Vec::len)).into();
    }
    if let Some(m) = b.get_mut("messages").and_then(Value::as_array_mut) {
        let n = m.len();
        if n > 4 {
            m.drain(..n - 4);
            m.insert(0, format!("[{} earlier messages]", n - 4).into());
        }
    }
    elide(&mut b);
    b
}

/// A picture for the model: one the user attaches, or one `view_image` opens.
#[derive(Clone, Debug)]
pub struct Image {
    pub media_type: String,
    /// Base64.
    pub data: String,
}

/// Providers take pictures up to 5 MB once in base64.
const MAX_IMAGE: usize = 3_750_000;

impl Image {
    pub fn load(path: &Path) -> Result<Image> {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        let media_type = match ext.as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            _ => bail!("{} isn't a PNG, JPEG, GIF or WebP picture", path.display()),
        };
        let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
        if bytes.len() > MAX_IMAGE {
            bail!("{} is over 3.75 MB, more than the providers take", path.display());
        }
        Ok(Image { media_type: media_type.into(), data: base64::engine::general_purpose::STANDARD.encode(bytes) })
    }

    /// From a `data:image/...;base64,` URL, as a web page reads a file.
    pub fn from_data_url(url: &str) -> Result<Image> {
        let (head, data) = url.strip_prefix("data:").and_then(|u| u.split_once(";base64,")).context("not a base64 data URL")?;
        if !["image/png", "image/jpeg", "image/gif", "image/webp"].contains(&head) {
            bail!("pictures must be PNG, JPEG, GIF or WebP, not {head}");
        }
        if data.len() > MAX_IMAGE / 3 * 4 + 4 {
            bail!("the picture is over 3.75 MB, more than the providers take");
        }
        Ok(Image { media_type: head.into(), data: data.into() })
    }

    fn anthropic(&self) -> Value {
        json!({ "type": "image", "source": { "type": "base64", "media_type": self.media_type, "data": self.data } })
    }

    fn openai(&self) -> Value {
        json!({ "type": "image_url", "image_url": { "url": format!("data:{};base64,{}", self.media_type, self.data) } })
    }
}

/// rmm's MCP server in this process, spoken to as a client speaks to `rmm mcp` over stdio.
struct Tools {
    rt: Option<tokio::runtime::Runtime>,
    w: WriteHalf<DuplexStream>,
    r: Lines<BufReader<ReadHalf<DuplexStream>>>,
    id: u64,
}

impl Tools {
    fn start(root: PathBuf) -> Result<(Tools, Value)> {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
        let (ours, theirs) = tokio::io::duplex(1 << 16);
        rt.spawn(async move {
            if let Ok(server) = Rmm::new(root).serve(tokio::io::split(theirs)).await {
                let _ = server.waiting().await;
            }
        });
        let (r, w) = tokio::io::split(ours);
        let mut t = Tools { rt: Some(rt), w, r: BufReader::new(r).lines(), id: 0 };
        let init = t.rpc("initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "rmm-ai", "version": env!("CARGO_PKG_VERSION") } }))?;
        t.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))?;
        Ok((t, init))
    }

    fn send(&mut self, msg: &Value) -> Result<()> {
        let line = format!("{msg}\n");
        let w = &mut self.w;
        self.rt.as_ref().context("stopped")?.block_on(async {
            w.write_all(line.as_bytes()).await?;
            w.flush().await
        })?;
        Ok(())
    }

    fn rpc(&mut self, method: &str, params: Value) -> Result<Value> {
        self.id += 1;
        let id = self.id;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;
        let r = &mut self.r;
        let v = self.rt.as_ref().context("stopped")?.block_on(async {
            loop {
                let line = r.next_line().await?.context("rmm's tool server stopped")?;
                let v: Value = serde_json::from_str(&line)?;
                if v["id"] == id {
                    return anyhow::Ok(v);
                }
            }
        })?;
        if let Some(e) = v.get("error") {
            bail!("{}", e["message"].as_str().unwrap_or("the call failed"));
        }
        Ok(v["result"].clone())
    }
}

impl Drop for Tools {
    // Without waiting, so dropping is safe inside another runtime (the desktop app's).
    fn drop(&mut self) {
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

/// What happens during a turn, for the screen; a chat keeps them to draw it again.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    /// The user's message as typed (a role's instructions aren't repeated).
    User {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role: Option<String>,
        #[serde(default)]
        pictures: usize,
    },
    /// The model's words to the user.
    Text { text: String },
    /// A tool call starting (after its approval).
    Call { name: String, args: Value },
    /// Its answer, cut short for the screen.
    Result { name: String, ok: bool, text: String },
    /// About the run itself: a refusal, a cut-off reply, a stop.
    Notice { text: String },
}

pub enum Answer {
    Yes,
    No,
    /// Yes, and to every change for the rest of this chat.
    Always,
}

pub trait Ui {
    fn event(&mut self, e: Event);
    /// Whether a call that changes something may run.
    fn approve(&mut self, name: &str, args: &Value) -> Answer;
    /// Checked between steps; true ends the turn.
    fn stopped(&self) -> bool {
        false
    }
    /// Each request to the provider, for an inspector.
    fn exchange(&mut self, _x: &Exchange) {}
}

fn notice(text: impl Into<String>) -> Event {
    Event::Notice { text: text.into() }
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

struct Call {
    id: String,
    name: String,
    /// Err: why it can't run, for the model.
    args: Result<Value, String>,
}

struct Done {
    text: String,
    ok: bool,
    image: Option<Image>,
}

fn failed(text: impl Into<String>) -> Done {
    Done { text: text.into(), ok: false, image: None }
}

/// A saved conversation, in `ai/chats/<id>.json` in the instance folder.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Chat {
    pub id: String,
    pub title: String,
    pub provider: String,
    pub model: String,
    /// Seconds since 1970.
    pub updated: u64,
    #[serde(default)]
    pub usage: Usage,
    /// What the screen showed, to draw it again.
    #[serde(default)]
    pub log: Vec<Event>,
    /// The conversation in the provider's own format.
    #[serde(default)]
    pub messages: Vec<Value>,
}

/// The provider of a chat that logs an AI app's session over MCP; its model is the app's name.
pub const MCP: &str = "mcp";

impl Chat {
    /// An AI app's session over MCP: its tool calls and the notes it saves.
    pub fn external(app: &str) -> Chat {
        Chat { id: now_ms().to_string(), title: format!("{app} over MCP"), provider: MCP.into(), model: app.into(), updated: now_ms() / 1000, ..Default::default() }
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
            format!("rmm's AI, {} {}", provider(&self.provider).map_or(self.provider.as_str(), |p| p.name), self.model)
        };
        let lines: Vec<String> = self
            .log
            .iter()
            .map(|e| match e {
                Event::User { text, role, .. } => format!("User{}: {text}", role.as_deref().map(|r| format!(" ({r})")).unwrap_or_default()),
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
    #[serde(default)]
    pub usage: Usage,
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
// ponytail: reads every chat whole (pictures included); an index file if lists get slow.
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

pub fn delete_chat(root: &Path, id: &str) -> Result<bool> {
    match std::fs::remove_file(chat_file(root, id)?) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// A step handed to a teammate, on the instance's team board (`ai/team.json`).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Task {
    pub n: usize,
    pub role: String,
    pub task: String,
    /// Who handed it over: rmm's AI (its chat's id) or an AI app (its name).
    pub from: String,
    /// "waiting" for the AI app over MCP, "running" on a provider, "done" or "failed".
    pub state: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub answer: String,
    /// The teammate's chat, when it ran on a provider.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub chat: String,
    pub created: u64,
    #[serde(default)]
    pub updated: u64,
}

fn board(root: &Path) -> PathBuf {
    root.join("ai").join("team.json")
}

/// The team board, oldest first.
pub fn tasks(root: &Path) -> Result<Vec<Task>> {
    match std::fs::read(board(root)) {
        Ok(b) => serde_json::from_slice(&b).context("ai/team.json is damaged"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
        Err(e) => Err(e.into()),
    }
}

// ponytail: read, change, write without a lock; two processes posting in the same instant can lose
// one task. A lock file if that ever bites.
fn change_tasks<T>(root: &Path, f: impl FnOnce(&mut Vec<Task>) -> Result<T>) -> Result<T> {
    let mut all = tasks(root)?;
    let r = f(&mut all)?;
    std::fs::create_dir_all(root.join("ai"))?;
    std::fs::write(board(root), serde_json::to_string_pretty(&all)?)?;
    Ok(r)
}

fn post_task(root: &Path, role: &str, task: &str, from: &str, state: &str) -> Result<usize> {
    change_tasks(root, |all| {
        let n = all.iter().map(|t| t.n).max().unwrap_or(0) + 1;
        let now = now_ms() / 1000;
        all.push(Task { n, role: role.into(), task: task.into(), from: from.into(), state: state.into(), answer: String::new(), chat: String::new(), created: now, updated: now });
        Ok(n)
    })
}

fn close_task(root: &Path, n: usize, from_state: &str, state: &str, answer: &str, chat: &str) -> Result<()> {
    change_tasks(root, |all| {
        let t = all.iter_mut().find(|t| t.n == n).with_context(|| format!("no team task {n}"))?;
        if t.state != from_state {
            bail!("team task {n} is {}, not {from_state}", t.state);
        }
        (t.state, t.answer, t.chat, t.updated) = (state.into(), answer.into(), chat.into(), now_ms() / 1000);
        Ok(())
    })
}

/// The user giving a message to a role that belongs to the AI app over MCP: it waits on the board.
/// Returns what to tell the user.
pub fn hand_to_app(root: &Path, role_name: &str, task: &str) -> Result<String> {
    let &(key, prompt, name) = role(role_name)?;
    let n = post_task(root, key, task.trim(), "you", "waiting")?;
    Ok(format!(
        "The {name} is your AI app connected over MCP, so this is team task {n} for it. Ask the app to do its rmm team tasks (or to use rmm's {prompt} prompt). Its answer goes on the team board, which any AI here reads with team_tasks."
    ))
}

/// The AI app over MCP finishing a task that waited for it.
pub fn finish_task(root: &Path, n: usize, answer: &str) -> Result<()> {
    if answer.trim().is_empty() {
        bail!("the answer is empty: say what you did and found");
    }
    close_task(root, n, "waiting", "done", answer.trim(), "")
}

/// A turn nobody watches: changes are declined (the teammate was told to report them instead).
struct Unattended;
impl Ui for Unattended {
    fn event(&mut self, _: Event) {}
    fn approve(&mut self, _: &str, _: &Value) -> Answer {
        Answer::No
    }
}

/// For an AI app leading over MCP: hand `task` to `role`'s provider, which works in the background
/// with its own chat; the answer lands on the team board. Returns what to tell the app.
// ponytail: the run lives in this process; if it ends first (the app closes rmm's stdio server),
// the task stays "running".
pub fn start_teammate(root: PathBuf, role_name: &str, task: &str, from: &str) -> Result<String> {
    let &(key, prompt, name) = role(role_name)?;
    let seat = seat(key);
    if key == "lead" {
        bail!("ask the researcher, installer or tester; you lead");
    }
    if seat.provider == MCP {
        bail!("the {name} is you, the AI app connected over MCP (the user chose that in rmm's Settings): do it yourself with the {prompt} prompt");
    }
    let task = task.trim();
    if task.is_empty() {
        bail!("say what the {name} is to do");
    }
    let conn = Conn::for_seat(&seat)?;
    let who = format!("{} · {}", conn.provider.name, conn.model);
    let n = post_task(&root, key, task, from, "running")?;
    let (title, task) = (format!("{name}: {task}").chars().take(70).collect::<String>(), format!("{task}\n\n({UNATTENDED})"));
    std::thread::spawn(move || {
        let run = || -> Result<(String, String)> {
            let mut mate = Agent::new(conn, root.clone())?;
            mate.solo();
            mate.chat.title = title;
            let r = mate.send(Some(prompt), &task, &[], &mut Unattended);
            r.map(|()| (mate.answer(), mate.chat.id.clone()))
        };
        let (state, answer, chat) = match run() {
            Ok((a, c)) => ("done", a, c),
            Err(e) => ("failed", format!("{e:#}"), String::new()),
        };
        let _ = close_task(&root, n, "running", state, &answer, &chat);
    });
    Ok(format!(
        "Team task {n}: the {name} ({who}) is on it in the background. Its answer comes in team_tasks. It can read and preview but not change anything here, so it reports the changes for you to make."
    ))
}

fn def_name(d: &Value) -> &str {
    d["name"].as_str().or_else(|| d["function"]["name"].as_str()).unwrap_or_default()
}

/// A chat with one model over rmm's tools.
pub struct Agent {
    pub conn: Conn,
    pub chat: Chat,
    root: PathBuf,
    tools: Tools,
    mcp: BTreeMap<String, Value>,
    defs: Vec<Value>,
    system: String,
    /// The mod team: name, description, and the argument the user's words fill.
    roles: Vec<(String, String, Option<String>)>,
    /// Changes run without asking: the user answered "always".
    pub trusted: bool,
}

impl Agent {
    /// A new chat with `conn`, with rmm's tools on the instance at `root`.
    pub fn new(conn: Conn, root: PathBuf) -> Result<Agent> {
        let chat = Chat { id: now_ms().to_string(), provider: conn.provider.id.into(), model: conn.model.clone(), ..Default::default() };
        Agent::resume(conn, root, chat)
    }

    /// Go on with a saved chat; `conn` must be its provider's.
    pub fn resume(conn: Conn, root: PathBuf, chat: Chat) -> Result<Agent> {
        if chat.provider == MCP {
            bail!("chat {} is {}'s session over MCP; start a new chat and ask the AI to read it (read_chat {})", chat.id, chat.model, chat.id);
        }
        if chat.provider != conn.provider.id {
            bail!("this chat is with {}; start a new chat to use {}", provider(&chat.provider).map_or(chat.provider.as_str(), |p| p.name), conn.provider.name);
        }
        let (mut tools, init) = Tools::start(root.clone())?;
        // rmm keeps this chat itself, and hands work to teammates its own way (ask_teammate below):
        // these tools are for AI apps connected over MCP.
        let for_apps = |t: &Value| ["save_chat_note", "ask_teammate", "finish_team_task"].iter().any(|n| t["name"] == *n);
        let list: Vec<Value> = tools.rpc("tools/list", json!({}))?["tools"].as_array().cloned().unwrap_or_default().into_iter().filter(|t| !for_apps(t)).collect();
        let prompts = tools.rpc("prompts/list", json!({}))?["prompts"].as_array().cloned().unwrap_or_default();
        let roles = prompts
            .iter()
            .map(|p| (p["name"].as_str().unwrap_or_default().into(), p["description"].as_str().unwrap_or_default().into(), p["arguments"][0]["name"].as_str().map(String::from)))
            .collect();
        let mut specs: Vec<(String, String, Value)> = list
            .iter()
            .map(|t| {
                let mut schema = t["inputSchema"].clone();
                if let Some(o) = schema.as_object_mut() {
                    o.remove("$schema");
                }
                (t["name"].as_str().unwrap_or_default().into(), t["description"].as_str().unwrap_or_default().into(), schema)
            })
            .collect();
        specs.push((
            "view_image".into(),
            "Look at a PNG, JPEG, GIF or WebP file: a screenshot path from test_session_step, or a picture the user names.".into(),
            json!({ "type": "object", "properties": { "path": { "type": "string", "description": "The file's full path." } }, "required": ["path"] }),
        ));
        specs.push((
            "ask_teammate".into(),
            "Hand a step to a teammate: the researcher (find mods), the installer (install and review one) or the tester (play the game to try mods). Each runs on the AI the user chose for it in Settings, with its role's prompt and your task, in a chat of its own, and answers when done; the user approves its changes. Give it all it needs: the user's request, the mod picked, what to test and where. A role given to the AI app over MCP goes on the team board for that app (team_tasks shows the answer later).".into(),
            json!({ "type": "object", "properties": {
                "role": { "type": "string", "enum": ["researcher", "installer", "tester"] },
                "task": { "type": "string", "description": "What to do, with everything the teammate needs to know." } }, "required": ["role", "task"] }),
        ));
        let defs = specs
            .into_iter()
            .map(|(name, description, schema)| match conn.provider.api {
                Api::Anthropic => json!({ "name": name, "description": description, "input_schema": schema }),
                Api::OpenAi => json!({ "type": "function", "function": { "name": name, "description": description, "parameters": schema } }),
            })
            .collect();
        let mcp = list.into_iter().map(|t| (t["name"].as_str().unwrap_or_default().to_string(), t)).collect();
        let system = format!("{SYSTEM}\n\n{}", init["instructions"].as_str().unwrap_or_default());
        Ok(Agent { conn, chat, root, tools, mcp, defs, system, roles, trusted: false })
    }

    /// A teammate: it can't hand work on.
    pub fn solo(&mut self) {
        self.defs.retain(|d| def_name(d) != "ask_teammate");
    }

    /// The model's last words in the last turn.
    pub fn answer(&self) -> String {
        let start = self.chat.log.iter().rposition(|e| matches!(e, Event::User { .. })).map_or(0, |i| i + 1);
        self.chat.log[start..].iter().rev().find_map(|e| if let Event::Text { text } = e { Some(text.clone()) } else { None }).unwrap_or_default()
    }

    /// ask_teammate: the teammate's AI runs the task in this window (its changes asked for here, its
    /// cost added to this chat's), or the task waits on the board for the AI app over MCP.
    fn ask_teammate(&mut self, args: &Value, ui: &mut dyn Ui) -> Done {
        let task = args["task"].as_str().unwrap_or_default().trim();
        let (key, prompt, name) = match role(args["role"].as_str().unwrap_or_default()) {
            Ok(&r) if r.0 != "lead" => r,
            _ => return failed("role is researcher, installer or tester"),
        };
        if task.is_empty() {
            return failed("say in task what the teammate is to do");
        }
        let seat = seat(key);
        if seat.provider == MCP {
            return match post_task(&self.root, key, task, &self.chat.id, "waiting") {
                Ok(n) => Done {
                    text: format!("The {name} is the AI app connected over MCP (the user chose that in Settings), which rmm can't start. Team task {n} waits for it. Tell the user to ask that app to do its rmm team tasks, and end your turn; its answer will be in team_tasks."),
                    ok: true,
                    image: None,
                },
                Err(e) => failed(format!("{e:#}")),
            };
        }
        let mut mate = match Conn::for_seat(&seat).and_then(|c| Agent::new(c, self.root.clone())) {
            Ok(a) => a,
            Err(e) => return failed(format!("the {name} couldn't start: {e:#}")),
        };
        mate.solo();
        mate.trusted = self.trusted;
        mate.chat.title = format!("{name}: {task}").chars().take(70).collect();
        self.show(ui, notice(format!("The {name} ({} · {}) takes this on.", mate.conn.provider.name, mate.conn.model)));
        let r = mate.send(Some(prompt), task, &[], ui);
        self.trusted |= mate.trusted;
        self.chat.usage.add(&mate.chat.usage);
        self.show(ui, notice(format!("Back to the lead. The {name}'s chat is saved: \"{}\".", mate.chat.title)));
        let answer = mate.answer();
        match r {
            Ok(()) if !answer.is_empty() => Done { text: answer, ok: true, image: None },
            Ok(()) => failed(format!("the {name} ended without an answer")),
            Err(e) if answer.is_empty() => failed(format!("the {name} stopped: {e:#}")),
            Err(e) => failed(format!("the {name} stopped: {e:#}\nIts last words: {answer}")),
        }
    }

    /// The mod team roles: name and what it does.
    pub fn roles(&self) -> Vec<(String, String)> {
        self.roles.iter().map(|(n, d, _)| (n.clone(), d.clone())).collect()
    }

    fn system(&self) -> String {
        match self.conn.options.instructions.as_str() {
            "" => self.system.clone(),
            more => format!("{}\n\nThe user's instructions:\n{more}", self.system),
        }
    }

    fn show(&mut self, ui: &mut dyn Ui, e: Event) {
        self.chat.log.push(e.clone());
        ui.event(e);
    }

    /// One turn: the user's words (inside `role`'s instructions when one is named) and pictures,
    /// then the model and its tool calls until it answers. The chat is saved afterwards.
    pub fn send(&mut self, role: Option<&str>, text: &str, images: &[Image], ui: &mut dyn Ui) -> Result<()> {
        // A role by its name (researcher) or its prompt's (find_mods).
        let role = role.filter(|r| !r.is_empty()).map(|r| ROLES.iter().find(|x| x.0 == r).map_or(r, |x| x.1));
        let prompt = match role {
            Some(r) => self.role_prompt(r, text)?,
            None => text.to_string(),
        };
        if self.chat.title.is_empty() {
            let t = if text.trim().is_empty() { role.unwrap_or("Chat") } else { text.trim() };
            self.chat.title = t.chars().take(70).collect();
        }
        self.show(ui, Event::User { text: text.into(), role: role.map(String::from), pictures: images.len() });
        self.push_user(&prompt, images);
        let r = self.run(ui);
        self.chat.model = self.conn.model.clone();
        self.chat.updated = now_ms() / 1000;
        let saved = self.save();
        r.and(saved)
    }

    fn save(&self) -> Result<()> {
        self.chat.save(&self.root)
    }

    fn role_prompt(&mut self, role: &str, text: &str) -> Result<String> {
        let (name, _, arg) = self
            .roles
            .iter()
            .find(|r| r.0 == role)
            .cloned()
            .with_context(|| format!("no role {role:?}; the roles are {}", self.roles.iter().map(|r| r.0.as_str()).collect::<Vec<_>>().join(", ")))?;
        let mut args = serde_json::Map::new();
        if let Some(a) = arg.filter(|_| !text.trim().is_empty()) {
            args.insert(a, text.into());
        }
        let r = self.tools.rpc("prompts/get", json!({ "name": name, "arguments": args }))?;
        Ok(r["messages"].as_array().into_iter().flatten().filter_map(|m| m["content"]["text"].as_str()).collect::<Vec<_>>().join("\n\n"))
    }

    /// Add the user's words, joining them to a user message already last (after a stop or an error).
    fn push_user(&mut self, text: &str, images: &[Image]) {
        let api = self.conn.provider.api;
        let mut parts: Vec<Value> = match api {
            Api::Anthropic => images.iter().map(Image::anthropic).collect(),
            Api::OpenAi => images.iter().map(Image::openai).collect(),
        };
        parts.push(json!({ "type": "text", "text": text }));
        if let Some(last) = self.chat.messages.last_mut().filter(|m| m["role"] == "user") {
            let c = &mut last["content"];
            if let (Some(old), true) = (c.as_str(), images.is_empty()) {
                *c = format!("{old}\n\n{text}").into();
                return;
            }
            if let Some(old) = c.as_str() {
                *c = json!([{ "type": "text", "text": old }]);
            }
            if let Some(a) = c.as_array_mut() {
                a.extend(parts);
            }
            return;
        }
        // Some OpenAI-compatible servers take only plain text when there's no picture.
        let content = if api == Api::OpenAi && images.is_empty() { text.into() } else { Value::Array(parts) };
        self.chat.messages.push(json!({ "role": "user", "content": content }));
    }

    fn run(&mut self, ui: &mut dyn Ui) -> Result<()> {
        let cfg = rmm_core::settings::get().ai;
        for _ in 0..cfg.max_steps {
            if ui.stopped() {
                self.show(ui, notice("Stopped."));
                return Ok(());
            }
            if let Some(cost) = self.chat.usage.cost.filter(|&c| cfg.budget > 0.0 && c >= cfg.budget) {
                self.show(ui, notice(format!("Stopped: this chat has cost ${cost:.2}, which reaches your limit of ${:.2}. Raise or remove the limit in Settings, under AI, then say \"go on\".", cfg.budget)));
                return Ok(());
            }
            let calls = match self.conn.provider.api {
                Api::Anthropic => self.step_anthropic(ui)?,
                Api::OpenAi => self.step_openai(ui)?,
            };
            let Some(calls) = calls else { return Ok(()) };
            let done: Vec<Done> = calls.iter().map(|c| self.run_call(c, ui)).collect();
            self.push_results(&calls, done);
        }
        self.show(ui, notice(format!("Paused after {} steps; say \"go on\" to continue.", cfg.max_steps)));
        Ok(())
    }

    /// A request to the provider, counted in the chat's usage and shown to the inspector.
    fn request(&mut self, url: &str, headers: &[(&str, String)], body: &Value, ui: &mut dyn Ui) -> Result<Value> {
        let (at, t0) = (now_ms(), std::time::Instant::now());
        let v = http(self.conn.provider, url, headers, Some(body))?;
        let usage = Usage::of(self.conn.provider.api, &self.conn.model, &v);
        self.chat.usage.add(&usage);
        ui.exchange(&Exchange { at, model: self.conn.model.clone(), url: url.into(), ms: t0.elapsed().as_millis() as u64, request: trimmed(body), response: v.clone(), usage });
        Ok(v)
    }

    /// None: the model has answered. Some: calls to run (none after a pause, to ask again).
    fn step_anthropic(&mut self, ui: &mut dyn Ui) -> Result<Option<Vec<Call>>> {
        let model = self.conn.model.clone();
        let o = self.conn.options.clone();
        let mut body = json!({
            "model": model,
            "max_tokens": o.max_tokens.unwrap_or(16000),
            "system": self.system(),
            "tools": self.defs,
            "messages": self.chat.messages,
            "cache_control": { "type": "ephemeral" },
        });
        if EFFORT.iter().any(|m| model.starts_with(m)) {
            body["output_config"] = json!({ "effort": o.effort.as_deref().unwrap_or("high") });
        }
        let mut headers = self.conn.auth();
        if FALLBACKS.contains(&model.as_str()) {
            headers.push(("anthropic-beta", "server-side-fallback-2026-07-01".into()));
            body["fallbacks"] = "default".into();
        }
        let url = format!("{}/v1/messages", self.conn.base);
        let v = self.request(&url, &headers, &body, ui)?;
        let content = v["content"].as_array().cloned().unwrap_or_default();
        let mut calls = vec![];
        for b in &content {
            match b["type"].as_str() {
                Some("text") => self.show(ui, Event::Text { text: b["text"].as_str().unwrap_or_default().into() }),
                Some("tool_use") => calls.push(Call { id: b["id"].as_str().unwrap_or_default().into(), name: b["name"].as_str().unwrap_or_default().into(), args: Ok(b["input"].clone()) }),
                Some("fallback") => self.show(ui, notice(format!("{} declined; {} carried on.", b["from"]["model"].as_str().unwrap_or("the model"), b["to"]["model"].as_str().unwrap_or("another model")))),
                _ => {}
            }
        }
        let stop = v["stop_reason"].as_str().unwrap_or_default();
        if stop == "refusal" {
            self.show(ui, notice("The model declined to go on with this. Rephrase, or start a new chat."));
            return Ok(None);
        }
        // Kept whole: thinking blocks must go back unchanged.
        self.chat.messages.push(json!({ "role": "assistant", "content": content }));
        Ok(match stop {
            "tool_use" => Some(calls),
            "pause_turn" => Some(vec![]),
            "max_tokens" if !calls.is_empty() => Some(calls.into_iter().map(|c| Call { args: Err("Not run: the reply was cut off at the length limit. Make the call again with less in it.".into()), ..c }).collect()),
            "max_tokens" => {
                self.show(ui, notice("The reply reached the length limit."));
                None
            }
            _ => None,
        })
    }

    fn step_openai(&mut self, ui: &mut dyn Ui) -> Result<Option<Vec<Call>>> {
        let o = self.conn.options.clone();
        let mut messages = vec![json!({ "role": "system", "content": self.system() })];
        messages.extend(self.chat.messages.iter().cloned());
        let mut body = json!({ "model": self.conn.model, "messages": messages, "tools": self.defs });
        if let Some(t) = o.temperature {
            body["temperature"] = t.into();
        }
        if let Some(n) = o.max_tokens {
            // OpenAI's own API wants the newer name; the compatible ones take max_tokens.
            body[if self.conn.provider.id == "openai" { "max_completion_tokens" } else { "max_tokens" }] = n.into();
        }
        if let Some(e) = o.effort {
            body["reasoning_effort"] = e.into();
        }
        if self.conn.provider.id == "openrouter" {
            body["usage"] = json!({ "include": true });
        }
        let auth = self.conn.auth();
        let url = format!("{}/chat/completions", self.conn.base);
        let v = self.request(&url, &auth, &body, ui)?;
        let choice = &v["choices"][0];
        let mut msg = choice["message"].clone();
        if !msg.is_object() {
            // OpenRouter passes a model's error on inside a 200.
            bail!("{} sent no reply: {}", self.conn.provider.name, message_of(&v.to_string()));
        }
        if let Some(t) = msg["content"].as_str().filter(|t| !t.trim().is_empty()) {
            self.show(ui, Event::Text { text: t.into() });
        }
        let calls: Vec<Call> = msg["tool_calls"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| {
                let a = c["function"]["arguments"].as_str().filter(|a| !a.trim().is_empty()).unwrap_or("{}");
                Call {
                    id: c["id"].as_str().unwrap_or_default().into(),
                    name: c["function"]["name"].as_str().unwrap_or_default().into(),
                    args: serde_json::from_str(a).map_err(|e| format!("Not run: the arguments weren't valid JSON ({e}). Send them again.")),
                }
            })
            .collect();
        let finish = choice["finish_reason"].as_str().unwrap_or_default().to_string();
        // Kept whole: DeepSeek wants its reasoning_content back with tool calls.
        msg["role"] = "assistant".into();
        self.chat.messages.push(msg);
        if !calls.is_empty() {
            return Ok(Some(calls));
        }
        match finish.as_str() {
            "length" => self.show(ui, notice("The reply reached the length limit.")),
            "content_filter" => self.show(ui, notice("The provider's filter stopped the reply.")),
            _ => {}
        }
        Ok(None)
    }

    /// Reads, dry runs, the steps of a live test the user already let start, and the tools the user
    /// allowed in Settings run without asking.
    fn needs_ok(&self, name: &str, args: &Value) -> bool {
        if matches!(name, "view_image" | "test_session_step" | "test_session_stop") || rmm_core::settings::get().ai.auto_allow.iter().any(|t| t == name) {
            return false;
        }
        let Some(t) = self.mcp.get(name) else { return false };
        if t["annotations"]["readOnlyHint"] == true {
            return false;
        }
        let previews = t["inputSchema"]["properties"].get("dry_run").is_some();
        !(previews && args.get("dry_run").and_then(Value::as_bool).unwrap_or(true))
    }

    fn run_call(&mut self, c: &Call, ui: &mut dyn Ui) -> Done {
        let args = match &c.args {
            Ok(a) => a.clone(),
            Err(e) => return failed(e.clone()),
        };
        if ui.stopped() {
            return failed("Not run: the user stopped the turn.");
        }
        if !self.trusted && self.needs_ok(&c.name, &args) {
            match ui.approve(&c.name, &args) {
                Answer::Yes => {}
                Answer::Always => self.trusted = true,
                Answer::No => return failed("The user declined this call."),
            }
        }
        self.show(ui, Event::Call { name: c.name.clone(), args: args.clone() });
        let done = if c.name == "ask_teammate" {
            self.ask_teammate(&args, ui)
        } else if c.name == "view_image" {
            match args["path"].as_str().context("view_image needs a path").and_then(|p| Image::load(Path::new(p))) {
                Ok(image) => Done { text: "The picture is attached.".into(), ok: true, image: Some(image) },
                Err(e) => failed(format!("{e:#}")),
            }
        } else {
            match self.tools.rpc("tools/call", json!({ "name": c.name, "arguments": args })) {
                Ok(r) => {
                    let text: Vec<&str> = r["content"].as_array().into_iter().flatten().filter_map(|b| b["text"].as_str()).collect();
                    Done { text: cut(&text.join("\n"), MAX_RESULT), ok: r["isError"] != true, image: None }
                }
                Err(e) => failed(format!("{e:#}")),
            }
        };
        self.show(ui, Event::Result { name: c.name.clone(), ok: done.ok, text: cut(&done.text, 2000) });
        done
    }

    fn push_results(&mut self, calls: &[Call], done: Vec<Done>) {
        if calls.is_empty() {
            return;
        }
        match self.conn.provider.api {
            Api::Anthropic => {
                let blocks: Vec<Value> = calls
                    .iter()
                    .zip(done)
                    .map(|(c, d)| {
                        let mut content = vec![json!({ "type": "text", "text": d.text })];
                        content.extend(d.image.as_ref().map(Image::anthropic));
                        json!({ "type": "tool_result", "tool_use_id": c.id, "content": content, "is_error": !d.ok })
                    })
                    .collect();
                self.chat.messages.push(json!({ "role": "user", "content": blocks }));
            }
            Api::OpenAi => {
                // Tool messages carry text only, so pictures follow in a user message.
                let mut images = vec![json!({ "type": "text", "text": "The pictures view_image opened, in order:" })];
                for (c, d) in calls.iter().zip(done) {
                    let text = if d.ok { d.text } else { format!("Error: {}", d.text) };
                    self.chat.messages.push(json!({ "role": "tool", "tool_call_id": c.id, "content": text }));
                    images.extend(d.image.as_ref().map(Image::openai));
                }
                if images.len() > 1 {
                    self.chat.messages.push(json!({ "role": "user", "content": images }));
                }
            }
        }
    }
}
