//! The game's saves for the active profile: who, where, and the plugins each one was made with
//! that the profile doesn't load now (MO2's Saves tab). Only the start of each file is read.
use crate::install::is_plugin;
use crate::instance::{Instance, InstanceConfig, PluginEntry, Profile};
use crate::{Game, plugins};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize)]
pub struct SaveList {
    /// The folder the game saves in for this profile.
    pub dir: Option<PathBuf>,
    /// Newest first.
    pub saves: Vec<Save>,
    /// Script extender co-saves whose save is gone (deleted in the game, or by hand).
    pub orphans: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Save {
    pub file: String,
    /// Unix seconds.
    pub modified: u64,
    pub size: u64,
    pub number: u32,
    pub player: String,
    pub level: u32,
    pub location: String,
    /// Plugins the save was made with (full and light).
    pub plugins: usize,
    /// Of those, the ones the profile doesn't load now.
    pub missing: Vec<Missing>,
    /// Why the file couldn't be read; the other fields are empty then.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Missing {
    pub plugin: String,
    /// The installed mod that has it (disabled, or its plugin is), if any.
    pub mod_id: Option<String>,
}

/// What a save's header and plugin lists say.
#[derive(Debug, Default, PartialEq)]
struct Header {
    number: u32,
    player: String,
    level: u32,
    location: String,
    plugins: Vec<String>,
}

pub fn list(inst: &Instance) -> Result<SaveList> {
    let p = inst.profile()?;
    let Some(dir) = crate::profile_ini::save_dir(inst, &p) else { return Ok(SaveList { dir: None, saves: vec![], orphans: vec![] }) };
    let ext = ext(inst.config.game);
    let need = Needs::new(inst, &p)?;
    let mut saves = Vec::new();
    for e in fs::read_dir(&dir).into_iter().flatten().flatten() {
        let path = e.path();
        if !path.extension().is_some_and(|x| x.eq_ignore_ascii_case(ext)) {
            continue;
        }
        let m = e.metadata()?;
        let mut s = Save {
            file: e.file_name().to_string_lossy().into_owned(),
            modified: m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs()),
            size: m.len(),
            number: 0,
            player: String::new(),
            level: 0,
            location: String::new(),
            plugins: 0,
            missing: vec![],
            error: None,
        };
        match read(&path) {
            Ok(h) => {
                s.missing = need.missing(&h.plugins);
                (s.number, s.player, s.level, s.location, s.plugins) = (h.number, h.player, h.level, h.location, h.plugins.len());
            }
            Err(e) => s.error = Some(format!("{e:#}")),
        }
        saves.push(s);
    }
    saves.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.file.cmp(&b.file)));
    let orphans = orphans_in(&dir, inst.config.game);
    Ok(SaveList { dir: Some(dir), saves, orphans })
}

/// Co-saves in `dir` without their save.
fn orphans_in(dir: &Path, game: Game) -> Vec<String> {
    let (ext, co) = (ext(game), cosave(game));
    let mut out: Vec<String> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x.eq_ignore_ascii_case(co)) && !p.with_extension(ext).is_file())
        .filter_map(|p| Some(p.file_name()?.to_string_lossy().into_owned()))
        .collect();
    out.sort();
    out
}

/// Move the active profile's orphaned co-saves (`SaveList::orphans`) into the instance's trash
/// (trash/co-saves/<profile>/), where emptying the trash deletes them. Returns them.
pub fn clean_cosaves(inst: &Instance, dry_run: bool) -> Result<Vec<String>> {
    let dir = crate::profile_ini::save_dir(inst, &inst.profile()?).context("cannot work out the save folder")?;
    let orphans = orphans_in(&dir, inst.config.game);
    if dry_run || orphans.is_empty() {
        return Ok(orphans);
    }
    to_trash(inst, &dir, &orphans, "co-saves")?;
    Ok(orphans)
}

