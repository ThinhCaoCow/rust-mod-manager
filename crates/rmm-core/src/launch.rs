//! Starting the game (SPEC.md "Launch"): deploy, check that the script extender was built for
//! this game version, then start its loader (or the game) from the game folder, through Proton
//! on Linux.
use crate::deploy::{self, DeployPlan};
use crate::{Game, Instance, proton};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Serialize)]
pub struct LaunchReport {
    pub dry_run: bool,
    /// What starts: the script extender's loader when it is installed, otherwise the game.
    pub exe: PathBuf,
    pub game_version: String,
    /// The script extender's runtime DLL that matches the game version.
    pub script_extender: Option<String>,
    pub pid: Option<u32>,
    pub warnings: Vec<String>,
    /// What deploying now would do (dry run), or what the deploy before the launch did.
    pub deploy: DeployPlan,
}

/// Installed mods with the script extender's loader in root/ (the game folder).
pub fn script_extender_mods(inst: &Instance) -> Result<Vec<String>> {
    let loader = format!("root/{}", inst.config.game.loader()).to_lowercase();
    let has = |id: &str| inst.mod_files(id).is_ok_and(|f| f.iter().any(|f| f.to_lowercase() == loader));
    Ok(inst.profile()?.mods.into_iter().map(|m| m.id).filter(|id| !crate::instance::is_separator(id) && has(id)).collect())
}

/// Deploy and start the game. With `dry_run`, only the plan and the checks.
pub fn launch(inst: &Instance, dry_run: bool) -> Result<LaunchReport> {
    launch_with(inst, dry_run, &[])
}

/// `launch` with extra environment variables, which the script extender's loader passes on to
/// the game (the test harness reads its port and token from them).
pub fn launch_with(inst: &Instance, dry_run: bool, env: &[(&str, String)]) -> Result<LaunchReport> {
    let deploy = deploy::deploy(inst, dry_run)?; // refuses while the game runs
    let (root, game) = (&inst.config.game_path, inst.config.game);
    let game_exe = root.join(game.exe());
    let version = exe_version(&game_exe).with_context(|| format!("cannot read the version of {}", game_exe.display()))?;
    let mut r = LaunchReport {
        dry_run,
        exe: game_exe,
        game_version: version.map(|v| v.to_string()).join("."),
        script_extender: None,
        pid: None,
        warnings: vec![],
        deploy,
    };
    if dry_run && !r.deploy.is_noop() {
        r.warnings.push("the checks below see the game folder before this deploy".into());
    }
    r.warnings.extend(game_updated(inst, &r.game_version));
    let se = if game.base() == Game::SkyrimSe { "SKSE" } else { "F4SE" };
    r.warnings.extend(address_library(&inst.data_dir().join(se).join("Plugins"), game, &version));
    let cfg = crate::settings::get().launch;
    let loader = root.join(game.loader());
    if cfg.script_extender && loader.is_file() {
        r.script_extender = Some(se_runtime(root, game, &version)?);
        r.exe = loader;
    } else if has_se_plugins(inst, game) {
        r.warnings.push(if loader.is_file() {
            "the game starts without its script extender (Settings), so its plugins won't load".into()
        } else {
            format!("script extender plugins are deployed but {} isn't; they won't load", game.loader())
        });
    }
    if !dry_run {
        // Fire and forget: files the game writes into Data are captured by the next deploy.
        deploy::remember_unmanaged(inst)?;
        let mut cmd = if proton::needed(&r.exe) { proton::command(game, root, &r.exe)? } else { Command::new(&r.exe) };
        let child = cmd
            .args(crate::settings::split_args(&cfg.args))
            .envs(env.iter().map(|(k, v)| (*k, v)))
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("cannot start {}", r.exe.display()))?;
        r.pid = Some(child.id());
        // ponytail: a failed write only means the next game update goes unnoticed.
        let _ = fs::write(inst.root.join(PLAYED), &r.game_version);
    }
    Ok(r)
}

/// The game version at the last Play, to notice a game update.
const PLAYED: &str = "game-version.txt";

/// Says so when the game's version changed since the last Play (a Steam update, say).
pub(crate) fn game_updated(inst: &Instance, now: &str) -> Option<String> {
    let was = fs::read_to_string(inst.root.join(PLAYED)).ok()?;
    let was = was.trim();
    (!was.is_empty() && was != now).then(|| format!("the game was {was} at your last Play and is {now} now (a game update?)"))
}

