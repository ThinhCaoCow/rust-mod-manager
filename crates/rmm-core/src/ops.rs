//! Front-end-neutral operations shared by the CLI, the MCP server and the UI.
//! Every mutation goes through `Instance::edit`, so it lands in undo history.
use crate::install::is_plugin;
use crate::{Instance, conflicts};
use anyhow::{Result, bail};
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Debug, Serialize)]
pub struct ModRow {
    /// Priority: higher wins file conflicts.
    pub priority: usize,
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub files: usize,
    pub plugins: Vec<String>,
}

pub fn instance_info(inst: &Instance) -> Result<Value> {
    let p = inst.profile()?;
    Ok(json!({
        "root": inst.root,
        "game": inst.config.game,
        "game_path": inst.config.game_path,
        "local_path": inst.config.local_path,
        "profile": inst.config.profile,
        "mods": p.mods.len(),
        "enabled_mods": p.mods.iter().filter(|m| m.enabled).count(),
        "undo_steps": inst.history()?.len(),
    }))
}

pub fn list_mods(inst: &Instance) -> Result<Vec<ModRow>> {
    let p = inst.profile()?;
    p.mods
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let files = inst.mod_files(&m.id).unwrap_or_default();
            Ok(ModRow {
                priority: i,
                name: inst.mod_meta(&m.id).name,
                enabled: m.enabled,
                plugins: files.iter().filter(|f| !f.contains('/') && is_plugin(f)).cloned().collect(),
                files: files.len(),
                id: m.id.clone(),
            })
        })
        .collect()
}

pub fn mod_details(inst: &Instance, id: &str, max_files: usize) -> Result<Value> {
    let p = inst.profile()?;
    let Some(entry) = p.mods.iter().find(|m| m.id == id) else { bail!("no mod '{id}'") };
    let files = inst.mod_files(id)?;
    let conflict = conflicts::conflicts(inst, &p)?.into_iter().find(|c| c.id == id);
    Ok(json!({
        "id": id,
        "meta": inst.mod_meta(id),
        "enabled": entry.enabled,
        "file_count": files.len(),
        "files": files.iter().take(max_files).collect::<Vec<_>>(),
        "files_truncated": files.len() > max_files,
        "conflicts": conflict,
    }))
}

/// Enabling or disabling a mod also toggles its plugins (no hidden "enable plugins" step).
pub fn set_mod_enabled(inst: &Instance, id: &str, enabled: bool) -> Result<()> {
    let plugins: Vec<String> = inst.mod_files(id)?.into_iter().filter(|f| !f.contains('/') && is_plugin(f)).collect();
    let verb = if enabled { "enable" } else { "disable" };
    inst.edit(&format!("{verb} {id}"), |p| {
        let Some(m) = p.mods.iter_mut().find(|m| m.id == id) else { bail!("no mod '{id}'") };
        m.enabled = enabled;
        for pl in p.plugins.iter_mut().filter(|e| plugins.iter().any(|x| x.eq_ignore_ascii_case(&e.name))) {
            pl.enabled = enabled;
        }
        Ok(())
    })
}

pub fn move_mod(inst: &Instance, id: &str, priority: usize) -> Result<()> {
    inst.edit(&format!("move {id} to {priority}"), |p| {
        let Some(from) = p.mods.iter().position(|m| m.id == id) else { bail!("no mod '{id}'") };
        let m = p.mods.remove(from);
        p.mods.insert(priority.min(p.mods.len()), m);
        Ok(())
    })
}

/// Replace the whole mod order. `ids` must list every mod exactly once.
pub fn set_mod_order(inst: &Instance, ids: &[String]) -> Result<()> {
    inst.edit("reorder mods", |p| {
        if ids.len() != p.mods.len() || !p.mods.iter().all(|m| ids.contains(&m.id)) {
            bail!("ids must contain every installed mod exactly once ({} mods)", p.mods.len());
        }
        p.mods.sort_by_key(|m| ids.iter().position(|i| *i == m.id));
        Ok(())
    })
}

pub fn set_plugin_enabled(inst: &Instance, name: &str, enabled: bool) -> Result<()> {
    let verb = if enabled { "enable" } else { "disable" };
    inst.edit(&format!("{verb} plugin {name}"), |p| {
        match p.plugins.iter_mut().find(|e| e.name.eq_ignore_ascii_case(name)) {
            Some(e) => e.enabled = enabled,
            None => p.plugins.push(crate::PluginEntry { name: name.into(), enabled }),
        }
        Ok(())
    })
}

/// Put `names` in this order; plugins not listed keep their relative order after them.
pub fn set_plugin_order(inst: &Instance, names: &[String]) -> Result<()> {
    inst.edit("reorder plugins", |p| {
        let key = |n: &str| names.iter().position(|x| x.eq_ignore_ascii_case(n)).unwrap_or(usize::MAX);
        for n in names {
            if !p.plugins.iter().any(|e| e.name.eq_ignore_ascii_case(n)) {
                p.plugins.push(crate::PluginEntry { name: n.clone(), enabled: true });
            }
        }
        p.plugins.sort_by_key(|e| key(&e.name));
        Ok(())
    })
}
