//! Incremental hard-link deployment with a manifest (SPEC.md "Deployment").
//! Only files that differ from the last deploy are touched, unmanaged files are backed up
//! before being replaced, and files changed outside rmm are copied to overwrite/ first.
use crate::conflicts::file_map;
use crate::{Instance, path_key};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

const MANIFEST: &str = "deploy.json";
const OWNER: &str = ".rmm-owner";

#[derive(Serialize, Deserialize, Default)]
struct Manifest {
    /// path key of the game-relative target -> entry
    files: BTreeMap<String, Deployed>,
    /// Directories we created, game-relative; removed on purge if empty.
    dirs: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone)]
struct Deployed {
    target: String,
    mod_id: String,
    src: String,
    size: u64,
    mtime: u64,
    backed_up: bool,
    copied: bool,
}

#[derive(Debug, Default, Serialize)]
pub struct DeployPlan {
    /// Targets that will be (re)linked.
    pub link: Vec<String>,
    /// Targets that will be removed.
    pub remove: Vec<String>,
    /// Unmanaged files that will be moved to backup/ (restored on purge).
    pub backup: Vec<String>,
    /// Managed files changed outside rmm; copied to overwrite/ before being replaced.
    pub external_changes: Vec<String>,
    pub unchanged: usize,
    /// Files copied because hard links were impossible (different drives).
    pub copied: usize,
}

impl DeployPlan {
    pub fn is_noop(&self) -> bool {
        self.link.is_empty() && self.remove.is_empty() && self.external_changes.is_empty()
    }
}

struct Want {
    target: String,
    mod_id: String,
    src: String,
}

/// Mod-relative path -> game-relative target (`root/x` goes to the game root, the rest to Data).
fn target_of(rel: &str) -> String {
    match rel.strip_prefix("root/") {
        Some(r) => r.to_string(),
        None => format!("Data/{rel}"),
    }
}

pub fn deploy(inst: &Instance, dry_run: bool) -> Result<DeployPlan> {
    inst.locked(|| {
        check_owner(inst)?;
        let profile = inst.profile()?;
        let game = &inst.config.game_path;
        let desired: BTreeMap<String, Want> = file_map(inst, &profile)?
            .winners()
            .into_values()
            .map(|(mod_id, src)| {
                let target = target_of(&src);
                (path_key(&target), Want { target, mod_id, src })
            })
            .collect();
        let mut man = load_manifest(inst)?;
        let old_keys: HashSet<String> = man.files.keys().cloned().collect();
        let mut plan = DeployPlan::default();
        let mut to_link: Vec<(String, bool)> = Vec::new();

        for key in &old_keys {
            let e = man.files[key].clone();
            let path = game.join(&e.target);
            let cur = state(&path);
            let ours = cur == Some((e.size, e.mtime)) || still_linked(inst, &e, cur);
            let want = desired.get(key);
            let same_src = want.is_some_and(|w| w.mod_id == e.mod_id && w.src == e.src);
            if ours && same_src {
                plan.unchanged += 1;
                if !dry_run {
                    let ent = man.files.get_mut(key).unwrap();
                    (ent.size, ent.mtime) = cur.unwrap();
                }
                continue;
            }
            if !ours && path.exists() {
                plan.external_changes.push(e.target.clone());
                if !dry_run {
                    capture(inst, &e.target, &path)?;
                }
            }
            if want.is_none() {
                plan.remove.push(e.target.clone());
            }
            if !dry_run {
                remove_file_if_exists(&path)?;
                if e.backed_up && want.is_none() {
                    fs::rename(inst.root.join("backup").join(&e.target), &path)?;
                }
                man.files.remove(key);
            }
            if want.is_some() {
                to_link.push((key.clone(), e.backed_up));
            }
        }
        to_link.extend(desired.keys().filter(|k| !old_keys.contains(*k)).map(|k| (k.clone(), false)));
        to_link.sort();

        for (key, mut backed_up) in to_link {
            let w = &desired[&key];
            let path = game.join(&w.target);
            plan.link.push(w.target.clone());
            if !old_keys.contains(&key) && path.exists() {
                plan.backup.push(w.target.clone());
                backed_up = true;
                if !dry_run {
                    let b = inst.root.join("backup").join(&w.target);
                    fs::create_dir_all(b.parent().unwrap())?;
                    fs::rename(&path, b)?;
                }
            }
            if dry_run {
                continue;
            }
            create_parents(game, &path, &mut man.dirs)?;
            let copied = link_or_copy(&inst.mod_dir(&w.mod_id).join(&w.src), &path)?;
            plan.copied += copied as usize;
            let (size, mtime) = state(&path).expect("just deployed");
            man.files.insert(
                key,
                Deployed { target: w.target.clone(), mod_id: w.mod_id.clone(), src: w.src.clone(), size, mtime, backed_up, copied },
            );
        }

        if !dry_run {
            save_manifest(inst, &man)?;
            fs::write(game.join(OWNER), inst.root.display().to_string())?;
            crate::plugins::write_plugins_txt(inst, &profile)?;
        }
        Ok(plan)
    })
}

