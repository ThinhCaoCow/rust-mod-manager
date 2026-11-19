//! Health check: problems an AI or a user should fix before playing.
use crate::Instance;
use crate::plugins::PluginInfo;
use anyhow::Result;
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;

#[derive(Debug, Serialize)]
pub struct Issue {
    pub severity: &'static str,
    pub kind: &'static str,
    pub subject: String,
    pub message: String,
}

use crate::plugins::{MAX_FULL_PLUGINS, MAX_LIGHT_PLUGINS};
/// Fallout 4's limit on general (non-texture) BA2 archives: 256 until the August 2025 update,
/// 1024 since. Buffout 4 lifts it.
const BA2_OLD_LIMIT: usize = 256;
const BA2_LIMIT: usize = 1024;
/// Larger plugins are rarely light-plugin material and take long to read whole.
const ESL_SCAN_MAX_BYTES: u64 = 16 << 20;

/// Every kind of problem `check` reports, and what it means (for Settings).
pub const KINDS: &[(&str, &str)] = &[
    ("mod_missing", "A mod's folder is gone"),
    ("mod_rule_broken", "A load-after rule is broken"),
    ("mod_requires", "A mod's required mod isn't enabled"),
    ("mod_incompatible", "Incompatible mods are both enabled"),
    ("nexus_requirement", "A mod's Nexus page lists a requirement that isn't installed or enabled"),
    ("load_order_unreadable", "The load order can't be read"),
    ("plugin_unreadable", "A plugin can't be read"),
    ("missing_masters", "A plugin's masters are missing"),
    ("plugin_limit", "Too many full plugins"),
    ("light_plugin_limit", "Too many light plugins"),
    ("esl_candidates", "Plugins that could be ESL-flagged"),
    ("fully_overridden", "A mod whose every file is overridden"),
    ("archive_not_loaded", "An archive no plugin loads"),
    ("archive_unreadable", "An archive can't be read"),
    ("ba2_limit", "Too many BA2 archives (Fallout 4)"),
    ("script_extender_missing", "Script extender plugins without the loader"),
    ("script_extender_mismatch", "The script extender doesn't match the game version"),
    ("game_updated", "The game's version changed since the last Play"),
    ("steam_updates_held", "Steam's updates for the game are held back"),
    ("address_library_mismatch", "Address Library has no file for the game version"),
    ("script_extender_plugin", "A script extender plugin didn't load in the last run"),
    ("deploy_blocked", "Deploy can't run"),
    ("not_deployed", "Changes not deployed yet"),
    ("external_change", "Deployed files changed outside rmm"),
    ("overwrite_files", "Files in Overwrite"),
    ("stale_output", "Generated output out of date"),
    ("loot_message", "LOOT's messages"),
    ("loot_failed", "LOOT couldn't run"),
    ("ini_test_file", "sTestFile lines in Fallout4.ini"),
    ("archive_invalidation", "Archive invalidation is off (Fallout 4)"),
    ("addon", "Add-ons' checks"),
];

