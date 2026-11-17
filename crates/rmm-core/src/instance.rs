use crate::Game;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

const CONFIG: &str = "instance.toml";
pub(crate) const MOD_META: &str = "rmm-mod.toml";
const MOD_INDEX: &str = ".rmm-index";

type IndexCache = std::collections::HashMap<PathBuf, (Stamp, Vec<String>)>;
static INDEX_CACHE: std::sync::LazyLock<std::sync::Mutex<IndexCache>> = std::sync::LazyLock::new(Default::default);

/// What tells one `.rmm-index` from the next. The time alone doesn't: Linux moves file times on a
/// clock that ticks every few milliseconds, so a reinstall can write its index within the same
/// tick. The new index is a new file, though (the old one went to trash/ with its mod).
type Stamp = (std::time::SystemTime, u64, u64);

fn stamp(m: &fs::Metadata) -> Stamp {
    #[cfg(unix)]
    let file = std::os::unix::fs::MetadataExt::ino(m);
    #[cfg(windows)]
    let file = std::os::windows::fs::MetadataExt::creation_time(m);
    (m.modified().unwrap_or(std::time::UNIX_EPOCH), m.len(), file)
}

/// Separators head a group of mods in the list, like MO2's `_separator` mods. Their ids start
/// with `sep:`, which no mod id can (`:` is stripped from mod names), and they hold no files.
pub fn is_separator(id: &str) -> bool {
    id.starts_with("sep:")
}

/// Pseudo mod id for `overwrite/`, deployed above every mod. `<>` never appears in real ids.
pub const OVERWRITE: &str = "<overwrite>";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct InstanceConfig {
    pub game: Game,
    pub game_path: PathBuf,
    /// Folder holding plugins.txt.
    pub local_path: PathBuf,
    pub profile: String,
    /// Folder holding the game's INI files; worked out from `local_path` when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ini_path: Option<PathBuf>,
    /// Where downloads go; `downloads/` in the instance when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downloads_path: Option<PathBuf>,
}

/// Everything a setup is, minus the (immutable) mod files. Small, so it is snapshotted for undo.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Profile {
    /// Priority order: later entries win file conflicts.
    #[serde(default)]
    pub mods: Vec<ModEntry>,
    /// Plugin load order. Plugins whose mod is disabled stay here so they keep their slot.
    #[serde(default)]
    pub plugins: Vec<PluginEntry>,
    /// Row colours (`#rrggbb`) by separator or mod id.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub colors: std::collections::BTreeMap<String, String>,
    /// Plugins locked to a position in `plugins` (MO2's index lock): every change puts them back.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub locks: std::collections::BTreeMap<String, usize>,
    /// The profile keeps its own copies of the game's INIs (`profiles/<name>/ini/`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub local_inis: bool,
    /// The profile's saves go to `Saves/<name>/`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub local_saves: bool,
}

impl Profile {
    /// Put locked plugins back at their positions, lowest first.
    pub fn apply_locks(&mut self) {
        let mut locked: Vec<(usize, PluginEntry)> = Vec::new();
        for (name, &at) in &self.locks {
            if let Some(i) = self.plugins.iter().position(|e| e.name.eq_ignore_ascii_case(name)) {
                locked.push((at, self.plugins.remove(i)));
            }
        }
        locked.sort_by_key(|(at, _)| *at);
        for (at, e) in locked {
            self.plugins.insert(at.min(self.plugins.len()), e);
        }
    }
}

/// What a change did to a profile, or what restoring a history entry would do.
#[derive(Serialize, Debug, Default, PartialEq)]
pub struct Diff {
    pub enabled: Vec<String>,
    pub disabled: Vec<String>,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    /// Mods whose place among the others changed.
    pub moved: Vec<String>,
    /// Plugins enabled, disabled, added, removed or moved.
    pub plugins: usize,
}

