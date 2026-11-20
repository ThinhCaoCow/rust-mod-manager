//! `modlist.lock.toml`: a setup written down so it can be checked or rebuilt on another machine.
//! Each mod records its archive (name and sha256), its FOMOD choices, its Nexus ids and a
//! fingerprint of its installed files; the mod order, enabled state, colours, plugin order, mod
//! rules, notes and the game, script extender and Address Library versions come along.
//!
//! A pack is a zip of the lockfile with the files of the mods no archive brings back (patches made
//! by hand or from Overwrite, an xEdit patch), so a friend gets those too.
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
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub const FILE: &str = "modlist.lock.toml";
pub const PACK: &str = "modlist.pack.zip";
pub const MARKDOWN: &str = "modlist.md";

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Lockfile {
    pub version: u32,
    pub game: Game,
    #[serde(default, skip_serializing_if = "Env::is_empty")]
    pub env: Env,
    /// Priority order, lowest first; separators included.
    pub mods: Vec<LockedMod>,
    #[serde(default)]
    pub plugins: Vec<PluginEntry>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub colors: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<ModRule>,
    /// The pack this was read from, whose mods `apply` installs.
    #[serde(skip)]
    pub pack: Option<PathBuf>,
}

/// What the setup was made with. Script extender plugins and Address Library are built for one
/// game version, so a lockfile from another one may not work as it did.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct Env {
    /// The game exe's version, "1.6.1170.0".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game_version: Option<String>,
    /// The script extender's runtime DLLs in the game folder, whose names say the game version
    /// they're built for ("skse64_1_6_1170.dll").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub script_extender: Vec<String>,
    /// The game versions Address Library has files for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub address_library: Vec<String>,
}

impl Env {
    fn is_empty(&self) -> bool {
        *self == Env::default()
    }
}

/// The game, script extender and Address Library versions in use now.
pub fn env(inst: &Instance) -> Env {
    let (root, game) = (&inst.config.game_path, inst.config.game);
    let dotted = |v: &[u32; 4]| v.map(|x| x.to_string()).join(".");
    let se = if game.base() == Game::SkyrimSe { "SKSE" } else { "F4SE" };
    let mut script_extender: Vec<String> = crate::launch::runtimes(root, game).unwrap_or_default().into_iter().map(|(n, _)| n).collect();
    script_extender.sort();
    Env {
        game_version: crate::launch::game_version(root, game).ok().map(|v| dotted(&v)),
        script_extender,
        address_library: crate::launch::address_library_versions(&inst.data_dir().join(se).join("Plugins")).iter().map(dotted).collect(),
    }
}

/// What differs between the versions a lockfile was made with and these.
fn env_warnings(locked: &Env, now: &Env) -> Vec<String> {
    let mut w = Vec::new();
    if let (Some(a), Some(b)) = (&locked.game_version, &now.game_version)
        && a != b
    {
        w.push(format!("the lockfile was made with game version {a}, this game is {b}: script extender plugins and Address Library are built for one game version, so some mods may not load (Problems says which)"));
    }
    if !locked.script_extender.is_empty() && !now.script_extender.is_empty() && locked.script_extender != now.script_extender {
        w.push(format!("the lockfile was made with the script extender {}, this game folder has {}", locked.script_extender.join(", "), now.script_extender.join(", ")));
    }
    w
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

impl LockedMod {
    /// The name to show: the mod's, or a separator's without `sep:`.
    pub fn label(&self) -> &str {
        self.name.as_deref().unwrap_or(self.id.strip_prefix("sep:").unwrap_or(&self.id))
    }
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
            notes: meta.notes.filter(|n| !n.trim().is_empty()),
        });
    }
    Ok(Lockfile {
        version: 1,
        game: inst.config.game,
        env: env(inst),
        mods,
        plugins: p.plugins,
        colors: p.colors,
        rules: crate::rules::rules(inst)?,
        pack: None,
    })
}

/// What `write` makes: the lockfile, a pack (the lockfile and the files of mods no archive brings
/// back), or a readable list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    #[default]
    Toml,
    Pack,
    Markdown,
}

#[derive(Serialize, Debug)]
pub struct Written {
    pub path: PathBuf,
    /// The mods whose files are in the pack.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub packed: Vec<String>,
}