pub fn check(inst: &Instance) -> Result<Vec<Issue>> {
    let profile = inst.profile()?;
    let mut issues = Vec::new();
    let mut add = |severity, kind, subject: &str, message: String| {
        issues.push(Issue { severity, kind, subject: subject.into(), message })
    };

    for m in profile.mods.iter().filter(|m| !crate::instance::is_separator(&m.id)) {
        if !inst.mod_dir(&m.id).is_dir() {
            add("error", "mod_missing", &m.id, "mod is in the profile but its folder is gone".into());
        }
    }

    for (id, after) in crate::rules::broken(inst, &profile)? {
        add("warning", "mod_rule_broken", &id, format!("should load after {after} (a mod rule); Sort by rules fixes the order"));
    }
    for u in crate::rules::unmet(inst, &profile)? {
        if !u.missing.is_empty() {
            add("error", "mod_requires", &u.id, format!("requires {}, which {} not enabled (a mod rule)", u.missing.join(", "), if u.missing.len() == 1 { "is" } else { "are" }));
        }
        if !u.clashes.is_empty() {
            add("error", "mod_incompatible", &u.id, format!("is incompatible with {}, which {} enabled too (a mod rule)", u.clashes.join(", "), if u.clashes.len() == 1 { "is" } else { "are" }));
        }
    }

    for (id, message) in crate::nexus::missing_requirements(inst, &profile) {
        add("warning", "nexus_requirement", &id, message);
    }

    // libloadorder reads the game's INIs from the real Documents folder; if that fails (a OneDrive
    // placeholder with OneDrive stopped, say), report it and check what doesn't need the load order.
    let (plugins, lo_ok) = match crate::plugins::list(inst, &profile) {
        Ok(p) => (p, true),
        Err(e) => {
            add("error", "load_order_unreadable", "load order", format!("{e:#}"));
            (vec![], false)
        }
    };
    for p in &plugins {
        if let Some(e) = &p.error {
            add("warning", "plugin_unreadable", &p.name, format!("could not read plugin header: {e}"));
        }
        if p.enabled && !p.missing_masters.is_empty() {
            // A master that is on but loads later only needs the order fixed.
            let (later, absent): (Vec<&String>, Vec<&String>) =
                p.missing_masters.iter().partition(|m| plugins.iter().any(|x| x.enabled && x.name.eq_ignore_ascii_case(m)));
            let join = |v: &[&String]| v.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ");
            let msg = if absent.is_empty() {
                format!("loads before {}, which it's built on (its masters); the game crashes at start until they load first. Sort with LOOT or move it below them", join(&later))
            } else {
                format!("needs {} turned on and loaded before it: its masters, the plugins it's built on; the game crashes at start without them", join(&p.missing_masters.iter().collect::<Vec<_>>()))
            };
            add("error", "missing_masters", &p.name, msg);
        }
    }
    let full = plugins.iter().filter(|p| p.enabled && !p.light).count();
    let light = plugins.iter().filter(|p| p.enabled && p.light).count();
    if full > MAX_FULL_PLUGINS {
        add("error", "plugin_limit", "load order", format!("{full} full plugins enabled; the game supports {MAX_FULL_PLUGINS}"));
    }
    if light > MAX_LIGHT_PLUGINS {
        add("error", "light_plugin_limit", "load order", format!("{light} light plugins enabled; the game supports {MAX_LIGHT_PLUGINS}"));
    }
    let candidates = esl_candidates(inst, &plugins);
    if !candidates.is_empty() {
        let shown = candidates.iter().take(20).cloned().collect::<Vec<_>>().join(", ");
        let more = if candidates.len() > 20 { format!(" and {} more", candidates.len() - 20) } else { String::new() };
        add(
            if full > MAX_FULL_PLUGINS { "warning" } else { "info" },
            "esl_candidates",
            "load order",
            format!("{} full plugins could be flagged light (ESL), which frees full slots: {shown}{more}. Flagging changes the plugin; ESLifier or xEdit does it", candidates.len()),
        );
    }

    let map = crate::conflicts::file_map(inst, &profile)?;
    for c in if lo_ok { crate::conflicts::conflicts(inst, &profile)? } else { vec![] } {
        if c.fully_overridden {
            add("info", "fully_overridden", &c.id, format!("every file is overridden by {}; the mod has no effect", c.overwritten_by.join(", ")));
        }
    }

    let (loaded, unloaded) = if lo_ok { crate::conflicts::mod_archives(inst, &profile, &map)? } else { (vec![], vec![]) };
    for (mod_id, name) in unloaded {
        // Base-game archive names are loaded from the INIs, not by a plugin.
        let n = name.to_lowercase();
        if !n.starts_with("skyrim - ") && !n.starts_with("fallout4 - ") {
            add("warning", "archive_not_loaded", &name, format!("{mod_id}: no enabled plugin loads this archive; the game only loads archives named after an active plugin"));
        }
    }
    for a in &loaded {
        if let Err(e) = crate::archives::list(&inst.mod_dir(&a.mod_id).join(&a.name)) {
            add("warning", "archive_unreadable", &a.name, format!("{}: {e:#}", a.mod_id));
        }
    }
    if inst.config.game.base() == crate::Game::Fallout4 {
        // General archives from mods, plus the ones in Data no mod provides (base game, Creation Club).
        let general = |n: &str| n.to_lowercase().ends_with(".ba2") && !n.to_lowercase().contains(" - textures");
        let managed: std::collections::HashSet<String> = map.files.keys().cloned().collect();
        let unmanaged = std::fs::read_dir(inst.data_dir())
            .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| general(n) && !managed.contains(&crate::path_key(n))).count())
            .unwrap_or(0);
        let n = loaded.iter().filter(|a| general(&a.name)).count() + unmanaged;
        let limit = crate::settings::get().health.ba2_limit;
        if limit != 0 {
            if n > limit {
                add("error", "ba2_limit", "archives", format!("{n} general BA2 archives load; your game handles {limit} (Settings). Merge or extract some, or use Buffout 4"));
            }
        } else if n > BA2_LIMIT {
            add("error", "ba2_limit", "archives", format!("{n} general BA2 archives load; Fallout 4 handles {BA2_LIMIT}. Merge or extract some, or use Buffout 4"));
        } else if n > BA2_OLD_LIMIT {
            add("warning", "ba2_limit", "archives", format!("{n} general BA2 archives load; Fallout 4 before the August 2025 update handles {BA2_OLD_LIMIT} (now {BA2_LIMIT}). Update the game or use Buffout 4"));
        }
    }

    let game = inst.config.game;
    let se_dir = format!("data/{}/plugins/", if game.base() == crate::Game::SkyrimSe { "skse" } else { "f4se" });
    let has_se_plugins = map.files.values().any(|(rel, _)| crate::path_key(&format!("Data/{rel}")).starts_with(&se_dir) && rel.to_lowercase().ends_with(".dll"));
    let loader_present = inst.config.game_path.join(game.loader()).is_file() || map.files.contains_key(&format!("root/{}", game.loader()));
    if has_se_plugins && !loader_present {
        add("error", "script_extender_missing", game.loader(), "script extender plugins are installed but the loader is not".into());
    }
    let root = &inst.config.game_path;
    let version = crate::launch::game_version(root, game).ok();
    if let Some(v) = &version
        && root.join(game.loader()).is_file()
        && crate::settings::get().launch.script_extender
        && let Err(e) = crate::launch::se_runtime(root, game, v)
    {
        add("error", "script_extender_mismatch", game.loader(), format!("{e:#}. Play won't start the game until it matches"));
    }
    let se = if game.base() == crate::Game::SkyrimSe { "SKSE" } else { "F4SE" };
    if crate::launch::steam_updates(inst).held {
        add("info", "steam_updates_held", game.exe(), "Steam can't update the game: its Steam manifest is read-only. When you want the update, Let Steam update the game (beside Play, or `rmm steam-updates --release`)".into());
    }
    if let Some(v) = &version {
        if let Some(m) = crate::launch::game_updated(inst, &v.map(|x| x.to_string()).join(".")) {
            add("warning", "game_updated", game.exe(), format!("{m}. The script extender, Address Library and many {se} plugins are made for one game version; the other findings here say what no longer matches. Until they catch up, Play without mods (beside Play) starts the game with none. This goes away at the next Play"));
        }
        if let Some(m) = crate::launch::address_library(&inst.data_dir().join(se).join("Plugins"), game, v) {
            add("error", "address_library_mismatch", "Address Library", m);
        }
    }
    // Plugins from this setup's enabled mods that didn't load in the last run, unless their mod changed since.
    if let Ok(log) = crate::logs::script_extender_log(inst, false) {
        for p in log.plugins.into_iter().filter(|p| !p.changed) {
            let Some(id) = p.mod_id.filter(|id| profile.mods.iter().any(|m| &m.id == id && m.enabled)) else { continue };
            let why = p.problem.unwrap_or_else(|| "the log ends after checking it, so the game may have closed or crashed while loading it".into());
            add("warning", "script_extender_plugin", &id, format!("{} didn't load in the last game run: {why} (Script extender log…)", p.file));
        }
    }

    match crate::deploy::deploy(inst, true) {
        Err(e) => add("error", "deploy_blocked", "deploy", e.to_string()),
        Ok(plan) => {
            if !plan.is_noop() {
                add("info", "not_deployed", "deploy", format!("{} files to link and {} to remove since the last deploy", plan.link.len(), plan.remove.len()));
            }
            for f in plan.external_changes {
                add("warning", "external_change", &f, "changed outside rmm; deploy keeps it in Overwrite (the files that belong to no mod), which wins over every mod".into());
            }
        }
    }
    let over = inst.mod_files(crate::instance::OVERWRITE)?.len();
    if over > 0 {
        let files = if over == 1 { "1 file belongs" } else { &format!("{over} files belong") };
        add("info", "overwrite_files", "Overwrite", format!("{files} to no mod: what the game, MCM menus and tools wrote, which wins over every mod. Move what you want to keep into a mod (Overwrite's Move to mod…), so it goes with the setup"));
    }
    let tools = crate::tools::list(inst).unwrap_or_default();
    for s in crate::tools::stale_outputs(inst).unwrap_or_default() {
        let tool = tools.get(&s.tool).map_or(s.tool.as_str(), |t| t.name.as_str());
        add("warning", "stale_output", &s.output, format!("made by {tool} from a setup that has changed since: {}. Rebuild outputs (Tools) runs it again", s.summary()));
    }
    issues.extend(fallout4_ini(inst)?);
    match crate::loot::issues(inst) {
        Ok(loot) => issues.extend(loot),
        Err(e) => issues.push(Issue { severity: "warning", kind: "loot_failed", subject: "LOOT".into(), message: format!("{e:#}") }),
    }
    for (addon, r) in crate::addons::hook(inst, "health_check", &serde_json::json!({})) {
        match r {
            Ok(v) => {
                for i in v.as_array().into_iter().flatten() {
                    let severity = match i["severity"].as_str() {
                        Some("error") => "error",
                        Some("info") => "info",
                        _ => "warning",
                    };
                    let subject = i["subject"].as_str().unwrap_or(&addon).to_string();
                    issues.push(Issue { severity, kind: "addon", subject, message: format!("{} ({addon})", i["message"].as_str().unwrap_or("")) });
                }
            }
            Err(e) => issues.push(Issue { severity: "warning", kind: "addon", subject: addon, message: format!("{e:#}") }),
        }
    }
    // Kinds the user turned off (Settings).
    let hidden = crate::settings::get().health.hidden;
    issues.retain(|i| !hidden.iter().any(|h| h == i.kind));
    Ok(issues)
}

