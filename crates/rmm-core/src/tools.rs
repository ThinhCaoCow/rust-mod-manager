//! Tool runs (SPEC.md "Tools"): launch xEdit, BodySlide, Synthesis or any other program against
//! the deployed setup, then capture what it wrote into a generated mod.
//!
//! A run disables the tool's previous output and deploys, so the tool sees the current setup.
//! Deployed files the tool rewrites in place get their hard links swapped for copies, so the mod
//! store isn't written through. rmm snapshots Data, runs the tool and diffs Data. The generated
//! mod is built from the staging folder (`{output}`) and the diff. It replaces the previous output
//! (which goes to trash/ so undo restores it), and a final deploy links it in.
//!
//! A tool set to edit in place (xEdit) instead works like MO2's "create files in mod": nothing is
//! disabled, files it changes go back to the mods they came from, and new files go into the
//! output mod, added to rather than replaced. Each mod changed is kept as it was for undo.
use crate::deploy::{self, Deployed, DeployPlan};
use crate::instance::{ModMeta, index_dir, write_atomic};
use crate::install::{is_plugin, sanitize, unique_id};
use crate::{Game, Instance, ModEntry, PluginEntry, ops, path_key, proton};
use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const TOOLS: &str = "tools.toml";
/// Presets `add` knows. The exe always comes from the user.
pub const PRESETS: &[&str] = &["xedit", "xedit-qac", "xlodgen", "bodyslide", "synthesis", "pandora", "pgpatcher", "texgen", "dyndolod"];
const LIST_CAP: usize = 100;
/// Presets that generate output from the whole setup, in the order a rebuild runs them: TexGen's
/// textures before DynDOLOD, and the patchers after what the generators made.
pub const CHAIN: &[&str] = &["xlodgen", "texgen", "dyndolod", "pandora", "bodyslide", "synthesis", "pgpatcher"];

/// A configured tool, stored in the instance's `tools.toml` under its id.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Tool {
    pub name: String,
    pub exe: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    /// Default values for `{var}` placeholders; a run can override them.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub vars: BTreeMap<String, String>,
    /// Data-relative globs (`*`, `**`) of files the tool rewrites in place. Deployed ones are
    /// swapped for copies during the run; vanilla ones are kept so they can be restored.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rewrites: Vec<String>,
    /// Name of the generated mod (placeholders allowed). Defaults to "<name> Output".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_mod: Option<String>,
    /// Working directory; defaults to the exe's folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Globs of files from the previous output copied into {output} before a run: state the tool
    /// reads back, such as Pandora's mod list or PGPatcher's plugin (stable FormIDs).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keep: Vec<String>,
    /// A JSON settings file edited before each run, for tools that take paths only from there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<Settings>,
    /// A program, relative to the game folder, that the run also waits for: what a launcher
    /// starts before exiting, e.g. SkyrimSE.exe behind skse64_loader.exe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_for: Option<String>,
    /// xEdit-style patching: the previous output stays enabled, files the tool changes go back to
    /// the mods they come from, and new files go into `output_mod` (an installed mod's id or name,
    /// or a new mod by that name), added to rather than replaced.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub edit_in_place: bool,
}

/// Keys rmm sets in a tool's JSON settings file before each run. Everything else in it stays.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default, PartialEq)]
pub struct Settings {
    /// The JSON file, relative to the exe's folder unless absolute. Created if missing.
    pub file: PathBuf,
    /// Dotted key path -> value, e.g. {"games.SkyrimSE.gameDataPath": "{data}"}. Placeholders
    /// in string values are filled in.
    pub set: BTreeMap<String, Value>,
}

/// What `add` takes: a preset and/or explicit fields. Fields given override the preset's.
#[derive(Deserialize, JsonSchema, Default, Debug)]
pub struct ToolSpec {
    /// Path to the tool's executable.
    pub exe: PathBuf,
    /// xedit, xedit-qac, xlodgen, bodyslide, synthesis, pandora, pgpatcher, texgen or dyndolod.
    /// Defaults to the tool id when it names a preset.
    #[serde(default)]
    pub preset: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    /// Arguments. Placeholders: {data}, {game}, {plugins} (plugins.txt), {ini} (game INI file),
    /// {ini_dir}, {output} (empty folder captured into the generated mod), {state} (rmm's folder
    /// for this tool) and any var.
    #[serde(default)]
    pub args: Option<Vec<String>>,
    /// Default placeholder values, e.g. {"groups": "CBBE"} for BodySlide.
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    /// Data-relative globs of files the tool rewrites in place.
    #[serde(default)]
    pub rewrites: Option<Vec<String>>,
    /// Name of the generated mod.
    #[serde(default)]
    pub output_mod: Option<String>,
    /// Globs of files from the previous output copied into {output} before a run.
    #[serde(default)]
    pub keep: Option<Vec<String>>,
    /// JSON settings keys set before each run.
    #[serde(default)]
    pub settings: Option<Settings>,
    /// A program, relative to the game folder, the run also waits for, because a launcher
    /// starts it and exits (SkyrimSE.exe behind skse64_loader.exe, CreationKit.exe behind ckpe_loader.exe).
    #[serde(default)]
    pub wait_for: Option<String>,
    /// xEdit-style: changed files go back to the mods they come from, new files into output_mod
    /// (an installed mod's id or name, or a new mod), and the previous output stays enabled.
    /// The xedit preset does this.
    #[serde(default)]
    pub edit_in_place: Option<bool>,
}

#[derive(Debug, Default, Serialize)]
pub struct RunReport {
    pub tool: String,
    pub dry_run: bool,
    /// Executable followed by its arguments, placeholders filled in.
    pub command: Vec<String>,
    /// Name of the generated mod this run fills.
    pub output_mod: String,
    /// The tool's previous generated mod: disabled for the run, then replaced. Editing in place,
    /// the installed mod new files go into, which stays enabled.
    pub previous: Option<String>,
    /// Changed files go back to their mods, new files into the output mod.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub edit_in_place: bool,
    /// Editing in place: the mod files the tool's changes went back to, as "mod: path".
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub into_mods: Vec<String>,
    /// Deployed files whose hard links are swapped for copies while the tool runs.
    pub swapped: Vec<String>,
    /// The settings file rmm edits before the run.
    pub settings_file: Option<PathBuf>,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// The new generated mod; none when the tool wrote nothing.
    pub mod_id: Option<String>,
    pub files: usize,
    /// Data-relative paths the tool created, changed or deleted.
    pub new: Vec<String>,
    pub changed: Vec<String>,
    pub deleted: Vec<String>,
    /// Mod files the tool rewrote through a hard link. Their original contents are gone:
    /// reinstall those mods, and add the paths to the tool's rewrites.
    pub modified_in_place: Vec<String>,
    pub warnings: Vec<String>,
    pub log: Option<PathBuf>,
    pub log_tail: String,
    /// What deploying now would do (dry run), or what the final deploy did.
    pub deploy: Option<DeployPlan>,
    /// LOOT's sort after a run that added plugins (only once a masterlist is cached).
    pub sort: Option<crate::loot::SortReport>,
}

