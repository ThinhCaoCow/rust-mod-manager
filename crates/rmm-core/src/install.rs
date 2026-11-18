//! Install a mod from a folder, .zip, .7z or .rar: extract, find the Data root (or run its FOMOD
//! installer), store under mods/<id>.
use crate::fomod::{Choices, Env, FileState, Installer, Walk};
use crate::instance::{ModMeta, index_dir};
use crate::{Instance, ModEntry, PluginEntry, path_key};
use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

const DATA_DIRS: &[&str] = &[
    "meshes", "textures", "scripts", "interface", "sound", "music", "seq", "strings", "skse",
    "f4se", "materials", "vis", "lodsettings", "grass", "shadersfx", "video", "nemesis_engine",
    "calientetools", "dyndolod", "meta", "facegen", "mcm", "netscriptframework", "terrain",
    "dialogueviews", "trees", "lod", "distantlod", "source", "particles", "effects", "sky",
];
const DATA_EXTS: &[&str] = &["esp", "esm", "esl", "bsa", "ba2"];
const ROOT_EXTS: &[&str] = &["exe", "dll"];

#[derive(Debug, Serialize)]
pub struct InstallReport {
    pub id: String,
    pub name: String,
    pub files: usize,
    pub root_files: usize,
    pub plugins: Vec<String>,
    pub warnings: Vec<String>,
    /// FOMOD choices used, stored in rmm-mod.toml for reinstalls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fomod: Option<Choices>,
}

/// `choices` is required when the mod has a FOMOD installer (`Choices::default()` takes its defaults).
/// The mod goes at the end, before generated tool output, or where mod rules place it.
pub fn install(inst: &Instance, source: &Path, name: Option<&str>, enable: bool, choices: Option<&Choices>) -> Result<InstallReport> {
    install_at(inst, source, name, enable, choices, None)
}

/// `install` at a given priority (0 = lowest) instead of the default place.
pub fn install_at(inst: &Instance, source: &Path, name: Option<&str>, enable: bool, choices: Option<&Choices>, priority: Option<usize>) -> Result<InstallReport> {
    // A folder keeps its whole name ("Address Library (1.7.104.0)"); an archive loses its extension.
    let stem = if source.is_dir() { source.file_name() } else { source.file_stem() }.map(|s| s.to_string_lossy().into_owned());
    let nexus = stem.as_deref().and_then(nexus_name);
    let name = name
        .map(str::to_owned)
        .or_else(|| nexus.as_ref().map(|(n, _)| n.clone()))
        .or(stem)
        .context("cannot derive a mod name from the source path")?;
    let id = unique_id(inst, &sanitize(&name));
    let r = install_with_id(inst, source, &id, &name, enable, choices, priority)?;
    if let Some((_, n)) = nexus {
        crate::nexus::remember(inst, &r.id, n)?; // so update checks find a manual download too
    }
    Ok(r)
}

/// The mod name, Nexus mod id and version in a Nexus download's file name, as MO2 reads them:
/// "Address Library - All In One 47327 1.11.240 2026-08-21T23-48Z Hbu6377Rl" (the site's names
/// now) or "Unofficial Skyrim Special Edition Patch-266-4-2-5a-1623868815" (older ones).
fn nexus_name(stem: &str) -> Option<(String, crate::instance::NexusRef)> {
    let found = |name: &str, id: &str, version: &str| {
        let mod_id = id.bytes().all(|b| b.is_ascii_digit()).then(|| id.parse().ok()).flatten().filter(|_| !name.trim().is_empty())?;
        Some((name.trim().to_string(), crate::instance::NexusRef { mod_id, file_id: None, version: Some(version.into()), category: None }))
    };
    if let [hash, time, version, id, name] = stem.rsplitn(5, ' ').collect::<Vec<_>>()[..] {
        let t = time.as_bytes();
        let is_time = t.len() == 17 && t.iter().enumerate().all(|(i, &b)| match i {
            4 | 7 | 13 => b == b'-',
            10 => b == b'T',
            16 => b == b'Z',
            _ => b.is_ascii_digit(),
        });
        if is_time && hash.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return found(name, id, version);
        }
    }
    let (rest, time) = stem.rsplit_once('-')?;
    if time.len() != 10 || !time.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // The name ends at the first "-<digits>-": the mod id, then the version.
    rest.match_indices('-').find_map(|(i, _)| {
        let (id, version) = rest[i + 1..].split_once('-')?;
        found(&rest[..i], id, &version.replace('-', "."))
    })
}

