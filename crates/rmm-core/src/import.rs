//! Bring in a setup from Mod Organizer 2 or Vortex: mod folders (hard-linked when they share a
//! volume, copied otherwise), names, Nexus ids, FOMOD choices, notes, mod order and enabled state,
//! separators, plugin order and locks, and Vortex's mod rules. From MO2 also: every profile at
//! once, each profile's own INIs and saves, its Executables as tools, the mods its programs
//! wrote as those tools' outputs, ignored update versions, and hidden files left out. The formats
//! are written up in `docs/internals/mo2.md` and `docs/internals/vortex.md` (§9.4 in each).
//!
//! Mods go in under their own names; one that is already installed under that id is left as it
//! is, so importing again only adds what is new. The profile change is one undoable step.
use crate::install::sanitize;
use crate::instance::{InstanceConfig, ModEntry, ModMeta, NexusRef, OVERWRITE, PluginEntry, Profile, index_dir};
use crate::rules::ModRule;
use crate::tools::Tool;
use crate::{Game, Instance};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Serialize, Debug, Default)]
pub struct ImportReport {
    pub dry_run: bool,
    /// The MO2 or Vortex profile read.
    pub profile: String,
    /// Mods brought in (or that would be), lowest priority first.
    pub mods: Vec<String>,
    pub separators: usize,
    /// Mods already installed under the same id, left as they are.
    pub existing: Vec<String>,
    pub plugins: usize,
    pub rules: usize,
    /// The rmm profile it went into.
    pub into: String,
    /// MO2's Executables added as tools (their titles).
    pub tools: Vec<String>,
    /// Mods made by a program, now that tool's output ("name (tool id)"): its next run in rmm
    /// replaces them in their place.
    pub outputs: Vec<String>,
    /// The MO2 profile's own INIs and saves copied in (or that would be).
    pub inis: usize,
    pub saves: usize,
    /// Files MO2 had hidden (`.mohidden`), kept hidden in their mods.
    pub hidden: usize,
    /// Files hard-linked (sharing the source's disk space) and copied.
    pub linked: usize,
    pub copied: usize,
    /// The files of the mods brought in, and the bytes of those copied rather than linked (on
    /// another drive than the instance): new disk space.
    pub files: usize,
    pub copy_bytes: u64,
    /// Files in MO2's overwrite folder, which go to rmm's overwrite/.
    pub overwrite: usize,
    /// Mods already installed under an imported mod's id, with both versions.
    pub clashes: Vec<Clash>,
    /// The profile's mods the import doesn't name: they stay, after the imported list, so they
    /// win their file conflicts with it.
    pub after: Vec<String>,
    pub warnings: Vec<String>,
}

/// A mod being imported whose id is installed already: the installed one stays.
#[derive(Serialize, Debug)]
pub struct Clash {
    pub id: String,
    /// The installed mod's version and file count.
    pub have: Option<String>,
    pub have_files: usize,
    /// The imported one's.
    pub incoming: Option<String>,
    pub incoming_files: usize,
}

/// What a folder would bring in: (files, bytes, hidden files), as `link_tree` counts them.
fn count_tree(src: &Path) -> (usize, u64, usize) {
    let (mut files, mut bytes, mut hidden) = (0, 0, 0);
    for e in walkdir::WalkDir::new(src).min_depth(1).into_iter().flatten().filter(|e| e.file_type().is_file()) {
        let low = e.path().strip_prefix(src).map(|p| p.to_string_lossy().to_lowercase().replace('\\', "/")).unwrap_or_default();
        if matches!(low.as_str(), "meta.ini" | "__folder_managed_by_vortex" | "rmm-mod.toml" | ".rmm-index") {
            continue;
        }
        files += 1;
        bytes += e.metadata().map_or(0, |m| m.len());
        hidden += usize::from(low.split('/').any(|c| c.ends_with(".mohidden")));
    }
    (files, bytes, hidden)
}

/// Whether two paths are on the same drive, so a hard link can join them.
fn same_volume(a: &Path, b: &Path) -> bool {
    let root = |p: &Path| p.components().next().map(|c| c.as_os_str().to_string_lossy().to_lowercase());
    root(&fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf())) == root(&fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf()))
}

/// One row of the imported list: a mod folder to bring in, or a separator (`from` is None).
struct Incoming {
    id: String,
    enabled: bool,
    from: Option<(PathBuf, ModMeta, bool)>, // folder, metadata, everything belongs in root/
    color: Option<String>,
}

/// MO2 instances and Vortex state backups found in their usual places.
#[derive(Serialize, Debug, Default)]
pub struct Sources {
    /// MO2 instance folders (holding ModOrganizer.ini).
    pub mo2: Vec<PathBuf>,
    /// Vortex's live state folder (state.v2) first, then its state backups, newest first.
    pub vortex: Vec<PathBuf>,
}

pub fn find_sources() -> Sources {
    let mut s = Sources::default();
    if let Some(local) = dirs::data_local_dir()
        && let Ok(rd) = fs::read_dir(local.join("ModOrganizer"))
    {
        s.mo2 = rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.join("ModOrganizer.ini").is_file()).collect();
        s.mo2.sort();
    }
    if let Some(roaming) = dirs::config_dir() {
        let mut found: Vec<(std::time::SystemTime, PathBuf)> = [&["temp", "state_backups_full"][..], &["temp", "state_backups"], &["state_backup"]]
            .iter()
            .filter_map(|d| fs::read_dir(d.iter().fold(roaming.join("Vortex"), |p, s| p.join(s))).ok())
            .flatten()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .filter_map(|p| Some((fs::metadata(&p).ok()?.modified().ok()?, p)))
            .collect();
        found.sort_by_key(|f| std::cmp::Reverse(f.0));
        s.vortex = vortex_live_state().into_iter().chain(found.into_iter().map(|(_, p)| p)).collect();
    }
    s
}

// ---- MO2 ----------------------------------------------------------------------------------------

