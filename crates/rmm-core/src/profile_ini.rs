//! Profile-specific INIs and saves (MO2's two profile options), with real files. A real deploy
//! puts the profile's copies of the game's INIs (`profiles/<name>/ini/`) into My Games, and points
//! saves at `Saves/<profile>/` with sLocalSavePath in the main INI. Before that, and on purge,
//! what the game changed in My Games goes back to the profile it came from, and the user's own
//! INIs (kept in `ini-backup/`) come back when no profile wants its own.
//!
//! The user's own plugins.txt waits in `ini-backup/` too, from rmm's first write of it until a
//! purge puts it back.
use crate::health::{ini_get, ini_set};
use crate::instance::{Instance, Profile, write_atomic};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

const STATE: &str = "ini-state.json";

#[derive(Serialize, Deserialize, Default)]
struct State {
    /// The profile whose INIs are in My Games.
    #[serde(default)]
    profile: Option<String>,
    /// INIs rmm put there, and whether the user had one of that name (kept in ini-backup/).
    #[serde(default)]
    files: Vec<(String, bool)>,
    /// sLocalSavePath before rmm set it.
    #[serde(default)]
    saves: Option<Saves>,
    /// rmm has written plugins.txt; whether the user had one (kept in ini-backup/).
    #[serde(default)]
    plugins_txt: Option<bool>,
    /// Settings the enabled mods' INI tweaks wrote, with the value each had before.
    #[serde(default)]
    tweaks: Vec<Applied>,
}

#[derive(Serialize, Deserialize)]
struct Applied {
    /// The INI in My Games.
    file: String,
    section: String,
    key: String,
    before: Option<String>,
    /// The INI had no such section: putting it back removes the header rmm added.
    #[serde(default)]
    new_section: bool,
}

fn section_of(line: &[u8]) -> Option<String> {
    let t = String::from_utf8_lossy(line);
    t.trim().strip_prefix('[').and_then(|t| t.strip_suffix(']')).map(|s| s.trim().to_lowercase())
}

/// `ini` without the `[section]` header when no setting follows it.
fn drop_empty_section(ini: Vec<u8>, section: &str) -> Vec<u8> {
    let lines: Vec<&[u8]> = ini.split_inclusive(|b| *b == b'\n').collect();
    let Some(at) = lines.iter().position(|l| section_of(l).as_deref() == Some(&section.to_lowercase())) else { return ini };
    let empty = lines[at + 1..].iter().take_while(|l| section_of(l).is_none()).all(|l| l.trim_ascii().is_empty());
    if !empty {
        return ini;
    }
    lines.iter().enumerate().filter(|(i, _)| *i != at).flat_map(|(_, l)| l.iter().copied()).collect()
}

/// A struct, not `Option<Option<_>>`: JSON can't tell `Some(None)` from `None`.
#[derive(Serialize, Deserialize)]
struct Saves {
    before: Option<String>,
}

/// Bring My Games in line with the active profile's options. Runs at the end of a real deploy.
pub fn apply(inst: &Instance, p: &Profile) -> Result<()> {
    if inst.ini_dir().is_none() {
        return Ok(());
    }
    let mut st = load(inst)?;
    put_back(inst, &mut st)?;
    let cur = inst.config.profile.clone();
    if st.profile.as_ref().is_some_and(|old| !(p.local_inis && *old == cur)) {
        restore_own(inst, &mut st)?;
    }
    if p.local_inis && st.profile.is_none() {
        let dir = profile_ini_dir(inst, &cur);
        for name in names(inst) {
            let (path, mine) = (my_games(inst, &name), backup(inst, &name));
            if !st.files.iter().any(|f| f.0 == name) {
                let had = path.is_file();
                if had {
                    copy(&path, &mine)?;
                }
                st.files.push((name.clone(), had));
            }
            // A profile's first deploy starts from the user's own INIs.
            if !dir.join(&name).exists() && mine.is_file() {
                copy(&mine, &dir.join(&name))?;
            }
            if dir.join(&name).is_file() {
                copy(&dir.join(&name), &path)?;
            }
        }
        st.profile = Some(cur.clone());
    }
    if p.local_saves {
        let path = my_games(inst, inst.config.game.ini_name());
        let text = fs::read(&path).unwrap_or_default();
        st.saves = Some(Saves { before: ini_get(&String::from_utf8_lossy(&text), "General", "sLocalSavePath") });
        fs::write(&path, ini_set(&text, &[("General", "sLocalSavePath", Some(format!("Saves\\{cur}\\")))]))?;
    }
    apply_tweaks(inst, p, &mut st)?;
    save(inst, &st)
}