/// Install under an exact id (a lockfile's), which must be free and valid.
pub fn install_as(inst: &Instance, source: &Path, id: &str, name: &str, enable: bool, choices: Option<&Choices>) -> Result<InstallReport> {
    if sanitize(id) != id || id.contains(':') || unique_id(inst, id) != id {
        bail!("'{id}' is taken or isn't a valid mod id");
    }
    install_with_id(inst, source, id, name, enable, choices, None)
}

fn install_with_id(inst: &Instance, source: &Path, id: &str, name: &str, enable: bool, choices: Option<&Choices>, priority: Option<usize>) -> Result<InstallReport> {
    let staging = inst.root.join(".staging").join(id);
    let _ = fs::remove_dir_all(&staging);
    let mut result = install_inner(inst, source, &staging, id, name, enable, choices, priority);
    let _ = fs::remove_dir_all(&staging);
    if let Ok(r) = &mut result {
        let _ = fs::remove_dir_all(preview_dir(inst));
        let done = crate::addons::hook(inst, "after_install", &serde_json::json!({ "id": r.id, "name": r.name }));
        r.warnings.extend(crate::addons::warnings(&done));
    }
    result
}

/// Steps, groups and options of a mod's FOMOD installer as they play out with `choices`
/// (anything not chosen takes the installer's default), and how many files that installs.
pub fn fomod_options(inst: &Instance, source: &Path, choices: Option<&Choices>) -> Result<Walk> {
    let raw = if source.is_dir() { source.to_path_buf() } else { unpack(inst, source)? };
    let Layout::Fomod(root) = find_layout(raw)? else {
        bail!("{} has no FOMOD installer; install it directly", source.display());
    };
    let mut walk = fomod_walk(inst, &root, choices.unwrap_or(&Choices::default()))?;
    let (files, warnings) = walk.resolve(&root)?;
    walk.files = files.len();
    walk.warnings.extend(warnings);
    Ok(walk)
}

/// Whether the mod at `source` has a FOMOD installer. An archive is unpacked into the preview
/// cache, so fomod_options and the install that follow don't extract it again.
pub fn has_fomod(inst: &Instance, source: &Path) -> Result<bool> {
    let raw = if source.is_dir() { source.to_path_buf() } else { unpack(inst, source)? };
    Ok(matches!(find_layout(raw)?, Layout::Fomod(_)))
}

/// The file an option's `image` names (FOMOD paths ignore case and may use `\`), inside the
/// installer: `..` and absolute paths match nothing.
pub fn fomod_image(inst: &Instance, source: &Path, image: &str) -> Result<PathBuf> {
    let raw = if source.is_dir() { source.to_path_buf() } else { unpack(inst, source)? };
    let Layout::Fomod(root) = find_layout(raw)? else {
        bail!("{} has no FOMOD installer", source.display());
    };
    image
        .split(['/', '\\'])
        .filter(|part| !part.is_empty() && *part != ".")
        .try_fold(root, |dir, part| child_ci(&dir, part))
        .filter(|p| p.is_file())
        .with_context(|| format!("the installer has no image '{image}'"))
}