/// Move saves of the active profile, each with its co-save, into the instance's trash
/// (trash/saves/<profile>/), where emptying the trash deletes them. Returns the files moved.
pub fn delete(inst: &Instance, files: &[String]) -> Result<Vec<String>> {
    let dir = crate::profile_ini::save_dir(inst, &inst.profile()?).context("cannot work out the save folder")?;
    let names = with_cosaves(inst.config.game, &dir, files)?;
    to_trash(inst, &dir, &names, "saves")?;
    Ok(names)
}

/// The saves named, checked, each followed by its co-save when it has one.
fn with_cosaves(game: Game, dir: &Path, files: &[String]) -> Result<Vec<String>> {
    if files.is_empty() {
        bail!("no saves given");
    }
    let mut names: Vec<String> = Vec::new();
    for f in files {
        if f.contains(['/', '\\']) || !Path::new(f).extension().is_some_and(|x| x.eq_ignore_ascii_case(ext(game))) {
            bail!("not a save file name: {f}");
        }
        if !dir.join(f).is_file() {
            bail!("no save '{f}' in {}", dir.display());
        }
        let c = Path::new(f).with_extension(cosave(game)).to_string_lossy().into_owned();
        for f in [f.clone(), c] {
            if dir.join(&f).is_file() && !names.contains(&f) {
                names.push(f);
            }
        }
    }
    Ok(names)
}

/// Move `names` from `dir` into trash/<kind>/<profile>/, renaming any whose name is taken there.
fn to_trash(inst: &Instance, dir: &Path, names: &[String], kind: &str) -> Result<()> {
    let to = inst.root.join("trash").join(kind).join(&inst.config.profile);
    fs::create_dir_all(&to)?;
    for f in names {
        let dest = (0..).map(|n| if n == 0 { to.join(f) } else { to.join(format!("{f}.{n}")) }).find(|p| !p.exists()).expect("a free name");
        // My Games is often on another drive than the instance: copy, then delete.
        if fs::rename(dir.join(f), &dest).is_err() {
            fs::copy(dir.join(f), &dest)?;
            fs::remove_file(dir.join(f))?;
        }
    }
    Ok(())
}

/// The newest save, with those of `plugins` it was made with; None when it uses none of them.
pub fn newest_using(inst: &Instance, plugins: &[String]) -> Result<Option<(String, Vec<String>)>> {
    let Some(dir) = crate::profile_ini::save_dir(inst, &inst.profile()?) else { return Ok(None) };
    let ext = ext(inst.config.game);
    let newest = fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case(ext)))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e)))
        .max_by_key(|(t, _)| *t);
    let Some((_, e)) = newest else { return Ok(None) };
    let Ok(h) = read(&e.path()) else { return Ok(None) };
    let used: Vec<String> = h.plugins.into_iter().filter(|n| plugins.iter().any(|p| p.eq_ignore_ascii_case(n))).collect();
    Ok((!used.is_empty()).then(|| (e.file_name().to_string_lossy().into_owned(), used)))
}

/// Enable the installed mods and plugins a save needs, as one change. Returns the plugins enabled.
pub fn enable_missing(inst: &Instance, file: &str) -> Result<Vec<String>> {
    if file.contains(['/', '\\']) {
        bail!("not a save file name: {file}");
    }
    let p = inst.profile()?;
    let dir = crate::profile_ini::save_dir(inst, &p).context("cannot work out the save folder")?;
    let fix: Vec<(String, String)> = Needs::new(inst, &p)?.missing(&read(&dir.join(file))?.plugins).into_iter().filter_map(|m| Some((m.mod_id?, m.plugin))).collect();
    if fix.is_empty() {
        return Ok(vec![]);
    }
    inst.edit(&format!("enable what {file} needs"), |p| {
        for (id, plugin) in &fix {
            if let Some(m) = p.mods.iter_mut().find(|m| m.id == *id) {
                m.enabled = true;
            }
            match p.plugins.iter_mut().find(|e| e.name.eq_ignore_ascii_case(plugin)) {
                Some(e) => e.enabled = true,
                None => p.plugins.push(PluginEntry { name: plugin.clone(), enabled: true }),
            }
        }
        Ok(fix.into_iter().map(|(_, plugin)| plugin).collect())
    })
}