/// Enabled full plugins (.esp, not master-flagged) whose new records fit a light plugin's
/// FormID range, so they could take an ESL flag. Results are cached by file size and mtime.
fn esl_candidates(inst: &Instance, plugins: &[PluginInfo]) -> Vec<String> {
    use esplugin::{ParseOptions, Plugin, plugins_metadata};
    type Seen = HashMap<PathBuf, (u64, SystemTime, bool)>; // size, mtime, candidate
    static CACHE: Mutex<Option<Seen>> = Mutex::new(None);
    let game = inst.config.game.esp_id();
    let wanted: Vec<&PluginInfo> = plugins
        .iter()
        .filter(|p| p.enabled && !p.light && !p.master && !p.implicit && p.error.is_none() && p.name.to_lowercase().ends_with(".esp"))
        .collect();
    if wanted.is_empty() {
        return vec![];
    }
    let stamp = |p: &PathBuf| std::fs::metadata(p).ok().and_then(|m| Some((m.len(), m.modified().ok()?)));
    let mut cache = CACHE.lock().unwrap();
    let cache = cache.get_or_insert_with(HashMap::new);
    let mut metadata = None; // every plugin's header, read only if something isn't cached
    let mut out = Vec::new();
    for p in wanted {
        let Some((len, mtime)) = stamp(&p.path) else { continue };
        let ok = match cache.get(&p.path) {
            Some(&(l, t, ok)) if l == len && t == mtime => ok,
            _ => {
                let meta = metadata.get_or_insert_with(|| {
                    let headers: Vec<Plugin> = plugins
                        .iter()
                        .filter_map(|x| {
                            let mut h = Plugin::new(game, &x.path);
                            h.parse_file(ParseOptions::header_only()).ok().map(|_| h)
                        })
                        .collect();
                    plugins_metadata(&headers.iter().collect::<Vec<_>>()).unwrap_or_default()
                });
                let ok = len <= ESL_SCAN_MAX_BYTES && {
                    let mut whole = Plugin::new(game, &p.path);
                    whole.parse_file(ParseOptions::whole_plugin()).is_ok()
                        && whole.resolve_record_ids(meta).is_ok()
                        && whole.is_valid_as_light_plugin().unwrap_or(false)
                };
                cache.insert(p.path.clone(), (len, mtime, ok));
                ok
            }
        };
        if ok {
            out.push(p.name.clone());
        }
    }
    out
}