#[allow(clippy::too_many_arguments)]
fn install_inner(inst: &Instance, source: &Path, staging: &Path, id: &str, name: &str, enable: bool, choices: Option<&Choices>, priority: Option<usize>) -> Result<InstallReport> {
    let dest = inst.mod_dir(id);
    let (files, fomod, warnings) = build(inst, source, staging, &dest, choices)?;
    // An archive's hash lets a lockfile find the same download elsewhere.
    let sha256 = if source.is_file() { Some(crate::lockfile::sha256_file(source)?) } else { None };
    inst.write_mod_meta(id, &ModMeta { name: name.into(), source: Some(source.display().to_string()), fomod: fomod.clone(), sha256, ..Default::default() })?;
    let plugins = top_plugins(&files);
    inst.edit(&format!("install {id}"), |p| {
        let ids: Vec<String> = p.mods.iter().map(|m| m.id.clone()).collect();
        let at = match priority {
            Some(n) => n.min(ids.len()),
            None => crate::rules::place(inst, &ids, id)?,
        };
        p.mods.insert(at, ModEntry { id: id.into(), enabled: enable });
        add_plugins(p, &plugins, enable);
        Ok(())
    })?;
    Ok(report(id, name, &files, plugins, warnings, fomod))
}

/// Install a mod again from the archive or folder it came from, with new FOMOD choices
/// (`None` keeps the stored ones). It keeps its id, name, place and enabled state; plugins the
/// new files add join the plugin list. The old files go to trash/; this isn't in the undo history.
pub fn reinstall(inst: &Instance, id: &str, choices: Option<&Choices>) -> Result<InstallReport> {
    replace(inst, id, None, choices)
}

/// Update a mod in place from another archive or folder (a newer version): it keeps its id, name,
/// place, enabled state, notes, categories, colour, rules and Nexus ids. Without `choices` a
/// FOMOD installer takes the stored ones. The old files go to trash/; undo swaps them back.
pub fn update(inst: &Instance, id: &str, source: &Path, choices: Option<&Choices>) -> Result<InstallReport> {
    replace(inst, id, Some(source), choices)
}

/// Reinstall (`new_source` None) or update a mod, as one undoable step.
fn replace(inst: &Instance, id: &str, new_source: Option<&Path>, choices: Option<&Choices>) -> Result<InstallReport> {
    if crate::instance::is_separator(id) || id == crate::instance::OVERWRITE || !inst.mod_dir(id).is_dir() {
        bail!("'{id}' is not an installed mod");
    }
    let meta = inst.mod_meta(id);
    let source = match new_source {
        Some(s) => s.to_path_buf(),
        None => PathBuf::from(meta.source.clone().filter(|s| !s.starts_with("tool:")).with_context(|| {
            format!("{id} wasn't installed from an archive or folder rmm can read again; tool output is rebuilt by running its tool")
        })?),
    };
    if !source.exists() {
        bail!("{} is gone; install the new download instead", source.display());
    }
    let Some(enabled) = inst.profile()?.mods.iter().find(|m| m.id == id).map(|m| m.enabled) else {
        bail!("mod '{id}' is not in the profile");
    };
    let staging = inst.root.join(".staging").join(id);
    let _ = fs::remove_dir_all(&staging);
    let choices = choices.or(meta.fomod.as_ref()).cloned().unwrap_or_default();
    let result = (|| {
        fs::create_dir_all(&staging)?;
        let fresh = staging.join("mod");
        let (files, fomod, warnings) = build(inst, &source, &staging, &fresh, Some(&choices))?;
        // The old files stay in trash/.replaced/ for undo to swap back.
        let kept = (1..).map(|n| format!(".replaced/{id}-{n}")).find(|f| !inst.root.join("trash").join(f).exists()).expect("a free name");
        inst.locked(|| {
            let old = inst.root.join("trash").join(&kept);
            fs::create_dir_all(old.parent().expect("under trash/"))?;
            fs::rename(inst.mod_dir(id), &old)?;
            if let Err(e) = fs::rename(&fresh, inst.mod_dir(id)) {
                fs::rename(&old, inst.mod_dir(id))?;
                return Err(e.into());
            }
            let updated = new_source.map(|s| -> Result<ModMeta> {
                let sha256 = if s.is_file() { Some(crate::lockfile::sha256_file(s)?) } else { None };
                Ok(ModMeta { source: Some(s.display().to_string()), sha256, ..meta.clone() })
            });
            inst.write_mod_meta(id, &ModMeta { fomod: fomod.clone(), ..updated.transpose()?.unwrap_or_else(|| meta.clone()) })
        })?;
        let plugins = top_plugins(&files);
        let label = if new_source.is_some() { format!("update {id}") } else { format!("reinstall {id}") };
        inst.edit_swapping(&label, Some((id, &kept)), |p| {
            add_plugins(p, &plugins, enabled);
            Ok(())
        })?;
        Ok(report(id, &meta.name, &files, plugins, warnings, fomod))
    })();
    let _ = fs::remove_dir_all(&staging);
    if result.is_ok() {
        let _ = fs::remove_dir_all(preview_dir(inst));
    }
    result
}