/// Write the enabled mods' INI tweaks into My Games, keeping each value they replace. A setting
/// goes to the Prefs INI when that already has it, otherwise to the main INI; an INI the game
/// hasn't made yet is left alone.
fn apply_tweaks(inst: &Instance, p: &Profile, st: &mut State) -> Result<()> {
    let want = crate::ini_tweaks::wanted(inst, p);
    if want.is_empty() {
        return Ok(());
    }
    let main = inst.config.game.ini_name().to_string();
    let prefs = main.replace(".ini", "Prefs.ini");
    let prefs_text = fs::read(my_games(inst, &prefs)).map(|t| String::from_utf8_lossy(&t).into_owned()).unwrap_or_default();
    for name in [main, prefs.clone()] {
        let path = my_games(inst, &name);
        let Ok(text) = fs::read(&path) else { continue };
        let lossy = String::from_utf8_lossy(&text).into_owned();
        let mine: Vec<_> = want.iter().filter(|s| ini_get(&prefs_text, &s.section, &s.key).is_some() == (name == prefs)).collect();
        if mine.is_empty() {
            continue;
        }
        for s in &mine {
            let new_section = !text.split_inclusive(|b| *b == b'\n').any(|l| section_of(l).as_deref() == Some(&s.section.to_lowercase()));
            st.tweaks.push(Applied { file: name.clone(), section: s.section.clone(), key: s.key.clone(), before: ini_get(&lossy, &s.section, &s.key), new_section });
        }
        let set: Vec<(&str, &str, Option<String>)> = mine.iter().map(|s| (s.section.as_str(), s.key.as_str(), Some(s.value.clone()))).collect();
        fs::write(&path, ini_set(&text, &set))?;
    }
    Ok(())
}

/// Give My Games back to the user: the game's changes go to their profile, the user's own INIs,
/// save folder and plugins.txt come back. Runs on purge.
pub fn release(inst: &Instance) -> Result<()> {
    let mut st = load(inst)?;
    put_back(inst, &mut st)?;
    restore_own(inst, &mut st)?;
    if let Some(had) = st.plugins_txt.take() {
        let (path, mine) = (crate::plugins::plugins_txt(inst)?, backup(inst, "plugins.txt"));
        if had {
            copy(&mine, &path)?;
            fs::remove_file(&mine)?;
        } else if path.exists() {
            fs::remove_file(&path)?;
        }
    }
    save(inst, &st)
}

/// Before rmm first writes plugins.txt (at `path`), keep the user's.
pub fn keep_plugins_txt(inst: &Instance, path: &Path) -> Result<()> {
    let mut st = load(inst)?;
    if st.plugins_txt.is_none() {
        let had = path.is_file();
        if had {
            copy(path, &backup(inst, "plugins.txt"))?;
        }
        st.plugins_txt = Some(had);
        save(inst, &st)?;
    }
    Ok(())
}

/// A profile was renamed: its INIs in My Games now belong to the new name.
pub fn renamed(inst: &Instance, from: &str, to: &str) -> Result<()> {
    let mut st = load(inst)?;
    if st.profile.as_deref() == Some(from) {
        st.profile = Some(to.into());
        save(inst, &st)?;
    }
    Ok(())
}

