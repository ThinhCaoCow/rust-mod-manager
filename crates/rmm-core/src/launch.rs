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

/// One of the mods nearly every setup starts with, and where this one has it.
#[derive(Debug, Serialize)]
pub struct Essential {
    /// script_extender, address_library or crash_logger.
    pub what: &'static str,
    pub name: &'static str,
    /// The enabled mod that has it, or "the game folder".
    pub have: Option<String>,
    /// Its Nexus mod id (none for a game an add-on defines).
    pub nexus_id: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct Essentials {
    pub items: Vec<Essential>,
    /// The game's version, to pick each file by (unknown when the exe can't be read).
    pub game_version: Option<String>,
    /// Whether Play (or a test run) has started the game from this instance.
    pub played: bool,
}

/// What a new setup needs before its first game, for the window's getting-started list: the
/// script extender, Address Library and a crash logger, each found in an enabled mod or the game
/// folder, and whether Play has started the game yet.
pub fn essentials(inst: &Instance) -> Result<Essentials> {
    let game = inst.config.game;
    let skyrim = game.base() == Game::SkyrimSe;
    let plugins = if skyrim { "skse/plugins/" } else { "f4se/plugins/" };
    let loader = format!("root/{}", game.loader().to_lowercase());
    let kind = |f: &str| essential_kind(game, f);
    let version = game_version(&inst.config.game_path, game).ok();
    let new_gen = version.is_some_and(|v| v >= [1, 10, 980, 0]);
    let list: [(&str, &str, u64); 3] = if skyrim {
        [("script_extender", "Skyrim Script Extender (SKSE64)", 30379), ("address_library", "Address Library for SKSE Plugins", 32444), ("crash_logger", "Crash Logger SSE AE VR", 59818)]
    } else {
        let buffout = if new_gen { ("crash_logger", "Buffout 4 NG", 64880) } else { ("crash_logger", "Buffout 4", 47359) };
        [("script_extender", "Fallout 4 Script Extender (F4SE)", 42147), ("address_library", "Address Library for F4SE Plugins", 47327), buffout]
    };
    let mut have: [Option<String>; 3] = Default::default();
    for m in inst.profile()?.mods.iter().filter(|m| m.enabled && !crate::instance::is_separator(&m.id)) {
        for f in inst.mod_files(&m.id).unwrap_or_default() {
            if let Some(k) = kind(&f.to_lowercase()) {
                have[k].get_or_insert_with(|| inst.mod_meta(&m.id).name);
            }
        }
    }
    // Put in by hand, or by another manager.
    let mut in_game: Vec<String> = fs::read_dir(inst.data_dir().join(if skyrim { "SKSE" } else { "F4SE" }).join("Plugins"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| format!("{plugins}{}", e.file_name().to_string_lossy().to_lowercase()))
        .collect();
    if inst.config.game_path.join(game.loader()).is_file() {
        in_game.push(loader.clone());
    }
    for f in in_game {
        if let Some(k) = kind(&f) {
            have[k].get_or_insert_with(|| "the game folder".into());
        }
    }
    let builtin = matches!(game, Game::SkyrimSe | Game::Fallout4);
    let items = list.into_iter().zip(have).map(|((what, name, id), have)| Essential { what, name, have, nexus_id: builtin.then_some(id) }).collect();
    Ok(Essentials { items, game_version: version.map(|v| v.map(|n| n.to_string()).join(".")), played: inst.root.join(PLAYED).is_file() })
}

/// Which essential a file (mod-relative, lowercase) is: 0 the script extender's loader, 1
/// Address Library, 2 a crash logger.
fn essential_kind(game: Game, f: &str) -> Option<usize> {
    if f == format!("root/{}", game.loader().to_lowercase()) {
        return Some(0);
    }
    let name = f.strip_prefix(if game.base() == Game::SkyrimSe { "skse/plugins/" } else { "f4se/plugins/" })?;
    if name.ends_with(".bin") && (name.starts_with("versionlib-") || name.starts_with("version-")) {
        Some(1)
    } else {
        ["crashlogger.dll", "trainwreck.dll", "buffout4.dll"].contains(&name).then_some(2)
    }
}

/// Installed mods with the script extender, Address Library or a crash logger: Find the culprit
/// keeps them on, as without them nothing else loads, or no crash log is written.
pub fn essential_mods(inst: &Instance) -> Result<Vec<String>> {
    let game = inst.config.game;
    let has = |id: &str| inst.mod_files(id).is_ok_and(|f| f.iter().any(|f| essential_kind(game, &f.to_lowercase()).is_some()));
    Ok(inst.profile()?.mods.into_iter().map(|m| m.id).filter(|id| !crate::instance::is_separator(id) && has(id)).collect())
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

/// Start the game without mods, as when a game update broke the script extender: purge the game
/// folder (the next Play deploys the mods again) and start the game's own exe.
pub fn launch_vanilla(inst: &Instance, dry_run: bool) -> Result<LaunchReport> {
    let deploy = deploy::purge(inst, dry_run)?; // refuses while the game runs
    let (root, game) = (&inst.config.game_path, inst.config.game);
    let exe = root.join(game.exe());
    let version = exe_version(&exe).with_context(|| format!("cannot read the version of {}", exe.display()))?;
    let mut r = LaunchReport { dry_run, exe, game_version: version.map(|v| v.to_string()).join("."), script_extender: None, pid: None, warnings: vec![], deploy };
    if !dry_run {
        let mut cmd = if proton::needed(&r.exe) { proton::command(game, root, &r.exe)? } else { Command::new(&r.exe) };
        let child = cmd
            .args(crate::settings::split_args(&crate::settings::get().launch.args))
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("cannot start {}", r.exe.display()))?;
        r.pid = Some(child.id());
        // Not remembered as a Play: the version check is about playing with the mods.
    }
    Ok(r)
}

/// A shortcut that starts a profile's game, as MO2's do.
#[derive(Serialize, Debug)]
pub struct Shortcut {
    pub dry_run: bool,
    pub path: PathBuf,
    /// What it runs: rmm's command line with `arguments`. Steam's "Add a Non-Steam Game" takes
    /// the same two as its target and launch options.
    pub target: PathBuf,
    pub arguments: String,
}

/// A Windows shortcut, `<game> - <profile>.lnk` on the Desktop (or in `dir`), that runs
/// `cli -i <instance> launch --profile <profile> --apply --pause`: switch to the profile, deploy
/// and start the game, keeping an error on screen until it's read.
pub fn shortcut(inst: &Instance, profile: &str, cli: &Path, dir: Option<&Path>, dry_run: bool) -> Result<Shortcut> {
    let profile = inst.find_profile(profile)?;
    let dir = match dir {
        Some(d) => d.to_path_buf(),
        None => dirs::desktop_dir().context("can't find the Desktop folder")?,
    };
    let r = Shortcut {
        dry_run,
        path: dir.join(format!("{} - {profile}.lnk", inst.config.game.local_dir())),
        target: cli.to_path_buf(),
        arguments: format!("-i \"{}\" launch --profile \"{profile}\" --apply --pause", std::path::absolute(&inst.root)?.display()),
    };
    if !dry_run {
        anyhow::ensure!(cfg!(windows), "shortcuts are Windows .lnk files");
        fs::create_dir_all(&dir)?;
        // WScript.Shell writes .lnk files; the values go in through the environment, so nothing
        // in them needs quoting.
        let script = "$s = (New-Object -ComObject WScript.Shell).CreateShortcut($env:RMM_LNK); $s.TargetPath = $env:RMM_LNK_TARGET; \
                      $s.Arguments = $env:RMM_LNK_ARGS; $s.WorkingDirectory = $env:RMM_LNK_DIR; $s.IconLocation = $env:RMM_LNK_ICON; $s.Save()";
        let out = crate::hidden("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .env("RMM_LNK", &r.path)
            .env("RMM_LNK_TARGET", cli)
            .env("RMM_LNK_ARGS", &r.arguments)
            .env("RMM_LNK_DIR", &inst.config.game_path)
            .env("RMM_LNK_ICON", inst.config.game_path.join(inst.config.game.exe()))
            .output()
            .context("cannot run PowerShell to write the shortcut")?;
        anyhow::ensure!(out.status.success() && r.path.is_file(), "couldn't write {}: {}", r.path.display(), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(r)
}

/// Steam's manifest for the game, `appmanifest_<app id>.acf` in the `steamapps` folder whose
/// `common` folder has the game. None for a game outside a Steam library.
pub fn app_manifest(game: Game, game_path: &Path) -> Option<PathBuf> {
    let steamapps = game_path.parent()?.parent()?;
    let p = steamapps.join(format!("appmanifest_{}.acf", game.steam_app_id()));
    p.is_file().then_some(p)
}

/// Whether Steam may update the game.
#[derive(Debug, Serialize)]
pub struct SteamUpdates {
    /// The game's Steam manifest; None when rmm can't find one (not a Steam install).
    pub manifest: Option<PathBuf>,
    /// The manifest is read-only, so Steam can't update the game.
    pub held: bool,
    /// The copy kept from before rmm first made it read-only.
    pub backup: Option<PathBuf>,
}

fn manifest_backup(inst: &Instance, manifest: &Path) -> PathBuf {
    inst.root.join("ini-backup").join(manifest.file_name().unwrap_or_default())
}

pub fn steam_updates(inst: &Instance) -> SteamUpdates {
    let manifest = app_manifest(inst.config.game, &inst.config.game_path);
    let held = manifest.as_ref().is_some_and(|m| fs::metadata(m).is_ok_and(|m| m.permissions().readonly()));
    let backup = manifest.as_ref().map(|m| manifest_backup(inst, m)).filter(|b| b.is_file());
    SteamUpdates { manifest, held, backup }
}

/// Hold Steam's updates for the game by making its manifest read-only (a game update breaks the
/// script extender and the mods that need it until they catch up), keeping a copy of it first;
/// `hold` false lets Steam update it again.
pub fn hold_steam_updates(inst: &Instance, hold: bool) -> Result<SteamUpdates> {
    let game = inst.config.game;
    let Some(manifest) = app_manifest(game, &inst.config.game_path) else {
        bail!(
            "there's no Steam manifest (appmanifest_{}.acf) beside the game's folder in steamapps, so this isn't a Steam install rmm can hold updates for",
            game.steam_app_id()
        );
    };
    if hold {
        let backup = manifest_backup(inst, &manifest);
        fs::create_dir_all(backup.parent().unwrap())?;
        fs::copy(&manifest, &backup).with_context(|| format!("cannot keep a copy of {}", manifest.display()))?;
    }
    let mut perm = fs::metadata(&manifest)?.permissions();
    #[cfg(unix)]
    std::os::unix::fs::PermissionsExt::set_mode(&mut perm, if hold { 0o444 } else { 0o644 });
    #[cfg(not(unix))]
    perm.set_readonly(hold);
    fs::set_permissions(&manifest, perm).with_context(|| format!("cannot change {}", manifest.display()))?;
    Ok(steam_updates(inst))
}

/// The game version at the last Play, to notice a game update.
pub(crate) const PLAYED: &str = "game-version.txt";

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
    let have = address_library_versions(plugins);
    if have.is_empty() || have.contains(version) {
        return None;
    }
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

/// The game versions Address Library has files for in a `Plugins` folder, oldest first.
pub(crate) fn address_library_versions(plugins: &Path) -> Vec<[u32; 4]> {
    let Ok(rd) = fs::read_dir(plugins) else { return vec![] };
    let mut have: Vec<[u32; 4]> = rd
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
    have.sort();
    have.dedup();
    have
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
pub fn game_version(root: &Path, game: Game) -> Result<[u32; 4]> {
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
pub(crate) fn exe_version(exe: &Path) -> Result<[u32; 4]> {
    let b = fs::read(exe)?;
    let at = b.windows(8).rposition(|w| w == [0xBD, 0x04, 0xEF, 0xFE, 0, 0, 1, 0]).context("no version resource")?;
    let ms = u32::from_le_bytes(b.get(at + 8..at + 12).context("truncated version resource")?.try_into()?);
    let ls = u32::from_le_bytes(b.get(at + 12..at + 16).context("truncated version resource")?.try_into()?);
    Ok([ms >> 16, ms & 0xffff, ls >> 16, ls & 0xffff])
}

/// Runtime DLLs in the game folder and the game version each was built for:
/// `skse64_1_6_1170.dll` -> 1.6.1170. The steam loader DLL doesn't parse and is skipped.
pub(crate) fn runtimes(root: &Path, game: Game) -> Result<Vec<(String, [u32; 3])>> {
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