#[derive(Debug, Serialize)]
pub struct Transfer {
    /// The other profile's save folder.
    pub to: PathBuf,
    /// The files copied or moved: each save, and its script extender co-save.
    pub files: Vec<String>,
}

/// Copy, or move (`keep` false), saves of the active profile to `profile`'s save folder, each
/// with its co-save. A file of that name already there stops it before anything is done.
pub fn transfer(inst: &Instance, files: &[String], profile: &str, keep: bool) -> Result<Transfer> {
    let name = inst.find_profile(profile)?;
    let cur = &inst.config.profile;
    if name == *cur {
        bail!("'{name}' is the active profile");
    }
    let (from, to) = (save_dir_of(inst, cur)?, save_dir_of(inst, &name)?);
    if from.to_string_lossy().to_lowercase() == to.to_string_lossy().to_lowercase() {
        bail!("{cur} and {name} share one save folder; turn on Own saves for one of them");
    }
    let names = with_cosaves(inst.config.game, &from, files)?;
    if let Some(f) = names.iter().find(|f| to.join(f).exists()) {
        bail!("{name}'s save folder already has {f}");
    }
    fs::create_dir_all(&to)?;
    for f in &names {
        if keep {
            fs::copy(from.join(f), to.join(f))?;
        } else {
            fs::rename(from.join(f), to.join(f))?; // ponytail: both are in My Games; copy and delete if a profile's folder can be elsewhere
        }
    }
    Ok(Transfer { to, files: names })
}

/// The folder `profile`'s saves go to once it is deployed.
fn save_dir_of(inst: &Instance, profile: &str) -> Result<PathBuf> {
    let at = Instance { root: inst.root.clone(), config: InstanceConfig { profile: profile.into(), ..inst.config.clone() } };
    crate::profile_ini::save_dir(&at, &at.profile()?).context("the game's My Games folder isn't known for this instance")
}

/// The profile's loaded plugins, and which mod has each plugin it could load.
struct Needs {
    loaded: HashSet<String>,
    owner: HashMap<String, String>,
}

impl Needs {
    fn new(inst: &Instance, p: &Profile) -> Result<Self> {
        let loaded = plugins::list(inst, p)?.into_iter().filter(|x| x.enabled).map(|x| x.name.to_lowercase()).collect();
        let mut owner = HashMap::new();
        for m in p.mods.iter().filter(|m| !crate::instance::is_separator(&m.id)) {
            for f in inst.mod_files(&m.id)?.into_iter().filter(|f| !f.contains('/') && is_plugin(f)) {
                owner.insert(f.to_lowercase(), m.id.clone()); // the later mod wins, as in a deploy
            }
        }
        Ok(Needs { loaded, owner })
    }

    fn missing(&self, plugins: &[String]) -> Vec<Missing> {
        let lower = |s: &String| s.to_lowercase();
        plugins
            .iter()
            .filter(|n| !self.loaded.contains(&lower(n)))
            .map(|n| Missing { plugin: n.clone(), mod_id: self.owner.get(&lower(n)).cloned() })
            .collect()
    }
}

pub(crate) fn ext(game: Game) -> &'static str {
    match game.base() {
        Game::Fallout4 => "fos",
        _ => "ess",
    }
}

/// The script extender's co-save beside each save.
fn cosave(game: Game) -> &'static str {
    match game.base() {
        Game::Fallout4 => "f4se",
        _ => "skse",
    }
}

