//! Bring in a setup from Mod Organizer 2 or Vortex: mod folders (hard-linked when they share a
//! volume, copied otherwise), names, Nexus ids, FOMOD choices, notes, mod order and enabled state,
//! separators, plugin order and locks, and Vortex's mod rules. The formats are written up in
//! `docs/internals/mo2.md` and `docs/internals/vortex.md` (§9.4 in each).
//!
//! Mods go in under their own names; one that is already installed under that id is left as it
//! is, so importing again only adds what is new. The profile change is one undoable step.
use crate::install::sanitize;
use crate::instance::{ModEntry, ModMeta, NexusRef, OVERWRITE, PluginEntry, index_dir};
use crate::rules::ModRule;
use crate::{Game, Instance};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Serialize, Debug, Default)]
pub struct ImportReport {
    pub dry_run: bool,
    /// The MO2 or Vortex profile read.
    pub profile: String,
    /// Mods brought in (or that would be), lowest priority first.
    pub mods: Vec<String>,
    pub separators: usize,
    /// Mods already installed under the same id, left as they are.
    pub existing: Vec<String>,
    pub plugins: usize,
    pub rules: usize,
    /// Files hard-linked (sharing the source's disk space) and copied.
    pub linked: usize,
    pub copied: usize,
    pub warnings: Vec<String>,
}

/// One row of the imported list: a mod folder to bring in, or a separator (`from` is None).
struct Incoming {
    id: String,
    enabled: bool,
    from: Option<(PathBuf, ModMeta, bool)>, // folder, metadata, everything belongs in root/
}

/// MO2 instances and Vortex state backups found in their usual places.
#[derive(Serialize, Debug, Default)]
pub struct Sources {
    /// MO2 instance folders (holding ModOrganizer.ini).
    pub mo2: Vec<PathBuf>,
    /// Vortex's live state folder (state.v2) first, then its state backups, newest first.
    pub vortex: Vec<PathBuf>,
}

pub fn find_sources() -> Sources {
    let mut s = Sources::default();
    if let Some(local) = dirs::data_local_dir()
        && let Ok(rd) = fs::read_dir(local.join("ModOrganizer"))
    {
        s.mo2 = rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.join("ModOrganizer.ini").is_file()).collect();
        s.mo2.sort();
    }
    if let Some(roaming) = dirs::config_dir() {
        let mut found: Vec<(std::time::SystemTime, PathBuf)> = [&["temp", "state_backups_full"][..], &["temp", "state_backups"], &["state_backup"]]
            .iter()
            .filter_map(|d| fs::read_dir(d.iter().fold(roaming.join("Vortex"), |p, s| p.join(s))).ok())
            .flatten()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .filter_map(|p| Some((fs::metadata(&p).ok()?.modified().ok()?, p)))
            .collect();
        found.sort_by_key(|f| std::cmp::Reverse(f.0));
        s.vortex = vortex_live_state().into_iter().chain(found.into_iter().map(|(_, p)| p)).collect();
    }
    s
}

// ---- MO2 ----------------------------------------------------------------------------------------