/// Write the active profile as `format` (default: `modlist.lock.toml`, `modlist.pack.zip` or
/// `modlist.md` in the instance).
pub fn write(inst: &Instance, path: Option<&Path>, format: Format) -> Result<Written> {
    let lock = export(inst)?;
    let path = path.map(Path::to_path_buf).unwrap_or_else(|| {
        inst.root.join(match format {
            Format::Toml => FILE,
            Format::Pack => PACK,
            Format::Markdown => MARKDOWN,
        })
    });
    let mut packed = Vec::new();
    match format {
        Format::Toml => fs::write(&path, toml::to_string_pretty(&lock)?)?,
        Format::Markdown => fs::write(&path, markdown(&lock))?,
        Format::Pack => packed = write_pack(inst, &lock, &path)?,
    }
    Ok(Written { path, packed })
}

/// Mods no archive brings back: made by hand, from Overwrite or installed from a folder, and a
/// tool's output when it's plugins only (an xEdit patch). Other tool output is rebuilt by
/// running the tool.
fn packable(inst: &Instance, m: &LockedMod) -> bool {
    let from_archive = m.sha256.is_some() || m.archive.as_deref().is_some_and(is_archive);
    if is_separator(&m.id) || from_archive || !inst.mod_dir(&m.id).is_dir() {
        return false;
    }
    m.tool.is_none() || inst.mod_files(&m.id).is_ok_and(|f| !f.is_empty() && f.iter().all(|f| crate::install::is_plugin(f)))
}

/// An archive's name, not a folder's (a mod installed from a folder records the folder's).
fn is_archive(name: &str) -> bool {
    let name = name.to_lowercase();
    [".7z", ".zip", ".rar"].iter().any(|e| name.ends_with(e))
}

fn write_pack(inst: &Instance, lock: &Lockfile, path: &Path) -> Result<Vec<String>> {
    let mut zip = zip::ZipWriter::new(fs::File::create(path).with_context(|| format!("cannot write {}", path.display()))?);
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated).large_file(true);
    zip.start_file(FILE, opts)?;
    zip.write_all(toml::to_string_pretty(lock)?.as_bytes())?;
    let mut packed = Vec::new();
    for m in lock.mods.iter().filter(|m| packable(inst, m)) {
        let dir = inst.mod_dir(&m.id);
        for f in inst.mod_files(&m.id)? {
            zip.start_file(format!("mods/{}/{}", m.id, f.replace('\\', "/")), opts)?;
            std::io::copy(&mut fs::File::open(dir.join(&f))?, &mut zip)?;
        }
        packed.push(m.id.clone());
    }
    zip.finish()?;
    Ok(packed)
}

/// Read a lockfile, or the one in a pack.
pub fn read(path: &Path) -> Result<Lockfile> {
    let is_zip = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip"));
    let text = if is_zip {
        let mut zip = zip::ZipArchive::new(fs::File::open(path).with_context(|| format!("cannot read {}", path.display()))?)?;
        let mut text = String::new();
        zip.by_name(FILE).with_context(|| format!("{} has no {FILE}: it isn't an rmm pack", path.display()))?.read_to_string(&mut text)?;
        text
    } else {
        fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?
    };
    let mut lock: Lockfile = toml::from_str(&text)?;
    if lock.version != 1 {
        bail!("{} is lockfile version {}; this rmm reads version 1", path.display(), lock.version);
    }
    lock.pack = is_zip.then(|| path.to_path_buf());
    Ok(lock)
}

/// The mod ids a pack has files for.
fn pack_ids(pack: &Path) -> Result<HashSet<String>> {
    let zip = zip::ZipArchive::new(fs::File::open(pack)?)?;
    Ok(zip.file_names().filter_map(|n| n.ok()?.strip_prefix("mods/")?.split_once('/').map(|(id, _)| id.to_string())).collect())
}

/// What changed from one lockfile to another (or to the profile now).
#[derive(Serialize, Debug, Default, PartialEq)]
pub struct Diff {
    /// "game version: 1.6.640.0 → 1.6.1170.0" and the like.
    pub versions: Vec<String>,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    /// In both, with other files: another version, or other FOMOD choices.
    pub changed: Vec<String>,
    pub turned_on: Vec<String>,
    pub turned_off: Vec<String>,
    /// In both, at another place among the others.
    pub moved: Vec<String>,
    pub plugins_added: Vec<String>,
    pub plugins_removed: Vec<String>,
    pub plugins_on: Vec<String>,
    pub plugins_off: Vec<String>,
    pub plugins_moved: Vec<String>,
}

