//! LOOT sorting and messages through libloot (SPEC.md "LOOT"). The masterlist and prelude are
//! cached in `<instance>/loot/` and refreshed from LOOT's GitHub repositories at most once a day;
//! `loot/userlist.yaml`, if present, adds the user's own rules.
use crate::health::Issue;
use crate::instance::OVERWRITE;
use crate::plugins::PluginInfo;
use crate::{Game, Instance, PluginEntry, Profile, plugins};
use anyhow::{Context, Result, bail};
use libloot::metadata::{MessageType, select_message_content};
use libloot::{EvalMode, GameType, MergeMode};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Masterlist branch matching libloot 0.29's metadata syntax.
const BRANCH: &str = "v0.29";
const REFRESH: Duration = Duration::from_secs(24 * 60 * 60);

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
    let repo = match inst.config.game {
        Game::SkyrimSe => "skyrimse",
        Game::Fallout4 => "fallout4",
    };
    let files = [
        (format!("https://raw.githubusercontent.com/loot/{repo}/{BRANCH}/masterlist.yaml"), "masterlist.yaml"),
        (format!("https://raw.githubusercontent.com/loot/prelude/{BRANCH}/prelude.yaml"), "prelude.yaml"),
    ];
    let mut warnings = Vec::new();
    fs::create_dir_all(dir(inst))?;
    for (url, name) in files {
        let path = dir(inst).join(name);
        let fresh = fs::metadata(&path).and_then(|m| m.modified()).is_ok_and(|t| SystemTime::now().duration_since(t).unwrap_or_default() < REFRESH);
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
    let game_type = match c.game {
        Game::SkyrimSe => GameType::SkyrimSE,
        Game::Fallout4 => GameType::Fallout4,
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

/// Sort plugins with LOOT. A dry run returns the new order and what moves; otherwise the order
/// is saved to the profile (undoable) and reaches plugins.txt on the next deploy.
pub fn sort(inst: &Instance, dry_run: bool) -> Result<SortReport> {
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
    let text = |m: &libloot::metadata::Message| select_message_content(m.content(), "en").map(|c| c.text().to_owned()).unwrap_or_default();
    let severity = |m: &libloot::metadata::Message| match m.message_type() {
        MessageType::Error => Some("error"),
        MessageType::Warn => Some("warning"),
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