pub fn list(inst: &Instance) -> Result<BTreeMap<String, Tool>> {
    match fs::read_to_string(inst.root.join(TOOLS)) {
        Ok(t) => Ok(toml::from_str(&t)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e.into()),
    }
}

/// Add or replace a tool.
pub fn add(inst: &Instance, id: &str, spec: ToolSpec) -> Result<Tool> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("tool ids may only use letters, digits, '-' and '_'");
    }
    if !spec.exe.is_file() {
        bail!("{} not found", spec.exe.display());
    }
    let preset_name = spec.preset.as_deref().or(PRESETS.contains(&id).then_some(id));
    let mut tool = match preset_name {
        Some(p) => preset(inst.config.game, p).with_context(|| format!("no preset '{p}' for this game (presets: {})", PRESETS.join(", ")))?,
        None => Tool { name: id.into(), ..Default::default() },
    };
    tool.exe = spec.exe;
    tool.vars.extend(spec.vars);
    if let Some(n) = spec.name {
        tool.name = n;
    }
    if let Some(a) = spec.args {
        tool.args = a;
    }
    if let Some(r) = spec.rewrites {
        tool.rewrites = r;
    }
    if spec.output_mod.is_some() {
        tool.output_mod = spec.output_mod;
    }
    if let Some(k) = spec.keep {
        tool.keep = k;
    }
    if spec.settings.is_some() {
        tool.settings = spec.settings;
    }
    if spec.wait_for.is_some() {
        tool.wait_for = spec.wait_for;
    }
    if let Some(e) = spec.edit_in_place {
        tool.edit_in_place = e;
    }
    put(inst, id, &tool)?;
    Ok(tool)
}

/// Store a tool under `id`, replacing one there.
pub(crate) fn put(inst: &Instance, id: &str, tool: &Tool) -> Result<()> {
    inst.locked(|| {
        let mut tools = list(inst)?;
        tools.insert(id.into(), tool.clone());
        write_atomic(&inst.root.join(TOOLS), &toml::to_string_pretty(&tools)?)
    })
}

pub fn remove(inst: &Instance, id: &str) -> Result<()> {
    inst.locked(|| {
        let mut tools = list(inst)?;
        if tools.remove(id).is_none() {
            bail!("no tool '{id}'");
        }
        write_atomic(&inst.root.join(TOOLS), &toml::to_string_pretty(&tools)?)
    })
}

