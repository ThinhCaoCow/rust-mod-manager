//! Help for asking others about a crash: a text report to paste into a post, and a support
//! bundle (a zip of the setup's state and logs) to attach. Both leave out the Windows user name.
use crate::{Instance, logs};
use anyhow::Result;
use regex::Regex;
use serde::Serialize;
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// Replace the user's name in paths (`C:\Users\<name>\...`) with `<user>`.
pub fn scrub(text: &str) -> String {
    static RE: LazyLock<Option<Regex>> = LazyLock::new(|| user_pattern(&user_names()));
    match &*RE {
        Some(re) => re.replace_all(text, "${1}<user>").into_owned(),
        None => text.to_string(),
    }
}

fn user_names() -> Vec<String> {
    let mut names: Vec<String> = ["USERNAME", "USER"].iter().filter_map(|v| std::env::var(v).ok()).collect();
    names.extend(["USERPROFILE", "HOME"].iter().filter_map(|v| std::env::var(v).ok()).filter_map(|p| Some(Path::new(&p).file_name()?.to_string_lossy().into_owned())));
    names.retain(|n| !n.trim().is_empty());
    names.sort();
    names.dedup();
    names
}

fn user_pattern(names: &[String]) -> Option<Regex> {
    let alt = names.iter().map(|n| regex::escape(n)).collect::<Vec<_>>().join("|");
    (!names.is_empty()).then(|| Regex::new(&format!(r"(?i)([\\/](?:users|home)[\\/]+)(?:{alt})\b")).ok()).flatten()
}

fn game_line(inst: &Instance) -> String {
    let game = serde_json::to_value(inst.config.game).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
    match crate::launch::game_version(&inst.config.game_path, inst.config.game) {
        Ok(v) => format!("{game} {}.{}.{}.{}", v[0], v[1], v[2], v[3]),
        Err(_) => game,
    }
}

/// The setup in one line: game and version, profile, mods and plugins enabled.
fn setup_line(inst: &Instance) -> String {
    let Ok(p) = inst.profile() else { return game_line(inst) };
    let mods = p.mods.iter().filter(|m| !crate::instance::is_separator(&m.id));
    let (on, all) = (mods.clone().filter(|m| m.enabled).count(), mods.count());
    let plugins = p.plugins.iter().filter(|e| e.enabled).count();
    format!("{}, profile {}: {on} of {all} mods and {plugins} plugins enabled", game_line(inst), inst.config.profile)
}

/// A crash report as text to paste into a forum post or a chat.
pub fn crash_text(inst: &Instance, r: &logs::CrashReport) -> String {
    let mut t = String::new();
    let _ = writeln!(t, "Crash report from rmm {}", env!("CARGO_PKG_VERSION"));
    let _ = writeln!(t, "Setup: {}", setup_line(inst));
    let file = r.path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
    let logger = r.logger.as_ref().map(|l| format!(", {l}")).unwrap_or_default();
    let _ = writeln!(t, "Crash log: {file}{logger}{}", r.game_version.as_ref().map(|v| format!(", game {v}")).unwrap_or_default());
    if let Some(e) = &r.error {
        let _ = writeln!(t, "{e}");
    }
    let enabled = |id: &str| inst.profile().is_ok_and(|p| p.mods.iter().any(|m| m.id == id && m.enabled));
    if !r.suspects.is_empty() {
        let _ = writeln!(t, "\nNamed in the log, most likely first:");
        for s in r.suspects.iter().take(10) {
            let from = match &s.mod_id {
                Some(id) => format!("{}{}", inst.mod_meta(id).name, if enabled(id) { "" } else { " (disabled now)" }),
                None => "not from a mod".into(),
            };
            let _ = writeln!(t, "- {}{}: {from}", s.name, if s.in_call_stack { " (call stack)" } else { "" });
        }
    }
    if !r.call_stack.is_empty() {
        let _ = writeln!(t, "\nCall stack (top):");
        for l in r.call_stack.iter().take(8) {
            let _ = writeln!(t, "    {l}");
        }
    }
    if !r.next_steps.is_empty() {
        let _ = writeln!(t, "\nWhat rmm suggests:");
        for s in &r.next_steps {
            let _ = writeln!(t, "- {s}");
        }
    }
    scrub(&t)
}

#[derive(Serialize, Debug)]
pub struct Bundle {
    pub path: PathBuf,
    /// The files in it.
    pub files: Vec<String>,
    /// Parts that couldn't be added, and why.
    pub skipped: Vec<String>,
}

/// The last `max` bytes of a log.
fn tail_of(path: &Path, max: usize) -> Option<String> {
    let b = fs::read(path).ok()?;
    let cut = b.len().saturating_sub(max);
    Some(String::from_utf8_lossy(&b[cut..]).into_owned())
}