/// When Address Library is installed but has no file for this game version, SKSE and F4SE plugins
/// that use it won't load. Its files name the version: `SKSE/Plugins/versionlib-1-6-1170-0.bin`
/// (`version-1-5-97-0.bin` before 1.6, `.csv` in VR), `F4SE/Plugins/version-1-10-163-0.bin`.
pub(crate) fn address_library(plugins: &Path, game: Game, version: &[u32; 4]) -> Option<String> {
    let mut have: Vec<[u32; 4]> = fs::read_dir(plugins)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_lowercase();
            let v = name.strip_prefix("versionlib-").or_else(|| name.strip_prefix("version-"))?;
            let v = v.strip_suffix(".bin").or_else(|| v.strip_suffix(".csv"))?;
            // "1-6-1170-0-1" is a second file for 1.6.1170.0.
            let parts: Vec<u32> = v.split('-').take(4).map(|p| p.parse().ok()).collect::<Option<_>>()?;
            parts.try_into().ok()
        })
        .collect();
    if have.is_empty() || have.contains(version) {
        return None;
    }
    have.sort();
    have.dedup();
    let dotted = |v: &[u32; 4]| v.map(|x| x.to_string()).join(".");
    let newest = have.iter().rev().take(3).map(dotted).collect::<Vec<_>>().join(", ");
    let (se, page) = match game {
        Game::SkyrimSe => ("SKSE", " (nexusmods.com/skyrimspecialedition/mods/32444)"),
        Game::Fallout4 => ("F4SE", " (nexusmods.com/fallout4/mods/47327)"),
        _ if game.base() == Game::SkyrimSe => ("SKSE", ""),
        _ => ("F4SE", ""),
    };
    let now = dotted(version);
    Some(format!(
        "Address Library has no file for game version {now} (its newest are for {newest}), so {se} plugins that use it won't load. Install the Address Library for {now}{page}, or wait for one if this game version is new"
    ))
}

/// The installed script extender's runtime DLL for this game version. The loader shows an error box
/// and quits on a mismatch; this says why, and what to do, before it gets that far.
pub(crate) fn se_runtime(root: &Path, game: Game, version: &[u32; 4]) -> Result<String> {
    let runtimes = runtimes(root, game)?;
    let site = if game.base() == Game::SkyrimSe { "skse.silverlock.org" } else { "f4se.silverlock.org" };
    let have = version.map(|x| x.to_string()).join(".");
    match runtimes.iter().find(|(_, v)| v[..] == version[..3]) {
        Some((name, _)) => Ok(name.clone()),
        None if runtimes.is_empty() => bail!("{} is installed without its runtime DLL; reinstall the script extender from {site}", game.loader()),
        None => bail!(
            "the script extender is built for game version {}, the game is {have} (a game update?); install the build for {have} from {site}, or wait for one if it isn't out yet. Mods that need the script extender won't work until then",
            runtimes.iter().map(|(_, v)| v.map(|x| x.to_string()).join(".")).collect::<Vec<_>>().join(", "),
        ),
    }
}

/// The game exe's version (cached by the exe's size and time: it takes reading the whole file).
pub(crate) fn game_version(root: &Path, game: Game) -> Result<[u32; 4]> {
    type Seen = (PathBuf, u64, std::time::SystemTime, [u32; 4]);
    static SEEN: std::sync::Mutex<Option<Seen>> = std::sync::Mutex::new(None);
    let exe = root.join(game.exe());
    let m = fs::metadata(&exe)?;
    let key = (exe.clone(), m.len(), m.modified()?);
    if let Some((p, n, t, v)) = SEEN.lock().unwrap().clone()
        && (p, n, t) == key
    {
        return Ok(v);
    }
    let v = exe_version(&exe)?;
    *SEEN.lock().unwrap() = Some((key.0, key.1, key.2, v));
    Ok(v)
}

/// The file version in an exe's VS_FIXEDFILEINFO: the signature 0xFEEF04BD, struct version 1.0,
/// then the version as two u32s. The resource section is at the end, so search backwards.
fn exe_version(exe: &Path) -> Result<[u32; 4]> {
    let b = fs::read(exe)?;
    let at = b.windows(8).rposition(|w| w == [0xBD, 0x04, 0xEF, 0xFE, 0, 0, 1, 0]).context("no version resource")?;
    let ms = u32::from_le_bytes(b.get(at + 8..at + 12).context("truncated version resource")?.try_into()?);
    let ls = u32::from_le_bytes(b.get(at + 12..at + 16).context("truncated version resource")?.try_into()?);
    Ok([ms >> 16, ms & 0xffff, ls >> 16, ls & 0xffff])
}

