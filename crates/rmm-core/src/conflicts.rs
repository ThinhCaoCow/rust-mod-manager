//! Conflicts between enabled mods. Loose files: later mods in the profile win. Files inside
//! archives (BSA/BA2): the archive whose plugin loads later wins, and any loose file beats them all.
use crate::{Instance, Profile, archives, path_key};
use anyhow::Result;
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};

pub struct FileMap {
    /// Enabled mod ids in priority order (index = priority).
    pub mods: Vec<String>,
    /// path key -> (relative path as the winner spells it, owning mods by priority, last = winner)
    pub files: BTreeMap<String, (String, Vec<usize>)>,
}

/// Enabled mods in profile order, then `overwrite/` on top (MO2's model).
pub fn file_map(inst: &Instance, profile: &Profile) -> Result<FileMap> {
    let mut mods: Vec<String> = profile.mods.iter().filter(|m| m.enabled).map(|m| m.id.clone()).collect();
    mods.push(crate::instance::OVERWRITE.into());
    let mut files: BTreeMap<String, (String, Vec<usize>)> = BTreeMap::new();
    for (i, id) in mods.iter().enumerate() {
        for rel in inst.mod_files(id)? {
            let e = files.entry(path_key(&rel)).or_default();
            // The winner's spelling: on a case-sensitive file system it is the file to open.
            e.0 = rel;
            e.1.push(i);
        }
    }
    Ok(FileMap { mods, files })
}

impl FileMap {
    /// path key -> (winning mod id, relative path inside that mod)
    pub fn winners(&self) -> BTreeMap<String, (String, String)> {
        self.files
            .iter()
            .map(|(k, (rel, owners))| (k.clone(), (self.mods[*owners.last().unwrap()].clone(), rel.clone())))
            .collect()
    }
}