/// Import an MO2 instance's profile (default: the one MO2 has selected) into the active profile.
pub fn import_mo2(inst: &Instance, dir: &Path, profile: Option<&str>, dry_run: bool) -> Result<ImportReport> {
    let ini = mo2_ini(dir)?;
    let get = |s: &str, k: &str| ini.get(s).and_then(|m| m.get(k)).filter(|v| !v.is_empty()).cloned();
    let Some(game) = game_of(&ini) else {
        bail!("this MO2 instance manages {}, which rmm doesn't support", get("General", "gameName").as_deref().unwrap_or("an unknown game"))
    };
    if game != inst.config.game {
        bail!("this MO2 instance is for {game:?}, the rmm instance for {:?}", inst.config.game);
    }
    let sub = |key: &str, default: &str| mo2_dir(&ini, dir, key, default);
    let (mods_dir, overwrite_dir, downloads) = (sub("mod_directory", "mods"), sub("overwrite_directory", "overwrite"), sub("download_directory", "downloads"));
    let name = profile.map(str::to_owned).or_else(|| get("General", "selected_profile")).context("no profile given, and ModOrganizer.ini has none selected")?;
    let pdir = sub("profiles_directory", "profiles").join(&name);
    let modlist = fs::read_to_string(pdir.join("modlist.txt")).with_context(|| format!("MO2 profile '{name}' has no modlist.txt"))?;
    let mut r = ImportReport { dry_run, profile: name.clone(), into: inst.config.profile.clone(), ..Default::default() };

    // modlist.txt names the highest priority first; rmm's list is last-wins.
    let mut seen = HashSet::new();
    let mut listed: Vec<(String, bool)> = Vec::new();
    for line in modlist.lines() {
        let l = line.trim_end_matches('\r');
        if l.is_empty() || l.starts_with('#') || l.starts_with('*') {
            continue; // comments, and DLC / Creation Club / unmanaged entries (not folders)
        }
        let (enabled, n) = match l.as_bytes()[0] {
            b'+' => (true, &l[1..]),
            b'-' => (false, &l[1..]),
            _ => (true, l),
        };
        if n != "Overwrite" && seen.insert(n.to_string()) {
            listed.push((n.to_string(), enabled));
        }
    }
    listed.reverse();
    // MO2 puts folders the list doesn't name on top; they come last here, disabled.
    if let Ok(rd) = fs::read_dir(&mods_dir) {
        let mut extra: Vec<String> =
            rd.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()).map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| !seen.contains(n)).collect();
        extra.sort();
        listed.extend(extra.into_iter().map(|n| (n, false)));
    }

    // MO2's own categories: `id|name|…` per line (the Nexus ids and parent that follow vary by version).
    let categories: HashMap<i64, String> = fs::read_to_string(dir.join("categories.dat"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut f = l.split('|');
            Some((f.next()?.trim().parse().ok()?, f.next()?.trim().to_string()))
        })
        .collect();
    let mut incoming = Vec::new();
    for (n, enabled) in listed {
        if let Some(sep) = n.strip_suffix("_separator") {
            let color = mo2_color(&fs::read_to_string(mods_dir.join(&n).join("meta.ini")).unwrap_or_default());
            incoming.push(Incoming { id: format!("sep:{}", sanitize(sep)), enabled: false, from: None, color });
            continue;
        }
        // MO2 keeps backups of reinstalled mods as always-disabled "<mod>_backup1" folders; a mod
        // named "... Backup" is a mod.
        if !enabled && n.trim_end_matches(|c: char| c.is_ascii_digit()).to_lowercase().ends_with("_backup") {
            continue;
        }
        let folder = mods_dir.join(&n);
        if !folder.is_dir() {
            r.warnings.push(format!("{n}: listed in modlist.txt, but its folder is missing"));
            continue;
        }
        let (meta, color) = mo2_meta(&folder, &n, &downloads, &categories);
        incoming.push(Incoming { id: sanitize(&n), enabled, from: Some((folder, meta, false)), color });
    }

    let plugins = plugin_lists(&pdir.join("plugins.txt"), &pdir.join("loadorder.txt"));
    let locks: BTreeMap<String, usize> = fs::read_to_string(pdir.join("lockedorder.txt"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (n, i) = l.rsplit_once('|')?;
            Some((n.trim().to_string(), i.trim().parse().ok()?))
        })
        .collect();
    // The profile's options, and the mods MO2 has programs write into ("Create files in mod").
    let settings = fs::read_to_string(pdir.join("settings.ini")).unwrap_or_default();
    let general = qt_section(&settings, "General");
    let on = |k: &str| general.iter().any(|(key, v)| key == k && v.eq_ignore_ascii_case("true"));
    let (local_inis, local_saves) = (on("LocalSettings"), on("LocalSaves"));
    let into_mod: HashMap<String, String> = qt_section(&settings, "custom_overwrites").into_iter().collect();
    let exes = mo2_executables(&fs::read_to_string(dir.join("ModOrganizer.ini")).unwrap_or_default());
    let tools = mo2_tools(inst, exes, &mods_dir, &into_mod, &mut r)?;
    mark_outputs(inst, &tools, &mut incoming, &mut r)?;
    if !dry_run {
        for (id, t) in &tools {
            crate::tools::put(inst, id, t)?;
        }
    }
    if overwrite_dir.is_dir() {
        r.overwrite = count_tree(&overwrite_dir).0;
    }
    if !dry_run && overwrite_dir.is_dir() {
        let (l, c, _) = link_tree(&overwrite_dir, &inst.mod_dir(OVERWRITE), false, true)?;
        let _ = fs::remove_dir_all(inst.mod_dir(OVERWRITE).join(crate::install::HIDDEN_DIR)); // Overwrite hides nothing
        if l + c > 0 {
            r.warnings.push(format!("{} files from MO2's overwrite folder are now in overwrite/", l + c));
        }
    }
    bring_in(inst, &format!("import MO2 profile {name}"), &incoming, plugins, locks, &mut r, |p| {
        p.local_inis |= local_inis;
        p.local_saves |= local_saves;
    })?;
    profile_files(inst, &pdir, local_inis, local_saves, &mut r);
    Ok(r)
}

/// Import every profile of an MO2 instance, each into the rmm profile of its name (made when
/// missing). Mods and tools come in with the first; the active profile stays the active one.
pub fn import_mo2_all(inst: &Instance, dir: &Path, dry_run: bool) -> Result<Vec<ImportReport>> {
    let mut out: Vec<ImportReport> = Vec::new();
    for name in mo2_profiles(dir)? {
        let profile = match inst.profiles()?.into_iter().find(|n| n.eq_ignore_ascii_case(&name)) {
            Some(n) => n,
            None if dry_run => name.clone(),
            None => {
                inst.create_profile(&name, false)?;
                name.clone()
            }
        };
        let at = Instance { root: inst.root.clone(), config: InstanceConfig { profile, ..inst.config.clone() } };
        let mut r = import_mo2(&at, dir, Some(&name), dry_run)?;
        if dry_run && !out.is_empty() {
            let mut first = std::mem::take(&mut r.mods);
            r.existing.append(&mut first);
            (r.tools, r.outputs) = (vec![], vec![]);
        }
        out.push(r);
    }
    Ok(out)
}

/// One of MO2's Executables (`[customExecutables]` in ModOrganizer.ini).
struct Mo2Exe {
    title: String,
    binary: PathBuf,
    args: Vec<String>,
    cwd: String,
    hide: bool,
}

fn mo2_executables(ini: &str) -> Vec<Mo2Exe> {
    let mut rows: BTreeMap<usize, HashMap<String, String>> = BTreeMap::new();
    for (k, v) in qt_section(ini, "customExecutables") {
        if let Some((i, key)) = k.split_once('\\')
            && let Ok(i) = i.parse()
        {
            rows.entry(i).or_default().insert(key.to_string(), v);
        }
    }
    rows.into_values()
        .filter_map(|x| {
            let get = |k: &str| x.get(k).cloned().unwrap_or_default();
            let binary = get("binary");
            (!binary.is_empty()).then(|| Mo2Exe { title: get("title"), args: split_args(&get("arguments")), cwd: get("workingDirectory"), hide: get("hide") == "true", binary: binary.into() })
        })
        .collect()
}

