//! `modlist.lock.toml`: a setup written down so it can be checked or rebuilt on another machine.
//! Each mod records its archive (name and sha256), its FOMOD choices, its Nexus ids and a
//! fingerprint of its installed files; the mod order, enabled state, colours, plugin order and
//! mod rules come along.
//!
//! Applying a lockfile installs what is missing from archives found in the given folders, lists
//! what still has to be downloaded, and sets the order. Mods the user added that the lockfile
//! doesn't list are kept, each after the mod it followed, so updating a curated list doesn't
//! throw away personal additions.
use crate::fomod::Choices;
use crate::instance::{ModEntry, NexusRef, PluginEntry, is_separator};
use crate::rules::ModRule;
use crate::{Game, Instance};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

pub const FILE: &str = "modlist.lock.toml";

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Lockfile {
    pub version: u32,
    pub game: Game,
    /// Priority order, lowest first; separators included.
    pub mods: Vec<LockedMod>,
    #[serde(default)]
    pub plugins: Vec<PluginEntry>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub colors: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<ModRule>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct LockedMod {
    pub id: String,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The archive's file name (its folder differs between machines).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Made by this tool's runs rather than installed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// sha256 over the installed files' paths and sizes: the same value means the same install.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub files: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fomod: Option<Choices>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nexus: Option<NexusRef>,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// The active profile as a lockfile.
pub fn export(inst: &Instance) -> Result<Lockfile> {
    let p = inst.profile()?;
    let mut mods = Vec::new();
    for (i, m) in p.mods.iter().enumerate() {
        crate::progress::report("fingerprinting mods", i, p.mods.len());
        if is_separator(&m.id) {
            mods.push(LockedMod { id: m.id.clone(), ..Default::default() });
            continue;
        }
        let meta = inst.mod_meta(&m.id);
        let tool = meta.source.as_deref().and_then(|s| s.strip_prefix("tool:")).map(str::to_owned);
        mods.push(LockedMod {
            id: m.id.clone(),
            enabled: m.enabled,
            name: Some(meta.name),
            archive: meta.source.filter(|_| tool.is_none()).and_then(|s| Path::new(&s).file_name().map(|f| f.to_string_lossy().into_owned())),
            sha256: meta.sha256,
            tool,
            content: Some(content_hash(inst, &m.id)?),
            files: inst.mod_files(&m.id)?.len(),
            fomod: meta.fomod,
            nexus: meta.nexus,
        });
    }
    Ok(Lockfile { version: 1, game: inst.config.game, mods, plugins: p.plugins, colors: p.colors, rules: crate::rules::rules(inst)? })
}

/// Write the active profile's lockfile (default: `modlist.lock.toml` in the instance).
pub fn write(inst: &Instance, path: Option<&Path>) -> Result<PathBuf> {
    let path = path.map(Path::to_path_buf).unwrap_or_else(|| inst.root.join(FILE));
    fs::write(&path, toml::to_string_pretty(&export(inst)?)?)?;
    Ok(path)
}

pub fn read(path: &Path) -> Result<Lockfile> {
    let lock: Lockfile = toml::from_str(&fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?)?;
    if lock.version != 1 {
        bail!("{} is lockfile version {}; this rmm reads version 1", path.display(), lock.version);
    }
    Ok(lock)
}

#[derive(Serialize, Debug, Default)]
pub struct ApplyReport {
    pub dry_run: bool,
    /// Installed from archives found in the given folders (or that would be).
    pub install: Vec<String>,
    /// Installed already, with the locked files.
    pub ok: usize,
    /// Installed under the locked id but with other files (another version or other FOMOD
    /// choices): left as they are.
    pub differs: Vec<String>,
    /// Not installed and no archive found.
    pub missing: Vec<Missing>,
    /// Mods in the profile the lockfile doesn't list: kept, each after the mod it followed.
    pub kept: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Serialize, Debug)]
pub struct Missing {
    pub id: String,
    pub name: Option<String>,
    pub archive: Option<String>,
    /// For tool output: the tool whose run makes it.
    pub tool: Option<String>,
    /// Where to download it, for Nexus mods.
    pub url: Option<String>,
}

/// Bring the active profile to `lock`: install missing mods from archives found (by file name,
/// checked against the recorded sha256) in `downloads` and their subfolders, then set the order,
/// enabled state, colours, plugin order and mod rules in one undoable step.
pub fn apply(inst: &Instance, lock: &Lockfile, downloads: &[PathBuf], dry_run: bool) -> Result<ApplyReport> {
    if lock.game != inst.config.game {
        bail!("the lockfile is for {:?}, this instance for {:?}", lock.game, inst.config.game);
    }
    let mut r = ApplyReport { dry_run, ..Default::default() };
    let mut archives: HashMap<String, PathBuf> = HashMap::new();
    for d in downloads {
        for e in walkdir::WalkDir::new(d).max_depth(3).into_iter().filter_map(|e| e.ok()).filter(|e| e.file_type().is_file()) {
            archives.entry(e.file_name().to_string_lossy().to_lowercase()).or_insert_with(|| e.path().to_path_buf());
        }
    }
    let domain = inst.config.game.nexus_domain();
    let total = lock.mods.len();
    for (i, m) in lock.mods.iter().enumerate() {
        crate::progress::report(if dry_run { "checking mods" } else { "installing mods" }, i, total);
        if is_separator(&m.id) {
            continue;
        }
        if inst.mod_dir(&m.id).exists() {
            if m.content.as_ref().is_some_and(|c| *c != content_hash(inst, &m.id).unwrap_or_default()) {
                r.differs.push(m.id.clone());
            } else {
                r.ok += 1;
            }
            continue;
        }
        let found = m.archive.as_ref().and_then(|a| archives.get(&a.to_lowercase()));
        let found = match (found, &m.sha256) {
            (Some(path), Some(want)) if sha256_file(path)? != *want => {
                r.warnings.push(format!("{}: {} isn't the locked file (its sha256 differs)", m.id, path.display()));
                None
            }
            (f, _) => f,
        };
        let Some(path) = found else {
            r.missing.push(Missing {
                id: m.id.clone(),
                name: m.name.clone(),
                archive: m.archive.clone(),
                tool: m.tool.clone(),
                url: m.nexus.as_ref().map(|n| {
                    let file = n.file_id.map(|f| format!("&file_id={f}")).unwrap_or_default();
                    format!("https://www.nexusmods.com/{domain}/mods/{}?tab=files{file}", n.mod_id)
                }),
            });
            continue;
        };
        r.install.push(m.id.clone());
        if dry_run {
            continue;
        }
        let name = m.name.as_deref().unwrap_or(&m.id);
        crate::install::install_as(inst, path, &m.id, name, m.enabled, Some(&m.fomod.clone().unwrap_or_default()))
            .with_context(|| format!("installing {}", m.id))?;
        if m.content.as_ref().is_some_and(|c| *c != content_hash(inst, &m.id).unwrap_or_default()) {
            r.warnings.push(format!("{}: installed, but its files differ from the locked ones", m.id));
        }
    }

    let locked: HashSet<&str> = lock.mods.iter().map(|m| m.id.as_str()).collect();
    let profile = inst.profile()?;
    r.kept = profile.mods.iter().filter(|m| !locked.contains(m.id.as_str())).map(|m| m.id.clone()).collect();
    if dry_run {
        return Ok(r);
    }
    let here = |id: &str| is_separator(id) || inst.mod_dir(id).exists();
    inst.edit("apply lockfile", |p| {
        // Each user mod keeps its place after the nearest locked mod above it.
        let mut after: HashMap<Option<String>, Vec<ModEntry>> = HashMap::new();
        let mut anchor: Option<String> = None;
        for m in &p.mods {
            if locked.contains(m.id.as_str()) {
                anchor = Some(m.id.clone());
            } else {
                after.entry(anchor.clone()).or_default().push(m.clone());
            }
        }
        let mut mods = after.remove(&None).unwrap_or_default();
        for m in lock.mods.iter().filter(|m| here(&m.id)) {
            mods.push(ModEntry { id: m.id.clone(), enabled: m.enabled && !is_separator(&m.id) });
            mods.extend(after.remove(&Some(m.id.clone())).unwrap_or_default());
        }
        p.mods = mods;
        p.colors.extend(lock.colors.clone());
        let listed: HashSet<String> = lock.plugins.iter().map(|e| e.name.to_lowercase()).collect();
        let others: Vec<PluginEntry> = p.plugins.iter().filter(|e| !listed.contains(&e.name.to_lowercase())).cloned().collect();
        p.plugins = lock.plugins.iter().cloned().chain(others).collect();
        Ok(())
    })?;
    for rule in &lock.rules {
        if let Err(e) = crate::rules::set_rule(inst, rule) {
            r.warnings.push(format!("rule for {}: {e:#}", rule.id));
        }
    }
    Ok(r)
}

/// sha256 over a mod's file paths (lowercase) and sizes. Cheap: no file is read.
pub fn content_hash(inst: &Instance, id: &str) -> Result<String> {
    let dir = inst.mod_dir(id);
    let mut h = Sha256::new();
    for f in inst.mod_files(id)? {
        h.update(f.to_lowercase().as_bytes());
        h.update([0]);
        h.update(fs::metadata(dir.join(&f))?.len().to_le_bytes());
    }
    Ok(hex(&h.finalize()))
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f = fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
