//! Front-end-neutral operations shared by the CLI, the MCP server and the UI.
//! Every mutation goes through `Instance::edit`, so it lands in undo history.
use crate::install::is_plugin;
use crate::instance::is_separator;
use crate::{Instance, ModEntry, conflicts};
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
    /// A separator: a heading for the mods below it, with no files and no enabled state.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub separator: bool,
    /// Separator colour, `#rrggbb`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// When the mod was installed (its metadata written), in seconds since 1970.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed: Option<u64>,
    /// What the mod contains, worked out from its files: plugins, skse/f4se, scripts, meshes,
    /// textures, animations, interface, sound, archives, root, fomod, tool output.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// Tags for a mod's files (`/`-separated, relative to the mod folder).
pub fn tags(files: &[String], meta: &crate::instance::ModMeta) -> Vec<&'static str> {
    let mut t = Vec::new();
    let any = |f: &dyn Fn(&str) -> bool| files.iter().any(|x| f(&x.to_lowercase()));
    type Rule<'a> = (&'static str, &'a dyn Fn(&str) -> bool);
    let rules: [Rule; 11] = [
        ("plugins", &|x| !x.contains('/') && is_plugin(x)),
        ("skse", &|x| (x.starts_with("skse/plugins/") || x.starts_with("f4se/plugins/")) && x.ends_with(".dll")),
        ("scripts", &|x| x.starts_with("scripts/") && x.ends_with(".pex")),
        ("meshes", &|x| x.starts_with("meshes/") && x.ends_with(".nif")),
        ("textures", &|x| x.starts_with("textures/")),
        ("animations", &|x| x.ends_with(".hkx")),
        ("interface", &|x| x.starts_with("interface/")),
        ("sound", &|x| x.starts_with("sound/") || x.starts_with("music/")),
        ("archives", &|x| !x.contains('/') && (x.ends_with(".bsa") || x.ends_with(".ba2"))),
        ("root", &|x| x.starts_with("root/")),
        ("ini", &|x| !x.contains('/') && x.ends_with(".ini")),
    ];
    for (tag, rule) in rules {
        if any(rule) {
            t.push(tag);
        }
    }
    if meta.fomod.is_some() {
        t.push("fomod");
    }
    if meta.source.as_deref().is_some_and(|s| s.starts_with("tool:")) {
        t.push("tool output");
    }
    t
}

