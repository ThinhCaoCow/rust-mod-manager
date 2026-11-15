//! Health check: problems an AI or a user should fix before playing.
use crate::Instance;
use anyhow::Result;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Issue {
    pub severity: &'static str,
    pub kind: &'static str,
    pub subject: String,
    pub message: String,
}

const MAX_FULL_PLUGINS: usize = 254;
const MAX_LIGHT_PLUGINS: usize = 4096;

pub fn check(inst: &Instance) -> Result<Vec<Issue>> {
    let profile = inst.profile()?;
    let mut issues = Vec::new();
    let mut add = |severity, kind, subject: &str, message: String| {
        issues.push(Issue { severity, kind, subject: subject.into(), message })
    };

    for m in &profile.mods {
        if !inst.mod_dir(&m.id).is_dir() {
            add("error", "mod_missing", &m.id, "mod is in the profile but its folder is gone".into());
        }
    }

    let plugins = crate::plugins::list(inst, &profile)?;
    for p in &plugins {
        if let Some(e) = &p.error {
            add("warning", "plugin_unreadable", &p.name, format!("could not read plugin header: {e}"));
        }
        if p.enabled && !p.missing_masters.is_empty() {
            add("error", "missing_masters", &p.name, format!("needs {} enabled and loaded earlier", p.missing_masters.join(", ")));
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

    let map = crate::conflicts::file_map(inst, &profile)?;
    for c in crate::conflicts::conflicts(inst, &profile)? {
        if c.fully_overridden {
            add("info", "fully_overridden", &c.id, format!("every file is overridden by {}; the mod has no effect", c.overwritten_by.join(", ")));
        }
    }

    let (loaded, unloaded) = crate::conflicts::mod_archives(inst, &profile, &map)?;
    for (mod_id, name) in unloaded {
        // Base-game archive names are loaded from the INIs, not by a plugin.
        let n = name.to_lowercase();
        if !n.starts_with("skyrim - ") && !n.starts_with("fallout4 - ") {
            add("warning", "archive_not_loaded", &name, format!("{mod_id}: no enabled plugin loads this archive; the game only loads archives named after an active plugin"));
        }
    }
    for a in loaded {
        if let Err(e) = crate::archives::list(&inst.mod_dir(&a.mod_id).join(&a.name)) {
            add("warning", "archive_unreadable", &a.name, format!("{}: {e:#}", a.mod_id));
        }
    }

    let game = inst.config.game;
    let se_dir = format!("data/{}/plugins/", if game == crate::Game::SkyrimSe { "skse" } else { "f4se" });
    let has_se_plugins = map.files.values().any(|(rel, _)| crate::path_key(&format!("Data/{rel}")).starts_with(&se_dir) && rel.to_lowercase().ends_with(".dll"));
    let loader_present = inst.config.game_path.join(game.loader()).is_file() || map.files.contains_key(&format!("root/{}", game.loader()));
    if has_se_plugins && !loader_present {
        add("error", "script_extender_missing", game.loader(), "script extender plugins are installed but the loader is not".into());
    }

    match crate::deploy::deploy(inst, true) {
        Err(e) => add("error", "deploy_blocked", "deploy", e.to_string()),
        Ok(plan) => {
            if !plan.is_noop() {
                add("warning", "not_deployed", "deploy", format!("{} files to link and {} to remove since the last deploy", plan.link.len(), plan.remove.len()));
            }
            for f in plan.external_changes {
                add("warning", "external_change", &f, "changed outside rmm; deploy copies it to overwrite/, which then wins over every mod".into());
            }
        }
    }
    issues.extend(fallout4_ini(inst)?);
    match crate::loot::issues(inst) {
        Ok(loot) => issues.extend(loot),
        Err(e) => issues.push(Issue { severity: "warning", kind: "loot_failed", subject: "LOOT".into(), message: format!("{e:#}") }),
    }
    Ok(issues)
}

/// Fallout 4 INI settings that quietly undo a deploy. Skipped until the game has written its INIs.
pub fn fallout4_ini(inst: &Instance) -> Result<Vec<Issue>> {
    let mut out = Vec::new();
    let dir = inst.ini_dir().filter(|_| inst.config.game == crate::Game::Fallout4);
    let Some(base) = dir.as_ref().and_then(|d| std::fs::read_to_string(d.join("Fallout4.ini")).ok()) else {
        return Ok(out);
    };
    let custom = std::fs::read_to_string(dir.unwrap().join("Fallout4Custom.ini")).unwrap_or_default();
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
fn ini_get(text: &str, section: &str, key: &str) -> Option<String> {
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
