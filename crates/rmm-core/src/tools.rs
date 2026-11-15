//! Tool runs (SPEC.md "Tools"): launch xEdit, BodySlide, Synthesis or any other program against
//! the deployed setup, then capture what it wrote into a generated mod.
//!
//! A run disables the tool's previous output and deploys, so the tool sees the current setup.
//! Deployed files the tool rewrites in place get their hard links swapped for copies, so the mod
//! store isn't written through. rmm snapshots Data, runs the tool and diffs Data. The generated
//! mod is built from the staging folder (`{output}`) and the diff. It replaces the previous output
//! (which goes to trash/ so undo restores it), and a final deploy links it in.
use crate::deploy::{self, Deployed, DeployPlan};
use crate::instance::{ModMeta, index_dir, write_atomic};
use crate::install::{is_plugin, sanitize, unique_id};
use crate::{Game, Instance, ModEntry, PluginEntry, ops, path_key};
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
}

#[derive(Debug, Default, Serialize)]
pub struct RunReport {
    pub tool: String,
    pub dry_run: bool,
    /// Executable followed by its arguments, placeholders filled in.
    pub command: Vec<String>,
    /// Name of the generated mod this run fills.
    pub output_mod: String,
    /// The tool's previous generated mod: disabled for the run, then replaced.
    pub previous: Option<String>,
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
    inst.locked(|| {
        let mut tools = list(inst)?;
        tools.insert(id.into(), tool.clone());
        write_atomic(&inst.root.join(TOOLS), &toml::to_string_pretty(&tools)?)
    })?;
    Ok(tool)
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
    let (switch, xedit) = match game {
        Game::SkyrimSe => ("-SSE", "SSEEdit"),
        Game::Fallout4 => ("-FO4", "FO4Edit"),
    };
    // xEdit only finds Steam installs by itself, and keeps backups and its cache inside Data.
    // -C: gets no trailing separator from xEdit, so it is written here.
    let xedit_args = [switch, "-D:{data}", "-P:{plugins}", "-M:{ini_dir}", "-I:{ini}", r"-B:{state}\backups", r"-C:{state}\cache\"];
    // DynDOLOD's tools (xEdit-based, case-insensitive switches) want a trailing backslash on folders.
    let lod_args = [switch, r"-o:{output}\", r"-d:{data}\", "-p:{plugins}", r"-m:{ini_dir}\"];
    let tool = |name: &str, args: Vec<String>| Tool { name: name.into(), args, ..Default::default() };
    let settings = |file: &str, set: &[(&str, Value)]| Some(Settings { file: file.into(), set: set.iter().map(|(k, v)| (k.to_string(), v.clone())).collect() });
    let skyrim = game == Game::SkyrimSe;
    Some(match name {
        // Saves by writing a new file and renaming it over the plugin, so links are safe.
        "xedit" => tool(xedit, s(&xedit_args)),
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
        // -o is never read; paths come from Settings.json, and outputPath must exist or output
        // silently goes to Data. Its mod list lives in the output folder, so it carries over.
        "pandora" if skyrim => Tool {
            settings: settings("Settings.json", &[("games.SkyrimSE.gameDataPath", "{data}".into()), ("games.SkyrimSE.outputPath", "{output}".into())]),
            keep: s(&["Pandora_Engine/ActiveMods.json"]),
            ..tool("Pandora", s(&["--auto_run", "--auto_close"]))
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

/// Run a tool. With `dry_run` nothing happens: the report shows the command, the generated mod
/// it would replace, the links it would swap and the deploy that runs first.
pub fn run(inst: &Instance, id: &str, given: &BTreeMap<String, String>, dry_run: bool, timeout: Option<Duration>) -> Result<RunReport> {
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
    if cfg!(not(windows)) && tool.exe.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe")) {
        bail!("running Windows tools through Proton isn't supported yet");
    }
    let source = format!("tool:{id}");
    let previous = inst.profile()?.mods.into_iter().find(|m| {
        let meta = inst.mod_meta(&m.id);
        meta.source.as_deref() == Some(source.as_str()) && meta.name == output_mod
    });
    let mut r = RunReport {
        tool: id.into(),
        dry_run,
        command: std::iter::once(tool.exe.display().to_string()).chain(args.iter().cloned()).collect(),
        output_mod: output_mod.clone(),
        previous: previous.as_ref().map(|m| m.id.clone()),
        ..Default::default()
    };
    let disable_previous = previous.as_ref().filter(|m| m.enabled).map(|m| m.id.clone());
    r.settings_file = tool.settings.as_ref().map(|s| tool.exe.parent().unwrap_or(Path::new("")).join(&s.file));

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

    let game = &inst.config.game_path;
    let data = inst.data_dir();
    let stash = state.join("stash");
    let _ = fs::remove_dir_all(&stash);
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;
    let kept = |f: &str| tool.keep.iter().any(|g| glob(g, f));
    if let Some(old) = &previous {
        let dir = inst.mod_dir(&old.id);
        for f in index_dir(&dir)?.into_iter().filter(|f| kept(f)) {
            copy_file(&dir.join(&f), &staging.join(&f))?;
        }
    }
    if let (Some(s), Some(path)) = (&tool.settings, &r.settings_file) {
        merge_settings(path, &s.set, &vars)?;
    }
    let man = deploy::manifest_files(inst)?;
    let swapped = swaps(&man, &rewrites);
    for e in &swapped {
        swap_for_copy(&game.join(&e.target))?;
    }
    r.swapped = swapped.iter().map(|e| e.target.clone()).collect();
    let swapped: HashSet<String> = swapped.iter().map(|e| path_key(&e.target)).collect();
    let before = snapshot(&data)?;
    for (key, (rel, ..)) in &before {
        if !man.contains_key(&format!("data/{key}")) && rewrites.iter().any(|g| glob(g, key)) {
            copy_file(&data.join(rel), &stash.join(rel))?;
        }
    }

    let log = state.join("last-run.log");
    match launch(tool, &args, &log, timeout) {
        Ok((code, timed_out)) => (r.exit_code, r.timed_out) = (code, timed_out),
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

    let after = snapshot(&data)?;
    let mut added_plugins = false;
    let new_id = unique_id(inst, &sanitize(&output_mod));
    let dest = inst.mod_dir(&new_id);
    fs::rename(&staging, &dest)?;
    for (key, (rel, size, mtime)) in &after {
        let path = data.join(rel);
        let mkey = format!("data/{key}");
        match before.get(key) {
            Some((_, s, m)) if (s, m) == (size, mtime) => {
                if swapped.contains(&mkey) {
                    fs::remove_file(&path)?; // untouched copy: the final deploy relinks it
                }
            }
            Some(_) => {
                r.changed.push(rel.clone());
                match man.get(&mkey) {
                    Some(e) if !e.copied && same_file::is_same_file(&path, inst.mod_dir(&e.mod_id).join(&e.src)).unwrap_or(false) => {
                        r.modified_in_place.push(format!("{}: {}", e.mod_id, e.src));
                        copy_file(&path, &dest.join(rel))?;
                    }
                    Some(_) => move_file(&path, &dest.join(rel), &data)?,
                    None if stash.join(rel).is_file() => {
                        move_file(&path, &dest.join(rel), &data)?;
                        move_file(&stash.join(rel), &path, &data)?;
                    }
                    None => {
                        copy_file(&path, &dest.join(rel))?;
                        r.warnings.push(format!("{rel} changed but isn't in the tool's rewrites, so its original wasn't kept; purge leaves the tool's version"));
                    }
                }
            }
            None => {
                r.new.push(rel.clone());
                move_file(&path, &dest.join(rel), &data)?;
            }
        }
    }
    for (key, (rel, ..)) in &before {
        if after.contains_key(key) {
            continue;
        }
        r.deleted.push(rel.clone());
        if man.contains_key(&format!("data/{key}")) {
            continue; // the final deploy relinks it
        }
        if stash.join(rel).is_file() {
            move_file(&stash.join(rel), &data.join(rel), &data)?;
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
    r.files = files.len();
    if files.iter().all(|f| kept(f)) {
        fs::remove_dir_all(&dest)?;
        r.warnings.push("the tool wrote nothing to Data or {output}".into());
        if let Some(p) = &disable_previous {
            ops::set_mod_enabled(inst, p, true)?;
        }
    } else {
        inst.write_mod_meta(&new_id, &ModMeta { name: output_mod, source: Some(source), fomod: None })?;
        let plugins: Vec<String> = files.iter().filter(|f| !f.contains('/') && is_plugin(f)).cloned().collect();
        let known = inst.profile()?.plugins;
        added_plugins = plugins.iter().any(|pl| !known.iter().any(|e| e.name.eq_ignore_ascii_case(pl)));
        inst.edit(&format!("run {id}"), |p| {
            let entry = ModEntry { id: new_id.clone(), enabled: true };
            match previous.as_ref().and_then(|old| p.mods.iter().position(|m| m.id == old.id)) {
                Some(i) => p.mods[i] = entry,
                None => p.mods.push(entry),
            }
            for pl in &plugins {
                match p.plugins.iter_mut().find(|e| e.name.eq_ignore_ascii_case(pl)) {
                    Some(e) => e.enabled = true,
                    None => p.plugins.push(PluginEntry { name: pl.clone(), enabled: true }),
                }
            }
            Ok(())
        })?;
        if let Some(old) = &previous {
            inst.trash_mod(&old.id)?;
        }
        r.mod_id = Some(new_id);
    }
    r.deploy = Some(deploy::deploy(inst, false)?);
    // New plugins land at the end of the list. Sort them in if the user sorts with LOOT already;
    // the first sort, with its download, stays the user's call.
    if added_plugins && inst.root.join("loot").join("masterlist.yaml").is_file() {
        match crate::loot::sort(inst, false) {
            Ok(s) => {
                if !s.moved.is_empty() {
                    crate::plugins::write_plugins_txt(inst, &inst.profile()?)?;
                }
                r.sort = Some(s);
            }
            Err(e) => r.warnings.push(format!("LOOT couldn't sort the new plugins: {e:#}")),
        }
    }
    for v in [&mut r.new, &mut r.changed, &mut r.deleted] {
        if v.len() > LIST_CAP {
            let more = v.len() - LIST_CAP;
            v.truncate(LIST_CAP);
            v.push(format!("... and {more} more"));
        }
    }
    Ok(r)
}

fn vars(inst: &Instance, tool: &Tool, state: &Path, staging: &Path, given: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    // Windows tools don't all accept '/', which paths typed into the CLI often contain.
    let p = |x: &Path| if cfg!(windows) { x.display().to_string().replace('/', "\\") } else { x.display().to_string() };
    let mut v = BTreeMap::from([
        ("game".to_string(), p(&inst.config.game_path)),
        ("data".into(), p(&inst.data_dir())),
        ("plugins".into(), p(&inst.config.local_path.join("Plugins.txt"))),
        ("output".into(), p(staging)),
        ("state".into(), p(state)),
    ]);
    if let Some(d) = inst.ini_dir() {
        v.insert("ini".into(), p(&d.join(inst.config.game.ini_name())));
        v.insert("ini_dir".into(), p(&d));
    }
    v.extend(tool.vars.clone());
    v.extend(given.clone());
    v
}

/// Set dotted keys in a JSON settings file, keeping the rest of it.
fn merge_settings(path: &Path, set: &BTreeMap<String, Value>, vars: &BTreeMap<String, String>) -> Result<()> {
    let mut root = match fs::read(path) {
        // .NET tools may write a byte-order mark.
        Ok(b) => serde_json::from_slice(b.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&b)).with_context(|| format!("cannot parse {}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Object(Default::default()),
        Err(e) => return Err(e.into()),
    };
    for (key, v) in set {
        let mut node = &mut root;
        for part in key.split('.') {
            if !node.is_object() {
                *node = Value::Object(Default::default());
            }
            node = node.as_object_mut().unwrap().entry(part).or_insert(Value::Null);
        }
        *node = match v {
            Value::String(t) => Value::String(expand(t, vars)?),
            v => v.clone(),
        };
    }
    fs::create_dir_all(path.parent().unwrap())?;
    write_atomic(path, &serde_json::to_string_pretty(&root)?)
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

/// Data-relative key -> (path, size, mtime). Metadata comes from opening each file: the
/// directory listing's cached size and time can be stale for hard links.
// ponytail: Data only; files tools write to the game root (ENB, grass trigger) aren't captured yet.
fn snapshot(data: &Path) -> Result<BTreeMap<String, (String, u64, u64)>> {
    let mut snap = BTreeMap::new();
    if !data.is_dir() {
        return Ok(snap);
    }
    for e in walkdir::WalkDir::new(data).min_depth(1) {
        let e = e?;
        if !e.file_type().is_file() {
            continue;
        }
        let rel = e.path().strip_prefix(data)?.to_string_lossy().replace('\\', "/");
        let (size, mtime) = deploy::state(e.path()).with_context(|| format!("cannot read {}", e.path().display()))?;
        snap.insert(path_key(&rel), (rel, size, mtime));
    }
    Ok(snap)
}

fn launch(tool: &Tool, args: &[String], log: &Path, timeout: Option<Duration>) -> Result<(Option<i32>, bool)> {
    fs::create_dir_all(log.parent().unwrap())?;
    let out = fs::File::create(log)?;
    let mut cmd = Command::new(&tool.exe);
    cmd.args(args).stdin(Stdio::null()).stdout(out.try_clone()?).stderr(out);
    if let Some(dir) = tool.cwd.as_deref().or(tool.exe.parent()) {
        cmd.current_dir(dir);
    }
    let mut child = cmd.spawn().with_context(|| format!("cannot start {}", tool.exe.display()))?;
    // ponytail: waits for the direct child only. Launchers that start the real program and exit
    // (skse64_loader, ckpe_loader) need a job object or a process search.
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
