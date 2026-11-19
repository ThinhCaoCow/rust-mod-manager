//! Nexus Mods: `nxm://` links, the user's API key, update checks and downloads (API v1).
//!
//! The key comes from `RMM_NEXUS_KEY` or the file `rmm/nexus.key` in the user's config folder
//! (`%APPDATA%` on Windows), which `set_key` writes after Nexus accepts it. It never goes into an
//! instance, a lockfile or a command line: curl gets it on stdin. Single sign-on (`sso`) gets the
//! same key from Nexus once the user approves rmm in their browser; it needs the application name
//! Nexus issues when it registers rmm (`SSO_APP`, or `RMM_NEXUS_APP` meanwhile).
//!
//! Update checks follow each file's `file_updates` chain (old file -> new file), falling back to
//! the newest main file's version. Nexus allows about 100 requests an hour once the daily burst is
//! spent, so a check asks which mods changed in the last month first and only re-reads those (and
//! mods not checked for 30 days); the answers are cached in `nexus-cache.json`.
use crate::Instance;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const API: &str = "https://api.nexusmods.com";
const CACHE: &str = "nexus-cache.json";
/// rmm's application name for Nexus single sign-on. Only Nexus staff issue one, when they
/// register the app; until then single sign-on is off unless `RMM_NEXUS_APP` sets a name.
const SSO_APP: Option<&str> = None;
const SSO: &str = "wss://sso.nexusmods.com";

/// A parsed `nxm://<game>/mods/<mod>/files/<file>?key=…&expires=…&user_id=…` link, what the
/// website's "Mod manager download" button opens.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NxmLink {
    pub game: String,
    pub mod_id: u64,
    pub file_id: u64,
    /// One-off download permission for accounts without Premium.
    pub key: Option<String>,
    pub expires: Option<u64>,
}

pub fn parse_nxm(link: &str) -> Result<NxmLink> {
    let rest = link.trim().strip_prefix("nxm://").context("not an nxm:// link")?;
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let parts: Vec<&str> = path.trim_end_matches('/').split('/').collect();
    let [game, "mods", m, "files", f] = parts.as_slice() else {
        bail!("rmm handles nxm links to mod files (nxm://<game>/mods/<id>/files/<id>), not '{path}'");
    };
    let q = |k: &str| query.split('&').find_map(|kv| kv.strip_prefix(k)?.strip_prefix('=')).map(str::to_owned);
    Ok(NxmLink {
        game: game.to_lowercase(),
        mod_id: m.parse().context("bad mod id in the link")?,
        file_id: f.parse().context("bad file id in the link")?,
        // It goes into a URL: anything but a plain token is not a Nexus key.
        key: q("key").filter(|k| !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')),
        expires: q("expires").and_then(|e| e.parse().ok()),
    })
}

/// The file an open window keeps locked on its instance, and the folder where nxm links wait for it.
const WINDOW_LOCK: &str = ".window-lock";
const INBOX: &str = ".nxm-inbox";

/// Lock the instance for this window, so `rmm nxm` sends links here. Closing the file unlocks it,
/// and so does the window closing or crashing.
pub fn hold_window(root: &Path) -> Result<fs::File> {
    let f = fs::OpenOptions::new().create(true).write(true).truncate(false).open(root.join(WINDOW_LOCK))?;
    if let Err(e) = f.try_lock() {
        bail!("another window has {} open ({e})", root.display());
    }
    Ok(f)
}

/// Whether a window has the instance open.
pub fn window_open(root: &Path) -> bool {
    fs::OpenOptions::new().write(true).open(root.join(WINDOW_LOCK)).is_ok_and(|f| matches!(f.try_lock(), Err(fs::TryLockError::WouldBlock)))
}

/// Where `hand_to_window` put a link.
#[derive(Debug, PartialEq)]
pub enum Handed {
    /// To the window that has the instance open.
    ToWindow,
    /// To a window it started for the instance.
    OpenedWindow,
    /// Nowhere: there is no window program to start. The caller installs it.
    NoWindow,
}

/// Give an nxm link (a "Mod manager download" button's) to the window that has the instance open,
/// or start `app`, rmm's window program, on the instance to take it. The window downloads it and
/// installs it like a dropped file: FOMOD installers show their options, and a newer version of
/// an installed mod is offered as an update.
pub fn hand_to_window(root: &Path, link: &str, app: &Path) -> Result<Handed> {
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    parse_nxm(link)?;
    let open = window_open(root);
    if !open && !app.is_file() {
        return Ok(Handed::NoWindow);
    }
    let inbox = root.join(INBOX);
    fs::create_dir_all(&inbox)?;
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = format!("{:020}-{}-{seq}", SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(), std::process::id());
    fs::write(inbox.join(format!("{name}.tmp")), link.trim())?;
    fs::rename(inbox.join(format!("{name}.tmp")), inbox.join(format!("{name}.nxm")))?; // never read half-written
    if open {
        return Ok(Handed::ToWindow);
    }
    // Several links clicked while no window is open start one.
    let starting = inbox.join(".starting");
    let recent = fs::metadata(&starting).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|e| e < Duration::from_secs(30));
    if !recent {
        fs::write(&starting, "")?;
        Command::new(app)
            .arg("--instance")
            .arg(root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("cannot start {}", app.display()))?;
    }
    Ok(Handed::OpenedWindow)
}

/// How many links wait for the window.
pub fn pending_links(root: &Path) -> usize {
    inbox_files(root).len()
}

/// The links waiting for the window, oldest first, taken out of the inbox.
pub fn take_links(root: &Path) -> Vec<String> {
    inbox_files(root)
        .into_iter()
        .filter_map(|p| {
            let link = fs::read_to_string(&p).ok()?;
            fs::remove_file(&p).ok()?;
            Some(link.trim().to_string())
        })
        .filter(|l| parse_nxm(l).is_ok())
        .collect()
}

fn inbox_files(root: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(root.join(INBOX)).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "nxm")).collect();
    files.sort();
    files
}

/// A connection to the Nexus API with the user's key.
pub struct Client {
    pub base: String,
    key: String,
}

fn key_file() -> Option<PathBuf> {
    crate::config_file("nexus.key")
}

impl Client {
    /// With the key from `RMM_NEXUS_KEY` or the saved key file; `RMM_NEXUS_API` overrides the
    /// server (for tests).
    pub fn from_env() -> Result<Client> {
        let key = std::env::var("RMM_NEXUS_KEY")
            .ok()
            .or_else(|| fs::read_to_string(key_file()?).ok())
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .context("no Nexus API key: copy yours from nexusmods.com > Site preferences > API Keys, then `rmm nexus login` (or set RMM_NEXUS_KEY)")?;
        Client::new(&key)
    }

    pub fn new(key: &str) -> Result<Client> {
        if key.is_empty() || key.chars().any(|c| c.is_control() || c == '"' || c == '\\' || c.is_whitespace()) {
            bail!("that doesn't look like a Nexus API key");
        }
        Ok(Client { base: std::env::var("RMM_NEXUS_API").unwrap_or_else(|_| API.into()), key: key.into() })
    }

    /// Without a key: enough for search and mod pages (`graphql`), which Nexus serves signed out.
    pub fn anonymous() -> Client {
        Client { base: std::env::var("RMM_NEXUS_API").unwrap_or_else(|_| API.into()), key: String::new() }
    }

    /// The saved key's client, or an anonymous one.
    pub fn any() -> Client {
        Client::from_env().unwrap_or_else(|_| Client::anonymous())
    }

    /// GET a JSON endpoint (`path` starts with `/v1/`).
    pub fn get(&self, path: &str) -> Result<Value> {
        self.request(path, None)
    }

    /// A query against Nexus's GraphQL API (v2): search, descriptions, requirements.
    pub fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        let v = self.request("/v2/graphql", Some(&serde_json::json!({ "query": query, "variables": variables }).to_string()))?;
        if let Some(e) = v["errors"].as_array().and_then(|e| e.first()) {
            bail!("Nexus: {}", e["message"].as_str().unwrap_or("the query failed"));
        }
        Ok(v["data"].clone())
    }

    fn request(&self, path: &str, body: Option<&str>) -> Result<Value> {
        self.send("", path, body.map(|b| ("application/json", b)))
    }

    /// A v1 call that takes form data (endorse, track): POST or DELETE.
    fn form(&self, method: &str, path: &str, form: &str) -> Result<Value> {
        self.send(method, path, Some(("application/x-www-form-urlencoded", form)))
    }

    fn send(&self, method: &str, path: &str, body: Option<(&str, &str)>) -> Result<Value> {
        let url = format!("{}{path}", self.base);
        let version = env!("CARGO_PKG_VERSION");
        // The key goes to curl on stdin, never on its command line where other programs see it.
        let mut config = format!("header = \"Application-Name: rmm\"\nheader = \"Application-Version: {version}\"\nuser-agent = \"rmm/{version}\"\n");
        if !self.key.is_empty() {
            config += &format!("header = \"apikey: {}\"\n", self.key);
        }
        let mut args = vec!["-sS", "--max-time", "60", "-w", "\n%{http_code}", "-K", "-"];
        if !method.is_empty() {
            args.extend(["-X", method]);
        }
        let content_type = body.map(|(t, _)| format!("Content-Type: {t}"));
        if let (Some(t), Some((_, b))) = (&content_type, body) {
            args.extend(["-H", t.as_str(), "-H", "Expect:", "--data-binary", b]);
        }
        args.extend(["--", &url]);
        let mut child = Command::new("curl").args(&args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().context("cannot run curl")?;
        child.stdin.take().unwrap().write_all(config.as_bytes())?;
        let out = child.wait_with_output()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let (body, code) = text.rsplit_once('\n').unwrap_or(("", &text));
        match code.trim() {
            "200" => serde_json::from_str(body).with_context(|| format!("Nexus sent something that isn't JSON for {path}")),
            "401" => bail!("Nexus refused the API key; copy it again from your account's API Keys page"),
            "403" => bail!("Nexus refused {path}: {}", message(body)),
            "404" => bail!("Nexus has no {path} (removed or hidden?)"),
            "429" => bail!("Nexus's request limit is used up for now; try again in an hour"),
            "000" | "" => bail!("couldn't reach Nexus: {}", String::from_utf8_lossy(&out.stderr).trim()),
            c => bail!("Nexus answered {c} for {path}: {}", message(body)),
        }
    }

    /// Who the key belongs to: `name`, `is_premium` and `user_id` (the key itself is dropped).
    pub fn validate(&self) -> Result<Value> {
        let mut v = self.get("/v1/users/validate.json")?;
        if let Some(o) = v.as_object_mut() {
            o.retain(|k, _| matches!(k.as_str(), "name" | "user_id" | "is_premium" | "is_supporter"));
        }
        Ok(v)
    }
}