/// The header and plugin lists of a Skyrim SE (`.ess`) or Fallout 4 (`.fos`) save, reading the
/// start of the file only: saves run to 100 MB.
fn read(path: &Path) -> Result<Header> {
    let mut f = File::open(path)?;
    let mut head = vec![0; 4096];
    let n = f.read(&mut head)?;
    head.truncate(n);
    let fo4 = head.starts_with(b"FO4_SAVEGAME");
    if !fo4 && !head.starts_with(b"TESV_SAVEGAME") {
        bail!("not a Skyrim SE or Fallout 4 save");
    }
    let mut r = Bytes { b: &head, i: if fo4 { 12 } else { 13 } };
    let size = r.u32()? as u64;
    let start = r.i as u64;
    let version = r.u32()?;
    let number = r.u32()?;
    let player = r.text()?;
    let level = r.u32()?;
    let location = r.text()?;
    r.text()?; // game date or play time
    r.text()?; // race
    r.skip(2 + 4 + 4 + 8)?; // sex, experience, experience to the next level, file time
    let (w, h) = (r.u32()? as u64, r.u32()? as u64);
    let se = !fo4 && version >= 12;
    let compression = if se { r.u16()? } else { 0 };
    let pixels = if !fo4 && !se { 3 } else { 4 };
    f.seek(SeekFrom::Start(start + size + w * h * pixels))?;
    // The plugin lists come first in the body, well inside its first megabyte.
    const BODY: u64 = 1 << 20;
    let mut body = Vec::new();
    if se {
        let mut lens = [0; 8];
        f.read_exact(&mut lens)?;
        let packed = u32::from_le_bytes(lens[4..].try_into().unwrap()) as u64;
        f.take(packed.min(BODY)).read_to_end(&mut body)?;
        body = match compression {
            0 => body,
            2 => lz4(&body, BODY as usize),
            // ponytail: zlib (1) is from early SE versions; add flate2 if such saves turn up.
            c => bail!("save compression {c} isn't supported"),
        };
    } else {
        f.take(BODY).read_to_end(&mut body)?;
    }
    let mut r = Bytes { b: &body, i: 0 };
    let form = r.u8()?;
    if fo4 {
        r.text()?; // game version
    }
    r.u32()?; // size of the plugin list
    let mut plugins = Vec::new();
    for _ in 0..r.u8()? {
        plugins.push(r.text()?);
    }
    if (fo4 && form >= 68) || (!fo4 && form >= 78) {
        for _ in 0..r.u16()? {
            plugins.push(r.text()?);
        }
    }
    Ok(Header { number, player, level, location, plugins })
}

struct Bytes<'a> {
    b: &'a [u8],
    i: usize,
}

impl Bytes<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        let s = self.b.get(self.i..self.i + n).context("the save ends early")?;
        self.i += n;
        Ok(s)
    }
    fn skip(&mut self, n: usize) -> Result<()> {
        self.take(n).map(|_| ())
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into()?))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into()?))
    }
    /// A length-prefixed string: UTF-8 when it is, Windows-1252 (as Latin-1) otherwise.
    fn text(&mut self) -> Result<String> {
        let n = self.u16()? as usize;
        let b = self.take(n)?;
        Ok(String::from_utf8(b.to_vec()).unwrap_or_else(|_| b.iter().map(|&c| c as char).collect()))
    }
}