/// Import an MO2 instance's profile (default: the one MO2 has selected) into the active profile.
pub fn import_mo2(inst: &Instance, dir: &Path, profile: Option<&str>, dry_run: bool) -> Result<ImportReport> {
    let ini = qt_ini(&fs::read_to_string(dir.join("ModOrganizer.ini")).with_context(|| format!("{} has no ModOrganizer.ini; pass the MO2 instance folder", dir.display()))?);
    let get = |s: &str, k: &str| ini.get(s).and_then(|m| m.get(k)).filter(|v| !v.is_empty()).cloned();
    let game = match get("General", "gameName").as_deref() {
        Some("Skyrim Special Edition") => Game::SkyrimSe,
        Some("Fallout 4") => Game::Fallout4,
        other => bail!("this MO2 instance manages {}, which rmm doesn't support", other.unwrap_or("an unknown game")),
    };
    if game != inst.config.game {
        bail!("this MO2 instance is for {game:?}, the rmm instance for {:?}", inst.config.game);
    }
    let base = get("Settings", "base_directory").map(PathBuf::from).unwrap_or_else(|| dir.to_path_buf());
    let sub = |key: &str, default: &str| {
        get("Settings", key).map(|v| PathBuf::from(v.replace("%BASE_DIR%", &base.to_string_lossy()))).unwrap_or_else(|| base.join(default))
    };
    let (mods_dir, overwrite_dir, downloads) = (sub("mod_directory", "mods"), sub("overwrite_directory", "overwrite"), sub("download_directory", "downloads"));
    let name = profile.map(str::to_owned).or_else(|| get("General", "selected_profile")).context("no profile given, and ModOrganizer.ini has none selected")?;
    let pdir = sub("profiles_directory", "profiles").join(&name);
    let modlist = fs::read_to_string(pdir.join("modlist.txt")).with_context(|| format!("MO2 profile '{name}' has no modlist.txt"))?;
    let mut r = ImportReport { dry_run, profile: name.clone(), ..Default::default() };

    // modlist.txt names the highest priority first; rmm's list is last-wins.
    let mut seen = HashSet::new();
    let mut listed: Vec<(String, bool)> = Vec::new();
    for line in modlist.lines() {
        let l = line.trim_end_matches('\r');
        if l.is_empty() || l.starts_with('#') || l.starts_with('*') {
            continue; // comments, and DLC / Creation Club / unmanaged entries (not folders)
        }
        let (enabled, n) = match l.as_bytes()[0] {
            b'+' => (true, &l[1..]),
            b'-' => (false, &l[1..]),
            _ => (true, l),
        };
        if n != "Overwrite" && seen.insert(n.to_string()) {
            listed.push((n.to_string(), enabled));
        }
    }
    listed.reverse();
    // MO2 puts folders the list doesn't name on top; they come last here, disabled.
    if let Ok(rd) = fs::read_dir(&mods_dir) {
        let mut extra: Vec<String> =
            rd.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()).map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| !seen.contains(n)).collect();
        extra.sort();
        listed.extend(extra.into_iter().map(|n| (n, false)));
    }

    // MO2's own categories: `id|name|…` per line (the Nexus ids and parent that follow vary by version).
    let categories: HashMap<i64, String> = fs::read_to_string(dir.join("categories.dat"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut f = l.split('|');
            Some((f.next()?.trim().parse().ok()?, f.next()?.trim().to_string()))
        })
        .collect();
    let mut incoming = Vec::new();
    for (n, enabled) in listed {
        if let Some(sep) = n.strip_suffix("_separator") {
            incoming.push(Incoming { id: format!("sep:{}", sanitize(sep)), enabled: false, from: None });
            continue;
        }
        if n.trim_end_matches(|c: char| c.is_ascii_digit()).to_lowercase().ends_with("backup") {
            continue; // MO2 keeps backups of reinstalled mods as always-disabled folders
        }
        let folder = mods_dir.join(&n);
        if !folder.is_dir() {
            r.warnings.push(format!("{n}: listed in modlist.txt, but its folder is missing"));
            continue;
        }
        let meta = mo2_meta(&folder, &n, &downloads, &categories);
        incoming.push(Incoming { id: sanitize(&n), enabled, from: Some((folder, meta, false)) });
    }

    let plugins = plugin_lists(&pdir.join("plugins.txt"), &pdir.join("loadorder.txt"));
    let locks: BTreeMap<String, usize> = fs::read_to_string(pdir.join("lockedorder.txt"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (n, i) = l.rsplit_once('|')?;
            Some((n.trim().to_string(), i.trim().parse().ok()?))
        })
        .collect();
    if !dry_run && overwrite_dir.is_dir() {
        let (l, c) = link_tree(&overwrite_dir, &inst.mod_dir(OVERWRITE), false, true)?;
        if l + c > 0 {
            r.warnings.push(format!("{} files from MO2's overwrite folder are now in overwrite/", l + c));
        }
    }
    bring_in(inst, &format!("import MO2 profile {name}"), &incoming, plugins, locks, &mut r)?;
    Ok(r)
}

