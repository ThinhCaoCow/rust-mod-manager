//! Nexus collections: install a collection's revision in one go. Nexus serves each revision as a
//! bundle whose `collection.json` (the format Vortex writes and reads) lists the mods by Nexus
//! file, with each one's FOMOD choices and whether it is optional, the rules between them and the
//! plugins. rmm downloads each file (Premium accounts), installs it with those choices, adds the
//! rules as mod rules, sorts the mods by them, applies the plugin list and sorts the plugins
//! with LOOT, as Vortex does. Mods already installed from the same Nexus file are kept and
//! turned on. Like Vortex it can install into a new profile, so the user's own setup stays as it
//! was. Mods of Vortex's "dinput" and "enb" types go to the game folder (see
//! `install::to_game_folder`).
//!
//! Free accounts can't download directly: the report lists the files that aren't in the
//! downloads folder yet (`to_download`), each with its download page, and the install goes ahead
//! once the user has downloaded them all from there ("Mod manager download").
//!
//! The profile remembers the revision and which mods are the collection's, so installing a
//! newer revision there is an update: a mod with a newer file is updated in place (or installed
//! beside the old one when another profile has that one on), mods the revision dropped are turned
//! off, and the user's own mods, and the collection's mods and plugins they turned off, stay as
//! they are.
use crate::Instance;
use crate::instance::NexusRef;
use crate::nexus::Client;
use crate::rules::ModRule;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

#[derive(Serialize, Debug, Default)]
pub struct Report {
    pub dry_run: bool,
    pub name: String,
    /// The new profile it went into, when it went into one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    pub revision: u64,
    /// The game versions the collection was made for.
    pub game_versions: Vec<String>,
    /// Mods to install (or installed), by name, in install order.
    pub install: Vec<String>,
    /// Bytes still to download.
    pub download_size: u64,
    /// Free accounts: the files to download from their pages first (Premium downloads them).
    pub to_download: Vec<Pending>,
    /// Mods already installed from the same Nexus file.
    pub installed: Vec<String>,
    /// Optional mods left out.
    pub optional: Vec<String>,
    /// Mods of Vortex's "dinput" or "enb" type: what isn't Data content goes to the game folder.
    pub game_folder: Vec<String>,
    /// Mods that couldn't be downloaded or installed, and why.
    pub failed: Vec<String>,
    /// The curator's notes on mods ("name: note"); each also becomes that mod's note.
    pub instructions: Vec<String>,
    /// The collection's tools, added as rmm tools (their names).
    pub tools: Vec<String>,
    /// Mods given rules, and plugins turned on or off by the collection.
    pub rules: usize,
    pub plugins: usize,
    pub warnings: Vec<String>,
    /// The revision the profile had: this is an update.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_from: Option<u64>,
    /// Mods the revision has a newer file of (by name): updated in place, or installed beside the
    /// old one when another profile has that on.
    pub updated: Vec<String>,
    /// Mods (ids) of the earlier revision this one dropped, or has a newer file of beside them:
    /// turned off here, still installed.
    pub turned_off: Vec<String>,
    /// The collection's mods (by name) the user had turned off: left off.
    pub kept_off: Vec<String>,
    /// Turned-on mods (ids) that aren't the collection's: left as they are.
    pub yours: Vec<String>,
}

/// A collection file a free account downloads from its page before the install.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Pending {
    pub name: String,
    pub mod_id: u64,
    pub file_id: u64,
    pub size: u64,
    /// The file's download page ("Mod manager download").
    pub page: String,
}

/// The collection a profile was installed from.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Installed {
    pub slug: String,
    pub name: String,
    pub revision: u64,
    /// Its optional mods came too.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
    /// Its mods, by id.
    #[serde(default)]
    pub mods: Vec<String>,
    /// Its plugin list (lowercase names), so an update changes only the plugins the curator did.
    #[serde(default)]
    pub plugins: BTreeMap<String, bool>,
}

