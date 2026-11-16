//! Desktop UI: a thin shell over rmm-core. The window is plain HTML/JS in `ui/`. Every action is
//! one `call` into the same core functions the CLI and MCP server use, so it lands in the same
//! undo history. The window polls `stamp` and reloads when another process (the CLI, an AI over
//! MCP) changes the instance.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use anyhow::{Context, Result, bail};
use base64::prelude::*;
use rmm_core::instance::OVERWRITE;
use rmm_core::{Game, Instance, addons, conflicts, deploy, game, harness, health, import, install, launch, lockfile, logs, loot, nexus, ops, plugins, rules, tools};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::Emitter;

/// The open instance folder.
struct Current(Mutex<Option<PathBuf>>);

/// The MCP server the window hosts for AI clients, when switched on.
struct Mcp(Mutex<Option<rmm::http::Http>>);

/// A fixed port, so AI clients' settings keep working.
const MCP_ADDR: &str = "127.0.0.1:47823";

/// Switch the HTTP MCP server on (again, for the open instance) or off.
#[tauri::command]
async fn mcp(current: tauri::State<'_, Current>, server: tauri::State<'_, Mcp>, on: bool) -> Result<Value, String> {
    let root = current.0.lock().unwrap().clone();
    let mut slot = server.0.lock().unwrap();
    *slot = None; // stops a running one, freeing the port
    if !on {
        return Ok(json!({ "on": false }));
    }
    let root = root.ok_or("no instance is open")?;
    let started = rmm::http::token().and_then(|token| Ok((rmm::http::start(root, MCP_ADDR, token.clone())?, token)));
    let (http, token) = started.map_err(|e| format!("{e:#}"))?;
    let url = format!("http://{}/mcp", http.addr);
    *slot = Some(http);
    Ok(json!({ "on": true, "url": url, "token": token }))
}

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
    let n = |k: &str| a[k].as_u64().map(|n| n as usize).with_context(|| format!("missing '{k}'"));
    let ids = || serde_json::from_value::<Vec<String>>(a["ids"].clone()).context("missing 'ids'");
    let choices = || -> Result<Option<rmm_core::fomod::Choices>> {
        Ok(a.get("choices").filter(|v| !v.is_null()).map(|v| serde_json::from_value(v.clone())).transpose()?)
    };
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
        // The three slow parts each read every mod, so they run side by side.
        "state" => std::thread::scope(|s| -> Result<Value> {
            let mods = s.spawn(|| ops::list_mods(&inst));
            let conflicts = s.spawn(|| conflicts::conflicts(&inst, &p));
            let plugins = s.spawn(|| plugins::list(&inst, &p));
            let mods = mods.join().unwrap()?;
            // Conflict lists name mods by priority instead of id, a fraction of the bytes on a
            // long list; the window maps them back.
            let at: std::collections::HashMap<&str, usize> = mods.iter().map(|m| (m.id.as_str(), m.priority)).collect();
            let mut conflicts = serde_json::to_value(conflicts.join().unwrap()?)?;
            for c in conflicts.as_array_mut().into_iter().flatten() {
                for k in ["overwrites", "overwritten_by", "archive_overwrites", "archive_overwritten_by"] {
                    for v in c.get_mut(k).and_then(Value::as_array_mut).into_iter().flatten() {
                        if let Some(&i) = v.as_str().and_then(|id| at.get(id)) {
                            *v = json!(i);
                        }
                    }
                }
            }
            Ok(json!({
                "info": ops::instance_info(&inst)?,
                "profiles": inst.profiles()?,
                "rules": rules::rules(&inst)?,
                "broken": rules::broken(&inst, &p)?,
                "unmet": rules::unmet(&inst, &p)?,
                "locks": p.locks.keys().collect::<Vec<_>>(),
                "options": { "local_inis": p.local_inis, "local_saves": p.local_saves },
                "history": inst.history()?,
                "mods": mods,
                "conflicts": conflicts,
                "plugins": plugins.join().unwrap()?,
                "overwrite": inst.mod_files(OVERWRITE)?.len(),
            }))
        })?,
        "stamp" => json!(stamp(&inst)),
        "plan" => json!(deploy::deploy(&inst, true)?),
        "deploy" => json!(deploy::deploy(&inst, false)?),
        "purge" => json!(deploy::purge(&inst, flag("dry_run"))?),
        "launch" => json!(launch::launch(&inst, false)?),
        "test_run" => json!(harness::test_run(&inst, &serde_json::from_value(a.clone())?)?),
        // A test run's screenshot as a data: URL; only from the instance's test-runs folder.
        "screenshot" => {
            let file = fs::canonicalize(s("path")?)?;
            if !file.starts_with(fs::canonicalize(inst.root.join("test-runs"))?) {
                bail!("not a test-run screenshot");
            }
            json!(format!("data:image/png;base64,{}", BASE64_STANDARD.encode(fs::read(file)?)))
        }
        "health" => json!(health::check(&inst)?),
        "details" => ops::mod_details(&inst, &s("id")?, 300)?,
        // A mod's FOMOD installer as it plays out with the window's choices; null when it has none.
        "fomod" => {
            let path = s("path")?;
            match install::has_fomod(&inst, Path::new(&path))? {
                true => json!(install::fomod_options(&inst, Path::new(&path), choices()?.as_ref())?),
                false => Value::Null,
            }
        }
        // An option's picture, as a data: URL the window can show.
        "fomod_image" => {
            let file = install::fomod_image(&inst, Path::new(&s("path")?), &s("image")?)?;
            let ext = file.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
            let mime = match ext.as_str() {
                "jpg" | "jpeg" => "image/jpeg",
                "gif" => "image/gif",
                "bmp" => "image/bmp",
                "webp" => "image/webp",
                _ => "image/png",
            };
            json!(format!("data:{mime};base64,{}", BASE64_STANDARD.encode(fs::read(file)?)))
        }
        // Without choices a FOMOD installer takes its defaults; the name defaults to the file's.
        "install" => {
            let name = a["name"].as_str().map(str::trim).filter(|n| !n.is_empty());
            let at = a["priority"].as_u64().map(|n| n as usize);
            let r = install::install_at(&inst, Path::new(&s("path")?), name, true, Some(&choices()?.unwrap_or_default()), at)?;
            if let Ok(n) = serde_json::from_value(a["nexus"].clone()) {
                nexus::remember(&inst, &r.id, n)?;
            }
            json!(r)
        }
        "reinstall" => json!(install::reinstall(&inst, &s("id")?, choices()?.as_ref())?),
        "rename_mod" => json!(ops::rename_mod(&inst, &s("id")?, &s("name")?)?),
        "overwrite_files" => json!(inst.mod_files(OVERWRITE)?),
        "mod_from_overwrite" => json!(install::mod_from_overwrite(&inst, &s("name")?)?),
        "clear_overwrite" => json!(install::clear_overwrite(&inst)?),
        "tools" => {
            // With each tool's last run, so the list can say how it went.
            let last = |id: &str| fs::read_to_string(inst.root.join("tools").join(id).join("last-run.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());
            let list: Vec<Value> = tools::list(&inst)?.into_iter().map(|(id, t)| json!({ "id": id, "last": last(&id), "tool": t })).collect();
            json!({ "tools": list, "presets": tools::PRESETS })
        }
        "add_tool" => json!(tools::add(&inst, &s("id")?, serde_json::from_value(a["spec"].clone())?)?),
        "remove_tool" => json!(tools::remove(&inst, &s("id")?)?),
        "run_tool" => {
            let vars = serde_json::from_value(a.get("vars").cloned().unwrap_or(json!({})))?;
            json!(tools::run(&inst, &s("id")?, &vars, flag("dry_run"), None)?)
        }
        "loot_rules" => json!({ "rules": loot::rules(&inst)?, "groups": loot::groups(&inst)? }),
        "set_loot_rule" => json!(loot::set_rule(&inst, &serde_json::from_value(a.clone())?)?),
        "uninstall" => json!(install::uninstall_mods(&inst, &ids()?)?),
        "enable_mods" => json!(ops::set_mods_enabled(&inst, &ids()?, flag("enabled"))?),
        "move_mods" => json!(ops::move_mods(&inst, &ids()?, a["priority"].as_u64().context("missing 'priority'")? as usize)?),
        "add_separator" => json!(ops::add_separator(&inst, &s("name")?, a["priority"].as_u64().map(|n| n as usize))?),
        "edit_separator" => json!(ops::edit_separator(&inst, &s("id")?, a["name"].as_str(), a["color"].as_str())?),
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
        "set_profile_options" => json!(ops::set_profile_options(&inst, a["local_inis"].as_bool(), a["local_saves"].as_bool())?),
        "delete_profile" => json!(inst.delete_profile(&s("name")?)?),
        "crash_log" => json!(logs::read_crash_log(&inst, a["path"].as_str().map(Path::new))?),
        "papyrus_log" => json!(logs::read_papyrus_log(&inst, 60)?),
        "nexus_status" => nexus::status(),
        "nexus_set_key" => nexus::set_key(&s("key")?)?,
        "nexus_sso" => nexus::sso(&nexus::open_in_browser)?,
        "nexus_forget_key" => json!(nexus::forget_key()?),
        "check_updates" => json!(nexus::check_updates(&inst, &nexus::Client::from_env()?, None)?),
        "nexus_fetch" => json!(nexus::fetch(&inst, &nexus::Client::from_env()?, &nexus::link_or_ids(&inst, &s("link")?)?)?),
        "addons" => json!(addons::list(&inst)?),
        "addon_install" => json!(addons::install(&inst, Path::new(&s("path")?), flag("dry_run"))?),
        "addon_remove" => json!(addons::remove(&inst, &s("name")?)?),
        "addon_call" => addons::call(&inst, &s("name")?, &s("function")?, &a["input"].clone())?,
        "import_sources" => json!(import::find_sources()),
        "import_mo2" => json!(import::import_mo2(&inst, Path::new(&s("dir")?), a["profile"].as_str(), flag("dry_run"))?),
        "import_vortex" => json!(import::import_vortex(&inst, Path::new(&s("path")?), a["staging"].as_str().map(Path::new), a["profile"].as_str(), flag("dry_run"))?),
        "export_lockfile" => json!(lockfile::write(&inst, None)?),
        "apply_lockfile" => {
            let downloads: Vec<PathBuf> = serde_json::from_value(a["downloads"].clone()).unwrap_or_default();
            json!(lockfile::apply(&inst, &lockfile::read(Path::new(&s("path")?))?, &downloads, flag("dry_run"))?)
        }
        "file_conflicts" => json!(conflicts::file_conflicts(&inst, &p, &s("id")?)?),
        "history_details" => json!(inst.history_details()?),
        "restore_preview" => json!(inst.restore_preview(n("n")?)?),
        "restore" => json!(inst.restore(n("n")?)?),
        "set_note" => json!(ops::set_mod_note(&inst, &s("id")?, &s("note")?)?),
        "set_color" => json!(ops::set_color(&inst, &s("id")?, &s("color")?)?),
        "lock_plugin" => json!(ops::lock_plugin(&inst, &s("name")?, flag("lock"))?),
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
        r.join("tools.toml"),
        r.join("loot").join("userlist.yaml"),
        r.join("overwrite"),
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
        .manage(Mcp(Mutex::new(None)))
        .setup(|app| {
            // Long jobs report progress; send the window at most ~10 updates a second.
            let handle = app.handle().clone();
            let last = Mutex::new((String::new(), Instant::now()));
            rmm_core::progress::set_hook(move |stage, done, total| {
                let mut last = last.lock().unwrap();
                if last.0 != stage || last.1.elapsed() > Duration::from_millis(100) {
                    *last = (stage.to_string(), Instant::now());
                    let _ = handle.emit("progress", json!({ "stage": stage, "done": done, "total": total }));
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![call, mcp])
        .run(tauri::generate_context!())
        .expect("error while running rmm");
}
