//! Crash logs (Crash Logger SSE, Buffout 4, Trainwreck, NetScriptFramework) and the Papyrus log,
//! read back to the mods that provide what they name.
use crate::{Game, Instance, path_key};
use anyhow::{Context, Result};
use regex::Regex;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::UNIX_EPOCH;

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct LogFile {
    pub path: PathBuf,
    /// Seconds since 1970.
    pub modified: u64,
}

#[derive(Serialize, Debug, Default)]
pub struct CrashReport {
    pub path: PathBuf,
    pub modified: u64,
    /// The logger's own line, e.g. "CrashLoggerSSE v1.19.0".
    pub logger: Option<String>,
    pub game_version: Option<String>,
    /// The "Unhandled exception ..." line.
    pub error: Option<String>,
    /// DLLs, plugins and asset files the log names, most likely culprit first, with the mod that
    /// provides each. A name in the call stack counts three times.
    pub suspects: Vec<Suspect>,
    /// The top of the call stack.
    pub call_stack: Vec<String>,
    /// Other crash logs, newest first.
    pub others: Vec<LogFile>,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Suspect {
    pub name: String,
    pub mod_id: Option<String>,
    pub score: usize,
    pub in_call_stack: bool,
}

/// Program files that show up in most crash logs and say nothing about the cause.
const NOISE: &[&str] = &[
    "skyrimse.exe", "fallout4.exe", "kernelbase.dll", "ntdll.dll", "kernel32.dll", "tbbmalloc.dll", "vcruntime140.dll",
    "vcruntime140_1.dll", "msvcp140.dll", "ucrtbase.dll", "d3d11.dll", "d3d12.dll", "dxgi.dll", "nvwgf2umx.dll",
    "nvgpucomp64.dll", "amdxx64.dll", "amdxc64.dll", "atidxx64.dll", "igd10iumd64.dll", "steam_api64.dll", "user32.dll",
    "win32u.dll", "combase.dll", "crashlogger.dll", "buffout4.dll", "addictol.dll", "xinput1_3.dll", "binkw64.dll",
    // Base-game masters own most records, so they are in nearly every log.
    "skyrim.esm", "update.esm", "dawnguard.esm", "hearthfires.esm", "dragonborn.esm", "fallout4.esm", "dlcrobot.esm",
    "dlcworkshop01.esm", "dlccoast.esm", "dlcworkshop02.esm", "dlcworkshop03.esm", "dlcnukaworld.esm", "dlcultrahighresolution.esm",
];
const ASSETS: &[&str] = &["nif", "dds", "hkx", "pex", "tri", "bgsm", "bgem", "wav", "xwm", "fuz", "swf", "bsa", "ba2", "seq"];

static DLL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)([\w.\-]+\.dll)\+").unwrap());
static QUOTED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""([^"\r\n]{3,260})""#).unwrap());

/// Folders the crash loggers write to, for this game.
fn crash_dirs(inst: &Instance) -> Vec<PathBuf> {
    let se = if inst.config.game.base() == Game::SkyrimSe { "SKSE" } else { "F4SE" };
    let mut dirs = Vec::new();
    if let Some(ini) = inst.ini_dir() {
        dirs.push(ini.join(se));
        dirs.push(ini.join(se).join("Crashlogs")); // Trainwreck
    }
    dirs.push(inst.data_dir().join("NetScriptFramework").join("Crash"));
    dirs
}

fn modified(p: &Path) -> Option<u64> {
    fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs())
}

/// Crash logs, newest first.
pub fn crash_logs(inst: &Instance) -> Vec<LogFile> {
    let mut out: Vec<LogFile> = crash_dirs(inst)
        .iter()
        .filter_map(|d| fs::read_dir(d).ok())
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_lowercase();
            n.starts_with("crash") && (n.ends_with(".log") || n.ends_with(".txt"))
        })
        .filter_map(|e| Some(LogFile { modified: modified(&e.path())?, path: e.path() }))
        .collect();
    out.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| b.path.cmp(&a.path)));
    out
}