/// Install a collection's revision (default: the newest) into the active profile, or into a new
/// empty profile `new_profile` that becomes the active one: its required mods, and with
/// `optional` its optional ones too. Into a profile that has an earlier revision it is an update
/// (see the module docs); an empty `link` means the profile's collection. A dry run downloads
/// only the bundle and says what would be installed.
#[allow(clippy::too_many_arguments)]
pub fn install(inst: &mut Instance, client: &Client, link: &str, number: Option<u64>, optional: bool, new_profile: Option<&str>, dry_run: bool, sort: bool) -> Result<Report> {
    if let Some(name) = new_profile {
        crate::instance::check_profile_name(name)?;
        if inst.find_profile(name).is_ok() {
            bail!("profile '{name}' already exists; pick another name, or switch to it and install without a new profile");
        }
    }
    let p = inst.profile()?;
    let slug = match link.trim() {
        "" => p.collection.as_ref().map(|x| x.slug.clone()).context("this profile wasn't installed from a collection; give the collection's link")?,
        l => slug(l)?,
    };
    let rev = revision(inst, client, &slug, number)?;
    let c = read(&bundle(inst, client, &slug, &rev)?)?;
    let domain = inst.config.game.nexus_domain();
    if let Some(d) = c["info"]["domainName"].as_str().filter(|d| *d != domain) {
        bail!("this collection is for {d}, the instance for {domain}");
    }
    let strings = |v: &Value| v.as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_owned)).collect::<Vec<_>>();
    let mut r = Report { dry_run, name: rev.name.clone(), profile: new_profile.map(str::to_owned), revision: rev.number, game_versions: strings(&c["info"]["gameVersions"]), ..Default::default() };
    if let Ok(v) = crate::launch::game_version(&inst.config.game_path, inst.config.game) {
        let have = v.map(|n| n.to_string()).join(".");
        if !r.game_versions.is_empty() && !r.game_versions.contains(&have) {
            r.warnings.push(format!("the collection was made for game version {}, and the game is {have}", r.game_versions.join(" or ")));
        }
    }

    // What the profile has of the collection already: an update. Not for a new profile.
    let prev = if new_profile.is_none() { previous(inst, &p, &slug, rev.number) } else { None };
    let optional = optional || prev.as_ref().is_some_and(|x| x.optional);
    r.update_from = prev.as_ref().map(|x| x.revision);
    let prev_mods: HashSet<&str> = prev.iter().flat_map(|x| x.mods.iter().map(String::as_str)).collect();
    let enabled: HashMap<&str, bool> = p.mods.iter().map(|m| (m.id.as_str(), m.enabled)).collect();
    let elsewhere = on_elsewhere(inst)?;

    // Installed mods by Nexus file, so a second run only adds what is missing.
    let have: HashMap<u64, String> = p.mods.iter().filter_map(|m| Some((inst.mod_meta(&m.id).nexus?.file_id?, m.id.clone()))).collect();
    let is_optional = |m: &&Value| m["optional"].as_bool() == Some(true);
    let mut mods: Vec<&Value> = c["mods"].as_array().into_iter().flatten().filter(|m| optional || !is_optional(m)).collect();
    if !optional {
        r.optional = c["mods"].as_array().into_iter().flatten().filter(is_optional).map(|m| m["name"].as_str().unwrap_or("?").to_string()).collect();
    }
    mods.sort_by_key(|m| m["phase"].as_u64().unwrap_or(0)); // Vortex installs phase by phase
    let kept: HashSet<&str> = mods.iter().filter_map(|m| have.get(&m["source"]["fileId"].as_u64()?)).map(String::as_str).collect();
    // The earlier revision's mods the new one has no file of: each may get its newer file.
    let mut olds: Vec<(u64, &str)> = prev_mods.iter().filter(|id| enabled.contains_key(*id) && !kept.contains(*id)).filter_map(|id| Some((inst.mod_meta(id).nexus?.mod_id, *id))).collect();
    olds.sort();
    let mut ids: Vec<(&Value, String)> = Vec::new(); // collection mod -> rmm id
    let mut off = HashSet::new(); // ids to leave off
    let mut todo = Vec::new();
    for m in mods {
        let name = m["name"].as_str().unwrap_or("?").to_string();
        let src = &m["source"];
        if let Some(t) = m["instructions"].as_str().filter(|t| !t.trim().is_empty()) {
            r.instructions.push(format!("{name}: {}", t.trim()));
        }
        if game_folder(m) {
            r.game_folder.push(name.clone());
        }
        let (Some(mod_id), Some(file_id), true) = (src["modId"].as_u64(), src["fileId"].as_u64(), src["type"] == "nexus") else {
            r.failed.push(format!("{name}: comes from {}, not a Nexus file; install it yourself", src["type"].as_str().unwrap_or("somewhere else")));
            continue;
        };
        if let Some(id) = have.get(&file_id) {
            if prev_mods.contains(id.as_str()) && enabled.get(id.as_str()) == Some(&false) {
                r.kept_off.push(name.clone());
                off.insert(id.clone());
            }
            r.installed.push(name);
            ids.push((m, id.clone()));
            continue;
        }
        r.download_size += rev.files.get(&file_id).map(|f| f.1).filter(|&s| s > 0).or(src["fileSize"].as_u64()).unwrap_or(0);
        // A newer file of a mod the profile has from the earlier revision, by the same name first.
        let at = olds.iter().position(|&(n, id)| n == mod_id && inst.mod_meta(id).name == name).or_else(|| olds.iter().position(|&(n, _)| n == mod_id));
        let old = at.map(|i| olds.remove(i).1.to_string());
        match &old {
            Some(o) => {
                if enabled.get(o.as_str()) == Some(&false) {
                    r.kept_off.push(name.clone());
                } else if elsewhere.contains(o) {
                    r.turned_off.push(o.clone());
                }
                r.updated.push(name);
            }
            None => r.install.push(name),
        }
        todo.push((m, mod_id, file_id, old));
    }
    let replaced: HashSet<&str> = todo.iter().filter_map(|t| t.3.as_deref()).collect();
    let dropped: Vec<String> = prev_mods.iter().filter(|id| enabled.get(**id) == Some(&true) && !kept.contains(*id) && !replaced.contains(*id)).map(|id| id.to_string()).collect();
    r.turned_off.extend(dropped);
    r.turned_off.sort_by_key(|id| p.mods.iter().position(|m| m.id == *id));
    if new_profile.is_none() {
        r.yours = p.mods.iter().filter(|m| m.enabled && !crate::instance::is_separator(&m.id) && !prev_mods.contains(m.id.as_str()) && !kept.contains(m.id.as_str())).map(|m| m.id.clone()).collect();
    }
    r.plugins = c["plugins"].as_array().map_or(0, Vec::len);
    // Free accounts: which files the user still has to download from the website (a request each).
    if !todo.is_empty() && client.validate()?["is_premium"] != true {
        for (m, mod_id, file_id, _) in &todo {
            if !crate::nexus::downloaded(inst, client, *mod_id, *file_id)?.2 {
                r.to_download.push(Pending {
                    name: m["name"].as_str().unwrap_or("?").into(),
                    mod_id: *mod_id,
                    file_id: *file_id,
                    size: rev.files.get(file_id).map(|f| f.1).filter(|&s| s > 0).or(m["source"]["fileSize"].as_u64()).unwrap_or(0),
                    page: format!("https://www.nexusmods.com/{domain}/mods/{mod_id}?tab=files&file_id={file_id}&nmm=1"),
                });
            }
        }
    }
    if dry_run {
        r.rules = c["modRules"].as_array().map_or(0, Vec::len);
        r.tools = c["tools"].as_array().into_iter().flatten().filter_map(|t| t["name"].as_str().map(str::to_owned)).collect();
        return Ok(r);
    }
    if let Some(f) = r.to_download.first() {
        bail!(
            "{} of the collection's files aren't downloaded yet. Free Nexus accounts download each from its page: click \"Mod manager download\" there (the first is {}: {}). The install goes ahead once they're all in. A dry run lists them; Nexus Premium downloads them directly",
            r.to_download.len(),
            f.name,
            f.page
        );
    }
    if let Some(name) = new_profile {
        // Empty: switching adds every installed mod, turned off; the collection turns its own on,
        // and the script extender stays on, as in a new empty profile in the window.
        inst.create_profile(name, false)?;
        inst.switch_profile(name)?;
        let se = crate::launch::script_extender_mods(inst)?;
        if !se.is_empty() {
            crate::ops::set_enabled_as(inst, &se, true, "turn the script extender on")?;
        }
    }
    for (i, (m, mod_id, file_id, old)) in todo.iter().enumerate() {
        let name = m["name"].as_str().unwrap_or("?");
        crate::progress::report(&format!("installing {name}"), i, todo.len());
        let beside = old.as_ref().is_some_and(|o| elsewhere.contains(o));
        match install_one(inst, client, m, *mod_id, *file_id, old.as_deref(), beside) {
            Ok(id) => {
                if old.as_ref().is_some_and(|o| enabled.get(o.as_str()) == Some(&false)) {
                    off.insert(id.clone());
                }
                ids.push((m, id));
            }
            Err(e) => {
                r.failed.push(format!("{name}: {e:#}"));
                r.turned_off.retain(|id| Some(id) != old.as_ref()); // the old one stays on
            }
        }
    }

    // Only mods that are off: turning one on turns on its plugins, which the user may have turned off.
    let now = inst.profile()?;
    let is_off = |id: &String| now.mods.iter().any(|m| m.id == *id && !m.enabled);
    let on: Vec<String> = ids.iter().map(|(_, id)| id.clone()).filter(|id| !off.contains(id) && is_off(id)).collect();
    if !on.is_empty() {
        crate::ops::set_enabled_as(inst, &on, true, &format!("collection {}: turn its mods on", rev.name))?;
    }
    if !r.turned_off.is_empty() {
        crate::ops::set_enabled_as(inst, &r.turned_off, false, &format!("collection {}: turn off what revision {} dropped", rev.name, rev.number))?;
    }
    r.rules = add_rules(inst, &c, &ids, &mut r.warnings)?;
    let states = plugin_states(&c);
    // An update changes only the plugins the curator changed, so the user's own choices stay.
    let want: BTreeMap<&String, bool> = states.iter().filter(|(n, on)| prev.as_ref().is_none_or(|x| x.plugins.get(*n) != Some(*on))).map(|(n, &on)| (n, on)).collect();
    let record = Installed { slug: slug.clone(), name: rev.name.clone(), revision: rev.number, optional, mods: ids.iter().map(|(_, id)| id.clone()).collect(), plugins: states.clone() };
    inst.edit(&format!("collection {}: plugins", rev.name), |p| {
        for e in p.plugins.iter_mut() {
            if let Some(&on) = want.get(&e.name.to_lowercase()) {
                e.enabled = on;
            }
        }
        p.collection = Some(record.clone());
        Ok(())
    })?;
    // `sort` off: the caller shows LOOT's order first (the window does), so hand-placed
    // plugins don't move unseen.
    match sort.then(|| crate::loot::sort(inst, false)) {
        Some(Ok(s)) => r.warnings.extend(s.warnings),
        Some(Err(e)) => r.warnings.push(format!("the plugins aren't sorted yet: {e:#}")),
        None => {}
    }
    r.tools = add_tools(inst, &c)?;
    Ok(r)
}