/// Save a support bundle: a summary, the health check, the lockfile, the deploy plan, the crash,
/// Papyrus and script extender logs, and rmm's settings, all without the Windows user name.
/// `crash` picks a crash log (default: the newest, if any). The zip, `rmm-support-<time>.zip`,
/// goes in `dir` (default: `support/` in the instance).
pub fn bundle(inst: &Instance, dir: Option<&Path>, crash: Option<&Path>) -> Result<Bundle> {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let path = dir.map(Path::to_path_buf).unwrap_or_else(|| inst.root.join("support")).join(format!("rmm-support-{secs}.zip"));
    let mut parts: Vec<(String, String)> = Vec::new();
    let mut skipped = Vec::new();
    let mut summary = format!("# rmm support bundle\n\nrmm {}. {}.\n", env!("CARGO_PKG_VERSION"), setup_line(inst));

    match logs::read_crash_log(inst, crash) {
        Ok(r) => {
            let _ = write!(summary, "\n## Crash\n\n{}", crash_text(inst, &r));
            if let Ok(text) = fs::read(&r.path) {
                parts.push((format!("logs/{}", r.path.file_name().unwrap_or_default().to_string_lossy()), String::from_utf8_lossy(&text).into_owned()));
            }
        }
        Err(e) if crash.is_some() => return Err(e),
        Err(_) => summary.push_str("\n## Crash\n\nNo crash log.\n"),
    }
    match crate::health::check(inst) {
        Ok(issues) => {
            summary.push_str("\n## Problems\n\n");
            for i in issues.iter().filter(|i| i.severity != "info") {
                let _ = writeln!(summary, "- {} {}: {}", i.severity, i.subject, i.message);
            }
            if issues.iter().all(|i| i.severity == "info") {
                summary.push_str("None.\n");
            }
            parts.push(("health.json".into(), serde_json::to_string_pretty(&issues)?));
        }
        Err(e) => skipped.push(format!("the health check: {e:#}")),
    }
    match logs::script_extender_log(inst, true) {
        Ok(se) if se.path.is_file() => {
            let failed: Vec<String> = se.plugins.iter().filter(|p| !p.loaded).map(|p| format!("- {}: {}", p.file, p.problem.as_deref().unwrap_or("didn't load"))).collect();
            if !failed.is_empty() {
                let _ = write!(summary, "\n## Script extender plugins that didn't load\n\n{}\n", failed.join("\n"));
            }
            if let Some(text) = tail_of(&se.path, 1 << 20) {
                parts.push((format!("logs/{}", se.path.file_name().unwrap_or_default().to_string_lossy()), text));
            }
        }
        Ok(_) => {}
        Err(e) => skipped.push(format!("the script extender log: {e:#}")),
    }
    if let Some(text) = inst.ini_dir().and_then(|d| tail_of(&d.join("Logs").join("Script").join("Papyrus.0.log"), 1 << 20)) {
        parts.push(("logs/Papyrus.0.log".into(), text));
    }
    match crate::lockfile::export(inst).and_then(|l| Ok(toml::to_string_pretty(&l)?)) {
        Ok(t) => parts.push(("modlist.lock.toml".into(), t)),
        Err(e) => skipped.push(format!("the lockfile: {e:#}")),
    }
    // The file lists can run to hundreds of thousands on a first deploy: the first 200 of each.
    match crate::deploy::deploy(inst, true) {
        Ok(plan) => {
            let mut v = serde_json::to_value(&plan)?;
            for k in ["link", "remove", "backup", "external_changes", "played"] {
                if let Some(list) = v.get_mut(k).and_then(|l| l.as_array_mut()) {
                    let n = list.len();
                    list.truncate(200);
                    v[format!("{k}_count")] = n.into();
                }
            }
            parts.push(("deploy-plan.json".into(), serde_json::to_string_pretty(&v)?));
        }
        Err(e) => skipped.push(format!("the deploy plan: {e:#}")),
    }
    // rmm's settings hold no keys; the AI role notes are the user's own text, not setup.
    let mut settings = serde_json::to_value(crate::settings::get())?;
    if let Some(ai) = settings.get_mut("ai").and_then(|a| a.as_object_mut()) {
        ai.remove("role_instructions");
    }
    parts.push(("settings.json".into(), serde_json::to_string_pretty(&settings)?));
    if !skipped.is_empty() {
        let _ = write!(summary, "\n## Left out\n\n{}\n", skipped.iter().map(|s| format!("- {s}")).collect::<Vec<_>>().join("\n"));
    }
    parts.insert(0, ("summary.md".into(), summary));

    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut zip = zip::ZipWriter::new(fs::File::create(&path)?);
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, text) in &parts {
        zip.start_file(name.as_str(), opts)?;
        zip.write_all(scrub(text).as_bytes())?;
    }
    zip.finish()?;
    Ok(Bundle { path, files: parts.into_iter().map(|(n, _)| n).collect(), skipped })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrubs_the_user_name_from_paths() {
        let re = user_pattern(&["Robin".into(), "rob".into()]).unwrap();
        let s = |t: &str| re.replace_all(t, "${1}<user>").into_owned();
        assert_eq!(s(r"C:\Users\robin\Documents\My Games"), r"C:\Users\<user>\Documents\My Games");
        assert_eq!(s("c:/users/ROBIN/OneDrive/x and /home/rob/y"), "c:/users/<user>/OneDrive/x and /home/<user>/y");
        assert_eq!(s(r"C:\Users\robinx\a, Robin says"), r"C:\Users\robinx\a, Robin says", "only whole names, only in paths");
        assert!(user_pattern(&[]).is_none());
    }
}
