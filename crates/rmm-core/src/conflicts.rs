//! Conflicts between enabled mods. Loose files: later mods in the profile win. Files inside
//! archives (BSA/BA2): the archive whose plugin loads later wins, and any loose file beats them all.
use crate::{Instance, Profile, archives, path_key};
use anyhow::Result;
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};

pub struct FileMap {
    /// Enabled mod ids in priority order (index = priority).
    pub mods: Vec<String>,
    /// path key -> (relative path as first seen, owning mods by priority, last = winner)
    pub files: BTreeMap<String, (String, Vec<usize>)>,
}

/// Enabled mods in profile order, then `overwrite/` on top (MO2's model).
pub fn file_map(inst: &Instance, profile: &Profile) -> Result<FileMap> {
    let mut mods: Vec<String> = profile.mods.iter().filter(|m| m.enabled).map(|m| m.id.clone()).collect();
    mods.push(crate::instance::OVERWRITE.into());
    let mut files: BTreeMap<String, (String, Vec<usize>)> = BTreeMap::new();
    for (i, id) in mods.iter().enumerate() {
        for rel in inst.mod_files(id)? {
            files.entry(path_key(&rel)).or_insert_with(|| (rel, Vec::new())).1.push(i);
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

    // ponytail: archives are re-read on every call (about 20 ms per 10k files); cache the lists
    // per mod (mods are immutable) if large setups make this slow.
    let mut archived: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for a in mod_archives(inst, profile, &map)?.0 {
        let Some(m) = map.mods.iter().position(|id| *id == a.mod_id) else { continue };
        let Ok(files) = archives::list(&inst.mod_dir(&a.mod_id).join(&a.name)) else { continue }; // health reports it
        for f in files {
            let owners = archived.entry(path_key(&f)).or_default();
            owners.retain(|&o| o != m);
            owners.push(m);
        }
    }
    for (key, owners) in &archived {
        let winner = map.files.get(key).map_or(*owners.last().unwrap(), |(_, loose)| *loose.last().unwrap());
        for &m in owners {
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

fn push_unique(v: &mut Vec<String>, s: &str) {
    if !v.iter().any(|x| x == s) {
        v.push(s.into());
    }
}
