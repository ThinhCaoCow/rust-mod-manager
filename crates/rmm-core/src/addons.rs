//! Add-ons (SPEC.md "Add-ons"): Extism WebAssembly plugins, each a folder in `<instance>/addons/`
//! with `addon.toml` and `addon.wasm`. They run sandboxed, with no files, processes or network of
//! their own, and reach rmm through one host function, `rmm` (JSON in, `{"ok": …}` or
//! `{"error": …}` out), which allows only what addon.toml's capabilities ask for:
//! - `read_instance`: `info`, `list_mods`, `list_plugins`, `list_conflicts`, `mod_files {id}`
//! - `modify_profile`: `set_mods_enabled {ids, enabled}`, `move_mods {ids, priority}`,
//!   `set_plugin_enabled {name, enabled}` (undoable like any change)
//! - `mod_files`: `read_file {id, path}` (base64), `write_overwrite {path, base64 | text}`
//! - `run_tools`: `run_tool {id, dry_run}` (a dry run unless dry_run is false)
//! - `network`: Extism's `http_request`, to the hosts addon.toml lists
//!
//! Hooks are exported functions named after the moment they run, given JSON: `after_install`
//! `{id, name}`, `before_deploy` `{}` (return `{"abort": "why"}` to stop it), `after_deploy`
//! `{linked, removed}`, `after_tool_run` `{tool, mod}`, `health_check` `{}` (return
//! `[{"severity", "subject", "message"}]`). Hooks run outside the instance lock, and not while an
//! add-on is running, so an add-on's own changes can't loop. Contributions: `tools` (functions AI
//! clients and the window can call, with an input schema) and `panels` (a function whose
//! `{"text"}` or `{"columns", "rows"}` the window shows).
use crate::{Instance, conflicts, ops, plugins, tools};
use anyhow::{Context, Result, anyhow, bail};
use base64::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::cell::Cell;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

pub const DIR: &str = "addons";

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    ReadInstance,
    ModifyProfile,
    ModFiles,
    RunTools,
    Network,
}

#[derive(Serialize, Deserialize, Clone, Debug, schemars::JsonSchema)]
pub struct AddonTool {
    /// The exported function.
    pub name: String,
    pub description: String,
    /// JSON schema of its input.
    #[serde(default)]
    pub input: Option<Value>,
}

#[derive(Serialize, Deserialize, Clone, Debug, schemars::JsonSchema)]
pub struct Panel {
    pub title: String,
    /// The exported function that returns the panel's content.
    pub function: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, schemars::JsonSchema)]
pub struct AddonToml {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    /// Hosts the `network` capability may reach (`*.nexusmods.com` style patterns).
    #[serde(default)]
    pub hosts: Vec<String>,
    /// Exported functions run at those moments (see the module docs).
    #[serde(default)]
    pub hooks: Vec<String>,
    #[serde(default)]
    pub tools: Vec<AddonTool>,
    #[serde(default)]
    pub panels: Vec<Panel>,
    /// Seconds a call may take (default 60).
    #[serde(default)]
    pub timeout: Option<u64>,
    /// Games this add-on adds (see "Add-ons" in SPEC.md). An add-on with only games needs no wasm.
    #[serde(default)]
    pub games: Vec<crate::game::GameDef>,
}

#[derive(Serialize, Debug)]
pub struct Addon {
    #[serde(flatten)]
    pub toml: AddonToml,
    pub dir: PathBuf,
}

pub const HOOKS: [&str; 5] = ["after_install", "before_deploy", "after_deploy", "after_tool_run", "health_check"];

thread_local! {
    /// True while an add-on runs on this thread: its own changes don't run hooks again.
    static IN_ADDON: Cell<bool> = const { Cell::new(false) };
}

fn read_toml(dir: &Path) -> Result<AddonToml> {
    let t: AddonToml = toml::from_str(&fs::read_to_string(dir.join("addon.toml")).with_context(|| format!("no addon.toml in {}", dir.display()))?)
        .context("addon.toml")?;
    if t.name.trim().is_empty() {
        bail!("addon.toml has no name");
    }
    if let Some(h) = t.hooks.iter().find(|h| !HOOKS.contains(&h.as_str())) {
        bail!("unknown hook '{h}' (hooks: {})", HOOKS.join(", "));
    }
    if !t.hosts.is_empty() && !t.capabilities.contains(&Capability::Network) {
        bail!("addon.toml lists hosts but doesn't ask for the network capability");
    }
    for g in &t.games {
        crate::game::register(g.clone()).map_err(|e| anyhow!(e))?;
    }
    Ok(t)
}

/// Make the games defined by the add-ons in an instance folder known, before its instance.toml
/// (which may name one of them) is read. A broken add-on is skipped here; `list` reports it.
pub fn register_games(root: &Path) {
    if let Ok(rd) = fs::read_dir(root.join(DIR)) {
        for e in rd.flatten() {
            let _ = read_toml(&e.path());
        }
    }
}