/// MO2's Executables as rmm tools, by tool id. Programs rmm has a preset for get it (and its
/// arguments); others run as MO2 ran them. Left out: the game, its launcher and script extender
/// (Play starts those), Explore Virtual Folder (MO2's virtual Data), hidden ones, programs that
/// aren't there, and ones rmm has as a tool already. A program inside a mod runs from the
/// deployed Data folder, as it ran from MO2's virtual one.
fn mo2_tools(inst: &Instance, exes: Vec<Mo2Exe>, mods_dir: &Path, into_mod: &HashMap<String, String>, r: &mut ImportReport) -> Result<Vec<(String, Tool)>> {
    let game = inst.config.game;
    let skip = [game.exe().to_string(), game.exe().replace(".exe", "Launcher.exe"), game.loader().to_string(), "explorer++.exe".into()].map(|s| s.to_lowercase());
    let key = |p: &Path| p.to_string_lossy().replace('\\', "/").to_ascii_lowercase();
    let mods = key(mods_dir) + "/";
    let mut have = crate::tools::list(inst)?;
    let mut out = Vec::new();
    for e in exes {
        let file = e.binary.file_name().map(|f| f.to_string_lossy().to_lowercase()).unwrap_or_default();
        if e.hide || skip.contains(&file) {
            continue;
        }
        let bin = e.binary.to_string_lossy().replace('\\', "/");
        let in_mod = bin.to_ascii_lowercase().starts_with(&mods).then(|| &bin[mods.len()..]).and_then(|rest| rest.split_once('/'));
        let exe = match in_mod {
            Some((_, rel)) => match rel.split_once('/') {
                Some((top, rest)) if top.eq_ignore_ascii_case("root") => inst.config.game_path.join(rest),
                _ => inst.data_dir().join(rel),
            },
            None if e.binary.is_file() => e.binary.clone(),
            None => {
                r.warnings.push(format!("MO2's executable {}: {} isn't there", e.title, e.binary.display()));
                continue;
            }
        };
        if have.values().any(|t| key(&t.exe) == key(&exe)) {
            continue;
        }
        let preset = mo2_preset(&file).and_then(|p| Some((p, crate::tools::preset(game, p)?)));
        let mut tool = match &preset {
            Some((_, t)) => {
                if !e.args.is_empty() {
                    r.warnings.push(format!("{}: rmm runs it with the {} preset's arguments, not MO2's ({})", e.title, t.name, e.args.join(" ")));
                }
                t.clone()
            }
            None => Tool { name: e.title.clone(), args: e.args.clone(), ..Default::default() },
        };
        tool.exe = exe;
        tool.cwd = (!e.cwd.is_empty()).then(|| PathBuf::from(&e.cwd));
        tool.output_mod = into_mod.get(&e.title).cloned().or(tool.output_mod);
        let base = preset.map(|(p, _)| p.to_string()).unwrap_or_else(|| tool_id(&e.title));
        let id = (1..).map(|n| if n == 1 { base.clone() } else { format!("{base}-{n}") }).find(|id| !have.contains_key(id)).unwrap();
        have.insert(id.clone(), tool.clone());
        r.tools.push(e.title);
        out.push((id, tool));
    }
    Ok(out)
}

/// The rmm preset for a program, by its file name.
pub(crate) fn mo2_preset(file: &str) -> Option<&'static str> {
    let stem = file.strip_suffix(".exe")?.trim_end_matches("x64").trim_end_matches("64").trim();
    Some(match stem {
        "sseedit" | "fo4edit" | "xedit" => "xedit",
        "sseeditquickautoclean" | "fo4editquickautoclean" => "xedit-qac",
        "bodyslide" => "bodyslide",
        "texgen" => "texgen",
        "dyndolod" => "dyndolod",
        "xlodgen" => "xlodgen",
        "pgpatcher" | "parallaxgen" => "pgpatcher",
        s if s.starts_with("pandora") => "pandora",
        _ => return None,
    })
}

/// A tool id from a program's title: lowercase letters and digits, '-' and '_'.
pub(crate) fn tool_id(title: &str) -> String {
    let id: String = title.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '-' }).collect();
    let id = id.split('-').filter(|s| !s.is_empty()).collect::<Vec<_>>().join("-");
    if id.is_empty() { "tool".into() } else { id }
}

/// Mods a program made (no archive, and named as a tool's output, or the mod MO2 had it write
/// into) become that tool's output, so its next run in rmm replaces them in their place. Mods an
/// xEdit-style tool adds to stay plain mods.
fn mark_outputs(inst: &Instance, added: &[(String, Tool)], incoming: &mut [Incoming], r: &mut ImportReport) -> Result<()> {
    let presets = crate::tools::PRESETS.iter().filter_map(|p| Some((p.to_string(), crate::tools::preset(inst.config.game, p)?)));
    let mut makers: HashMap<String, (String, String)> = HashMap::new(); // output, lowercase -> tool id, output
    for (id, t) in presets.chain(crate::tools::list(inst)?).chain(added.iter().cloned()) {
        let name = t.output_mod.clone().unwrap_or_else(|| format!("{} Output", t.name));
        if !t.edit_in_place && !name.contains('{') {
            makers.insert(name.to_lowercase(), (id, name));
        }
    }
    for m in incoming.iter_mut() {
        let Some((_, meta, _)) = &mut m.from else { continue };
        if meta.source.is_some() || inst.mod_dir(&m.id).exists() {
            continue;
        }
        if let Some((id, name)) = makers.get(&meta.name.to_lowercase()) {
            meta.source = Some(format!("tool:{id}"));
            meta.name = name.clone();
            r.outputs.push(format!("{name} ({id})"));
        }
    }
    Ok(())
}

/// Copy an MO2 profile's own INIs into the rmm profile's, and its own saves into the folder that
/// profile saves in. Files already there stay; a file that can't be copied is a warning.
fn profile_files(inst: &Instance, pdir: &Path, inis: bool, saves: bool, r: &mut ImportReport) {
    let mut todo: Vec<(bool, PathBuf, PathBuf)> = Vec::new(); // is a save, from, to
    if inis {
        let dest = crate::profile_ini::profile_ini_dir(inst, &inst.config.profile);
        let have: Vec<PathBuf> = fs::read_dir(pdir).map(|rd| rd.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        for name in crate::profile_ini::names(inst) {
            if let Some(from) = have.iter().find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(&name))) {
                todo.push((false, from.clone(), dest.join(&name)));
            }
        }
    }
    let from = pdir.join("saves");
    if saves && from.is_dir() {
        match inst.ini_dir() {
            Some(d) => {
                let dest = d.join("Saves").join(&inst.config.profile);
                for e in walkdir::WalkDir::new(&from).into_iter().flatten().filter(|e| e.file_type().is_file()) {
                    if let Ok(rel) = e.path().strip_prefix(&from) {
                        todo.push((true, e.path().to_path_buf(), dest.join(rel)));
                    }
                }
            }
            None => r.warnings.push("the MO2 profile's saves weren't copied: rmm doesn't know where the game's My Games folder is".into()),
        }
    }
    for (save, from, to) in todo {
        if to.exists() {
            continue;
        }
        if !r.dry_run {
            let copied = fs::create_dir_all(to.parent().unwrap_or(Path::new("."))).and_then(|_| fs::copy(&from, &to));
            if let Err(e) = copied {
                r.warnings.push(format!("couldn't copy {} to {}: {e}", from.display(), to.display()));
                continue;
            }
        }
        if save {
            r.saves += 1;
        } else {
            r.inis += 1;
        }
    }
}