/// Download one of the collection's files (unless it's in the downloads folder already) and
/// install it with the curator's choices: as a new mod, as the newer file of mod `old` (in place,
/// keeping its place, rules and notes), or with `beside` as a new mod just after `old`.
fn install_one(inst: &Instance, client: &Client, m: &Value, mod_id: u64, file_id: u64, old: Option<&str>, beside: bool) -> Result<String> {
    let (path, _) = crate::nexus::fetch_file(inst, client, mod_id, file_id, "")?;
    let choices: crate::fomod::Choices = serde_json::from_value(m["choices"].clone()).unwrap_or_default();
    let id = match old {
        Some(o) if !beside => {
            crate::install::update(inst, o, &path, Some(&choices))?;
            o.to_string()
        }
        _ => {
            let at = old.and_then(|o| inst.profile().ok()?.mods.iter().position(|x| x.id == o)).map(|i| i + 1);
            crate::install::install_at(inst, &path, m["name"].as_str(), true, Some(&choices), at)?.id
        }
    };
    let mut meta = inst.mod_meta(&id);
    if game_folder(m) {
        crate::install::to_game_folder(&inst.mod_dir(&id))?;
        meta.game_folder = true;
    }
    meta.nexus = Some(NexusRef { mod_id, file_id: Some(file_id), version: m["version"].as_str().map(str::to_owned), category: None });
    if let Some(t) = m["instructions"].as_str().filter(|t| !t.trim().is_empty() && meta.notes.is_none()) {
        meta.notes = Some(t.trim().into());
    }
    inst.write_mod_meta(&id, &meta)?;
    Ok(id)
}

