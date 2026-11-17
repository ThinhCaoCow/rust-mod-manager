//! LOOT sorting and messages through libloot (SPEC.md "LOOT"). The masterlist and prelude are
//! cached in `<instance>/loot/` and refreshed from LOOT's GitHub repositories at most once a day;
//! `loot/userlist.yaml`, if present, adds the user's own rules; `set_rule` edits it.
use crate::health::Issue;
use crate::instance::OVERWRITE;
use crate::plugins::PluginInfo;
use crate::{Game, Instance, PluginEntry, Profile, plugins};
use anyhow::{Context, Result, bail};
use libloot::metadata::{File, MessageType, PluginMetadata, select_message_content};
use libloot::{EvalMode, GameType, MergeMode, MetadataWriteOptions};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Masterlist branch matching libloot 0.29's metadata syntax.
const BRANCH: &str = "v0.29";
/// How old the masterlist may get (Settings).
fn refresh() -> Duration {
    Duration::from_secs(crate::settings::get().loot.refresh_hours * 3600)
}

#[derive(Debug, Serialize)]
pub struct SortReport {
    pub dry_run: bool,
    /// Every plugin in LOOT's order.
    pub order: Vec<String>,
    /// Plugins whose position changes, with old and new index.
    pub moved: Vec<Moved>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Moved {
    pub name: String,
    pub from: usize,
    pub to: usize,
}

fn dir(inst: &Instance) -> PathBuf {
    inst.root.join("loot")
}

/// Download the masterlist and prelude if the cached copies are missing, older than a day, or
/// `force` is set. A failed download keeps the cached copy and returns a warning.
pub fn update_masterlist(inst: &Instance, force: bool) -> Result<Vec<String>> {
    let repo = match (inst.config.game.lo_id(), inst.config.game.base()) {
        (loadorder::GameId::SkyrimVR, _) => "skyrimvr",
        (loadorder::GameId::Fallout4VR, _) => "fallout4vr",
        (_, Game::Fallout4) => "fallout4",
        _ => "skyrimse",
    };
    let files = [
        (format!("https://raw.githubusercontent.com/loot/{repo}/{BRANCH}/masterlist.yaml"), "masterlist.yaml"),
        (format!("https://raw.githubusercontent.com/loot/prelude/{BRANCH}/prelude.yaml"), "prelude.yaml"),
    ];
    let mut warnings = Vec::new();
    fs::create_dir_all(dir(inst))?;
    for (url, name) in files {
        let path = dir(inst).join(name);
        let fresh = fs::metadata(&path).and_then(|m| m.modified()).is_ok_and(|t| SystemTime::now().duration_since(t).unwrap_or_default() < refresh());
        if fresh && !force {
            continue;
        }
        if let Err(e) = download(&url, &path) {
            if !path.exists() {
                bail!("cannot download the LOOT {name}: {e:#}");
            }
            warnings.push(format!("couldn't refresh the LOOT {name} ({e:#}); using the cached copy"));
        }
    }
    Ok(warnings)
}

// ponytail: shells out to curl, which ships with Windows 10+ and every Linux distro, rather than
// adding an HTTP stack for two files. Swap in ureq if curl turns out to be missing somewhere.
fn download(url: &str, dest: &Path) -> Result<()> {
    let mut tmp = dest.as_os_str().to_owned();
    tmp.push(".part");
    let out = std::process::Command::new("curl")
        .args(["-fsSL", "--retry", "2", "--max-time", "120", "-o"])
        .arg(&tmp)
        .arg(url)
        .output()
        .context("cannot run curl")?;
    if !out.status.success() {
        let _ = fs::remove_file(&tmp);
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    fs::rename(&tmp, dest)?;
    Ok(())
}

/// A libloot handle that sees the profile rather than the deployed Data folder: enabled mods are
/// additional data paths, highest priority first, so `file()` conditions match the profile.
/// Active-plugin conditions still come from plugins.txt, which deploy writes.
fn handle(inst: &Instance, profile: &Profile) -> Result<(libloot::Game, Vec<String>)> {
    let c = &inst.config;
    let game_type = match (c.game.lo_id(), c.game.base()) {
        (loadorder::GameId::SkyrimVR, _) => GameType::SkyrimVR,
        (loadorder::GameId::Fallout4VR, _) => GameType::Fallout4VR,
        (_, Game::Fallout4) => GameType::Fallout4,
        _ => GameType::SkyrimSE,
    };
    let mut game = libloot::Game::with_local_path(game_type, &c.game_path, &c.local_path)?;
    let mut dirs = vec![inst.mod_dir(OVERWRITE)];
    dirs.extend(profile.mods.iter().rev().filter(|m| m.enabled).map(|m| inst.mod_dir(&m.id)));
    game.set_additional_data_paths(dirs.into_iter().filter(|d| d.is_dir()).collect())?;
    game.load_current_load_order_state()?;

    let mut warnings = Vec::new();
    let (masterlist, prelude, userlist) = (dir(inst).join("masterlist.yaml"), dir(inst).join("prelude.yaml"), dir(inst).join("userlist.yaml"));
    {
        let db = game.database();
        let mut db = db.write().map_err(|_| anyhow::anyhow!("LOOT database lock poisoned"))?;
        if masterlist.is_file() {
            if prelude.is_file() {
                db.load_masterlist_with_prelude(&masterlist, &prelude)?;
            } else {
                db.load_masterlist(&masterlist)?;
            }
        } else {
            warnings.push("no LOOT masterlist; sorting uses only plugin masters and record overlaps".into());
        }
        if userlist.is_file() {
            db.load_userlist(&userlist)?;
        }
    }
    Ok((game, warnings))
}

/// Load every readable plugin into libloot. Returns the plugins in the current load order.
fn load<'a>(game: &mut libloot::Game, infos: &'a [PluginInfo], warnings: &mut Vec<String>) -> Result<Vec<&'a PluginInfo>> {
    let usable: Vec<&PluginInfo> = infos.iter().filter(|p| p.error.is_none()).collect();
    for p in infos.iter().filter(|p| p.error.is_some()) {
        warnings.push(format!("{} can't be read, so LOOT skips it", p.name));
    }
    let paths: Vec<&Path> = usable.iter().map(|p| p.path.as_path()).collect();
    game.load_plugins(&paths)?;
    Ok(usable)
}