/// The game an MO2 instance manages, when rmm supports it.
pub fn mo2_game(dir: &Path) -> Option<Game> {
    game_of(&mo2_ini(dir).ok()?)
}

fn game_of(ini: &HashMap<String, HashMap<String, String>>) -> Option<Game> {
    match ini.get("General")?.get("gameName")?.as_str() {
        "Skyrim Special Edition" => Some(Game::SkyrimSe),
        "Fallout 4" => Some(Game::Fallout4),
        _ => None,
    }
}

fn mo2_ini(dir: &Path) -> Result<HashMap<String, HashMap<String, String>>> {
    Ok(qt_ini(&fs::read_to_string(dir.join("ModOrganizer.ini")).with_context(|| format!("{} has no ModOrganizer.ini; pass the MO2 instance folder. A global instance keeps it in %LOCALAPPDATA%\\ModOrganizer\\<instance name>, and `rmm import sources` lists the ones found", dir.display()))?))
}

/// A folder ModOrganizer.ini sets (`%BASE_DIR%` filled in), or its default in the base folder.
fn mo2_dir(ini: &HashMap<String, HashMap<String, String>>, dir: &Path, key: &str, default: &str) -> PathBuf {
    let get = |k: &str| ini.get("Settings").and_then(|m| m.get(k)).filter(|v| !v.is_empty());
    let base = get("base_directory").map(PathBuf::from).unwrap_or_else(|| dir.to_path_buf());
    get(key).map(|v| PathBuf::from(v.replace("%BASE_DIR%", &base.to_string_lossy()))).unwrap_or_else(|| base.join(default))
}

/// The MO2 instance's profiles (folders with a modlist.txt), the selected one first.
pub fn mo2_profiles(dir: &Path) -> Result<Vec<String>> {
    let ini = mo2_ini(dir)?;
    let selected = ini.get("General").and_then(|m| m.get("selected_profile")).cloned().unwrap_or_default();
    let mut names: Vec<String> = fs::read_dir(mo2_dir(&ini, dir, "profiles_directory", "profiles"))?
        .flatten()
        .filter(|e| e.path().join("modlist.txt").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort_by_key(|n| (*n != selected, n.to_lowercase()));
    Ok(names)
}

/// A mod's metadata and colour from its meta.ini.
fn mo2_meta(folder: &Path, name: &str, downloads: &Path, categories: &HashMap<i64, String>) -> (ModMeta, Option<String>) {
    let text = fs::read_to_string(folder.join("meta.ini")).unwrap_or_default();
    let ini = qt_ini(&text);
    let general = ini.get("General");
    let get = |k: &str| general.and_then(|m| m.get(k)).filter(|v| !v.is_empty()).cloned();
    let file_id = ini.get("installedFiles").and_then(|m| m.iter().find(|(k, _)| k.ends_with("fileid")).and_then(|(_, v)| v.parse().ok()));
    // `category="3,12,"`: MO2 category ids, -1 or 0 for none.
    let cats: Vec<String> = get("category").unwrap_or_default().split(',').filter_map(|c| categories.get(&c.trim().parse().ok()?).cloned()).collect();
    // MO2 has a one-line comment and notes (HTML from its editor); rmm keeps both as the note.
    let notes = raw_value(&text, "notes").map(|v| html_text(&qt_unescape(v).into_iter().filter_map(char::from_u32).collect::<String>()));
    let notes: Vec<String> = [get("comments"), notes].into_iter().flatten().filter(|n| !n.is_empty()).collect();
    // The INI tweaks turned on: `[INI%20Tweaks]` (Qt escapes the space) with `1\name=Grass.ini` for each.
    let mut ini_tweaks: Vec<String> = ini.get("INI%20Tweaks").or(ini.get("INI Tweaks")).map(|m| m.iter().filter(|(k, v)| k.ends_with("\\name") && !v.is_empty()).map(|(_, v)| v.clone()).collect()).unwrap_or_default();
    ini_tweaks.sort_by_key(|f| f.to_lowercase());
    let meta = ModMeta {
        ini_tweaks,
        name: name.into(),
        source: get("installationFile").map(|f| if Path::new(&f).is_absolute() { f } else { downloads.join(f).display().to_string() }),
        notes: (!notes.is_empty()).then(|| notes.join("\n\n")),
        categories: (!cats.is_empty()).then_some(cats),
        nexus: get("modid").and_then(|v| v.parse().ok()).filter(|&n: &u64| n > 0).map(|mod_id| NexusRef {
            mod_id,
            file_id,
            version: get("version"),
            category: get("nexusCategory").and_then(|c| c.parse().ok()).filter(|&c: &u64| c > 0),
        }),
        skip_version: get("ignoredVersion"),
        ..Default::default()
    };
    (meta, mo2_color(&text))
}

/// A key's value as a Qt INI has it (still escaped), without its quotes.
fn raw_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let v = text.lines().find_map(|l| l.trim().strip_prefix(key)?.strip_prefix('='))?.trim();
    Some(v.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(v))
}

/// Undo Qt's INI escaping (`\0`, `\x43`, `\n`, `\"`) into character codes.
fn qt_unescape(s: &str) -> Vec<u32> {
    let c: Vec<char> = s.chars().collect();
    let (mut out, mut i) = (Vec::new(), 0);
    while i < c.len() {
        let ch = c[i];
        i += 1;
        if ch != '\\' || i == c.len() {
            out.push(ch as u32);
            continue;
        }
        let radix = match c[i] {
            'x' => 16,
            '0'..='7' => 8,
            e => {
                i += 1;
                out.push(match e { 'a' => 7, 'b' => 8, 'f' => 12, 'n' => 10, 'r' => 13, 't' => 9, 'v' => 11, e => e as u32 });
                continue;
            }
        };
        i += usize::from(radix == 16);
        let mut v = 0u32;
        while let Some(d) = c.get(i).and_then(|ch| ch.to_digit(radix)) {
            v = v.saturating_mul(radix).saturating_add(d);
            i += 1;
        }
        out.push(v);
    }
    out
}

/// MO2's `color=@Variant(…)`: a QColor as Qt streams it (type 0x43, then spec, alpha, red, green
/// and blue as big-endian u16), INI-escaped. `#rrggbb` for an RGB colour (spec 1); MO2 writes
/// spec 0 for none.
fn mo2_color(meta_ini: &str) -> Option<String> {
    let b = qt_unescape(raw_value(meta_ini, "color")?.strip_prefix("@Variant(")?.strip_suffix(')')?);
    (b.len() >= 13 && b[..5] == [0, 0, 0, 0x43, 1]).then(|| format!("#{:02x}{:02x}{:02x}", b[7] & 0xff, b[9] & 0xff, b[11] & 0xff))
}