/// sLocalSavePath back as it was, then the INIs in My Games back to the profile they belong to
/// (unless it was deleted since).
fn put_back(inst: &Instance, st: &mut State) -> Result<()> {
    let mut files: Vec<String> = st.tweaks.iter().map(|a| a.file.clone()).collect();
    files.dedup();
    for name in files {
        if let Ok(text) = fs::read(my_games(inst, &name)) {
            let set: Vec<(&str, &str, Option<String>)> = st.tweaks.iter().filter(|a| a.file == name).map(|a| (a.section.as_str(), a.key.as_str(), a.before.clone())).collect();
            let added = st.tweaks.iter().filter(|a| a.file == name && a.new_section).map(|a| a.section.as_str());
            fs::write(my_games(inst, &name), added.fold(ini_set(&text, &set), drop_empty_section))?;
        }
    }
    st.tweaks.clear();
    if let Some(Saves { before }) = st.saves.take() {
        let path = my_games(inst, inst.config.game.ini_name());
        if let Ok(text) = fs::read(&path) {
            fs::write(&path, ini_set(&text, &[("General", "sLocalSavePath", before)]))?;
        }
    }
    if let Some(old) = &st.profile {
        let dir = profile_ini_dir(inst, old);
        if dir.parent().is_some_and(Path::is_dir) {
            for name in names(inst) {
                let path = my_games(inst, &name);
                if path.is_file() {
                    copy(&path, &dir.join(&name))?;
                }
            }
        }
    }
    Ok(())
}

fn restore_own(inst: &Instance, st: &mut State) -> Result<()> {
    for (name, had) in st.files.drain(..) {
        let (path, mine) = (my_games(inst, &name), backup(inst, &name));
        if had {
            copy(&mine, &path)?;
            fs::remove_file(&mine)?;
        } else if path.exists() {
            fs::remove_file(&path)?;
        }
    }
    st.profile = None;
    Ok(())
}

/// The game's INIs a profile can keep its own copies of.
pub(crate) fn names(inst: &Instance) -> [String; 3] {
    let main = inst.config.game.ini_name();
    [main.to_string(), main.replace(".ini", "Prefs.ini"), main.replace(".ini", "Custom.ini")]
}

fn my_games(inst: &Instance, name: &str) -> PathBuf {
    inst.ini_file(name).unwrap_or_default()
}

fn backup(inst: &Instance, name: &str) -> PathBuf {
    // Not under backup/: a purge without deploy.json moves all of that into the game folder.
    inst.root.join("ini-backup").join(name)
}

/// The folder the game saves in once the profile `p` is deployed.
pub fn save_dir(inst: &Instance, p: &Profile) -> Option<PathBuf> {
    let dir = inst.ini_dir()?;
    let rel = if p.local_saves {
        Some(format!("Saves\\{}\\", inst.config.profile))
    } else if let Some(Saves { before }) = load(inst).ok().and_then(|st| st.saves) {
        before // another profile's folder is set until the next deploy puts this back
    } else {
        fs::read(my_games(inst, inst.config.game.ini_name())).ok().and_then(|t| ini_get(&String::from_utf8_lossy(&t), "General", "sLocalSavePath"))
    };
    let rel = rel.filter(|r| !r.trim().is_empty()).unwrap_or_else(|| "Saves\\".into());
    Some(rel.split(['\\', '/']).filter(|s| !s.is_empty()).fold(dir, |d, s| d.join(s)))
}

pub fn profile_ini_dir(inst: &Instance, profile: &str) -> PathBuf {
    inst.root.join("profiles").join(profile).join("ini")
}

fn copy(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to.parent().unwrap_or(Path::new(".")))?;
    fs::copy(from, to)?;
    Ok(())
}

fn load(inst: &Instance) -> Result<State> {
    match fs::read_to_string(inst.root.join(STATE)) {
        Ok(t) => Ok(serde_json::from_str(&t)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(e) => Err(e.into()),
    }
}

fn save(inst: &Instance, st: &State) -> Result<()> {
    let path = inst.root.join(STATE);
    if st.profile.is_none() && st.files.is_empty() && st.saves.is_none() && st.plugins_txt.is_none() && st.tweaks.is_empty() {
        if path.exists() {
            fs::remove_file(path)?;
        }
        return Ok(());
    }
    write_atomic(&path, &serde_json::to_string_pretty(st)?)
}
