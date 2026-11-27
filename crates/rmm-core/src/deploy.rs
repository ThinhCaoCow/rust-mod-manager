//! Incremental hard-link deployment with a manifest (SPEC.md "Deployment").
//! Only files that differ from the last deploy are touched, unmanaged files are backed up
//! before being replaced, and files changed outside rmm are copied to overwrite/ first.
use crate::conflicts::file_map;
use crate::instance::OVERWRITE;
use crate::{Instance, path_key};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) const MANIFEST: &str = "deploy.json";
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
    /// The deployed file's id (NTFS file reference or inode); 0 in manifests from before it was kept.
    #[serde(default)]
    id: u64,
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
/// A file that comes with a mod but isn't for the game, by its target (`Data/...`, or a path in
/// the game folder): MO2's meta.ini, Vortex's override notes, a FOMOD installer, documents (read-
/// me's, licences, change logs, install notes) at the top of Data or the game folder or in a docs
/// folder at the top of Data, and the script extender's sources (`src/` in the game folder).
pub(crate) fn extra(target: &str) -> bool {
    const DOCS: &[&str] = &[".txt", ".md", ".pdf", ".rtf", ".htm", ".html", ".url", ".doc", ".docx"];
    let t = target.to_ascii_lowercase();
    let (in_root, rel) = match t.strip_prefix("data/") {
        Some(r) => (false, r),
        None => (true, t.as_str()),
    };
    let name = rel.rsplit('/').next().unwrap_or(rel);
    let first = rel.split('/').next().unwrap_or(rel);
    // "LICENSE" and "README" with no extension too; a plugin called readme.esp stays.
    let doc = DOCS.iter().any(|e| name.ends_with(e)) || (!name.contains('.') && ["readme", "license", "licence"].iter().any(|p| name.starts_with(p)));
    name == "meta.ini"
        || name == "vortex_override_instructions.json"
        || rel.starts_with("fomod/")
        || (!rel.contains('/') && doc)
        || (!in_root && rel.contains('/') && (first.starts_with("readme") || matches!(first, "docs" | "documentation")))
        || (in_root && rel.starts_with("src/"))
}

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
    // A dry run only reads, so it doesn't hold up changes while it scans a long list.
    let run = || -> Result<DeployPlan> {
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
        // A deploy that fails half-way still saves the manifest: it records every file changed so
        // far, so the next deploy doesn't take rmm's own links for the user's files.
        let applied = (|| -> Result<()> {
            capture_played(inst, &man, &mut plan, dry_run)?;
            // The game folder and each mod folder are listed once, with each file's size, time and
            // id, so no file is opened: a link that is still the file rmm linked, and still the
            // file at its source path, needs nothing more. A source replaced by a new file (an
            // editor or `sed -i` writing a new file) fails this and is linked again.
            // overwrite/ changes in place, so its files, and whatever fails this, are checked one by one.
            // Listed side by side: hundreds of mod folders one after another doubled a deploy's time.
            let mut ids: Vec<&String> = man.files.values().map(|e| &e.mod_id).filter(|id| *id != OVERWRITE).collect();
            ids.sort();
            ids.dedup();
            let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(16);
            let (seen, sources) = std::thread::scope(|s| {
                let seen = s.spawn(|| listing(game));
                let parts: Vec<_> = ids
                    .chunks(ids.len().div_ceil(threads).max(1))
                    .map(|part| s.spawn(move || part.iter().map(|id| ((*id).clone(), listing(&inst.mod_dir(id)))).collect::<Vec<_>>()))
                    .collect();
                let sources: HashMap<String, HashMap<String, Option<Stat>>> = parts.into_iter().flat_map(|p| p.join().unwrap_or_default()).collect();
                (seen.join().unwrap_or_default(), sources)
            });
            let source_id = |e: &Deployed| sources.get(&e.mod_id)?.get(&path_key(&e.src)).copied().flatten().map(|s| s.id);

            // Files changed outside rmm go to overwrite/ first, so they win the layering below.
            let mut changed: HashSet<String> = HashSet::new();
            let mut ok: HashSet<String> = HashSet::new();
            // Entries from manifests that didn't keep ids yet, and the ids they get.
            let mut learned: HashMap<String, u64> = HashMap::new();
            let checks = man.files.len();
            for (i, (key, e)) in man.files.iter().enumerate() {
                crate::progress::report("checking deployed files", i, checks);
                let Some(&found) = seen.get(key) else { continue }; // gone: linked again below
                let path = game.join(&e.target);
                let verdict = match found {
                    Some(f) if !e.copied && f.id != 0 && f.id == e.id && source_id(e) == Some(f.id) => Some(true),
                    Some(f) if e.copied && (f.size, f.mtime) == (e.size, e.mtime) => Some(true),
                    _ => ours(inst, e, &path),
                };
                if let (Some(true), Some(f)) = (verdict, found)
                    && e.id != f.id
                {
                    learned.insert(key.clone(), f.id);
                }
                match verdict {
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
            // The user's choices (Settings): files left out, by their path inside Data, and copies
            // instead of links.
            let cfg = crate::settings::get().deploy;
            if !cfg.exclude.is_empty() {
                desired.retain(|_, w| !crate::settings::excluded(&cfg.exclude, w.target.strip_prefix("Data/").unwrap_or(&w.target)));
            }
            if cfg.skip_extras {
                desired.retain(|_, w| !extra(&w.target));
            }
            let copy_all = cfg.method == "copy";
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
                    // Links become copies when the user switches to copies; copies stay until a purge.
                    !rewritten_in_play(key) && !copy_all
                };
                if ok.contains(key) && fresh && want.is_some_and(|w| w.mod_id == e.mod_id && w.src == e.src) {
                    plan.unchanged += 1;
                    if !dry_run && let Some(&id) = learned.get(key) {
                        man.files.get_mut(key).unwrap().id = id;
                    }
                    continue;
                }
                if want.is_none() {
                    plan.remove.push(e.target.clone());
                }
                // A file relinked below keeps its entry (and its backup) until it is linked again.
                if !dry_run {
                    remove_file_if_exists(&path)?;
                    if want.is_none() {
                        if e.backed_up {
                            move_file(&inst.root.join("backup").join(&e.target), &path)?;
                        }
                        man.files.remove(key);
                    }
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
                // The listing is from before the removals above, so a deploy asks the disk.
                let (is_dir, exists) = if dry_run { seen.get(&key).map_or((false, false), |s| (s.is_none(), true)) } else { (path.is_dir(), path.exists()) };
                // A folder rmm made, emptied by the removals above, where a mod now puts a file.
                if is_dir && man.dirs.iter().any(|d| path_key(d) == key) && (dry_run || fs::remove_dir(&path).is_ok()) {
                    man.dirs.retain(|d| path_key(d) != key);
                } else if !old_keys.contains(&key) && exists {
                    plan.backup.push(target.clone());
                    backed_up = true;
                    if !dry_run {
                        let b = inst.root.join("backup").join(&target);
                        fs::create_dir_all(b.parent().unwrap())?;
                        move_file(&path, &b)?;
                    }
                }
                if dry_run {
                    continue;
                }
                // Recorded before linking, so a failed link is retried and its backup still restored.
                man.files.insert(key.clone(), Deployed { target, mod_id: w.mod_id.clone(), src: w.src.clone(), size: 0, mtime: 0, id: 0, backed_up, copied: false });
                let e = man.files.get_mut(&key).unwrap();
                create_parents(game, &path, &mut man.dirs)?;
                let src = inst.mod_dir(&w.mod_id).join(&w.src);
                e.copied = if copy_all || rewritten_in_play(&key) {
                    copy_keep_mtime(&src, &path)?;
                    true
                } else {
                    link_or_copy(&src, &path)?
                };
                plan.copied += e.copied as usize;
                let s = stat(&path).expect("just deployed");
                (e.size, e.mtime, e.id) = (s.size, s.mtime, s.id);
            }
            // Folders rmm made that the removals above left empty (a mod's Docs, now an extra).
            if !dry_run {
                man.dirs.sort_by_key(|d| std::cmp::Reverse(d.len()));
                man.dirs.retain(|d| fs::remove_dir(game.join(d)).is_err()); // only succeeds when empty
            }
            Ok(())
        })();

        let saved = if dry_run { Ok(()) } else { save_manifest(inst, &man).and_then(|_| Ok(fs::write(game.join(OWNER), inst.root.display().to_string())?)) };
        applied?;
        saved?;
        if !dry_run {
            crate::plugins::write_plugins_txt(inst, &profile)?;
            crate::profile_ini::apply(inst, &profile)?;
        }
        Ok(plan)
    };
    if dry_run { run() } else { inst.locked(run) }
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
            let backup = inst.root.join("backup").join(&e.target);
            // A purge that stopped half-way already put this one back: the file there is the
            // original, not a change to keep.
            if e.backed_up && !backup.exists() && path.exists() && !is_ours(inst, e, &path) {
                continue;
            }
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
                    move_file(&backup, &path)?;
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
                move_file(&backup.join(&rel), &path)?;
            }
        }
    }
    if !dry_run {
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
    // Windows paths: the same folder whatever the case or slashes.
    let key = |p: &Path| p.to_string_lossy().replace('\\', "/").trim_end_matches('/').to_lowercase();
    if let Ok(other) = fs::read_to_string(&owner)
        && key(Path::new(other.trim())) != key(&inst.root) {
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
/// replaced file is not, even with the same size and mtime. The link rmm made to a file the mod
/// no longer has (reinstalled or uninstalled) is ours, but stale: `Some(false)`. Copies, and links
/// in manifests from before file ids were kept whose mod is gone, fall back to the size and mtime
/// recorded at deploy.
// ponytail: on Linux, an inode freed by every link and reused by a new file at the same path
// passes as rmm's link; NTFS file ids carry a reuse count, so Windows doesn't have this.
fn ours(inst: &Instance, e: &Deployed, path: &Path) -> Option<bool> {
    let src = inst.mod_dir(&e.mod_id).join(&e.src);
    if !e.copied {
        let same = |p: &Path| same_file::is_same_file(path, p).unwrap_or(false);
        if same(&src) {
            return Some(true);
        }
        if e.id != 0 {
            return (stat(path).map(|f| f.id) == Some(e.id)).then_some(false);
        }
        if src.exists() {
            // The old files of an uninstalled mod, or of an updated or reinstalled one.
            let trash = inst.root.join("trash");
            let replaced = fs::read_dir(trash.join(".replaced")).into_iter().flatten().flatten().map(|d| d.path());
            let old = std::iter::once(trash.join(&e.mod_id)).chain(replaced.filter(|d| {
                d.file_name().and_then(|n| n.to_str()?.strip_prefix(e.mod_id.as_str())?.strip_prefix('-')).is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()))
            }));
            return old.into_iter().any(|d| same(&d.join(&e.src))).then_some(false);
        }
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
            move_file(&from, &to)?;
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
    size_and_time(&fs::metadata(path).ok()?)
}

fn size_and_time(m: &fs::Metadata) -> Option<(u64, u64)> {
    Some((m.len(), m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos() as u64))
}

/// A file's size, mtime (ns since 1970, as `state` gives it) and id: the NTFS file reference
/// (unique per volume, with a reuse count) or the inode. Hard links to one file share the id.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Stat {
    size: u64,
    mtime: u64,
    id: u64,
}