/// The text of QTextEdit's HTML (MO2's notes): paragraphs and line breaks as new lines.
fn html_text(s: &str) -> String {
    let Some(body) = s.find("<body").and_then(|i| s[i..].find('>').map(|j| &s[i + j + 1..])) else { return s.trim().to_string() };
    let body = body.split("</body>").next().unwrap_or(body);
    let (mut out, mut tag) = (String::new(), None::<String>);
    for ch in body.chars() {
        match (&mut tag, ch) {
            (None, '<') => tag = Some(String::new()),
            (None, '\n') => {} // markup, not text
            (None, c) => out.push(c),
            (Some(t), '>') => {
                let t = t.to_lowercase();
                if t.starts_with("br") || t == "/p" {
                    out.push('\n');
                }
                tag = None;
            }
            (Some(t), c) => t.push(c),
        }
    }
    let out = out.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'").replace("&nbsp;", " ").replace("&amp;", "&");
    out.trim().to_string()
}

/// A Qt `QSettings` INI: section -> key -> value. `@ByteArray(…)` is unwrapped and `\\` in
/// values unescaped; nested keys keep their `\` (`1\fileid`).
fn qt_ini(text: &str) -> HashMap<String, HashMap<String, String>> {
    let mut out: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut section = "General".to_string();
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') && l.ends_with(']') {
            section = l[1..l.len() - 1].to_string();
            continue;
        }
        let Some((k, v)) = l.split_once('=') else { continue };
        let v = v.trim();
        let v = v.strip_prefix("@ByteArray(").and_then(|x| x.strip_suffix(')')).unwrap_or(v);
        out.entry(section.clone()).or_default().insert(k.trim().to_string(), v.trim_matches('"').replace("\\\\", "\\"));
    }
    out
}

/// One section of a Qt INI as (key, value) pairs, with the keys' %-escapes and the values'
/// quotes, `\` escapes and `@ByteArray()` undone.
fn qt_section(text: &str, name: &str) -> Vec<(String, String)> {
    let (mut on, mut out) = (false, Vec::new());
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') && l.ends_with(']') {
            on = l[1..l.len() - 1] == *name;
            continue;
        }
        if let Some((k, v)) = l.split_once('=').filter(|_| on) {
            let v = v.trim();
            let v: String = qt_unescape(v.strip_prefix('"').and_then(|x| x.strip_suffix('"')).unwrap_or(v)).into_iter().filter_map(char::from_u32).collect();
            let v = v.strip_prefix("@ByteArray(").and_then(|x| x.strip_suffix(')')).map(str::to_owned).unwrap_or(v);
            out.push((qt_key(k.trim()), v));
        }
    }
    out
}

/// A Qt INI key with its `%XX` and `%UXXXX` escapes undone.
fn qt_key(k: &str) -> String {
    let c: Vec<char> = k.chars().collect();
    let hex = |at: usize, n: usize| c.get(at..at + n).and_then(|h| u32::from_str_radix(&h.iter().collect::<String>(), 16).ok()).and_then(char::from_u32);
    let (mut out, mut i) = (String::new(), 0);
    while i < c.len() {
        let (ch, n) = match (c[i], c.get(i + 1)) {
            ('%', Some('U')) => hex(i + 2, 4).map_or(('%', 1), |ch| (ch, 6)),
            ('%', _) => hex(i + 1, 2).map_or(('%', 1), |ch| (ch, 3)),
            (ch, _) => (ch, 1),
        };
        out.push(ch);
        i += n;
    }
    out
}

/// A command line's arguments: split at spaces outside double quotes, the quotes removed.
// ponytail: no backslash-before-quote rule; MO2's argument lines don't need it.
fn split_args(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut quoted, mut any) = (Vec::new(), String::new(), false, false);
    for ch in s.chars() {
        match ch {
            '"' => (quoted, any) = (!quoted, true),
            c if c.is_whitespace() && !quoted => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                any = false;
            }
            c => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}

// ---- Vortex -------------------------------------------------------------------------------------

/// Vortex's live state: `%APPDATA%\Vortex\state.v2`, a LevelDB database.
pub fn vortex_live_state() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("Vortex").join("state.v2")).filter(|p| p.join("CURRENT").is_file())
}

/// Read Vortex's LevelDB state into the shape of its JSON backups. Keys are paths joined by
/// `###` (`persistent###mods###skyrimse###<id>###attributes###version`) and values are JSON.
/// Opening a LevelDB writes to it, so this reads a copy: Vortex's own files are never touched.
pub fn read_vortex_db(dir: &Path, scratch: &Path) -> Result<Value> {
    if !dir.join("CURRENT").is_file() {
        bail!("{} isn't a LevelDB folder (no CURRENT file)", dir.display());
    }
    let _ = fs::remove_dir_all(scratch);
    fs::create_dir_all(scratch)?;
    for e in fs::read_dir(dir)? {
        let e = e?;
        // LOCK belongs to a running Vortex; the copy needs its own.
        if e.file_type()?.is_file() && e.file_name() != "LOCK" {
            fs::copy(e.path(), scratch.join(e.file_name()))?;
        }
    }
    let read = || -> Result<Value> {
        let opts = rusty_leveldb::Options { create_if_missing: false, ..Default::default() };
        let mut db = rusty_leveldb::DB::open(scratch, opts).map_err(|e| anyhow::anyhow!("cannot read Vortex's database: {e}"))?;
        let mut it = db.new_iter().map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut root = Value::Object(Default::default());
        while let Some((k, v)) = rusty_leveldb::LdbIterator::next(&mut it) {
            let key = String::from_utf8_lossy(&k).into_owned();
            let value = serde_json::from_slice(&v).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&v).into_owned()));
            let mut node = &mut root;
            let parts: Vec<&str> = key.split("###").collect();
            for (i, part) in parts.iter().enumerate() {
                if !node.is_object() {
                    *node = Value::Object(Default::default());
                }
                let map = node.as_object_mut().unwrap();
                if i + 1 == parts.len() {
                    map.insert(part.to_string(), value);
                    break;
                }
                node = map.entry(part.to_string()).or_insert_with(|| Value::Object(Default::default()));
            }
        }
        Ok(root)
    };
    let r = read();
    let _ = fs::remove_dir_all(scratch);
    r
}