fn mo2_meta(folder: &Path, name: &str, downloads: &Path, categories: &HashMap<i64, String>) -> ModMeta {
    let ini = qt_ini(&fs::read_to_string(folder.join("meta.ini")).unwrap_or_default());
    let general = ini.get("General");
    let get = |k: &str| general.and_then(|m| m.get(k)).filter(|v| !v.is_empty()).cloned();
    let file_id = ini.get("installedFiles").and_then(|m| m.iter().find(|(k, _)| k.ends_with("fileid")).and_then(|(_, v)| v.parse().ok()));
    // `category="3,12,"`: MO2 category ids, -1 or 0 for none.
    let cats: Vec<String> = get("category").unwrap_or_default().split(',').filter_map(|c| categories.get(&c.trim().parse().ok()?).cloned()).collect();
    ModMeta {
        name: name.into(),
        source: get("installationFile").map(|f| if Path::new(&f).is_absolute() { f } else { downloads.join(f).display().to_string() }),
        notes: get("comments"),
        categories: (!cats.is_empty()).then_some(cats),
        nexus: get("modid").and_then(|v| v.parse().ok()).filter(|&n: &u64| n > 0).map(|mod_id| NexusRef {
            mod_id,
            file_id,
            version: get("version"),
            category: get("nexusCategory").and_then(|c| c.parse().ok()).filter(|&c: &u64| c > 0),
        }),
        ..Default::default()
    }
}

/// A Qt `QSettings` INI: section -> key -> value. `@ByteArray(…)` is unwrapped and `\\` in
/// values unescaped; nested keys keep their `\` (`1\fileid`).
fn qt_ini(text: &str) -> HashMap<String, HashMap<String, String>> {
    let mut out: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut section = "General".to_string();
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') && l.ends_with(']') {
            section = l[1..l.len() - 1].to_string();
            continue;
        }
        let Some((k, v)) = l.split_once('=') else { continue };
        let v = v.trim();
        let v = v.strip_prefix("@ByteArray(").and_then(|x| x.strip_suffix(')')).unwrap_or(v);
        out.entry(section.clone()).or_default().insert(k.trim().to_string(), v.trim_matches('"').replace("\\\\", "\\"));
    }
    out
}

// ---- Vortex -------------------------------------------------------------------------------------

/// Vortex's live state: `%APPDATA%\Vortex\state.v2`, a LevelDB database.
pub fn vortex_live_state() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("Vortex").join("state.v2")).filter(|p| p.join("CURRENT").is_file())
}

/// Read Vortex's LevelDB state into the shape of its JSON backups. Keys are paths joined by
/// `###` (`persistent###mods###skyrimse###<id>###attributes###version`) and values are JSON.
/// Opening a LevelDB writes to it, so this reads a copy: Vortex's own files are never touched.
pub fn read_vortex_db(dir: &Path, scratch: &Path) -> Result<Value> {
    if !dir.join("CURRENT").is_file() {
        bail!("{} isn't a LevelDB folder (no CURRENT file)", dir.display());
    }
    let _ = fs::remove_dir_all(scratch);
    fs::create_dir_all(scratch)?;
    for e in fs::read_dir(dir)? {
        let e = e?;
        // LOCK belongs to a running Vortex; the copy needs its own.
        if e.file_type()?.is_file() && e.file_name() != "LOCK" {
            fs::copy(e.path(), scratch.join(e.file_name()))?;
        }
    }
    let read = || -> Result<Value> {
        let opts = rusty_leveldb::Options { create_if_missing: false, ..Default::default() };
        let mut db = rusty_leveldb::DB::open(scratch, opts).map_err(|e| anyhow::anyhow!("cannot read Vortex's database: {e}"))?;
        let mut it = db.new_iter().map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut root = Value::Object(Default::default());
        while let Some((k, v)) = rusty_leveldb::LdbIterator::next(&mut it) {
            let key = String::from_utf8_lossy(&k).into_owned();
            let value = serde_json::from_slice(&v).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&v).into_owned()));
            let mut node = &mut root;
            let parts: Vec<&str> = key.split("###").collect();
            for (i, part) in parts.iter().enumerate() {
                if !node.is_object() {
                    *node = Value::Object(Default::default());
                }
                let map = node.as_object_mut().unwrap();
                if i + 1 == parts.len() {
                    map.insert(part.to_string(), value);
                    break;
                }
                node = map.entry(part.to_string()).or_insert_with(|| Value::Object(Default::default()));
            }
        }
        Ok(root)
    };
    let r = read();
    let _ = fs::remove_dir_all(scratch);
    r
}

