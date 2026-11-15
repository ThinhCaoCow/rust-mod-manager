use crate::Game;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

const CONFIG: &str = "instance.toml";
const MOD_META: &str = "rmm-mod.toml";
const MOD_INDEX: &str = ".rmm-index";
// ponytail: fixed undo depth; make it a setting if anyone hits it.
const HISTORY_LIMIT: usize = 200;

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
            },
        };
        fs::write(root.join(CONFIG), toml::to_string_pretty(&inst.config)?)?;
        write_atomic(&inst.profile_file(), &toml::to_string_pretty(&Profile::default())?)?;
        Ok(inst)
    }

    pub fn open(root: &Path) -> Result<Self> {
        let text = fs::read_to_string(root.join(CONFIG))
            .with_context(|| format!("{} is not an rmm instance (no {CONFIG})", root.display()))?;
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
    pub fn ini_dir(&self) -> Option<PathBuf> {
        self.config.ini_path.clone().or_else(|| crate::game::my_games_dir(&self.config.local_path))
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
            p.mods.retain(|m| next.mod_dir(&m.id).is_dir());
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

    pub fn write_mod_meta(&self, id: &str, meta: &ModMeta) -> Result<()> {
        fs::write(self.mod_dir(id).join(MOD_META), toml::to_string_pretty(meta)?)?;
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
        if let Ok(text) = fs::read_to_string(dir.join(MOD_INDEX)) {
            return Ok(text.lines().map(str::to_owned).collect());
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

    /// Move a mod folder to trash/ so an undo can bring it back.
    pub fn trash_mod(&self, id: &str) -> Result<()> {
        let dest = self.root.join("trash").join(id);
        let _ = fs::remove_dir_all(&dest);
        fs::rename(self.mod_dir(id), dest)?;
        Ok(())
    }

    fn shift(&self, from: &str, to: &str) -> Result<Option<String>> {
        let Some((path, label)) = self.stack(from)?.pop() else { return Ok(None) };
        let snapshot: Profile = toml::from_str(&fs::read_to_string(&path)?)?;
        self.push(to, &label, &self.profile()?)?;
        fs::remove_file(&path)?;
        write_atomic(&self.profile_file(), &toml::to_string_pretty(&snapshot)?)?;
        for m in &snapshot.mods {
            let trashed = self.root.join("trash").join(&m.id);
            if !self.mod_dir(&m.id).exists() && trashed.exists() {
                fs::rename(trashed, self.mod_dir(&m.id))?;
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
        for (old, _) in existing.iter().take((existing.len() + 1).saturating_sub(HISTORY_LIMIT)) {
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