/// The difference from `a` to `b`.
pub fn diff(a: &Profile, b: &Profile) -> Diff {
    use std::collections::HashMap;
    let before: HashMap<&str, bool> = a.mods.iter().map(|m| (m.id.as_str(), m.enabled)).collect();
    let after: HashMap<&str, bool> = b.mods.iter().map(|m| (m.id.as_str(), m.enabled)).collect();
    let mut d = Diff::default();
    for m in &b.mods {
        match before.get(m.id.as_str()) {
            None => d.added.push(m.id.clone()),
            Some(&was) if was != m.enabled && !is_separator(&m.id) => (if m.enabled { &mut d.enabled } else { &mut d.disabled }).push(m.id.clone()),
            _ => {}
        }
    }
    d.removed = a.mods.iter().filter(|m| !after.contains_key(m.id.as_str())).map(|m| m.id.clone()).collect();
    // Moved: the mods outside a longest run that kept its order.
    let pos: HashMap<&str, usize> = a.mods.iter().enumerate().map(|(i, m)| (m.id.as_str(), i)).collect();
    let common: Vec<(usize, &str)> = b.mods.iter().filter_map(|m| pos.get(m.id.as_str()).map(|&i| (i, m.id.as_str()))).collect();
    let keep = longest_increasing(&common.iter().map(|(i, _)| *i).collect::<Vec<_>>());
    d.moved = common.iter().enumerate().filter(|(k, _)| !keep.contains(k)).map(|(_, (_, id))| id.to_string()).collect();
    let plugin_state = |p: &Profile| p.plugins.iter().map(|e| (e.name.to_lowercase(), e.enabled)).collect::<Vec<_>>();
    let (pa, pb) = (plugin_state(a), plugin_state(b));
    d.plugins = pb.iter().enumerate().filter(|(i, e)| pa.get(*i) != Some(e)).count().max(pa.len().saturating_sub(pb.len()));
    d
}

/// Indices (into `v`) of one longest strictly increasing subsequence. O(n log n).
fn longest_increasing(v: &[usize]) -> std::collections::HashSet<usize> {
    let (mut tails, mut prev): (Vec<usize>, Vec<Option<usize>>) = (Vec::new(), vec![None; v.len()]);
    for i in 0..v.len() {
        let k = tails.partition_point(|&t| v[t] < v[i]);
        prev[i] = k.checked_sub(1).map(|k| tails[k]);
        if k == tails.len() { tails.push(i) } else { tails[k] = i }
    }
    let mut out = std::collections::HashSet::new();
    let mut at = tails.last().copied();
    while let Some(i) = at {
        out.insert(i);
        at = prev[i];
    }
    out
}

