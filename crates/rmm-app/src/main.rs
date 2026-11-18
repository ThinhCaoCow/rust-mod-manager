//! Desktop UI: a thin shell over rmm-core. The window is plain HTML/JS in `ui/`. Every action is
//! one `call` into the same core functions the CLI and MCP server use, so it lands in the same
//! undo history. The window polls `stamp` and reloads when another process (the CLI, an AI over
//! MCP) changes the instance.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use anyhow::{Context, Result, bail};
use base64::prelude::*;
use rmm_core::instance::OVERWRITE;
use rmm_core::{Game, Instance, addons, conflicts, deploy, game, harness, health, import, install, launch, lockfile, logs, loot, nexus, ops, plugins, rules, saves, settings, tools, workspace};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use rmm::agent::{self, Agent, Answer};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{Emitter, Manager};

/// The open instance folder.
struct Current(Mutex<Option<PathBuf>>);

/// The MCP server the window hosts for AI clients, when switched on.
struct Mcp(Mutex<Option<rmm::http::Http>>);


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
    let started = rmm::http::token().and_then(|token| Ok((rmm::http::start(root, &format!("127.0.0.1:{}", settings::get().mcp.port), token.clone())?, token)));
    let (http, token) = started.map_err(|e| format!("{e:#}"))?;
    let url = format!("http://{}/mcp", http.addr);
    *slot = Some(http);
    Ok(json!({ "on": true, "url": url, "token": token }))
}

/// The AI tab's chat: the agent (for this instance folder), the window's answer to a change waiting
/// for approval, and the Stop button.
#[derive(Default)]
struct Ai {
    agent: Arc<Mutex<Option<(PathBuf, Agent)>>>,
    answer: Mutex<Option<Sender<Answer>>>,
    stop: Arc<AtomicBool>,
}

/// Providers, keys, models and settings (`status`, `use`, `key`, `forget`, `models`, `options`),
/// and the instance's chats (`chats`, `chat`, `delete_chat`).
#[tauri::command]
async fn ai(current: tauri::State<'_, Current>, name: String, args: Value) -> Result<Value, String> {
    let root = current.0.lock().unwrap().clone();
    let work = move || -> Result<Value> {
        let s = |k: &str| args[k].as_str().map(str::to_owned);
        let need = |k: &str| s(k).with_context(|| format!("missing '{k}'"));
        let root = || root.clone().context("no instance is open");
        Ok(match name.as_str() {
            "options" => json!(agent::set_options(&need("provider")?, serde_json::from_value(args["options"].clone())?)?),
            "chats" => json!(agent::chats(&root()?)?),
            "chat" => {
                // The screen's log is enough to draw it; the provider's messages stay on disk.
                let mut c = agent::load_chat(&root()?, &need("id")?)?;
                c.messages.clear();
                json!(c)
            }
            "delete_chat" => json!(agent::delete_chat(&root()?, &need("id")?)?),
            "status" => json!(agent::status()),
            "use" => json!(agent::configure(&need("provider")?, s("model").as_deref(), s("base").as_deref(), args["switch"] != false)?),
            "team" => json!(agent::team()),
            "seat" => json!(agent::set_seat(&need("role")?, &s("provider").unwrap_or_default(), &s("model").unwrap_or_default())?),
            "key" => json!(agent::set_key(&need("provider")?, &need("key")?)?),
            "forget" => json!(agent::forget_key(&need("provider")?)?),
            "models" => json!(agent::Conn::open(s("provider").as_deref(), None)?.models()?),
            other => bail!("unknown AI call '{other}'"),
        })
    };
    tauri::async_runtime::spawn_blocking(work).await.map_err(|e| e.to_string())?.map_err(|e| format!("{e:#}"))
}

/// The window as the agent's screen: events go out as `ai` events, approvals wait for `ai_answer`.
struct Window {
    app: tauri::AppHandle,
    answers: Receiver<Answer>,
    stop: Arc<AtomicBool>,
}

