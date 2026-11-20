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
    if let Some(web) = crate::web::of_source(inst, source) {
        inst.write_mod_meta(&r.id, &ModMeta { web: Some(web), ..inst.mod_meta(&r.id) })?;
    }
    Ok(r)
}

/// The mod name, Nexus mod id and version in a Nexus download's file name, as MO2 reads them:
/// "Address Library - All In One 47327 1.11.240 2026-08-21T23-48Z Hbu6377Rl" (the site's names
/// now), "Unofficial Skyrim Special Edition Patch-266-4-2-5a-1623868815" (older ones), or without
/// the upload time and with any version: "Relationship Dialogue Overhaul - RDO Final-1187-Final".
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
        // No upload time: "<name>-<id>-<version>", the id the first number of 3 digits or more
        // (shorter ones are a name's own: "UIExtensions v1-2-0-17561-1-2-0"), the version a word.
        return stem.match_indices('-').find_map(|(i, _)| {
            let (id, version) = stem[i + 1..].split_once('-')?;
            let ok = id.len() >= 3 && !id.starts_with('0') && !version.is_empty() && !version.contains(' ');
            ok.then(|| found(&stem[..i], id, &version.replace('-', "."))).flatten()
        });
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

/// Whether the mod at `source` has a C# script installer (`fomod/script.cs`), which only runs
/// when the user allows it (`csharp::allowing`).
pub fn has_script(inst: &Instance, source: &Path) -> Result<bool> {
    let raw = if source.is_dir() { source.to_path_buf() } else { unpack(inst, source)? };
    Ok(matches!(find_layout(raw)?, Layout::Script(_)))
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
    inst.write_mod_meta(id, &ModMeta { name: name.into(), source: Some(source.display().to_string()), fomod: fomod.clone(), sha256, ini_tweaks: script_tweak(&files), ..Default::default() })?;
    let plugins = mod_plugins(inst, id, &files);
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

/// The INI tweak a C# installer script wrote (its INI settings), which starts turned on.
fn script_tweak(files: &[String]) -> Vec<String> {
    let t = format!("{}/{}", crate::ini_tweaks::DIR, crate::csharp::TWEAK);
    files.iter().filter(|f| **f == t).map(|_| crate::csharp::TWEAK.to_string()).collect()
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

/// Where a mod keeps the files the user hid: out of its index, so deploys, conflicts and the
/// plugin list don't see them.
pub const HIDDEN_DIR: &str = ".rmm-hidden";

/// Hide a mod's files or folders, or with `hide` false show them again. Hidden files move to
/// `<mod>/.rmm-hidden/`, out of deploys, conflicts and the plugin list (MO2's `.mohidden`; a
/// hidden plugin is MO2's optional ESP), and the mod remembers them, so a reinstall or update
/// hides them again. One undo step. Returns the files hidden or shown.
pub fn hide_files(inst: &Instance, id: &str, paths: &[String], hide: bool) -> Result<Vec<String>> {
    if crate::instance::is_separator(id) || id == crate::instance::OVERWRITE || !inst.mod_dir(id).is_dir() {
        bail!("'{id}' is not an installed mod (files in Overwrite can be moved into one first)");
    }
    let dir = inst.mod_dir(id);
    let mut meta = inst.mod_meta(id);
    let picked = pick(&if hide { inst.mod_files(id)? } else { meta.hidden.clone() }, paths)?;
    let kept = inst.keep_files(id)?;
    let label = format!("{} {} in {id}", if hide { "hide" } else { "show" }, files_label(&picked));
    inst.edit_swapping(&label, &[(id, &kept)], |_| {
        for f in &picked {
            let (shown, hidden) = (dir.join(f), dir.join(HIDDEN_DIR).join(f));
            let (from, to) = if hide { (shown, hidden) } else { (hidden, shown) };
            if from.is_file() {
                fs::create_dir_all(to.parent().expect("a file's folder"))?;
                fs::rename(&from, &to)?;
                prune_empty(&dir, &from);
            }
        }
        if hide {
            meta.hidden.extend(picked.iter().cloned());
            meta.hidden.sort();
            meta.hidden.dedup();
        } else {
            meta.hidden.retain(|h| !picked.contains(h));
        }
        crate::instance::write_atomic(&dir.join(".rmm-index"), &index_dir(&dir)?.join("\n"))?;
        inst.write_mod_meta(id, &meta)
    })?;
    Ok(picked)
}

/// "Engine.log" or "3 files", for a history label.
fn files_label(files: &[String]) -> String {
    match files {
        [one] => one.rsplit('/').next().unwrap_or(one).to_owned(),
        _ => format!("{} files", files.len()),
    }
}

/// Move files or folders from one mod (or Overwrite) into another (or Overwrite): MCM settings
/// and tool output into the mod they belong with. A file the target has already is refused
/// unless `replace`. One undo step, which brings back the target's replaced copies too. Returns
/// the files moved.
pub fn move_files(inst: &Instance, from: &str, paths: &[String], to: &str, replace: bool) -> Result<Vec<String>> {
    use crate::instance::{OVERWRITE, is_separator};
    for id in [from, to] {
        if is_separator(id) || (id != OVERWRITE && !inst.mod_dir(id).is_dir()) {
            bail!("'{id}' is not an installed mod");
        }
    }
    if from == to {
        bail!("pick another mod to move them to");
    }
    let picked = pick(&inst.mod_files(from)?, paths)?;
    let (src, dest) = (inst.mod_dir(from), inst.mod_dir(to));
    let clash: Vec<&String> = picked.iter().filter(|f| dest.join(f).exists()).collect();
    if !clash.is_empty() && !replace {
        let some: Vec<&str> = clash.iter().take(3).map(|f| f.as_str()).collect();
        bail!("{to} has {} of these already ({}); replacing them puts its copies in trash", clash.len(), some.join(", "));
    }
    // Both folders as they are now, hard-linked under trash/, for undo to swap back.
    let kept = [inst.keep_files(from)?, inst.keep_files(to)?];
    let name = |id: &str| if id == OVERWRITE { "Overwrite".to_owned() } else { id.to_owned() };
    let label = format!("move {} from {} to {}", files_label(&picked), name(from), name(to));
    inst.edit_swapping(&label, &[(from, &kept[0]), (to, &kept[1])], |_| {
        for f in &picked {
            let target = dest.join(f);
            if target.exists() {
                fs::remove_file(&target)?; // its copy is in `kept`
            }
            fs::create_dir_all(target.parent().expect("a file's folder"))?;
            if fs::rename(src.join(f), &target).is_err() {
                fs::copy(src.join(f), &target)?;
                fs::remove_file(src.join(f))?;
            }
            prune_empty(&src, &src.join(f));
        }
        for (id, dir) in [(from, &src), (to, &dest)] {
            if id != OVERWRITE {
                crate::instance::write_atomic(&dir.join(".rmm-index"), &index_dir(dir)?.join("\n"))?;
            }
        }
        Ok(())
    })?;
    Ok(picked)
}

/// The files in `pool` that `paths` name, each a file or a folder (every file under it), in any case.
fn pick(pool: &[String], paths: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for p in paths {
        let want = p.replace('\\', "/").trim_matches('/').to_lowercase();
        let under = format!("{want}/");
        let found: Vec<&String> = pool.iter().filter(|f| f.to_lowercase() == want || f.to_lowercase().starts_with(&under)).collect();
        if found.is_empty() {
            bail!("no file or folder '{p}' there");
        }
        out.extend(found.into_iter().cloned());
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Remove the folders a moved file leaves empty, up to (not including) `root`.
fn prune_empty(root: &Path, file: &Path) {
    let mut d = file.parent();
    while let Some(dir) = d.filter(|d| *d != root && d.starts_with(root)) {
        if fs::remove_dir(dir).is_err() {
            break;
        }
        d = dir.parent();
    }
}

/// In a freshly built mod folder, hide again the files the user had hidden; returns the rest.
fn rehide(dir: &Path, hidden: &[String], files: Vec<String>) -> Result<Vec<String>> {
    if hidden.is_empty() {
        return Ok(files);
    }
    for h in hidden.iter().filter(|h| dir.join(h).is_file()) {
        let to = dir.join(HIDDEN_DIR).join(h);
        fs::create_dir_all(to.parent().expect("a file's folder"))?;
        fs::rename(dir.join(h), to)?;
        prune_empty(dir, &dir.join(h));
    }
    let files = index_dir(dir)?;
    fs::write(dir.join(".rmm-index"), files.join("\n"))?;
    Ok(files)
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
    let edited = crate::workspace::live_edits(inst, id).unwrap_or_default();
    let staging = inst.root.join(".staging").join(id);
    let _ = fs::remove_dir_all(&staging);
    let choices = choices.or(meta.fomod.as_ref()).cloned().unwrap_or_default();
    let result = (|| {
        fs::create_dir_all(&staging)?;
        let fresh = staging.join("mod");
        let (files, fomod, warnings) = build(inst, &source, &staging, &fresh, Some(&choices))?;
        let files = if meta.game_folder { to_game_folder(&fresh)? } else { files };
        let files = rehide(&fresh, &meta.hidden, files)?;
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
                Ok(ModMeta { source: Some(s.display().to_string()), sha256, web: crate::web::of_source(inst, s).or_else(|| meta.web.clone()), ..meta.clone() })
            });
            let mut ini_tweaks = meta.ini_tweaks.clone();
            ini_tweaks.extend(script_tweak(&files).into_iter().filter(|t| !meta.ini_tweaks.contains(t)));
            inst.write_mod_meta(id, &ModMeta { fomod: fomod.clone(), ini_tweaks, ..updated.transpose()?.unwrap_or_else(|| meta.clone()) })
        })?;
        let plugins = mod_plugins(inst, id, &files);
        let label = if new_source.is_some() { format!("update {id}") } else { format!("reinstall {id}") };
        inst.edit_swapping(&label, &[(id, &kept)], |p| {
            add_plugins(p, &plugins, enabled);
            Ok(())
        })?;
        let mut warnings = warnings;
        if !edited.is_empty() {
            warnings.push(format!("the files you edited in rmm ({}) are the new archive's now; Undo puts your edited ones back", edited.join(", ")));
        }
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
    let number = |p: &str| p.len() >= 3 && !p.starts_with('0') && p.bytes().all(|b| b.is_ascii_digit());
    parts.iter().skip(1).take(parts.len().saturating_sub(2)).find(|p| number(p))?.parse().ok()
}

/// The installed mod `source` is another version of: the same Nexus mod (`nexus_mod`, else the
/// id in the file name), going by each mod's recorded Nexus id or its archive's name. (id, name).
/// When several installed mods came from that page (MfgFix and MfgFix MCM), the one whose file
/// has the same name; none when no name settles it, so the download installs as a mod of its own.
pub fn same_mod(inst: &Instance, source: &Path, nexus_mod: Option<u64>) -> Result<Option<(String, String)>> {
    // From a web page (a GitHub repository): the one mod installed from that page.
    if let Some(web) = crate::web::of_source(inst, source).filter(|_| nexus_mod.is_none()) {
        let mut from = inst.profile()?.mods.into_iter().map(|m| (inst.mod_meta(&m.id), m.id)).filter(|(meta, _)| meta.web.as_ref().is_some_and(|w| w.page == web.page));
        return Ok(match (from.next(), from.next()) {
            (Some((meta, id)), None) => Some((id, meta.name)),
            _ => None,
        });
    }
    let Some(want) = nexus_mod.or_else(|| nexus_id_in_name(&source.file_name()?.to_string_lossy())) else { return Ok(None) };
    let found: Vec<(String, crate::instance::ModMeta)> = inst
        .profile()?
        .mods
        .iter()
        .filter(|m| !crate::instance::is_separator(&m.id))
        .filter_map(|m| {
            let meta = inst.mod_meta(&m.id);
            let have = meta.nexus.as_ref().map(|n| n.mod_id).or_else(|| nexus_id_in_name(meta.source.as_deref()?));
            (have == Some(want)).then(|| (m.id.clone(), meta))
        })
        .collect();
    if found.len() < 2 {
        return Ok(found.into_iter().next().map(|(id, meta)| (id, meta.name)));
    }
    // A file's name on the page, from its archive's name ("MfgFix MCM-11669-1-2-1600000000").
    let file_name = |p: &Path| {
        let stem = if p.extension().is_some_and(|e| ["zip", "7z", "rar"].iter().any(|x| e.eq_ignore_ascii_case(x))) { p.file_stem() } else { p.file_name() };
        let stem = stem?.to_string_lossy().into_owned();
        Some(nexus_name(&stem).map_or(stem, |(n, _)| n).to_lowercase())
    };
    let Some(name) = file_name(source) else { return Ok(None) };
    let mut same = found.into_iter().filter(|(_, meta)| meta.source.as_deref().and_then(|s| file_name(Path::new(s))).as_ref() == Some(&name));
    Ok(match (same.next(), same.next()) {
        (Some((id, meta)), None) => Some((id, meta.name)),
        _ => None,
    })
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
        assert_eq!(id("UIExtensions v1-2-0-17561-1-2-0.7z"), Some(17561));
    }

    #[test]
    fn reads_old_names_without_an_upload_time() {
        let read = |s: &str| super::nexus_name(s).map(|(n, x)| (n, x.mod_id, x.version.unwrap_or_default()));
        let want = |n: &str, id: u64, v: &str| Some((n.to_string(), id, v.to_string()));
        assert_eq!(read("Relationship Dialogue Overhaul - RDO Final-1187-Final"), want("Relationship Dialogue Overhaul - RDO Final", 1187, "Final"));
        assert_eq!(read("Reverb and Ambiance Overhaul-701-1-5"), want("Reverb and Ambiance Overhaul", 701, "1.5"));
        assert_eq!(read("UIExtensions v1-2-0-17561-1-2-0"), want("UIExtensions v1-2-0", 17561, "1.2.0"));
        assert_eq!(read("LazyTools 0.10.1-13381-0-10-1"), want("LazyTools 0.10.1", 13381, "0.10.1"));
        assert_eq!((read("collection-h2uqa3-70"), read("Snow-12"), read("My Mod-2-Fixes")), (None, None, None));
    }
}

pub(crate) fn top_plugins(files: &[String]) -> Vec<String> {
    files.iter().filter(|f| !f.contains('/') && is_plugin(f)).cloned().collect()
}

/// A mod's plugins, each after the ones it's built on (its masters), so a patch never loads before
/// the plugin it patches from the same mod; otherwise in archive order.
fn mod_plugins(inst: &Instance, id: &str, files: &[String]) -> Vec<String> {
    let dir = inst.mod_dir(id);
    let mut left: Vec<(String, Vec<String>)> = top_plugins(files)
        .into_iter()
        .map(|name| {
            let mut p = esplugin::Plugin::new(inst.config.game.esp_id(), &dir.join(&name));
            let masters = match p.parse_file(esplugin::ParseOptions::header_only()) {
                Ok(()) => p.masters().unwrap_or_default().iter().map(|m| m.to_lowercase()).collect(),
                Err(_) => vec![],
            };
            (name, masters)
        })
        .collect();
    let mut out: Vec<String> = Vec::new();
    while !left.is_empty() {
        // The first plugin whose masters from this mod are all placed; a cycle keeps the order.
        let pending: Vec<String> = left.iter().map(|(n, _)| n.to_lowercase()).collect();
        let i = left.iter().position(|(n, ms)| ms.iter().all(|m| *m == n.to_lowercase() || !pending.contains(m))).unwrap_or(0);
        out.push(left.remove(i).0);
    }
    out
}

pub(crate) fn add_plugins(p: &mut crate::Profile, plugins: &[String], enable: bool) {
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
        Layout::Script(root) => {
            let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let out = crate::csharp::run(inst, &root, &name)?;
            warnings.extend(out.warnings);
            let put = |to: &str, from: &Path, link: bool| -> Result<()> {
                let to = to.split('/').filter(|s| !s.is_empty() && *s != ".").collect::<Vec<_>>();
                if to.is_empty() || to.contains(&"..") {
                    bail!("the installer script named a file outside Data: {}", to.join("/"));
                }
                let d = dest.join(to.join("/"));
                fs::create_dir_all(d.parent().unwrap())?;
                if !link || fs::hard_link(from, &d).is_err() {
                    fs::copy(from, &d)?;
                }
                Ok(())
            };
            for (from, to) in &out.copies {
                put(to, &root.join(from), true)?;
            }
            for (to, file) in &out.made {
                put(to, file, false)?;
            }
            if !out.ini.is_empty() {
                let t = dest.join(crate::ini_tweaks::DIR);
                fs::create_dir_all(&t)?;
                fs::write(t.join(crate::csharp::TWEAK), &out.ini)?;
            }
            if !dest.exists() {
                bail!("the installer script installed no files");
            }
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
    let installer = installer_at(root)?;
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
    let mut walk = installer.walk(choices, &Env { file_state: &state, script_extender })?;
    if child_ci(root, "wizard.txt").is_some() && child_ci(root, "fomod").and_then(|f| child_ci(&f, "moduleconfig.xml")).is_none() {
        walk.warnings.push("the package has a BAIN wizard (wizard.txt), which rmm doesn't run: its sub-packages were picked by hand or by name (00… on)".into());
    }
    Ok(walk)
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
    let plugins = mod_plugins(inst, &id, &files);
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
    /// Its mod and file on Nexus, when rmm downloaded it or `nexus::identify` found it; otherwise
    /// the mod its file name carries (no `file_id`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nexus: Option<crate::nexus::Identified>,
    /// The page it came from, when rmm downloaded it from a link or took it from the browser.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub web: Option<crate::web::WebRef>,
}

/// The downloads folder, newest first.
pub fn downloads(inst: &Instance) -> Result<Vec<Download>> {
    let mut from: std::collections::HashMap<String, Vec<String>> = Default::default();
    for e in fs::read_dir(inst.mods_dir()).into_iter().flatten().flatten() {
        let id = e.file_name().to_string_lossy().into_owned();
        if let Some(file) = inst.mod_meta(&id).source.as_deref().and_then(|s| Path::new(s).file_name()) {
            from.entry(file.to_string_lossy().to_lowercase()).or_default().push(id);
        }
    }
    let mut known = crate::nexus::identified(inst);
    let mut web = crate::web::known(inst);
    let mut out = Vec::new();
    for e in fs::read_dir(inst.downloads_dir()).into_iter().flatten().flatten() {
        let (name, m) = (e.file_name().to_string_lossy().into_owned(), e.metadata()?);
        if !m.is_file() || name.ends_with(".part") || name.starts_with('.') {
            continue;
        }
        let modified = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
        let lower = name.to_lowercase();
        // Not identified: the mod and version a Nexus download's name carries.
        let named = || Path::new(&name).file_stem()?.to_str().and_then(nexus_name).map(|(n, nexus)| crate::nexus::Identified { name: Some(n), nexus });
        let nexus = known.remove(&lower).or_else(named);
        out.push(Download { mods: from.remove(&lower).unwrap_or_default(), nexus, web: web.remove(&lower), path: e.path(), size: m.len(), modified, name });
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

pub(crate) fn extract(source: &Path, dest: &Path) -> Result<()> {
    if source.is_dir() {
        return copy_dir(source, dest);
    }
    // By the first bytes, then the extension: archives are sometimes named for another format.
    let mut head = [0u8; 6];
    let _ = std::io::Read::read(&mut fs::File::open(source)?, &mut head);
    let kind = match head {
        [b'P', b'K', 3, 4, ..] => Some("zip"),
        [b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C] => Some("7z"),
        [b'R', b'a', b'r', b'!', ..] => Some("rar"),
        _ => None,
    };
    match kind.map(str::to_owned).or_else(|| ext_of(&source.to_string_lossy())).as_deref() {
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
    let mut cmd = crate::hidden(&tar);
    cmd.arg("-xf").arg(source).arg("-C").arg(dest);
    let out = cmd.output().with_context(|| format!("RAR needs {}", tar.display()))?;
    if !out.status.success() {
        bail!("RAR: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

pub(crate) fn copy_dir(src: &Path, dest: &Path) -> Result<()> {
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

/// For a mod of Vortex's "dinput" or "enb" type, which Vortex deploys to the game folder: move
/// what sits at the top of the mod and isn't Data content into `root/`. Data content (the Data
/// folders rmm knows, plugins and archives, a plugin's own INI) stays, since it only works in
/// Data. A name `root/` has already stays too. Rewrites the index; returns the files.
pub(crate) fn to_game_folder(dir: &Path) -> Result<Vec<String>> {
    let entries: Vec<fs::DirEntry> = fs::read_dir(dir)?.collect::<std::io::Result<_>>()?;
    let names: Vec<String> = entries.iter().map(|e| e.file_name().to_string_lossy().to_lowercase()).collect();
    let plugins: Vec<&str> = names.iter().filter(|n| is_plugin(n)).filter_map(|n| Some(n.rsplit_once('.')?.0)).collect();
    let root = dir.join("root");
    for (e, n) in entries.iter().zip(&names) {
        let data = if e.file_type()?.is_dir() {
            n == "root" || DATA_DIRS.contains(&n.as_str())
        } else {
            [crate::instance::MOD_META, ".rmm-index"].contains(&n.as_str())
                || ext_of(n).is_some_and(|x| DATA_EXTS.contains(&x.as_str()))
                || n.strip_suffix(".ini").is_some_and(|s| plugins.contains(&s))
        };
        let to = root.join(e.file_name());
        if !data && !to.exists() {
            fs::create_dir_all(&root)?;
            fs::rename(e.path(), to)?;
        }
    }
    let files = index_dir(dir)?;
    fs::write(dir.join(".rmm-index"), files.join("\n"))?;
    Ok(files)
}

enum Layout {
    /// Folder holds `fomod/ModuleConfig.xml`, or BAIN sub-packages; the installer decides what goes where.
    Fomod(PathBuf),
    /// Folder holds `fomod/script.cs`, a C# installer script.
    Script(PathBuf),
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
        if crate::csharp::script_in(&dir).is_some() {
            return Ok(Layout::Script(dir));
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
        if bain_packages(&dir).is_some() {
            return Ok(Layout::Fomod(dir));
        }
        return Ok(Layout::Data(dir, true));
    }
}

/// A Wrye Bash BAIN package's sub-packages: at least two top-level folders, each like a Data
/// folder, and no other folders but the ones BAIN ignores (MO2's test).
fn bain_packages(dir: &Path) -> Option<Vec<String>> {
    const IGNORED: &[&str] = &["fomod", "omod conversion data", "images", "screenshots", "docs"];
    let mut out = Vec::new();
    for e in fs::read_dir(dir).ok()?.flatten().filter(|e| e.path().is_dir()) {
        let name = e.file_name().to_string_lossy().into_owned();
        if IGNORED.contains(&name.to_lowercase().as_str()) || name.starts_with("--") {
            continue;
        }
        let data_like = fs::read_dir(e.path()).ok()?.flatten().any(|f| {
            let n = f.file_name().to_string_lossy().to_lowercase();
            if f.path().is_dir() { n == "data" || DATA_DIRS.contains(&n.as_str()) } else { ext_of(&n).is_some_and(|x| DATA_EXTS.contains(&x.as_str())) }
        });
        if !data_like {
            return None;
        }
        out.push(name);
    }
    out.sort_by_key(|n| n.to_lowercase());
    (out.len() >= 2).then_some(out)
}

/// The installer at `root`: its FOMOD (`fomod/ModuleConfig.xml`), or its BAIN sub-packages.
fn installer_at(root: &Path) -> Result<Installer> {
    if let Some(config) = child_ci(root, "fomod").and_then(|f| child_ci(&f, "moduleconfig.xml")) {
        return Installer::parse(&fs::read(config)?);
    }
    let packages = bain_packages(root).context("fomod/ModuleConfig.xml is missing")?;
    Ok(Installer::bain(&root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), &packages))
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
