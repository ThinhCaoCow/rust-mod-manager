use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::str::FromStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Game {
    SkyrimSe,
    Fallout4,
    /// A game an add-on defines (`[[games]]` in addon.toml), following a built-in game's rules.
    Addon(&'static GameDef),
}

/// A game an add-on adds: a relative of a built-in game (Skyrim VR, Fallout 4 VR, Enderal SE)
/// that follows that game's rules for archives, plugins, the script extender and LOOT, with its
/// own executable, folders and ids.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, schemars::JsonSchema)]
pub struct GameDef {
    /// The name instances use (`game = "skyrimvr"`); Vortex's id for the game where it has one,
    /// so importing a Vortex setup finds it.
    pub id: String,
    pub name: String,
    /// The built-in game whose rules it follows: skyrim-se or fallout4.
    pub base: String,
    /// The plugin and load order rules (libloadorder's game): the base's, or SkyrimVR for a
    /// skyrim-se base, Fallout4VR for a fallout4 base.
    #[serde(default)]
    pub loadorder: Option<String>,
    pub exe: String,
    /// The script extender's loader.
    pub loader: String,
    pub steam_app_id: u32,
    /// Folder under `steamapps/common`.
    pub steam_dir: String,
    /// Folder under `%LOCALAPPDATA%` and `My Games`, and the Bethesda registry key.
    pub local_dir: String,
    /// The main INI in My Games.
    pub ini: String,
    pub nexus_domain: String,
}

static ADDON_GAMES: std::sync::Mutex<Vec<&'static GameDef>> = std::sync::Mutex::new(Vec::new());

/// Make an add-on's game known to this process (definitions live as long as it does). A
/// definition already known by that id is kept.
pub fn register(def: GameDef) -> Result<Game, String> {
    if builtin(&def.id).is_some() || def.id.trim().is_empty() {
        return Err(format!("an add-on game can't be called '{}'", def.id));
    }
    let base = builtin(&def.base).ok_or_else(|| format!("game '{}': base is skyrim-se or fallout4, not '{}'", def.id, def.base))?;
    let allowed: &[&str] = if base == Game::SkyrimSe { &["SkyrimSE", "SkyrimVR"] } else { &["Fallout4", "Fallout4VR"] };
    if let Some(lo) = def.loadorder.as_deref().filter(|lo| !allowed.contains(lo)) {
        return Err(format!("game '{}': loadorder for a {} base is {}, not '{lo}'", def.id, def.base, allowed.join(" or ")));
    }
    let mut games = ADDON_GAMES.lock().unwrap();
    if let Some(known) = games.iter().find(|g| g.id.eq_ignore_ascii_case(&def.id)) {
        return Ok(Game::Addon(known));
    }
    let def: &'static GameDef = Box::leak(Box::new(def));
    games.push(def);
    Ok(Game::Addon(def))
}

fn builtin(s: &str) -> Option<Game> {
    match s.to_lowercase().replace(['-', '_', ' '], "").as_str() {
        "skyrimse" | "sse" | "skyrimspecialedition" | "skyrimae" => Some(Game::SkyrimSe),
        "fallout4" | "fo4" => Some(Game::Fallout4),
        _ => None,
    }
}

impl Serialize for Game {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.id())
    }
}

impl<'de> Deserialize<'de> for Game {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?.parse().map_err(serde::de::Error::custom)
    }
}

impl schemars::JsonSchema for Game {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Game".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({ "type": "string", "description": "skyrim-se, fallout4, or a game an installed add-on defines" })
    }
}

impl Game {
    pub const ALL: [Game; 2] = [Game::SkyrimSe, Game::Fallout4];