/// Read a crash log (default: the newest) and rank what it names.
pub fn read_crash_log(inst: &Instance, path: Option<&Path>) -> Result<CrashReport> {
    let logs = crash_logs(inst);
    let path = match path {
        Some(p) => p.to_path_buf(),
        None => logs.first().map(|l| l.path.clone()).with_context(|| {
            format!("no crash logs in {}; Crash Logger SSE or Buffout 4 writes them when the game crashes", crash_dirs(inst).iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(", "))
        })?,
    };
    let text = String::from_utf8_lossy(&fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?).into_owned();
    let mut r = parse_crash(&text);
    r.modified = modified(&path).unwrap_or(0);
    r.others = logs.into_iter().filter(|l| l.path != path).take(10).collect();
    r.path = path;
    // Which mod provides each name.
    let profile = inst.profile()?;
    let mod_of = providers(inst, &profile)?;
    let plugins = crate::plugins::list(inst, &profile)?;
    let se = if inst.config.game.base() == Game::SkyrimSe { "SKSE" } else { "F4SE" };
    r.suspects.retain_mut(|s| {
        let lower = s.name.to_lowercase();
        if let Some(p) = plugins.iter().find(|p| p.name.eq_ignore_ascii_case(&s.name)) {
            s.mod_id = p.mod_id.clone();
            return !p.implicit; // base-game masters are in nearly every log
        }
        let keys = if lower.ends_with(".dll") { vec![format!("{se}/Plugins/{}", s.name), format!("root/{}", s.name)] } else { vec![s.name.replace('\\', "/")] };
        s.mod_id = keys.iter().find_map(|k| mod_of(k));
        s.mod_id.is_some() || !(NOISE.contains(&lower.as_str()) || lower.starts_with("skse64_") || lower.starts_with("f4se_"))
    });
    r.suspects.truncate(25);
    Ok(r)
}

/// Which mod provides a file: the enabled one that wins it, else any installed mod that has it
/// (a log often predates disabling the culprit).
fn providers(inst: &Instance, profile: &crate::instance::Profile) -> Result<impl Fn(&str) -> Option<String> + use<>> {
    let on = crate::conflicts::file_map(inst, profile)?.winners();
    let mut all = profile.clone();
    all.mods.iter_mut().for_each(|m| m.enabled = true);
    let any = crate::conflicts::file_map(inst, &all)?.winners();
    Ok(move |rel: &str| {
        let k = path_key(rel);
        on.get(&k).or_else(|| any.get(&k)).map(|(m, _)| m.clone())
    })
}

/// A section heading: unindented capitals ending in ':' ("PROBABLE CALL STACK:", "MODULES:"),
/// Crash Logger 1.20's "CALL STACK ([P]robable / [S]tack scan):", or "POSSIBLE RELEVANT OBJECTS".
fn heading(line: &str) -> Option<String> {
    if line.starts_with(char::is_whitespace) || line.starts_with('[') {
        return None;
    }
    let t = line.trim_end();
    let head = t.strip_suffix(':').or_else(|| t.starts_with("POSSIBLE RELEVANT OBJECTS").then_some(t))?;
    let words = head.split('(').next().unwrap_or_default().trim();
    let caps = words.chars().any(|c| c.is_ascii_uppercase()) && words.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == ' ' || c == '_' || c == '-');
    caps.then(|| words.to_string())
}