/// The Nexus mod id in a file name as Nexus serves it: `<name>-<mod id>-<version>-<upload time>`.
pub fn nexus_id_in_name(file: &str) -> Option<u64> {
    let stem = Path::new(file).file_stem()?.to_str()?;
    let parts: Vec<&str> = stem.split('-').collect();
    // The first number after the name; the version and the upload time follow it.
    let number = |p: &str| !p.is_empty() && !p.starts_with('0') && p.bytes().all(|b| b.is_ascii_digit());
    parts.iter().skip(1).take(parts.len().saturating_sub(2)).find(|p| number(p))?.parse().ok()
}

/// The installed mod `source` is another version of: the same Nexus mod (`nexus_mod`, else the
/// id in the file name), going by each mod's recorded Nexus id or its archive's name. (id, name).
pub fn same_mod(inst: &Instance, source: &Path, nexus_mod: Option<u64>) -> Result<Option<(String, String)>> {
    let Some(want) = nexus_mod.or_else(|| nexus_id_in_name(&source.file_name()?.to_string_lossy())) else { return Ok(None) };
    Ok(inst.profile()?.mods.iter().filter(|m| !crate::instance::is_separator(&m.id)).find_map(|m| {
        let meta = inst.mod_meta(&m.id);
        let have = meta.nexus.as_ref().map(|n| n.mod_id).or_else(|| nexus_id_in_name(meta.source.as_deref()?));
        (have == Some(want)).then(|| (m.id.clone(), meta.name))
    }))
}

#[cfg(test)]
mod nexus_name_tests {
    #[test]
    fn reads_the_mod_id_from_nexus_file_names() {
        let id = super::nexus_id_in_name;
        assert_eq!(id("Address Library for SKSE Plugins-32444-11-1710536800.zip"), Some(32444));
        assert_eq!(id("Unofficial Skyrim Special Edition Patch-266-4-2-9a-1693919367.7z"), Some(266));
        assert_eq!(id(r"D:\Downloads\SkyUI_5_2_SE-12604-5-2SE.7z"), Some(12604));
        assert_eq!(id("Address Library All in One (1.7.104.0) v13 32444 13 2026-08-27T15-29Z Ae46W7Fw2.zip"), None);
        assert_eq!((id("Snow.zip"), id("tool:xedit"), id("Name-123.zip")), (None, None, None));
    }
}

fn top_plugins(files: &[String]) -> Vec<String> {
    files.iter().filter(|f| !f.contains('/') && is_plugin(f)).cloned().collect()
}

fn add_plugins(p: &mut crate::Profile, plugins: &[String], enable: bool) {
    for pl in plugins {
        if !p.plugins.iter().any(|e| e.name.eq_ignore_ascii_case(pl)) {
            p.plugins.push(PluginEntry { name: pl.clone(), enabled: enable });
        }
    }
}

fn report(id: &str, name: &str, files: &[String], plugins: Vec<String>, warnings: Vec<String>, fomod: Option<Choices>) -> InstallReport {
    InstallReport {
        id: id.into(),
        name: name.into(),
        root_files: files.iter().filter(|f| f.starts_with("root/")).count(),
        files: files.len(),
        plugins,
        warnings,
        fomod,
    }
}