#[cfg(unix)]
fn stat(path: &Path) -> Option<Stat> {
    use std::os::unix::fs::MetadataExt;
    let m = fs::metadata(path).ok()?;
    let (size, mtime) = size_and_time(&m)?;
    Some(Stat { size, mtime, id: m.ino() })
}

#[cfg(unix)]
fn listing(dir: &Path) -> HashMap<String, Option<Stat>> {
    let entries = walkdir::WalkDir::new(dir).min_depth(1).into_iter().filter_map(|e| e.ok());
    entries
        .filter_map(|e| {
            let key = path_key(&e.path().strip_prefix(dir).ok()?.to_string_lossy());
            Some((key, if e.file_type().is_dir() { None } else { Some(stat(e.path())?) }))
        })
        .collect()
}

#[cfg(windows)]
fn stat(path: &Path) -> Option<Stat> {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, GetFileInformationByHandle};
    // No access rights asked, like fs::metadata: works on files other programs hold open.
    let f = fs::OpenOptions::new().access_mode(0).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(path).ok()?;
    // SAFETY: a valid handle and a struct of the type the call fills.
    let mut i: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(f.as_raw_handle(), &mut i) } == 0 {
        return None;
    }
    let t = i.ftLastWriteTime;
    Some(Stat {
        size: (i.nFileSizeHigh as u64) << 32 | i.nFileSizeLow as u64,
        mtime: filetime_ns((t.dwHighDateTime as i64) << 32 | t.dwLowDateTime as i64)?,
        id: (i.nFileIndexHigh as u64) << 32 | i.nFileIndexLow as u64,
    })
}