    /// The name instance.toml and the CLI use.
    pub fn id(self) -> &'static str {
        match self {
            Game::SkyrimSe => "skyrim-se",
            Game::Fallout4 => "fallout4",
            Game::Addon(d) => &d.id,
        }
    }

    /// The built-in game whose rules this one follows (itself for the built-in games).
    pub fn base(self) -> Game {
        match self {
            Game::Addon(d) => builtin(&d.base).unwrap_or(Game::SkyrimSe),
            g => g,
        }
    }

    /// The game's id in Vortex.
    pub fn vortex_id(self) -> &'static str {
        match self {
            Game::SkyrimSe => "skyrimse",
            Game::Fallout4 => "fallout4",
            Game::Addon(d) => &d.id,
        }
    }

    /// The game's name in Nexus Mods URLs and API paths.
    pub fn nexus_domain(self) -> &'static str {
        match self {
            Game::SkyrimSe => "skyrimspecialedition",
            Game::Fallout4 => "fallout4",
            Game::Addon(d) => &d.nexus_domain,
        }
    }

    pub fn steam_app_id(self) -> u32 {
        match self {
            Game::SkyrimSe => 489830,
            Game::Fallout4 => 377160,
            Game::Addon(d) => d.steam_app_id,
        }
    }
    pub fn steam_dir(self) -> &'static str {
        match self {
            Game::SkyrimSe => "Skyrim Special Edition",
            Game::Fallout4 => "Fallout 4",
            Game::Addon(d) => &d.steam_dir,
        }
    }
    /// Registry key under `Bethesda Softworks`, also the `%LOCALAPPDATA%` folder name.
    pub fn local_dir(self) -> &'static str {
        match self {
            Game::SkyrimSe => "Skyrim Special Edition",
            Game::Fallout4 => "Fallout4",
            Game::Addon(d) => &d.local_dir,
        }
    }
    /// Local and My Games folder name for this install: the store adds a suffix, recognised by a
    /// file only that store installs. Mirrors libloadorder's `appdata_folder_name`, which is
    /// private and only reachable through a constructor that also reads the game INIs.
    pub fn local_folder(self, game_path: &Path) -> String {
        if let Game::Addon(d) = self {
            return d.local_dir.clone();
        }
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
            Game::Addon(d) => &d.exe,
        }
    }
    /// The main INI in the My Games folder.
    pub fn ini_name(self) -> &'static str {
        match self {
            Game::SkyrimSe => "Skyrim.ini",
            Game::Fallout4 => "Fallout4.ini",
            Game::Addon(d) => &d.ini,
        }
    }
    pub fn loader(self) -> &'static str {
        match self {
            Game::SkyrimSe => "skse64_loader.exe",
            Game::Fallout4 => "f4se_loader.exe",
            Game::Addon(d) => &d.loader,
        }
    }
    pub fn lo_id(self) -> loadorder::GameId {
        match self {
            Game::SkyrimSe => loadorder::GameId::SkyrimSE,
            Game::Fallout4 => loadorder::GameId::Fallout4,
            Game::Addon(d) => match d.loadorder.as_deref() {
                Some("SkyrimVR") => loadorder::GameId::SkyrimVR,
                Some("Fallout4VR") => loadorder::GameId::Fallout4VR,
                _ => self.base().lo_id(),
            },
        }
    }
    /// The plugin format; the VR games use their base's.
    pub fn esp_id(self) -> esplugin::GameId {
        match self.base() {
            Game::Fallout4 => esplugin::GameId::Fallout4,
            _ => esplugin::GameId::SkyrimSE,
        }
    }
}

impl FromStr for Game {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        builtin(s)
            .or_else(|| ADDON_GAMES.lock().unwrap().iter().find(|d| d.id.eq_ignore_ascii_case(s)).map(|d| Game::Addon(d)))
            .ok_or_else(|| format!("unknown game '{s}' (use skyrim-se or fallout4, or an add-on that defines it)"))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Detected {
    pub game: Game,
    pub game_path: PathBuf,
    /// Folder holding plugins.txt (Windows %LOCALAPPDATA% or the Proton prefix equivalent).
    pub local_path: Option<PathBuf>,
    pub source: &'static str,
    /// The exe's version ("1.6.1170.0"), to tell copies apart.
    pub version: Option<String>,
    /// In a Steam library (under `steamapps`).
    pub steam: bool,
    /// Free space on its drive, in bytes.
    pub free: Option<u64>,
}

impl Detected {
    fn new(game: Game, game_path: PathBuf, source: &'static str) -> Detected {
        let version = crate::launch::game_version(&game_path, game).ok().map(|v| v.map(|n| n.to_string()).join("."));
        let steam = game_path.components().any(|c| c.as_os_str().eq_ignore_ascii_case("steamapps"));
        Detected { game, local_path: local_path(game, &game_path), free: free_space(&game_path), game_path, source, version, steam }
    }
}

/// Free space on the drive holding `path` (or its nearest folder that exists), in bytes.
pub fn free_space(path: &Path) -> Option<u64> {
    let dir = path.ancestors().find(|p| p.is_dir())?;
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
        let mut free = 0u64;
        let ok = unsafe { windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) };
        (ok != 0).then_some(free)
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        (unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0).then(|| st.f_bavail as u64 * st.f_frsize as u64)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = dir;
        None
    }
}

pub fn detect(game: Game) -> Option<Detected> {
    #[cfg(windows)]
    if let Some(p) = registry_install_path(game).filter(|p| p.join(game.exe()).is_file()) {
        return Some(Detected::new(game, p, "registry"));
    }
    steam_libraries().into_iter().find_map(|lib| {
        let p = lib.join("steamapps").join("common").join(game.steam_dir());
        p.join(game.exe()).is_file().then(|| Detected::new(game, p, "steam"))
    })
}

/// The game in a folder the user picked: the one whose exe is there. Picking its Data folder
/// counts as picking the game folder.
pub fn identify(dir: &Path) -> Option<Detected> {
    let dir = if dir.file_name().is_some_and(|n| n.eq_ignore_ascii_case("data")) { dir.parent()? } else { dir };
    Game::ALL.into_iter().find(|g| dir.join(g.exe()).is_file()).map(|game| Detected::new(game, dir.to_path_buf(), "chosen"))
}