pub fn diff(old: &Lockfile, new: &Lockfile) -> Diff {
    let mut d = Diff::default();
    let pair = |what: &str, a: String, b: String| (a != b && !a.is_empty() && !b.is_empty()).then(|| format!("{what}: {a} → {b}"));
    d.versions.extend(pair("game version", old.env.game_version.clone().unwrap_or_default(), new.env.game_version.clone().unwrap_or_default()));
    d.versions.extend(pair("script extender", old.env.script_extender.join(", "), new.env.script_extender.join(", ")));
    d.versions.extend(pair("Address Library", old.env.address_library.join(", "), new.env.address_library.join(", ")));

    let mods = |l: &'_ Lockfile| l.mods.iter().filter(|m| !is_separator(&m.id)).map(|m| (m.id.clone(), m.clone())).collect::<Vec<_>>();
    let (a, b) = (mods(old), mods(new));
    let (am, bm): (HashMap<_, _>, HashMap<_, _>) = (a.iter().cloned().collect(), b.iter().cloned().collect());
    d.added = b.iter().filter(|(id, _)| !am.contains_key(id)).map(|(_, m)| m.label().to_string()).collect();
    d.removed = a.iter().filter(|(id, _)| !bm.contains_key(id)).map(|(_, m)| m.label().to_string()).collect();
    for (id, m) in &b {
        let Some(o) = am.get(id) else { continue };
        if o.content.is_some() && m.content.is_some() && o.content != m.content || o.sha256.is_some() && m.sha256.is_some() && o.sha256 != m.sha256 {
            d.changed.push(m.label().to_string());
        }
        match (o.enabled, m.enabled) {
            (false, true) => d.turned_on.push(m.label().to_string()),
            (true, false) => d.turned_off.push(m.label().to_string()),
            _ => {}
        }
    }
    let ids = |v: &[(String, LockedMod)]| v.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>();
    d.moved = moved(&ids(&a), &ids(&b)).into_iter().map(|id| bm[&id].label().to_string()).collect();

    let plugins = |l: &Lockfile| l.plugins.iter().map(|p| (p.name.to_lowercase(), p.clone())).collect::<Vec<_>>();
    let (a, b) = (plugins(old), plugins(new));
    let (am, bm): (HashMap<_, _>, HashMap<_, _>) = (a.iter().cloned().collect(), b.iter().cloned().collect());
    d.plugins_added = b.iter().filter(|(k, _)| !am.contains_key(k)).map(|(_, p)| p.name.clone()).collect();
    d.plugins_removed = a.iter().filter(|(k, _)| !bm.contains_key(k)).map(|(_, p)| p.name.clone()).collect();
    for (k, p) in &b {
        match (am.get(k).map(|o| o.enabled), p.enabled) {
            (Some(false), true) => d.plugins_on.push(p.name.clone()),
            (Some(true), false) => d.plugins_off.push(p.name.clone()),
            _ => {}
        }
    }
    let keys = |v: &[(String, PluginEntry)]| v.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>();
    d.plugins_moved = moved(&keys(&a), &keys(&b)).into_iter().map(|k| bm[&k].name.clone()).collect();
    d
}

/// The keys in both lists that changed place: those outside a longest run kept in order, in the
/// new list's order.
fn moved(old: &[String], new: &[String]) -> Vec<String> {
    let at: HashMap<&str, usize> = old.iter().enumerate().map(|(i, k)| (k.as_str(), i)).collect();
    let both: Vec<(&String, usize)> = new.iter().filter_map(|k| at.get(k.as_str()).map(|&i| (k, i))).collect();
    // Longest increasing subsequence of the old positions (patience sorting).
    let (mut tails, mut prev): (Vec<usize>, Vec<Option<usize>>) = (Vec::new(), vec![None; both.len()]);
    for (j, &(_, i)) in both.iter().enumerate() {
        let p = tails.partition_point(|&t| both[t].1 < i);
        prev[j] = p.checked_sub(1).map(|p| tails[p]);
        if p == tails.len() { tails.push(j) } else { tails[p] = j }
    }
    let mut keep = vec![false; both.len()];
    let mut cur = tails.last().copied();
    while let Some(j) = cur {
        keep[j] = true;
        cur = prev[j];
    }
    both.iter().zip(keep).filter(|(_, k)| !k).map(|((key, _), _)| (*key).clone()).collect()
}