/// Unpack `source` (or run its FOMOD installer) into `dest`, which must not exist yet, and index
/// it. Returns the files, the FOMOD choices used and any warnings.
fn build(inst: &Instance, source: &Path, staging: &Path, dest: &Path, choices: Option<&Choices>) -> Result<(Vec<String>, Option<Choices>, Vec<String>)> {
    crate::progress::report("installing", 0, 0);
    let archive = !source.is_dir();
    let raw = if archive {
        unpack(inst, source)?
    } else {
        let raw = staging.join("raw");
        fs::create_dir_all(&raw)?;
        copy_dir(source, &raw)?;
        raw
    };

    let mut warnings = Vec::new();
    let mut fomod = None;
    let layout = find_layout(raw)?;
    if archive && !matches!(layout, Layout::Fomod(_)) {
        // The moves below take files out of the unpacked archive, so it can't be reused.
        let _ = fs::remove_file(preview_dir(inst).join(".key"));
    }
    match layout {
        Layout::Fomod(root) => {
            let choices = choices.context("this mod has a FOMOD installer: look at its options with fomod_options, then install with choices ({} takes the defaults)")?;
            let walk = fomod_walk(inst, &root, choices)?;
            let (files, w) = walk.resolve(&root)?;
            warnings.extend(walk.warnings.iter().cloned().chain(w));
            if files.is_empty() {
                bail!("the chosen options install no files");
            }
            for (rel, src) in files.values() {
                let d = dest.join(rel);
                fs::create_dir_all(d.parent().unwrap())?;
                if fs::hard_link(src, &d).is_err() {
                    fs::copy(src, &d)?;
                }
            }
            fomod = Some(walk.choices);
        }
        Layout::Data(d, guessed) => {
            if guessed {
                warnings.push("no Data folder markers found; installed the archive contents as Data files".into());
            }
            fs::rename(d, dest)?;
        }
        Layout::Root(r) => {
            fs::create_dir_all(dest)?;
            fs::rename(r, dest.join("root"))?;
        }
        Layout::DataWithRoot { root, data } => {
            fs::rename(&data, dest)?;
            let root_dest = dest.join("root");
            for e in fs::read_dir(&root)? {
                let e = e?;
                fs::create_dir_all(&root_dest)?;
                fs::rename(e.path(), root_dest.join(e.file_name()))?;
            }
        }
    }

    let files = index_dir(dest)?;
    fs::write(dest.join(".rmm-index"), files.join("\n"))?;
    Ok((files, fomod, warnings))
}

/// Run a FOMOD installer found at `root` against the instance's current setup.
fn fomod_walk(inst: &Instance, root: &Path, choices: &Choices) -> Result<Walk> {
    let config = child_ci(root, "fomod").and_then(|f| child_ci(&f, "moduleconfig.xml")).context("fomod/ModuleConfig.xml is missing")?;
    let installer = Installer::parse(&fs::read(config)?)?;
    let profile = inst.profile()?;
    let winners = crate::conflicts::file_map(inst, &profile)?.winners();
    let state = |f: &str| {
        if !winners.contains_key(&path_key(f)) && !inst.data_dir().join(f.replace('\\', "/")).exists() {
            return FileState::Missing;
        }
        // Plugins the profile doesn't list (base game, Creation Club) are loaded by the game.
        let off = is_plugin(f) && profile.plugins.iter().any(|p| p.name.eq_ignore_ascii_case(f) && !p.enabled);
        if off { FileState::Inactive } else { FileState::Active }
    };
    let loader = inst.config.game.loader();
    let script_extender = inst.config.game_path.join(loader).is_file() || winners.contains_key(&path_key(&format!("root/{loader}")));
    installer.walk(choices, &Env { file_state: &state, script_extender })
}

fn preview_dir(inst: &Instance) -> PathBuf {
    inst.root.join(".staging").join("preview")
}

/// Unpack an archive into the one-slot preview cache, so fomod_options and the install that
/// follows extract it once. Keyed by path, size and mtime.
fn unpack(inst: &Instance, source: &Path) -> Result<PathBuf> {
    let meta = fs::metadata(source).with_context(|| format!("cannot read {}", source.display()))?;
    let key = format!("{}|{}|{:?}", source.display(), meta.len(), meta.modified().ok());
    let dir = preview_dir(inst);
    let raw = dir.join("raw");
    if fs::read_to_string(dir.join(".key")).is_ok_and(|k| k == key) && raw.is_dir() {
        return Ok(raw);
    }
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&raw)?;
    crate::progress::report("extracting", 0, 0);
    extract(source, &raw)?;
    fs::write(dir.join(".key"), key)?;
    Ok(raw)
}