/// A Windows FILETIME (100 ns since 1601) as ns since 1970, the way std's `modified()` gives it.
#[cfg(windows)]
fn filetime_ns(t: i64) -> Option<u64> {
    u64::try_from(t - 116_444_736_000_000_000).ok().map(|t| t * 100)
}

/// Every file and folder under `dir`: path key -> its Stat (None for a folder). On Windows one
/// FileIdBothDirectoryInfo listing per folder carries the sizes, times and file ids, so no file is
/// opened.
#[cfg(windows)]
fn listing(dir: &Path) -> HashMap<String, Option<Stat>> {
    let mut out = HashMap::new();
    list_into(dir, "", &mut out);
    out
}

#[cfg(windows)]
fn list_into(dir: &Path, prefix: &str, out: &mut HashMap<String, Option<Stat>>) {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::*;
    let Ok(f) = fs::OpenOptions::new().read(true).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(dir) else { return };
    // 8-aligned, as the records are; the slack past the 64 KiB handed over keeps a reference to the
    // last record (whose struct ends after its name) inside the buffer.
    let mut buf = vec![0u64; 8192 + 8];
    let mut class = FileIdBothDirectoryRestartInfo;
    let mut subdirs = Vec::new();
    // SAFETY: a directory handle and a buffer of the size given; the call fills it with
    // FILE_ID_BOTH_DIR_INFO records chained by NextEntryOffset, each name FileNameLength bytes long.
    while unsafe { GetFileInformationByHandleEx(f.as_raw_handle(), class, buf.as_mut_ptr().cast(), 8192 * 8) } != 0 {
        class = FileIdBothDirectoryInfo;
        let mut at = 0;
        loop {
            let rec = unsafe { buf.as_ptr().cast::<u8>().add(at).cast::<FILE_ID_BOTH_DIR_INFO>() };
            let i = unsafe { &*rec };
            let name = unsafe { std::slice::from_raw_parts(std::ptr::addr_of!((*rec).FileName).cast::<u16>(), i.FileNameLength as usize / 2) };
            let name = String::from_utf16_lossy(name);
            if name != "." && name != ".." {
                let rel = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
                if i.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                    // Junctions and folder symlinks aren't followed, as walkdir doesn't.
                    if i.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
                        subdirs.push((dir.join(&name), rel.clone()));
                    }
                    out.insert(path_key(&rel), None);
                } else if let Some(mtime) = filetime_ns(i.LastWriteTime) {
                    out.insert(path_key(&rel), Some(Stat { size: i.EndOfFile as u64, mtime, id: i.FileId as u64 }));
                }
            }
            if i.NextEntryOffset == 0 {
                break;
            }
            at += i.NextEntryOffset as usize;
        }
    }
    drop(f);
    for (path, rel) in subdirs {
        list_into(&path, &rel, out);
    }
}

