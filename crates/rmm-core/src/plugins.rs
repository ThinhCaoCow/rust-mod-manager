//! Plugin discovery (enabled mods + vanilla Data), header parsing, and plugins.txt writing.
use crate::conflicts::file_map;
use crate::install::is_plugin;
use crate::{Instance, Profile, path_key};
use anyhow::Result;
use esplugin::{ParseOptions, Plugin};
use loadorder::GameSettings;
use serde::Serialize;
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Debug, Serialize)]
pub struct PluginInfo {
    pub name: String,
    /// Mod providing the plugin; None = game Data folder (base game, Creation Club, manual installs).
    pub mod_id: Option<String>,
    pub enabled: bool,
    /// Always active and loaded first by the game (base masters, Creation Club).
    pub implicit: bool,
    pub master: bool,
    pub light: bool,
    pub masters: Vec<String>,
    /// Masters that are not enabled earlier in the load order.
    pub missing_masters: Vec<String>,
    pub error: Option<String>,
    #[serde(skip)]
    pub path: PathBuf,
}

fn settings(inst: &Instance) -> Result<GameSettings> {
    let c = &inst.config;
    GameSettings::with_local_path(c.game.lo_id(), &c.game_path, &c.local_path).map_err(|e| match &e {
        // ERROR_CLOUD_FILE_PROVIDER_NOT_RUNNING: the game can't read it either.
        loadorder::Error::IoError(path, io) if io.raw_os_error() == Some(362) => anyhow::anyhow!(
            "{} is online-only in OneDrive, and OneDrive isn't running: start OneDrive, or make the My Games folder available offline",
            path.display()
        ),
        _ => e.into(),
    })
}

/// Plugins in the order the game will load them: implicit first, then masters, then the rest,
/// each group following the profile's plugin order.
pub fn list(inst: &Instance, profile: &Profile) -> Result<Vec<PluginInfo>> {
    let game = inst.config.game;
    let implicit: Vec<String> = settings(inst)?.implicitly_active_plugins().to_vec();
    let managed = crate::deploy::managed_keys(inst)?;

    let mut found: Vec<(String, Option<String>, PathBuf)> = Vec::new();
    for (_, (mod_id, rel)) in file_map(inst, profile)?.winners() {
        if !rel.contains('/') && is_plugin(&rel) {
            found.push((rel.clone(), Some(mod_id.clone()), inst.mod_dir(&mod_id).join(&rel)));
        }
    }
    let from_mods: HashSet<String> = found.iter().map(|f| path_key(&f.0)).collect();
    if let Ok(rd) = std::fs::read_dir(inst.data_dir()) {
        for e in rd.filter_map(|e| e.ok()) {
            let name = e.file_name().to_string_lossy().into_owned();
            let key = path_key(&name);
            if is_plugin(&name) && !managed.contains(&format!("data/{key}")) && !from_mods.contains(&key) {
                found.push((name, None, e.path()));
            }
        }
    }

    let rank = |name: &str| -> (usize, usize) {
        if let Some(i) = implicit.iter().position(|p| p.eq_ignore_ascii_case(name)) {
            return (0, i);
        }
        let i = profile.plugins.iter().position(|p| p.name.eq_ignore_ascii_case(name)).unwrap_or(usize::MAX);
        (1, i)
    };
    let mut infos: Vec<PluginInfo> = found
        .into_iter()
        .map(|(name, mod_id, path)| {
            let mut p = Plugin::new(game.esp_id(), &path);
            let parsed = p.parse_file(ParseOptions::header_only());
            let is_implicit = rank(&name).0 == 0;
            let enabled = is_implicit
                || profile.plugins.iter().find(|e| e.name.eq_ignore_ascii_case(&name)).is_some_and(|e| e.enabled);
            PluginInfo {
                enabled,
                implicit: is_implicit,
                master: parsed.is_ok() && p.is_master_file(),
                light: parsed.is_ok() && p.is_light_plugin(),
                masters: p.masters().unwrap_or_default(),
                missing_masters: vec![],
                error: parsed.err().map(|e| e.to_string()),
                name,
                mod_id,
                path,
            }
        })
        .collect();
    infos.sort_by_key(|p| (rank(&p.name).0, !p.master, rank(&p.name).1, p.name.to_lowercase()));

    let mut active: HashSet<String> = HashSet::new();
    for p in &mut infos {
        p.missing_masters = p.masters.iter().filter(|m| !active.contains(&m.to_lowercase())).cloned().collect();
        if p.enabled {
            active.insert(p.name.to_lowercase());
        }
    }
    Ok(infos)
}

/// Write plugins.txt (and loadorder.txt where the game uses one) from the profile.
/// Must run after deploy so every plugin exists in Data.
/// The game loads at most this many full plugins, plus this many light ones.
pub const MAX_FULL_PLUGINS: usize = 254;
pub const MAX_LIGHT_PLUGINS: usize = 4096;

/// Why plugins.txt can't hold the enabled plugins, if it can't.
pub fn over_limit(infos: &[PluginInfo]) -> Option<String> {
    let full = infos.iter().filter(|p| p.enabled && !p.light).count();
    let light = infos.iter().filter(|p| p.enabled && p.light).count();
    if full > MAX_FULL_PLUGINS {
        Some(format!("{full} full plugins are enabled and the game loads {MAX_FULL_PLUGINS}: disable some, or flag small ones light (health_check lists candidates)"))
    } else if light > MAX_LIGHT_PLUGINS {
        Some(format!("{light} light plugins are enabled and the game loads {MAX_LIGHT_PLUGINS}"))
    } else {
        None
    }
}

/// Where the game reads its load order from.
pub fn plugins_txt(inst: &Instance) -> Result<std::path::PathBuf> {
    Ok(settings(inst)?.active_plugins_file().clone())
}

pub fn write_plugins_txt(inst: &Instance, profile: &Profile) -> Result<()> {
    std::fs::create_dir_all(&inst.config.local_path)?;
    let infos = list(inst, profile)?;
    let settings = settings(inst)?;
    crate::profile_ini::keep_plugins_txt(inst, settings.active_plugins_file())?;
    let mut lo = settings.into_load_order();
    lo.load()?;
    let names: Vec<&str> = infos.iter().map(|p| p.name.as_str()).collect();
    lo.set_load_order(&names)?;
    let active: Vec<&str> = infos.iter().filter(|p| p.enabled).map(|p| p.name.as_str()).collect();
    lo.set_active_plugins(&active)?;
    lo.save()?;
    Ok(())
}