/// Import a Vortex profile (default: the last active one) from Vortex's live state (the
/// `state.v2` folder; close Vortex first so it is current) or a state backup: the JSON files in
/// `%APPDATA%\Vortex\temp\state_backups_full`, or one made with Settings > Workarounds > Create
/// Backup. `staging` defaults to the folder the state names. Vortex keeps no mod order, so mods
/// go in by install time and then by its rules, which become rmm mod rules.
pub fn import_vortex(inst: &Instance, backup: &Path, staging: Option<&Path>, profile: Option<&str>, dry_run: bool) -> Result<ImportReport> {
    let state: Value = if backup.is_dir() {
        read_vortex_db(backup, &inst.root.join(".staging").join("vortex-state"))?
    } else {
        serde_json::from_str(&fs::read_to_string(backup).with_context(|| format!("cannot read {}", backup.display()))?)
            .with_context(|| format!("{} is not JSON", backup.display()))?
    };
    if state.get("persistent").is_none() {
        bail!("{} isn't Vortex's state (it has no 'persistent' section)", backup.display());
    }
    let gid = inst.config.game.vortex_id();
    let Some(mods) = state["persistent"]["mods"][gid].as_object() else {
        let games: Vec<&String> = state["persistent"]["mods"].as_object().map(|m| m.keys().collect()).unwrap_or_default();
        bail!("Vortex has no {gid} mods (it has mods for: {})", if games.is_empty() { "no game".into() } else { games.iter().map(|g| g.as_str()).collect::<Vec<_>>().join(", ") })
    };
    let userdata = dirs::config_dir().unwrap_or_default().join("Vortex");
    let resolve = |pattern: &str| {
        let mut p = pattern.to_string();
        for (k, v) in [("{USERDATA}", userdata.to_string_lossy().into_owned()), ("{userdata}", userdata.to_string_lossy().into_owned()), ("{GAME}", gid.into()), ("{game}", gid.into())] {
            p = p.replace(k, &v);
        }
        PathBuf::from(p)
    };
    let staging = staging.map(Path::to_path_buf).unwrap_or_else(|| resolve(state["settings"]["mods"]["installPath"][gid].as_str().unwrap_or("{USERDATA}/{GAME}/mods")));
    let downloads = resolve(state["settings"]["downloads"]["path"].as_str().unwrap_or("{USERDATA}/downloads")).join(gid);
    let last = state["settings"]["profiles"]["lastActiveProfile"][gid].as_str();
    let Some((pid, prof)) = state["persistent"]["profiles"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, p)| p["gameId"] == gid)
        .find(|(id, p)| match profile.or(last) {
            Some(w) => id.as_str() == w || p["name"].as_str() == Some(w),
            None => true,
        })
    else {
        bail!("no {gid} profile{} in the backup", profile.map(|p| format!(" named '{p}'")).unwrap_or_default());
    };
    let mut r = ImportReport { dry_run, profile: prof["name"].as_str().unwrap_or(pid).to_string(), ..Default::default() };

    let mut list: Vec<(&String, &Value)> = mods.iter().filter(|(_, m)| m["state"] == "installed" && m["type"] != "collection").collect();
    list.sort_by(|a, b| (a.1["attributes"]["installTime"].as_str(), a.0).cmp(&(b.1["attributes"]["installTime"].as_str(), b.0)));
    let num = |v: &Value| v.as_u64().or_else(|| v.as_str()?.parse().ok());
    let mut ids: HashMap<&str, String> = HashMap::new();
    let mut used = HashSet::new();
    let mut incoming = Vec::new();
    for (vid, m) in &list {
        let a = &m["attributes"];
        let name = ["customFileName", "logicalFileName", "modName", "name"].iter().find_map(|k| a[*k].as_str().filter(|s| !s.trim().is_empty())).unwrap_or(vid);
        let folder = staging.join(m["installationPath"].as_str().unwrap_or(vid));
        if !folder.is_dir() {
            r.warnings.push(format!("{name}: its staging folder {} is missing", folder.display()));
            continue;
        }
        let base = sanitize(name);
        let id = (1..).map(|n| if n == 1 { base.clone() } else { format!("{base} ({n})") }).find(|id| used.insert(id.clone())).unwrap();
        let meta = ModMeta {
            name: name.into(),
            source: a["fileName"].as_str().map(|f| downloads.join(f).display().to_string()),
            fomod: serde_json::from_value::<crate::fomod::Choices>(a["installerChoices"].clone()).ok().filter(|c| !c.options.is_empty()),
            notes: a["notes"].as_str().filter(|s| !s.is_empty()).map(str::to_owned),
            nexus: num(&a["modId"]).filter(|&n| n > 0).map(|mod_id| NexusRef { mod_id, file_id: num(&a["fileId"]), version: a["version"].as_str().map(str::to_owned), category: None }),
            ..Default::default()
        };
        let root = matches!(m["type"].as_str(), Some("dinput" | "enb"));
        let enabled = prof["modState"][vid.as_str()]["enabled"].as_bool().unwrap_or(false);
        ids.insert(vid.as_str(), id.clone());
        incoming.push(Incoming { id, enabled, from: Some((folder, meta, root)) });
    }

    // Rules point at a mod by id, or by archive hash or logical file name.
    let find = |rf: &Value| -> Option<String> {
        for k in ["id", "idHint"] {
            if let Some(id) = rf[k].as_str().and_then(|v| ids.get(v)) {
                return Some(id.clone());
            }
        }
        let md5 = rf["fileMD5"].as_str().or(rf["md5Hint"].as_str());
        let lfn = rf["logicalFileName"].as_str();
        list.iter()
            .find(|(_, m)| (md5.is_some() && m["attributes"]["fileMD5"].as_str() == md5) || (lfn.is_some() && m["attributes"]["logicalFileName"].as_str() == lfn))
            .and_then(|(vid, _)| ids.get(vid.as_str()).cloned())
    };
    let mut rules: BTreeMap<String, ModRule> = BTreeMap::new();
    for (vid, m) in &list {
        let Some(me) = ids.get(vid.as_str()).cloned() else { continue };
        for x in m["rules"].as_array().into_iter().flatten() {
            if x["ignored"].as_bool() == Some(true) {
                continue;
            }
            let Some(other) = find(&x["reference"]).filter(|o| *o != me) else { continue };
            // "A before B" is B's "after A".
            let (target, kind, item) = match x["type"].as_str() {
                Some("after") => (me.clone(), 0, other),
                Some("before") => (other, 0, me.clone()),
                Some("requires") => (me.clone(), 1, other),
                Some("conflicts") => (me.clone(), 2, other),
                _ => continue,
            };
            let rl = rules.entry(target.clone()).or_insert_with(|| ModRule { id: target, ..Default::default() });
            match kind {
                0 => rl.after.push(item),
                1 => rl.requires.push(item),
                _ => rl.incompatible.push(item),
            }
        }
    }
    r.rules = rules.len();

    // Plugins: the live files for the profile Vortex has active, its profile copy otherwise.
    let active = state["settings"]["profiles"]["activeProfileId"].as_str() == Some(pid.as_str()) || last == Some(pid.as_str());
    let pdir = if active { inst.config.local_path.clone() } else { userdata.join(gid).join("profiles").join(pid) };
    let plugins = plugin_lists(&pdir.join("plugins.txt"), &pdir.join("loadorder.txt"));
    bring_in(inst, &format!("import Vortex profile {}", r.profile), &incoming, plugins, BTreeMap::new(), &mut r)?;
    if dry_run {
        return Ok(r);
    }
    for (_, rl) in rules {
        let old = crate::rules::rules(inst)?.into_iter().find(|x| x.id == rl.id).unwrap_or_default();
        let merged = ModRule {
            id: rl.id.clone(),
            after: [old.after, rl.after].concat(),
            requires: [old.requires, rl.requires].concat(),
            incompatible: [old.incompatible, rl.incompatible].concat(),
        };
        if let Err(e) = crate::rules::set_rule(inst, &merged) {
            r.warnings.push(format!("rule for {}: {e:#}", rl.id));
        }
    }
    let sorted = crate::rules::sort(inst, false)?;
    if !sorted.cycles.is_empty() {
        r.warnings.push(format!("{} rule pairs contradict each other and were left out of the order", sorted.cycles.len()));
    }
    let userlist = userdata.join(gid).join("userlist.yaml");
    let ours = inst.root.join("loot").join("userlist.yaml");
    if userlist.is_file() && !ours.exists() {
        fs::create_dir_all(ours.parent().unwrap())?;
        fs::copy(&userlist, &ours)?;
        r.warnings.push("Vortex's LOOT userlist is now rmm's".into());
    }
    Ok(r)
}