/// One undoable change: its label and what it did.
#[derive(Serialize, Debug)]
pub struct HistoryEntry {
    pub label: String,
    pub change: Diff,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ModEntry {
    pub id: String,
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PluginEntry {
    pub name: String,
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ModMeta {
    pub name: String,
    #[serde(default)]
    pub source: Option<String>,
    /// FOMOD choices the mod was installed with (Vortex's `installerChoices` format).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fomod: Option<crate::fomod::Choices>,
    /// The user's notes on the mod.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// Category labels, like MO2's. None means never set, so the Nexus category shows instead;
    /// an empty list means the user removed them all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub categories: Option<Vec<String>>,
    /// Where the mod comes from on Nexus Mods, for update checks and download links.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nexus: Option<NexusRef>,
    /// sha256 of the archive it was installed from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// A review of the mod's code (an AI's or the user's), kept so it isn't checked again while
    /// its files stay the same.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<crate::inspect::Review>,
}

/// A mod's page and file on Nexus Mods (the game comes from the instance).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, schemars::JsonSchema)]
pub struct NexusRef {
    pub mod_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The mod's category on Nexus, by id (`nexus::category_names` has the names).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<u64>,
}

pub struct Instance {
    pub root: PathBuf,
    pub config: InstanceConfig,
}

impl Instance {
    pub fn create(root: &Path, game: Game, game_path: &Path, local_path: &Path) -> Result<Self> {
        if root.join(CONFIG).exists() {
            bail!("{} is already an instance", root.display());
        }
        if !game_path.join(game.exe()).is_file() {
            bail!("{} not found in {}", game.exe(), game_path.display());
        }
        for d in ["mods", "overwrite", "backup", "trash", "profiles/default"] {
            fs::create_dir_all(root.join(d))?;
        }
        let inst = Instance {
            root: root.to_path_buf(),
            config: InstanceConfig {
                game,
                game_path: game_path.into(),
                local_path: local_path.into(),
                profile: "default".into(),
                ini_path: None,
                downloads_path: None,
            },
        };
        fs::write(root.join(CONFIG), toml::to_string_pretty(&inst.config)?)?;
        write_atomic(&inst.profile_file(), &toml::to_string_pretty(&Profile::default())?)?;
        Ok(inst)
    }

    pub fn open(root: &Path) -> Result<Self> {
        let text = fs::read_to_string(root.join(CONFIG))
            .with_context(|| format!("{} is not an rmm instance (no {CONFIG})", root.display()))?;
        crate::addons::register_games(root);
        Ok(Instance { root: root.into(), config: toml::from_str(&text)? })
    }

    pub fn mods_dir(&self) -> PathBuf {
        self.root.join("mods")
    }
    pub fn mod_dir(&self, id: &str) -> PathBuf {
        if id == OVERWRITE { self.root.join("overwrite") } else { self.mods_dir().join(id) }
    }
    pub fn data_dir(&self) -> PathBuf {
        self.config.game_path.join("Data")
    }
    /// Where downloads are kept: the folder set for this instance, else `downloads/` in it.
    pub fn downloads_dir(&self) -> PathBuf {
        self.config.downloads_path.clone().unwrap_or_else(|| self.root.join("downloads"))
    }

    /// Keep downloads in `dir` (None: `downloads/` in the instance). Files already downloaded stay
    /// where they are.
    pub fn set_downloads_dir(&mut self, dir: Option<PathBuf>) -> Result<()> {
        if let Some(d) = &dir {
            if !d.is_absolute() {
                bail!("the downloads folder must be a full path");
            }
            fs::create_dir_all(d).with_context(|| format!("cannot make {}", d.display()))?;
        }
        let mut config = Instance::open(&self.root)?.config;
        config.downloads_path = dir;
        self.locked(|| write_atomic(&self.root.join(CONFIG), &toml::to_string_pretty(&config)?))?;
        self.config = config;
        Ok(())
    }

    pub fn ini_dir(&self) -> Option<PathBuf> {
        self.config.ini_path.clone().or_else(|| crate::game::my_games_dir(&self.config.local_path))
    }
    /// `name` in the INI folder, in whatever letter case it has there (under Proton the game
    /// writes `Skyrim.INI`); the plain path when there is no such file yet.
    pub fn ini_file(&self, name: &str) -> Option<PathBuf> {
        let dir = self.ini_dir()?;
        let found = std::fs::read_dir(&dir).ok().and_then(|rd| {
            rd.flatten().map(|e| e.path()).find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(name)))
        });
        Some(found.unwrap_or_else(|| dir.join(name)))
    }
    fn profile_dir(&self) -> PathBuf {
        self.root.join("profiles").join(&self.config.profile)
    }
    fn profile_file(&self) -> PathBuf {
        self.profile_dir().join("profile.toml")
    }

    pub fn profile(&self) -> Result<Profile> {
        Ok(toml::from_str(&fs::read_to_string(self.profile_file())?)?)
    }

    /// Profile names, sorted. Each profile has its own mod order, plugin order and undo history;
    /// mods, overwrite/, tools and the LOOT userlist are shared.
    pub fn profiles(&self) -> Result<Vec<String>> {
        let mut names: Vec<String> = fs::read_dir(self.root.join("profiles"))?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().join("profile.toml").is_file())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| !n.starts_with('.'))
            .collect();
        names.sort_by_key(|n| n.to_lowercase());
        Ok(names)
    }

    /// The stored spelling of a profile name (folder names are case-insensitive on Windows).
    fn find_profile(&self, name: &str) -> Result<String> {
        let want = name.to_lowercase();
        self.profiles()?.into_iter().find(|n| n.to_lowercase() == want).with_context(|| format!("no profile '{name}'"))
    }

    /// A new profile: a copy of the active one, or empty (every mod is added, disabled, when it
    /// is first switched to).
    pub fn create_profile(&self, name: &str, copy: bool) -> Result<()> {
        check_profile_name(name)?;
        self.locked(|| {
            if self.find_profile(name).is_ok() {
                bail!("profile '{name}' already exists");
            }
            let p = if copy { self.profile()? } else { Profile::default() };
            let dir = self.root.join("profiles").join(name);
            fs::create_dir_all(&dir)?;
            write_atomic(&dir.join("profile.toml"), &toml::to_string_pretty(&p)?)
        })
    }

    /// Make `name` the active profile. Mods installed while another profile was active are added
    /// at the end, disabled, and mods uninstalled since are dropped. The game folder follows on
    /// the next deploy.
    pub fn switch_profile(&mut self, name: &str) -> Result<()> {
        let mut config = self.config.clone();
        config.profile = self.find_profile(name)?;
        let next = Instance { root: self.root.clone(), config };
        self.locked(|| {
            let before = next.profile()?;
            let mut p = before.clone();
            p.mods.retain(|m| is_separator(&m.id) || next.mod_dir(&m.id).is_dir());
            let mut installed: Vec<String> = fs::read_dir(next.mods_dir())?
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect();
            installed.sort_by_key(|n| n.to_lowercase());
            for id in installed {
                if p.mods.iter().any(|m| m.id == id) {
                    continue;
                }
                for pl in next.mod_files(&id)?.into_iter().filter(|f| !f.contains('/') && crate::install::is_plugin(f)) {
                    if !p.plugins.iter().any(|e| e.name.eq_ignore_ascii_case(&pl)) {
                        p.plugins.push(PluginEntry { name: pl, enabled: false });
                    }
                }
                p.mods.push(ModEntry { id, enabled: false });
            }
            if p != before {
                write_atomic(&next.profile_file(), &toml::to_string_pretty(&p)?)?;
            }
            write_atomic(&self.root.join(CONFIG), &toml::to_string_pretty(&next.config)?)
        })?;
        *self = next;
        Ok(())
    }

    pub fn rename_profile(&mut self, from: &str, to: &str) -> Result<()> {
        check_profile_name(to)?;
        let from = self.find_profile(from)?;
        let mut config = Instance::open(&self.root)?.config;
        self.locked(|| {
            if self.find_profile(to).is_ok_and(|n| n != from) {
                bail!("profile '{to}' already exists");
            }
            let dir = self.root.join("profiles");
            fs::rename(dir.join(&from), dir.join(to))?;
            crate::profile_ini::renamed(self, &from, to)?;
            if config.profile == from {
                config.profile = to.into();
                write_atomic(&self.root.join(CONFIG), &toml::to_string_pretty(&config)?)?;
            }
            Ok(())
        })?;
        self.config = config;
        Ok(())
    }

    /// Move a profile to `profiles/.deleted/`, replacing an earlier deleted one of that name.
    pub fn delete_profile(&self, name: &str) -> Result<()> {
        let name = self.find_profile(name)?;
        self.locked(|| {
            if Instance::open(&self.root)?.config.profile == name {
                bail!("'{name}' is the active profile; switch to another one first");
            }
            let bin = self.root.join("profiles").join(".deleted");
            fs::create_dir_all(&bin)?;
            let _ = fs::remove_dir_all(bin.join(&name));
            fs::rename(self.root.join("profiles").join(&name), bin.join(&name))?;
            Ok(())
        })
    }

    pub fn mod_meta(&self, id: &str) -> ModMeta {
        fs::read_to_string(self.mod_dir(id).join(MOD_META))
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_else(|| ModMeta { name: id.into(), ..Default::default() })
    }

    /// Written whole or not at all, through a temp file in mods/: moving it out changes the time of
    /// mods/ itself, which the window watches, so a rename or a note from the CLI shows there. The
    /// file keeps its time, which is the mod's install date.
    pub fn write_mod_meta(&self, id: &str, meta: &ModMeta) -> Result<()> {
        let path = self.mod_dir(id).join(MOD_META);
        let tmp = self.mods_dir().join(format!(".{id}.{MOD_META}.tmp"));
        fs::write(&tmp, toml::to_string_pretty(meta)?)?;
        if let Ok(t) = fs::metadata(&path).and_then(|m| m.modified()) {
            fs::File::options().write(true).open(&tmp)?.set_modified(t)?;
        }
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Files of an installed mod, as `/`-separated paths relative to the mod folder
    /// (`root/...` for game-root files). Cached in `.rmm-index` because mods are immutable.
    pub fn mod_files(&self, id: &str) -> Result<Vec<String>> {
        let dir = self.mod_dir(id);
        if id == OVERWRITE {
            // Not cached: tools and captures keep adding files here.
            return if dir.is_dir() { index_dir(&dir) } else { Ok(vec![]) };
        }
        // Kept in memory too, keyed by the index file's stamp: a long-lived process (the app)
        // would otherwise reread thousands of index files on every refresh.
        let index = dir.join(MOD_INDEX);
        if let Ok(meta) = fs::metadata(&index) {
            let st = stamp(&meta);
            if let Some((t, files)) = INDEX_CACHE.lock().unwrap().get(&index)
                && *t == st
            {
                return Ok(files.clone());
            }
            if let Ok(text) = fs::read_to_string(&index) {
                let files: Vec<String> = text.lines().map(str::to_owned).collect();
                INDEX_CACHE.lock().unwrap().insert(index, (st, files.clone()));
                return Ok(files);
            }
        }
        if !dir.is_dir() {
            bail!("mod '{id}' is not installed");
        }
        let files = index_dir(&dir)?;
        fs::write(dir.join(MOD_INDEX), files.join("\n"))?;
        Ok(files)
    }

    /// Run `f` while holding the instance write lock (shared by UI, CLI and MCP processes).
    pub fn locked<R>(&self, f: impl FnOnce() -> Result<R>) -> Result<R> {
        let file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(self.root.join(".lock"))?;
        let mut lock = fd_lock::RwLock::new(file);
        let _guard = lock.write()?;
        f()
    }

    /// Mutate the active profile. The previous state is pushed onto the undo stack.
    pub fn edit<R>(&self, label: &str, f: impl FnOnce(&mut Profile) -> Result<R>) -> Result<R> {
        self.locked(|| {
            let before = self.profile()?;
            let mut after = before.clone();
            let r = f(&mut after)?;
            after.apply_locks();
            if after != before {
                self.push("undo", label, &before)?;
                let _ = fs::remove_dir_all(self.stack_dir("redo"));
                write_atomic(&self.profile_file(), &toml::to_string_pretty(&after)?)?;
            }
            Ok(r)
        })
    }

    /// Restore the state before the last change. Returns the undone change's label.
    pub fn undo(&self) -> Result<Option<String>> {
        self.locked(|| self.shift("undo", "redo"))
    }
    pub fn redo(&self) -> Result<Option<String>> {
        self.locked(|| self.shift("redo", "undo"))
    }

    /// Labels of undoable changes, newest first.
    pub fn history(&self) -> Result<Vec<String>> {
        let mut entries = self.stack("undo")?;
        entries.reverse();
        Ok(entries.into_iter().map(|(_, label)| label).collect())
    }

    /// Undoable changes, newest first, each with the mods it touched.
    pub fn history_details(&self) -> Result<Vec<HistoryEntry>> {
        let snaps = self.snapshots()?;
        let mut after = self.profile()?;
        let mut out = Vec::new();
        for (label, before) in snaps.into_iter().rev() {
            out.push(HistoryEntry { label, change: diff(&before, &after) });
            after = before;
        }
        Ok(out)
    }

    /// What restoring the state before history entry `n` (0 = the newest) would change.
    pub fn restore_preview(&self, n: usize) -> Result<Diff> {
        let snaps = self.snapshots()?;
        let (_, target) = snaps.len().checked_sub(n + 1).and_then(|i| snaps.into_iter().nth(i)).context("no such history entry")?;
        Ok(diff(&self.profile()?, &target))
    }

    /// Go back to the state before history entry `n`: `n + 1` undos in one go, so redo can step
    /// forward again. Returns the labels undone.
    pub fn restore(&self, n: usize) -> Result<Vec<String>> {
        self.locked(|| {
            if n >= self.stack("undo")?.len() {
                bail!("no such history entry");
            }
            (0..=n).map(|_| self.shift("undo", "redo").map(Option::unwrap_or_default)).collect()
        })
    }

    /// (label, profile before the change) for every undo entry, oldest first. Mods deleted for good
    /// (the trash emptied) are left out: going back can't bring them, and undo drops them too.
    fn snapshots(&self) -> Result<Vec<(String, Profile)>> {
        let names = |dir: PathBuf| -> std::collections::HashSet<String> { fs::read_dir(dir).into_iter().flatten().flatten().filter_map(|e| e.file_name().into_string().ok()).collect() };
        let kept: std::collections::HashSet<String> = names(self.mods_dir()).into_iter().chain(names(self.root.join("trash"))).collect();
        self.stack("undo")?
            .into_iter()
            .map(|(path, label)| {
                let mut p: Profile = toml::from_str(&fs::read_to_string(path)?)?;
                p.mods.retain(|m| is_separator(&m.id) || kept.contains(&m.id));
                Ok((label, p))
            })
            .collect()
    }

    /// Move a mod folder to trash/ so an undo can bring it back.
    pub fn trash_mod(&self, id: &str) -> Result<()> {
        let dest = self.root.join("trash").join(id);
        let _ = fs::remove_dir_all(&dest);
        fs::rename(self.mod_dir(id), dest)?;
        Ok(())
    }

    fn shift(&self, from: &str, to: &str) -> Result<Option<String>> {
        let Some((path, label)) = self.stack(from)?.pop() else { return Ok(None) };
        let mut snapshot: Profile = toml::from_str(&fs::read_to_string(&path)?)?;
        let current = self.profile()?;
        self.push(to, &label, &current)?;
        fs::remove_file(&path)?;
        for m in &snapshot.mods {
            let trashed = self.root.join("trash").join(&m.id);
            if !self.mod_dir(&m.id).exists() && trashed.exists() {
                fs::rename(trashed, self.mod_dir(&m.id))?;
            }
        }
        // A mod whose files are gone for good (the trash was emptied) leaves the list.
        snapshot.mods.retain(|m| is_separator(&m.id) || self.mod_dir(&m.id).is_dir());
        write_atomic(&self.profile_file(), &toml::to_string_pretty(&snapshot)?)?;
        // Mods the step takes off the list (an install undone, an uninstall redone) go back to
        // trash/, unless another profile lists them; otherwise they'd stay installed unseen.
        let kept: std::collections::HashSet<&str> = snapshot.mods.iter().map(|m| m.id.as_str()).collect();
        let gone: Vec<&str> = current.mods.iter().map(|m| m.id.as_str()).filter(|id| !kept.contains(id) && !is_separator(id) && self.mod_dir(id).is_dir()).collect();
        if !gone.is_empty() {
            let mut listed = std::collections::HashSet::new();
            for name in self.profiles()?.into_iter().filter(|n| *n != self.config.profile) {
                let p: Profile = toml::from_str(&fs::read_to_string(self.root.join("profiles").join(name).join("profile.toml"))?)?;
                listed.extend(p.mods.into_iter().map(|m| m.id));
            }
            for id in gone.into_iter().filter(|id| !listed.contains(*id)) {
                self.trash_mod(id)?;
            }
        }
        Ok(Some(label))
    }

    fn stack_dir(&self, name: &str) -> PathBuf {
        self.profile_dir().join("history").join(name)
    }

    /// (file, label) oldest first.
    fn stack(&self, name: &str) -> Result<Vec<(PathBuf, String)>> {
        let Ok(rd) = fs::read_dir(self.stack_dir(name)) else { return Ok(vec![]) };
        let mut files: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
        files.sort();
        Ok(files
            .into_iter()
            .map(|p| {
                let label = fs::read_to_string(&p)
                    .ok()
                    .and_then(|t| t.lines().next().and_then(|l| l.strip_prefix("# ")).map(str::to_owned))
                    .unwrap_or_default();
                (p, label)
            })
            .collect())
    }

    fn push(&self, name: &str, label: &str, p: &Profile) -> Result<()> {
        let dir = self.stack_dir(name);
        fs::create_dir_all(&dir)?;
        let existing = self.stack(name)?;
        let next = existing
            .last()
            .and_then(|(f, _)| f.file_stem()?.to_str()?.parse::<u64>().ok())
            .map_or(0, |n| n + 1);
        let body = format!("# {}\n{}", label.replace('\n', " "), toml::to_string_pretty(p)?);
        fs::write(dir.join(format!("{next:010}.toml")), body)?;
        for (old, _) in existing.iter().take((existing.len() + 1).saturating_sub(crate::settings::get().history.limit)) {
            fs::remove_file(old)?;
        }
        Ok(())
    }
}

/// Profile names are folder names: no path characters, no leading dot (`.deleted` is rmm's).
fn check_profile_name(name: &str) -> Result<()> {
    let bad = name.is_empty()
        || name.len() > 64
        || name.trim() != name
        || name.starts_with('.')
        || name.ends_with('.')
        || name.chars().any(|c| r#"<>:"/\|?*"#.contains(c) || c.is_control());
    if bad {
        bail!("invalid profile name '{name}': up to 64 characters, no <>:\"/\\|?*, no leading dot or surrounding spaces");
    }
    Ok(())
}

pub(crate) fn index_dir(dir: &Path) -> Result<Vec<String>> {
    let mut files = Vec::new();
    for e in walkdir::WalkDir::new(dir).min_depth(1) {
        let e = e?;
        if !e.file_type().is_file() {
            continue;
        }
        let rel = e.path().strip_prefix(dir)?.to_string_lossy().replace('\\', "/");
        if rel != MOD_META && rel != MOD_INDEX {
            files.push(rel);
        }
    }
    files.sort();
    Ok(files)
}

pub(crate) fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, text)?;
    fs::rename(tmp, path)?;
    Ok(())
}