/// Runtime DLLs in the game folder and the game version each was built for:
/// `skse64_1_6_1170.dll` -> 1.6.1170. The steam loader DLL doesn't parse and is skipped.
fn runtimes(root: &Path, game: Game) -> Result<Vec<(String, [u32; 3])>> {
    let loader = game.loader();
    let prefix = &loader[..=loader.find('_').unwrap()];
    let mut out = Vec::new();
    for e in fs::read_dir(root)? {
        let name = e?.file_name().to_string_lossy().into_owned();
        let lower = name.to_lowercase();
        let Some(v) = lower.strip_prefix(prefix).and_then(|s| s.strip_suffix(".dll")) else { continue };
        let parts: Option<Vec<u32>> = v.split('_').map(|p| p.parse().ok()).collect();
        if let Some(Ok(v)) = parts.map(|p| <[u32; 3]>::try_from(p.as_slice())) {
            out.push((name, v));
        }
    }
    Ok(out)
}

fn has_se_plugins(inst: &Instance, game: Game) -> bool {
    let dir = inst.data_dir().join(if game.base() == Game::SkyrimSe { "SKSE" } else { "F4SE" }).join("Plugins");
    fs::read_dir(dir).is_ok_and(|rd| rd.filter_map(|e| e.ok()).any(|e| e.file_name().to_string_lossy().to_lowercase().ends_with(".dll")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_versions_and_runtime_names() {
        let dir = tempfile::tempdir().unwrap();
        let mut exe = b"MZ junk".to_vec();
        exe.extend([0xBD, 0x04, 0xEF, 0xFE, 0, 0, 1, 0]);
        exe.extend(((1u32 << 16) | 6).to_le_bytes());
        exe.extend((1170u32 << 16).to_le_bytes());
        fs::write(dir.path().join("SkyrimSE.exe"), exe).unwrap();
        assert_eq!(exe_version(&dir.path().join("SkyrimSE.exe")).unwrap(), [1, 6, 1170, 0]);

        for f in ["skse64_1_6_1170.dll", "skse64_steam_loader.dll", "skse64_1_6.dll", "f4se_1_10_163.dll"] {
            fs::write(dir.path().join(f), "").unwrap();
        }
        assert_eq!(runtimes(dir.path(), Game::SkyrimSe).unwrap(), vec![("skse64_1_6_1170.dll".to_string(), [1, 6, 1170])]);
        assert_eq!(runtimes(dir.path(), Game::Fallout4).unwrap(), vec![("f4se_1_10_163.dll".to_string(), [1, 10, 163])]);
        assert_eq!(game_version(dir.path(), Game::SkyrimSe).unwrap(), [1, 6, 1170, 0]);
        assert_eq!(se_runtime(dir.path(), Game::SkyrimSe, &[1, 6, 1170, 0]).unwrap(), "skse64_1_6_1170.dll");
        let e = se_runtime(dir.path(), Game::SkyrimSe, &[1, 6, 1179, 0]).unwrap_err().to_string();
        assert!(e.contains("built for game version 1.6.1170, the game is 1.6.1179") && e.contains("skse.silverlock.org"), "{e}");
    }

    #[test]
    fn finds_address_library_for_the_game_version() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        assert_eq!(address_library(p, Game::SkyrimSe, &[1, 6, 1170, 0]), None, "no Address Library, nothing to say");
        for f in ["version-1-5-97-0.bin", "versionlib-1-6-1130-0.bin", "versionlib-1-6-1170-0-1.bin", "AddressLibrary.dll", "version-notes.txt"] {
            fs::write(p.join(f), "").unwrap();
        }
        assert_eq!(address_library(p, Game::SkyrimSe, &[1, 6, 1170, 0]), None);
        assert_eq!(address_library(p, Game::SkyrimSe, &[1, 5, 97, 0]), None);
        let m = address_library(p, Game::SkyrimSe, &[1, 6, 1179, 0]).unwrap();
        assert!(m.starts_with("Address Library has no file for game version 1.6.1179.0 (its newest are for 1.6.1170.0, 1.6.1130.0, 1.5.97.0)"), "{m}");
        assert!(m.contains("SKSE plugins") && m.contains("mods/32444"), "{m}");
        fs::write(p.join("version-1-11-240-0.bin"), "").unwrap();
        assert!(address_library(p, Game::Fallout4, &[1, 11, 191, 0]).unwrap().contains("F4SE plugins that use it won't load. Install the Address Library for 1.11.191.0 (nexusmods.com/fallout4/mods/47327)"));
    }
}
