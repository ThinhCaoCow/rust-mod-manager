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

    let game = inst.config.game;
    let se_dir = format!("data/{}/plugins/", if game == crate::Game::SkyrimSe { "skse" } else { "f4se" });
    let has_se_plugins = map.files.values().any(|(rel, _)| crate::path_key(&format!("Data/{rel}")).starts_with(&se_dir) && rel.to_lowercase().ends_with(".dll"));
    let loader_present = inst.config.game_path.join(game.loader()).is_file() || map.files.contains_key(&format!("root/{}", game.loader()));
    if has_se_plugins && !loader_present {
        add("error", "script_extender_missing", game.loader(), "script extender plugins are installed but the loader is not".into());
    }

    let plan = crate::deploy::deploy(inst, true)?;
    if !plan.is_noop() {
        add("warning", "not_deployed", "deploy", format!("{} files to link and {} to remove since the last deploy", plan.link.len(), plan.remove.len()));
    }
    for f in plan.external_changes {
        add("warning", "external_change", &f, "changed outside rmm; it will be copied to overwrite/ on next deploy".into());
    }
    Ok(issues)
}
