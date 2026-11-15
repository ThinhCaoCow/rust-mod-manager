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
    /// Local and My Games folder name for this install: the store adds a suffix, recognised by a
    /// file only that store installs. Mirrors libloadorder's `appdata_folder_name`, which is
    /// private and only reachable through a constructor that also reads the game INIs.
    pub fn local_folder(self, game_path: &Path) -> String {
        let has = |f: &str| game_path.join(f).exists();
        let suffix = match self {
            Game::SkyrimSe if has("Galaxy64.dll") => " GOG",
            Game::Fallout4 if has("appxmanifest.xml") => " MS",
            _ if has("EOSSDK-Win64-Shipping.dll") => " EPIC",
            Game::SkyrimSe if has("appxmanifest.xml") => " MS",
            _ => "",
        };
        format!("{}{suffix}", self.local_dir())
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
        return Some(Detected { game, local_path: local_path(game, &p), game_path: p, source: "registry" });
    }
    steam_libraries().into_iter().find_map(|lib| {
        let p = lib.join("steamapps").join("common").join(game.steam_dir());
        p.join(game.exe()).is_file().then(|| Detected { game, local_path: local_path(game, &p), game_path: p, source: "steam" })
    })
}

/// Folder holding plugins.txt, named by [`Game::local_folder`].
/// - Windows: under `%LOCALAPPDATA%`.
/// - Linux: under the Proton prefix of the Steam library holding the game.
pub fn local_path(game: Game, game_path: &Path) -> Option<PathBuf> {
    let base = if cfg!(windows) {
        dirs::data_local_dir()?
    } else {
        // <lib>/steamapps/common/<game>
        let steamapps = game_path.parent()?.parent().filter(|p| p.file_name() == Some("steamapps".as_ref()))?;
        steamapps.join("compatdata").join(game.steam_app_id().to_string()).join("pfx/drive_c/users/steamuser/AppData/Local")
    };
    Some(base.join(game.local_folder(game_path)))
}

/// `Documents/My Games/<folder>`, where the game keeps its INIs. The folder name matches the
/// local folder's, store suffix included (libloadorder's `my_games_folder_name`).
pub fn my_games_dir(local_path: &Path) -> Option<PathBuf> {
    let name = local_path.file_name()?;
    let docs = if cfg!(windows) {
        dirs::document_dir()?
    } else {
        // .../users/steamuser/AppData/Local/<folder> -> .../users/steamuser/Documents
        local_path.ancestors().nth(3)?.join("Documents")
    };
    Some(docs.join("My Games").join(name))
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

    #[cfg(windows)]
    #[test]
    fn local_path_follows_the_store() {
        let tmp = std::env::temp_dir().join(format!("rmm-store-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let steam = local_path(Game::SkyrimSe, &tmp).unwrap();
        std::fs::write(tmp.join("Galaxy64.dll"), "").unwrap();
        let gog = local_path(Game::SkyrimSe, &tmp).unwrap();
        std::fs::remove_dir_all(&tmp).unwrap();
        assert!(steam.ends_with("Skyrim Special Edition"), "{steam:?}");
        assert!(gog.ends_with("Skyrim Special Edition GOG"), "{gog:?}");
        assert!(my_games_dir(&gog).unwrap().ends_with(r"My Games\Skyrim Special Edition GOG"));
    }

    #[cfg(not(windows))]
    #[test]
    fn proton_paths_come_from_the_library() {
        let local = local_path(Game::Fallout4, Path::new("/lib/steamapps/common/Fallout 4")).unwrap();
        assert_eq!(local, Path::new("/lib/steamapps/compatdata/377160/pfx/drive_c/users/steamuser/AppData/Local/Fallout4"));
        assert_eq!(
            my_games_dir(&local).unwrap(),
            Path::new("/lib/steamapps/compatdata/377160/pfx/drive_c/users/steamuser/Documents/My Games/Fallout4")
        );
        assert!(local_path(Game::Fallout4, Path::new("/opt/fallout4")).is_none());
    }

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