/// A collection's plugin list: lowercase names, on or off.
fn plugin_states(c: &Value) -> BTreeMap<String, bool> {
    c["plugins"].as_array().into_iter().flatten().filter_map(|x| Some((x["name"].as_str()?.to_lowercase(), x["enabled"].as_bool().unwrap_or(true)))).collect()
}

/// What the profile has of the collection: its record, or, for a collection installed before rmm
/// kept one, the turned-on mods an earlier revision's bundle in the downloads lists.
fn previous(inst: &Instance, p: &crate::Profile, slug: &str, number: u64) -> Option<Installed> {
    if let Some(x) = p.collection.as_ref().filter(|x| x.slug == slug) {
        return Some(x.clone());
    }
    let prefix = format!("collection-{slug}-");
    let n = fs::read_dir(inst.downloads_dir())
        .ok()?
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.strip_prefix(&prefix)?.strip_suffix(".7z")?.parse::<u64>().ok())
        .filter(|&n| n <= number)
        .max()?;
    let dir = inst.root.join(".staging").join(format!("{prefix}{n}"));
    if !dir.join("collection.json").is_file() {
        let _ = fs::remove_dir_all(&dir);
        crate::install::extract(&inst.downloads_dir().join(format!("{prefix}{n}.7z")), &dir).ok()?;
    }
    let c = read(&dir).ok()?;
    let files: HashSet<u64> = c["mods"].as_array()?.iter().filter_map(|m| m["source"]["fileId"].as_u64()).collect();
    let mods: Vec<String> = p.mods.iter().filter(|m| m.enabled && inst.mod_meta(&m.id).nexus.and_then(|x| x.file_id).is_some_and(|f| files.contains(&f))).map(|m| m.id.clone()).collect();
    (!mods.is_empty()).then(|| Installed { slug: slug.into(), name: c["info"]["name"].as_str().unwrap_or(slug).into(), revision: n, optional: false, mods, plugins: plugin_states(&c) })
}