/// A readable list for a post or a friend: the mods under their separators with links and notes,
/// then the plugins.
pub fn markdown(lock: &Lockfile) -> String {
    let domain = lock.game.nexus_domain();
    let mods: Vec<&LockedMod> = lock.mods.iter().filter(|m| !is_separator(&m.id)).collect();
    let mut s = format!("# {} mod list\n\n", lock.game.steam_dir());
    let mut made = Vec::new();
    if let Some(v) = &lock.env.game_version {
        made.push(format!("game version {v}"));
    }
    if !lock.env.script_extender.is_empty() {
        made.push(format!("script extender {}", lock.env.script_extender.join(", ")));
    }
    if !lock.env.address_library.is_empty() {
        made.push(format!("Address Library for {}", lock.env.address_library.join(", ")));
    }
    if !made.is_empty() {
        s += &format!("Made with {}.\n\n", made.join("; "));
    }
    s += &format!(
        "{} mods ({} on), {} plugins ({} on). Mods further down the list win file conflicts.\n",
        mods.len(),
        mods.iter().filter(|m| m.enabled).count(),
        lock.plugins.len(),
        lock.plugins.iter().filter(|p| p.enabled).count()
    );
    let mut heading = false;
    for m in &lock.mods {
        if is_separator(&m.id) {
            s += &format!("\n## {}\n\n", m.label());
            heading = true;
            continue;
        }
        if !heading {
            s += "\n## Mods\n\n";
            heading = true;
        }
        let name = match &m.nexus {
            Some(n) => format!("[{}](https://www.nexusmods.com/{domain}/mods/{})", m.label(), n.mod_id),
            None => m.label().to_string(),
        };
        let mut extra = Vec::new();
        if let Some(t) = &m.tool {
            extra.push(format!("made by {t}"));
        } else if let Some(a) = m.archive.as_ref().filter(|a| is_archive(a)) {
            extra.push(format!("`{a}`"));
        }
        if let Some(n) = m.fomod.as_ref().map(|c| c.options.iter().flat_map(|s| &s.groups).map(|g| g.choices.len()).sum::<usize>()).filter(|&n| n > 0) {
            extra.push(format!("{n} installer choices"));
        }
        let extra = if extra.is_empty() { String::new() } else { format!(" ({})", extra.join(", ")) };
        s += &format!("- [{}] {name}{extra}\n", if m.enabled { "x" } else { " " });
        if let Some(n) = &m.notes {
            for line in n.trim().lines() {
                s += &format!("  > {line}\n");
            }
        }
    }
    s += "\n## Plugins\n\n";
    for (i, p) in lock.plugins.iter().enumerate() {
        s += &format!("{}. {}{}\n", i + 1, p.name, if p.enabled { "" } else { " (off)" });
    }
    s
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
    // A lockfile comes from someone else: its ids become folder names in mods/.
    if let Some(m) = lock.mods.iter().filter(|m| !is_separator(&m.id)).find(|m| {
        let mut c = std::path::Path::new(&m.id).components();
        m.id.contains(['/', '\\', ':']) || !matches!((c.next(), c.next()), (Some(std::path::Component::Normal(_)), None))
    }) {
        bail!("the lockfile names a mod '{}', which isn't a folder name", m.id);
    }
    let mut r = ApplyReport { dry_run, ..Default::default() };
    r.warnings = env_warnings(&lock.env, &env(inst));
    let packed = match &lock.pack {
        Some(p) => pack_ids(p)?,
        None => HashSet::new(),
    };
    // Unpacked on the first mod that needs it.
    let staging = inst.root.join(".pack");
    let mut unpacked = false;
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
        let from_pack = found.is_none() && packed.contains(&m.id);
        if from_pack {
            r.install.push(m.id.clone());
            if dry_run {
                continue;
            }
            if !unpacked {
                let _ = fs::remove_dir_all(&staging);
                zip::ZipArchive::new(fs::File::open(lock.pack.as_ref().unwrap())?)?.extract(&staging).context("unpacking the pack")?;
                unpacked = true;
            }
            let name = m.name.as_deref().unwrap_or(&m.id);
            let installed = crate::install::install_as(inst, &staging.join("mods").join(&m.id), &m.id, name, m.enabled, None);
            if let Err(e) = installed {
                let _ = fs::remove_dir_all(&staging);
                return Err(e.context(format!("installing {} from the pack", m.id)));
            }
            // Its files came from the pack, not a folder that is about to go.
            let mut meta = inst.mod_meta(&m.id);
            meta.source = m.tool.as_ref().map(|t| format!("tool:{t}"));
            inst.write_mod_meta(&m.id, &meta)?;
            continue;
        }
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

    if unpacked {
        let _ = fs::remove_dir_all(&staging);
    }
    if !dry_run {
        // Notes and Nexus ids the installed mods lack.
        for m in lock.mods.iter().filter(|m| (m.notes.is_some() || m.nexus.is_some()) && !is_separator(&m.id) && inst.mod_dir(&m.id).is_dir()) {
            let mut meta = inst.mod_meta(&m.id);
            let before = (meta.notes.clone(), meta.nexus.clone());
            meta.notes = meta.notes.filter(|n| !n.trim().is_empty()).or(m.notes.clone());
            meta.nexus = meta.nexus.or(m.nexus.clone());
            if (meta.notes.clone(), meta.nexus.clone()) != before {
                inst.write_mod_meta(&m.id, &meta)?;
            }
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