/// Put only the `new` plugins where LOOT's sort would, each right after the nearest plugin LOOT puts
/// before it; every other plugin keeps the user's order. Saved to the profile (undoable).
pub fn place(inst: &Instance, new: &[String]) -> Result<SortReport> {
    let mut r = sort(inst, true)?;
    let mut moved = vec![];
    inst.edit("place new plugins (LOOT)", |p| {
        let before = p.plugins.clone();
        p.plugins = placed(&before, &r.order, new);
        let at = |list: &[PluginEntry], n: &str| list.iter().position(|e| e.name.eq_ignore_ascii_case(n));
        moved = new.iter().filter_map(|n| Some(Moved { name: n.clone(), from: at(&before, n)?, to: at(&p.plugins, n)? })).filter(|m| m.from != m.to).collect();
        Ok(())
    })?;
    r.order = inst.profile()?.plugins.into_iter().map(|e| e.name).collect();
    (r.dry_run, r.moved) = (false, moved);
    Ok(r)
}

fn placed(plugins: &[PluginEntry], loot: &[String], new: &[String]) -> Vec<PluginEntry> {
    let is_new = |n: &str| new.iter().any(|x| x.eq_ignore_ascii_case(n));
    let mut out: Vec<PluginEntry> = plugins.iter().filter(|e| !is_new(&e.name)).cloned().collect();
    for (i, name) in loot.iter().enumerate().filter(|(_, n)| is_new(n)) {
        let Some(e) = plugins.iter().find(|e| e.name.eq_ignore_ascii_case(name)) else { continue };
        let at = loot[..i].iter().rev().find_map(|b| out.iter().position(|x| x.name.eq_ignore_ascii_case(b))).map_or(0, |j| j + 1);
        out.insert(at, e.clone());
    }
    // New plugins LOOT didn't see stay where they were, at the end.
    let unseen: Vec<PluginEntry> = plugins.iter().filter(|e| is_new(&e.name) && !loot.iter().any(|n| n.eq_ignore_ascii_case(&e.name))).cloned().collect();
    out.extend(unseen);
    out
}