fn message(body: &str) -> String {
    serde_json::from_str::<Value>(body).ok().and_then(|v| v["message"].as_str().map(str::to_owned)).unwrap_or_else(|| body.chars().take(200).collect())
}

/// Check the key with Nexus and save it for later runs. Returns who it belongs to.
pub fn set_key(key: &str) -> Result<Value> {
    let key = key.trim();
    let who = Client::new(key)?.validate()?;
    let path = key_file().context("no config folder to keep the key in")?;
    crate::write_private(&path, key.as_bytes())?;
    Ok(who)
}

pub fn forget_key() -> Result<bool> {
    match key_file().map(fs::remove_file) {
        Some(Ok(())) => Ok(true),
        Some(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Some(Err(e)) => Err(e.into()),
        None => Ok(false),
    }
}

/// Whether a key is set, and whose it is (asks Nexus); `sso` says whether single sign-on is on.
pub fn status() -> Value {
    let mut v = match Client::from_env() {
        Err(e) => serde_json::json!({ "key": false, "note": format!("{e:#}") }),
        Ok(c) => match c.validate() {
            Ok(who) => serde_json::json!({ "key": true, "user": who }),
            Err(e) => serde_json::json!({ "key": true, "error": format!("{e:#}") }),
        },
    };
    v["sso"] = sso_app().is_some().into();
    v
}

fn sso_app() -> Option<String> {
    std::env::var("RMM_NEXUS_APP").ok().filter(|a| !a.is_empty()).or(SSO_APP.map(str::to_owned))
}

/// Single sign-on: rmm asks Nexus for a sign-in over a websocket, `open` shows the user Nexus's
/// page to approve rmm, and Nexus sends their API key back, which is checked and saved like a
/// pasted one. Returns who it belongs to. Gives up after 10 minutes.
pub fn sso(open: &dyn Fn(&str) -> Result<()>) -> Result<Value> {
    let app = sso_app().context("rmm isn't registered with Nexus for single sign-on yet; paste your API key instead")?;
    set_key(&sso_key(&std::env::var("RMM_NEXUS_SSO").unwrap_or_else(|_| SSO.into()), &app, Duration::from_secs(600), open)?)
}

/// Nexus's SSO protocol 2 (github.com/Nexus-Mods/sso-integration-demo): send a random id, get a
/// connection token, open `nexusmods.com/sso?id=…&application=…`, wait for the key.
fn sso_key(url: &str, app: &str, wait: Duration, open: &dyn Fn(&str) -> Result<()>) -> Result<String> {
    let id = uuid()?;
    let deadline = Instant::now() + wait;
    let mut ws = ws::Socket::connect(url).context("couldn't reach Nexus's sign-in service")?;
    ws.send(&serde_json::json!({ "id": id, "token": null, "protocol": 2 }).to_string())?;
    let mut opened = false;
    loop {
        let m: Value = serde_json::from_str(&ws.recv(deadline)?).context("Nexus's sign-in sent something that isn't JSON")?;
        if m["success"] != true {
            bail!("Nexus sign-in failed: {}", m["error"].as_str().unwrap_or("no reason given"));
        }
        if let Some(key) = m["data"]["api_key"].as_str() {
            return Ok(key.to_owned());
        }
        if !opened && m["data"]["connection_token"].is_string() {
            open(&format!("https://www.nexusmods.com/sso?id={id}&application={app}"))?;
            opened = true;
        }
    }
}

fn random<const N: usize>() -> Result<[u8; N]> {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
    Ok(b)
}

fn uuid() -> Result<String> {
    let mut b: [u8; 16] = random()?;
    b[6] = b[6] & 0x0f | 0x40;
    b[8] = b[8] & 0x3f | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..]))
}

/// Open a web page in the user's browser.
pub fn open_in_browser(url: &str) -> Result<()> {
    // Not `cmd /c start`: cmd splits the URL at `&`.
    #[cfg(windows)]
    let mut c = Command::new("rundll32");
    #[cfg(windows)]
    c.arg("url.dll,FileProtocolHandler");
    #[cfg(not(windows))]
    let mut c = Command::new("xdg-open");
    c.arg(url).spawn().context("couldn't open the browser")?;
    Ok(())
}

/// The little of RFC 6455 single sign-on needs: a client socket for short text messages, over TLS
/// (rustls with the Mozilla roots) or plain TCP for tests.
mod ws {
    use anyhow::{Context, Result, bail};
    use base64::Engine;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    trait Stream: Read + Write + Send {}
    impl<T: Read + Write + Send> Stream for T {}

    pub struct Socket(Box<dyn Stream>);

    impl Socket {
        pub fn connect(url: &str) -> Result<Socket> {
            let (tls, rest) = match url.split_once("://") {
                Some(("wss", r)) => (true, r),
                Some(("ws", r)) => (false, r),
                _ => bail!("not a websocket URL: {url}"),
            };
            let (authority, path) = rest.split_once('/').map_or((rest, "/".to_string()), |(a, p)| (a, format!("/{p}")));
            let (host, port) = match authority.rsplit_once(':') {
                Some((h, p)) => (h, p.parse().context("bad port")?),
                None => (authority, if tls { 443 } else { 80 }),
            };
            let tcp = TcpStream::connect((host, port))?;
            // Nexus drops quiet connections: a read that waits this long sends a ping instead.
            tcp.set_read_timeout(Some(Duration::from_secs(30)))?;
            let mut s: Box<dyn Stream> = if tls {
                let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
                let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                    .with_safe_default_protocol_versions()?
                    .with_root_certificates(roots)
                    .with_no_client_auth();
                let name = rustls::pki_types::ServerName::try_from(host.to_string())?;
                Box::new(rustls::StreamOwned::new(rustls::ClientConnection::new(Arc::new(config), name)?, tcp))
            } else {
                Box::new(tcp)
            };
            let key = base64::prelude::BASE64_STANDARD.encode(super::random::<16>()?);
            write!(s, "GET {path} HTTP/1.1\r\nHost: {authority}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n")?;
            s.flush()?;
            let mut head = Vec::new();
            let mut b = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                s.read_exact(&mut b)?;
                head.push(b[0]);
                if head.len() > 16 * 1024 {
                    bail!("the server's answer is too long");
                }
            }
            // ponytail: Sec-WebSocket-Accept isn't checked (it needs SHA-1); TLS already vouches for Nexus.
            let status = String::from_utf8_lossy(&head).lines().next().unwrap_or_default().to_string();
            if !status.starts_with("HTTP/1.1 101") {
                bail!("the server refused the websocket: {status}");
            }
            Ok(Socket(s))
        }

        pub fn send(&mut self, text: &str) -> Result<()> {
            self.frame(0x1, text.as_bytes())
        }