/// Mods turned on in another profile. Mod folders are shared, so those get a newer file beside
/// them rather than in place.
fn on_elsewhere(inst: &Instance) -> Result<HashSet<String>> {
    let mut on = HashSet::new();
    for name in inst.profiles()?.into_iter().filter(|n| *n != inst.config.profile) {
        let p: crate::Profile = toml::from_str(&fs::read_to_string(inst.root.join("profiles").join(name).join("profile.toml"))?)?;
        on.extend(p.mods.into_iter().filter(|m| m.enabled).map(|m| m.id));
    }
    Ok(on)
}

/// Whether the collection gives the mod Vortex's "dinput" or "enb" type, which Vortex deploys to
/// the game folder instead of Data.
fn game_folder(m: &Value) -> bool {
    matches!(m["details"]["type"].as_str(), Some("dinput" | "enb"))
}

/// The collection's mod rules as rmm mod rules, then the mods sorted by them. Rules name mods by
/// archive hash, logical file name or archive name; ones naming a mod that isn't here are left
/// out. Returns how many mods got rules.
fn add_rules(inst: &Instance, c: &Value, ids: &[(&Value, String)], warnings: &mut Vec<String>) -> Result<usize> {
    // Each mod's archive name without its extension, which `fileExpression` names.
    let stems: HashMap<&str, String> = ids
        .iter()
        .filter_map(|(_, id)| Some((id.as_str(), std::path::Path::new(&inst.mod_meta(id).source?).file_stem()?.to_string_lossy().into_owned())))
        .collect();
    let find = |rf: &Value| -> Option<String> {
        let same = |a: &Value, b: &Value| a.as_str().is_some_and(|a| Some(a) == b.as_str());
        ids.iter()
            .find(|(m, id)| {
                same(&rf["fileMD5"], &m["source"]["md5"]) || same(&rf["logicalFileName"], &m["source"]["logicalFilename"]) || rf["fileExpression"].as_str().is_some_and(|e| stems.get(id.as_str()).is_some_and(|s| s == e))
            })
            .map(|(_, id)| id.clone())
    };
    let mut rules: BTreeMap<String, ModRule> = BTreeMap::new();
    for x in c["modRules"].as_array().into_iter().flatten() {
        let (Some(me), Some(other)) = (find(&x["source"]), find(&x["reference"])) else { continue };
        if me == other {
            continue;
        }
        let mut add = |target: &str, f: fn(&mut ModRule) -> &mut Vec<String>, item: String| {
            let list = f(rules.entry(target.to_string()).or_insert_with(|| ModRule { id: target.to_string(), ..Default::default() }));
            if !list.contains(&item) {
                list.push(item);
            }
        };
        // "A before B" is B's "after A".
        match x["type"].as_str() {
            Some("after") => add(&me, |r| &mut r.after, other),
            Some("before") => add(&other, |r| &mut r.after, me),
            Some("requires") => add(&me, |r| &mut r.requires, other),
            Some("conflicts") => add(&me, |r| &mut r.incompatible, other),
            _ => {}
        }
    }
    let old: HashMap<String, ModRule> = crate::rules::rules(inst)?.into_iter().map(|x| (x.id.clone(), x)).collect();
    for (id, mut rl) in rules.clone() {
        if let Some(o) = old.get(&id) {
            for (mine, theirs) in [(&mut rl.after, &o.after), (&mut rl.requires, &o.requires), (&mut rl.incompatible, &o.incompatible)] {
                // Leaving out mods since uninstalled: a mod installed again gets a new id.
                let extra: Vec<String> = theirs.iter().filter(|t| !mine.contains(t) && inst.mod_dir(t).is_dir()).cloned().collect();
                mine.extend(extra);
            }
        }
        if let Err(e) = crate::rules::set_rule(inst, &rl) {
            warnings.push(format!("rule for {id}: {e:#}"));
        }
    }
    let sorted = crate::rules::sort(inst, false)?;
    if !sorted.cycles.is_empty() {
        warnings.push(format!("{} rule pairs contradict each other and were left out of the order", sorted.cycles.len()));
    }
    Ok(rules.len())
}