fn child_ci(dir: &Path, name: &str) -> Option<PathBuf> {
    fs::read_dir(dir).ok()?.filter_map(|e| e.ok()).find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(name)).map(|e| e.path())
}

pub fn uninstall(inst: &Instance, id: &str) -> Result<()> {
    uninstall_mods(inst, &[id.to_string()])
}

/// Uninstall mods (their folders go to trash/, so undo brings them back) and remove
/// separators, in one undo step.
pub fn uninstall_mods(inst: &Instance, ids: &[String]) -> Result<()> {
    let label = match ids {
        [one] if crate::instance::is_separator(one) => format!("remove separator {}", &one[4..]),
        [one] => format!("uninstall {one}"),
        _ => format!("uninstall {} mods", ids.len()),
    };
    inst.edit(&label, |p| {
        if let Some(id) = ids.iter().find(|id| !p.mods.iter().any(|m| m.id == **id)) {
            bail!("mod '{id}' is not in the profile");
        }
        p.mods.retain(|m| !ids.contains(&m.id));
        p.colors.retain(|k, _| !ids.contains(k));
        Ok(())
    })?;
    for id in ids.iter().filter(|id| !crate::instance::is_separator(id)) {
        inst.trash_mod(id)?;
    }
    Ok(())
}

/// Turn everything in overwrite/ into a new mod named `name`, enabled at the end of the list
/// (undoable like an install). overwrite/ starts empty again.
pub fn mod_from_overwrite(inst: &Instance, name: &str) -> Result<InstallReport> {
    let name = name.trim();
    if name.is_empty() {
        bail!("the new mod needs a name");
    }
    let over = inst.mod_dir(crate::instance::OVERWRITE);
    let files = inst.mod_files(crate::instance::OVERWRITE)?;
    if files.is_empty() {
        bail!("overwrite/ is empty");
    }
    let id = unique_id(inst, &sanitize(name));
    let dest = inst.mod_dir(&id);
    inst.locked(|| {
        fs::create_dir_all(inst.mods_dir())?;
        fs::rename(&over, &dest)?;
        fs::create_dir_all(&over)?;
        fs::write(dest.join(".rmm-index"), files.join("\n"))?;
        inst.write_mod_meta(&id, &ModMeta { name: name.into(), ..Default::default() })
    })?;
    let plugins = top_plugins(&files);
    inst.edit(&format!("create {id} from overwrite"), |p| {
        let ids: Vec<String> = p.mods.iter().map(|m| m.id.clone()).collect();
        p.mods.insert(crate::rules::place(inst, &ids, &id)?, ModEntry { id: id.clone(), enabled: true });
        add_plugins(p, &plugins, true);
        Ok(())
    })?;
    Ok(report(&id, name, &files, plugins, vec![], None))
}

/// Empty overwrite/ in one undo step: its files become a mod, "Cleared overwrite", that goes
/// straight to trash/. The step before lists that mod, so undo brings the files back as a mod at
/// the end of the list. Returns how many files went.
pub fn clear_overwrite(inst: &Instance) -> Result<usize> {
    let files = inst.mod_files(crate::instance::OVERWRITE)?;
    if files.is_empty() {
        return Ok(0);
    }
    let id = unique_id(inst, "Cleared overwrite");
    inst.locked(|| {
        let (over, dest) = (inst.mod_dir(crate::instance::OVERWRITE), inst.mod_dir(&id));
        fs::create_dir_all(inst.mods_dir())?;
        fs::rename(&over, &dest)?;
        fs::create_dir_all(&over)?;
        fs::write(dest.join(".rmm-index"), files.join("\n"))?;
        inst.write_mod_meta(&id, &ModMeta { name: id.clone(), ..Default::default() })?;
        inst.list_quietly(&id)
    })?;
    inst.edit("clear overwrite", |p| {
        p.mods.retain(|m| m.id != id);
        Ok(())
    })?;
    inst.trash_mod(&id)?;
    Ok(files.len())
}

