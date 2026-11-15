//! Install a mod from a folder, .zip or .7z: extract, find the Data root (or run its FOMOD
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
pub fn install(inst: &Instance, source: &Path, name: Option<&str>, enable: bool, choices: Option<&Choices>) -> Result<InstallReport> {
    let name = name
        .map(str::to_owned)
        .or_else(|| source.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .context("cannot derive a mod name from the source path")?;
    let id = unique_id(inst, &sanitize(&name));
    let staging = inst.root.join(".staging").join(&id);
    let _ = fs::remove_dir_all(&staging);
    let result = install_inner(inst, source, &staging, &id, &name, enable, choices);
    let _ = fs::remove_dir_all(&staging);
    if result.is_ok() {
        let _ = fs::remove_dir_all(preview_dir(inst));
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

fn install_inner(inst: &Instance, source: &Path, staging: &Path, id: &str, name: &str, enable: bool, choices: Option<&Choices>) -> Result<InstallReport> {
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
    let dest = inst.mod_dir(id);
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
            fs::rename(d, &dest)?;
        }
        Layout::Root(r) => {
            fs::create_dir_all(&dest)?;
            fs::rename(r, dest.join("root"))?;
        }
        Layout::DataWithRoot { root, data } => {
            fs::rename(&data, &dest)?;
            let root_dest = dest.join("root");
            for e in fs::read_dir(&root)? {
                let e = e?;
                fs::create_dir_all(&root_dest)?;
                fs::rename(e.path(), root_dest.join(e.file_name()))?;
            }
        }
    }

    let files = index_dir(&dest)?;
    fs::write(dest.join(".rmm-index"), files.join("\n"))?;
    inst.write_mod_meta(id, &ModMeta { name: name.into(), source: Some(source.display().to_string()), fomod: fomod.clone() })?;
    let plugins: Vec<String> = files.iter().filter(|f| !f.contains('/') && is_plugin(f)).cloned().collect();

    inst.edit(&format!("install {id}"), |p| {
        p.mods.push(ModEntry { id: id.into(), enabled: enable });
        for pl in &plugins {
            if !p.plugins.iter().any(|e| e.name.eq_ignore_ascii_case(pl)) {
                p.plugins.push(PluginEntry { name: pl.clone(), enabled: enable });
            }
        }
        Ok(())
    })?;

    Ok(InstallReport {
        id: id.into(),
        name: name.into(),
        root_files: files.iter().filter(|f| f.starts_with("root/")).count(),
        files: files.len(),
        plugins,
        warnings,
        fomod,
    })
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
    extract(source, &raw)?;
    fs::write(dir.join(".key"), key)?;
    Ok(raw)
}

fn child_ci(dir: &Path, name: &str) -> Option<PathBuf> {
    fs::read_dir(dir).ok()?.filter_map(|e| e.ok()).find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(name)).map(|e| e.path())
}

pub fn uninstall(inst: &Instance, id: &str) -> Result<()> {
    inst.edit(&format!("uninstall {id}"), |p| {
        let before = p.mods.len();
        p.mods.retain(|m| m.id != id);
        if p.mods.len() == before {
            bail!("mod '{id}' is not in the profile");
        }
        Ok(())
    })?;
    inst.trash_mod(id)
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
        Some("rar") => bail!("RAR is not supported yet; extract it and install the folder"),
        _ => bail!("{} is not a folder, .zip or .7z", source.display()),
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