        /// Client frames are masked (RFC 6455 5.3).
        fn frame(&mut self, op: u8, data: &[u8]) -> Result<()> {
            let mut f = vec![0x80 | op];
            match data.len() {
                n if n < 126 => f.push(0x80 | n as u8),
                n if n <= 0xffff => {
                    f.push(0x80 | 126);
                    f.extend((n as u16).to_be_bytes());
                }
                n => {
                    f.push(0x80 | 127);
                    f.extend((n as u64).to_be_bytes());
                }
            }
            let mask: [u8; 4] = super::random()?;
            f.extend(mask);
            f.extend(data.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
            self.0.write_all(&f)?;
            Ok(self.0.flush()?)
        }

        /// The next text message, answering pings and sending one while nothing comes.
        pub fn recv(&mut self, deadline: Instant) -> Result<String> {
            let mut text = Vec::new();
            loop {
                if Instant::now() > deadline {
                    bail!("timed out waiting for Nexus; start the sign-in again");
                }
                let mut h = [0u8; 2];
                match self.0.read(&mut h[..1]) {
                    Ok(0) => bail!("Nexus closed the sign-in connection"),
                    Ok(_) => {}
                    Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                        self.frame(0x9, b"")?;
                        continue;
                    }
                    Err(e) => return Err(e.into()),
                }
                self.0.read_exact(&mut h[1..])?;
                let (fin, op) = (h[0] & 0x80 != 0, h[0] & 0x0f);
                let len = match h[1] & 0x7f {
                    126 => {
                        let mut b = [0u8; 2];
                        self.0.read_exact(&mut b)?;
                        u16::from_be_bytes(b) as u64
                    }
                    127 => {
                        let mut b = [0u8; 8];
                        self.0.read_exact(&mut b)?;
                        u64::from_be_bytes(b)
                    }
                    n => n as u64,
                };
                if h[1] & 0x80 != 0 || len > 1 << 20 {
                    bail!("Nexus's sign-in sent a frame rmm doesn't accept");
                }
                let mut data = vec![0u8; len as usize];
                self.0.read_exact(&mut data)?;
                match op {
                    0x8 => bail!("Nexus closed the sign-in connection"),
                    0x9 => self.frame(0xA, &data)?,
                    0x0 | 0x1 => {
                        text.extend(data);
                        if fin {
                            return Ok(String::from_utf8(text)?);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Update {
    pub id: String,
    pub name: String,
    pub mod_id: u64,
    pub installed: Option<String>,
    pub latest: Option<String>,
    /// The file that replaces the installed one.
    pub file_id: u64,
    pub file_name: String,
    pub url: String,
}

#[derive(Serialize, Deserialize, Default)]
struct Cache {
    /// Nexus mod id -> when its files were read (seconds since 1970).
    checked: BTreeMap<u64, u64>,
    /// Nexus mod id -> its files.json.
    files: BTreeMap<u64, Value>,
}

/// The game's category names on Nexus, apart from the cache: the mod list reads them on every
/// reload, and the cache grows with every mod's files.
const CATEGORIES: &str = "nexus-categories.json";

/// Nexus's category names for the instance's game, as last read (no network).
pub fn category_names(inst: &Instance) -> BTreeMap<u64, String> {
    fs::read_to_string(inst.root.join(CATEGORIES)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// Read the game's category names from Nexus, once. Returns whether it read them.
// ponytail: read once and kept; a category Nexus renames keeps its old name until nexus-categories.json is deleted.
pub fn refresh_categories(inst: &Instance, client: &Client) -> Result<bool> {
    if !category_names(inst).is_empty() {
        return Ok(false);
    }
    let game = client.get(&format!("/v1/games/{}.json", inst.config.game.nexus_domain()))?;
    let names: BTreeMap<u64, String> = game["categories"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| Some((c["category_id"].as_u64()?, c["name"].as_str()?.to_string())))
        .collect();
    fs::write(inst.root.join(CATEGORIES), serde_json::to_string(&names)?)?;
    Ok(!names.is_empty())
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The last check's updates, by mod id, held ones included (see `offered`).
const UPDATES: &str = "nexus-updates.json";

/// What the last update check found, by mod id (no network). `offered` says which still apply.
pub fn saved_updates(inst: &Instance) -> BTreeMap<String, Update> {
    fs::read_to_string(inst.root.join(UPDATES)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// Whether a found update still applies to the mod: not held, not skipped, not installed since.
pub fn offered(u: &Update, meta: &crate::instance::ModMeta) -> bool {
    meta.nexus.as_ref().is_some_and(|n| n.mod_id == u.mod_id && n.file_id != Some(u.file_id)) && !meta.pinned
        && meta.skip_file != Some(u.file_id)
        && (meta.skip_version.is_none() || meta.skip_version != u.latest)
}

/// How to hold a mod's updates back.
#[derive(Deserialize, Clone, Copy, Debug, PartialEq, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Hold {
    /// Keep the mod at its version: no update is offered until released.
    Pin,
    /// Skip the newer file the last check found; a later one is offered.
    Skip,
    /// Offer updates again.
    Release,
}

/// Hold back (or release) the updates of mods. Lives with the mods, like notes, so it isn't in
/// the undo history. Skip needs an update found by the last check.
pub fn hold_updates(inst: &Instance, ids: &[String], hold: Hold) -> Result<()> {
    let found = saved_updates(inst);
    inst.locked(|| {
        for id in ids {
            if crate::instance::is_separator(id) || !inst.mod_dir(id).join(crate::instance::MOD_META).is_file() {
                bail!("mod '{id}' is not installed");
            }
            let mut meta = inst.mod_meta(id);
            match hold {
                Hold::Pin => meta.pinned = true,
                Hold::Release => (meta.pinned, meta.skip_file, meta.skip_version) = (false, None, None),
                Hold::Skip => {
                    let u = found.get(id).filter(|u| meta.nexus.as_ref().is_some_and(|n| n.mod_id == u.mod_id));
                    meta.skip_file = Some(u.with_context(|| format!("no update found for '{id}'; check for updates first"))?.file_id);
                }
            }
            inst.write_mod_meta(id, &meta)?;
        }
        Ok(())
    })
}

/// A version's numbers, for ordering: "1.10b" -> [1, 10]. Nexus versions are free text.
fn ver_key(v: &str) -> Vec<u64> {
    v.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty()).map(|s| s.parse().unwrap_or(u64::MAX)).collect()
}

/// A mod's changelog on Nexus, newest first: the versions after `since` (the installed one), or
/// the newest three when that's unknown.
pub fn changelog(inst: &Instance, client: &Client, mod_id: u64, since: Option<&str>) -> Result<Vec<(String, Vec<String>)>> {
    let v = client.get(&format!("/v1/games/{}/mods/{mod_id}/changelogs.json", inst.config.game.nexus_domain()))?;
    Ok(changelog_after(&v, since))
}

fn changelog_after(v: &Value, since: Option<&str>) -> Vec<(String, Vec<String>)> {
    let mut all: Vec<(String, Vec<String>)> = v
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, l)| (k.clone(), l.as_array().into_iter().flatten().filter_map(|s| s.as_str().map(|s| s.trim().to_owned())).filter(|s| !s.is_empty()).collect()))
        .collect();
    all.sort_by_key(|(k, _)| std::cmp::Reverse(ver_key(k)));
    match since.map(ver_key).filter(|k| !k.is_empty()) {
        Some(have) => all.into_iter().take_while(|(k, _)| ver_key(k) > have).collect(),
        None => all.into_iter().take(3).collect(),
    }
}

/// Mods with a newer file on Nexus, leaving out the ones the user holds back (`hold_updates`).
/// `ids` limits the check (default: every mod with Nexus ids).
pub fn check_updates(inst: &Instance, client: &Client, ids: Option<&[String]>) -> Result<Vec<Update>> {
    let domain = inst.config.game.nexus_domain();
    let cache_path = inst.root.join(CACHE);
    let _ = refresh_categories(inst, client); // names for the Category column; the check stands without them
    let mut cache: Cache = fs::read_to_string(&cache_path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let mods: Vec<(String, crate::instance::ModMeta)> = inst
        .profile()?
        .mods
        .into_iter()
        .filter(|m| !crate::instance::is_separator(&m.id) && ids.is_none_or(|ids| ids.contains(&m.id)))
        .map(|m| (m.id.clone(), inst.mod_meta(&m.id)))
        .filter(|(_, meta)| meta.nexus.is_some())
        .collect();
    // Mods that changed lately; others are re-read only when their cache is old.
    let recent: BTreeMap<u64, u64> = client
        .get(&format!("/v1/games/{domain}/mods/updated.json?period=1m"))?
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|u| Some((u["mod_id"].as_u64()?, u["latest_file_update"].as_u64().unwrap_or(0))))
        .collect();
    let mut found = saved_updates(inst);
    found.retain(|id, _| !mods.iter().any(|(m, _)| m == id));
    let mut out = Vec::new();
    let known = requirements(inst);
    let mut reread = Vec::new(); // mods whose requirements to read again
    for (i, (id, meta)) in mods.iter().enumerate() {
        crate::progress::report("checking Nexus for updates", i, mods.len());
        let n = meta.nexus.as_ref().unwrap();
        let checked = cache.checked.get(&n.mod_id).copied();
        let stale = match checked {
            None => true,
            Some(t) => recent.get(&n.mod_id).is_some_and(|&u| u >= t) || now().saturating_sub(t) > crate::settings::get().nexus.recheck_days * 86400,
        };
        if stale || !cache.files.contains_key(&n.mod_id) {
            let files = client.get(&format!("/v1/games/{domain}/mods/{}/files.json", n.mod_id))?;
            cache.files.insert(n.mod_id, files);
            cache.checked.insert(n.mod_id, now());
            reread.push(n.mod_id);
        } else if !known.contains_key(&n.mod_id) {
            reread.push(n.mod_id);
        }
        if let Some(u) = newer(id, &meta.name, n, &cache.files[&n.mod_id], domain) {
            if offered(&u, meta) {
                out.push(u.clone());
            }
            found.insert(id.clone(), u);
        }
    }
    fs::write(&cache_path, serde_json::to_string(&cache)?)?;
    fs::write(inst.root.join(UPDATES), serde_json::to_string(&found)?)?; // for the mod list's Update tags
    let _ = refresh_requirements(inst, client, &reread); // the update check stands without them
    Ok(out)
}

/// The update for one mod from its files.json, if there is one.
fn newer(id: &str, name: &str, n: &crate::instance::NexusRef, files: &Value, domain: &str) -> Option<Update> {
    let list = files["files"].as_array()?;
    let file = |fid: u64| list.iter().find(|f| f["file_id"].as_u64() == Some(fid));
    let target = match n.file_id {
        // Follow old -> new until the end of the chain.
        Some(mut fid) => {
            let updates = files["file_updates"].as_array().cloned().unwrap_or_default();
            let start = fid;
            for _ in 0..updates.len() {
                match updates.iter().find(|u| u["old_file_id"].as_u64() == Some(fid)).and_then(|u| u["new_file_id"].as_u64()) {
                    Some(next) => fid = next,
                    None => break,
                }
            }
            if fid == start {
                return None;
            }
            file(fid)?
        }
        // No file id (an MO2 import, say): the newest main file, if its version differs.
        None => {
            let main = newest_main(files)?;
            let v = main["version"].as_str();
            if v.is_none() || v == n.version.as_deref() {
                return None;
            }
            main
        }
    };
    let fid = target["file_id"].as_u64()?;
    Some(Update {
        id: id.into(),
        name: name.into(),
        mod_id: n.mod_id,
        installed: n.version.clone(),
        latest: target["version"].as_str().map(str::to_owned),
        file_id: fid,
        file_name: target["file_name"].as_str().or(target["name"].as_str()).unwrap_or_default().to_string(),
        url: format!("https://www.nexusmods.com/{domain}/mods/{}?tab=files&file_id={fid}", n.mod_id),
    })
}

/// A mod's newest main file in its files.json.
fn newest_main(files: &Value) -> Option<&Value> {
    files["files"].as_array()?.iter().filter(|f| f["category_name"] == "MAIN").max_by_key(|f| f["uploaded_timestamp"].as_u64().unwrap_or(0))
}

/// Download a mod file into `<instance>/downloads/`. Accounts without Premium need the key and
/// expiry from an nxm link (the website's download button); Premium accounts can pass bare ids.
pub fn download(inst: &Instance, client: &Client, link: &NxmLink) -> Result<PathBuf> {
    download_info(inst, client, link).map(|(path, _)| path)
}

/// `download`, plus the file's details from Nexus.
fn download_info(inst: &Instance, client: &Client, link: &NxmLink) -> Result<(PathBuf, Value)> {
    let domain = inst.config.game.nexus_domain();
    if link.game != domain {
        bail!("the link is for {}, this instance is {domain}", link.game);
    }
    if link.key.is_none() && client.validate()?["is_premium"] != true {
        bail!(
            "free Nexus accounts download through the website: on the mod's Files tab (https://www.nexusmods.com/{domain}/mods/{}?tab=files), click \"Mod manager download\" on the file you want. With rmm set to open Nexus links it installs here; you can also paste that button's nxm:// link",
            link.mod_id
        );
    }
    let query = match (&link.key, link.expires) {
        (Some(k), Some(e)) => format!("?key={k}&expires={e}"),
        _ => String::new(),
    };
    fetch_file(inst, client, link.mod_id, link.file_id, &query)
}

/// Download a file into the downloads folder under its name on Nexus, unless a file of that name
/// and size is there already. Returns where it is and the file's details. (Download links of
/// newer uploads end in an id, not the file's name.)
pub(crate) fn fetch_file(inst: &Instance, client: &Client, mod_id: u64, file_id: u64, query: &str) -> Result<(PathBuf, Value)> {
    let base = format!("/v1/games/{}/mods/{mod_id}/files/{file_id}", inst.config.game.nexus_domain());
    let (dest, info, there) = downloaded(inst, client, mod_id, file_id)?;
    if there {
        return Ok((dest, info));
    }
    let name = dest.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let links = client.get(&format!("{base}/download_link.json{query}"))?;
    let uri = links.as_array().and_then(|l| l.first()).and_then(|l| l["URI"].as_str()).context("Nexus sent no download link")?;
    fs::create_dir_all(dest.parent().unwrap())?;
    fetch_sized(uri, &dest, info["size_in_bytes"].as_u64(), &format!("downloading {name} (MB)"))?;
    Ok((dest, info))
}

/// Where a Nexus file goes in the downloads folder, its details, and whether it's there already
/// (by name and size). One request.
pub(crate) fn downloaded(inst: &Instance, client: &Client, mod_id: u64, file_id: u64) -> Result<(PathBuf, Value, bool)> {
    let info = client.get(&format!("/v1/games/{}/mods/{mod_id}/files/{file_id}.json", inst.config.game.nexus_domain()))?;
    let name = info["file_name"].as_str().map(crate::install::sanitize).unwrap_or_else(|| format!("{mod_id}-{file_id}"));
    let dest = inst.downloads_dir().join(&name);
    let there = info["size_in_bytes"].as_u64().is_some_and(|s| fs::metadata(&dest).is_ok_and(|m| m.is_file() && m.len() == s));
    Ok((dest, info, there))
}

pub(crate) fn fetch_url(url: &str, dest: &Path) -> Result<()> {
    fetch_sized(url, dest, None, "downloading")
}

/// Download `url` to `dest` through `dest.part`, which a failed or stopped download leaves for
/// the next try to carry on from. A known `size` shows progress (in MB) and checks the result.
fn fetch_sized(url: &str, dest: &Path, size: Option<u64>, stage: &str) -> Result<()> {
    // curl rejects spaces and other raw bytes in a URL, and Nexus's CDN links carry file names.
    let url: String = url.bytes().map(|b| if b.is_ascii_graphic() { char::from(b).to_string() } else { format!("%{b:02X}") }).collect();
    let mut tmp = dest.as_os_str().to_owned();
    tmp.push(".part");
    let tmp = PathBuf::from(tmp);
    if size.is_some_and(|s| fs::metadata(&tmp).is_ok_and(|m| m.len() > s)) {
        fs::remove_file(&tmp)?; // another file's leftovers
    }
    let mb = |b: u64| (b >> 20) as usize;
    let len = |p: &Path| fs::metadata(p).map_or(0, |m| m.len());
    let mut resume = true;
    loop {
        if size.is_some_and(|s| len(&tmp) == s) {
            break; // finished last time, short of the rename
        }
        let mut cmd = Command::new("curl");
        cmd.args(["-fsSL", "--proto", "=http,https", "--proto-redir", "=http,https", "--retry", "2"]);
        if resume {
            cmd.args(["-C", "-"]); // carry on from what .part has
        }
        cmd.arg("-o").arg(&tmp).args(["--", &url]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
        #[cfg(windows)]
        std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000); // no console window
        let mut child = cmd.spawn().context("cannot run curl")?;
        let status = loop {
            if let Some(s) = child.try_wait()? {
                break s;
            }
            crate::progress::report(stage, mb(len(&tmp)), size.map_or(0, |s| mb(s).max(1)));
            std::thread::sleep(Duration::from_millis(250));
        };
        if status.success() {
            break;
        }
        if resume && status.code() == Some(33) {
            // The server can't send part of a file: start over.
            let _ = fs::remove_file(&tmp);
            resume = false;
            continue;
        }
        let mut err = String::new();
        let _ = std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut err);
        let kept = len(&tmp);
        let more = if kept > 0 { format!(" ({} MB kept; trying again carries on from there)", mb(kept)) } else { String::new() };
        bail!("download failed: {}{more}", err.trim());
    }
    if let Some(s) = size {
        let got = fs::metadata(&tmp)?.len();
        if got != s {
            let _ = fs::remove_file(&tmp);
            bail!("the download came out at {got} bytes, Nexus says {s}; try again");
        }
    }
    fs::rename(&tmp, dest)?;
    Ok(())
}

/// The downloads identified on Nexus, by file name (lowercase), in `downloads/.nexus.json`: what
/// rmm downloaded itself and what `identify` found.
pub fn identified(inst: &Instance) -> BTreeMap<String, Identified> {
    fs::read_to_string(inst.downloads_dir().join(IDENTIFIED)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

const IDENTIFIED: &str = ".nexus.json";

/// A download's mod and file on Nexus.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Identified {
    /// The mod's name on Nexus.
    pub name: Option<String>,
    pub nexus: crate::instance::NexusRef,
}

fn remember_download(inst: &Instance, file: &str, id: Identified) -> Result<()> {
    let mut all = identified(inst);
    all.insert(file.to_lowercase(), id);
    fs::write(inst.downloads_dir().join(IDENTIFIED), serde_json::to_string_pretty(&all)?)?;
    Ok(())
}

#[derive(Serialize, Debug, Default)]
pub struct IdentifyReport {
    /// File name, then what it is.
    pub found: Vec<(String, Identified)>,
    /// Files Nexus doesn't know (not from Nexus, or changed since).
    pub not_found: Vec<String>,
    /// File name, then why.
    pub failed: Vec<(String, String)>,
    /// Installed mods that got their Nexus ids from these files.
    pub mods: Vec<String>,
}

/// Look downloads up on Nexus by their MD5, many in one go (MO2's Query Info): `names` in the
/// downloads folder, or every one not identified yet. What's found is kept for installs from the
/// file, and installed mods made from one that have no Nexus ids get them, so update checks
/// see them.
pub fn identify(inst: &Instance, client: &Client, names: &[String]) -> Result<IdentifyReport> {
    let known = identified(inst);
    let names: Vec<String> = if names.is_empty() {
        crate::install::downloads(inst)?.into_iter().map(|d| d.name).filter(|n| !known.contains_key(&n.to_lowercase())).collect()
    } else {
        names.to_vec()
    };
    let domain = inst.config.game.nexus_domain();
    let mut r = IdentifyReport::default();
    for (i, name) in names.iter().enumerate() {
        crate::progress::report("identifying downloads", i, names.len());
        let path = inst.downloads_dir().join(name);
        if name.contains(['/', '\\']) || !path.is_file() {
            r.failed.push((name.clone(), "no such download".into()));
            continue;
        }
        let md5 = md5_file(&path)?;
        let hits = match client.get(&format!("/v1/games/{domain}/mods/md5_search/{md5}.json")) {
            Ok(v) => v,
            Err(e) if format!("{e:#}").starts_with("Nexus has no ") => {
                r.not_found.push(name.clone());
                continue;
            }
            // A refused key or the day's requests used up: the rest would fail the same way.
            Err(e) if ["refused the API key", "request limit"].iter().any(|m| format!("{e:#}").contains(m)) => {
                r.failed.extend(names[i..].iter().map(|n| (n.clone(), format!("{e:#}"))));
                break;
            }
            Err(e) => {
                r.failed.push((name.clone(), format!("{e:#}")));
                continue;
            }
        };
        let hit = hits.as_array().and_then(|h| h.iter().find(|h| h["file_details"]["md5"].as_str().is_none_or(|m| m.eq_ignore_ascii_case(&md5))));
        let Some(hit) = hit.filter(|h| h["mod"]["mod_id"].is_u64()) else {
            r.not_found.push(name.clone());
            continue;
        };
        let id = Identified {
            name: hit["mod"]["name"].as_str().map(str::to_owned),
            nexus: crate::instance::NexusRef {
                mod_id: hit["mod"]["mod_id"].as_u64().unwrap_or_default(),
                file_id: hit["file_details"]["file_id"].as_u64(),
                version: hit["file_details"]["version"].as_str().map(str::to_owned),
                category: hit["mod"]["category_id"].as_u64(),
            },
        };
        remember_download(inst, name, id.clone())?;
        r.found.push((name.clone(), id));
    }
    // Installed mods made from these files, without Nexus ids.
    let found: BTreeMap<String, &Identified> = r.found.iter().map(|(n, id)| (n.to_lowercase(), id)).collect();
    for d in crate::install::downloads(inst)? {
        let Some(id) = found.get(&d.name.to_lowercase()) else { continue };
        for m in d.mods {
            if inst.mod_meta(&m).nexus.is_none() {
                remember(inst, &m, id.nexus.clone())?;
                r.mods.push(m);
            }
        }
    }
    if found.values().any(|id| id.nexus.category.is_some()) {
        let _ = refresh_categories(inst, client);
    }
    Ok(r)
}

/// A file's MD5, which Nexus finds files by: certutil on Windows, md5sum elsewhere.
fn md5_file(path: &Path) -> Result<String> {
    let mut cmd = if cfg!(windows) { Command::new("certutil") } else { Command::new("md5sum") };
    if cfg!(windows) {
        cmd.arg("-hashfile").arg(path).arg("MD5");
    } else {
        cmd.arg(path);
    }
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
    let out = cmd.output().context("cannot run certutil")?;
    let hex = |w: &str| w.len() == 32 && w.bytes().all(|b| b.is_ascii_hexdigit());
    // certutil: the hash on the second line (with spaces between bytes before Windows 10);
    // md5sum: the first word.
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| [l.split_whitespace().next().unwrap_or("").to_string(), l.replace(' ', "")].into_iter().find(|w| hex(w)))
        .map(|w| w.to_lowercase())
        .with_context(|| format!("cannot read the MD5 of {}", path.display()))
}

/// A downloaded file and what to record about it.
#[derive(Serialize, Debug)]
pub struct Fetched {
    pub path: PathBuf,
    /// The mod's name on Nexus.
    pub name: Option<String>,
    pub nexus: crate::instance::NexusRef,
}

/// Download a file and look up its mod's name and the file's version.
pub fn fetch(inst: &Instance, client: &Client, link: &NxmLink) -> Result<Fetched> {
    let (path, info) = download_info(inst, client, link)?;
    let domain = inst.config.game.nexus_domain();
    let m = client.get(&format!("/v1/games/{domain}/mods/{}.json", link.mod_id)).unwrap_or_default();
    let name = m["name"].as_str().map(str::to_owned);
    let nexus = crate::instance::NexusRef {
        mod_id: link.mod_id,
        file_id: Some(link.file_id),
        version: info["version"].as_str().map(str::to_owned),
        category: m["category_id"].as_u64(),
    };
    if nexus.category.is_some() {
        let _ = refresh_categories(inst, client); // the name is a nicety; the download stands without it
    }
    if let Some(file) = path.file_name() {
        // So an install from the Downloads tab later records the ids too.
        let _ = remember_download(inst, &file.to_string_lossy(), Identified { name: name.clone(), nexus: nexus.clone() });
    }
    Ok(Fetched { path, name, nexus })
}

/// Record where an installed mod comes from on Nexus.
pub fn remember(inst: &Instance, id: &str, nexus: crate::instance::NexusRef) -> Result<()> {
    let mut meta = inst.mod_meta(id);
    meta.nexus = Some(nexus);
    inst.write_mod_meta(id, &meta)
}

/// Download a file and install it (FOMOD installers take their defaults unless `choices` are
/// given), recording its Nexus ids so update checks find it.
pub fn install(inst: &Instance, client: &Client, link: &NxmLink, choices: Option<&crate::fomod::Choices>) -> Result<crate::install::InstallReport> {
    let f = fetch(inst, client, link)?;
    let r = crate::install::install(inst, &f.path, f.name.as_deref(), true, Some(&choices.cloned().unwrap_or_default()))?;
    remember(inst, &r.id, f.nexus)?;
    let _ = refresh_requirements(inst, client, &[link.mod_id]); // Problems uses them; the install stands without
    Ok(r)
}

/// A link for bare ids, which only Premium accounts can download without the website's button.
pub fn premium_link(inst: &Instance, mod_id: u64, file_id: u64) -> NxmLink {
    NxmLink { game: inst.config.game.nexus_domain().into(), mod_id, file_id, key: None, expires: None }
}

/// What to download: an nxm:// link, a mod page link (`nexusmods.com/<game>/mods/<id>`, with
/// `file_id=` when it names a file), `<mod id>/<file id>`, or a bare mod id. Without a file the
/// mod's newest main file is taken. All but nxm links download only with Premium.
pub fn resolve(inst: &Instance, client: &Client, what: &str) -> Result<NxmLink> {
    let what = what.trim();
    if what.starts_with("nxm://") {
        return parse_nxm(what);
    }
    let domain = inst.config.game.nexus_domain();
    let (mod_id, file_id) = if let Some(i) = what.find("nexusmods.com/") {
        let path = &what[i + "nexusmods.com/".len()..];
        let (game, rest) = path.split_once("/mods/").context("that isn't a mod page link (nexusmods.com/<game>/mods/<id>)")?;
        if !game.eq_ignore_ascii_case(domain) {
            bail!("the link is for {game}, this instance is {domain}");
        }
        let digits = |s: &str| s.chars().take_while(char::is_ascii_digit).collect::<String>();
        let file = rest.split_once("file_id=").map(|(_, f)| digits(f)).filter(|f| !f.is_empty());
        (digits(rest).parse().context("no mod id in the link")?, file.map(|f| f.parse()).transpose()?)
    } else {
        match what.split_once('/') {
            Some((m, f)) => (m.trim().parse().context("give <mod id>/<file id>")?, Some(f.trim().parse().context("give <mod id>/<file id>")?)),
            None => (what.parse().context("give an nxm:// link, a mod page link or a mod id")?, None),
        }
    };
    let file_id = match file_id {
        Some(f) => f,
        None => {
            let files = client.get(&format!("/v1/games/{domain}/mods/{mod_id}/files.json"))?;
            newest_main(&files).and_then(|f| f["file_id"].as_u64()).with_context(|| format!("mod {mod_id} has no main file; name one with <mod id>/<file id>"))?
        }
    };
    Ok(premium_link(inst, mod_id, file_id))
}

/// What the signed-in account has endorsed, abstained from and tracks on this game's Nexus, as
/// last read (`refresh_account`), kept beside the key: it's the account's, not the instance's.
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct Account {
    /// Nexus mod id -> "Endorsed" or "Abstained".
    pub endorsements: BTreeMap<u64, String>,
    pub tracked: std::collections::BTreeSet<u64>,
    /// When it was read (seconds since 1970).
    pub read: u64,
}

fn accounts() -> BTreeMap<String, Account> {
    crate::config_file("nexus-account.json").and_then(|p| fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save_account(domain: &str, a: &Account) -> Result<()> {
    let path = crate::config_file("nexus-account.json").context("no config folder")?;
    let mut all = accounts();
    all.insert(domain.into(), a.clone());
    if let Some(d) = path.parent() {
        fs::create_dir_all(d)?;
    }
    crate::instance::write_atomic(&path, &serde_json::to_string_pretty(&all)?)
}

/// The account's endorsements and tracked mods for this game, as last read (no network).
pub fn account(inst: &Instance) -> Account {
    accounts().remove(inst.config.game.nexus_domain()).unwrap_or_default()
}

/// Read the account's endorsements and tracked mods from Nexus (two requests) and keep them.
pub fn refresh_account(inst: &Instance, client: &Client) -> Result<Account> {
    let domain = inst.config.game.nexus_domain();
    let ours = |v: &Value| v["domain_name"].as_str() == Some(domain);
    let mut a = Account { read: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()), ..Default::default() };
    for e in client.get("/v1/user/endorsements.json")?.as_array().into_iter().flatten().filter(|e| ours(e)) {
        if let (Some(id), Some(s)) = (e["mod_id"].as_u64(), e["status"].as_str()) {
            a.endorsements.insert(id, s.into());
        }
    }
    a.tracked = client.get("/v1/user/tracked_mods.json")?.as_array().into_iter().flatten().filter(|t| ours(t)).filter_map(|t| t["mod_id"].as_u64()).collect();
    save_account(domain, &a)?;
    Ok(a)
}

/// An installed mod's Nexus mod id and version, for endorsing and tracking.
fn nexus_of(inst: &Instance, id: &str) -> Result<(u64, Option<String>)> {
    let n = inst.mod_meta(id).nexus.with_context(|| format!("{id} has no Nexus id: Identify on Nexus (Downloads) or install it from Nexus first"))?;
    Ok((n.mod_id, n.version))
}

/// Endorse an installed mod on Nexus, or abstain (Nexus's way to take an endorsement back, or to
/// say you won't). Acts publicly on the account. Returns the account's state after.
pub fn endorse(inst: &Instance, client: &Client, id: &str, yes: bool) -> Result<Account> {
    let (mod_id, version) = nexus_of(inst, id)?;
    let domain = inst.config.game.nexus_domain();
    let what = if yes { "endorse" } else { "abstain" };
    let r = client.form("POST", &format!("/v1/games/{domain}/mods/{mod_id}/{what}.json"), &format!("version={}", urlencode(version.as_deref().unwrap_or(""))))?;
    let mut a = account(inst);
    a.endorsements.insert(mod_id, r["status"].as_str().unwrap_or(if yes { "Endorsed" } else { "Abstained" }).into());
    save_account(domain, &a)?;
    Ok(a)
}

/// Track an installed mod on Nexus (its updates show in the account's tracking centre), or stop.
pub fn track(inst: &Instance, client: &Client, id: &str, yes: bool) -> Result<Account> {
    let (mod_id, _) = nexus_of(inst, id)?;
    let domain = inst.config.game.nexus_domain();
    client.form(if yes { "POST" } else { "DELETE" }, &format!("/v1/user/tracked_mods.json?domain_name={domain}"), &format!("mod_id={mod_id}"))?;
    let mut a = account(inst);
    if yes {
        a.tracked.insert(mod_id);
    } else {
        a.tracked.remove(&mod_id);
    }
    save_account(domain, &a)?;
    Ok(a)
}

fn urlencode(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// The page where users make a personal API key.
pub const KEYS_PAGE: &str = "https://next.nexusmods.com/settings/api-keys";

/// Make nxm:// links open `rmm nxm <link>` for this instance. Only when the user asks: it changes
/// which program the system opens these links with (HKCU\Software\Classes\nxm on Windows, an
/// xdg-mime default on Linux). Returns what it set.
pub fn register_handler(rmm_exe: &Path, instance: &Path) -> Result<String> {
    let command = format!("\"{}\" -i \"{}\" nxm \"%1\"", rmm_exe.display(), instance.display());
    #[cfg(windows)]
    {
        use winreg::RegKey;
        use winreg::enums::HKEY_CURRENT_USER;
        let (nxm, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(r"Software\Classes\nxm")?;
        nxm.set_value("", &"URL:NXM Protocol")?;
        nxm.set_value("URL Protocol", &"")?;
        let (cmd, _) = nxm.create_subkey(r"shell\open\command")?;
        cmd.set_value("", &command)?;
        Ok(format!(r"HKCU\Software\Classes\nxm\shell\open\command = {command}"))
    }
    #[cfg(not(windows))]
    {
        let apps = dirs::data_dir().context("no data folder")?.join("applications");
        fs::create_dir_all(&apps)?;
        let exec = command.replace("%1", "%u");
        fs::write(apps.join("rmm-nxm.desktop"), format!("[Desktop Entry]\nType=Application\nName=rmm (Nexus links)\nExec={exec}\nNoDisplay=true\nMimeType=x-scheme-handler/nxm;\n"))?;
        let ok = Command::new("xdg-mime").args(["default", "rmm-nxm.desktop", "x-scheme-handler/nxm"]).status().is_ok_and(|s| s.success());
        if !ok {
            bail!("wrote {}, but xdg-mime couldn't make it the nxm handler", apps.join("rmm-nxm.desktop").display());
        }
        Ok(format!("x-scheme-handler/nxm -> rmm-nxm.desktop ({exec})"))
    }
}

/// The command Windows runs for nxm:// links, when a program has them (read only; Windows only).
pub fn handler_command() -> Option<String> {
    #[cfg(windows)]
    {
        use winreg::RegKey;
        use winreg::enums::HKEY_CLASSES_ROOT;
        RegKey::predef(HKEY_CLASSES_ROOT).open_subkey(r"nxm\shell\open\command").ok()?.get_value("").ok()
    }
    #[cfg(not(windows))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changelog_after_the_installed_version() {
        let v = serde_json::json!({ "1.9": ["nine"], "1.10": ["ten", " "], "2.0b": ["two"], "1.2": ["old"] });
        let names = |l: Vec<(String, Vec<String>)>| l.into_iter().map(|(k, _)| k).collect::<Vec<_>>();
        assert_eq!(names(changelog_after(&v, Some("1.9"))), ["2.0b", "1.10"]);
        assert_eq!(names(changelog_after(&v, None)), ["2.0b", "1.10", "1.9"]);
        assert_eq!(changelog_after(&v, Some("1.9"))[1].1, ["ten"]);
    }

    /// Single sign-on against a fake Nexus: rmm's id goes out, the approval page opens once the
    /// connection token arrives, a ping gets its pong, and the key comes back.
    #[test]
    fn single_sign_on() {
        use std::io::{Read, Write};
        use std::net::TcpStream;
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}/", server.local_addr().unwrap());
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let fake = std::thread::spawn(move || {
            let (mut s, _) = server.accept().unwrap();
            let mut head = Vec::new();
            let mut b = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                s.read_exact(&mut b).unwrap();
                head.push(b[0]);
            }
            assert!(String::from_utf8_lossy(&head).contains("Upgrade: websocket"));
            s.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n").unwrap();
            let send = |s: &mut TcpStream, op: u8, text: &str| {
                let mut f = vec![0x80 | op, text.len() as u8];
                f.extend(text.as_bytes());
                s.write_all(&f).unwrap();
            };
            // Short client frames: 2 header bytes and a 4-byte mask.
            let read = |s: &mut TcpStream| -> (u8, String) {
                let mut h = [0u8; 6];
                s.read_exact(&mut h).unwrap();
                assert_eq!(h[1] & 0x80, 0x80, "client frames are masked");
                let mut d = vec![0u8; (h[1] & 0x7f) as usize];
                s.read_exact(&mut d).unwrap();
                (h[0] & 0x0f, d.iter().enumerate().map(|(i, b)| (b ^ h[2 + i % 4]) as char).collect())
            };
            let (op, hello) = read(&mut s);
            let hello: Value = serde_json::from_str(&hello).unwrap();
            assert_eq!((op, hello["protocol"].clone(), hello["token"].clone()), (1, 2.into(), Value::Null));
            let id = hello["id"].as_str().unwrap().to_owned();
            assert!(id.len() == 36 && id.as_bytes()[14] == b'4', "a v4 uuid: {id}");
            send(&mut s, 1, r#"{"success":true,"data":{"connection_token":"tok"},"error":null}"#);
            let page = rx.recv().unwrap();
            assert_eq!(page, format!("https://www.nexusmods.com/sso?id={id}&application=rmm-test"));
            send(&mut s, 9, "hi");
            assert_eq!(read(&mut s), (0xA, "hi".to_string()));
            send(&mut s, 1, r#"{"success":true,"data":{"api_key":"KEY-123"},"error":null}"#);
        });
        let open = |page: &str| -> Result<()> {
            tx.send(page.to_owned()).unwrap();
            Ok(())
        };
        assert_eq!(sso_key(&url, "rmm-test", Duration::from_secs(20), &open).unwrap(), "KEY-123");
        fake.join().unwrap();
    }

    /// The real service (`cargo test -- --ignored`): TLS, the upgrade and the first answer. Sends
    /// only a random id, and stops before any approval page.
    #[test]
    #[ignore]
    fn single_sign_on_reaches_nexus() {
        let err = sso_key(SSO, "rmm", Duration::from_secs(30), &|_| bail!("got a connection token")).unwrap_err();
        assert_eq!(format!("{err:#}"), "got a connection token");
    }

    #[test]
    fn nxm_links() {
        let l = parse_nxm("nxm://SkyrimSpecialEdition/mods/266/files/123456?key=AbC-1_x&expires=1700000000&user_id=42").unwrap();
        assert_eq!(l, NxmLink { game: "skyrimspecialedition".into(), mod_id: 266, file_id: 123456, key: Some("AbC-1_x".into()), expires: Some(1700000000) });
        assert_eq!(parse_nxm("nxm://fallout4/mods/1/files/2").unwrap().key, None);
        assert_eq!(parse_nxm("nxm://fallout4/mods/1/files/2?key=a%23b#x&expires=1").unwrap().key, None, "not a plain token");
        assert!(parse_nxm("nxm://skyrimspecialedition/collections/abc/revisions/3").is_err());
        assert!(parse_nxm("https://www.nexusmods.com/").is_err());
        assert!(Client::new("has space").is_err() && Client::new("quote\"").is_err());
    }

    #[test]
    fn update_chains() {
        let files = serde_json::json!({
            "files": [
                { "file_id": 1, "version": "1.0", "category_name": "OLD_VERSION", "file_name": "A-1.zip", "uploaded_timestamp": 1 },
                { "file_id": 2, "version": "1.1", "category_name": "OLD_VERSION", "file_name": "A-2.zip", "uploaded_timestamp": 2 },
                { "file_id": 3, "version": "1.2", "category_name": "MAIN", "file_name": "A-3.zip", "uploaded_timestamp": 3 }
            ],
            "file_updates": [{ "old_file_id": 1, "new_file_id": 2 }, { "old_file_id": 2, "new_file_id": 3 }]
        });
        let r = |file_id, version: &str| crate::instance::NexusRef { mod_id: 9, file_id, version: Some(version.into()), category: None };
        let u = newer("A", "A", &r(Some(1), "1.0"), &files, "skyrimspecialedition").unwrap();
        assert_eq!((u.file_id, u.latest.as_deref(), u.url.as_str()), (3, Some("1.2"), "https://www.nexusmods.com/skyrimspecialedition/mods/9?tab=files&file_id=3"));
        assert!(newer("A", "A", &r(Some(3), "1.2"), &files, "x").is_none());
        assert_eq!(newer("A", "A", &r(None, "1.1"), &files, "x").unwrap().file_id, 3, "by version without a file id");
        assert!(newer("A", "A", &r(None, "1.2"), &files, "x").is_none());
    }
}

/// What to look for on Nexus (`search`).
#[derive(Deserialize, schemars::JsonSchema, Default, Debug)]
pub struct Search {
    /// Words to match in mod names and descriptions; empty lists the category's or game's mods.
    #[serde(default)]
    pub query: String,
    /// Only this Nexus category (the names list_mods shows, such as "Armour" or "Patches").
    #[serde(default)]
    pub category: Option<String>,
    /// "relevance" (default with a query), "endorsements", "downloads", "updated" or "created".
    #[serde(default)]
    pub sort: Option<String>,
    /// How many (default 20, at most 50).
    #[serde(default)]
    pub count: Option<u32>,
    /// Skip this many, for the next page.
    #[serde(default)]
    pub offset: Option<u32>,
    /// Include adult-only mods (default false).
    #[serde(default)]
    pub adult: bool,
}

/// A mod on Nexus.
#[derive(Serialize, Debug, Default)]
pub struct Found {
    pub mod_id: u64,
    pub name: String,
    pub summary: String,
    pub category: String,
    pub author: String,
    pub version: String,
    pub endorsements: u64,
    pub downloads: u64,
    /// Last update (RFC 3339).
    pub updated: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub picture: Option<String>,
    /// The installed mod with this Nexus id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed: Option<String>,
}

#[derive(Serialize, Debug)]
pub struct Results {
    pub total: u64,
    pub mods: Vec<Found>,
}

/// A mod another one lists as required on its Nexus page (or one that lists it).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Needed {
    /// None for a requirement outside Nexus (the script extender's site, say).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mod_id: Option<u64>,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
    /// For requirements outside Nexus.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// A DLC (`name` is Nexus's name for it), checked by its master file in the game's Data folder.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dlc: bool,
}

/// A mod another one needs, or one that needs it, and the installed mod that is it.
#[derive(Serialize, Debug)]
pub struct Requirement {
    #[serde(flatten)]
    pub needed: Needed,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed: Option<String>,
}

/// A mod's page: what `search` shows plus its description and requirements.
#[derive(Serialize, Debug)]
pub struct ModInfo {
    #[serde(flatten)]
    pub found: Found,
    pub tags: Vec<String>,
    /// The page's description as plain text (BBCode removed, links kept), up to 12,000 characters.
    pub description: String,
    pub requires: Vec<Requirement>,
    pub dlc: Vec<String>,
    /// Mods that list this one as a requirement (patches and add-ons for it); the first 100 are in required_by.
    pub required_by_count: u64,
    pub required_by: Vec<Requirement>,
}

const MOD_FIELDS: &str = "modId name summary category author version endorsements downloads updatedAt pictureUrl";

/// A mod's Nexus mod id: the one rmm recorded, or the one in its archive's file name.
pub fn nexus_id_of(meta: &crate::instance::ModMeta) -> Option<u64> {
    meta.nexus.as_ref().map(|n| n.mod_id).or_else(|| crate::install::nexus_id_in_name(meta.source.as_deref()?))
}

/// What a mod's Nexus page lists as required, as last read, each with the installed mod that is
/// it. None until its requirements have been read (`refresh_requirements`).
pub fn needs(inst: &Instance, id: &str) -> Option<Vec<Requirement>> {
    let nx = nexus_id_of(&inst.mod_meta(id))?;
    let reqs = requirements(inst).remove(&nx)?;
    let installed = installed_by_nexus(inst);
    Some(reqs.into_iter().filter(|r| r.mod_id != Some(nx)).map(|n| Requirement { installed: n.mod_id.and_then(|i| installed.get(&i).cloned()), needed: n }).collect())
}

/// Installed mods by their Nexus mod id.
fn installed_by_nexus(inst: &Instance) -> BTreeMap<u64, String> {
    inst.profile().map(|p| p.mods.iter().filter_map(|m| Some((inst.mod_meta(&m.id).nexus?.mod_id, m.id.clone()))).collect()).unwrap_or_default()
}

fn found(inst: &Instance, v: &Value, installed: &BTreeMap<u64, String>) -> Found {
    let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
    let mod_id = v["modId"].as_u64().unwrap_or_default();
    Found {
        mod_id,
        name: s("name"),
        summary: s("summary"),
        category: s("category"),
        author: s("author"),
        version: s("version"),
        endorsements: v["endorsements"].as_u64().unwrap_or_default(),
        downloads: v["downloads"].as_u64().unwrap_or_default(),
        updated: s("updatedAt"),
        url: format!("https://www.nexusmods.com/{}/mods/{mod_id}", inst.config.game.nexus_domain()),
        picture: v["pictureUrl"].as_str().map(str::to_owned),
        installed: installed.get(&mod_id).cloned(),
    }
}

/// Search the game's mods on Nexus by name and description (no key needed).
pub fn search(inst: &Instance, client: &Client, s: &Search) -> Result<Results> {
    let query = s.query.trim();
    let mut filter = serde_json::json!({ "gameDomainName": [{ "value": inst.config.game.nexus_domain() }] });
    if !query.is_empty() {
        filter["filter"] = serde_json::json!([{ "op": "OR", "nameStemmed": [{ "value": query, "op": "MATCHES" }], "description": [{ "value": query, "op": "MATCHES" }] }]);
    }
    if !s.adult {
        filter["adultContent"] = serde_json::json!([{ "value": false }]);
    }
    if let Some(c) = s.category.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        // Nexus matches category names exactly; the saved names fix the case.
        let names = category_names(inst);
        let exact = names.values().find(|n| n.eq_ignore_ascii_case(c));
        if exact.is_none() && !names.is_empty() {
            bail!("Nexus has no category {c:?} for this game; it has: {}", names.values().cloned().collect::<Vec<_>>().join(", "));
        }
        filter["categoryName"] = serde_json::json!([{ "value": exact.map_or(c, String::as_str) }]);
    }
    let sort = match s.sort.as_deref().unwrap_or("relevance") {
        "relevance" if !query.is_empty() => "relevance",
        "relevance" | "endorsements" => "endorsements",
        "downloads" => "downloads",
        "updated" => "updatedAt",
        "created" => "createdAt",
        other => bail!("sort is relevance, endorsements, downloads, updated or created, not {other:?}"),
    };
    let data = client.graphql(
        &format!("query($f: ModsFilter, $s: [ModsSort!], $n: Int, $o: Int) {{ mods(filter: $f, sort: $s, count: $n, offset: $o) {{ totalCount nodes {{ {MOD_FIELDS} }} }} }}"),
        serde_json::json!({ "f": filter, "s": [{ sort: { "direction": "DESC" } }], "n": s.count.unwrap_or(20).clamp(1, 50), "o": s.offset.unwrap_or(0) }),
    )?;
    let installed = installed_by_nexus(inst);
    Ok(Results {
        total: data["mods"]["totalCount"].as_u64().unwrap_or_default(),
        mods: data["mods"]["nodes"].as_array().into_iter().flatten().map(|v| found(inst, v, &installed)).collect(),
    })
}

const REQ_NODES: &str = "nodes { modId modName notes url externalRequirement gameId }";

/// The game's number on Nexus: one mod by id needs it, and a list query for it costs Nexus too much.
fn game_number(inst: &Instance, client: &Client) -> Result<Value> {
    let game = client.graphql("query($d: String) { game(domainName: $d) { id } }", serde_json::json!({ "d": inst.config.game.nexus_domain() }))?;
    Ok(game["game"]["id"].clone())
}

/// A requirements list from Nexus, without other games' mods.
fn needed(page: &Value, game: &Value) -> Vec<Needed> {
    let game = game.to_string().trim_matches('"').to_string();
    page["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|n| n["gameId"].is_null() || n["gameId"].to_string().trim_matches('"') == game)
        .map(|n| Needed {
            mod_id: n["modId"].as_str().and_then(|i| i.parse().ok()).or(n["modId"].as_u64()).filter(|_| n["externalRequirement"] != true),
            name: n["modName"].as_str().unwrap_or_default().into(),
            notes: n["notes"].as_str().unwrap_or_default().into(),
            url: n["url"].as_str().unwrap_or_default().into(),
            dlc: false,
        })
        .collect()
}

/// A mod page's DLC requirements.
fn dlc_needed(requirements: &Value) -> Vec<Needed> {
    requirements["dlcRequirements"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| d["gameExpansion"]["name"].as_str())
        .map(|name| Needed { mod_id: None, name: name.into(), notes: String::new(), url: String::new(), dlc: true })
        .collect()
}

/// A DLC's master file, by Nexus's name for it. Others (Creation Club bundles) aren't checked.
fn dlc_master(name: &str) -> Option<&'static str> {
    let n = name.to_lowercase();
    [
        ("dawnguard", "Dawnguard.esm"),
        ("hearthfire", "HearthFires.esm"),
        ("dragonborn", "Dragonborn.esm"),
        ("automatron", "DLCRobot.esm"),
        ("wasteland workshop", "DLCworkshop01.esm"),
        ("far harbor", "DLCCoast.esm"),
        ("contraptions workshop", "DLCworkshop02.esm"),
        ("vault-tec workshop", "DLCworkshop03.esm"),
        ("nuka-world", "DLCNukaWorld.esm"),
        ("nuka world", "DLCNukaWorld.esm"),
    ]
    .iter()
    .find(|(k, _)| n.contains(k))
    .map(|(_, f)| *f)
}

/// A mod's page on Nexus: description, tags, what it needs and what needs it, each marked when installed.
pub fn mod_info(inst: &Instance, client: &Client, mod_id: u64) -> Result<ModInfo> {
    let data = client.graphql(
        &format!("query($m: ID!, $g: ID!) {{ mod(modId: $m, gameId: $g) {{ {MOD_FIELDS} gameId description tags {{ name }} modRequirements {{ dlcRequirements {{ gameExpansion {{ name }} }} nexusRequirements(count: 100) {{ {REQ_NODES} }} modsRequiringThisMod(count: 100) {{ totalCount {REQ_NODES} }} }} }} }}"),
        serde_json::json!({ "m": mod_id, "g": game_number(inst, client)? }),
    )?;
    let v = &data["mod"];
    if v.is_null() {
        bail!("Nexus has no mod {mod_id} for this game (removed or hidden?)");
    }
    let installed = installed_by_nexus(inst);
    let list = |page: &Value| -> Vec<Requirement> {
        needed(page, &v["gameId"]).into_iter().map(|n| Requirement { installed: n.mod_id.and_then(|i| installed.get(&i).cloned()), needed: n }).collect()
    };
    let r = &v["modRequirements"];
    Ok(ModInfo {
        found: found(inst, v, &installed),
        tags: v["tags"].as_array().into_iter().flatten().filter_map(|t| t["name"].as_str().map(str::to_owned)).collect(),
        description: plain_text(v["description"].as_str().unwrap_or_default()),
        requires: list(&r["nexusRequirements"]),
        dlc: r["dlcRequirements"].as_array().into_iter().flatten().filter_map(|d| d["gameExpansion"]["name"].as_str().map(str::to_owned)).collect(),
        required_by_count: r["modsRequiringThisMod"]["totalCount"].as_u64().unwrap_or_default(),
        required_by: list(&r["modsRequiringThisMod"]),
    })
}

/// What each Nexus mod lists as required on its page, by its Nexus mod id. Apart from the cache,
/// which the health check would otherwise read whole.
const REQUIREMENTS: &str = "nexus-requirements.json";

/// The requirements read so far (no network).
pub fn requirements(inst: &Instance) -> BTreeMap<u64, Vec<Needed>> {
    fs::read_to_string(inst.root.join(REQUIREMENTS)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// Read what these Nexus mods list as required on their pages, and keep it. A mod Nexus no longer
/// shows keeps what was read before.
pub fn refresh_requirements(inst: &Instance, client: &Client, mod_ids: &[u64]) -> Result<()> {
    if mod_ids.is_empty() {
        return Ok(());
    }
    let game = game_number(inst, client)?;
    let query = format!("query($m: ID!, $g: ID!) {{ mod(modId: $m, gameId: $g) {{ gameId modRequirements {{ dlcRequirements {{ gameExpansion {{ name }} }} nexusRequirements(count: 100) {{ {REQ_NODES} }} }} }} }}");
    let mut all = requirements(inst);
    for (i, &m) in mod_ids.iter().enumerate() {
        crate::progress::report("reading requirements on Nexus", i, mod_ids.len());
        let Ok(data) = client.graphql(&query, serde_json::json!({ "m": m, "g": game })) else { continue };
        let v = &data["mod"];
        if !v.is_null() {
            let r = &v["modRequirements"];
            all.insert(m, needed(&r["nexusRequirements"], &v["gameId"]).into_iter().chain(dlc_needed(r)).collect());
        }
    }
    fs::write(inst.root.join(REQUIREMENTS), serde_json::to_string(&all)?)?;
    Ok(())
}

/// Nexus mods that do another's job, by game: (domain, the mod, its stand-ins).
const STAND_INS: &[(&str, u64, &[u64])] = &[
    // Pandora runs Nemesis's and FNIS's behaviour patches; Nemesis runs FNIS's.
    ("skyrimspecialedition", 60033, &[133232]),
    ("skyrimspecialedition", 3038, &[60033, 133232]),
];

/// Whether a requirement's note (or its name) says this setup can do without it: the note calls
/// it optional or not required, or it is only for other game versions ("older than 1.6.1130",
/// "1.6.640+", "if you're on 1.5", "for Skyrim 1.5"). `game` is the game's version and the
/// major.minor pairs its versions have had; other numbers (a mod's own version) aren't read as one.
fn waived(r: &Needed, game: Option<([u32; 4], &[[u32; 2]])>) -> bool {
    use std::cmp::Ordering;
    let notes = r.notes.to_lowercase();
    let optional = regex::Regex::new(r"\b(optional|not (required|needed|necessary|mandatory)|no longer (required|needed)|if you want)\b").unwrap();
    if optional.is_match(&notes) && !notes.contains("not optional") {
        return true;
    }
    let Some((have, eras)) = game else { return false };
    let text = format!("{} {notes}", r.name.to_lowercase());
    const V: &str = r"v?(\d+(?:\.\d+)+)";
    // The phrase, and whether the game needs the requirement given how its version compares.
    let rules = [
        (format!(r"{V}\s*(?:\+|or (?:newer|later|higher|above))"), Ordering::is_ge as fn(Ordering) -> bool),
        (format!(r"{V}\s*(?:or|and) (?:older|earlier|lower|below)"), Ordering::is_le),
        (format!(r"\b(?:newer|later|higher) than\s*{V}"), Ordering::is_gt),
        (format!(r"\b(?:older than|earlier than|lower than|below|before|prior to|pre-?)\s*{V}"), Ordering::is_lt),
        (format!(r"\b(?:on|for)\s+(?:skyrim\s+|fallout 4\s+)?(?:se\s+|ae\s+)?{V}"), Ordering::is_eq),
    ];
    for (phrase, needs) in rules {
        for c in regex::Regex::new(&phrase).unwrap().captures_iter(&text) {
            let v: Vec<u32> = c[1].split('.').filter_map(|p| p.parse().ok()).take(4).collect();
            if v.len() >= 2 && eras.contains(&[v[0], v[1]]) {
                return !needs(have[..v.len()].cmp(&v[..]));
            }
        }
    }
    false
}

/// A requirement an enabled mod's Nexus page lists that this setup lacks.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Lacking {
    /// The mod whose page lists it.
    pub for_mod: String,
    pub name: String,
    /// Its Nexus mod id; None for the script extender (another site) or a DLC.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mod_id: Option<u64>,
    /// Where to get it ("nexusmods.com/...", "skse.silverlock.org"); empty for a DLC.
    pub page: String,
    /// Installed but disabled: that mod's id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub dlc: bool,
}

/// Enabled mods' Nexus requirements that aren't installed, or are but are disabled, as Problems
/// shows them: (mod id, message).
pub fn missing_requirements(inst: &Instance, profile: &crate::instance::Profile) -> Vec<(String, String)> {
    let mut by_mod: Vec<(String, Vec<String>)> = Vec::new();
    for l in lacking_requirements(inst, profile) {
        let text = match &l.disabled {
            _ if l.dlc => format!("the {} DLC, not in the game's Data folder", l.name),
            Some(id) => match inst.mod_meta(id).name {
                n if n.eq_ignore_ascii_case(&l.name) => format!("{}, installed but disabled", l.name),
                n => format!("{}, installed as {n} but disabled", l.name),
            },
            None => format!("{} ({}), not installed", l.name, l.page),
        };
        match by_mod.last_mut() {
            Some((m, v)) if *m == l.for_mod => v.push(text),
            _ => by_mod.push((l.for_mod, vec![text])),
        }
    }
    by_mod
        .into_iter()
        .map(|(m, lacking)| {
            let it = if lacking.len() == 1 { "it" } else { "them" };
            (m, format!("its Nexus page lists as required: {}. Ignore this if you have {it} another way", lacking.join("; ")))
        })
        .collect()
}

/// Enabled mods' Nexus requirements that aren't installed, or are but are disabled. A mod's Nexus
/// id is the recorded one or the one in its archive's name, and a requirement also counts as
/// installed when a mod has its name or is a stand-in for it. The script extender counts when its
/// loader is in the game folder or an enabled mod, and a DLC when its master is in the game's Data
/// folder. Other sites' requirements aren't checked, nor ones their note waives for this game (see
/// `waived`).
pub fn lacking_requirements(inst: &Instance, profile: &crate::instance::Profile) -> Vec<Lacking> {
    let all = requirements(inst);
    if all.is_empty() {
        return vec![];
    }
    let domain = inst.config.game.nexus_domain();
    let eras: &[[u32; 2]] = match inst.config.game {
        crate::game::Game::SkyrimSe => &[[1, 5], [1, 6], [1, 7]],
        crate::game::Game::Fallout4 => &[[1, 10], [1, 11]],
        crate::game::Game::Addon(_) => &[],
    };
    let game = crate::launch::game_version(&inst.config.game_path, inst.config.game).ok().map(|v| (v, eras));
    let mods: Vec<(&crate::ModEntry, Option<u64>, String)> = profile
        .mods
        .iter()
        .filter(|m| !crate::instance::is_separator(&m.id))
        .map(|m| {
            let meta = inst.mod_meta(&m.id);
            (m, nexus_id_of(&meta), meta.name)
        })
        .collect();
    let loader = inst.config.game.loader();
    let se_on = inst.config.game_path.join(loader).is_file()
        || crate::launch::script_extender_mods(inst).unwrap_or_default().iter().any(|id| profile.mods.iter().any(|m| &m.id == id && m.enabled));
    let is_se = |r: &Needed| {
        let n = r.name.to_lowercase();
        r.url.contains("silverlock.org") || n.contains("script extender") || ["skse", "skse64", "sksevr", "f4se", "f4sevr"].contains(&n.as_str())
    };
    let data_files: std::collections::HashSet<String> =
        fs::read_dir(inst.data_dir()).into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().to_lowercase()).collect();
    let mut out = Vec::new();
    for (m, nx, _) in mods.iter().filter(|x| x.0.enabled) {
        let Some(reqs) = nx.and_then(|n| all.get(&n)) else { continue };
        let lack = |r: &Needed, page: &str, disabled: Option<String>| Lacking {
            for_mod: m.id.clone(),
            name: r.name.clone(),
            mod_id: r.mod_id,
            page: page.to_string(),
            disabled,
            dlc: r.dlc,
        };
        for r in reqs.iter().filter(|r| r.mod_id.is_none_or(|id| Some(id) != *nx) && !waived(r, game)) {
            let page = match r.mod_id {
                Some(id) => format!("nexusmods.com/{domain}/mods/{id}"),
                None => r.url.trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/').to_string(),
            };
            if r.dlc {
                if dlc_master(&r.name).is_some_and(|f| !data_files.contains(&f.to_lowercase())) {
                    out.push(lack(r, "", None));
                }
                continue;
            }
            if is_se(r) {
                if !se_on {
                    out.push(lack(r, &page, None));
                }
                continue;
            }
            if r.mod_id.is_none() {
                continue; // another site's download; rmm can't tell whether it's there
            }
            let stand_ins = STAND_INS.iter().find(|s| s.0 == domain && Some(s.1) == r.mod_id).map_or(&[][..], |s| s.2);
            let same: Vec<&(&crate::ModEntry, Option<u64>, String)> =
                mods.iter().filter(|x| x.1 == r.mod_id || x.1.is_some_and(|id| stand_ins.contains(&id)) || x.2.eq_ignore_ascii_case(&r.name)).collect();
            match same.iter().find(|x| x.0.enabled).or(same.first()) {
                None => out.push(lack(r, &page, None)),
                Some(x) if !x.0.enabled => out.push(lack(r, &page, Some(x.0.id.clone()))),
                Some(_) => {}
            }
        }
    }
    out
}

#[derive(Serialize, Debug, Default)]
pub struct InstalledLacking {
    pub installed: Vec<String>,
    pub turned_on: Vec<String>,
    /// Requirement, then why.
    pub failed: Vec<(String, String)>,
    /// The script extender, DLC and other sites' downloads: for the user to get.
    pub left: Vec<Lacking>,
}

/// Install every lacking requirement that's on Nexus (its newest main file, with the installer's
/// defaults: Premium only) and turn on the installed but disabled ones, in one undoable step.
pub fn install_lacking(inst: &Instance, client: &Client) -> Result<InstalledLacking> {
    let lacking = lacking_requirements(inst, &inst.profile()?);
    let mut r = InstalledLacking::default();
    let mut off: Vec<String> = lacking.iter().filter_map(|l| l.disabled.clone()).collect();
    off.sort();
    off.dedup();
    if !off.is_empty() {
        crate::ops::set_mods_enabled(inst, &off, true)?;
        r.turned_on = off;
    }
    let mut done = std::collections::HashSet::new();
    for l in lacking.into_iter().filter(|l| l.disabled.is_none()) {
        let Some(id) = l.mod_id else {
            r.left.push(l);
            continue;
        };
        if !done.insert(id) {
            continue;
        }
        match resolve(inst, client, &id.to_string()).and_then(|link| install(inst, client, &link, None)) {
            Ok(i) => r.installed.push(i.id),
            Err(e) => r.failed.push((l.name, format!("{e:#}"))),
        }
    }
    Ok(r)
}

/// A mod page's BBCode as plain text: links keep their address, images go.
fn plain_text(bb: &str) -> String {
    let t = bb.replace("<br />", "\n").replace("<br>", "\n").replace('\r', "").replace("[*]", "\n- ");
    let t = regex::Regex::new(r"(?is)\[img[^\]]*\].*?\[/img\]").unwrap().replace_all(&t, "").into_owned();
    let t = regex::Regex::new(r"(?i)\[url=([^\]]+)\]").unwrap().replace_all(&t, "$1 ").into_owned();
    let tags = r"(?i)\[/?(b|i|u|s|center|left|right|justify|size|colou?r|font|url|img|quote|code|spoiler|list|line|heading|youtube|table|tr|td|th|hr|sub|sup|indent|email)(=[^\]]*)?\]|<[^>]*>";
    let t = regex::Regex::new(tags).unwrap().replace_all(&t, "").into_owned();
    let t = t.replace("&amp;", "&").replace("&quot;", "\"").replace("&#39;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&nbsp;", " ");
    let mut out = String::new();
    let mut blank = 0;
    for line in t.lines().map(str::trim) {
        blank = if line.is_empty() { blank + 1 } else { 0 };
        if blank < 2 {
            out.push_str(line);
            out.push('\n');
        }
    }
    let out = out.trim();
    if out.chars().count() > 12_000 { out.chars().take(12_000).collect::<String>() + "…" } else { out.to_string() }
}

#[cfg(test)]
mod text_tests {
    #[test]
    fn bbcode_to_text() {
        let bb = "[center][size=5]Frostfall [SE][/size][/center]\r<br />[img]http://x/y.png[/img]<br /><br /><br />Needs [url=https://nexusmods.com/skyrimspecialedition/mods/667]Campfire[/url] &amp; more[list][*]one[/list]";
        assert_eq!(super::plain_text(bb), "Frostfall [SE]\n\nNeeds https://nexusmods.com/skyrimspecialedition/mods/667 Campfire & more\n- one");
    }

    /// Notes from real Nexus pages (QuickLoot IE, Precision), on Skyrim 1.6.1170 and on 1.5.97.
    #[test]
    fn notes_that_waive_a_requirement() {
        let eras: &[[u32; 2]] = &[[1, 5], [1, 6], [1, 7]];
        let waived = |name: &str, notes: &str, v: [u32; 4]| super::waived(&super::Needed { mod_id: Some(1), name: name.into(), notes: notes.into(), url: String::new(), dlc: false }, Some((v, eras)));
        let (ae, se) = ([1, 6, 1170, 0], [1, 5, 97, 0]);
        for (name, notes, on_ae, on_se) in [
            ("Nemesis", "Technically optional but very strongly recommended.", true, true),
            ("QuickLoot EE", "NOT REQUIRED, simply here for visibility. Go endorse it!", true, true),
            ("I4", "OPTIONAL - Only required if you want fancy icons to show up", true, true),
            ("BEES", "Only required if you are on a version older than 1.6.1130", true, false),
            ("I4 for Skyrim 1.5", "", true, false),
            ("X", "Only for 1.6.640+", false, true),
            ("X", "Needed on 1.5.97 or older", true, false),
            ("SkyUI", "MANDATORY", false, false),
            ("MCM Helper", "Required for the MCM to work.", false, false),
            ("X", "Needs version 2.0.1 or older of this", false, false),
            ("X", "Not optional.", false, false),
        ] {
            assert_eq!((waived(name, notes, ae), waived(name, notes, se)), (on_ae, on_se), "{name}: {notes}");
        }
        assert!(!super::waived(&super::Needed { mod_id: Some(1), name: "BEES".into(), notes: "older than 1.6.1130".into(), url: String::new(), dlc: false }, None), "no game version: keep it");
    }
}