/// What trash/ holds: uninstalled mods, replaced files of reinstalled mods and tool output, and
/// cleared overwrite/. Undo restores mods from it, so nothing there is deleted on its own.
#[derive(Debug, Serialize)]
pub struct Trash {
    pub entries: usize,
    pub bytes: u64,
}

pub fn trash(inst: &Instance) -> Result<Trash> {
    let dir = inst.root.join("trash");
    let entries = fs::read_dir(&dir).map_or(0, |rd| rd.count());
    let files = walkdir::WalkDir::new(&dir).into_iter().filter_map(|e| e.ok()).filter(|e| e.file_type().is_file());
    Ok(Trash { entries, bytes: files.filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum() })
}

/// Delete everything in trash/ for good to free the space. Undo can't bring those mods back
/// afterwards (they leave the list instead). Returns what was there.
pub fn empty_trash(inst: &Instance) -> Result<Trash> {
    inst.locked(|| {
        let t = trash(inst)?;
        for e in fs::read_dir(inst.root.join("trash")).into_iter().flatten().flatten() {
            if e.file_type()?.is_dir() { fs::remove_dir_all(e.path())? } else { fs::remove_file(e.path())? }
        }
        Ok(t)
    })
}

/// An archive in the instance's `downloads/` folder (where Nexus downloads go).
#[derive(Debug, Serialize)]
pub struct Download {
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
    /// Unix seconds.
    pub modified: u64,
    /// Mods installed from a file of this name.
    pub mods: Vec<String>,
}

/// The downloads folder, newest first.
// ponytail: install from here records no Nexus ids; keep fetch's details beside the file if
// update checks for such mods matter.
pub fn downloads(inst: &Instance) -> Result<Vec<Download>> {
    let mut from: std::collections::HashMap<String, Vec<String>> = Default::default();
    for e in fs::read_dir(inst.mods_dir()).into_iter().flatten().flatten() {
        let id = e.file_name().to_string_lossy().into_owned();
        if let Some(file) = inst.mod_meta(&id).source.as_deref().and_then(|s| Path::new(s).file_name()) {
            from.entry(file.to_string_lossy().to_lowercase()).or_default().push(id);
        }
    }
    let mut out = Vec::new();
    for e in fs::read_dir(inst.downloads_dir()).into_iter().flatten().flatten() {
        let (name, m) = (e.file_name().to_string_lossy().into_owned(), e.metadata()?);
        if !m.is_file() || name.ends_with(".part") {
            continue;
        }
        let modified = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
        out.push(Download { mods: from.remove(&name.to_lowercase()).unwrap_or_default(), path: e.path(), size: m.len(), modified, name });
    }
    out.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.name.cmp(&b.name)));
    Ok(out)
}

/// Delete a file from the downloads folder. Mods installed from it stay; they can't be
/// reinstalled from it any more.
pub fn delete_download(inst: &Instance, name: &str) -> Result<()> {
    let path = inst.downloads_dir().join(name);
    if name.contains(['/', '\\']) || name.starts_with('.') || !path.is_file() {
        bail!("no download named {name}");
    }
    Ok(fs::remove_file(path)?)
}

pub fn is_plugin(name: &str) -> bool {
    ext_of(name).is_some_and(|e| matches!(e.as_str(), "esp" | "esm" | "esl"))
}

fn ext_of(name: &str) -> Option<String> {
    Path::new(name).extension().map(|e| e.to_string_lossy().to_lowercase())
}

fn extract(source: &Path, dest: &Path) -> Result<()> {
    if source.is_dir() {
        return copy_dir(source, dest);
    }
    match ext_of(&source.to_string_lossy()).as_deref() {
        Some("zip") => zip::ZipArchive::new(fs::File::open(source)?)?.extract(dest)?,
        Some("7z") => sevenz_rust2::decompress_file(source, dest).map_err(|e| anyhow!("7z: {e}"))?,
        Some("rar") => extract_rar(source, dest)?,
        _ => bail!("{} is not a folder, .zip, .7z or .rar", source.display()),
    }
    Ok(())
}