// ---- Shared -------------------------------------------------------------------------------------

/// plugins.txt (`*` = active; read as Latin-1, the way the game writes it) for the active state,
/// loadorder.txt for the order when there is one.
fn plugin_lists(plugins_txt: &Path, loadorder_txt: &Path) -> Vec<PluginEntry> {
    let lines = |t: String| t.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty() && !l.starts_with('#')).collect::<Vec<_>>();
    let plugins = lines(fs::read(plugins_txt).map(|b| b.iter().map(|&c| c as char).collect()).unwrap_or_default());
    let active: HashSet<String> = plugins.iter().filter_map(|l| l.strip_prefix('*')).map(str::to_lowercase).collect();
    let order = match fs::read_to_string(loadorder_txt) {
        Ok(t) => lines(t),
        Err(_) => plugins.iter().map(|l| l.trim_start_matches('*').to_string()).collect(),
    };
    order.into_iter().map(|name| PluginEntry { enabled: active.contains(&name.to_lowercase()), name }).collect()
}

/// Copy the mod folders in, then set the profile in one undoable step: the imported list first,
/// then mods the profile had that the import doesn't name.
fn bring_in(inst: &Instance, label: &str, incoming: &[Incoming], plugins: Vec<PluginEntry>, locks: BTreeMap<String, usize>, r: &mut ImportReport) -> Result<()> {
    for (i, m) in incoming.iter().enumerate() {
        crate::progress::report("importing mods", i, incoming.len());
        let Some((dir, meta, root)) = &m.from else {
            r.separators += 1;
            continue;
        };
        if inst.mod_dir(&m.id).exists() {
            r.existing.push(m.id.clone());
            continue;
        }
        r.mods.push(m.id.clone());
        if r.dry_run {
            continue;
        }
        let tmp = inst.root.join(".staging").join(format!("import-{}", m.id));
        let _ = fs::remove_dir_all(&tmp);
        let (l, c) = link_tree(dir, &tmp, *root, false)?;
        (r.linked, r.copied) = (r.linked + l, r.copied + c);
        fs::write(tmp.join(".rmm-index"), index_dir(&tmp)?.join("\n"))?;
        fs::write(tmp.join(crate::instance::MOD_META), toml::to_string_pretty(meta)?)?;
        fs::rename(&tmp, inst.mod_dir(&m.id))?;
    }
    r.plugins = plugins.len();
    if r.dry_run {
        return Ok(());
    }
    let named: HashSet<&str> = incoming.iter().map(|m| m.id.as_str()).collect();
    let listed: HashSet<String> = plugins.iter().map(|e| e.name.to_lowercase()).collect();
    inst.edit(label, |p| {
        let rest: Vec<ModEntry> = p.mods.iter().filter(|m| !named.contains(m.id.as_str())).cloned().collect();
        p.mods = incoming.iter().map(|m| ModEntry { id: m.id.clone(), enabled: m.enabled && m.from.is_some() }).chain(rest).collect();
        let others: Vec<PluginEntry> = p.plugins.iter().filter(|e| !listed.contains(&e.name.to_lowercase())).cloned().collect();
        p.plugins = plugins.into_iter().chain(others).collect();
        p.locks.extend(locks);
        Ok(())
    })
}

