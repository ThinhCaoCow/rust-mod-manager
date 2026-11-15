//! Loose-file conflicts between enabled mods. Later mods in the profile win.
use crate::{Instance, Profile, path_key};
use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeMap;

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
    for c in &mut out {
        c.fully_overridden = c.total_files > 0 && c.losing_files == c.total_files;
    }
    out.retain(|c| c.winning_files + c.losing_files > 0);
    Ok(out)
}

fn push_unique(v: &mut Vec<String>, s: &str) {
    if !v.iter().any(|x| x == s) {
        v.push(s.into());
    }
}