/// The collection's tools (paths relative to the game folder) as rmm tools: rmm's preset when it
/// has one, the rest as given. The game and its script extender are left out (Play starts them),
/// and so are programs that are a tool already.
fn add_tools(inst: &Instance, c: &Value) -> Result<Vec<String>> {
    let game = inst.config.game;
    let skip = [game.exe().to_lowercase(), game.loader().to_lowercase(), game.exe().replace(".exe", "Launcher.exe").to_lowercase()];
    let key = |p: &std::path::Path| p.to_string_lossy().replace('\\', "/").to_ascii_lowercase();
    let mut have = crate::tools::list(inst)?;
    let mut added = Vec::new();
    for t in c["tools"].as_array().into_iter().flatten() {
        let (Some(name), Some(exe)) = (t["name"].as_str(), t["exe"].as_str()) else { continue };
        let exe = inst.config.game_path.join(exe.replace('\\', "/"));
        let file = exe.file_name().map(|f| f.to_string_lossy().to_lowercase()).unwrap_or_default();
        if skip.contains(&file) || have.values().any(|x| key(&x.exe) == key(&exe)) {
            continue;
        }
        let preset = crate::import::mo2_preset(&file).and_then(|p| Some((p, crate::tools::preset(game, p)?)));
        let mut tool = match &preset {
            Some((_, t)) => t.clone(),
            None => crate::tools::Tool { name: name.into(), args: t["args"].as_array().into_iter().flatten().filter_map(|a| a.as_str().map(str::to_owned)).collect(), ..Default::default() },
        };
        tool.exe = exe;
        let base = preset.map(|(p, _)| p.to_string()).unwrap_or_else(|| crate::import::tool_id(name));
        let id = (1..).map(|n| if n == 1 { base.clone() } else { format!("{base}-{n}") }).find(|id| !have.contains_key(id)).unwrap();
        crate::tools::put(inst, &id, &tool)?;
        have.insert(id, tool);
        added.push(name.to_string());
    }
    Ok(added)
}