/// Decode an LZ4 block until `want` bytes are out or the input ends: a prefix is all `read` needs.
fn lz4(src: &[u8], want: usize) -> Vec<u8> {
    fn len(src: &[u8], i: &mut usize, mut n: usize) -> Option<usize> {
        if n == 15 {
            loop {
                let b = *src.get(*i)?;
                *i += 1;
                n += b as usize;
                if b != 255 {
                    break;
                }
            }
        }
        Some(n)
    }
    let mut out = Vec::with_capacity(want);
    let mut i = 0;
    while out.len() < want && i < src.len() {
        let token = src[i];
        i += 1;
        let Some(lit) = len(src, &mut i, (token >> 4) as usize) else { break };
        out.extend_from_slice(&src[i.min(src.len())..(i + lit).min(src.len())]);
        i += lit;
        let Some(off) = src.get(i..i + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize) else { break };
        i += 2;
        let Some(m) = len(src, &mut i, (token & 15) as usize) else { break };
        if off == 0 || off > out.len() {
            break; // corrupt
        }
        let from = out.len() - off;
        for k in 0..m + 4 {
            out.push(out[from + k]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Vec<u8> {
        [(s.len() as u16).to_le_bytes().to_vec(), s.as_bytes().to_vec()].concat()
    }

    /// A save with a 2x1 screenshot and these plugin lists.
    fn save(fo4: bool, body: Vec<u8>) -> Vec<u8> {
        let mut h = Vec::new();
        h.extend((if fo4 { 15u32 } else { 12 }).to_le_bytes());
        h.extend(7u32.to_le_bytes());
        h.extend(text("Prisoner"));
        h.extend(3u32.to_le_bytes());
        h.extend(text("Riverwood"));
        h.extend(text("000.01.02"));
        h.extend(text("NordRace"));
        h.extend([0; 2 + 4 + 4 + 8]);
        h.extend(2u32.to_le_bytes());
        h.extend(1u32.to_le_bytes());
        if !fo4 {
            h.extend(2u16.to_le_bytes()); // LZ4
        }
        let mut f = (if fo4 { &b"FO4_SAVEGAME"[..] } else { &b"TESV_SAVEGAME"[..] }).to_vec();
        f.extend((h.len() as u32).to_le_bytes());
        f.extend(h);
        f.extend([9; 8]); // the screenshot
        if fo4 {
            f.extend(body);
        } else {
            // One LZ4 sequence of literals only.
            let mut packed = vec![0xF0];
            let mut n = body.len() - 15;
            while n >= 255 {
                packed.push(255);
                n -= 255;
            }
            packed.push(n as u8);
            packed.extend(&body);
            f.extend((body.len() as u32).to_le_bytes());
            f.extend((packed.len() as u32).to_le_bytes());
            f.extend(packed);
        }
        f
    }

    fn body(fo4: bool, full: &[&str], light: &[&str]) -> Vec<u8> {
        let mut b = vec![if fo4 { 68 } else { 78 }];
        if fo4 {
            b.extend(text("1.10.163.0"));
        }
        b.extend(0u32.to_le_bytes());
        b.push(full.len() as u8);
        full.iter().for_each(|p| b.extend(text(p)));
        b.extend((light.len() as u16).to_le_bytes());
        light.iter().for_each(|p| b.extend(text(p)));
        b.extend([0; 300]); // the rest of the body
        b
    }

    #[test]
    fn lists_saves_and_enables_what_they_need() {
        let tmp = tempfile::tempdir().unwrap();
        let (game, src, ini) = (tmp.path().join("game"), tmp.path().join("src"), tmp.path().join("My Games"));
        for (path, text) in [(game.join("SkyrimSE.exe"), "exe"), (game.join("Data/Skyrim.esm"), "esm"), (src.join("A/A.esp"), "esp"), (ini.join("Skyrim.ini"), "")] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        let mut inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
        inst.config.ini_path = Some(ini.clone());
        let a = crate::install::install(&inst, &src.join("A"), None, false, None).unwrap().id;
        fs::create_dir_all(ini.join("Saves")).unwrap();
        fs::write(ini.join("Saves/x.ess"), save(false, body(false, &["Skyrim.esm", "A.esp", "Gone.esp"], &["Small.esl"]))).unwrap();
        fs::write(ini.join("Saves/x.skse"), "co-save").unwrap();

        let l = list(&inst).unwrap();
        assert_eq!((l.dir, l.saves.len()), (Some(ini.join("Saves")), 1));
        let missing = |l: SaveList| l.saves[0].missing.iter().map(|m| format!("{} {:?}", m.plugin, m.mod_id)).collect::<Vec<_>>();
        assert_eq!(missing(list(&inst).unwrap()), [format!("A.esp Some({a:?})"), "Gone.esp None".into(), "Small.esl None".into()]);
        assert_eq!(enable_missing(&inst, "x.ess").unwrap(), ["A.esp"]);
        assert!(inst.profile().unwrap().mods.iter().any(|m| m.id == a && m.enabled));
        assert_eq!(missing(list(&inst).unwrap()), ["Gone.esp None", "Small.esl None"]);
        assert!(enable_missing(&inst, "../x.ess").is_err());
        assert_eq!(newest_using(&inst, &["a.ESP".into(), "Other.esp".into()]).unwrap(), Some(("x.ess".into(), vec!["A.esp".to_string()])));
        assert_eq!(newest_using(&inst, &["Other.esp".into()]).unwrap(), None);

        // A co-save whose save is gone moves to the trash; one with its save stays.
        fs::write(ini.join("Saves/old.skse"), "co-save").unwrap();
        assert_eq!(list(&inst).unwrap().orphans, ["old.skse"]);
        assert_eq!(clean_cosaves(&inst, true).unwrap(), ["old.skse"]);
        assert!(ini.join("Saves/old.skse").is_file(), "a dry run moves nothing");
        assert_eq!(clean_cosaves(&inst, false).unwrap(), ["old.skse"]);
        assert!(!ini.join("Saves/old.skse").exists() && ini.join("Saves/x.skse").is_file());
        assert!(inst.root.join("trash/co-saves/default/old.skse").is_file());
        assert!(list(&inst).unwrap().orphans.is_empty());

        // Deleting a save moves it and its co-save to the trash, beside an earlier one of that name.
        assert!(delete(&inst, &["../x.ess".into()]).is_err() && delete(&inst, &["nope.ess".into()]).is_err());
        fs::create_dir_all(inst.root.join("trash/saves/default")).unwrap();
        fs::write(inst.root.join("trash/saves/default/x.ess"), "an older x").unwrap();
        assert_eq!(delete(&inst, &["x.ess".into()]).unwrap(), ["x.ess", "x.skse"]);
        assert!(!ini.join("Saves/x.ess").exists() && !ini.join("Saves/x.skse").exists());
        assert!(inst.root.join("trash/saves/default/x.ess.1").is_file() && inst.root.join("trash/saves/default/x.skse").is_file());
        assert!(list(&inst).unwrap().saves.is_empty());

        // With its own saves, the profile has a folder of its own.
        crate::ops::set_profile_options(&inst, None, Some(true)).unwrap();
        let l = list(&inst).unwrap();
        assert_eq!((l.dir, l.saves.len()), (Some(ini.join("Saves/default")), 0));
    }

    #[test]
    fn reads_skyrim_and_fallout_saves() {
        let dir = tempfile::tempdir().unwrap();
        for fo4 in [false, true] {
            let path = dir.path().join("a.sav");
            fs::write(&path, save(fo4, body(fo4, &["Skyrim.esm", "Mod.esp"], &["Small.esl"]))).unwrap();
            let h = read(&path).unwrap();
            let want = Header { number: 7, player: "Prisoner".into(), level: 3, location: "Riverwood".into(), plugins: vec!["Skyrim.esm".into(), "Mod.esp".into(), "Small.esl".into()] };
            assert_eq!(h, want, "fo4: {fo4}");
        }
        fs::write(dir.path().join("b.sav"), b"TESV_SAVEGAME").unwrap();
        assert!(read(&dir.path().join("b.sav")).is_err());
    }

    #[test]
    fn lz4_copies_matches() {
        // "abc", then 9 bytes from 3 back, then the literal "!".
        let src = [0x35, b'a', b'b', b'c', 3, 0, 0x10, b'!'];
        assert_eq!(lz4(&src, 100), b"abcabcabcabc!");
        assert_eq!(lz4(&src, 4), b"abcabcabcabc"); // stops once it has enough
        assert_eq!(lz4(&src[..5], 100), b"abc"); // and where the input ends
    }
}