pub fn instance_info(inst: &Instance) -> Result<Value> {
    let p = inst.profile()?;
    Ok(json!({
        "root": inst.root,
        "game": inst.config.game,
        "game_path": inst.config.game_path,
        "local_path": inst.config.local_path,
        "profile": inst.config.profile,
        "mods": p.mods.iter().filter(|m| !is_separator(&m.id)).count(),
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
            let separator = is_separator(&m.id);
            let files = if separator { vec![] } else { inst.mod_files(&m.id).unwrap_or_default() };
            let meta = if separator { Default::default() } else { inst.mod_meta(&m.id) };
            Ok(ModRow {
                priority: i,
                tags: if separator { vec![] } else { tags(&files, &meta) },
                notes: meta.notes.clone(),
                name: if separator { m.id[4..].to_string() } else { meta.name },
                enabled: m.enabled,
                plugins: files.iter().filter(|f| !f.contains('/') && is_plugin(f)).cloned().collect(),
                files: files.len(),
                id: m.id.clone(),
                separator,
                color: p.colors.get(&m.id).cloned(),
                installed: std::fs::metadata(inst.mod_dir(&m.id).join(crate::instance::MOD_META))
                    .and_then(|f| f.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs()),
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
    if is_separator(id) {
        bail!("a separator has no enabled state");
    }
    set_mods_enabled(inst, &[id.to_string()], enabled)
}

/// Enable or disable several mods and their plugins in one undo step. Separators are skipped.
pub fn set_mods_enabled(inst: &Instance, ids: &[String], enabled: bool) -> Result<()> {
    let ids: Vec<&String> = ids.iter().filter(|id| !is_separator(id)).collect();
    let mut plugins = Vec::new();
    for id in &ids {
        plugins.extend(inst.mod_files(id)?.into_iter().filter(|f| !f.contains('/') && is_plugin(f)));
    }
    let verb = if enabled { "enable" } else { "disable" };
    let label = match ids.as_slice() {
        [one] => format!("{verb} {one}"),
        _ => format!("{verb} {} mods", ids.len()),
    };
    inst.edit(&label, |p| {
        for id in &ids {
            let Some(m) = p.mods.iter_mut().find(|m| m.id == **id) else { bail!("no mod '{id}'") };
            m.enabled = enabled;
        }
        for pl in p.plugins.iter_mut().filter(|e| plugins.iter().any(|x| x.eq_ignore_ascii_case(&e.name))) {
            pl.enabled = enabled;
        }
        Ok(())
    })
}

pub fn move_mod(inst: &Instance, id: &str, priority: usize) -> Result<()> {
    move_mods(inst, &[id.to_string()], priority)
}

/// Move mods (and separators) as one block, keeping their order, so the first lands at
/// `priority` in the resulting list (clamped to the end). One undo step.
pub fn move_mods(inst: &Instance, ids: &[String], priority: usize) -> Result<()> {
    let label = match ids {
        [one] => format!("move {one} to {priority}"),
        _ => format!("move {} mods to {priority}", ids.len()),
    };
    inst.edit(&label, |p| {
        if let Some(id) = ids.iter().find(|id| !p.mods.iter().any(|m| m.id == **id)) {
            bail!("no mod '{id}'");
        }
        let (block, mut rest): (Vec<ModEntry>, Vec<ModEntry>) =
            std::mem::take(&mut p.mods).into_iter().partition(|m| ids.contains(&m.id));
        let at = priority.min(rest.len());
        rest.splice(at..at, block);
        p.mods = rest;
        Ok(())
    })
}

/// Add a separator, a heading for the mods below it, at `priority` (default: the end).
/// Returns its id.
pub fn add_separator(inst: &Instance, name: &str, priority: Option<usize>) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        bail!("a separator needs a name");
    }
    inst.edit(&format!("add separator {name}"), |p| {
        let base = format!("sep:{name}");
        let taken = |id: &str| p.mods.iter().any(|m| m.id == id);
        let id = if taken(&base) { (2..).map(|n| format!("{base} ({n})")).find(|id| !taken(id)).unwrap() } else { base };
        let at = priority.unwrap_or(p.mods.len()).min(p.mods.len());
        p.mods.insert(at, ModEntry { id: id.clone(), enabled: false });
        Ok(id)
    })
}

/// Rename a mod (its display name; the id and folder stay). Returns the previous name. Names
/// live with the mod, not the profile, so this isn't in the undo history.
pub fn rename_mod(inst: &Instance, id: &str, name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        bail!("a mod needs a name");
    }
    if is_separator(id) {
        bail!("'{id}' is a separator; use edit_separator");
    }
    if !inst.mod_dir(id).join(crate::instance::MOD_META).is_file() {
        bail!("mod '{id}' is not installed");
    }
    inst.locked(|| {
        let mut meta = inst.mod_meta(id);
        let old = std::mem::replace(&mut meta.name, name.into());
        inst.write_mod_meta(id, &meta)?;
        Ok(old)
    })
}

/// Set or clear (empty text) a mod's notes. Returns the previous notes. Notes live with the mod,
/// so this isn't in the undo history.
pub fn set_mod_note(inst: &Instance, id: &str, note: &str) -> Result<Option<String>> {
    if is_separator(id) || !inst.mod_dir(id).join(crate::instance::MOD_META).is_file() {
        bail!("mod '{id}' is not installed");
    }
    inst.locked(|| {
        let mut meta = inst.mod_meta(id);
        let old = std::mem::replace(&mut meta.notes, Some(note.trim().to_string()).filter(|n| !n.is_empty()));
        inst.write_mod_meta(id, &meta)?;
        Ok(old)
    })
}

/// Colour a row (separator or mod): `#rrggbb`, or empty to remove it. Undoable.
pub fn set_color(inst: &Instance, id: &str, color: &str) -> Result<()> {
    let valid = color.len() == 7 && color.starts_with('#') && color[1..].chars().all(|ch| ch.is_ascii_hexdigit());
    if !color.is_empty() && !valid {
        bail!("a colour is written #rrggbb");
    }
    inst.edit(&format!("colour {}", id.strip_prefix("sep:").unwrap_or(id)), |p| {
        if !p.mods.iter().any(|m| m.id == id) {
            bail!("no mod or separator '{id}'");
        }
        if color.is_empty() {
            p.colors.remove(id);
        } else {
            p.colors.insert(id.into(), color.to_lowercase());
        }
        Ok(())
    })
}

/// Lock a plugin at its current position in the profile's plugin list (MO2's index lock), or
/// unlock it. Sorting and moving other plugins then leave it there. Undoable.
pub fn lock_plugin(inst: &Instance, name: &str, lock: bool) -> Result<()> {
    inst.edit(&format!("{} {name}", if lock { "lock" } else { "unlock" }), |p| {
        let Some(i) = p.plugins.iter().position(|e| e.name.eq_ignore_ascii_case(name)) else { bail!("no plugin '{name}' in the profile") };
        let name = p.plugins[i].name.clone();
        p.locks.retain(|k, _| !k.eq_ignore_ascii_case(&name));
        if lock {
            p.locks.insert(name, i);
        }
        Ok(())
    })
}

/// Turn the active profile's own INIs and saves on or off (`None` leaves one as it is). They take
/// effect on the next deploy. Undoable.
pub fn set_profile_options(inst: &Instance, local_inis: Option<bool>, local_saves: Option<bool>) -> Result<()> {
    inst.edit("profile options", |p| {
        p.local_inis = local_inis.unwrap_or(p.local_inis);
        p.local_saves = local_saves.unwrap_or(p.local_saves);
        Ok(())
    })
}

/// Rename a separator and/or set its colour (`#rrggbb`; an empty string removes it).
/// Returns its id, which follows the name.
pub fn edit_separator(inst: &Instance, id: &str, name: Option<&str>, color: Option<&str>) -> Result<String> {
    if !is_separator(id) {
        bail!("'{id}' is not a separator");
    }
    let valid = |c: &str| c.len() == 7 && c.starts_with('#') && c[1..].chars().all(|ch| ch.is_ascii_hexdigit());
    if let Some(c) = color
        && !c.is_empty()
        && !valid(c)
    {
        bail!("a colour is written #rrggbb");
    }
    inst.edit(&format!("edit separator {}", &id[4..]), |p| {
        let Some(i) = p.mods.iter().position(|m| m.id == id) else { bail!("no separator '{id}'") };
        let mut new_id = id.to_string();
        if let Some(n) = name.map(str::trim).filter(|n| !n.is_empty()) {
            new_id = format!("sep:{n}");
            if new_id != id && p.mods.iter().any(|m| m.id == new_id) {
                bail!("there is already a separator named '{n}'");
            }
            p.mods[i].id = new_id.clone();
            if let Some(c) = p.colors.remove(id) {
                p.colors.insert(new_id.clone(), c);
            }
        }
        match color {
            Some("") => {
                p.colors.remove(&new_id);
            }
            Some(c) => {
                p.colors.insert(new_id.clone(), c.to_lowercase());
            }
            None => {}
        }
        Ok(new_id)
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