/// Fallout 4 INI settings that quietly undo a deploy. Skipped until the game has written its INIs.
pub fn fallout4_ini(inst: &Instance) -> Result<Vec<Issue>> {
    let mut out = Vec::new();
    let read = |name: &str| inst.ini_file(name).and_then(|p| std::fs::read_to_string(p).ok());
    let Some(base) = read("Fallout4.ini").filter(|_| inst.config.game.base() == crate::Game::Fallout4) else {
        return Ok(out);
    };
    let custom = read("Fallout4Custom.ini").unwrap_or_default();
    let get = |section: &str, key: &str| ini_get(&custom, section, key).or_else(|| ini_get(&base, section, key));
    let mut add = |severity, kind, subject: &str, message: &str| {
        out.push(Issue { severity, kind, subject: subject.into(), message: message.into() })
    };
    if (1..=10).any(|i| get("General", &format!("sTestFile{i}")).is_some_and(|v| !v.is_empty())) {
        add("error", "ini_test_file", "Fallout4.ini", "sTestFile entries under [General] make the game ignore plugins.txt; remove them");
    }
    let map = crate::conflicts::file_map(inst, &inst.profile()?)?;
    let loose = map.files.keys().any(|k| k.contains('/') && !k.starts_with("root/"));
    let invalidates = get("Archive", "bInvalidateOlderFiles").as_deref() == Some("1")
        && get("Archive", "sResourceDataDirsFinal").is_none_or(|v| v.is_empty());
    if loose && !invalidates {
        add("warning", "archive_invalidation", "Fallout4Custom.ini", "loose files can lose to BA2 archives; set bInvalidateOlderFiles=1 and an empty sResourceDataDirsFinal= under [Archive]");
    }
    Ok(out)
}