/// The games an add-on folder defines, made known to this process.
pub fn games_in(dir: &Path) -> Result<Vec<crate::Game>> {
    read_toml(dir)?.games.into_iter().map(|g| crate::game::register(g).map_err(|e| anyhow!(e))).collect()
}

/// Add-ons that only define games have no wasm.
fn needs_wasm(t: &AddonToml) -> bool {
    !(!t.games.is_empty() && t.tools.is_empty() && t.hooks.is_empty() && t.panels.is_empty())
}

/// Installed add-ons, by name.
pub fn list(inst: &Instance) -> Result<Vec<Addon>> {
    let Ok(rd) = fs::read_dir(inst.root.join(DIR)) else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for e in rd.flatten().filter(|e| e.path().join("addon.toml").is_file()) {
        out.push(Addon { toml: read_toml(&e.path())?, dir: e.path() });
    }
    out.sort_by(|a, b| a.toml.name.cmp(&b.toml.name));
    Ok(out)
}

fn find(inst: &Instance, name: &str) -> Result<Addon> {
    list(inst)?.into_iter().find(|a| a.toml.name == name).with_context(|| format!("no add-on '{name}'"))
}

/// Copy an add-on folder into the instance. A dry run checks it (it must load) and shows what it
/// asks for, for the user to approve.
pub fn install(inst: &Instance, src: &Path, dry_run: bool) -> Result<Addon> {
    let toml = read_toml(src)?;
    if !src.join("addon.wasm").is_file() {
        if needs_wasm(&toml) {
            bail!("no addon.wasm in {}", src.display());
        }
    } else {
        plugin(inst, &Addon { toml: toml.clone(), dir: src.to_path_buf() })?;
    }
    let dir = inst.root.join(DIR).join(crate::install::sanitize(&toml.name));
    if !dry_run {
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        fs::create_dir_all(&dir)?;
        for e in fs::read_dir(src)?.flatten().filter(|e| e.path().is_file()) {
            fs::copy(e.path(), dir.join(e.file_name()))?;
        }
    }
    Ok(Addon { toml, dir })
}

pub fn remove(inst: &Instance, name: &str) -> Result<()> {
    Ok(fs::remove_dir_all(find(inst, name)?.dir)?)
}

struct Ctx {
    root: PathBuf,
    caps: Vec<Capability>,
}

extism::host_fn!(rmm_host(ctx: Ctx; input: String) -> String {
    let ctx = ctx.get()?;
    let ctx = ctx.lock().unwrap();
    Ok(match host_op(&ctx, &input) {
        Ok(v) => json!({ "ok": v }),
        Err(e) => json!({ "error": format!("{e:#}") }),
    }
    .to_string())
});

/// A path inside a mod or overwrite/: relative, without `..`.
fn inside(rel: &str) -> Result<PathBuf> {
    let p = PathBuf::from(rel.replace('\\', "/"));
    if rel.is_empty() || !p.components().all(|c| matches!(c, Component::Normal(_))) {
        bail!("'{rel}' must be a relative path without '..'");
    }
    Ok(p)
}