/// The header lines and the names a crash log mentions, before mapping them to mods.
pub fn parse_crash(text: &str) -> CrashReport {
    let mut r = CrashReport::default();
    let mut section = String::new();
    let mut hits: BTreeMap<String, (usize, bool)> = BTreeMap::new(); // lowercase -> (score, in stack)
    let mut spelled: HashMap<String, String> = HashMap::new();
    for (i, line) in text.lines().enumerate() {
        let l = line.trim();
        if i < 10 {
            if r.game_version.is_none() && (l.starts_with("Skyrim SSE v") || l.starts_with("Skyrim SE v") || l.starts_with("Fallout 4 v")) {
                r.game_version = l.split(" v").nth(1).map(|v| v.split_whitespace().next().unwrap_or(v).to_string());
            } else if r.logger.is_none() && ["CrashLoggerSSE", "Buffout 4", "Addictol", "Trainwreck", "NetScriptFramework"].iter().any(|n| l.contains(n)) {
                r.logger = Some(l.to_string());
            }
            if r.error.is_none() && l.starts_with("Unhandled exception") {
                r.error = Some(l.to_string());
            }
        }
        if let Some(h) = heading(line) {
            section = h;
            continue;
        }
        let stack = section.contains("CALL STACK");
        if stack && r.call_stack.len() < 15 && !l.is_empty() {
            r.call_stack.push(l.to_string());
        }
        if !(stack || matches!(section.as_str(), "REGISTERS" | "STACK" | "POSSIBLE RELEVANT OBJECTS")) {
            continue; // MODULES and PLUGINS list everything loaded
        }
        let dlls = DLL.captures_iter(l).map(|c| c[1].to_string());
        let quoted = QUOTED.captures_iter(l).map(|c| c[1].trim().to_string()).filter(|q| {
            let ext = q.rsplit('.').next().unwrap_or_default().to_lowercase();
            crate::install::is_plugin(q) || ASSETS.contains(&ext.as_str())
        });
        for name in dlls.chain(quoted) {
            let key = name.to_lowercase();
            let h = hits.entry(key.clone()).or_default();
            h.0 += if stack { 3 } else { 1 };
            h.1 |= stack;
            spelled.entry(key).or_insert(name);
        }
    }
    r.suspects = hits.into_iter().map(|(k, (score, in_call_stack))| Suspect { name: spelled.remove(&k).unwrap_or(k), mod_id: None, score, in_call_stack }).collect();
    r.suspects.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.name.cmp(&b.name)));
    r
}

#[derive(Serialize, Debug, Default)]
pub struct PapyrusReport {
    pub path: PathBuf,
    pub modified: Option<u64>,
    /// `[Papyrus] bEnableLogging=1` in the game's INI files; without it the game writes no log.
    pub logging_enabled: bool,
    pub errors: usize,
    pub warnings: usize,
    /// Distinct error messages, most frequent first (form ids folded into "…").
    pub messages: Vec<(String, usize)>,
    /// Scripts in errors' stacks, most frequent first, with the mod that provides each.
    pub scripts: Vec<ScriptHits>,
    /// The last lines of the log.
    pub tail: Vec<String>,
}

#[derive(Serialize, Debug, PartialEq)]
pub struct ScriptHits {
    pub script: String,
    pub mod_id: Option<String>,
    pub count: usize,
}