/// A collection's slug from its link (`nexusmods.com/games/<game>/collections/<slug>`, with
/// `/revisions/<n>` after it or not) or the slug itself.
pub fn slug(link: &str) -> Result<String> {
    let s = match link.split_once("/collections/") {
        Some((_, rest)) => rest.split(['/', '?', '#']).next().unwrap_or(""),
        None => link.trim(),
    };
    if s.is_empty() || !s.chars().all(|c| c.is_ascii_alphanumeric()) {
        bail!("that isn't a collection link (nexusmods.com/games/<game>/collections/<id>) or id");
    }
    Ok(s.to_string())
}

/// A revision as Nexus's API describes it.
pub struct Revision {
    pub name: String,
    pub number: u64,
    pub download_link: String,
    /// modFiles: fileId -> (optional, size in bytes).
    pub files: HashMap<u64, (bool, u64)>,
}

pub fn revision(inst: &Instance, client: &Client, slug: &str, number: Option<u64>) -> Result<Revision> {
    let q = "query($slug: String!, $domain: String!, $rev: Int) { collectionRevision(slug: $slug, domainName: $domain, revision: $rev, viewAdultContent: true) { revisionNumber downloadLink collection { name } modFiles { optional fileId file { sizeInBytes } } } }";
    let v = client.graphql(q, serde_json::json!({ "slug": slug, "domain": inst.config.game.nexus_domain(), "rev": number }))?;
    let r = &v["collectionRevision"];
    if r.is_null() {
        bail!("Nexus has no collection '{slug}' for {}", inst.config.game.nexus_domain());
    }
    let files = r["modFiles"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| {
            let size = f["file"]["sizeInBytes"].as_str().and_then(|s| s.parse().ok()).or(f["file"]["sizeInBytes"].as_u64()).unwrap_or(0);
            Some((f["fileId"].as_u64()?, (f["optional"].as_bool().unwrap_or(false), size)))
        })
        .collect();
    Ok(Revision {
        name: r["collection"]["name"].as_str().unwrap_or(slug).to_string(),
        number: r["revisionNumber"].as_u64().context("Nexus sent no revision number")?,
        download_link: r["downloadLink"].as_str().context("Nexus sent no download link for the collection")?.to_string(),
        files,
    })
}

/// Download the revision's bundle and unpack it; returns the folder holding collection.json.
pub fn bundle(inst: &Instance, client: &Client, slug: &str, rev: &Revision) -> Result<PathBuf> {
    let archive = inst.downloads_dir().join(format!("collection-{slug}-{}.7z", rev.number));
    if !archive.is_file() {
        let links = client.get(&rev.download_link)?;
        let list = links.get("download_links").unwrap_or(&links);
        let uri = list.as_array().and_then(|l| l.first()).and_then(|l| l["URI"].as_str()).context("Nexus sent no link for the collection's bundle")?;
        fs::create_dir_all(archive.parent().unwrap())?;
        crate::progress::report(&format!("downloading {}", rev.name), 0, 0);
        crate::nexus::fetch_url(uri, &archive)?;
    }
    let dir = inst.root.join(".staging").join(format!("collection-{slug}-{}", rev.number));
    let _ = fs::remove_dir_all(&dir);
    crate::install::extract(&archive, &dir)?;
    if !dir.join("collection.json").is_file() {
        bail!("the collection's bundle has no collection.json");
    }
    Ok(dir)
}

/// The bundle's collection.json.
pub fn read(dir: &std::path::Path) -> Result<Value> {
    serde_json::from_str(&fs::read_to_string(dir.join("collection.json"))?).context("collection.json isn't JSON")
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_a_collection_link() {
        assert_eq!(super::slug("https://www.nexusmods.com/games/skyrimspecialedition/collections/h2uqa3/revisions/70").unwrap(), "h2uqa3");
    }
}
