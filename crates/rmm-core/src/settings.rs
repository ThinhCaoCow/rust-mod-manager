//! The user's settings for rmm's features, in `settings.json` in rmm's config folder, shared by the
//! CLI, the MCP server and the window. Every field has a default, which is how rmm worked before
//! it had settings; a missing or damaged file means all defaults. The window's own look (theme,
//! density, editor) is kept by the window.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Settings {
    pub deploy: Deploy,
    pub launch: Launch,
    pub install: Install,
    pub history: History,
    pub loot: Loot,
    pub health: Health,
    pub test: Test,
    pub nexus: Nexus,
    pub tools: Tools,
    pub ai: Ai,
    pub mcp: Mcp,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Deploy {
    /// "link": hard links, copies where a link can't be made (another drive); "copy": copies only,
    /// so nothing the game does to a deployed file reaches the mod.
    pub method: String,
    /// Files never deployed, by their path inside Data: `*` stands for any run of characters, and a
    /// pattern without `/` matches the file's name (`*.txt`, `readme*`, `fomod/*`).
    pub exclude: Vec<String>,
}
impl Default for Deploy {
    fn default() -> Self {
        Deploy { method: "link".into(), exclude: vec![] }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Launch {
    /// Start the game through its script extender's loader when it's installed.
    pub script_extender: bool,
    /// Passed to the game (or its loader); quotes keep spaces in one argument.
    pub args: String,
}
impl Default for Launch {
    fn default() -> Self {
        Launch { script_extender: true, args: String::new() }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Install {
    /// Mods installed in the window start enabled (the CLI's `--disabled` stays per install).
    pub enable: bool,
}
impl Default for Install {
    fn default() -> Self {
        Install { enable: true }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct History {
    /// Undo steps kept per profile.
    pub limit: usize,
}
impl Default for History {
    fn default() -> Self {
        History { limit: 200 }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Loot {
    /// How old the masterlist may get before a sort downloads it again.
    pub refresh_hours: u64,
    /// LOOT's messages in this language where the masterlist has it (en, de, fr, es, ru, ja, zh...).
    pub language: String,
    /// Show LOOT's notes too, not only its warnings and errors.
    pub show_info: bool,
    /// Place the plugins a tool run adds where LOOT would (once LOOT is in use); the rest keep
    /// their order.
    pub sort_after_tools: bool,
}
impl Default for Loot {
    fn default() -> Self {
        Loot { refresh_hours: 24, language: "en".into(), show_info: false, sort_after_tools: true }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Health {
    /// Kinds of problems not reported (see `health::KINDS`).
    pub hidden: Vec<String>,
    /// Fallout 4's limit on general BA2 archives: 0 works it out (1,024 since the August 2025
    /// update, with a warning above 256), or 256 or 1024.
    pub ba2_limit: usize,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Test {
    /// The game's window in test runs: "<width>x<height>" or "fullscreen".
    pub window: String,
    /// Seconds in each cell.
    pub dwell: u64,
    /// Seconds to wait for the main menu or a cell to load.
    pub timeout: u64,
    /// Take screenshots (the window's default for a test run).
    pub screenshots: bool,
    /// Test runs whose screenshots are kept in `test-runs/`; 0 keeps all.
    pub keep_runs: usize,
}
impl Default for Test {
    fn default() -> Self {
        Test { window: "960x540".into(), dwell: 10, timeout: 180, screenshots: true, keep_runs: 0 }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Nexus {
    /// A mod not in Nexus's recently-updated list is asked about again after this many days.
    pub recheck_days: u64,
    /// The window checks for updates when it opens (signed in only).
    pub check_on_start: bool,
}
impl Default for Nexus {
    fn default() -> Self {
        Nexus { recheck_days: 30, check_on_start: false }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Tools {
    /// Minutes a tool run from the window may take before it is stopped; 0: no limit.
    pub timeout_mins: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Ai {
    /// Model calls in one turn before rmm's AI pauses and asks to go on.
    pub max_steps: usize,
    /// US dollars a chat may cost before it stops (Claude and OpenRouter report costs); 0: no limit.
    /// A few dollars by default, so a first try can't run up a bill.
    pub budget: f64,
    /// Tools rmm's AI may run without asking, on top of reads and dry runs.
    pub auto_allow: Vec<String>,
    /// The user's instructions for each mod team role (lead, researcher, installer, tester),
    /// added to its prompt for rmm's AI and for AI apps over MCP.
    pub role_instructions: BTreeMap<String, String>,
}
impl Default for Ai {
    fn default() -> Self {
        Ai { max_steps: 60, budget: 2.0, auto_allow: vec![], role_instructions: BTreeMap::new() }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Mcp {
    /// The port of the window's AI access (on 127.0.0.1).
    pub port: u16,
    /// Keep each AI app's session as a chat in the instance.
    pub record_sessions: bool,
}
impl Default for Mcp {
    fn default() -> Self {
        Mcp { port: 47823, record_sessions: true }
    }
}

const ROLES: &[&str] = &["lead", "researcher", "installer", "tester"];

/// The settings now; defaults for anything not set.
pub fn get() -> Settings {
    crate::config_file("settings.json").and_then(|f| std::fs::read_to_string(f).ok()).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

impl Settings {
    /// Refuse values rmm can't work with.
    pub fn check(&self) -> Result<()> {
        if !matches!(self.deploy.method.as_str(), "link" | "copy") {
            bail!("deploy.method is link or copy");
        }
        if let Some(p) = self.deploy.exclude.iter().find(|p| p.trim().is_empty() || p.contains(['\\', ':'])) {
            bail!("deploy.exclude: {p:?} isn't a pattern like *.txt or docs/* (use /)");
        }
        if !(1..=10_000).contains(&self.history.limit) {
            bail!("history.limit is 1 to 10000");
        }
        if !(1..=24 * 365).contains(&self.loot.refresh_hours) {
            bail!("loot.refresh_hours is 1 to 8760");
        }
        if self.loot.language.trim().is_empty() {
            bail!("loot.language is a language code such as en");
        }
        if ![0, 256, 1024].contains(&self.health.ba2_limit) {
            bail!("health.ba2_limit is 0 (work it out), 256 or 1024");
        }
        let w = self.test.window.trim();
        if w != "fullscreen" && !w.split_once('x').is_some_and(|(a, b)| a.parse::<u32>().is_ok_and(|a| a >= 320) && b.parse::<u32>().is_ok_and(|b| b >= 200)) {
            bail!("test.window is fullscreen or a size such as 960x540");
        }
        if !(1..=600).contains(&self.test.dwell) {
            bail!("test.dwell is 1 to 600 seconds");
        }
        if !(30..=3600).contains(&self.test.timeout) {
            bail!("test.timeout is 30 to 3600 seconds");
        }
        if !(1..=365).contains(&self.nexus.recheck_days) {
            bail!("nexus.recheck_days is 1 to 365");
        }
        if !(1..=500).contains(&self.ai.max_steps) {
            bail!("ai.max_steps is 1 to 500");
        }
        if !self.ai.budget.is_finite() || self.ai.budget < 0.0 {
            bail!("ai.budget is 0 (no limit) or more dollars");
        }
        if let Some(r) = self.ai.role_instructions.keys().find(|r| !ROLES.contains(&r.as_str())) {
            bail!("ai.role_instructions: no role {r:?}; the roles are {}", ROLES.join(", "));
        }
        if self.mcp.port < 1024 {
            bail!("mcp.port is 1024 to 65535");
        }
        Ok(())
    }
}

/// Save the settings, after checking them.
pub fn set(s: &Settings) -> Result<()> {
    s.check()?;
    let f = crate::config_file("settings.json").context("no config folder to keep the settings in")?;
    std::fs::create_dir_all(f.parent().unwrap_or(&f))?;
    std::fs::write(f, serde_json::to_string_pretty(s)?)?;
    Ok(())
}

/// Change one setting by its dotted name (`deploy.method`), from text: a string setting takes the
/// text as it is, a list takes JSON or comma-separated items, anything else JSON (`true`, `30`).
pub fn set_one(key: &str, value: &str) -> Result<Settings> {
    let mut v = serde_json::to_value(get())?;
    let slot = key.split('.').try_fold(&mut v, |v, k| v.get_mut(k)).with_context(|| format!("no setting {key}; `rmm settings` lists them"))?;
    *slot = match slot {
        Value::String(_) => Value::String(value.into()),
        Value::Array(_) => serde_json::from_str(value)
            .unwrap_or_else(|_| Value::Array(value.split(',').map(str::trim).filter(|x| !x.is_empty()).map(|x| Value::String(x.into())).collect())),
        _ => serde_json::from_str(value).with_context(|| format!("{key} can't be {value}"))?,
    };
    let s: Settings = serde_json::from_value(v).with_context(|| format!("{key} can't be {value}"))?;
    set(&s)?;
    Ok(s)
}

/// Put one setting (or, without a name, all of them) back to its default.
pub fn reset(key: Option<&str>) -> Result<Settings> {
    let Some(key) = key else {
        set(&Settings::default())?;
        return Ok(Settings::default());
    };
    let default = serde_json::to_value(Settings::default())?;
    let want = key.split('.').try_fold(&default, |v, k| v.get(k)).with_context(|| format!("no setting {key}"))?.clone();
    let mut v = serde_json::to_value(get())?;
    *key.split('.').try_fold(&mut v, |v, k| v.get_mut(k)).context("no such setting")? = want;
    let s: Settings = serde_json::from_value(v)?;
    set(&s)?;
    Ok(s)
}

/// Whether a file (its path inside Data, with `/`) matches one of `patterns`.
pub fn excluded(patterns: &[String], rel: &str) -> bool {
    let rel = rel.to_ascii_lowercase();
    let name = rel.rsplit('/').next().unwrap_or(&rel);
    patterns.iter().any(|p| {
        let p = p.trim().to_ascii_lowercase();
        glob(p.as_bytes(), if p.contains('/') { rel.as_bytes() } else { name.as_bytes() })
    })
}

/// `*` matches any run of bytes; everything else itself.
fn glob(p: &[u8], s: &[u8]) -> bool {
    let (mut pi, mut si, mut star, mut mark) = (0, 0, None, 0);
    while si < s.len() {
        if pi < p.len() && p[pi] == b'*' {
            (star, mark) = (Some(pi), si);
            pi += 1;
        } else if pi < p.len() && p[pi] == s[si] {
            pi += 1;
            si += 1;
        } else if let Some(st) = star {
            pi = st + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == b'*')
}

/// Split launch arguments on spaces, keeping "quoted parts" together.
pub fn split_args(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut quoted) = (Vec::new(), String::new(), false);
    for c in s.chars() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_and_args() {
        let p = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(excluded(&p(&["*.txt"]), "docs/ReadMe.TXT"));
        assert!(excluded(&p(&["readme*"]), "readme - snow.pdf"));
        assert!(excluded(&p(&["fomod/*"]), "fomod/info.xml"));
        assert!(!excluded(&p(&["fomod/*"]), "meshes/fomod.nif"));
        assert!(!excluded(&p(&["*.txt"]), "scripts/a.pex"));
        assert!(excluded(&p(&["a*b*c"]), "axxbyyc") && !excluded(&p(&["a*b*c"]), "axxbyy"));
        assert_eq!(split_args(r#"-a "b c"  d"#), ["-a", "b c", "d"]);
    }

    #[test]
    fn checks() {
        let mut s = Settings::default();
        assert!(s.check().is_ok());
        s.deploy.method = "symlink".into();
        assert!(s.check().is_err());
        s = Settings::default();
        s.test.window = "12x".into();
        assert!(s.check().is_err());
        s = Settings::default();
        s.ai.role_instructions.insert("cook".into(), "x".into());
        assert!(s.check().is_err());
    }
}
