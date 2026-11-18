//! Nexus collections: install a collection's revision in one go. Nexus serves each revision as a
//! bundle whose `collection.json` (the format Vortex writes and reads) lists the mods by Nexus
//! file, with each one's FOMOD choices and whether it is optional, the rules between them and the
//! plugins. rmm downloads each file (Premium accounts), installs it with those choices, adds the
//! rules as mod rules, sorts the mods by them, applies the plugin list and sorts the plugins
//! with LOOT, as Vortex does. Mods already installed from the same Nexus file are kept.
use crate::Instance;
use crate::instance::NexusRef;
use crate::nexus::Client;
use crate::rules::ModRule;
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;

#[derive(Serialize, Debug, Default)]
pub struct Report {
    pub dry_run: bool,
    pub name: String,
    pub revision: u64,
    /// The game versions the collection was made for.
    pub game_versions: Vec<String>,
    /// Mods to install (or installed), by name, in install order.
    pub install: Vec<String>,
    /// Bytes still to download.
    pub download_size: u64,
    /// Mods already installed from the same Nexus file.
    pub installed: Vec<String>,
    /// Optional mods left out.
    pub optional: Vec<String>,
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
}

/// Install a collection's revision (default: the newest) into the active profile: its required
/// mods, and with `optional` its optional ones too. A dry run downloads only the bundle and
/// says what would be installed.
pub fn install(inst: &Instance, client: &Client, link: &str, number: Option<u64>, optional: bool, dry_run: bool) -> Result<Report> {
    let slug = slug(link)?;
    let rev = revision(inst, client, &slug, number)?;
    let c = read(&bundle(inst, client, &slug, &rev)?)?;
    let domain = inst.config.game.nexus_domain();
    if let Some(d) = c["info"]["domainName"].as_str().filter(|d| *d != domain) {
        bail!("this collection is for {d}, the instance for {domain}");
    }
    let strings = |v: &Value| v.as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_owned)).collect::<Vec<_>>();
    let mut r = Report { dry_run, name: rev.name.clone(), revision: rev.number, game_versions: strings(&c["info"]["gameVersions"]), ..Default::default() };
    if let Ok(v) = crate::launch::game_version(&inst.config.game_path, inst.config.game) {
        let have = v.map(|n| n.to_string()).join(".");
        if !r.game_versions.is_empty() && !r.game_versions.contains(&have) {
            r.warnings.push(format!("the collection was made for game version {}, and the game is {have}", r.game_versions.join(" or ")));
        }
    }

    // Installed mods by Nexus file, so a second run only adds what is missing.
    let p = inst.profile()?;
    let have: HashMap<u64, String> = p.mods.iter().filter_map(|m| Some((inst.mod_meta(&m.id).nexus?.file_id?, m.id.clone()))).collect();
    let mut mods: Vec<&Value> = c["mods"].as_array().into_iter().flatten().collect();
    mods.sort_by_key(|m| m["phase"].as_u64().unwrap_or(0)); // Vortex installs phase by phase
    let mut ids: Vec<(&Value, String)> = Vec::new(); // collection mod -> rmm id
    let mut todo = Vec::new();
    for m in mods {
        let name = m["name"].as_str().unwrap_or("?").to_string();
        let src = &m["source"];
        if m["optional"].as_bool() == Some(true) && !optional {
            r.optional.push(name);
            continue;
        }
        if let Some(t) = m["instructions"].as_str().filter(|t| !t.trim().is_empty()) {
            r.instructions.push(format!("{name}: {}", t.trim()));
        }
        let (Some(mod_id), Some(file_id), true) = (src["modId"].as_u64(), src["fileId"].as_u64(), src["type"] == "nexus") else {
            r.failed.push(format!("{name}: comes from {}, not a Nexus file; install it yourself", src["type"].as_str().unwrap_or("somewhere else")));
            continue;
        };
        match have.get(&file_id) {
            Some(id) => {
                r.installed.push(name);
                ids.push((m, id.clone()));
            }
            None => {
                r.download_size += rev.files.get(&file_id).map(|f| f.1).filter(|&s| s > 0).or(src["fileSize"].as_u64()).unwrap_or(0);
                r.install.push(name);
                todo.push((m, mod_id, file_id));
            }
        }
    }
    r.plugins = c["plugins"].as_array().map_or(0, Vec::len);
    if dry_run {
        r.rules = c["modRules"].as_array().map_or(0, Vec::len);
        r.tools = c["tools"].as_array().into_iter().flatten().filter_map(|t| t["name"].as_str().map(str::to_owned)).collect();
        return Ok(r);
    }
    if !todo.is_empty() && client.validate()?["is_premium"] != true {
        bail!("installing a collection needs a Nexus Premium account: rmm downloads each of its files directly");
    }
    for (i, (m, mod_id, file_id)) in todo.iter().enumerate() {
        let name = m["name"].as_str().unwrap_or("?");
        crate::progress::report(&format!("installing {name}"), i, todo.len());
        match install_one(inst, client, m, *mod_id, *file_id) {
            Ok(id) => ids.push((m, id)),
            Err(e) => r.failed.push(format!("{name}: {e:#}")),
        }
    }

    r.rules = add_rules(inst, &c, &ids, &mut r.warnings)?;
    let want: HashMap<String, bool> = c["plugins"].as_array().into_iter().flatten().filter_map(|x| Some((x["name"].as_str()?.to_lowercase(), x["enabled"].as_bool().unwrap_or(true)))).collect();
    inst.edit(&format!("collection {}: plugins", rev.name), |p| {
        for e in p.plugins.iter_mut() {
            if let Some(&on) = want.get(&e.name.to_lowercase()) {
                e.enabled = on;
            }
        }
        Ok(())
    })?;
    match crate::loot::sort(inst, false) {
        Ok(s) => r.warnings.extend(s.warnings),
        Err(e) => r.warnings.push(format!("the plugins aren't sorted yet: {e:#}")),
    }
    r.tools = add_tools(inst, &c)?;
    Ok(r)
}

/// Download one of the collection's files (unless it's in the downloads folder already) and
/// install it with the curator's choices.
fn install_one(inst: &Instance, client: &Client, m: &Value, mod_id: u64, file_id: u64) -> Result<String> {
    let (path, _) = crate::nexus::fetch_file(inst, client, mod_id, file_id, "")?;
    let choices: crate::fomod::Choices = serde_json::from_value(m["choices"].clone()).unwrap_or_default();
    let done = crate::install::install(inst, &path, m["name"].as_str(), true, Some(&choices))?;
    let mut meta = inst.mod_meta(&done.id);
    meta.nexus = Some(NexusRef { mod_id, file_id: Some(file_id), version: m["version"].as_str().map(str::to_owned), category: None });
    if let Some(t) = m["instructions"].as_str().filter(|t| !t.trim().is_empty()) {
        meta.notes = Some(t.trim().into());
    }
    inst.write_mod_meta(&done.id, &meta)?;
    Ok(done.id)
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