static FORM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\([0-9A-Fa-f]{8}\)|0x[0-9A-Fa-f]+").unwrap());
static SCRIPT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\]\.([A-Za-z0-9_:]+)\.[A-Za-z0-9_]+\(").unwrap());

/// Summarise `Logs/Script/Papyrus.0.log`: errors grouped by message, scripts by mod, and the tail.
pub fn read_papyrus_log(inst: &Instance, tail: usize) -> Result<PapyrusReport> {
    let dir = inst.ini_dir().context("can't find your Documents folder, where the game keeps its logs")?;
    let get = |key: &str| {
        let read = |name: &str| inst.ini_file(name).and_then(|p| fs::read_to_string(p).ok()).unwrap_or_default();
        let (custom, base) = (read(&inst.config.game.ini_name().replace(".ini", "Custom.ini")), read(inst.config.game.ini_name()));
        crate::health::ini_get(&custom, "Papyrus", key).or_else(|| crate::health::ini_get(&base, "Papyrus", key))
    };
    let path = dir.join("Logs").join("Script").join("Papyrus.0.log");
    let mut r = PapyrusReport { logging_enabled: get("bEnableLogging").as_deref() == Some("1"), modified: modified(&path), ..Default::default() };
    let text = fs::read(&path).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
    r.path = path;
    let (mut messages, mut scripts) = (HashMap::<String, usize>::new(), HashMap::<String, usize>::new());
    let mut in_stack = false;
    for line in text.lines() {
        if let Some((_, msg)) = line.split_once("] error: ") {
            r.errors += 1;
            *messages.entry(FORM.replace_all(msg.trim(), "…").chars().take(200).collect()).or_default() += 1;
            in_stack = true;
        } else if line.contains("] warning: ") {
            r.warnings += 1;
            in_stack = false;
        } else if line.starts_with('[') {
            in_stack = false;
        } else if in_stack && let Some(c) = SCRIPT.captures(line) {
            *scripts.entry(c[1].to_string()).or_default() += 1;
        }
    }
    let mut messages: Vec<_> = messages.into_iter().collect();
    messages.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    messages.truncate(30);
    r.messages = messages;
    let mod_of = providers(inst, &inst.profile()?)?;
    let mut scripts: Vec<ScriptHits> = scripts
        .into_iter()
        .map(|(script, count)| {
            let mod_id = mod_of(&format!("scripts/{}.pex", script.replace(':', "/")));
            ScriptHits { script, mod_id, count }
        })
        .collect();
    scripts.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.script.cmp(&b.script)));
    scripts.truncate(30);
    r.scripts = scripts;
    let lines: Vec<&str> = text.lines().collect();
    r.tail = lines[lines.len().saturating_sub(tail)..].iter().map(|l| l.to_string()).collect();
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crash_logger_and_buffout_layouts() {
        let cl = r#"Skyrim SSE v1.6.1170
CrashLoggerSSE v1-19-0-0 Nov 10 2025 12:00:00

Unhandled exception "EXCEPTION_ACCESS_VIOLATION" at 0x7FF6A1B2C3D4 SkyrimSE.exe+0123456

SYSTEM SPECS:
	OS: Microsoft Windows 11 Pro v10.0.26200

CALL STACK ([P]robable / [S]tack scan):
	[ 0] 0x7FF6A1B2C3D4 SkyrimSE.exe+0123456
	[ 1] 0x7FFA11223344 hdtSMP64.dll+00A1B2C
	[ 2] 0x7FFA11223355 hdtSMP64.dll+00A1B99

REGISTERS:
	RAX 0x0 (NULL)
	RCX 0x1A2B (TESObjectREFR*) File: "Skyrim.esm" Modified by: "Update.esm" -> "Cool Armor.esp" Model: "meshes\armor\cool\cuirass.nif"

MODULES:
	EngineFixes.dll 0x7FFA00000000
	hdtSMP64.dll 0x7FFA11220000

PLUGINS:
	[ 0] Skyrim.esm
	[FE:  0] Unrelated.esl
"#;
        let r = parse_crash(cl);
        assert_eq!(r.game_version.as_deref(), Some("1.6.1170"));
        assert!(r.logger.as_deref().unwrap().starts_with("CrashLoggerSSE"));
        assert!(r.error.as_deref().unwrap().contains("EXCEPTION_ACCESS_VIOLATION"));
        assert_eq!(r.call_stack.len(), 3);
        let names: Vec<(&str, usize, bool)> = r.suspects.iter().map(|s| (s.name.as_str(), s.score, s.in_call_stack)).collect();
        assert_eq!(names[0], ("hdtSMP64.dll", 6, true));
        assert!(names.contains(&("Cool Armor.esp", 1, false)) && names.contains(&("meshes\\armor\\cool\\cuirass.nif", 1, false)));
        assert!(!names.iter().any(|n| n.0 == "EngineFixes.dll" || n.0 == "Unrelated.esl"), "MODULES and PLUGINS are lists, not evidence");

        let b4 = "Fallout 4 v1.10.163\nBuffout 4 v1.28.6\n\nUnhandled exception \"EXCEPTION_ACCESS_VIOLATION\" at 0x7FF6\n\nPROBABLE CALL STACK:\n\t[0] 0x7FF6 f4ee.dll+0012345\n";
        let r = parse_crash(b4);
        assert_eq!((r.game_version.as_deref(), r.suspects[0].name.as_str()), (Some("1.10.163"), "f4ee.dll"));
        assert!(r.logger.unwrap().starts_with("Buffout 4"));
    }

    #[test]
    fn headings() {
        assert_eq!(heading("PROBABLE CALL STACK:").as_deref(), Some("PROBABLE CALL STACK"));
        assert_eq!(heading("CALL STACK ([P]robable / [S]tack scan):").as_deref(), Some("CALL STACK"));
        assert_eq!(heading("POSSIBLE RELEVANT OBJECTS (3)").as_deref(), Some("POSSIBLE RELEVANT OBJECTS"));
        assert_eq!(heading("\tRAX 0x0:"), None);
        assert_eq!(heading("Unhandled exception at 0x1:"), None);
    }
}