/// First `key=value` under `[section]`, matched case-insensitively.
pub(crate) fn ini_get(text: &str, section: &str, key: &str) -> Option<String> {
    let mut inside = false;
    for line in text.lines().map(|l| l.trim_start_matches('\u{feff}').trim()) {
        if let Some(s) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            inside = s.trim().eq_ignore_ascii_case(section);
        } else if inside
            && let Some((k, v)) = line.split_once('=')
            && k.trim().eq_ignore_ascii_case(key)
        {
            return Some(v.trim().to_owned());
        }
    }
    None
}

/// `ini` with each (section, key, value) of `set` written at the top of its section (added at the
/// end when missing), or removed where the value is `None`. Other lines keep their bytes.
pub(crate) fn ini_set(ini: &[u8], set: &[(&str, &str, Option<String>)]) -> Vec<u8> {
    let eol: &[u8] = if ini.windows(2).any(|p| p == b"\r\n") { b"\r\n" } else { b"\n" };
    let put = |out: &mut Vec<u8>, section: &str| {
        for (_, k, v) in set.iter().filter(|x| x.0.eq_ignore_ascii_case(section)) {
            let Some(v) = v else { continue };
            out.extend_from_slice(format!("{k}={v}").as_bytes());
            out.extend_from_slice(eol);
        }
    };
    let (mut out, mut section, mut seen) = (Vec::new(), String::new(), Vec::new());
    for line in ini.split_inclusive(|b| *b == b'\n') {
        let text = String::from_utf8_lossy(line);
        let t = text.trim();
        if let Some(name) = t.strip_prefix('[').and_then(|t| t.strip_suffix(']')) {
            section = name.to_string();
            out.extend_from_slice(line);
            if set.iter().any(|x| x.0.eq_ignore_ascii_case(name)) {
                if !line.ends_with(b"\n") {
                    out.extend_from_slice(eol);
                }
                seen.push(name.to_ascii_lowercase());
                put(&mut out, name);
            }
            continue;
        }
        let key = t.split('=').next().unwrap_or("").trim();
        if !set.iter().any(|x| x.0.eq_ignore_ascii_case(&section) && x.1.eq_ignore_ascii_case(key)) {
            out.extend_from_slice(line);
        }
    }
    for (name, ..) in set.iter().filter(|x| x.2.is_some()) {
        if !seen.contains(&name.to_ascii_lowercase()) {
            if !out.is_empty() && !out.ends_with(b"\n") {
                out.extend_from_slice(eol);
            }
            out.extend_from_slice(format!("[{name}]").as_bytes());
            out.extend_from_slice(eol);
            put(&mut out, name);
            seen.push(name.to_ascii_lowercase());
        }
    }
    out
}