/// Hard-link (or, across volumes, copy) a mod folder's files into `dest`. MO2's `meta.ini` and
/// Vortex's marker files stay behind; a top `Root` folder (Root Builder) becomes `root/`, and
/// with `under_root` everything goes there. Returns (linked, copied).
fn link_tree(src: &Path, dest: &Path, under_root: bool, keep_existing: bool) -> Result<(usize, usize)> {
    let (mut linked, mut copied) = (0, 0);
    fs::create_dir_all(dest)?;
    for e in walkdir::WalkDir::new(src).min_depth(1) {
        let e = e?;
        if !e.file_type().is_file() {
            continue;
        }
        let rel = e.path().strip_prefix(src)?.to_string_lossy().replace('\\', "/");
        let low = rel.to_lowercase();
        if matches!(low.as_str(), "meta.ini" | "__folder_managed_by_vortex" | "rmm-mod.toml" | ".rmm-index") {
            continue;
        }
        let rel = match rel.split_once('/') {
            Some((top, rest)) if top.eq_ignore_ascii_case("root") => format!("root/{rest}"),
            _ => rel,
        };
        let to = dest.join(if under_root { format!("root/{rel}") } else { rel });
        if keep_existing && to.exists() {
            continue;
        }
        fs::create_dir_all(to.parent().unwrap())?;
        if fs::hard_link(e.path(), &to).is_ok() {
            linked += 1;
        } else {
            fs::copy(e.path(), &to)?;
            copied += 1;
        }
    }
    Ok((linked, copied))
}