/// Sort plugins with LOOT. A dry run returns the new order and what moves; otherwise the order
/// is saved to the profile (undoable) and reaches plugins.txt on the next deploy.
pub fn sort(inst: &Instance, dry_run: bool) -> Result<SortReport> {
    crate::progress::report("sorting plugins with LOOT", 0, 0);
    let mut warnings = update_masterlist(inst, false).unwrap_or_else(|e| vec![format!("{e:#}")]);
    let profile = inst.profile()?;
    let infos = plugins::list(inst, &profile)?;
    let (mut game, w) = handle(inst, &profile)?;
    warnings.extend(w);
    let current = load(&mut game, &infos, &mut warnings)?;
    let names: Vec<&str> = current.iter().map(|p| p.name.as_str()).collect();
    let order = game.sort_plugins(&names)?;

    let deployed_active = current.iter().filter(|p| game.is_plugin_active(&p.name) != p.enabled).count();
    if deployed_active > 0 {
        warnings.push(format!("plugins.txt differs from the profile for {deployed_active} plugins; deploy, then sort again, so LOOT's conditions on active plugins see the profile"));
    }
    let moved: Vec<Moved> = order
        .iter()
        .enumerate()
        .filter_map(|(to, name)| {
            let from = names.iter().position(|n| n.eq_ignore_ascii_case(name))?;
            (from != to).then(|| Moved { name: name.clone(), from, to })
        })
        .collect();

    if !dry_run && !moved.is_empty() {
        inst.edit("sort plugins (LOOT)", |p| {
            let find = |name: &str| p.plugins.iter().find(|e| e.name.eq_ignore_ascii_case(name)).cloned();
            let mut sorted: Vec<PluginEntry> = order
                .iter()
                .filter_map(|name| {
                    let info = current.iter().find(|i| i.name.eq_ignore_ascii_case(name))?;
                    // Base-game masters always load first; the profile doesn't list them.
                    if info.implicit {
                        return None;
                    }
                    Some(find(name).unwrap_or(PluginEntry { name: name.clone(), enabled: info.enabled }))
                })
                .collect();
            // Entries LOOT didn't see (their mod is gone or disabled) keep their order at the end.
            sorted.extend(p.plugins.iter().filter(|e| !order.iter().any(|n| n.eq_ignore_ascii_case(&e.name))).cloned());
            p.plugins = sorted;
            Ok(())
        })?;
    }
    Ok(SortReport { dry_run, order, moved, warnings })
}

/// LOOT's warnings and errors for enabled plugins, plus dirty-plugin reports, from the cached
/// masterlist. Nothing is downloaded; without a masterlist there is nothing to report.
pub fn issues(inst: &Instance) -> Result<Vec<Issue>> {
    if !dir(inst).join("masterlist.yaml").is_file() {
        return Ok(vec![]);
    }
    let profile = inst.profile()?;
    let infos = plugins::list(inst, &profile)?;
    let (mut game, _) = handle(inst, &profile)?;
    let current = load(&mut game, &infos, &mut Vec::new())?;
    let db = game.database();
    let db = db.read().map_err(|_| anyhow::anyhow!("LOOT database lock poisoned"))?;
    let mut issues = Vec::new();
    let cfg = crate::settings::get().loot;
    let text = |m: &libloot::metadata::Message| select_message_content(m.content(), &cfg.language).map(|c| c.text().to_owned()).unwrap_or_default();
    let severity = |m: &libloot::metadata::Message| match m.message_type() {
        MessageType::Error => Some("error"),
        MessageType::Warn => Some("warning"),
        MessageType::Say if cfg.show_info => Some("info"),
        _ => None,
    };
    for m in db.general_messages(MergeMode::WithUserMetadata, EvalMode::Evaluate)? {
        if let Some(s) = severity(&m) {
            issues.push(Issue { severity: s, kind: "loot_message", subject: "LOOT".into(), message: text(&m) });
        }
    }
    for p in current.iter().filter(|p| p.enabled) {
        let Some(meta) = db.plugin_metadata(&p.name, MergeMode::WithUserMetadata, EvalMode::Evaluate)? else { continue };
        for m in meta.messages() {
            if let Some(s) = severity(m) {
                issues.push(Issue { severity: s, kind: "loot_message", subject: p.name.clone(), message: text(m) });
            }
        }
        for d in meta.dirty_info() {
            issues.push(Issue {
                severity: "warning",
                kind: "dirty_plugin",
                subject: p.name.clone(),
                message: format!(
                    "{} ITMs, {} deleted references, {} deleted navmeshes; clean it with xEdit QuickAutoClean (run_tool xedit-qac with plugin={})",
                    d.itm_count(),
                    d.deleted_reference_count(),
                    d.deleted_navmesh_count(),
                    p.name
                ),
            });
        }
    }
    Ok(issues)
}

/// A user rule for one plugin, kept in `loot/userlist.yaml` in LOOT's own format, so LOOT itself
/// reads the same rules.
#[derive(Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct Rule {
    pub plugin: String,
    /// Plugins this one must load after.
    #[serde(default)]
    pub after: Vec<String>,
    /// A LOOT group from the masterlist, e.g. "Late Loaders".
    #[serde(default)]
    pub group: Option<String>,
}

