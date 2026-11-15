use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::str::FromStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Game {
    SkyrimSe,
    Fallout4,
}

impl Game {
    pub const ALL: [Game; 2] = [Game::SkyrimSe, Game::Fallout4];

    pub fn steam_app_id(self) -> u32 {
        match self {
            Game::SkyrimSe => 489830,
            Game::Fallout4 => 377160,
        }
    }
    pub fn steam_dir(self) -> &'static str {
        match self {
            Game::SkyrimSe => "Skyrim Special Edition",
            Game::Fallout4 => "Fallout 4",
        }
    }
    /// Registry key under `Bethesda Softworks`, also the `%LOCALAPPDATA%` folder name.
    pub fn local_dir(self) -> &'static str {
        match self {
            Game::SkyrimSe => "Skyrim Special Edition",
            Game::Fallout4 => "Fallout4",
        }
    }
    pub fn exe(self) -> &'static str {
        match self {
            Game::SkyrimSe => "SkyrimSE.exe",
            Game::Fallout4 => "Fallout4.exe",
        }
    }
    pub fn loader(self) -> &'static str {
        match self {
            Game::SkyrimSe => "skse64_loader.exe",
            Game::Fallout4 => "f4se_loader.exe",
        }
    }
    pub fn lo_id(self) -> loadorder::GameId {
        match self {
            Game::SkyrimSe => loadorder::GameId::SkyrimSE,
            Game::Fallout4 => loadorder::GameId::Fallout4,
        }
    }
    pub fn esp_id(self) -> esplugin::GameId {
        match self {
            Game::SkyrimSe => esplugin::GameId::SkyrimSE,
            Game::Fallout4 => esplugin::GameId::Fallout4,
        }
    }
}

impl FromStr for Game {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.to_lowercase().replace(['-', '_', ' '], "").as_str() {
            "skyrimse" | "sse" | "skyrimspecialedition" | "skyrimae" => Ok(Game::SkyrimSe),
            "fallout4" | "fo4" => Ok(Game::Fallout4),
            _ => Err(format!("unknown game '{s}' (use skyrim-se or fallout4)")),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Detected {
    pub game: Game,
    pub game_path: PathBuf,
    /// Folder holding plugins.txt (Windows %LOCALAPPDATA% or the Proton prefix equivalent).
    pub local_path: Option<PathBuf>,
    pub source: &'static str,
}

pub fn detect(game: Game) -> Option<Detected> {
    #[cfg(windows)]
    if let Some(p) = registry_install_path(game).filter(|p| p.join(game.exe()).is_file()) {
        return Some(Detected { game, game_path: p, local_path: local_path(game, None), source: "registry" });
    }
    steam_libraries().into_iter().find_map(|lib| {
        let p = lib.join("steamapps").join("common").join(game.steam_dir());
        p.join(game.exe()).is_file().then(|| Detected {
            game,
            local_path: local_path(game, Some(&lib)),
            game_path: p,
            source: "steam",
        })
    })
}

/// Windows: `%LOCALAPPDATA%\<game>`. Linux/Proton: inside the game's compatdata prefix.
pub fn local_path(game: Game, steam_lib: Option<&Path>) -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join(game.local_dir()))
    } else {
        steam_lib.map(|l| {
            l.join("steamapps/compatdata")
                .join(game.steam_app_id().to_string())
                .join("pfx/drive_c/users/steamuser/AppData/Local")
                .join(game.local_dir())
        })
    }
}

#[cfg(windows)]
fn registry_install_path(game: Game) -> Option<PathBuf> {
    use winreg::{RegKey, enums::HKEY_LOCAL_MACHINE};
    let key = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(format!(r"SOFTWARE\WOW6432Node\Bethesda Softworks\{}", game.local_dir()))
        .ok()?;
    key.get_value::<String, _>("Installed Path").ok().map(PathBuf::from)
}

pub fn steam_libraries() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    #[cfg(windows)]
    {
        use winreg::{RegKey, enums::HKEY_CURRENT_USER};
        if let Ok(k) = RegKey::predef(HKEY_CURRENT_USER).open_subkey(r"Software\Valve\Steam")
            && let Ok(p) = k.get_value::<String, _>("SteamPath") {
                roots.push(PathBuf::from(p));
            }
    }
    #[cfg(not(windows))]
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for rel in [".steam/steam", ".local/share/Steam", ".var/app/com.valvesoftware.Steam/.local/share/Steam"] {
            roots.push(home.join(rel));
        }
    }
    let mut libs = Vec::new();
    for root in roots {
        if let Ok(vdf) = std::fs::read_to_string(root.join("steamapps/libraryfolders.vdf")) {
            libs.extend(parse_library_paths(&vdf));
        }
        libs.push(root);
    }
    libs.dedup();
    libs
}

/// Pulls every `"path" "..."` value out of Steam's libraryfolders.vdf.
pub fn parse_library_paths(vdf: &str) -> Vec<PathBuf> {
    vdf.lines()
        .filter_map(|l| l.trim().strip_prefix("\"path\""))
        .map(|v| PathBuf::from(v.trim().trim_matches('"').replace("\\\\", "\\")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_library_folders() {
        let vdf = "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"C:\\\\Program Files (x86)\\\\Steam\"\n\t}\n\t\"1\"\n\t{\n\t\t\"path\"\t\t\"D:\\\\Games\\\\Steam\"\n\t}\n}";
        assert_eq!(
            parse_library_paths(vdf),
            vec![PathBuf::from(r"C:\Program Files (x86)\Steam"), PathBuf::from(r"D:\Games\Steam")]
        );
    }

    #[test]
    fn game_names_parse() {
        assert_eq!("Skyrim-SE".parse::<Game>(), Ok(Game::SkyrimSe));
        assert_eq!("fo4".parse::<Game>(), Ok(Game::Fallout4));
        assert!("oblivion".parse::<Game>().is_err());
    }
}