fn host_op(ctx: &Ctx, input: &str) -> Result<Value> {
    let a: Value = serde_json::from_str(input).context("the input is not JSON")?;
    let op = a["op"].as_str().context("missing 'op'")?;
    let need = |c: Capability| {
        let name = serde_json::to_value(c).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
        if ctx.caps.contains(&c) { Ok(()) } else { Err(anyhow!("'{op}' needs the {name} capability in addon.toml")) }
    };
    let s = |k: &str| a[k].as_str().with_context(|| format!("missing '{k}'"));
    let b = |k: &str| a[k].as_bool().with_context(|| format!("missing '{k}'"));
    let ids = || serde_json::from_value::<Vec<String>>(a["ids"].clone()).context("missing 'ids'");
    let inst = Instance::open(&ctx.root)?;
    Ok(match op {
        "info" => {
            need(Capability::ReadInstance)?;
            ops::instance_info(&inst)?
        }
        "list_mods" => {
            need(Capability::ReadInstance)?;
            json!(ops::list_mods(&inst)?)
        }
        "list_plugins" => {
            need(Capability::ReadInstance)?;
            json!(plugins::list(&inst, &inst.profile()?)?)
        }
        "list_conflicts" => {
            need(Capability::ReadInstance)?;
            json!(conflicts::conflicts(&inst, &inst.profile()?)?)
        }
        "mod_files" => {
            need(Capability::ReadInstance)?;
            json!(inst.mod_files(inside(s("id")?)?.to_str().context("bad id")?)?)
        }
        "set_mods_enabled" => {
            need(Capability::ModifyProfile)?;
            ops::set_mods_enabled(&inst, &ids()?, b("enabled")?)?;
            Value::Null
        }
        "move_mods" => {
            need(Capability::ModifyProfile)?;
            ops::move_mods(&inst, &ids()?, a["priority"].as_u64().context("missing 'priority'")? as usize)?;
            Value::Null
        }
        "set_plugin_enabled" => {
            need(Capability::ModifyProfile)?;
            ops::set_plugin_enabled(&inst, s("name")?, b("enabled")?)?;
            Value::Null
        }
        "read_file" => {
            need(Capability::ModFiles)?;
            let id = inside(s("id")?)?;
            let path = inst.mod_dir(id.to_str().context("bad id")?).join(inside(s("path")?)?);
            json!(BASE64_STANDARD.encode(fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?))
        }
        "write_overwrite" => {
            need(Capability::ModFiles)?;
            let data = match (a["base64"].as_str(), a["text"].as_str()) {
                (Some(b), _) => BASE64_STANDARD.decode(b).context("bad base64")?,
                (None, Some(t)) => t.as_bytes().to_vec(),
                _ => bail!("give 'base64' or 'text'"),
            };
            let rel = inside(s("path")?)?;
            // Code the game or Windows would run is out of an add-on's reach: no files beside the
            // game's exe (root/), no programs or plugins.
            let ext = rel.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
            if rel.components().next().is_some_and(|c| c.as_os_str().eq_ignore_ascii_case("root"))
                || ["dll", "exe", "asi", "bat", "cmd", "ps1", "vbs", "js", "com", "scr", "msi", "lnk"].contains(&ext.as_str())
            {
                bail!("add-ons can't write '{}': no root/ files or programs", rel.display());
            }
            let path = inst.mod_dir(crate::instance::OVERWRITE).join(rel);
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(path, data)?;
            Value::Null
        }
        "run_tool" => {
            need(Capability::RunTools)?;
            json!(tools::run(&inst, s("id")?, &Default::default(), a["dry_run"].as_bool().unwrap_or(true), None)?)
        }
        _ => bail!("unknown op '{op}'"),
    })
}

fn plugin(inst: &Instance, a: &Addon) -> Result<extism::Plugin> {
    let mut m = extism::Manifest::new([extism::Wasm::file(a.dir.join("addon.wasm"))])
        .with_timeout(Duration::from_secs(a.toml.timeout.unwrap_or(60)))
        .with_memory_max(4096); // 256 MiB
    if a.toml.capabilities.contains(&Capability::Network) {
        m = m.with_allowed_hosts(a.toml.hosts.iter().cloned());
    }
    let ctx = extism::UserData::new(Ctx { root: inst.root.clone(), caps: a.toml.capabilities.clone() });
    extism::PluginBuilder::new(m)
        .with_wasi(false)
        .with_function("rmm", [extism::PTR], [extism::PTR], ctx, rmm_host)
        .build()
        .with_context(|| format!("cannot load add-on '{}'", a.toml.name))
}

fn run(inst: &Instance, a: &Addon, function: &str, input: &Value) -> Result<Value> {
    let mut p = plugin(inst, a)?;
    if !p.function_exists(function) {
        bail!("add-on '{}' has no function '{function}'", a.toml.name);
    }
    IN_ADDON.set(true);
    let out = p.call::<&str, &str>(function, &input.to_string()).map(str::to_owned);
    IN_ADDON.set(false);
    let out = out.map_err(|e| anyhow!("add-on '{}', {function}: {e:#}", a.toml.name))?;
    Ok(if out.trim().is_empty() { Value::Null } else { serde_json::from_str(&out).unwrap_or(Value::String(out)) })
}

/// Call one of an add-on's functions: a contributed tool or panel.
pub fn call(inst: &Instance, name: &str, function: &str, input: &Value) -> Result<Value> {
    let a = find(inst, name)?;
    if !a.toml.tools.iter().any(|t| t.name == function) && !a.toml.panels.iter().any(|p| p.function == function) {
        bail!("'{function}' isn't one of add-on '{name}''s tools or panels");
    }
    run(inst, &a, function, input)
}

/// Run a hook in every add-on that has it: (add-on, its answer).
pub fn hook(inst: &Instance, name: &str, input: &Value) -> Vec<(String, Result<Value>)> {
    if IN_ADDON.get() {
        return Vec::new();
    }
    let addons = match list(inst) {
        Ok(a) => a,
        Err(e) => return vec![("addons".into(), Err(e))],
    };
    addons.iter().filter(|a| a.toml.hooks.iter().any(|h| h == name)).map(|a| (a.toml.name.clone(), run(inst, a, name, input))).collect()
}

/// Hook failures as warnings.
pub fn warnings(results: &[(String, Result<Value>)]) -> Vec<String> {
    results.iter().filter_map(|(a, r)| r.as_ref().err().map(|e| format!("add-on {a}: {e:#}"))).collect()
}