fn rule_of(m: &PluginMetadata) -> Rule {
    Rule { plugin: m.name().to_owned(), after: m.load_after_files().iter().map(|f| f.name().as_str().to_owned()).collect(), group: m.group().map(str::to_owned) }
}

/// The user rules for the profile's plugins.
pub fn rules(inst: &Instance) -> Result<Vec<Rule>> {
    let profile = inst.profile()?;
    let (game, _) = handle(inst, &profile)?;
    let db = game.database();
    let db = db.read().map_err(|_| anyhow::anyhow!("LOOT database lock poisoned"))?;
    let mut out = Vec::new();
    for p in plugins::list(inst, &profile)? {
        if let Some(m) = db.plugin_user_metadata(&p.name, EvalMode::DoNotEvaluate)? {
            out.push(rule_of(&m));
        }
    }
    Ok(out)
}

/// LOOT's group names (masterlist and userlist), for picking a plugin's group. Empty until a
/// masterlist is cached.
pub fn groups(inst: &Instance) -> Result<Vec<String>> {
    if !dir(inst).join("masterlist.yaml").is_file() {
        return Ok(vec![]);
    }
    let (game, _) = handle(inst, &inst.profile()?)?;
    let db = game.database();
    let db = db.read().map_err(|_| anyhow::anyhow!("LOOT database lock poisoned"))?;
    Ok(db.groups(MergeMode::WithUserMetadata).iter().map(|g| g.name().to_owned()).collect())
}

/// Replace a plugin's load-after list and group in the userlist; an empty rule removes them.
/// Other user metadata for the plugin (messages, tags) stays. Returns the previous rule, which
/// is how to undo this: the userlist isn't part of the profile history.
pub fn set_rule(inst: &Instance, rule: &Rule) -> Result<Option<Rule>> {
    let profile = inst.profile()?;
    let (game, _) = handle(inst, &profile)?;
    let db = game.database();
    let mut db = db.write().map_err(|_| anyhow::anyhow!("LOOT database lock poisoned"))?;
    if let Some(g) = &rule.group
        && dir(inst).join("masterlist.yaml").is_file()
        && !db.groups(MergeMode::WithUserMetadata).iter().any(|x| x.name() == g)
    {
        bail!("LOOT has no group '{g}'; groups come from the masterlist");
    }
    let old = db.plugin_user_metadata(&rule.plugin, EvalMode::DoNotEvaluate)?;
    let mut m = match &old {
        Some(m) => m.clone(),
        None => PluginMetadata::new(&rule.plugin)?,
    };
    m.set_load_after_files(rule.after.iter().map(|a| File::new(a.clone())).collect());
    match &rule.group {
        Some(g) => m.set_group(g.clone()),
        None => m.unset_group(),
    }
    db.discard_plugin_user_metadata(&rule.plugin);
    if !m.has_name_only() {
        db.set_plugin_user_metadata(m);
    }
    fs::create_dir_all(dir(inst))?;
    let mut opts = MetadataWriteOptions::new();
    opts.set_truncate(true);
    db.write_user_metadata(&dir(inst).join("userlist.yaml"), &opts)?;
    Ok(old.as_ref().map(rule_of))
}

#[cfg(test)]
mod place_tests {
    use super::*;

    #[test]
    fn only_new_plugins_move() {
        let e = |n: &str| PluginEntry { name: n.into(), enabled: true };
        let names = |v: Vec<PluginEntry>| v.into_iter().map(|e| e.name).collect::<Vec<_>>();
        // The user keeps C before A on purpose; LOOT would swap them and puts New after A.
        let mine = vec![e("C.esp"), e("A.esp"), e("B.esp"), e("New.esp")];
        let loot: Vec<String> = ["A.esp", "New.esp", "B.esp", "C.esp"].map(String::from).to_vec();
        assert_eq!(names(placed(&mine, &loot, &["new.esp".into()])), ["C.esp", "A.esp", "New.esp", "B.esp"]);
        // First in LOOT's order: to the top. Unseen by LOOT: left at the end.
        let loot: Vec<String> = ["New.esp", "A.esp", "B.esp", "C.esp"].map(String::from).to_vec();
        assert_eq!(names(placed(&mine, &loot, &["New.esp".into()])), ["New.esp", "C.esp", "A.esp", "B.esp"]);
        assert_eq!(names(placed(&mine, &loot[1..], &["New.esp".into()])), ["C.esp", "A.esp", "B.esp", "New.esp"]);
    }
}