/// Import a Vortex profile (default: the last active one) from Vortex's live state (the
/// `state.v2` folder; close Vortex first so it is current) or a state backup: the JSON files in
/// `%APPDATA%\Vortex\temp\state_backups_full`, or one made with Settings > Workarounds > Create
/// Backup. `staging` defaults to the folder the state names. Vortex keeps no mod order, so mods
/// go in by install time and then by its rules, which become rmm mod rules.
pub fn import_vortex(inst: &Instance, backup: &Path, staging: Option<&Path>, profile: Option<&str>, dry_run: bool) -> Result<ImportReport> {
    let state: Value = if backup.is_dir() {
        read_vortex_db(backup, &inst.root.join(".staging").join("vortex-state"))?
    } else {
        serde_json::from_str(&fs::read_to_string(backup).with_context(|| format!("cannot read {}", backup.display()))?)
            .with_context(|| format!("{} is not JSON", backup.display()))?
    };
    if state.get("persistent").is_none() {
        bail!("{} isn't Vortex's state (it has no 'persistent' section)", backup.display());
    }
    let gid = inst.config.game.vortex_id();
    let Some(mods) = state["persistent"]["mods"][gid].as_object() else {
        let games: Vec<&String> = state["persistent"]["mods"].as_object().map(|m| m.keys().collect()).unwrap_or_default();
        bail!("Vortex has no {gid} mods (it has mods for: {})", if games.is_empty() { "no game".into() } else { games.iter().map(|g| g.as_str()).collect::<Vec<_>>().join(", ") })
    };
    let userdata = dirs::config_dir().unwrap_or_default().join("Vortex");
    let resolve = |pattern: &str| {
        let mut p = pattern.to_string();
        for (k, v) in [("{USERDATA}", userdata.to_string_lossy().into_owned()), ("{userdata}", userdata.to_string_lossy().into_owned()), ("{GAME}", gid.into()), ("{game}", gid.into())] {
            p = p.replace(k, &v);
        }
        PathBuf::from(p)
    };
    let staging = staging.map(Path::to_path_buf).unwrap_or_else(|| resolve(state["settings"]["mods"]["installPath"][gid].as_str().unwrap_or("{USERDATA}/{GAME}/mods")));
    let downloads = resolve(state["settings"]["downloads"]["path"].as_str().unwrap_or("{USERDATA}/downloads")).join(gid);
    let last = state["settings"]["profiles"]["lastActiveProfile"][gid].as_str();
    let Some((pid, prof)) = state["persistent"]["profiles"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, p)| p["gameId"] == gid)
        .find(|(id, p)| match profile.or(last) {
            Some(w) => id.as_str() == w || p["name"].as_str() == Some(w),
            None => true,
        })
    else {
        bail!("no {gid} profile{} in the backup", profile.map(|p| format!(" named '{p}'")).unwrap_or_default());
    };
    let mut r = ImportReport { dry_run, profile: prof["name"].as_str().unwrap_or(pid).to_string(), into: inst.config.profile.clone(), ..Default::default() };

    let mut list: Vec<(&String, &Value)> = mods.iter().filter(|(_, m)| m["state"] == "installed" && m["type"] != "collection").collect();
    list.sort_by(|a, b| (a.1["attributes"]["installTime"].as_str(), a.0).cmp(&(b.1["attributes"]["installTime"].as_str(), b.0)));
    let num = |v: &Value| v.as_u64().or_else(|| v.as_str()?.parse().ok());
    let mut ids: HashMap<&str, String> = HashMap::new();
    let mut used = HashSet::new();
    let mut incoming = Vec::new();
    for (vid, m) in &list {
        let a = &m["attributes"];
        let name = ["customFileName", "logicalFileName", "modName", "name"].iter().find_map(|k| a[*k].as_str().filter(|s| !s.trim().is_empty())).unwrap_or(vid);
        let folder = staging.join(m["installationPath"].as_str().unwrap_or(vid));
        if !folder.is_dir() {
            r.warnings.push(format!("{name}: its staging folder {} is missing", folder.display()));
            continue;
        }
        let base = sanitize(name);
        let id = (1..).map(|n| if n == 1 { base.clone() } else { format!("{base} ({n})") }).find(|id| used.insert(id.clone())).unwrap();
        let meta = ModMeta {
            name: name.into(),
            source: a["fileName"].as_str().map(|f| downloads.join(f).display().to_string()),
            fomod: serde_json::from_value::<crate::fomod::Choices>(a["installerChoices"].clone()).ok().filter(|c| !c.options.is_empty()),
            notes: a["notes"].as_str().filter(|s| !s.is_empty()).map(str::to_owned),
            nexus: num(&a["modId"]).filter(|&n| n > 0).map(|mod_id| NexusRef { mod_id, file_id: num(&a["fileId"]), version: a["version"].as_str().map(str::to_owned), category: None }),
            ..Default::default()
        };
        let root = matches!(m["type"].as_str(), Some("dinput" | "enb"));
        let enabled = prof["modState"][vid.as_str()]["enabled"].as_bool().unwrap_or(false);
        ids.insert(vid.as_str(), id.clone());
        incoming.push(Incoming { id, enabled, from: Some((folder, meta, root)), color: None });
    }

    // Rules point at a mod by id, or by archive hash or logical file name.
    let find = |rf: &Value| -> Option<String> {
        for k in ["id", "idHint"] {
            if let Some(id) = rf[k].as_str().and_then(|v| ids.get(v)) {
                return Some(id.clone());
            }
        }
        let md5 = rf["fileMD5"].as_str().or(rf["md5Hint"].as_str());
        let lfn = rf["logicalFileName"].as_str();
        list.iter()
            .find(|(_, m)| (md5.is_some() && m["attributes"]["fileMD5"].as_str() == md5) || (lfn.is_some() && m["attributes"]["logicalFileName"].as_str() == lfn))
            .and_then(|(vid, _)| ids.get(vid.as_str()).cloned())
    };
    let mut rules: BTreeMap<String, ModRule> = BTreeMap::new();
    for (vid, m) in &list {
        let Some(me) = ids.get(vid.as_str()).cloned() else { continue };
        for x in m["rules"].as_array().into_iter().flatten() {
            if x["ignored"].as_bool() == Some(true) {
                continue;
            }
            let Some(other) = find(&x["reference"]).filter(|o| *o != me) else { continue };
            // "A before B" is B's "after A".
            let (target, kind, item) = match x["type"].as_str() {
                Some("after") => (me.clone(), 0, other),
                Some("before") => (other, 0, me.clone()),
                Some("requires") => (me.clone(), 1, other),
                Some("conflicts") => (me.clone(), 2, other),
                _ => continue,
            };
            let rl = rules.entry(target.clone()).or_insert_with(|| ModRule { id: target, ..Default::default() });
            match kind {
                0 => rl.after.push(item),
                1 => rl.requires.push(item),
                _ => rl.incompatible.push(item),
            }
        }
    }
    r.rules = rules.len();

    // Plugins: the live files for the profile Vortex has active, its profile copy otherwise.
    let active = state["settings"]["profiles"]["activeProfileId"].as_str() == Some(pid.as_str()) || last == Some(pid.as_str());
    let pdir = if active { inst.config.local_path.clone() } else { userdata.join(gid).join("profiles").join(pid) };
    let plugins = plugin_lists(&pdir.join("plugins.txt"), &pdir.join("loadorder.txt"));
    bring_in(inst, &format!("import Vortex profile {}", r.profile), &incoming, plugins, BTreeMap::new(), &mut r, |_| {})?;
    if dry_run {
        return Ok(r);
    }
    for (_, rl) in rules {
        let old = crate::rules::rules(inst)?.into_iter().find(|x| x.id == rl.id).unwrap_or_default();
        let merged = ModRule {
            id: rl.id.clone(),
            after: [old.after, rl.after].concat(),
            requires: [old.requires, rl.requires].concat(),
            incompatible: [old.incompatible, rl.incompatible].concat(),
        };
        if let Err(e) = crate::rules::set_rule(inst, &merged) {
            r.warnings.push(format!("rule for {}: {e:#}", rl.id));
        }
    }
    let sorted = crate::rules::sort(inst, false)?;
    if !sorted.cycles.is_empty() {
        r.warnings.push(format!("{} rule pairs contradict each other and were left out of the order", sorted.cycles.len()));
    }
    let userlist = userdata.join(gid).join("userlist.yaml");
    let ours = inst.root.join("loot").join("userlist.yaml");
    if userlist.is_file() && !ours.exists() {
        fs::create_dir_all(ours.parent().unwrap())?;
        fs::copy(&userlist, &ours)?;
        r.warnings.push("Vortex's LOOT userlist is now rmm's".into());
    }
    Ok(r)
}

