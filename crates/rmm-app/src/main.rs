//! Desktop UI: a thin shell over rmm-core. The window is plain HTML/JS in `ui/`. Every action is
//! one `call` into the same core functions the CLI and MCP server use, so it lands in the same
//! undo history. The window polls `stamp` and reloads when another process (the CLI, an AI over
//! MCP) changes the instance.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use anyhow::{Context, Result, bail};
use rmm_core::{Game, Instance, conflicts, deploy, game, health, install, launch, loot, ops, plugins, rules};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The open instance folder.
struct Current(Mutex<Option<PathBuf>>);

#[tauri::command]
async fn call(current: tauri::State<'_, Current>, name: String, args: Value) -> Result<Value, String> {
    let root = current.0.lock().unwrap().clone();
    // Deploys, LOOT sorts and installs take a while; keep them off the UI thread.
    let (value, root) = tauri::async_runtime::spawn_blocking(move || run(root, &name, &args))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:#}"))?;
    *current.0.lock().unwrap() = root;
    Ok(value)
}

fn run(root: Option<PathBuf>, name: &str, a: &Value) -> Result<(Value, Option<PathBuf>)> {
    let s = |k: &str| a[k].as_str().map(str::to_owned).with_context(|| format!("missing '{k}'"));
    let flag = |k: &str| a[k].as_bool().unwrap_or(false);
    match name {
        "detect" => return Ok((json!(Game::ALL.iter().filter_map(|g| game::detect(*g)).collect::<Vec<_>>()), root)),
        "open" => {
            let inst = Instance::open(Path::new(&s("path")?))?;
            return Ok((ops::instance_info(&inst)?, Some(inst.root)));
        }
        "create" => {
            let g: Game = serde_json::from_value(a["game"].clone())?;
            let found = game::detect(g).context("game not found; create the instance with `rmm init --game-path`")?;
            let local = found.local_path.context("cannot work out where plugins.txt lives; use `rmm init --local-path`")?;
            let inst = Instance::create(Path::new(&s("path")?), g, &found.game_path, &local)?;
            return Ok((ops::instance_info(&inst)?, Some(inst.root)));
        }
        _ => {}
    }
    let root = root.context("no instance is open")?;
    let mut inst = Instance::open(&root)?;
    let p = inst.profile()?;
    let v = match name {
        "state" => json!({
            "info": ops::instance_info(&inst)?,
            "profiles": inst.profiles()?,
            "mods": ops::list_mods(&inst)?,
            "conflicts": conflicts::conflicts(&inst, &p)?,
            "plugins": plugins::list(&inst, &p)?,
            "rules": rules::rules(&inst)?,
            "broken": rules::broken(&inst, &p)?,
            "history": inst.history()?,
        }),
        "stamp" => json!(stamp(&inst)),
        "plan" => json!(deploy::deploy(&inst, true)?),
        "deploy" => json!(deploy::deploy(&inst, false)?),
        "launch" => json!(launch::launch(&inst, false)?),
        "health" => json!(health::check(&inst)?),
        "details" => ops::mod_details(&inst, &s("id")?, 300)?,
        // FOMOD installers take their defaults here; choosing options needs the CLI or MCP for now.
        "install" => json!(install::install(&inst, Path::new(&s("path")?), None, true, Some(&Default::default()))?),
        "uninstall" => json!(install::uninstall(&inst, &s("id")?)?),
        "enable_mod" => json!(ops::set_mod_enabled(&inst, &s("id")?, flag("enabled"))?),
        "move_mod" => json!(ops::move_mod(&inst, &s("id")?, a["priority"].as_u64().context("missing 'priority'")? as usize)?),
        "enable_plugin" => json!(ops::set_plugin_enabled(&inst, &s("name")?, flag("enabled"))?),
        "plugin_order" => json!(ops::set_plugin_order(&inst, &serde_json::from_value::<Vec<String>>(a["names"].clone())?)?),
        "set_rule" => json!(rules::set_rule(&inst, &serde_json::from_value(a.clone())?)?),
        "sort_mods" => json!(rules::sort(&inst, flag("dry_run"))?),
        "sort_plugins" => json!(loot::sort(&inst, flag("dry_run"))?),
        "undo" => json!(inst.undo()?),
        "redo" => json!(inst.redo()?),
        "create_profile" => json!(inst.create_profile(&s("name")?, flag("copy"))?),
        "switch_profile" => json!(inst.switch_profile(&s("name")?)?),
        "rename_profile" => json!(inst.rename_profile(&s("from")?, &s("to")?)?),
        "delete_profile" => json!(inst.delete_profile(&s("name")?)?),
        other => bail!("unknown call '{other}'"),
    };
    Ok((v, Some(root)))
}

/// Changes when the instance does: the files every change writes, plus the mods folder.
fn stamp(inst: &Instance) -> String {
    let r = &inst.root;
    let files = [
        r.join("instance.toml"),
        r.join("profiles").join(&inst.config.profile).join("profile.toml"),
        r.join("rules.toml"),
        r.join("deploy.json"),
        r.join("mods"),
    ];
    format!("{:?}", files.map(|f| fs::metadata(f).and_then(|m| m.modified()).ok()))
}

fn main() {
    // `rmm-app --instance <dir>`, then $RMM_INSTANCE; otherwise the window reopens the last one.
    let args: Vec<String> = std::env::args().collect();
    let start = args
        .iter()
        .position(|a| a == "--instance" || a == "-i")
        .and_then(|i| args.get(i + 1).cloned())
        .or_else(|| std::env::var("RMM_INSTANCE").ok())
        .map(PathBuf::from);
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(Current(Mutex::new(start)))
        .invoke_handler(tauri::generate_handler![call])
        .run(tauri::generate_context!())
        .expect("error while running rmm");
}