/// Copy a file that changed outside rmm into overwrite/ (Data-relative, root files under root/).
fn capture(inst: &Instance, target: &str, path: &Path) -> Result<()> {
    let dest = inst.root.join("overwrite").join(mod_rel(target));
    fs::create_dir_all(dest.parent().unwrap())?;
    // An older overwrite/ file may be a link to somewhere; copying onto it would write through.
    remove_file_if_exists(&dest)?;
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

/// Rename, or copy and delete where the instance and the game are on different drives.
fn move_file(from: &Path, to: &Path) -> Result<()> {
    if fs::rename(from, to).is_err() {
        fs::copy(from, to)?;
        fs::remove_file(from)?;
    }
    Ok(())
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod extra_tests {
    use super::extra;

    #[test]
    fn files_that_arent_for_the_game() {
        for t in ["Data/meta.ini", "Data/fomod/ModuleConfig.xml", "Data/README.txt", "Data/License.md", "src/skse64/main.cpp", "readme.txt", "LICENSE", "Data/vortex_override_instructions.json", "skse64_whatsnew.txt", "SSE Engine Fixes - Install Instructions.txt", "Data/readmes/a.pdf", "Data/Docs/x.html"] {
            assert!(extra(t), "{t}");
        }
        for t in ["Data/SkyUI_SE.esp", "Data/interface/readme.swf", "skse64_loader.exe", "Data/scripts/source/a.psc", "Data/src/x.txt", "Data/meshes/fomod.nif", "enbseries.ini", "Data/SKSE/Plugins/notes.txt", "Data/readme.esp"] {
            assert!(!extra(t), "{t}");
        }
    }
}