// ---- Shared -------------------------------------------------------------------------------------

/// plugins.txt (`*` = active; read as Latin-1, the way the game writes it) for the active state,
/// loadorder.txt for the order when there is one.
fn plugin_lists(plugins_txt: &Path, loadorder_txt: &Path) -> Vec<PluginEntry> {
    let lines = |t: String| t.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty() && !l.starts_with('#')).collect::<Vec<_>>();
    let plugins = lines(fs::read(plugins_txt).map(|b| b.iter().map(|&c| c as char).collect()).unwrap_or_default());
    let active: HashSet<String> = plugins.iter().filter_map(|l| l.strip_prefix('*')).map(str::to_lowercase).collect();
    let order = match fs::read_to_string(loadorder_txt) {
        Ok(t) => lines(t),
        Err(_) => plugins.iter().map(|l| l.trim_start_matches('*').to_string()).collect(),
    };
    order.into_iter().map(|name| PluginEntry { enabled: active.contains(&name.to_lowercase()), name }).collect()
}

/// Copy the mod folders in, then set the profile in one undoable step: the imported list first,
/// then mods the profile had that the import doesn't name; `also` changes the rest.
fn bring_in(inst: &Instance, label: &str, incoming: &[Incoming], plugins: Vec<PluginEntry>, locks: BTreeMap<String, usize>, r: &mut ImportReport, also: impl FnOnce(&mut Profile)) -> Result<()> {
    for (i, m) in incoming.iter().enumerate() {
        crate::progress::report("importing mods", i, incoming.len());
        let Some((dir, meta, root)) = &m.from else {
            r.separators += 1;
            continue;
        };
        if inst.mod_dir(&m.id).exists() {
            r.existing.push(m.id.clone());
            let have = inst.mod_meta(&m.id);
            r.clashes.push(Clash {
                id: m.id.clone(),
                have: have.nexus.and_then(|n| n.version),
                have_files: inst.mod_files(&m.id).map_or(0, |f| f.len()),
                incoming: meta.nexus.as_ref().and_then(|n| n.version.clone()),
                incoming_files: count_tree(dir).0,
            });
            continue;
        }
        r.mods.push(m.id.clone());
        if r.dry_run {
            let (files, bytes, hidden) = count_tree(dir);
            r.files += files;
            r.hidden += hidden;
            if !same_volume(dir, &inst.root) {
                r.copy_bytes += bytes;
            }
            continue;
        }
        let tmp = inst.root.join(".staging").join(format!("import-{}", m.id));
        let _ = fs::remove_dir_all(&tmp);
        let (l, c, h) = link_tree(dir, &tmp, *root, false)?;
        (r.linked, r.copied, r.hidden, r.files) = (r.linked + l, r.copied + c, r.hidden + h.len(), r.files + l + c);
        fs::write(tmp.join(".rmm-index"), index_dir(&tmp)?.join("\n"))?;
        fs::write(tmp.join(crate::instance::MOD_META), toml::to_string_pretty(&ModMeta { hidden: h, ..meta.clone() })?)?;
        fs::rename(&tmp, inst.mod_dir(&m.id))?;
    }
    r.plugins = plugins.len();
    let named: HashSet<&str> = incoming.iter().map(|m| m.id.as_str()).collect();
    // A dry run into a profile not made yet: nothing there to keep.
    r.after = inst.profile().map(|p| p.mods).unwrap_or_default().into_iter().map(|m| m.id).filter(|id| !named.contains(id.as_str()) && !crate::instance::is_separator(id)).collect();
    if r.dry_run {
        return Ok(());
    }
    let listed: HashSet<String> = plugins.iter().map(|e| e.name.to_lowercase()).collect();
    inst.edit(label, |p| {
        let rest: Vec<ModEntry> = p.mods.iter().filter(|m| !named.contains(m.id.as_str())).cloned().collect();
        p.mods = incoming.iter().map(|m| ModEntry { id: m.id.clone(), enabled: m.enabled && m.from.is_some() }).chain(rest).collect();
        let others: Vec<PluginEntry> = p.plugins.iter().filter(|e| !listed.contains(&e.name.to_lowercase())).cloned().collect();
        p.plugins = plugins.into_iter().chain(others).collect();
        p.locks.extend(locks);
        p.colors.extend(incoming.iter().filter_map(|m| Some((m.id.clone(), m.color.clone()?))));
        also(p);
        Ok(())
    })
}

#[derive(Serialize, Debug, Default)]
pub struct Creations {
    pub dry_run: bool,
    /// The Creations (by plugin name) that become mods, or go back into Data.
    pub mods: Vec<String>,
    pub files: usize,
    pub bytes: u64,
}

/// Creation Club content in the game's Data folder: each plugin the game's `.ccc` file lists,
/// with its archives (`<name>.bsa`, `<name> - Textures.ba2`), unless a mod provides it.
fn creation_files(inst: &Instance) -> Result<Vec<(String, Vec<String>)>> {
    let ccc = inst.config.game_path.join(format!("{}.ccc", inst.config.game.ini_name().trim_end_matches(".ini")));
    let Ok(text) = fs::read_to_string(&ccc) else { return Ok(vec![]) };
    let data: Vec<String> = fs::read_dir(inst.data_dir())?.flatten().filter(|e| e.path().is_file()).filter_map(|e| e.file_name().into_string().ok()).collect();
    let mut from_mods = HashSet::new();
    for m in inst.profile()?.mods.iter().filter(|m| !crate::instance::is_separator(&m.id)) {
        from_mods.extend(inst.mod_files(&m.id).unwrap_or_default().into_iter().filter(|f| !f.contains('/')).map(|f| f.to_lowercase()));
    }
    let mut out = Vec::new();
    for listed in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let Some(plugin) = data.iter().find(|f| f.eq_ignore_ascii_case(listed) && !from_mods.contains(&f.to_lowercase())) else { continue };
        let stem = plugin.rsplit_once('.').map_or(plugin.as_str(), |x| x.0).to_lowercase();
        let archive = |f: &str| {
            let rest = &f[stem.len()..];
            [".bsa", ".ba2"].iter().any(|x| rest == *x || (rest.starts_with(" - ") && rest.ends_with(x)))
        };
        let files = data.iter().filter(|f| *f == plugin || (f.to_lowercase().starts_with(&stem) && archive(&f.to_lowercase()) && !from_mods.contains(&f.to_lowercase()))).cloned().collect();
        out.push((plugin.clone(), files));
    }
    Ok(out)
}

fn move_file(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to.parent().expect("a file's folder"))?;
    if fs::rename(from, to).is_err() {
        fs::copy(from, to)?;
        fs::remove_file(from)?;
    }
    Ok(())
}

