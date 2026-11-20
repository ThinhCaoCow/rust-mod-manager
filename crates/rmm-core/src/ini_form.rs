//! Common game settings as a form (MO2's INI Configurator, BethINI's everyday part): each a
//! known key with a kind and a range. Values are read as the game sees them (`<game>Custom.ini`
//! over `<game>Prefs.ini` over `<game>.ini`) and written to the file that holds the key now, else
//! to Prefs, through the workspace's writes, so each change is in Edits and can be put back.
use crate::health::{ini_get, ini_set};
use crate::{Game, Instance, workspace};
use anyhow::{Context, Result, bail};
use serde::Serialize;

#[derive(Debug, Serialize, Clone, Copy)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Kind {
    Bool,
    Int { min: i64, max: i64 },
    Float { min: f64, max: f64, step: f64 },
    Choice { options: &'static [&'static str] },
}

#[derive(Debug, Serialize, Clone, Copy)]
pub struct Def {
    pub group: &'static str,
    pub section: &'static str,
    pub key: &'static str,
    pub label: &'static str,
    #[serde(flatten)]
    pub kind: Kind,
    /// The game's own value when no INI sets it.
    pub default: &'static str,
    pub help: &'static str,
    /// Only for Fallout 4 (`Some(true)`) or only for Skyrim SE (`Some(false)`).
    #[serde(skip)]
    pub fo4: Option<bool>,
}

#[allow(clippy::too_many_arguments)] // a row of the table below
const fn d(group: &'static str, section: &'static str, key: &'static str, label: &'static str, kind: Kind, default: &'static str, help: &'static str, fo4: Option<bool>) -> Def {
    Def { group, section, key, label, kind, default, help, fo4 }
}
const B: Kind = Kind::Bool;

pub const SETTINGS: &[Def] = &[
    d("Display", "Display", "iSize W", "Width", Kind::Int { min: 640, max: 7680 }, "1920", "The game's width in pixels.", None),
    d("Display", "Display", "iSize H", "Height", Kind::Int { min: 480, max: 4320 }, "1080", "The game's height in pixels.", None),
    d("Display", "Display", "bFull Screen", "Full screen", B, "1", "Off with Borderless on gives a borderless window.", None),
    d("Display", "Display", "bBorderless", "Borderless window", B, "1", "", None),
    d("Display", "Display", "iVSyncPresentInterval", "VSync", B, "1", "Off lets the frame rate go above 60, which breaks Skyrim's physics without a fix such as SSE Display Tweaks.", Some(false)),
    d("Display", "Display", "iPresentInterval", "VSync", B, "1", "Off lets the frame rate go above 60, which breaks Fallout 4's physics without a fix.", Some(true)),
    d("Display", "Display", "fGamma", "Gamma", Kind::Float { min: 0.5, max: 1.5, step: 0.05 }, "1.0000", "", Some(false)),
    d("Display", "Display", "iMaxAnisotropy", "Anisotropic filtering", Kind::Choice { options: &["0", "2", "4", "8", "16"] }, "16", "Sharper textures at an angle.", None),
    d("Display", "Display", "fShadowDistance", "Shadow distance", Kind::Float { min: 1000.0, max: 20000.0, step: 500.0 }, "8000.0000", "How far away shadows are drawn; an ENB may override it.", None),
    d("Display", "Display", "iShadowMapResolution", "Shadow resolution", Kind::Choice { options: &["1024", "2048", "4096", "8192"] }, "4096", "Sharper shadows; 8192 costs a lot.", None),
    d("Display", "Display", "bTreesReceiveShadows", "Trees receive shadows", B, "1", "", Some(false)),
    d("Interface", "Interface", "bShowCompass", "Compass", B, "1", "", None),
    d("Interface", "Interface", "bDialogueSubtitles", "Dialogue subtitles", B, "1", "", None),
    d("Interface", "Interface", "bGeneralSubtitles", "General subtitles", B, "0", "What people around you say.", None),
    d("Interface", "GamePlay", "bShowFloatingQuestMarkers", "Floating quest markers", B, "1", "", Some(false)),
    d("Interface", "GamePlay", "bShowQuestMarkers", "Quest markers", B, "1", "", Some(false)),
    d("Controls", "Controls", "bGamepadEnable", "Gamepad", B, "1", "", None),
    d("Controls", "Controls", "bInvertYValues", "Invert the Y axis", B, "0", "", None),
    d("Controls", "Controls", "fMouseHeadingSensitivity", "Look sensitivity", Kind::Float { min: 0.002, max: 0.05, step: 0.0005 }, "0.0125", "", None),
    d("Saves", "MAIN", "bSaveOnPause", "Save when pausing", B, "1", "", Some(false)),
    d("Saves", "MAIN", "bSaveOnRest", "Save when resting", B, "1", "", Some(false)),
    d("Saves", "MAIN", "bSaveOnWait", "Save when waiting", B, "1", "", Some(false)),
    d("Saves", "MAIN", "bSaveOnTravel", "Save when travelling", B, "1", "", Some(false)),
    d("Mods", "Archive", "bInvalidateOlderFiles", "Loose files win over archives", B, "0", "Fallout 4 needs it on, or loose files from mods don't replace the game's archives.", Some(true)),
    d("Mods", "Launcher", "bEnableFileSelection", "Mods enabled", B, "0", "Fallout 4 reads the plugin list only with this on.", Some(true)),
    d("Mods", "Grass", "bAllowCreateGrass", "Grass cache: create", B, "0", "On only while a grass cache is generated (No Grass In Objects), off to play.", Some(false)),
    d("Mods", "Grass", "bAllowLoadGrass", "Grass cache: load", B, "0", "On to use a grass cache (No Grass In Objects).", Some(false)),
    d("Logs", "Papyrus", "bEnableLogging", "Papyrus log", B, "0", "The script log that Problems' Papyrus log reads. Costs a little speed.", None),
    d("Logs", "Papyrus", "bEnableTrace", "Papyrus trace", B, "0", "Scripts' own messages in the log.", None),
    d("Logs", "Papyrus", "bLoadDebugInformation", "Papyrus line numbers", B, "0", "Line numbers in the log's errors.", None),
];

