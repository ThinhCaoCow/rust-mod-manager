//! INI tweaks per mod (MO2's INI Tweaks tab, Wrye Bash's INI Tweaks): `.ini` files in a mod's
//! `INI Tweaks` folder, each a few `[Section]` and `key=value` lines. The user turns them on mod
//! by mod (`ModMeta::ini_tweaks`). A real deploy writes the settings of the enabled mods' tweaks
//! into the game's INIs in My Games, and the next deploy or a purge puts the old values back
//! (`profile_ini`).
use crate::instance::{Instance, Profile};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// The folder in a mod (Data/INI Tweaks) that holds its tweaks.
pub const DIR: &str = "INI Tweaks";

#[derive(Serialize, Debug)]
pub struct Tweak {
    /// File name in the mod's INI Tweaks folder.
    pub file: String,
    pub enabled: bool,
    pub settings: Vec<Setting>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Setting {
    pub section: String,
    pub key: String,
    pub value: String,
}

fn dir(inst: &Instance, id: &str) -> Option<PathBuf> {
    let m = inst.mod_dir(id);
    fs::read_dir(&m).ok()?.flatten().find(|e| e.path().is_dir() && e.file_name().to_string_lossy().eq_ignore_ascii_case(DIR)).map(|e| e.path())
}

/// The mod's tweaks, by file name, with what each one sets.
pub fn list(inst: &Instance, id: &str) -> Result<Vec<Tweak>> {
    let on = inst.mod_meta(id).ini_tweaks;
    let Some(dir) = dir(inst, id) else { return Ok(vec![]) };
    let mut out: Vec<Tweak> = fs::read_dir(dir)?
        .flatten()
        .filter(|e| e.path().is_file() && e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("ini")))
        .map(|e| {
            let file = e.file_name().to_string_lossy().into_owned();
            let settings = parse(&String::from_utf8_lossy(&fs::read(e.path()).unwrap_or_default()));
            Tweak { enabled: on.iter().any(|f| f.eq_ignore_ascii_case(&file)), file, settings }
        })
        .collect();
    out.sort_by_key(|t| t.file.to_lowercase());
    Ok(out)
}

/// Turn one of the mod's tweaks on or off. It reaches the game's INIs at the next deploy.
pub fn set(inst: &Instance, id: &str, file: &str, on: bool) -> Result<()> {
    let Some(t) = list(inst, id)?.into_iter().find(|t| t.file.eq_ignore_ascii_case(file)) else {
        bail!("mod '{id}' has no INI tweak '{file}' (its {DIR} folder has the .ini files)");
    };
    inst.locked(|| {
        let mut meta = inst.mod_meta(id);
        meta.ini_tweaks.retain(|f| !f.eq_ignore_ascii_case(&t.file));
        if on {
            meta.ini_tweaks.push(t.file.clone());
            meta.ini_tweaks.sort_by_key(|f| f.to_lowercase());
        }
        inst.write_mod_meta(id, &meta)
    })
}

/// `[Section]` and `key=value` lines; comments (`;`, `#`) and lines outside a section are skipped.
pub(crate) fn parse(text: &str) -> Vec<Setting> {
    let mut section = None;
    let mut out = Vec::new();
    for line in text.lines().map(|l| l.trim_start_matches('\u{feff}').trim()) {
        if line.starts_with([';', '#']) || line.is_empty() {
            continue;
        }
        if let Some(s) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = Some(s.trim().to_string());
        } else if let (Some(section), Some((k, v))) = (&section, line.split_once('='))
            && !k.trim().is_empty()
        {
            out.push(Setting { section: section.clone(), key: k.trim().into(), value: v.trim().into() });
        }
    }
    out
}

/// The settings of the enabled mods' enabled tweaks, one per section and key: a mod lower in
/// the list wins, as with files.
pub(crate) fn wanted(inst: &Instance, p: &Profile) -> Vec<Setting> {
    let mut out: Vec<Setting> = Vec::new();
    for m in p.mods.iter().filter(|m| m.enabled) {
        for t in list(inst, &m.id).unwrap_or_default().into_iter().filter(|t| t.enabled) {
            for s in t.settings {
                out.retain(|o| !(o.section.eq_ignore_ascii_case(&s.section) && o.key.eq_ignore_ascii_case(&s.key)));
                out.push(s);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_sections_and_keys() {
        let s = super::parse("\u{feff}; grass off\nstray=1\n[Grass]\nbAllowCreateGrass = 0\n# note\n[ Display ]\nfShadowDistance=4000\n=x\n");
        let got: Vec<String> = s.iter().map(|s| format!("{}|{}|{}", s.section, s.key, s.value)).collect();
        assert_eq!(got, ["Grass|bAllowCreateGrass|0", "Display|fShadowDistance|4000"]);
    }
}