/// Take the Creation Club content out of the game's Data folder as mods, one per Creation, on
/// and at the top of every profile, as MO2's Curation Club does: each can then be turned off,
/// and Purge (or Play without mods) leaves them all out. Not an undo step, as the content was
/// the game's; `restore_creations` puts it back. Deploy (or Play) links it into Data again.
pub fn adopt_creations(inst: &Instance, dry_run: bool) -> Result<Creations> {
    let found = creation_files(inst)?;
    let data = inst.data_dir();
    let mut r = Creations { dry_run, mods: found.iter().map(|x| x.0.clone()).collect(), ..Default::default() };
    r.files = found.iter().map(|x| x.1.len()).sum();
    r.bytes = found.iter().flat_map(|x| &x.1).filter_map(|f| fs::metadata(data.join(f)).ok()).map(|m| m.len()).sum();
    if dry_run || found.is_empty() {
        return Ok(r);
    }
    inst.locked(|| {
        let mut added = Vec::new();
        for (plugin, files) in &found {
            let name = plugin.rsplit_once('.').map_or(plugin.as_str(), |x| x.0);
            let id = crate::install::unique_id(inst, &sanitize(name));
            let tmp = inst.root.join(".staging").join(format!("creation-{id}"));
            let _ = fs::remove_dir_all(&tmp);
            for f in files {
                move_file(&data.join(f), &tmp.join(f))?;
            }
            fs::write(tmp.join(".rmm-index"), index_dir(&tmp)?.join("\n"))?;
            fs::write(tmp.join(crate::instance::MOD_META), toml::to_string_pretty(&ModMeta { name: name.into(), source: Some("Creation Club".into()), creation: true, ..Default::default() })?)?;
            fs::create_dir_all(inst.mods_dir())?;
            fs::rename(&tmp, inst.mod_dir(&id))?;
            added.push((id, plugin.clone()));
        }
        for name in inst.profiles()? {
            let path = inst.root.join("profiles").join(&name).join("profile.toml");
            let mut p: Profile = toml::from_str(&fs::read_to_string(&path)?)?;
            let entries = added.iter().filter(|(id, _)| !p.mods.iter().any(|m| m.id == *id)).map(|(id, _)| ModEntry { id: id.clone(), enabled: true }).collect::<Vec<_>>();
            p.mods.splice(0..0, entries);
            crate::install::add_plugins(&mut p, &added.iter().map(|x| x.1.clone()).collect::<Vec<_>>(), true);
            crate::instance::write_atomic(&path, &toml::to_string_pretty(&p)?)?;
        }
        Ok(())
    })?;
    Ok(r)
}

/// Put adopted Creation Club content back into the game's Data folder: purges first (so no deploy
/// record points at it), moves each Creation's files back, and takes the Creations out of every
/// profile (their emptied folders go to trash/).
pub fn restore_creations(inst: &Instance, dry_run: bool) -> Result<Creations> {
    let mut ids: Vec<String> = fs::read_dir(inst.mods_dir())?.flatten().filter_map(|e| e.file_name().into_string().ok()).filter(|id| inst.mod_meta(id).creation).collect();
    ids.sort();
    let mut r = Creations { dry_run, ..Default::default() };
    for id in &ids {
        let files = inst.mod_files(id)?;
        r.mods.extend(crate::install::top_plugins(&files));
        r.bytes += files.iter().filter_map(|f| fs::metadata(inst.mod_dir(id).join(f)).ok()).map(|m| m.len()).sum::<u64>();
        r.files += files.len();
    }
    if dry_run || ids.is_empty() {
        return Ok(r);
    }
    crate::deploy::purge(inst, false)?;
    let data = inst.data_dir();
    inst.locked(|| {
        for id in &ids {
            for f in inst.mod_files(id)? {
                if !data.join(&f).exists() {
                    move_file(&inst.mod_dir(id).join(&f), &data.join(&f))?;
                }
            }
            inst.trash_mod(id)?;
        }
        for name in inst.profiles()? {
            let path = inst.root.join("profiles").join(&name).join("profile.toml");
            let mut p: Profile = toml::from_str(&fs::read_to_string(&path)?)?;
            p.mods.retain(|m| !ids.contains(&m.id));
            crate::instance::write_atomic(&path, &toml::to_string_pretty(&p)?)?;
        }
        Ok(())
    })?;
    Ok(r)
}

/// Hard-link (or, across volumes, copy) a mod folder's files into `dest`. MO2's `meta.ini` and
/// Vortex's marker files stay behind; the files MO2 hides (`.mohidden`, or in a folder so named)
/// go to `.rmm-hidden/` under their own names, still hidden. A top `Root` folder (Root Builder)
/// becomes `root/`, and with `under_root` everything goes there. Returns (linked, copied, the
/// hidden files' paths).
fn link_tree(src: &Path, dest: &Path, under_root: bool, keep_existing: bool) -> Result<(usize, usize, Vec<String>)> {
    let (mut linked, mut copied, mut hidden) = (0, 0, Vec::new());
    fs::create_dir_all(dest)?;
    for e in walkdir::WalkDir::new(src).min_depth(1) {
        let e = e?;
        if !e.file_type().is_file() {
            continue;
        }
        let rel = e.path().strip_prefix(src)?.to_string_lossy().replace('\\', "/");
        let low = rel.to_lowercase();
        if matches!(low.as_str(), "meta.ini" | "__folder_managed_by_vortex" | "rmm-mod.toml" | ".rmm-index") {
            continue;
        }
        let hide = low.split('/').any(|c| c.ends_with(".mohidden"));
        let rel = if hide { rel.split('/').map(|c| if c.to_lowercase().ends_with(".mohidden") { &c[..c.len() - 9] } else { c }).collect::<Vec<_>>().join("/") } else { rel };
        let rel = match rel.split_once('/') {
            Some((top, rest)) if top.eq_ignore_ascii_case("root") => format!("root/{rest}"),
            _ => rel,
        };
        let rel = if under_root { format!("root/{rel}") } else { rel };
        let to = if hide {
            hidden.push(rel.clone());
            dest.join(crate::install::HIDDEN_DIR).join(&rel)
        } else {
            dest.join(&rel)
        };
        if keep_existing && to.exists() {
            continue;
        }
        fs::create_dir_all(to.parent().unwrap())?;
        if fs::hard_link(e.path(), &to).is_ok() {
            linked += 1;
        } else {
            fs::copy(e.path(), &to)?;
            copied += 1;
        }
    }
    Ok((linked, copied, hidden))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_mo2_colours_and_notes() {
        let c = |v: &str| mo2_color(&format!("[General]\r\nmodid=1\r\ncolor={v}\r\n"));
        assert_eq!(c(r"@Variant(\0\0\0\x43\x1\xff\xff\xff\xff\x80\x80\0\0\0\0)").as_deref(), Some("#ff8000"));
        assert_eq!(c(r"@Variant(\0\0\0\x43\0\xff\xff\0\0\0\0\0\0\0\0)"), None, "MO2's no colour");
        assert_eq!(c(r#""@Variant(\0\0\0\x43\x1\xff\xff==\x1\x1;;\0\0)""#).as_deref(), Some("#3d013b"), "quoted, with plain characters");
        let html = "<html><head><style>p { a: b; }</style></head><body style=\"x\">\n<p>One &amp; <b>two</b><br />three</p>\n<p>four</p></body></html>";
        assert_eq!(html_text(html), "One & two\nthree\nfour");
        assert_eq!(html_text(" plain\ntext "), "plain\ntext");
        assert_eq!(qt_unescape(r#"a\n\"b\\"#), "a\n\"b\\".chars().map(|c| c as u32).collect::<Vec<_>>());
    }
}