/// RAR through libarchive's bsdtar, which reads RAR4 and RAR5 (not encrypted ones). Windows 10
/// and 11 ship it as System32\tar.exe; elsewhere it's `bsdtar` (libarchive-tools). Like any bsdtar
/// run without -P, it won't write outside `dest`.
fn extract_rar(source: &Path, dest: &Path) -> Result<()> {
    let tar = std::env::var_os("SystemRoot").map(|r| Path::new(&r).join("System32").join("tar.exe")).filter(|t| t.is_file()).unwrap_or_else(|| "bsdtar".into());
    fs::create_dir_all(dest)?;
    let mut cmd = std::process::Command::new(&tar);
    cmd.arg("-xf").arg(source).arg("-C").arg(dest);
    // CREATE_NO_WINDOW: no console flashes up over the desktop app.
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
    let out = cmd.output().with_context(|| format!("RAR needs {}", tar.display()))?;
    if !out.status.success() {
        bail!("RAR: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

fn copy_dir(src: &Path, dest: &Path) -> Result<()> {
    for e in walkdir::WalkDir::new(src).min_depth(1) {
        let e = e?;
        let target = dest.join(e.path().strip_prefix(src)?);
        if e.file_type().is_dir() {
            fs::create_dir_all(&target)?;
        } else {
            fs::copy(e.path(), &target)?;
        }
    }
    Ok(())
}

enum Layout {
    /// Folder holds `fomod/ModuleConfig.xml`; its installer decides what goes where.
    Fomod(PathBuf),
    /// Folder is Data contents. `bool` = guessed (no markers found).
    Data(PathBuf, bool),
    /// Folder is game-root contents (ENB, script extender loaders).
    Root(PathBuf),
    /// Folder holds root files plus a `Data` subfolder (typical SKSE/F4SE archive).
    DataWithRoot { root: PathBuf, data: PathBuf },
}

fn find_layout(mut dir: PathBuf) -> Result<Layout> {
    loop {
        let entries: Vec<_> = fs::read_dir(&dir)?.filter_map(|e| e.ok()).collect();
        let lower = |e: &fs::DirEntry| e.file_name().to_string_lossy().to_lowercase();
        let is_dir = |e: &fs::DirEntry| e.file_type().map(|t| t.is_dir()).unwrap_or(false);

        if entries.iter().any(|e| is_dir(e) && lower(e) == "fomod" && child_ci(&e.path(), "moduleconfig.xml").is_some()) {
            return Ok(Layout::Fomod(dir));
        }
        if let Some(data) = entries.iter().find(|e| is_dir(e) && lower(e) == "data") {
            return Ok(Layout::DataWithRoot { data: data.path(), root: dir });
        }
        let data_like = entries.iter().any(|e| {
            let n = lower(e);
            if is_dir(e) { DATA_DIRS.contains(&n.as_str()) } else { ext_of(&n).is_some_and(|x| DATA_EXTS.contains(&x.as_str())) }
        });
        if data_like {
            return Ok(Layout::Data(dir, false));
        }
        if entries.iter().any(|e| !is_dir(e) && ext_of(&lower(e)).is_some_and(|x| ROOT_EXTS.contains(&x.as_str()))) {
            return Ok(Layout::Root(dir));
        }
        let subdirs: Vec<_> = entries.iter().filter(|e| is_dir(e)).collect();
        if subdirs.len() == 1 {
            dir = subdirs[0].path();
            continue;
        }
        return Ok(Layout::Data(dir, true));
    }
}

pub(crate) fn sanitize(name: &str) -> String {
    let s: String = name.chars().filter(|c| !r#"<>:"/\|?*"#.contains(*c) && !c.is_control()).collect();
    let s = s.trim().trim_end_matches('.').to_string();
    if s.is_empty() { "mod".into() } else { s }
}

pub(crate) fn unique_id(inst: &Instance, base: &str) -> String {
    let taken = |id: &str| inst.mod_dir(id).exists() || inst.root.join("trash").join(id).exists();
    if !taken(base) {
        return base.into();
    }
    (2..).map(|n| format!("{base} ({n})")).find(|id| !taken(id)).unwrap()
}