impl agent::Ui for Window {
    fn event(&mut self, e: agent::Event) {
        let _ = self.app.emit("ai", e);
    }

    fn approve(&mut self, name: &str, args: &Value) -> Answer {
        let _ = self.app.emit("ai", json!({ "kind": "approve", "name": name, "args": args }));
        self.answers.recv().unwrap_or(Answer::No)
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn exchange(&mut self, x: &agent::Exchange) {
        let _ = self.app.emit("ai-exchange", x);
    }
}

/// Send a message in `chat` (a saved chat's id, or none for a new one). A role's message goes to the
/// AI chosen for that role in Settings (a role given to the AI app over MCP goes on the team board);
/// without one, the chat goes on with its own AI, and a new chat gets the provider in use. The turn
/// runs on its own thread and ends with an `ai` event of kind `done` carrying the chat that ran.
#[tauri::command]
fn ai_send(app: tauri::AppHandle, current: tauri::State<'_, Current>, ai: tauri::State<'_, Ai>, text: String, role: Option<String>, images: Vec<String>, chat: Option<String>) -> Result<(), String> {
    let root = current.0.lock().unwrap().clone().ok_or("no instance is open")?;
    let (tx, answers) = std::sync::mpsc::channel();
    *ai.answer.lock().unwrap() = Some(tx);
    ai.stop.store(false, Ordering::Relaxed);
    let (slot, stop) = (ai.agent.clone(), ai.stop.clone());
    // Its own thread, not the async runtime's: the agent runs a runtime of its own for rmm's tools.
    std::thread::spawn(move || {
        let mut ui = Window { app: app.clone(), answers, stop };
        let mut slot = slot.lock().unwrap();
        let mut ran = false;
        let mut turn = || -> Result<()> {
            // Picked in the window (data: URLs) or dropped on it (paths).
            let images = images.iter().map(|d| if d.starts_with("data:") { agent::Image::from_data_url(d) } else { agent::Image::load(Path::new(d)) }).collect::<Result<Vec<_>>>()?;
            let role = role.as_deref().filter(|r| !r.is_empty());
            let seat = role.map(|r| agent::role(r).map(|&(key, ..)| agent::seat(key))).transpose()?;
            if let (Some(r), Some(s)) = (role, &seat)
                && s.provider == agent::MCP
            {
                agent::Ui::event(&mut ui, agent::Event::Notice { text: agent::hand_to_app(&root, r, &text)? });
                return Ok(());
            }
            let kept = matches!(slot.as_ref(), Some((r, a)) if *r == root && chat.as_deref() == Some(a.chat.id.as_str()));
            if !kept {
                *slot = None; // the old chat's tools stop first
            }
            let saved = match (&chat, kept) {
                (Some(id), false) => Some(agent::load_chat(&root, id)?),
                _ => None,
            };
            let on = slot.as_ref().map(|(_, a)| &a.chat).or(saved.as_ref()).map(|c| (c.provider.clone(), c.model.clone()));
            let conn = match (seat, on) {
                (Some(s), _) => agent::Conn::for_seat(&s)?,
                (None, Some((p, m))) => agent::Conn::open(Some(&p), Some(&m))?,
                (None, None) => agent::Conn::for_seat(&agent::seat("chat"))?,
            };
            match slot.as_mut() {
                Some((_, a)) => {
                    if a.conn.provider.id != conn.provider.id {
                        bail!("this chat is with {}; start a new chat for {}", a.conn.provider.name, conn.provider.name);
                    }
                    a.conn = conn;
                }
                None => {
                    let a = match saved {
                        Some(c) => Agent::resume(conn, root.clone(), c)?,
                        None => Agent::new(conn, root.clone())?,
                    };
                    *slot = Some((root.clone(), a));
                }
            }
            let (_, a) = slot.as_mut().context("no chat")?;
            ran = true;
            a.send(role, &text, &images, &mut ui)
        };
        let error = turn().err().map(|e| format!("{e:#}"));
        let chat = slot.as_ref().filter(|_| ran).map(|(_, a)| json!({ "id": a.chat.id, "title": a.chat.title, "usage": a.chat.usage, "provider": a.chat.provider, "model": a.chat.model }));
        let _ = app.emit("ai", json!({ "kind": "done", "error": error, "chat": chat }));
    });
    Ok(())
}

/// The window's answer to the change waiting for approval: yes, no or always.
#[tauri::command]
fn ai_answer(ai: tauri::State<'_, Ai>, answer: String) {
    let a = match answer.as_str() {
        "yes" => Answer::Yes,
        "always" => Answer::Always,
        _ => Answer::No,
    };
    if let Some(tx) = ai.answer.lock().unwrap().as_ref() {
        let _ = tx.send(a);
    }
}

/// End the turn after the current step (declining a change that waits).
#[tauri::command]
fn ai_stop(ai: tauri::State<'_, Ai>) {
    ai.stop.store(true, Ordering::Relaxed);
    if let Some(tx) = ai.answer.lock().unwrap().as_ref() {
        let _ = tx.send(Answer::No);
    }
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
        "search_drives" => return Ok((json!(game::search_drives()), root)),
        "open" => {
            let inst = Instance::open(Path::new(&s("path")?))?;
            return Ok((ops::instance_info(&inst)?, Some(inst.root)));
        }
        "create" => {
            let g: Game = serde_json::from_value(a["game"].clone())?;
            // A folder the search found, or the one detection finds.
            let game_path = match a["game_path"].as_str() {
                Some(p) => PathBuf::from(p),
                None => game::detect(g).context("game not found; create the instance with `rmm init --game-path`")?.game_path,
            };
            let local = game::local_path(g, &game_path).context("cannot work out where plugins.txt lives; use `rmm init --local-path`")?;
            let inst = Instance::create(Path::new(&s("path")?), g, &game_path, &local)?;
            return Ok((ops::instance_info(&inst)?, Some(inst.root)));
        }
        // rmm's settings (no instance needed), with what the Settings tab needs to draw them.
        "settings" => return Ok((json!({ "settings": settings::get(), "health_kinds": health::KINDS }), root)),
        "set_settings" => {
            settings::set(&serde_json::from_value(a["settings"].clone())?)?;
            return Ok((json!(settings::get()), root));
        }
        "reset_settings" => return Ok((json!(settings::reset(None)?), root)),
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
        // The Deploy button's count, without the file lists (megabytes on a long list).
        "pending" => {
            let p = deploy::deploy(&inst, true)?;
            json!(p.link.len() + p.remove.len() + p.external_changes.len() + p.played.len())
        }
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
            let r = install::install_at(&inst, Path::new(&s("path")?), name, settings::get().install.enable, Some(&choices()?.unwrap_or_default()), at)?;
            if let Ok(n) = serde_json::from_value::<rmm_core::instance::NexusRef>(a["nexus"].clone()) {
                let id = n.mod_id;
                nexus::remember(&inst, &r.id, n)?;
                let _ = nexus::refresh_requirements(&inst, &nexus::Client::any(), &[id]); // for Problems; the install stands without
            }
            json!(r)
        }
        "reinstall" => json!(install::reinstall(&inst, &s("id")?, choices()?.as_ref())?),
        "update_mod" => {
            let r = install::update(&inst, &s("id")?, Path::new(&s("path")?), choices()?.as_ref())?;
            if let Ok(n) = serde_json::from_value::<rmm_core::instance::NexusRef>(a["nexus"].clone()) {
                let id = n.mod_id;
                nexus::remember(&inst, &r.id, n)?;
                let _ = nexus::refresh_requirements(&inst, &nexus::Client::any(), &[id]); // a new version can need new mods
            }
            json!(r)
        }
        // The installed mod a download is another version of, if any: [id, name].
        "same_mod" => json!(install::same_mod(&inst, Path::new(&s("path")?), a["mod_id"].as_u64())?),
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
            let limit = Some(Duration::from_secs(settings::get().tools.timeout_mins * 60)).filter(|d| !d.is_zero());
            json!(tools::run(&inst, &s("id")?, &vars, flag("dry_run"), limit)?)
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
        "script_extender_mods" => json!(launch::script_extender_mods(&inst)?),
        "create_profile" => json!(inst.create_profile(&s("name")?, flag("copy"))?),
        "switch_profile" => json!(inst.switch_profile(&s("name")?)?),
        "rename_profile" => json!(inst.rename_profile(&s("from")?, &s("to")?)?),
        "set_profile_options" => json!(ops::set_profile_options(&inst, a["local_inis"].as_bool(), a["local_saves"].as_bool())?),
        "delete_profile" => json!(inst.delete_profile(&s("name")?)?),
        "crash_log" => json!(logs::read_crash_log(&inst, a["path"].as_str().map(Path::new))?),
        "papyrus_log" => json!(logs::read_papyrus_log(&inst, 60)?),
        "se_log" => json!(logs::script_extender_log(&inst, true)?),
        // nxm links `rmm nxm` left for this window (the website's "Mod manager download" buttons).
        "nxm_links" => json!(nexus::take_links(&inst.root)),
        "nexus_status" => {
            let mut st = nexus::status();
            // Who opens nxm:// links, and whether it's this instance (`rmm -i "<root>" nxm`).
            let cmd = nexus::handler_command();
            let here = format!("-i \"{}\"", inst.root.display()).to_lowercase();
            st["handler_here"] = json!(cmd.as_deref().is_some_and(|c| c.to_lowercase().contains(&here)));
            st["handler"] = json!(cmd);
            // Signed in: fetch the game's category names once, so Nexus categories show.
            if st["user"].is_object() {
                st["categories_read"] = json!(nexus::Client::from_env().and_then(|c| nexus::refresh_categories(&inst, &c)).unwrap_or(false));
            }
            st
        }
        "nexus_set_key" => nexus::set_key(&s("key")?)?,
        "nexus_sso" => nexus::sso(&nexus::open_in_browser)?,
        "nexus_forget_key" => json!(nexus::forget_key()?),
        "check_updates" => json!(nexus::check_updates(&inst, &nexus::Client::from_env()?, None)?),
        "nexus_fetch" => {
            let client = nexus::Client::from_env()?;
            json!(nexus::fetch(&inst, &client, &nexus::resolve(&inst, &client, &s("link")?)?)?)
        }
        "nexus_keys_page" => json!(nexus::open_in_browser(nexus::KEYS_PAGE)?),
        // Only on the user's click: it makes this rmm the system's program for nxm:// links.
        "nexus_handler" => {
            let cli = std::env::current_exe()?.with_file_name(if cfg!(windows) { "rmm.exe" } else { "rmm" });
            anyhow::ensure!(cli.is_file(), "rmm's command-line program isn't next to the app ({})", cli.display());
            json!(nexus::register_handler(&cli, &inst.root)?)
        }
        "addons" => json!(addons::list(&inst)?),
        "addon_install" => json!(addons::install(&inst, Path::new(&s("path")?), flag("dry_run"))?),
        "addon_remove" => json!(addons::remove(&inst, &s("name")?)?),
        "addon_call" => addons::call(&inst, &s("name")?, &s("function")?, &a["input"].clone())?,
        "import_sources" => json!(import::find_sources()),
        // Of these MO2 instances, the ones for this instance's game.
        "mo2_for_game" => json!(serde_json::from_value::<Vec<PathBuf>>(a["dirs"].clone())?.into_iter().filter(|d| import::mo2_game(d) == Some(inst.config.game)).collect::<Vec<_>>()),
        "mo2_profiles" => json!(import::mo2_profiles(Path::new(&s("dir")?))?),
        "import_mo2" => json!(import::import_mo2(&inst, Path::new(&s("dir")?), a["profile"].as_str(), flag("dry_run"))?),
        "import_vortex" => json!(import::import_vortex(&inst, Path::new(&s("path")?), a["staging"].as_str().map(Path::new), a["profile"].as_str(), flag("dry_run"))?),
        "export_lockfile" => json!(lockfile::write(&inst, None)?),
        "apply_lockfile" => {
            let downloads: Vec<PathBuf> = serde_json::from_value(a["downloads"].clone()).unwrap_or_default();
            json!(lockfile::apply(&inst, &lockfile::read(Path::new(&s("path")?))?, &downloads, flag("dry_run"))?)
        }
        "file_conflicts" => json!(conflicts::file_conflicts(&inst, &p, &s("id")?)?),
        "data" => json!(conflicts::data_view(&inst, &p, &serde_json::from_value(a.clone())?)?),
        "saves" => json!(saves::list(&inst)?),
        "newest_save_using" => json!(saves::newest_using(&inst, &serde_json::from_value::<Vec<String>>(a["plugins"].clone())?)?),
        "enable_save_plugins" => json!(saves::enable_missing(&inst, &s("file")?)?),
        "downloads" => json!(install::downloads(&inst)?),
        "downloads_dir" => json!({ "path": inst.downloads_dir(), "custom": inst.config.downloads_path.is_some() }),
        "set_downloads_dir" => {
            inst.set_downloads_dir(a["path"].as_str().map(str::trim).filter(|p| !p.is_empty()).map(PathBuf::from))?;
            json!({ "path": inst.downloads_dir(), "custom": inst.config.downloads_path.is_some() })
        }
        "delete_download" => json!(install::delete_download(&inst, &s("name")?)?),
        "trash" => json!(install::trash(&inst)?),
        "empty_trash" => json!(install::empty_trash(&inst)?),
        "history_details" => json!(inst.history_details()?),
        "restore_preview" => json!(inst.restore_preview(n("n")?)?),
        "restore" => json!(inst.restore(n("n")?)?),
        "set_note" => json!(ops::set_mod_note(&inst, &s("id")?, &s("note")?)?),
        "ide_list" => json!(workspace::list(&inst, a["path"].as_str().unwrap_or(""))?),
        "ide_read" => json!(workspace::read(&inst, &s("path")?)?),
        "ide_write" => json!(workspace::write(&inst, &s("path")?, &s("text")?, a["expect"].as_str(), "you")?),
        "ide_edits" => json!(workspace::edits(&inst)?),
        "ide_revert" => json!(workspace::revert(&inst, n("n")?)?),
        "categories" => {
            let list = |k: &str| serde_json::from_value::<Vec<String>>(a[k].clone()).unwrap_or_default();
            json!(ops::edit_categories(&inst, &ids()?, &list("add"), &list("remove"))?)
        }
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
        .manage(Ai::default())
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
            // Hold the open instance's window lock, so `rmm nxm` leaves links for this window, and
            // bring the window up when links arrive; the page takes them with `nxm_links`.
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                let mut held: Option<(PathBuf, fs::File)> = None;
                let mut waiting = 0;
                loop {
                    let root = handle.state::<Current>().0.lock().unwrap().clone();
                    if held.as_ref().map(|h| &h.0) != root.as_ref() {
                        held = root.and_then(|r| nexus::hold_window(&r).ok().map(|f| (r, f)));
                        waiting = 0;
                    }
                    if let Some((r, _)) = &held {
                        let now = nexus::pending_links(r);
                        if now > waiting {
                            if let Some(w) = handle.get_webview_window("main") {
                                let _ = w.unminimize();
                                let _ = w.set_focus();
                            }
                            let _ = handle.emit("nxm", now);
                        }
                        waiting = now;
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![call, mcp, ai, ai_send, ai_answer, ai_stop])
        .run(tauri::generate_context!())
        .expect("error while running rmm");
}
