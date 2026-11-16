//! Incremental hard-link deployment with a manifest (SPEC.md "Deployment").
//! Only files that differ from the last deploy are touched, unmanaged files are backed up
//! before being replaced, and files changed outside rmm are copied to overwrite/ first.
use crate::conflicts::file_map;
use crate::instance::OVERWRITE;
use crate::{Instance, path_key};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

const MANIFEST: &str = "deploy.json";
const OWNER: &str = ".rmm-owner";
/// Unmanaged Data files when the game was last launched, one path key per line.
const LAUNCHED: &str = "launched.txt";

#[derive(Serialize, Deserialize, Default)]
struct Manifest {
    /// path key of the game-relative target -> entry
    files: BTreeMap<String, Deployed>,
    /// Directories we created, game-relative; removed on purge if empty.
    dirs: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct Deployed {
    pub target: String,
    pub mod_id: String,
    pub src: String,
    size: u64,
    mtime: u64,
    backed_up: bool,
    pub copied: bool,
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
    /// Files the game and its plugins created in Data since `launch_game`; moved to overwrite/
    /// and linked back, so they are managed like everything else.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub played: Vec<String>,
    pub unchanged: usize,
    /// Files copied because hard links were impossible (different drives).
    pub copied: usize,
    /// Add-on hooks that failed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl DeployPlan {
    pub fn is_noop(&self) -> bool {
        self.link.is_empty() && self.remove.is_empty() && self.external_changes.is_empty() && self.played.is_empty()
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
    if dry_run {
        return deploy_with(inst, true, true);
    }
    // Add-on hooks run outside the lock, so they can call back into rmm.
    let before = crate::addons::hook(inst, "before_deploy", &serde_json::json!({}));
    if let Some((addon, why)) = before.iter().find_map(|(a, r)| Some((a, r.as_ref().ok()?["abort"].as_str()?))) {
        bail!("add-on {addon} stopped the deploy: {why}");
    }
    let mut plan = deploy_with(inst, false, true)?;
    let after = crate::addons::hook(inst, "after_deploy", &serde_json::json!({ "linked": plan.link.len(), "removed": plan.remove.len() }));
    plan.warnings.extend(crate::addons::warnings(&before));
    plan.warnings.extend(crate::addons::warnings(&after));
    Ok(plan)
}

/// `data: false` deploys only the game-root files (and takes linked Data files back out), for
/// the Linux VFS mode, which shows Data through a mount instead.
pub fn deploy_with(inst: &Instance, dry_run: bool, data: bool) -> Result<DeployPlan> {
    inst.locked(|| {
        #[cfg(target_os = "linux")]
        if let Some(pid) = crate::vfs::mounted_by(inst) {
            bail!("Data is mounted by `rmm vfs` (process {pid}); stop it (Ctrl+C) before deploying");
        }
        check_owner(inst)?;
        check_foreign(inst)?;
        let game = &inst.config.game_path;
        if !inst.root.join(MANIFEST).exists() && game.join(OWNER).exists() {
            bail!("deploy.json is missing but the game folder still has rmm's files; run purge (it finds them by file identity), then deploy");
        }
        if !dry_run {
            check_not_running(inst)?;
        }
        let profile = inst.profile()?;
        // plugins.txt is written last; refuse up front rather than leave a half-done deploy.
        if let Some(why) = crate::plugins::over_limit(&crate::plugins::list(inst, &profile)?) {
            bail!("{why}. Nothing was deployed");
        }
        let mut man = load_manifest(inst)?;
        let mut plan = DeployPlan::default();
        capture_played(inst, &man, &mut plan, dry_run)?;

        // Files changed outside rmm go to overwrite/ first, so they win the layering below.
        let mut changed: HashSet<String> = HashSet::new();
        let mut ok: HashSet<String> = HashSet::new();
        let checks = man.files.len();
        for (i, (key, e)) in man.files.iter().enumerate() {
            crate::progress::report("checking deployed files", i, checks);
            let path = game.join(&e.target);
            if !path.exists() {
                continue;
            }
            match ours(inst, e, &path) {
                Some(true) => {
                    ok.insert(key.clone());
                }
                Some(false) => {} // a reinstalled mod's old file: relinked below
                None => {
                    plan.external_changes.push(e.target.clone());
                    changed.insert(key.clone());
                    if !dry_run {
                        capture(inst, &e.target, &path)?;
                    }
                }
            }
        }

        let mut desired: BTreeMap<String, Want> = file_map(inst, &profile)?
            .winners()
            .into_values()
            .map(|(mod_id, src)| {
                let target = target_of(&src);
                (path_key(&target), Want { target, mod_id, src })
            })
            .collect();
        for key in &changed {
            // Already true after a real capture; a dry run has to pretend.
            let target = man.files[key].target.clone();
            desired.insert(key.clone(), Want { src: mod_rel(&target), mod_id: OVERWRITE.into(), target });
        }
        if !data {
            desired.retain(|_, w| !w.target.starts_with("Data/"));
        }

        let old_keys: HashSet<String> = man.files.keys().cloned().collect();
        let mut to_link: Vec<(String, bool)> = Vec::new();
        for key in &old_keys {
            let e = man.files[key].clone();
            let path = game.join(&e.target);
            let want = desired.get(key);
            // Copies keep their source's size and mtime, so a source that no longer matches the
            // recorded state was edited after the deploy and the copy is stale.
            let fresh = if e.copied {
                state(&inst.mod_dir(&e.mod_id).join(&e.src)).is_none_or(|s| s == (e.size, e.mtime))
            } else {
                !rewritten_in_play(key)
            };
            if ok.contains(key) && fresh && want.is_some_and(|w| w.mod_id == e.mod_id && w.src == e.src) {
                plan.unchanged += 1;
                if !dry_run && let Some(s) = state(&path) {
                    // In-place edits through a hard link change size/mtime on both sides.
                    let ent = man.files.get_mut(key).unwrap();
                    (ent.size, ent.mtime) = s;
                }
                continue;
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

        let links = to_link.len();
        let mut spelling = Spelling::default();
        for (i, (key, mut backed_up)) in to_link.into_iter().enumerate() {
            crate::progress::report(if dry_run { "planning" } else { "linking files" }, i, links);
            let w = &desired[&key];
            let target = spelling.on_disk(game, &w.target);
            let path = game.join(&target);
            plan.link.push(target.clone());
            // A folder rmm made, emptied by the removals above, where a mod now puts a file.
            if path.is_dir() && man.dirs.iter().any(|d| path_key(d) == key) && (dry_run || fs::remove_dir(&path).is_ok()) {
                man.dirs.retain(|d| path_key(d) != key);
            } else if !old_keys.contains(&key) && path.exists() {
                plan.backup.push(target.clone());
                backed_up = true;
                if !dry_run {
                    let b = inst.root.join("backup").join(&target);
                    fs::create_dir_all(b.parent().unwrap())?;
                    fs::rename(&path, b)?;
                }
            }
            if dry_run {
                continue;
            }
            create_parents(game, &path, &mut man.dirs)?;
            let src = inst.mod_dir(&w.mod_id).join(&w.src);
            let copied = if rewritten_in_play(&key) {
                copy_keep_mtime(&src, &path)?;
                true
            } else {
                link_or_copy(&src, &path)?
            };
            plan.copied += copied as usize;
            let (size, mtime) = state(&path).expect("just deployed");
            man.files.insert(
                key,
                Deployed { target, mod_id: w.mod_id.clone(), src: w.src.clone(), size, mtime, backed_up, copied },
            );
        }

        if !dry_run {
            save_manifest(inst, &man)?;
            fs::write(game.join(OWNER), inst.root.display().to_string())?;
            crate::plugins::write_plugins_txt(inst, &profile)?;
            crate::profile_ini::apply(inst, &profile)?;
        }
        Ok(plan)
    })
}

/// Remove everything rmm deployed and restore backed-up files.
pub fn purge(inst: &Instance, dry_run: bool) -> Result<DeployPlan> {
    inst.locked(|| {
        #[cfg(target_os = "linux")]
        if let Some(pid) = crate::vfs::mounted_by(inst) {
            bail!("Data is mounted by `rmm vfs` (process {pid}); stop it (Ctrl+C) first");
        }
        if !dry_run {
            check_not_running(inst)?;
            crate::profile_ini::release(inst)?;
        }
        if !inst.root.join(MANIFEST).exists() {
            return purge_by_identity(inst, dry_run);
        }
        let game = &inst.config.game_path;
        let man = load_manifest(inst)?;
        let mut plan = DeployPlan::default();
        capture_played(inst, &man, &mut plan, dry_run)?;
        for e in man.files.values() {
            let path = game.join(&e.target);
            plan.remove.push(e.target.clone());
            if path.exists() && !is_ours(inst, e, &path) {
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

/// Purge without deploy.json: remove every game file that is a hard link to a file in mods/,
/// trash/ or overwrite/, then put backed-up files back where nothing replaced them.
// ponytail: copies made across drives can't be told from vanilla files without the manifest; they stay.
fn purge_by_identity(inst: &Instance, dry_run: bool) -> Result<DeployPlan> {
    let game = &inst.config.game_path;
    let mut plan = DeployPlan::default();
    let mut sources: Vec<PathBuf> = vec![inst.root.join("overwrite")];
    for parent in ["mods", "trash"] {
        if let Ok(rd) = fs::read_dir(inst.root.join(parent)) {
            sources.extend(rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()));
        }
    }
    let data = inst.data_dir();
    for dir in sources.iter().filter(|d| d.is_dir()) {
        for rel in crate::instance::index_dir(dir)? {
            let target = target_of(&rel);
            let path = game.join(&target);
            if !same_file::is_same_file(&path, dir.join(&rel)).unwrap_or(false) {
                continue;
            }
            plan.remove.push(target);
            if !dry_run {
                fs::remove_file(&path)?;
                for d in path.ancestors().skip(1).take_while(|d| d.starts_with(game) && *d != game.as_path() && *d != data.as_path()) {
                    if fs::remove_dir(d).is_err() {
                        break; // not empty: someone else's folder
                    }
                }
            }
        }
    }
    let backup = inst.root.join("backup");
    if !dry_run && backup.is_dir() {
        for rel in crate::instance::index_dir(&backup)? {
            let path = game.join(&rel);
            if !path.exists() {
                fs::create_dir_all(path.parent().unwrap())?;
                fs::rename(backup.join(&rel), &path)?;
            }
        }
        remove_file_if_exists(&game.join(OWNER))?;
    }
    Ok(plan)
}

/// Path keys (game-relative, e.g. `data/foo.esp`) of every deployed file.
pub fn managed_keys(inst: &Instance) -> Result<HashSet<String>> {
    Ok(manifest_files(inst)?.into_keys().collect())
}

/// Every deployed file, keyed like `managed_keys`.
pub(crate) fn manifest_files(inst: &Instance) -> Result<BTreeMap<String, Deployed>> {
    Ok(load_manifest(inst)?.files)
}

fn check_owner(inst: &Instance) -> Result<()> {
    let owner = inst.config.game_path.join(OWNER);
    if let Ok(other) = fs::read_to_string(&owner)
        && Path::new(other.trim()) != inst.root {
            bail!("the game folder is deployed by another instance ({}); purge it there first", other.trim());
        }
    Ok(())
}

/// Vortex leaves `vortex.deployment*.json` in every folder it deploys to. Two managers linking
/// into one game folder wreck each other's records, so rmm waits until Vortex has purged.
pub fn vortex_manifest(inst: &Instance) -> Option<PathBuf> {
    [inst.config.game_path.clone(), inst.data_dir()].into_iter().find_map(|dir| {
        fs::read_dir(dir).ok()?.filter_map(|e| e.ok()).map(|e| e.path()).find(|p| {
            let n = p.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
            n.starts_with("vortex.deployment") && n.ends_with(".json")
        })
    })
}

fn check_foreign(inst: &Instance) -> Result<()> {
    if let Some(p) = vortex_manifest(inst) {
        bail!("Vortex has mods deployed here ({}); purge them in Vortex first", p.display());
    }
    Ok(())
}

fn check_not_running(inst: &Instance) -> Result<()> {
    let exe = inst.config.game.exe();
    if game_running(&inst.config.game_path, exe) {
        bail!("{exe} is running; close the game before changing its files");
    }
    Ok(())
}

/// Is `exe` (relative to the game folder) running? On Windows a running .exe can't be opened for
/// writing (ERROR_SHARING_VIOLATION).
#[cfg(windows)]
pub(crate) fn game_running(game: &Path, exe: &str) -> bool {
    matches!(fs::OpenOptions::new().write(true).open(game.join(exe)), Err(e) if e.raw_os_error() == Some(32))
}

/// Under Proton the game is a Wine process whose command line names the .exe.
#[cfg(not(windows))]
pub(crate) fn game_running(_game: &Path, exe: &str) -> bool {
    let exe = Path::new(exe).file_name().unwrap_or_default().to_string_lossy().to_lowercase();
    let Ok(rd) = fs::read_dir("/proc") else { return false };
    rd.filter_map(|e| e.ok()).any(|e| {
        fs::read(e.path().join("cmdline")).is_ok_and(|c| {
            String::from_utf8_lossy(&c).to_lowercase().split('\0').any(|a| a.rsplit(['/', '\\']).next() == Some(exe.as_str()))
        })
    })
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

/// Is the file at `path` still what rmm put there? Links are compared by file identity (NTFS file
/// id or inode): an in-place edit through the link is ours (the mod file changed with it), a
/// replaced file is not, even with the same size and mtime. Copies, and links whose source is gone
/// (mod uninstalled), fall back to the size and mtime recorded at deploy. A reinstalled mod's old
/// files are in trash/, and a link to one of those is still ours, but stale: `Some(false)`.
fn ours(inst: &Instance, e: &Deployed, path: &Path) -> Option<bool> {
    let src = inst.mod_dir(&e.mod_id).join(&e.src);
    if !e.copied && src.exists() {
        let same = |p: &Path| same_file::is_same_file(path, p).unwrap_or(false);
        return if same(&src) {
            Some(true)
        } else {
            same(&inst.root.join("trash").join(&e.mod_id).join(&e.src)).then_some(false)
        };
    }
    (state(path) == Some((e.size, e.mtime))).then_some(true)
}

fn is_ours(inst: &Instance, e: &Deployed, path: &Path) -> bool {
    ours(inst, e, path).is_some()
}

/// Where a game-relative target lives inside a mod or overwrite/: Data files at the top, the
/// rest under root/ (the reverse of `target_of`).
pub(crate) fn mod_rel(target: &str) -> String {
    match target.split_once('/') {
        Some((d, rest)) if d.eq_ignore_ascii_case("data") => rest.to_string(),
        _ => format!("root/{target}"),
    }
}

/// Script-extender plugin settings, which plugins rewrite in place when the game starts
/// (CrashLogger.ini, GrassControl.ini). They are deployed as copies, so the game's edit shows as
/// an external change and goes to overwrite/ instead of into the mod's file through a link.
fn rewritten_in_play(key: &str) -> bool {
    (key.starts_with("data/skse/plugins/") || key.starts_with("data/f4se/plugins/")) && [".ini", ".json", ".toml", ".yaml", ".yml"].iter().any(|e| key.ends_with(e))
}

/// Before the game starts: note the Data files rmm doesn't manage, so the next deploy can tell
/// which files the game and its plugins created during play.
pub(crate) fn remember_unmanaged(inst: &Instance) -> Result<()> {
    let man = load_manifest(inst)?;
    let keys: Vec<String> = data_files(inst)?.into_keys().filter(|k| !man.files.contains_key(k)).collect();
    crate::instance::write_atomic(&inst.root.join(LAUNCHED), &keys.join("\n"))
}

/// Move files that appeared in Data since the launch into overwrite/ (MO2 sends them there too).
// ponytail: only after launch_game; a game started from Steam leaves its new files unmanaged.
fn capture_played(inst: &Instance, man: &Manifest, plan: &mut DeployPlan, dry_run: bool) -> Result<()> {
    let Ok(seen) = fs::read_to_string(inst.root.join(LAUNCHED)) else { return Ok(()) };
    let seen: HashSet<&str> = seen.lines().collect();
    let data = inst.data_dir();
    for (key, target) in data_files(inst)? {
        if seen.contains(key.as_str()) || man.files.contains_key(&key) {
            continue;
        }
        plan.played.push(target.clone());
        if !dry_run {
            let (from, to) = (inst.config.game_path.join(&target), inst.root.join("overwrite").join(mod_rel(&target)));
            fs::create_dir_all(to.parent().unwrap())?;
            if fs::rename(&from, &to).is_err() {
                fs::copy(&from, &to)?;
                fs::remove_file(&from)?;
            }
            // Deploy recreates the folders it needs and records them, so purge removes them.
            for d in from.ancestors().skip(1).take_while(|d| d.starts_with(&data) && *d != data.as_path()) {
                if fs::remove_dir(d).is_err() {
                    break;
                }
            }
        }
    }
    if !dry_run {
        remove_file_if_exists(&inst.root.join(LAUNCHED))?;
    }
    Ok(())
}

/// Every file in Data: path key -> game-relative target (`Data/...`).
fn data_files(inst: &Instance) -> Result<BTreeMap<String, String>> {
    let data = inst.data_dir();
    let mut out = BTreeMap::new();
    for e in walkdir::WalkDir::new(&data).min_depth(1).into_iter().filter_map(|e| e.ok()).filter(|e| e.file_type().is_file()) {
        let target = format!("Data/{}", e.path().strip_prefix(&data)?.to_string_lossy().replace('\\', "/"));
        out.insert(path_key(&target), target);
    }
    Ok(out)
}

pub(crate) fn state(path: &Path) -> Option<(u64, u64)> {
    let m = fs::metadata(path).ok()?;
    let mtime = m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos() as u64;
    Some((m.len(), mtime))
}

/// Copy a file that changed outside rmm into overwrite/ (Data-relative, root files under root/).
fn capture(inst: &Instance, target: &str, path: &Path) -> Result<()> {
    let dest = inst.root.join("overwrite").join(mod_rel(target));
    fs::create_dir_all(dest.parent().unwrap())?;
    fs::copy(path, dest)?;
    Ok(())
}

/// Where a target goes on disk. Windows matches names without case; on Linux each part of the
/// path takes the spelling already on disk (or given to an earlier target), as Wine sees it, so
/// a mod's `Textures/` lands in vanilla's `textures/` instead of beside it.
#[derive(Default)]
#[cfg_attr(not(unix), allow(dead_code))]
struct Spelling(std::collections::HashMap<PathBuf, std::collections::HashMap<String, String>>);

impl Spelling {
    #[cfg(not(unix))]
    fn on_disk(&mut self, _game: &Path, target: &str) -> String {
        target.to_string()
    }

    #[cfg(unix)]
    fn on_disk(&mut self, game: &Path, target: &str) -> String {
        let mut dir = game.to_path_buf();
        let mut out = Vec::new();
        for part in target.split('/') {
            let names = self.0.entry(dir.clone()).or_insert_with(|| {
                fs::read_dir(&dir)
                    .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).map(|n| (n.to_lowercase(), n)).collect())
                    .unwrap_or_default()
            });
            let actual = names.entry(part.to_lowercase()).or_insert_with(|| part.to_string()).clone();
            dir.push(&actual);
            out.push(actual);
        }
        out.join("/")
    }
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
    copy_keep_mtime(src, dst)?;
    Ok(true)
}

/// Copy a file and give the copy its source's mtime. Windows copies keep it anyway; Linux ones
/// would get the copy time, whose coarse clock can equal a source edit made right after.
fn copy_keep_mtime(src: &Path, dst: &Path) -> Result<()> {
    fs::copy(src, dst)?;
    let t = fs::metadata(src)?.modified()?;
    fs::File::options().write(true).open(dst)?.set_modified(t)?;
    Ok(())
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}
