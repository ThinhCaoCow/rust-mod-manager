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
const RECHECK_SECS: u64 = 30 * 86400;
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

    /// GET a JSON endpoint (`path` starts with `/v1/`).
    pub fn get(&self, path: &str) -> Result<Value> {
        let url = format!("{}{path}", self.base);
        let version = env!("CARGO_PKG_VERSION");
        // The key goes to curl on stdin, never on its command line where other programs see it.
        let config = format!(
            "header = \"apikey: {}\"\nheader = \"Application-Name: rmm\"\nheader = \"Application-Version: {version}\"\nuser-agent = \"rmm/{version}\"\n",
            self.key
        );
        let mut child = Command::new("curl")
            .args(["-sS", "--max-time", "60", "-w", "\n%{http_code}", "-K", "-", "--", &url])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("cannot run curl")?;
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

#[derive(Serialize, Debug, PartialEq)]
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

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Mods with a newer file on Nexus. `ids` limits the check (default: every mod with Nexus ids).
pub fn check_updates(inst: &Instance, client: &Client, ids: Option<&[String]>) -> Result<Vec<Update>> {
    let domain = inst.config.game.nexus_domain();
    let cache_path = inst.root.join(CACHE);
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
    let mut out = Vec::new();
    for (i, (id, meta)) in mods.iter().enumerate() {
        crate::progress::report("checking Nexus for updates", i, mods.len());
        let n = meta.nexus.as_ref().unwrap();
        let checked = cache.checked.get(&n.mod_id).copied();
        let stale = match checked {
            None => true,
            Some(t) => recent.get(&n.mod_id).is_some_and(|&u| u >= t) || now().saturating_sub(t) > RECHECK_SECS,
        };
        if stale || !cache.files.contains_key(&n.mod_id) {
            let files = client.get(&format!("/v1/games/{domain}/mods/{}/files.json", n.mod_id))?;
            cache.files.insert(n.mod_id, files);
            cache.checked.insert(n.mod_id, now());
        }
        if let Some(u) = newer(id, &meta.name, n, &cache.files[&n.mod_id], domain) {
            out.push(u);
        }
    }
    fs::write(&cache_path, serde_json::to_string(&cache)?)?;
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
            let main = list.iter().filter(|f| f["category_name"] == "MAIN").max_by_key(|f| f["uploaded_timestamp"].as_u64().unwrap_or(0))?;
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
    let base = format!("/v1/games/{domain}/mods/{}/files/{}", link.mod_id, link.file_id);
    let info = client.get(&format!("{base}.json"))?;
    let query = match (&link.key, link.expires) {
        (Some(k), Some(e)) => format!("?key={k}&expires={e}"),
        _ => String::new(),
    };
    let links = client.get(&format!("{base}/download_link.json{query}"))?;
    let uri = links.as_array().and_then(|l| l.first()).and_then(|l| l["URI"].as_str()).context("Nexus sent no download link")?;
    let name = info["file_name"].as_str().map(crate::install::sanitize).unwrap_or_else(|| format!("{}-{}", link.mod_id, link.file_id));
    let dest = inst.root.join("downloads").join(&name);
    fs::create_dir_all(dest.parent().unwrap())?;
    crate::progress::report(&format!("downloading {name}"), 0, 0);
    fetch_url(uri, &dest)?;
    Ok((dest, info))
}

fn fetch_url(url: &str, dest: &Path) -> Result<()> {
    let mut tmp = dest.as_os_str().to_owned();
    tmp.push(".part");
    let out = Command::new("curl").args(["-fsSL", "--proto", "=http,https", "--proto-redir", "=http,https", "--retry", "2", "-o"]).arg(&tmp).args(["--", url]).output().context("cannot run curl")?;
    if !out.status.success() {
        let _ = fs::remove_file(&tmp);
        bail!("download failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    fs::rename(&tmp, dest)?;
    Ok(())
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
    let name = client.get(&format!("/v1/games/{domain}/mods/{}.json", link.mod_id)).ok().and_then(|m| m["name"].as_str().map(str::to_owned));
    let nexus = crate::instance::NexusRef { mod_id: link.mod_id, file_id: Some(link.file_id), version: info["version"].as_str().map(str::to_owned) };
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
    Ok(r)
}

/// A link for bare ids, which only Premium accounts can download without the website's button.
pub fn premium_link(inst: &Instance, mod_id: u64, file_id: u64) -> NxmLink {
    NxmLink { game: inst.config.game.nexus_domain().into(), mod_id, file_id, key: None, expires: None }
}

/// An nxm:// link, or `<mod id>/<file id>` (Premium accounts).
pub fn link_or_ids(inst: &Instance, what: &str) -> Result<NxmLink> {
    let what = what.trim();
    match what.split_once('/') {
        Some((m, f)) if !what.contains(':') => Ok(premium_link(inst, m.trim().parse()?, f.trim().parse()?)),
        _ => parse_nxm(what),
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

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
        let r = |file_id, version: &str| crate::instance::NexusRef { mod_id: 9, file_id, version: Some(version.into()) };
        let u = newer("A", "A", &r(Some(1), "1.0"), &files, "skyrimspecialedition").unwrap();
        assert_eq!((u.file_id, u.latest.as_deref(), u.url.as_str()), (3, Some("1.2"), "https://www.nexusmods.com/skyrimspecialedition/mods/9?tab=files&file_id=3"));
        assert!(newer("A", "A", &r(Some(3), "1.2"), &files, "x").is_none());
        assert_eq!(newer("A", "A", &r(None, "1.1"), &files, "x").unwrap().file_id, 3, "by version without a file id");
        assert!(newer("A", "A", &r(None, "1.2"), &files, "x").is_none());
    }
}