/// Built-in definitions, from reading each tool's source (docs/internals/tools/).
pub fn preset(game: Game, name: &str) -> Option<Tool> {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let (switch, xedit) = match (game.lo_id(), game.base()) {
        (loadorder::GameId::SkyrimVR, _) => ("-TES5VR", "TES5VREdit"),
        (loadorder::GameId::Fallout4VR, _) => ("-FO4VR", "FO4VREdit"),
        (_, Game::Fallout4) => ("-FO4", "FO4Edit"),
        _ => ("-SSE", "SSEEdit"),
    };
    // xEdit only finds Steam installs by itself, and keeps backups and its cache inside Data.
    // -C: gets no trailing separator from xEdit, so it is written here.
    let xedit_args = [switch, "-D:{data}", "-P:{plugins}", "-M:{ini_dir}", "-I:{ini}", r"-B:{state}\backups", r"-C:{state}\cache\"];
    // DynDOLOD's tools (xEdit-based, case-insensitive switches) want a trailing backslash on folders.
    let lod_args = [switch, r"-o:{output}\", r"-d:{data}\", "-p:{plugins}", r"-m:{ini_dir}\"];
    let tool = |name: &str, args: Vec<String>| Tool { name: name.into(), args, ..Default::default() };
    let settings = |file: &str, set: &[(&str, Value)]| Some(Settings { file: file.into(), set: set.iter().map(|(k, v)| (k.to_string(), v.clone())).collect() });
    let skyrim = game.base() == Game::SkyrimSe;
    Some(match name {
        // Saves by writing a new file and renaming it over the plugin, so links are safe. Edits go
        // back to the plugins' mods, as patching sessions want.
        "xedit" => Tool { edit_in_place: true, ..tool(xedit, s(&xedit_args)) },
        "xedit-qac" => Tool {
            rewrites: s(&["{plugin}"]),
            output_mod: Some("Cleaned {plugin}".into()),
            ..tool(&format!("{xedit} QuickAutoClean"), s(&[&xedit_args[..], &["-qac", "-autoload", "-autoexit", "{plugin}"]].concat()))
        },
        // Truncates its config XMLs (next to the exe, usually in Data) on every exit.
        "bodyslide" => Tool {
            rewrites: s(&["CalienteTools/BodySlide/*.xml"]),
            ..tool("BodySlide", s(&["--groupbuild", "{groups}", "--targetdir", "{output}", "--trimorphs", "--quiet"]))
        },
        // The CLI isn't in Synthesis releases; build Synthesis.Bethesda.CLI. Without -m Text it
        // keeps no FormID mapping between runs.
        "synthesis" => tool(
            "Synthesis",
            s(&["run-pipeline", "-s", "{settings}", "-p", "{profile}", "-d", "{data}", "-l", "{plugins}", "-o", "{output}", "-m", "Text", "-r", r"{state}\persistence"]),
        ),
        // xEdit in LOD mode: terrain LOD for both games. Without -o: it writes to the game folder.
        "xlodgen" => tool("xLODGen", s(&[&xedit_args[..], &["-lodgen", r"-o:{output}\"]].concat())),
        // GUI only: someone has to click through. TexGen runs before DynDOLOD, and its output
        // must be enabled when DynDOLOD runs. DynDOLOD has no Fallout 4 object LOD.
        "texgen" => tool("TexGen", s(&lod_args)),
        "dyndolod" if skyrim => tool("DynDOLOD", s(&lod_args)),
        // 4.1 takes --tesv (the game folder) and --output; without them it writes to the Data
        // folder of the game in the registry, whatever rmm's game is. Older versions read the
        // paths only from Settings.json, where outputPath must exist or output goes to Data.
        "pandora" if skyrim => Tool {
            settings: settings("Settings.json", &[("games.SkyrimSE.gameDataPath", "{data}".into()), ("games.SkyrimSE.outputPath", "{output}".into())]),
            keep: s(&["Pandora_Engine/ActiveMods.json"]),
            ..tool("Pandora", s(&["--tesv", "{game}", "--output", "{output}", "--auto_run", "--auto_close"]))
        },
        // Mod manager 0 (None) reads the deployed Data folder. It reuses the previous
        // PGPatcher.esp for stable FormIDs. The run ends in a dialog someone has to close.
        "pgpatcher" if skyrim => Tool {
            settings: settings(
                "cfg/settings.json",
                &[("params.game.dir", "{game}".into()), ("params.modmanager.type", 0.into()), ("params.output.dir", "{output}".into()), ("params.output.zip", false.into())],
            ),
            keep: s(&["PGPatcher.esp"]),
            ..tool("PGPatcher", s(&["--autostart"]))
        },
        _ => return None,
    })
}

/// A configured tool's place in `CHAIN`: its id is the preset's, or it has the preset's name.
// ponytail: tools don't record their preset, so one renamed under another id isn't recognised.
fn chain_rank(inst: &Instance, id: &str, tool: &Tool) -> Option<usize> {
    CHAIN.iter().position(|p| *p == id || preset(inst.config.game, p).is_some_and(|t| t.name == tool.name))
}

/// What a whole-setup tool reads: each enabled mod but generated output (so outputs don't make
/// each other stale) that has files the tool reads, by those files, the archive or Nexus file it
/// came from and its plugins' sizes and times; and the order of the plugins the tool reads.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Inputs {
    pub mods: BTreeMap<String, String>,
    pub plugins: String,
    /// What the tool reads (`Reads::name`); empty from an rmm that counted every file.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reads: String,
}

/// What a whole-setup tool reads, so only a change to that makes its output out of date.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Reads {
    /// LOD and parallax generators: meshes, textures, archives, and plugins with worldspace,
    /// cell or object records.
    Lod,
    /// Pandora: behaviour and animation files.
    Behaviour,
    /// BodySlide: its projects and shapes under CalienteTools.
    BodySlide,
    /// Synthesis: every plugin.
    Plugins,
    /// Anything else: every file and plugin.
    All,
}

/// Top-level record groups a LOD or parallax generator reads.
const LOD_GROUPS: &[&[u8; 4]] = &[b"WRLD", b"CELL", b"STAT", b"MSTT", b"TREE", b"FLOR", b"ACTI", b"FURN", b"SCOL", b"TXST", b"LTEX", b"GRAS", b"LIGH", b"DOOR", b"CONT"];

impl Reads {
    /// From a `CHAIN` preset's name.
    pub fn of(preset: &str) -> Reads {
        match preset {
            "xlodgen" | "texgen" | "dyndolod" | "pgpatcher" => Reads::Lod,
            "pandora" => Reads::Behaviour,
            "bodyslide" => Reads::BodySlide,
            "synthesis" => Reads::Plugins,
            _ => Reads::All,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Reads::Lod => "lod",
            Reads::Behaviour => "behaviour",
            Reads::BodySlide => "bodyslide",
            Reads::Plugins => "plugins",
            Reads::All => "all",
        }
    }
    /// A mod file (lowercase, mod-relative) that isn't a plugin.
    fn file(self, f: &str) -> bool {
        let archive = f.ends_with(".bsa") || f.ends_with(".ba2");
        match self {
            Reads::Lod => archive || ["meshes/", "textures/", "dyndolod/", "lodsettings/"].iter().any(|d| f.starts_with(d)),
            Reads::Behaviour => (f.starts_with("meshes/") && [".hkx", ".txt", ".xml"].iter().any(|e| f.ends_with(e))) || f.starts_with("nemesis_engine/") || f.starts_with("pandora_engine/"),
            Reads::BodySlide => f.starts_with("calientetools/"),
            Reads::Plugins => false,
            Reads::All => true,
        }
    }
    fn plugin(self, path: &Path) -> bool {
        match self {
            Reads::Lod => crate::plugins::top_groups(path).is_ok_and(|g| LOD_GROUPS.iter().any(|k| g.contains(*k))),
            Reads::Behaviour | Reads::BodySlide => false,
            Reads::Plugins | Reads::All => true,
        }
    }
}

/// The `CHAIN` preset a configured tool is, if it's a whole-setup generator.
pub fn chain_preset(inst: &Instance, id: &str, tool: &Tool) -> Option<&'static str> {
    chain_rank(inst, id, tool).map(|i| CHAIN[i])
}

// ponytail: a plugin counts by its size and time, not its contents: an edit that keeps both (rare)
// goes unseen, and a reinstall of the same file with a new time counts as a change.
pub fn inputs(inst: &Instance, reads: Reads) -> Result<Inputs> {
    use sha2::{Digest, Sha256};
    let digest = |parts: &[String]| -> String {
        let mut h = Sha256::new();
        for s in parts {
            h.update(s.as_bytes());
            h.update([0]);
        }
        h.finalize()[..8].iter().map(|b| format!("{b:02x}")).collect()
    };
    let p = inst.profile()?;
    let read: std::collections::HashMap<String, bool> = crate::plugins::list(inst, &p)?.into_iter().filter(|x| x.enabled).map(|x| (x.name.to_lowercase(), reads.plugin(&x.path))).collect();
    let (mut mods, mut generated) = (BTreeMap::new(), HashSet::new());
    for m in p.mods.iter().filter(|m| m.enabled && !crate::instance::is_separator(&m.id)) {
        let meta = inst.mod_meta(&m.id);
        let files = inst.mod_files(&m.id).unwrap_or_default();
        if meta.source.as_deref().is_some_and(|s| s.starts_with("tool:")) {
            generated.extend(files.iter().filter(|f| !f.contains('/') && is_plugin(f)).map(|f| f.to_lowercase()));
            continue;
        }
        let mut parts = Vec::new();
        for f in &files {
            let lower = f.to_lowercase();
            if !lower.contains('/') && is_plugin(f) {
                if read.get(&lower).copied().unwrap_or(false) {
                    let st = fs::metadata(inst.mod_dir(&m.id).join(f)).ok();
                    let when = st.as_ref().and_then(|s| s.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
                    parts.push(format!("{f} {} {when}", st.map_or(0, |s| s.len())));
                }
            } else if reads.file(&lower) {
                parts.push(f.clone());
            }
        }
        if parts.is_empty() {
            continue; // nothing the tool reads
        }
        parts.push(meta.sha256.unwrap_or_default());
        parts.push(meta.nexus.and_then(|n| n.file_id).map(|f| f.to_string()).unwrap_or_default());
        mods.insert(m.id.clone(), digest(&parts));
    }
    let order: Vec<String> = p
        .plugins
        .iter()
        .map(|e| e.name.to_lowercase())
        .filter(|n| read.get(n).copied().unwrap_or(false) && !generated.contains(n))
        .collect();
    Ok(Inputs { mods, plugins: digest(&order), reads: reads.name().into() })
}

/// A generated mod whose tool would now read a different setup.
#[derive(Serialize, Debug)]
pub struct Stale {
    pub output: String,
    pub tool: String,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
    pub load_order: bool,
    /// rmm doesn't know what it was made from: imported, or made by an older rmm.
    pub unknown: bool,
}

impl Stale {
    /// "2 mods added (A, B), 1 mod changed (C)", or that the load order changed.
    pub fn summary(&self) -> String {
        if self.unknown {
            return "rmm doesn't know what it was made from (it was imported, or made before rmm kept track)".into();
        }
        let part = |what: &str, ids: &[String]| {
            (!ids.is_empty()).then(|| {
                let more = if ids.len() > 3 { ", …" } else { "" };
                format!("{} mod{} {what} ({}{more})", ids.len(), if ids.len() == 1 { "" } else { "s" }, ids[..ids.len().min(3)].join(", "))
            })
        };
        let parts: Vec<String> = [part("added", &self.added), part("removed", &self.removed), part("changed", &self.changed)].into_iter().flatten().collect();
        if parts.is_empty() { "the load order changed".into() } else { parts.join(", ") }
    }
}

/// Enabled output of whole-setup tools made from a setup that has changed since, in what the
/// tool reads, or from a setup rmm doesn't know (`unknown`: imported, or made by an older rmm).
pub fn stale_outputs(inst: &Instance) -> Result<Vec<Stale>> {
    let tools = list(inst)?;
    let mut now: std::collections::HashMap<&'static str, Inputs> = Default::default();
    let mut out = vec![];
    for m in inst.profile()?.mods.into_iter().filter(|m| m.enabled && !crate::instance::is_separator(&m.id)) {
        let meta = inst.mod_meta(&m.id);
        let Some(tool) = meta.source.as_deref().and_then(|s| s.strip_prefix("tool:")).map(str::to_owned) else { continue };
        let preset = tools.get(&tool).and_then(|t| chain_preset(inst, &tool, t)).or_else(|| CHAIN.iter().find(|p| **p == tool).copied());
        let Some(preset) = preset else { continue }; // not a whole-setup tool
        let reads = Reads::of(preset);
        let mut s = Stale { tool, output: m.id, added: vec![], removed: vec![], changed: vec![], load_order: false, unknown: false };
        match meta.built_from.filter(|b| b.reads == reads.name()) {
            None => s.unknown = true,
            Some(then) => {
                let now = match now.get(reads.name()) {
                    Some(n) => n,
                    None => now.entry(reads.name()).or_insert(inputs(inst, reads)?),
                };
                let keys = |a: &Inputs, b: &Inputs| a.mods.keys().filter(|k| !b.mods.contains_key(*k)).cloned().collect::<Vec<_>>();
                s.added = keys(now, &then);
                s.removed = keys(&then, now);
                s.changed = now.mods.iter().filter(|(k, v)| then.mods.get(*k).is_some_and(|t| t != *v)).map(|(k, _)| k.clone()).collect();
                s.load_order = now.plugins != then.plugins;
            }
        }
        if s.unknown || s.load_order || !(s.added.is_empty() && s.removed.is_empty() && s.changed.is_empty()) {
            out.push(s);
        }
    }
    Ok(out)
}

#[derive(Serialize, Debug, Default)]
pub struct Rebuild {
    pub dry_run: bool,
    /// The tools to run, in order, and why.
    pub steps: Vec<(String, String)>,
    /// Each run's generated mod (None: it captured nothing).
    pub done: Vec<(String, Option<String>)>,
    /// The run that failed, and why; the tools after it didn't run, since they'd read its output.
    pub failed: Option<(String, String)>,
}

/// Run again, in `CHAIN` order, the whole-setup tools whose output is out of date or was made
/// before rmm kept track of its inputs, each a run of its own. Tools with no output in this
/// profile are left alone.
pub fn rebuild(inst: &Instance, dry_run: bool, timeout: Option<Duration>) -> Result<Rebuild> {
    let (stale, p) = (stale_outputs(inst)?, inst.profile()?);
    let mut chain: Vec<(usize, String)> = list(inst)?.into_iter().filter_map(|(id, t)| Some((chain_rank(inst, &id, &t)?, id))).collect();
    chain.sort();
    let mut r = Rebuild { dry_run, ..Default::default() };
    for (_, id) in chain {
        let source = format!("tool:{id}");
        let Some(out) = p.mods.iter().find(|m| m.enabled && inst.mod_meta(&m.id).source.as_deref() == Some(source.as_str())) else { continue };
        let Some(why) = stale.iter().find(|s| s.output == out.id).map(Stale::summary) else { continue };
        r.steps.push((id, why));
    }
    if dry_run {
        return Ok(r);
    }
    for (i, (id, _)) in r.steps.iter().enumerate() {
        crate::progress::report(&format!("running {id}"), i, r.steps.len());
        match run(inst, id, &BTreeMap::new(), false, timeout) {
            Ok(x) => r.done.push((id.clone(), x.mod_id)),
            Err(e) => {
                r.failed = Some((id.clone(), format!("{e:#}")));
                break;
            }
        }
    }
    Ok(r)
}

#[derive(Serialize, Debug, Default)]
pub struct Cleaned {
    pub dry_run: bool,
    /// The QuickAutoClean tool's id.
    pub tool: String,
    /// The plugins to clean (or cleaned), with what LOOT found in them.
    pub plugins: Vec<crate::loot::Dirty>,
    /// Plugins with only deleted navmeshes, which QuickAutoClean can't fix: xEdit by hand.
    pub by_hand: Vec<crate::loot::Dirty>,
    /// Each cleaned plugin and the mod its clean copy went into ("Cleaned <plugin>").
    pub mods: Vec<(String, Option<String>)>,
    /// Plugins whose run failed, and why.
    pub failed: Vec<(String, String)>,
    /// Plugins LOOT still finds dirty afterwards.
    pub still_dirty: Vec<String>,
}

/// Clean every enabled plugin LOOT's masterlist knows as dirty with xEdit QuickAutoClean, one run
/// after another (each its own undo step, its clean copy in its own "Cleaned <plugin>" mod), as
/// MO2's Batch Plugin Cleaner does. Needs a QuickAutoClean tool (the xedit-qac preset); a dry run
/// lists the plugins.
pub fn clean_dirty(inst: &Instance, dry_run: bool) -> Result<Cleaned> {
    let tool = list(inst)?
        .into_iter()
        .find(|(_, t)| t.args.iter().any(|a| a.eq_ignore_ascii_case("-qac")))
        .map(|(id, _)| id)
        .context("add xEdit QuickAutoClean as a tool first: the xedit-qac preset with SSEEdit's or FO4Edit's exe (Tools, Add a tool)")?;
    let (plugins, by_hand) = crate::loot::dirty_plugins(inst)?.into_iter().partition(|d| d.itm + d.deleted_references > 0);
    let mut r = Cleaned { dry_run, tool, plugins, by_hand, ..Default::default() };
    if dry_run {
        return Ok(r);
    }
    for (i, d) in r.plugins.iter().enumerate() {
        crate::progress::report(&format!("cleaning {}", d.plugin), i, r.plugins.len());
        match run(inst, &r.tool, &BTreeMap::from([("plugin".to_string(), d.plugin.clone())]), false, None) {
            Ok(x) => r.mods.push((d.plugin.clone(), x.mod_id)),
            Err(e) => r.failed.push((d.plugin.clone(), format!("{e:#}"))),
        }
    }
    let after = crate::loot::dirty_plugins(inst)?;
    r.still_dirty = r.plugins.iter().filter(|d| after.iter().any(|a| a.plugin == d.plugin)).map(|d| d.plugin.clone()).collect();
    Ok(r)
}

/// Run a tool. With `dry_run` nothing happens: the report shows the command, the generated mod
/// it would replace, the links it would swap and the deploy that runs first.
pub fn run(inst: &Instance, id: &str, given: &BTreeMap<String, String>, dry_run: bool, timeout: Option<Duration>) -> Result<RunReport> {
    let mut r = run_inner(inst, id, given, dry_run, timeout)?;
    if !dry_run {
        let done = crate::addons::hook(inst, "after_tool_run", &serde_json::json!({ "tool": id, "mod": r.mod_id }));
        r.warnings.extend(crate::addons::warnings(&done));
    }
    Ok(r)
}

fn run_inner(inst: &Instance, id: &str, given: &BTreeMap<String, String>, dry_run: bool, timeout: Option<Duration>) -> Result<RunReport> {
    let tools = list(inst)?;
    let tool = tools.get(id).with_context(|| format!("no tool '{id}'; add one first (presets: {})", PRESETS.join(", ")))?;
    let state = inst.root.join("tools").join(id);
    let staging = inst.root.join(".staging").join(format!("tool-{id}"));
    let vars = vars(inst, tool, &state, &staging, given);
    let args = tool.args.iter().map(|a| expand(a, &vars)).collect::<Result<Vec<_>>>()?;
    let rewrites = tool.rewrites.iter().map(|g| expand(g, &vars).map(|g| path_key(&g))).collect::<Result<Vec<_>>>()?;
    let output_mod = expand(tool.output_mod.as_deref().unwrap_or(&format!("{} Output", tool.name)), &vars)?;
    if !tool.exe.is_file() {
        bail!("{} not found", tool.exe.display());
    }
    let source = format!("tool:{id}");
    let in_place = tool.edit_in_place;
    let previous = inst.profile()?.mods.into_iter().find(|m| {
        if crate::instance::is_separator(&m.id) {
            return false;
        }
        let meta = inst.mod_meta(&m.id);
        match in_place {
            true => m.id == output_mod || meta.name == output_mod,
            false => meta.source.as_deref() == Some(source.as_str()) && meta.name == output_mod,
        }
    });
    let mut r = RunReport {
        tool: id.into(),
        dry_run,
        command: std::iter::once(tool.exe.display().to_string()).chain(args.iter().cloned()).collect(),
        output_mod: output_mod.clone(),
        previous: previous.as_ref().map(|m| m.id.clone()),
        edit_in_place: in_place,
        ..Default::default()
    };
    let disable_previous = previous.as_ref().filter(|m| m.enabled && !in_place).map(|m| m.id.clone());
    r.settings_file = tool.settings.as_ref().map(|s| tool.exe.parent().unwrap_or(Path::new("")).join(&s.file));
    if let Some(m) = previous.as_ref().filter(|m| in_place && !m.enabled) {
        r.warnings.push(format!("{} is disabled: the tool doesn't see it, and new files go into it all the same", m.id));
    }

    if dry_run {
        r.swapped = swaps(&deploy::manifest_files(inst)?, &rewrites).into_iter().map(|e| e.target).collect();
        r.deploy = Some(deploy::deploy(inst, true)?);
        if let Some(p) = &disable_previous {
            r.warnings.push(format!("{p} is disabled before the run, so the first deploy also removes its files"));
        }
        return Ok(r);
    }

    // ponytail: the instance lock isn't held while the tool runs (deploy and edit take it
    // themselves); a CLI write during a run is the user's problem. Add a run marker if it bites.
    if let Some(p) = &disable_previous {
        ops::set_mod_enabled(inst, p, false)?;
    }
    deploy::deploy(inst, false)?;
    // A whole-setup generator notes what it reads, so its output can tell when it's out of date.
    let built_from = match chain_preset(inst, id, tool) {
        Some(p) if !in_place => Some(inputs(inst, Reads::of(p))?),
        _ => None,
    };

    let game = &inst.config.game_path;
    let stash = state.join("stash");
    let _ = fs::remove_dir_all(&stash);
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;
    let kept = |f: &str| tool.keep.iter().any(|g| glob(g, f));
    if let Some(old) = previous.as_ref().filter(|_| !in_place) {
        let dir = inst.mod_dir(&old.id);
        for f in index_dir(&dir)?.into_iter().filter(|f| kept(f)) {
            copy_file(&dir.join(&f), &staging.join(&f))?;
        }
    }
    // rmm's keys go back to their old values after the run; anything else the tool saves stays.
    let mut restore = None;
    if let (Some(s), Some(path)) = (&tool.settings, &r.settings_file) {
        let set = s.set.iter().map(|(k, v)| {
            let v = match v {
                Value::String(t) => Value::String(expand(t, &vars)?),
                v => v.clone(),
            };
            Ok((k.clone(), Some(v)))
        });
        let existed = path.exists();
        restore = Some((path.clone(), edit_json(path, set.collect::<Result<_>>()?)?, existed));
    }
    let man = deploy::manifest_files(inst)?;
    let swapped = swaps(&man, &rewrites);
    for e in &swapped {
        swap_for_copy(&game.join(&e.target))?;
    }
    r.swapped = swapped.iter().map(|e| e.target.clone()).collect();
    let swapped: HashSet<String> = swapped.iter().map(|e| path_key(&e.target)).collect();
    let before = snapshot(game)?;
    let rewritten = |key: &str| key.strip_prefix("data/").is_some_and(|k| rewrites.iter().any(|g| glob(g, k)));
    for (key, (rel, ..)) in &before {
        if !man.contains_key(key) && rewritten(key) {
            copy_file(&game.join(rel), &stash.join(rel))?;
        }
    }

    let log = state.join("last-run.log");
    let launched = launch(inst, tool, &args, &log, timeout);
    if let Ok((code, timed_out)) = launched {
        (r.exit_code, r.timed_out) = (code, timed_out);
        if let Some(exe) = tool.wait_for.as_deref().filter(|_| !timed_out)
            && !wait_for(game, exe)
        {
            r.warnings.push(format!("{exe} never started, so the run didn't wait for it"));
        }
    }
    if let Some((path, previous, existed)) = restore {
        match edit_json(&path, previous) {
            Err(e) => r.warnings.push(format!("couldn't put back the settings rmm changed in {}: {e:#}", path.display())),
            // A file rmm made that holds nothing once its keys are out isn't left behind.
            Ok(_) if !existed && fs::read(&path).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).is_some_and(|v| hollow(&v)) => {
                let _ = fs::remove_file(&path);
            }
            Ok(_) => {}
        }
    }
    match launched {
        Ok(_) => {}
        Err(e) => {
            // Nothing ran: drop the copies and put the previous output back.
            for t in &r.swapped {
                let _ = fs::remove_file(game.join(t));
            }
            if let Some(p) = &disable_previous {
                ops::set_mod_enabled(inst, p, true)?;
            }
            deploy::deploy(inst, false)?;
            return Err(e);
        }
    }
    r.log_tail = tail(&log);
    r.log = Some(log);

    let after = snapshot(game)?;
    let mut added_plugins: Vec<String> = vec![];
    // Editing in place, new files join the output mod if it's installed; mods rmm changes are
    // kept as they were first (mod id -> folder under trash/), so undo can swap them back.
    let target = previous.as_ref().filter(|_| in_place).map(|m| m.id.clone());
    let new_id = target.clone().unwrap_or_else(|| unique_id(inst, &sanitize(&output_mod)));
    let dest = inst.mod_dir(&new_id);
    let mut kept_as: BTreeMap<String, String> = BTreeMap::new();
    let mut keep_old = |id: &str| -> Result<()> {
        if !kept_as.contains_key(id) {
            kept_as.insert(id.into(), inst.keep_files(id)?);
        }
        Ok(())
    };
    match &target {
        Some(t) => {
            for f in index_dir(&staging)? {
                keep_old(t)?;
                r.new.push(f.clone());
                move_file(&staging.join(&f), &dest.join(&f), &staging)?;
            }
            fs::remove_dir_all(&staging)?;
        }
        None => fs::rename(&staging, &dest)?,
    }
    // The tool's settings file is rmm's to set and put back, not output, even when the tool is
    // a mod run from Data.
    let settings_key = r.settings_file.as_ref().and_then(|p| p.strip_prefix(game).ok()).map(|p| path_key(&p.to_string_lossy()));
    // Keys and paths are game-relative; Data files go to the mod's top level, others to root/.
    for (key, (rel, size, mtime)) in after.iter().filter(|(k, _)| Some(*k) != settings_key.as_ref()) {
        let path = game.join(rel);
        let to = dest.join(deploy::mod_rel(rel));
        match before.get(key) {
            Some((_, s, m)) if (s, m) == (size, mtime) => {
                if swapped.contains(key) {
                    fs::remove_file(&path)?; // untouched copy: the final deploy relinks it
                }
            }
            Some(_) => {
                r.changed.push(rel.clone());
                match man.get(key) {
                    Some(e) if !e.copied && same_file::is_same_file(&path, inst.mod_dir(&e.mod_id).join(&e.src)).unwrap_or(false) => {
                        r.modified_in_place.push(format!("{}: {}", e.mod_id, e.src));
                        if !in_place {
                            copy_file(&path, &to)?;
                        }
                    }
                    // The tool saved a mod's file: it goes back into that mod.
                    Some(e) if in_place => {
                        keep_old(&e.mod_id)?;
                        move_file(&path, &inst.mod_dir(&e.mod_id).join(&e.src), game)?;
                        r.into_mods.push(format!("{}: {}", e.mod_id, e.src));
                    }
                    Some(_) => move_file(&path, &to, game)?,
                    None if stash.join(rel).is_file() => {
                        if target.is_some() {
                            keep_old(&new_id)?;
                        }
                        move_file(&path, &to, game)?;
                        move_file(&stash.join(rel), &path, game)?;
                    }
                    None => {
                        if target.is_some() {
                            keep_old(&new_id)?;
                        }
                        copy_file(&path, &to)?;
                        r.warnings.push(format!("{rel} changed but isn't in the tool's rewrites, so its original wasn't kept; purge leaves the tool's version"));
                    }
                }
            }
            None => {
                if target.is_some() {
                    keep_old(&new_id)?;
                }
                r.new.push(rel.clone());
                move_file(&path, &to, game)?;
            }
        }
    }
    for (key, (rel, ..)) in &before {
        if after.contains_key(key) || Some(key) == settings_key.as_ref() {
            continue;
        }
        r.deleted.push(rel.clone());
        if man.contains_key(key) {
            continue; // the final deploy relinks it
        }
        if stash.join(rel).is_file() {
            move_file(&stash.join(rel), &game.join(rel), game)?;
        } else {
            r.warnings.push(format!("{rel} was deleted and isn't rmm's to restore"));
        }
    }
    let _ = fs::remove_dir_all(&stash);

    if r.timed_out {
        r.warnings.push(format!("stopped after {}s; the output may be incomplete", timeout.unwrap_or_default().as_secs()));
    }
    if let Some(c) = r.exit_code.filter(|c| *c != 0) {
        r.warnings.push(format!("exited with code {c}"));
    }
    let files = index_dir(&dest)?;
    // A new output mod, unless the tool wrote nothing for one.
    let made = target.is_none() && !files.iter().all(|f| kept(f));
    if target.is_none() && !made {
        fs::remove_dir_all(&dest)?;
    }
    r.files = if in_place { r.new.len() + r.changed.len() } else { files.len() }; // editing in place: the files it put into mods
    if !made && kept_as.is_empty() {
        r.warnings.push("the tool wrote nothing to the game folder or {output}".into());
        if let Some(p) = &disable_previous {
            ops::set_mod_enabled(inst, p, true)?;
        }
    } else {
        if made {
            inst.write_mod_meta(&new_id, &ModMeta { name: output_mod, source: Some(source), built_from, ..Default::default() })?;
        }
        for id in kept_as.keys() {
            inst.forget_index(id);
        }
        let files = if dest.is_dir() { inst.mod_files(&new_id)? } else { vec![] };
        let known = inst.profile()?.plugins;
        let is_new = |pl: &String| !known.iter().any(|e| e.name.eq_ignore_ascii_case(pl));
        // Editing in place, the output mod's own plugins keep the state the user gave them.
        let plugins: Vec<String> = files.iter().filter(|f| !f.contains('/') && is_plugin(f) && (!in_place || is_new(f))).cloned().collect();
        added_plugins = plugins.iter().filter(|pl| is_new(pl)).cloned().collect();
        let swaps: Vec<(&str, &str)> = kept_as.iter().map(|(m, f)| (m.as_str(), f.as_str())).collect();
        inst.edit_swapping(&format!("run {id}"), &swaps, |p| {
            if made {
                let entry = ModEntry { id: new_id.clone(), enabled: true };
                match previous.as_ref().and_then(|old| p.mods.iter().position(|m| m.id == old.id)) {
                    Some(i) => p.mods[i] = entry,
                    None => p.mods.push(entry),
                }
            }
            for pl in &plugins {
                match p.plugins.iter_mut().find(|e| e.name.eq_ignore_ascii_case(pl)) {
                    Some(e) => e.enabled = true,
                    None => p.plugins.push(PluginEntry { name: pl.clone(), enabled: true }),
                }
            }
            Ok(())
        })?;
        if let Some(old) = previous.as_ref().filter(|_| made) {
            inst.trash_mod(&old.id)?;
            crate::rules::rename(inst, &old.id, &new_id)?;
        }
        r.mod_id = (made || kept_as.contains_key(&new_id)).then_some(new_id);
    }
    r.deploy = Some(deploy::deploy(inst, false)?);
    // New plugins land at the end of the list. Place them where LOOT would if the user sorts with
    // LOOT already, leaving every other plugin alone; the first sort, with its download, stays the
    // user's call.
    if !added_plugins.is_empty() && crate::settings::get().loot.sort_after_tools && inst.root.join("loot").join("masterlist.yaml").is_file() {
        match crate::loot::place(inst, &added_plugins) {
            Ok(s) => {
                if !s.moved.is_empty() {
                    crate::plugins::write_plugins_txt(inst, &inst.profile()?)?;
                }
                r.sort = Some(s);
            }
            Err(e) => r.warnings.push(format!("LOOT couldn't place the new plugins: {e:#}")),
        }
    }
    for v in [&mut r.new, &mut r.changed, &mut r.deleted, &mut r.into_mods] {
        if v.len() > LIST_CAP {
            let more = v.len() - LIST_CAP;
            v.truncate(LIST_CAP);
            v.push(format!("... and {more} more"));
        }
    }
    // For tool_job, and for anyone checking on a run after the fact.
    write_atomic(&state.join("last-run.json"), &serde_json::to_string_pretty(&r)?)?;
    Ok(r)
}

fn vars(inst: &Instance, tool: &Tool, state: &Path, staging: &Path, given: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    // Windows tools don't all accept '/', which paths typed into the CLI often contain. Under
    // Proton they see Linux paths through drive Z:.
    let wine = proton::needed(&tool.exe);
    let p = |x: &Path| match () {
        _ if wine => proton::windows_path(x),
        _ if cfg!(windows) => x.display().to_string().replace('/', "\\"),
        _ => x.display().to_string(),
    };
    let mut v = BTreeMap::from([
        ("game".to_string(), p(&inst.config.game_path)),
        ("data".into(), p(&inst.data_dir())),
        ("plugins".into(), p(&inst.config.local_path.join("Plugins.txt"))),
        ("output".into(), p(staging)),
        ("state".into(), p(state)),
    ]);
    if let (Some(d), Some(ini)) = (inst.ini_dir(), inst.ini_file(inst.config.game.ini_name())) {
        v.insert("ini".into(), p(&ini));
        v.insert("ini_dir".into(), p(&d));
    }
    v.extend(tool.vars.clone());
    v.extend(given.clone());
    v
}

/// Set (Some) or remove (None) dotted keys in a JSON settings file, keeping the rest of it.
/// Returns each key's previous value: passing that back undoes the edit.
/// Only empty objects, all the way down.
fn hollow(v: &Value) -> bool {
    v.as_object().is_some_and(|o| o.values().all(hollow))
}

fn edit_json(path: &Path, changes: Vec<(String, Option<Value>)>) -> Result<Vec<(String, Option<Value>)>> {
    let mut root = match fs::read(path) {
        // .NET tools may write a byte-order mark.
        Ok(b) => serde_json::from_slice(b.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&b)).with_context(|| format!("cannot parse {}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Object(Default::default()),
        Err(e) => return Err(e.into()),
    };
    let mut previous = Vec::new();
    'keys: for (key, v) in changes {
        let k = key.clone();
        let parts: Vec<&str> = k.split('.').collect();
        let (leaf, parents) = parts.split_last().unwrap();
        let mut node = &mut root;
        for part in parents.iter().chain([&""]) {
            if !node.is_object() {
                if v.is_none() {
                    previous.push((key, None)); // nothing there to remove
                    continue 'keys;
                }
                *node = Value::Object(Default::default());
            }
            if part.is_empty() {
                break;
            }
            node = node.as_object_mut().unwrap().entry(*part).or_insert_with(|| Value::Object(Default::default()));
        }
        let obj = node.as_object_mut().unwrap();
        let old = match v {
            Some(v) => obj.insert(leaf.to_string(), v),
            None => obj.remove(*leaf),
        };
        previous.push((key, old));
    }
    previous.reverse();
    fs::create_dir_all(path.parent().unwrap())?;
    write_atomic(path, &serde_json::to_string_pretty(&root)?)?;
    Ok(previous)
}

/// Fill `{name}` placeholders. Braces around anything but a name stay as they are.
fn expand(s: &str, vars: &BTreeMap<String, String>) -> Result<String> {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let name = rest[1..].find('}').map(|j| &rest[1..=j]).filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        match name {
            Some(n) => {
                out.push_str(vars.get(n).with_context(|| format!("{{{n}}} has no value; set it in the tool's vars or pass it to the run"))?);
                rest = &rest[n.len() + 2..];
            }
            None => {
                out.push('{');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// Case-insensitive glob over `/` paths: `*` stays inside one folder, `**` crosses folders.
fn glob(pattern: &str, path: &str) -> bool {
    fn m(p: &[u8], s: &[u8]) -> bool {
        match p {
            [] => s.is_empty(),
            [b'*', b'*', rest @ ..] => {
                let rest = rest.strip_prefix(b"/").unwrap_or(rest);
                (0..=s.len()).filter(|&i| i == 0 || i == s.len() || s[i - 1] == b'/').any(|i| m(rest, &s[i..]))
            }
            [b'*', rest @ ..] => (0..=s.len()).take_while(|&i| i == 0 || s[i - 1] != b'/').any(|i| m(rest, &s[i..])),
            [c, rest @ ..] => s.first() == Some(c) && m(rest, &s[1..]),
        }
    }
    m(path_key(pattern).as_bytes(), path_key(path).as_bytes())
}

/// Deployed hard links under Data that match the rewrite globs.
fn swaps(man: &BTreeMap<String, Deployed>, rewrites: &[String]) -> Vec<Deployed> {
    man.iter()
        .filter(|(k, e)| !e.copied && k.strip_prefix("data/").is_some_and(|rel| rewrites.iter().any(|g| glob(g, rel))))
        .map(|(_, e)| e.clone())
        .collect()
}

/// Replace a hard link with a copy of itself, so writes no longer reach the mod store.
fn swap_for_copy(path: &Path) -> Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".rmm-tmp");
    fs::copy(path, &tmp)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Game-relative key -> (path, size, mtime) for the whole game folder. Metadata comes from
/// opening each file: the directory listing's cached size and time can be stale for hard links.
fn snapshot(game: &Path) -> Result<BTreeMap<String, (String, u64, u64)>> {
    let mut snap = BTreeMap::new();
    for e in walkdir::WalkDir::new(game).min_depth(1) {
        let e = e?;
        if !e.file_type().is_file() {
            continue;
        }
        let rel = e.path().strip_prefix(game)?.to_string_lossy().replace('\\', "/");
        let (size, mtime) = deploy::state(e.path()).with_context(|| format!("cannot read {}", e.path().display()))?;
        snap.insert(path_key(&rel), (rel, size, mtime));
    }
    Ok(snap)
}

fn launch(inst: &Instance, tool: &Tool, args: &[String], log: &Path, timeout: Option<Duration>) -> Result<(Option<i32>, bool)> {
    fs::create_dir_all(log.parent().unwrap())?;
    let out = fs::File::create(log)?;
    let mut cmd = if proton::needed(&tool.exe) { proton::command(inst.config.game, &inst.config.game_path, &tool.exe)? } else { Command::new(&tool.exe) };
    cmd.args(args).stdin(Stdio::null()).stdout(out.try_clone()?).stderr(out);
    if let Some(dir) = tool.cwd.as_deref().or(tool.exe.parent()) {
        cmd.current_dir(dir);
    }
    let mut child = cmd.spawn().with_context(|| format!("cannot start {}", tool.exe.display()))?;
    // Waits for the direct child; launchers that exit early are covered by `wait_for`.
    let Some(limit) = timeout else { return Ok((child.wait()?.code(), false)) };
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok((status.code(), false));
        }
        if start.elapsed() >= limit {
            let _ = child.kill();
            let _ = child.wait();
            return Ok((None, true));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Wait for a program a launcher started. It shows up shortly after the launcher exits; returns
/// false if it never did. No timeout: the game can run for hours (grass precaching).
fn wait_for(game: &Path, exe: &str) -> bool {
    let start = Instant::now();
    while !deploy::game_running(game, exe) {
        if start.elapsed() > Duration::from_secs(15) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    while deploy::game_running(game, exe) {
        std::thread::sleep(Duration::from_secs(1));
    }
    true
}

fn tail(log: &Path) -> String {
    let text = String::from_utf8_lossy(&fs::read(log).unwrap_or_default()).into_owned();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(30)..].join("\n")
}

fn copy_file(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst.parent().unwrap())?;
    fs::copy(src, dst)?;
    Ok(())
}

/// Move a file (copying across drives) and remove folders it leaves empty, up to `stop`.
/// The final deploy recreates the ones it needs and records them, so purge removes them later.
fn move_file(src: &Path, dst: &Path, stop: &Path) -> Result<()> {
    fs::create_dir_all(dst.parent().unwrap())?;
    if fs::rename(src, dst).is_err() {
        fs::copy(src, dst)?;
        fs::remove_file(src)?;
    }
    for d in src.ancestors().skip(1).take_while(|d| d.starts_with(stop) && *d != stop) {
        if fs::remove_dir(d).is_err() {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_and_placeholders() {
        assert!(glob("CalienteTools/BodySlide/*.xml", "calientetools/bodyslide/Config.xml"));
        assert!(!glob("CalienteTools/BodySlide/*.xml", "calientetools/bodyslide/sub/Config.xml"));
        assert!(glob("meshes/**", "meshes/a/b.nif"));
        assert!(glob("**/*.hkx", "meshes/actors/x.hkx"));
        assert!(glob("a/**/b", "a/b") && glob("a/**/b", "a/x/y/b") && !glob("a/**/b", "a/xb"));
        assert!(glob("Update.esm", "update.esm") && !glob("Update.esm", "Update.esm.bak"));

        let vars = BTreeMap::from([("data".to_string(), r"C:\g\Data".to_string())]);
        assert_eq!(expand("-D:{data}", &vars).unwrap(), r"-D:C:\g\Data");
        assert_eq!(expand(r#"{"json": {data}}"#, &vars).unwrap(), r#"{"json": C:\g\Data}"#);
        assert!(expand("{missing}", &vars).is_err());
    }
}
