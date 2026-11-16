//! Windows programs on Linux run the way Steam runs the game: through the Proton build that last
//! ran the game's prefix, in that prefix (`proton run`, which waits for the program to exit).
use crate::Game;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// `proton run <exe>` in the game's prefix. Arguments are added by the caller.
pub fn command(game: Game, game_path: &Path, exe: &Path) -> Result<Command> {
    let steamapps = game_path.parent().and_then(Path::parent).filter(|p| p.ends_with("steamapps")).context("the game isn't in a Steam library, so it has no Proton prefix")?;
    let compat = steamapps.join("compatdata").join(game.steam_app_id().to_string());
    let info = std::fs::read_to_string(compat.join("config_info")).with_context(|| format!("no Proton prefix in {}; start the game once from Steam", compat.display()))?;
    let (proton, steam) = parse_config_info(&info).context("can't tell which Proton build runs the game; start it once from Steam")?;
    let id = game.steam_app_id().to_string();
    let mut cmd = Command::new(proton.join("proton"));
    cmd.arg("run")
        .arg(exe)
        .env("STEAM_COMPAT_DATA_PATH", &compat)
        .env("STEAM_COMPAT_CLIENT_INSTALL_PATH", steam.unwrap_or_else(|| steamapps.parent().unwrap().to_path_buf()))
        .env("SteamAppId", &id)
        .env("SteamGameId", &id);
    Ok(cmd)
}

/// Proton writes `config_info` into the prefix's compatdata folder on every run: the prefix
/// version, then `<proton>/files/share/fonts/` (`dist/` before Proton 5.13), the lib folders (one
/// since Proton 11, two before), and the Steam client folder. Returns the Proton folder and the
/// Steam folder.
fn parse_config_info(info: &str) -> Option<(PathBuf, Option<PathBuf>)> {
    let lines: Vec<&str> = info.lines().collect();
    let inside = |l: &str| ["/files/", "/dist/"].iter().find_map(|m| l.find(m)).map(|i| PathBuf::from(&l[..i]));
    let proton = inside(lines.get(1)?)?;
    let steam = lines.iter().skip(2).find(|l| l.starts_with('/') && inside(l).is_none()).map(|l| PathBuf::from(l.trim_end_matches('/')));
    Some((proton, steam))
}

/// A Linux path as Wine sees it: drive Z: is `/`.
pub fn windows_path(p: &Path) -> String {
    format!("Z:{}", p.display().to_string().replace('/', "\\"))
}

/// Is this a Windows program that needs Proton here?
pub fn needed(exe: &Path) -> bool {
    cfg!(not(windows)) && exe.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_config_info() {
        let info = "9.0-201\n/home/u/.local/share/Steam/steamapps/common/Proton 9.0 (Beta)/files/share/fonts/\n/home/u/.local/share/Steam/steamapps/common/Proton 9.0 (Beta)/files/lib/\n/x/files/lib64/\n/home/u/.local/share/Steam/\n123\n";
        let (proton, steam) = parse_config_info(info).unwrap();
        assert_eq!(proton, Path::new("/home/u/.local/share/Steam/steamapps/common/Proton 9.0 (Beta)"));
        assert_eq!(steam.unwrap(), Path::new("/home/u/.local/share/Steam"));
        let p11 = "11.0-100\n/s/steamapps/common/Proton - Experimental/files/share/fonts/\n/s/steamapps/common/Proton - Experimental/files/lib/\n/s\n1794535548.0\n";
        assert_eq!(parse_config_info(p11).unwrap(), (PathBuf::from("/s/steamapps/common/Proton - Experimental"), Some(PathBuf::from("/s"))));
        let old = "3\n/opt/GE-Proton5-9/dist/share/fonts/\n";
        assert_eq!(parse_config_info(old).unwrap(), (PathBuf::from("/opt/GE-Proton5-9"), None));
        assert!(parse_config_info("9.0-201\n").is_none());
        assert_eq!(windows_path(Path::new("/home/u/Data")), r"Z:\home\u\Data");
    }
}