#[derive(Debug, Serialize)]
pub struct ModConflicts {
    pub id: String,
    pub total_files: usize,
    pub winning_files: usize,
    pub losing_files: usize,
    /// True when every file is overridden: the mod currently does nothing.
    pub fully_overridden: bool,
    pub overwrites: Vec<String>,
    pub overwritten_by: Vec<String>,
    /// Files inside this mod's archives that the game loads.
    #[serde(skip_serializing_if = "is_zero")]
    pub archived_files: usize,
    /// Of those, files another mod replaces: with a loose file, or in an archive loaded later.
    #[serde(skip_serializing_if = "is_zero")]
    pub archived_losing: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub archive_overwrites: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub archive_overwritten_by: Vec<String>,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// A mod archive the game loads because an enabled plugin is named after it.
pub struct LoadedArchive {
    pub mod_id: String,
    /// The archive's file name inside the mod.
    pub name: String,
    pub plugin: String,
}

/// (mod id, archive file name)
pub type ModArchive = (String, String);

/// Archives from enabled mods in the order the game loads them (their plugins' load order),
/// and the (mod id, archive) pairs that no enabled plugin loads.
pub fn mod_archives(inst: &Instance, profile: &Profile, map: &FileMap) -> Result<(Vec<LoadedArchive>, Vec<ModArchive>)> {
    let found: Vec<(String, String)> = map.winners().into_values().filter(|(_, rel)| archives::is_archive(rel)).collect();
    let mut loaded = Vec::new();
    let mut used = HashSet::new();
    for p in crate::plugins::list(inst, profile)?.into_iter().filter(|p| p.enabled) {
        for (mod_id, rel) in &found {
            if archives::loaded_by(inst.config.game, &p.name, rel) && used.insert(rel.to_lowercase()) {
                loaded.push(LoadedArchive { mod_id: mod_id.clone(), name: rel.clone(), plugin: p.name.clone() });
            }
        }
    }
    let unloaded = found.into_iter().filter(|(_, rel)| !used.contains(&rel.to_lowercase())).collect();
    Ok((loaded, unloaded))
}

/// One entry per enabled mod that has at least one conflict.
pub fn conflicts(inst: &Instance, profile: &Profile) -> Result<Vec<ModConflicts>> {
    let map = file_map(inst, profile)?;
    let mut out: Vec<ModConflicts> = map
        .mods
        .iter()
        .map(|id| ModConflicts {
            id: id.clone(),
            total_files: 0,
            winning_files: 0,
            losing_files: 0,
            fully_overridden: false,
            overwrites: vec![],
            overwritten_by: vec![],
            archived_files: 0,
            archived_losing: 0,
            archive_overwrites: vec![],
            archive_overwritten_by: vec![],
        })
        .collect();
    for (_, owners) in map.files.values() {
        for (pos, &m) in owners.iter().enumerate() {
            let c = &mut out[m];
            c.total_files += 1;
            if owners.len() == 1 {
                continue;
            }
            if pos + 1 == owners.len() { c.winning_files += 1 } else { c.losing_files += 1 }
            for &other in &owners[..pos] {
                push_unique(&mut c.overwrites, &map.mods[other]);
            }
            for &other in &owners[pos + 1..] {
                push_unique(&mut c.overwritten_by, &map.mods[other]);
            }
        }
    }

    for (key, owners) in &archived(inst, profile, &map)? {
        let owners: Vec<usize> = owners.iter().map(|(m, _)| *m).collect();
        let winner = map.files.get(key).map_or(*owners.last().unwrap(), |(_, loose)| *loose.last().unwrap());
        for &m in &owners {
            out[m].archived_files += 1;
            if m != winner {
                out[m].archived_losing += 1;
                push_unique(&mut out[m].archive_overwritten_by, &map.mods[winner]);
                push_unique(&mut out[winner].archive_overwrites, &map.mods[m]);
            }
        }
    }

    for c in &mut out {
        c.fully_overridden = c.total_files > 0 && c.losing_files == c.total_files && c.archived_losing == c.archived_files;
    }
    out.retain(|c| c.winning_files + c.losing_files + c.archived_losing > 0 || !c.archive_overwrites.is_empty());
    Ok(out)
}

/// Files inside loaded archives: path key -> (mod index, archive name), in load order.
// ponytail: archives are re-read on every call (about 20 ms per 10k files); cache the lists
// per mod (mods are immutable) if large setups make this slow.
fn archived(inst: &Instance, profile: &Profile, map: &FileMap) -> Result<BTreeMap<String, Vec<(usize, String)>>> {
    let mut out: BTreeMap<String, Vec<(usize, String)>> = BTreeMap::new();
    for a in mod_archives(inst, profile, map)?.0 {
        let Some(m) = map.mods.iter().position(|id| *id == a.mod_id) else { continue };
        let Ok(files) = archives::list(&inst.mod_dir(&a.mod_id).join(&a.name)) else { continue }; // health reports it
        for f in files {
            let owners = out.entry(path_key(&f)).or_default();
            owners.retain(|(o, _)| *o != m);
            owners.push((m, a.name.clone()));
        }
    }
    Ok(out)
}

/// One conflicting file of a mod: who provides it, lowest priority first, and who wins.
#[derive(Debug, Serialize, PartialEq)]
pub struct FileConflict {
    pub file: String,
    /// Mod ids providing the file, in the order they load (the last loose one wins).
    pub mods: Vec<String>,
    pub winner: String,
    /// For a file inside archives: the archive of each mod in `mods`; empty for loose files.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub archives: Vec<String>,
}

/// Every file of `id` that another enabled mod also provides, loose or in a loaded archive.
pub fn file_conflicts(inst: &Instance, profile: &Profile, id: &str) -> Result<Vec<FileConflict>> {
    let map = file_map(inst, profile)?;
    let Some(m) = map.mods.iter().position(|x| x == id) else { return Ok(vec![]) };
    let arch = archived(inst, profile, &map)?;
    let mut out = Vec::new();
    for (key, (rel, owners)) in &map.files {
        if owners.len() > 1 && owners.contains(&m) && !arch.contains_key(key) {
            let mods: Vec<String> = owners.iter().map(|&o| map.mods[o].clone()).collect();
            out.push(FileConflict { file: rel.clone(), winner: mods.last().unwrap().clone(), mods, archives: vec![] });
        }
    }
    for (key, owners) in arch {
        let loose = map.files.get(&key);
        if !(owners.iter().any(|(o, _)| *o == m) || loose.is_some_and(|(_, l)| l.contains(&m))) || (owners.len() < 2 && loose.is_none()) {
            continue;
        }
        let winner = loose.map_or(owners.last().unwrap().0, |(_, l)| *l.last().unwrap());
        let mut mods: Vec<String> = owners.iter().map(|(o, _)| map.mods[*o].clone()).collect();
        let mut names: Vec<String> = owners.iter().map(|(_, a)| a.clone()).collect();
        if let Some((_, l)) = loose {
            for &o in l {
                mods.push(map.mods[o].clone());
                names.push(String::new()); // loose
            }
        }
        out.push(FileConflict { file: key, winner: map.mods[winner].clone(), mods, archives: names });
    }
    out.sort_by_key(|a| a.file.to_lowercase());
    Ok(out)
}

/// A file or folder in the Data folder as the game sees it after a deploy (MO2's Data tab).
#[derive(Debug, Serialize, PartialEq)]
pub struct DataEntry {
    /// The name in its folder; the path from Data in a search.
    pub name: String,
    pub dir: bool,
    /// Folders: how many files are inside (None: only the game's own files are).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<usize>,
    /// Files: the mods providing it in load order, the last one wins. Empty: the game's own file.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mods: Vec<String>,
    /// A file only in archives: the archive it comes from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct DataQuery {
    /// A folder in Data, such as `meshes/actors`; empty for Data itself.
    #[serde(default)]
    pub dir: String,
    /// Search every folder for paths containing this instead (the first FIND_LIMIT).
    #[serde(default)]
    pub find: String,
    /// Only files more than one mod provides.
    #[serde(default)]
    pub conflicts_only: bool,
    /// Also files inside the BSA/BA2 archives the game loads.
    #[serde(default)]
    pub archives: bool,
}

pub const FIND_LIMIT: usize = 1000;

/// One folder of Data: its subfolders, then its files with the mods behind each.
pub fn data_view(inst: &Instance, profile: &Profile, q: &DataQuery) -> Result<Vec<DataEntry>> {
    // A folder in Data: "C:\Windows" or "/etc" would join to itself.
    if q.dir.split(['/', '\\']).any(|c| c == "..") || q.dir.contains(':') || q.dir.starts_with(['/', '\\']) {
        anyhow::bail!("not a folder in Data: {}", q.dir);
    }
    let map = file_map(inst, profile)?;
    let arch = if q.archives { archived(inst, profile, &map)? } else { BTreeMap::new() };
    // path key -> (spelling, providers in load order, the archive when no loose file wins)
    let mut all: BTreeMap<&str, (&str, Vec<&str>, Option<&str>)> = BTreeMap::new();
    for (key, owners) in &arch {
        all.insert(key.as_str(), (key.as_str(), owners.iter().map(|(m, _)| map.mods[*m].as_str()).collect(), owners.last().map(|(_, a)| a.as_str())));
    }
    for (key, (rel, owners)) in map.files.iter().filter(|(k, _)| !k.starts_with("root/")) {
        let e = all.entry(key.as_str()).or_insert((rel.as_str(), vec![], None));
        (e.0, e.2) = (rel.as_str(), None);
        e.1.extend(owners.iter().map(|&m| map.mods[m].as_str()));
    }
    let shown = |mods: &[&str]| !q.conflicts_only || mods.len() > 1;
    let file = |name: &str, mods: &[&str], archive: Option<&str>| DataEntry {
        name: name.into(),
        dir: false,
        files: None,
        mods: mods.iter().map(|m| m.to_string()).collect(),
        archive: archive.map(Into::into),
    };
    if !q.find.is_empty() {
        let find = path_key(&q.find);
        let hits = all.iter().filter(|(k, (_, mods, _))| k.contains(&find) && shown(mods));
        return Ok(hits.take(FIND_LIMIT).map(|(_, (rel, mods, a))| file(rel, mods, *a)).collect());
    }
    let prefix = match path_key(&q.dir).trim_matches('/') {
        "" => String::new(),
        d => format!("{d}/"),
    };
    let depth = prefix.matches('/').count();
    let mut dirs: BTreeMap<String, DataEntry> = BTreeMap::new();
    let mut files = Vec::new();
    for (_, (rel, mods, a)) in all.range(prefix.as_str()..).take_while(|(k, _)| k.starts_with(&prefix)).filter(|(_, (_, m, _))| shown(m)) {
        let mut parts = rel.split('/').skip(depth);
        let name = parts.next().unwrap_or_default();
        if parts.next().is_none() {
            files.push(file(name, mods, *a));
            continue;
        }
        let d = dirs.entry(name.to_lowercase()).or_insert_with(|| DataEntry { name: name.into(), dir: true, files: Some(0), mods: vec![], archive: None });
        d.files = Some(d.files.unwrap_or(0) + 1);
    }
    // What's in the game's Data folder that no mod puts there: the base game, manual installs.
    if !q.conflicts_only {
        let managed = crate::deploy::managed_keys(inst)?;
        for e in std::fs::read_dir(inst.data_dir().join(&q.dir)).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let key = format!("{prefix}{}", path_key(&name));
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                dirs.entry(path_key(&name)).or_insert(DataEntry { name, dir: true, files: None, mods: vec![], archive: None });
            } else if !all.contains_key(key.as_str()) && !managed.contains(&format!("data/{key}")) {
                files.push(file(&name, &[], None));
            }
        }
    }
    files.sort_by_key(|f| f.name.to_lowercase());
    Ok(dirs.into_values().chain(files).collect())
}

fn push_unique(v: &mut Vec<String>, s: &str) {
    if !v.iter().any(|x| x == s) {
        v.push(s.into());
    }
}