/// Remove everything rmm deployed and restore backed-up files.
pub fn purge(inst: &Instance, dry_run: bool) -> Result<DeployPlan> {
    inst.locked(|| {
        let game = &inst.config.game_path;
        let man = load_manifest(inst)?;
        let mut plan = DeployPlan::default();
        for e in man.files.values() {
            let path = game.join(&e.target);
            plan.remove.push(e.target.clone());
            let cur = state(&path);
            if cur.is_some_and(|s| s != (e.size, e.mtime)) && !still_linked(inst, e, cur) {
                plan.external_changes.push(e.target.clone());
                if !dry_run {
                    capture(inst, &e.target, &path)?;
                }
            }
            if !dry_run {
                remove_file_if_exists(&path)?;
                if e.backed_up {
                    fs::rename(inst.root.join("backup").join(&e.target), &path)?;
                }
            }
        }
        if !dry_run {
            let mut dirs = man.dirs.clone();
            dirs.sort_by_key(|d| std::cmp::Reverse(d.len()));
            for d in dirs {
                let _ = fs::remove_dir(game.join(d)); // only succeeds when empty
            }
            remove_file_if_exists(&inst.root.join(MANIFEST))?;
            remove_file_if_exists(&game.join(OWNER))?;
        }
        Ok(plan)
    })
}

/// Path keys (game-relative, e.g. `data/foo.esp`) of every deployed file.
pub fn managed_keys(inst: &Instance) -> Result<HashSet<String>> {
    Ok(load_manifest(inst)?.files.into_keys().collect())
}

fn check_owner(inst: &Instance) -> Result<()> {
    let owner = inst.config.game_path.join(OWNER);
    if let Ok(other) = fs::read_to_string(&owner)
        && Path::new(other.trim()) != inst.root {
            bail!("the game folder is deployed by another instance ({}); purge it there first", other.trim());
        }
    Ok(())
}

fn load_manifest(inst: &Instance) -> Result<Manifest> {
    match fs::read_to_string(inst.root.join(MANIFEST)) {
        Ok(t) => Ok(serde_json::from_str(&t)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Manifest::default()),
        Err(e) => Err(e.into()),
    }
}

fn save_manifest(inst: &Instance, man: &Manifest) -> Result<()> {
    crate::instance::write_atomic(&inst.root.join(MANIFEST), &serde_json::to_string(man)?)
}

/// A hard link shares metadata with its mod file, so if both changed together the
/// change lives in the mod already (edited in place). Nothing to capture.
fn still_linked(inst: &Instance, e: &Deployed, cur: Option<(u64, u64)>) -> bool {
    !e.copied && cur.is_some() && cur == state(&inst.mod_dir(&e.mod_id).join(&e.src))
}

fn state(path: &Path) -> Option<(u64, u64)> {
    let m = fs::metadata(path).ok()?;
    let mtime = m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos() as u64;
    Some((m.len(), mtime))
}

/// Copy a file that changed outside rmm into overwrite/ (Data-relative, root files under root/).
fn capture(inst: &Instance, target: &str, path: &Path) -> Result<()> {
    let rel = target.strip_prefix("Data/").map(str::to_owned).unwrap_or_else(|| format!("root/{target}"));
    let dest = inst.root.join("overwrite").join(rel);
    fs::create_dir_all(dest.parent().unwrap())?;
    fs::copy(path, dest)?;
    Ok(())
}

fn create_parents(game: &Path, path: &Path, created: &mut Vec<String>) -> Result<()> {
    let mut missing: Vec<PathBuf> = path.ancestors().skip(1).take_while(|a| !a.exists()).map(Path::to_path_buf).collect();
    missing.reverse();
    for d in missing {
        fs::create_dir(&d)?;
        created.push(d.strip_prefix(game)?.to_string_lossy().replace('\\', "/"));
    }
    Ok(())
}

/// Hard link, falling back to a copy (e.g. instance and game on different drives). Returns true if copied.
fn link_or_copy(src: &Path, dst: &Path) -> Result<bool> {
    if fs::hard_link(src, dst).is_ok() {
        return Ok(false);
    }
    fs::copy(src, dst)?;
    Ok(true)
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}
