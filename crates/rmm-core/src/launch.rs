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
    let cfg = crate::settings::get().launch;
    let loader = root.join(game.loader());
    if cfg.script_extender && loader.is_file() {
        // The loader shows an error box and quits on a mismatch; say it here instead.
        let runtimes = runtimes(root, game)?;
        match runtimes.iter().find(|(_, v)| v[..] == version[..3]) {
            Some((name, _)) => r.script_extender = Some(name.clone()),
            None if runtimes.is_empty() => bail!("{} is installed without its runtime DLL; reinstall the script extender", game.loader()),
            None => bail!(
                "the script extender is built for game version {}, the game is {}; install the build for {}",
                runtimes.iter().map(|(_, v)| v.map(|x| x.to_string()).join(".")).collect::<Vec<_>>().join(", "),
                r.game_version,
                r.game_version
            ),
        }
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
    }
    Ok(r)
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
    }
}