/// What a search of every drive found.
#[derive(Debug, Default, Serialize)]
pub struct Search {
    /// Game folders: the game's exe beside a Data folder (source "search").
    pub games: Vec<Detected>,
    /// MO2 instances: folders holding ModOrganizer.ini.
    pub mo2: Vec<PathBuf>,
}

/// How many folders below a drive's root the search looks (Steam libraries put games 4 or 5 down).
const SEARCH_DEPTH: usize = 6;

/// Look through every local drive for game folders (copies outside Steam and other stores too)
/// and MO2 instances (portable ones too). Skips Windows, hidden and system folders and links,
/// and doesn't look inside what it finds.
pub fn search_drives() -> Search {
    let mut found = Search::default();
    for root in drives() {
        walk(&root, SEARCH_DEPTH, &mut found, true);
    }
    found
}

/// Fixed and removable drives (not network drives or discs).
#[cfg(windows)]
fn drives() -> Vec<PathBuf> {
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| format!("{}:\\", (b'A' + i) as char))
        .filter(|root| {
            let wide: Vec<u16> = root.encode_utf16().chain([0]).collect();
            matches!(unsafe { GetDriveTypeW(wide.as_ptr()) }, 2 | 3) // DRIVE_REMOVABLE, DRIVE_FIXED
        })
        .map(PathBuf::from)
        .collect()
}

#[cfg(not(windows))]
fn drives() -> Vec<PathBuf> {
    vec![PathBuf::from("/")]
}

fn walk(dir: &Path, depth: usize, found: &mut Search, top: bool) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let (mut files, mut subdirs) = (Vec::new(), Vec::new());
    for e in rd.flatten() {
        let Ok(t) = e.file_type() else { continue };
        let name = e.file_name().to_string_lossy().to_lowercase();
        if t.is_file() {
            files.push(name);
        } else if t.is_dir() && !skipped(&e, &name) {
            subdirs.push((name, e.path())); // links aren't directories here, so no loops
        }
    }
    let has = |f: &str| files.iter().any(|n| n.eq_ignore_ascii_case(f));
    let mut hit = false;
    for game in Game::ALL {
        if has(game.exe()) && subdirs.iter().any(|(n, _)| n == "data") {
            found.games.push(Detected::new(game, dir.to_path_buf(), "search"));
            hit = true;
        }
    }
    if has("ModOrganizer.ini") {
        found.mo2.push(dir.to_path_buf());
        hit = true;
    }
    if hit || depth == 0 {
        return;
    }
    for (_, p) in subdirs {
        if top {
            crate::progress::report(&format!("searching {}", p.display()), 0, 0);
        }
        walk(&p, depth - 1, found, false);
    }
}

fn skipped(e: &std::fs::DirEntry, name: &str) -> bool {
    if name.starts_with(['.', '$']) || name == "windows" || name == "node_modules" {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const HIDDEN_OR_SYSTEM: u32 = 0x2 | 0x4;
        if e.metadata().is_ok_and(|m| m.file_attributes() & HIDDEN_OR_SYSTEM != 0) {
            return true;
        }
    }
    #[cfg(not(windows))]
    let _ = e;
    false
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

    #[test]
    fn searches_folders_for_games_and_mo2() {
        let tmp = tempfile::tempdir().unwrap();
        let put = |rel: &str| {
            let p = tmp.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "x").unwrap();
        };
        put("Games/Skyrim Downgraded/skyrimse.exe");
        put("Games/Skyrim Downgraded/Data/Skyrim.esm");
        put("Games/Skyrim Downgraded/Data/Nested/SkyrimSE.exe"); // not looked at: inside a find
        put("Games/MO2/ModOrganizer.ini");
        put("Games/MO2/mods/Old/ModOrganizer.ini");
        put("Patcher/Fallout4.exe"); // no Data folder: not a game folder
        put(".hidden/Fallout4.exe");
        put(".hidden/Data/x");
        put("a/b/c/Fallout4.exe");
        put("a/b/c/Data/x");
        let mut found = Search::default();
        walk(tmp.path(), 3, &mut found, true);
        let rel = |p: &Path| p.strip_prefix(tmp.path()).unwrap().to_string_lossy().replace('\\', "/");
        let mut games: Vec<_> = found.games.iter().map(|d| (d.game, rel(&d.game_path))).collect();
        games.sort_by(|a, b| a.1.cmp(&b.1));
        assert_eq!(games, [(Game::SkyrimSe, "Games/Skyrim Downgraded".to_string()), (Game::Fallout4, "a/b/c".to_string())]);
        assert_eq!(found.mo2.iter().map(|p| rel(p)).collect::<Vec<_>>(), ["Games/MO2"]);
        let mut shallow = Search::default();
        walk(tmp.path(), 2, &mut shallow, true);
        assert_eq!(shallow.games.len(), 1, "a/b/c is 3 folders down");
    }

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