/// The setting files, in the order the game reads them (later wins).
fn files(game: Game) -> [String; 3] {
    let stem = game.ini_name().trim_end_matches(".ini");
    [format!("{stem}.ini"), format!("{stem}Prefs.ini"), format!("{stem}Custom.ini")]
}

#[derive(Debug, Serialize)]
pub struct Value {
    #[serde(flatten)]
    pub def: Def,
    /// As the game reads it; None: not set, the game's default.
    pub value: Option<String>,
    /// The file it comes from.
    pub file: Option<String>,
}

fn defs(game: Game) -> impl Iterator<Item = &'static Def> {
    let fo4 = game.base() == Game::Fallout4;
    SETTINGS.iter().filter(move |s| s.fo4.is_none_or(|f| f == fo4))
}

/// The game's settings in the form, with their values now.
pub fn read(inst: &Instance) -> Result<Vec<Value>> {
    let texts: Vec<(String, Option<String>)> = files(inst.config.game).into_iter().map(|f| {
        let t = workspace::read(inst, &format!("ini/{f}")).ok().map(|d| d.text);
        (f, t)
    }).collect();
    if texts.iter().all(|(_, t)| t.is_none()) {
        bail!("the game hasn't written its INI files yet: start it once from its launcher");
    }
    Ok(defs(inst.config.game)
        .map(|def| {
            let found = texts.iter().rev().find_map(|(f, t)| t.as_deref().and_then(|t| ini_get(t, def.section, def.key)).map(|v| (f.clone(), v)));
            Value { def: *def, value: found.as_ref().map(|(_, v)| v.clone()), file: found.map(|(f, _)| f) }
        })
        .collect())
}

/// Set `key` (in `section`) to `value`: in the file that holds it now, else Prefs.
pub fn set(inst: &Instance, section: &str, key: &str, value: &str, by: &str) -> Result<workspace::Edit> {
    let def = defs(inst.config.game).find(|d| d.section.eq_ignore_ascii_case(section) && d.key.eq_ignore_ascii_case(key)).with_context(|| format!("[{section}] {key} isn't one of the form's settings"))?;
    let ok = match def.kind {
        Kind::Bool => value == "0" || value == "1",
        Kind::Int { min, max } => value.parse::<i64>().is_ok_and(|v| (min..=max).contains(&v)),
        Kind::Float { min, max, .. } => value.parse::<f64>().is_ok_and(|v| (min..=max).contains(&v)),
        Kind::Choice { options } => options.contains(&value),
    };
    if !ok {
        bail!("{value} isn't a value {} takes", def.label);
    }
    let value = match def.kind {
        Kind::Float { .. } => format!("{:.4}", value.parse::<f64>()?),
        _ => value.to_owned(),
    };
    let [main, prefs, custom] = files(inst.config.game);
    let docs: Vec<(String, workspace::Doc)> = [custom, prefs.clone(), main].into_iter().filter_map(|f| workspace::read(inst, &format!("ini/{f}")).ok().map(|d| (f, d))).collect();
    let (file, doc) = docs
        .iter()
        .find(|(_, d)| ini_get(&d.text, def.section, def.key).is_some())
        .or_else(|| docs.iter().find(|(f, _)| *f == prefs))
        .or(docs.last())
        .context("the game hasn't written its INI files yet: start it once from its launcher")?;
    let text = ini_set(doc.text.as_bytes(), &[(def.section, def.key, Some(value))]);
    workspace::write(inst, &format!("ini/{file}"), &String::from_utf8_lossy(&text), Some(&doc.hash), by)
}
