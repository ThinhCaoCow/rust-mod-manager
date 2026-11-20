//! LOOT sorting and messages through libloot (SPEC.md "LOOT"). The masterlist and prelude are
//! cached in `<instance>/loot/` and refreshed from LOOT's GitHub repositories at most once a day;
//! `loot/userlist.yaml`, if present, adds the user's own rules; `set_rule` edits it.
use crate::health::Issue;
use crate::instance::OVERWRITE;
use crate::plugins::PluginInfo;
use crate::{Game, Instance, PluginEntry, Profile, plugins};
use anyhow::{Context, Result, bail};
use libloot::metadata::{File, MessageType, PluginMetadata, select_message_content};
use libloot::{EvalMode, GameType, MergeMode, MetadataWriteOptions};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Masterlist branch matching libloot 0.29's metadata syntax.
const BRANCH: &str = "v0.29";
/// How old the masterlist may get (Settings).
fn refresh() -> Duration {
    Duration::from_secs(crate::settings::get().loot.refresh_hours * 3600)
}

#[derive(Debug, Serialize)]
pub struct SortReport {
    pub dry_run: bool,
    /// Every plugin in LOOT's order.
    pub order: Vec<String>,
    /// The plugins that really move: the fewest whose moving explains the new order (the rest
    /// keep their order among themselves, even when their numbers shift), with old and new index.
    pub moved: Vec<Moved>,
    /// Plugins on in the profile but off in the game folder's plugins.txt, or the other way round.
    /// LOOT reads which plugins are on from there, so a deploy first lets its conditions see the
    /// profile.
    pub undeployed: usize,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Moved {
    pub name: String,
    pub from: usize,
    pub to: usize,
    /// The plugin it then loads right after; none when it goes first.
    pub after: Option<String>,
    /// Why it moves, in LOOT's rules: a plugin it now loads before or after, then each rule on
    /// the way there.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub why: Vec<String>,
}

/// One of LOOT's rules in a sort: `from` loads before `to`. `kind` is libloot's name for it
/// ("Master", "User Load After", "Record Overlap"…).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub kind: String,
}

/// The rule in plain words.
fn describe(e: &Edge) -> String {
    let (from, to) = (&e.from, &e.to);
    if !crate::install::is_plugin(from) {
        // A cycle among groups: the vertices are group names.
        let whose = if e.kind.starts_with("User") { "your group settings" } else { "LOOT's masterlist" };
        return format!("group {from} comes before group {to} ({whose})");
    }
    let why = match e.kind.as_str() {
        "Hardcoded" => "the game loads it early, in a fixed order".to_string(),
        "Master Flag" => "it is a master file (ESM flag), and those load before other plugins".into(),
        "Master" => format!("it is one of {to}'s masters"),
        "Masterlist Requirement" => format!("LOOT's masterlist says {to} needs it"),
        "User Requirement" => format!("your LOOT rule says {to} needs it"),
        "Masterlist Load After" => format!("LOOT's masterlist says {to} loads after it"),
        "User Load After" => format!("your LOOT rule says {to} loads after it"),
        "Masterlist Group" => format!("its LOOT group comes before {to}'s"),
        "User Group" => format!("its LOOT group comes before {to}'s, a group you set"),
        "Record Overlap" => format!("both change some of the same records and it changes more records overall, so it loads first and {to}'s changes win"),
        "Asset Overlap" => format!("their archives hold some of the same files and its archive holds more, so it loads first and {to}'s files win"),
        "Tie Break" => format!("it was already before {to}"),
        "Blueprint Master" => format!("{to} is a blueprint master, and those load last"),
        other => format!("LOOT's \"{other}\" rule"),
    };
    format!("{from} loads before {to}: {why}.")
}

/// The rules LOOT used in one sort, read from libloot's log ("Adding Master edge from "A" to
/// "B"."). libloot has no other way to hand them out; the flow test `loot_sorts_and_reports` breaks if the wording
/// changes, and Cargo.toml pins the version.
#[derive(Default)]
struct Graph {
    edges: Vec<Edge>,
}

static EDGES: std::sync::Mutex<Option<Vec<Edge>>> = std::sync::Mutex::new(None);
/// One sort that reads the rules at a time: libloot's log is the whole program's.
static READING: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn parse_edge(line: &str) -> Option<Edge> {
    let rest = line.strip_prefix("Adding ")?.strip_suffix("\".")?;
    let (kind, rest) = rest.split_once(" edge from \"")?;
    let (from, to) = rest.split_once("\" to \"")?;
    Some(Edge { from: from.into(), to: to.into(), kind: kind.into() })
}

/// Sort, keeping the rules LOOT added on the way.
fn sort_reading(game: &libloot::Game, names: &[&str]) -> (Result<Vec<String>, libloot::error::SortPluginsError>, Graph) {
    static HOOKED: std::sync::Once = std::sync::Once::new();
    HOOKED.call_once(|| {
        libloot::set_logging_callback(|_, line| {
            if let Some(e) = parse_edge(line)
                && let Some(v) = EDGES.lock().unwrap_or_else(|p| p.into_inner()).as_mut()
            {
                v.push(e);
            }
        })
    });
    let _one = READING.lock().unwrap_or_else(|p| p.into_inner());
    *EDGES.lock().unwrap_or_else(|p| p.into_inner()) = Some(Vec::new());
    let result = game.sort_plugins(names);
    let edges = EDGES.lock().unwrap_or_else(|p| p.into_inner()).take().unwrap_or_default();
    (result, Graph { edges })
}

impl Graph {
    /// Every plugin reachable from `start` through rules other than tie-breaks (which only keep
    /// the existing order), going forward (loads later) or backward, with the rule that reached
    /// each one first (by lowercase name).
    fn reach(&self, start: &str, forward: bool) -> std::collections::HashMap<String, usize> {
        let key = |s: &str| s.to_lowercase();
        let mut by: std::collections::HashMap<String, Vec<usize>> = Default::default();
        for (i, e) in self.edges.iter().enumerate().filter(|(_, e)| e.kind != "Tie Break") {
            by.entry(key(if forward { &e.from } else { &e.to })).or_default().push(i);
        }
        let mut seen: std::collections::HashMap<String, usize> = Default::default();
        let mut queue = std::collections::VecDeque::from([key(start)]);
        while let Some(n) = queue.pop_front() {
            for &i in by.get(&n).into_iter().flatten() {
                let next = key(if forward { &self.edges[i].to } else { &self.edges[i].from });
                if next != key(start) && !seen.contains_key(&next) {
                    seen.insert(next.clone(), i);
                    queue.push_back(next);
                }
            }
        }
        seen
    }

    /// The rules from `start` to `end` in a `reach` from `start`, in load order.
    fn chain(&self, reached: &std::collections::HashMap<String, usize>, start: &str, end: &str, forward: bool) -> Vec<String> {
        let mut out = Vec::new();
        let mut at = end.to_lowercase();
        while at != start.to_lowercase() {
            let Some(&i) = reached.get(&at) else { break };
            let e = &self.edges[i];
            out.push(describe(e));
            at = (if forward { &e.from } else { &e.to }).to_lowercase();
        }
        if forward {
            out.reverse();
        }
        out
    }
}

/// Why `name` moves from its place in `old` to its place in `new`: the nearest plugin it passes
/// that LOOT's rules put on its other side, and those rules.
fn why_moved(g: &Graph, old: &[&str], new: &[String], name: &str, master: &dyn Fn(&str) -> bool) -> Vec<String> {
    let lower = |s: &str| s.to_lowercase();
    let old_at: std::collections::HashMap<String, usize> = old.iter().enumerate().map(|(i, n)| (lower(n), i)).collect();
    let (Some(&o), Some(n)) = (old_at.get(&lower(name)), new.iter().position(|x| x.eq_ignore_ascii_case(name))) else { return vec![] };
    let (later, earlier) = (g.reach(name, true), g.reach(name, false));
    // Plugins it passes, nearest first: it now loads before ones that were before it, or after
    // ones that were after it.
    let mut passed: Vec<(usize, &String, bool)> = new
        .iter()
        .enumerate()
        .filter_map(|(i, p)| {
            let po = *old_at.get(&lower(p))?;
            if i > n && po < o {
                Some((i - n, p, true))
            } else if i < n && po > o {
                Some((n - i, p, false))
            } else {
                None
            }
        })
        .collect();
    passed.sort_by_key(|p| p.0);
    for &(_, p, before) in &passed {
        let reached = if before { &later } else { &earlier };
        if reached.contains_key(&lower(p)) {
            let head = if before { format!("It now loads before {p}.") } else { format!("It now loads after {p}.") };
            return std::iter::once(head).chain(g.chain(reached, name, p, before)).collect();
        }
    }
    if let Some(&(_, p, false)) = passed.iter().find(|(_, p, before)| !before && master(p) && !master(name)) {
        return vec![format!("It now loads after {p}: {p} is a master file (ESM flag), and those load before other plugins.")];
    }
    vec!["No rule of LOOT's moves it by itself: the plugins around it moved, and it keeps its order with them.".into()]
}

/// Why `name` sits where it does in LOOT's `order`: the nearest plugin before it and the nearest
/// after it that LOOT's rules tie it to.
fn why_here(g: &Graph, order: &[String], name: &str, master: &dyn Fn(&str) -> bool) -> Vec<String> {
    let Some(n) = order.iter().position(|x| x.eq_ignore_ascii_case(name)) else { return vec![] };
    let (earlier, later) = (g.reach(name, false), g.reach(name, true));
    let mut out = Vec::new();
    if let Some(p) = order[..n].iter().rev().find(|p| earlier.contains_key(&p.to_lowercase())) {
        out.push(format!("In LOOT's order it loads after {p}."));
        out.extend(g.chain(&earlier, name, p, false));
    } else if !master(name) && let Some(p) = order[..n].iter().rev().find(|p| master(p)) {
        out.push(format!("In LOOT's order it loads after {p}: {p} is a master file (ESM flag), and those load before other plugins."));
    }
    if let Some(p) = order[n + 1..].iter().find(|p| later.contains_key(&p.to_lowercase())) {
        out.push(format!("In LOOT's order it loads before {p}."));
        out.extend(g.chain(&later, name, p, true));
    }
    if out.is_empty() {
        out.push("No rule of LOOT's ties it to the plugins around it, so it keeps its place.".into());
    }
    out
}

/// LOOT's rules go in a circle, so it can't sort.
#[derive(Debug, Clone, Serialize)]
pub struct Cycle {
    /// Each rule loads its `from` before its `to`; the last leads back to the first.
    pub rules: Vec<Edge>,
    /// The circle in words.
    pub text: Vec<String>,
    /// Plugins in it with rules of yours: sorting without one of them (`sort_without`) may break
    /// the circle.
    pub yours: Vec<String>,
}

impl std::fmt::Display for Cycle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LOOT can't sort: its rules go in a circle. {}", self.text.join(" "))?;
        match &self.yours[..] {
            [] if self.rules.iter().any(|r| !crate::install::is_plugin(&r.from)) => write!(f, " The circle is in the order of LOOT's groups; change the group settings involved."),
            [] => write!(f, " None of the rules are yours, so it needs a fix in LOOT's masterlist or the plugins."),
            yours => write!(f, " Your rules for {} are part of it: sort without them just this once (--without), or change them.", yours.join(", ")),
        }
    }
}

impl std::error::Error for Cycle {}

/// A failed sort as an error: a circle of rules as a `Cycle` (the window takes it apart).
fn sort_error(game: &libloot::Game, e: libloot::error::SortPluginsError) -> anyhow::Error {
    let libloot::error::SortPluginsError::CycleFound(vertices) = &e else { return e.into() };
    let rules: Vec<Edge> = vertices
        .iter()
        .enumerate()
        .map(|(i, v)| Edge {
            from: v.name().to_owned(),
            to: vertices[(i + 1) % vertices.len()].name().to_owned(),
            kind: v.out_edge_type().map_or_else(|| "?".into(), |t| t.to_string()),
        })
        .collect();
    let mut yours = Vec::new();
    if rules.iter().any(|r| r.kind.starts_with("User")) {
        let db = game.database();
        if let Ok(db) = db.read() {
            for v in vertices {
                if crate::install::is_plugin(v.name()) && matches!(db.plugin_user_metadata(v.name(), EvalMode::DoNotEvaluate), Ok(Some(_))) && !yours.iter().any(|y: &String| y.eq_ignore_ascii_case(v.name())) {
                    yours.push(v.name().to_owned());
                }
            }
        }
    }
    let text = rules.iter().map(describe).collect();
    Cycle { rules, text, yours }.into()
}

/// Leave the user's rules for `plugins` out of this sort (the userlist stays as it is).
fn forget(game: &libloot::Game, plugins: &[String]) -> Result<()> {
    if plugins.is_empty() {
        return Ok(());
    }
    let db = game.database();
    let mut db = db.write().map_err(|_| anyhow::anyhow!("LOOT database lock poisoned"))?;
    for p in plugins {
        if db.plugin_user_metadata(p, EvalMode::DoNotEvaluate)?.is_none() {
            bail!("you have no LOOT rule for {p}");
        }
        db.discard_plugin_user_metadata(p);
    }
    Ok(())
}

fn is_master(game: &libloot::Game) -> impl Fn(&str) -> bool + '_ {
    |n| game.plugin(n).is_some_and(|p| p.is_master())
}

fn dir(inst: &Instance) -> PathBuf {
    inst.root.join("loot")
}

/// Whether LOOT's masterlist was downloaded, so its messages can show in Problems.
pub fn has_masterlist(inst: &Instance) -> bool {
    dir(inst).join("masterlist.yaml").is_file()
}

/// Download the masterlist and prelude if the cached copies are missing, older than a day, or
/// `force` is set. A failed download keeps the cached copy and returns a warning.
pub fn update_masterlist(inst: &Instance, force: bool) -> Result<Vec<String>> {
    let repo = match (inst.config.game.lo_id(), inst.config.game.base()) {
        (loadorder::GameId::SkyrimVR, _) => "skyrimvr",
        (loadorder::GameId::Fallout4VR, _) => "fallout4vr",
        (_, Game::Fallout4) => "fallout4",
        _ => "skyrimse",
    };
    let files = [
        (format!("https://raw.githubusercontent.com/loot/{repo}/{BRANCH}/masterlist.yaml"), "masterlist.yaml"),
        (format!("https://raw.githubusercontent.com/loot/prelude/{BRANCH}/prelude.yaml"), "prelude.yaml"),
    ];
    let mut warnings = Vec::new();
    fs::create_dir_all(dir(inst))?;
    for (url, name) in files {
        let path = dir(inst).join(name);
        let fresh = fs::metadata(&path).and_then(|m| m.modified()).is_ok_and(|t| SystemTime::now().duration_since(t).unwrap_or_default() < refresh());
        if fresh && !force {
            continue;
        }
        if let Err(e) = download(&url, &path) {
            if !path.exists() {
                bail!("cannot download the LOOT {name}: {e:#}");
            }
            warnings.push(format!("couldn't refresh the LOOT {name} ({e:#}); using the cached copy"));
        }
    }
    Ok(warnings)
}

// ponytail: shells out to curl, which ships with Windows 10+ and every Linux distro, rather than
// adding an HTTP stack for two files. Swap in ureq if curl turns out to be missing somewhere.
fn download(url: &str, dest: &Path) -> Result<()> {
    let mut tmp = dest.as_os_str().to_owned();
    tmp.push(".part");
    let out = crate::hidden("curl")
        .args(["-fsSL", "--retry", "2", "--max-time", "120", "-o"])
        .arg(&tmp)
        .arg(url)
        .output()
        .context("cannot run curl")?;
    if !out.status.success() {
        let _ = fs::remove_file(&tmp);
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    fs::rename(&tmp, dest)?;
    Ok(())
}

/// A libloot handle that sees the profile rather than the deployed Data folder: enabled mods are
/// additional data paths, highest priority first, so `file()` conditions match the profile, and
/// it reads the profile's plugins.txt (written to loot/local), so `active()` conditions and the
/// load order do too, before a deploy.
fn handle(inst: &Instance, profile: &Profile) -> Result<(libloot::Game, Vec<String>)> {
    let c = &inst.config;
    let game_type = match (c.game.lo_id(), c.game.base()) {
        (loadorder::GameId::SkyrimVR, _) => GameType::SkyrimVR,
        (loadorder::GameId::Fallout4VR, _) => GameType::Fallout4VR,
        (_, Game::Fallout4) => GameType::Fallout4,
        _ => GameType::SkyrimSE,
    };
    let local = dir(inst).join("local");
    write_plugins_txt(&local, &plugins::list(inst, profile)?)?;
    let mut game = libloot::Game::with_local_path(game_type, &c.game_path, &local)?;
    let mut dirs = vec![inst.mod_dir(OVERWRITE)];
    dirs.extend(profile.mods.iter().rev().filter(|m| m.enabled).map(|m| inst.mod_dir(&m.id)));
    game.set_additional_data_paths(dirs.into_iter().filter(|d| d.is_dir()).collect())?;
    game.load_current_load_order_state()?;

    let mut warnings = Vec::new();
    let (masterlist, prelude, userlist) = (dir(inst).join("masterlist.yaml"), dir(inst).join("prelude.yaml"), dir(inst).join("userlist.yaml"));
    {
        let db = game.database();
        let mut db = db.write().map_err(|_| anyhow::anyhow!("LOOT database lock poisoned"))?;
        if masterlist.is_file() {
            if prelude.is_file() {
                db.load_masterlist_with_prelude(&masterlist, &prelude)?;
            } else {
                db.load_masterlist(&masterlist)?;
            }
        } else {
            warnings.push("no LOOT masterlist; sorting uses only plugin masters and record overlaps".into());
        }
        if userlist.is_file() {
            db.load_userlist(&userlist)?;
        }
    }
    Ok((game, warnings))
}

/// The profile's plugins.txt as the game would read it: `*` marks the enabled ones. Written
/// whole and renamed, so two LOOT runs at once never read half of one.
fn write_plugins_txt(local: &Path, infos: &[PluginInfo]) -> Result<()> {
    fs::create_dir_all(local)?;
    // ponytail: Latin-1 for Windows-1252 (the game's encoding); differs only in 0x80-0x9F, where
    // a name gets '?'. encoding_rs (already in the build) if a plugin ever needs those.
    let bytes: Vec<u8> = infos
        .iter()
        .filter(|p| !p.implicit)
        .flat_map(|p| format!("{}{}
", if p.enabled { "*" } else { "" }, p.name).chars().map(|ch| u8::try_from(ch).ok().filter(|b| !(0x80..0xA0).contains(b)).unwrap_or(b'?')).collect::<Vec<_>>())
        .collect();
    let tmp = local.join(format!("plugins.{}.part", std::process::id()));
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, local.join("plugins.txt"))?;
    Ok(())
}

/// Load every readable plugin into libloot. Returns the plugins in the current load order.
fn load<'a>(game: &mut libloot::Game, infos: &'a [PluginInfo], warnings: &mut Vec<String>) -> Result<Vec<&'a PluginInfo>> {
    let usable: Vec<&PluginInfo> = infos.iter().filter(|p| p.error.is_none()).collect();
    for p in infos.iter().filter(|p| p.error.is_some()) {
        warnings.push(format!("{} can't be read, so LOOT skips it", p.name));
    }
    let paths: Vec<&Path> = usable.iter().map(|p| p.path.as_path()).collect();
    game.load_plugins(&paths)?;
    Ok(usable)
}

/// Put only the `new` plugins where LOOT's sort would, each right after the nearest plugin LOOT puts
/// before it; every other plugin keeps the user's order. Saved to the profile (undoable).
pub fn place(inst: &Instance, new: &[String]) -> Result<SortReport> {
    let (mut r, graph, masters) = run(inst, true, &[])?;
    let mut moved = vec![];
    inst.edit("place new plugins (LOOT)", |p| {
        let before = p.plugins.clone();
        p.plugins = placed(&before, &r.order, new);
        let at = |list: &[PluginEntry], n: &str| list.iter().position(|e| e.name.eq_ignore_ascii_case(n));
        moved = new
            .iter()
            .filter_map(|n| {
                let to = at(&p.plugins, n)?;
                let why = why_here(&graph, &r.order, n, &|x: &str| masters.iter().any(|m| m.eq_ignore_ascii_case(x)));
                Some(Moved { name: n.clone(), from: at(&before, n)?, to, after: to.checked_sub(1).map(|i| p.plugins[i].name.clone()), why })
            })
            .filter(|m| m.from != m.to)
            .collect();
        Ok(())
    })?;
    r.order = inst.profile()?.plugins.into_iter().map(|e| e.name).collect();
    (r.dry_run, r.moved) = (false, moved);
    Ok(r)
}

fn placed(plugins: &[PluginEntry], loot: &[String], new: &[String]) -> Vec<PluginEntry> {
    let is_new = |n: &str| new.iter().any(|x| x.eq_ignore_ascii_case(n));
    let mut out: Vec<PluginEntry> = plugins.iter().filter(|e| !is_new(&e.name)).cloned().collect();
    for (i, name) in loot.iter().enumerate().filter(|(_, n)| is_new(n)) {
        let Some(e) = plugins.iter().find(|e| e.name.eq_ignore_ascii_case(name)) else { continue };
        let at = loot[..i].iter().rev().find_map(|b| out.iter().position(|x| x.name.eq_ignore_ascii_case(b))).map_or(0, |j| j + 1);
        out.insert(at, e.clone());
    }
    // New plugins LOOT didn't see stay where they were, at the end.
    let unseen: Vec<PluginEntry> = plugins.iter().filter(|e| is_new(&e.name) && !loot.iter().any(|n| n.eq_ignore_ascii_case(&e.name))).cloned().collect();
    out.extend(unseen);
    out
}

/// Sort plugins with LOOT. A dry run returns the new order and what moves, with LOOT's reasons;
/// otherwise the order is saved to the profile (undoable) and reaches plugins.txt on the next
/// deploy. When LOOT's rules go in a circle the error is a `Cycle`.
pub fn sort(inst: &Instance, dry_run: bool) -> Result<SortReport> {
    sort_without(inst, dry_run, &[])
}

/// `sort`, leaving the user's rules for `without` out of this one sort (to get past a `Cycle`
/// they are part of). The userlist keeps them.
pub fn sort_without(inst: &Instance, dry_run: bool, without: &[String]) -> Result<SortReport> {
    Ok(run(inst, dry_run, without)?.0)
}

/// Why LOOT puts `plugin` where it does: where it is now, where LOOT's sort puts it, and the
/// rules that tie it to the plugins nearest it.
pub fn why(inst: &Instance, plugin: &str) -> Result<serde_json::Value> {
    let (r, graph, masters) = run(inst, true, &[])?;
    let Some(to) = r.order.iter().position(|n| n.eq_ignore_ascii_case(plugin)) else { bail!("LOOT doesn't sort a plugin named {plugin} in this profile") };
    let now = plugins::list(inst, &inst.profile()?)?.iter().position(|p| p.name.eq_ignore_ascii_case(plugin));
    let why = why_here(&graph, &r.order, plugin, &|x: &str| masters.iter().any(|m| m.eq_ignore_ascii_case(x)));
    let moves = r.moved.iter().find(|m| m.name.eq_ignore_ascii_case(plugin)).map(|m| &m.why);
    Ok(serde_json::json!({ "plugin": r.order[to], "loot_position": to, "profile_position": now, "why": why, "why_it_would_move": moves }))
}

/// A sort, with the rules LOOT used and the master-flagged plugins.
fn run(inst: &Instance, dry_run: bool, without: &[String]) -> Result<(SortReport, Graph, Vec<String>)> {
    crate::progress::report("sorting plugins with LOOT", 0, 0);
    let mut warnings = update_masterlist(inst, false).unwrap_or_else(|e| vec![format!("{e:#}")]);
    let profile = inst.profile()?;
    let infos = plugins::list(inst, &profile)?;
    let (mut game, w) = handle(inst, &profile)?;
    warnings.extend(w);
    forget(&game, without)?;
    let current = load(&mut game, &infos, &mut warnings)?;
    let names: Vec<&str> = current.iter().map(|p| p.name.as_str()).collect();
    let (result, graph) = sort_reading(&game, &names);
    let order = result.map_err(|e| sort_error(&game, e))?;
    let masters: Vec<String> = names.iter().filter(|n| is_master(&game)(n)).map(|n| n.to_string()).collect();

    let undeployed = current.iter().filter(|p| game.is_plugin_active(&p.name) != p.enabled).count();
    if undeployed > 0 {
        warnings.push(format!("LOOT saw {undeployed} plugins on or off differently from this profile"));
    }
    let mut moved = moves(&names, &order);
    for m in &mut moved {
        m.why = why_moved(&graph, &names, &order, &m.name, &is_master(&game));
    }

    if !dry_run && !moved.is_empty() {
        inst.edit("sort plugins (LOOT)", |p| {
            let find = |name: &str| p.plugins.iter().find(|e| e.name.eq_ignore_ascii_case(name)).cloned();
            let mut sorted: Vec<PluginEntry> = order
                .iter()
                .filter_map(|name| {
                    let info = current.iter().find(|i| i.name.eq_ignore_ascii_case(name))?;
                    // Base-game masters always load first; the profile doesn't list them.
                    if info.implicit {
                        return None;
                    }
                    Some(find(name).unwrap_or(PluginEntry { name: name.clone(), enabled: info.enabled }))
                })
                .collect();
            // Entries LOOT didn't see (their mod is gone or disabled) keep their order at the end.
            sorted.extend(p.plugins.iter().filter(|e| !order.iter().any(|n| n.eq_ignore_ascii_case(&e.name))).cloned());
            p.plugins = sorted;
            Ok(())
        })?;
    }
    Ok((SortReport { dry_run, order, moved, undeployed, warnings }, graph, masters))
}

/// The plugins that move from `old` to `new`: all but one longest run that keeps its order, so
/// one plugin moved up counts once, not with every plugin that shifts along.
fn moves(old: &[&str], new: &[String]) -> Vec<Moved> {
    let at: std::collections::HashMap<String, usize> = old.iter().enumerate().map(|(i, n)| (n.to_lowercase(), i)).collect();
    let both: Vec<(usize, usize)> = new.iter().enumerate().filter_map(|(to, n)| Some((to, *at.get(&n.to_lowercase())?))).collect();
    let keep = crate::instance::longest_increasing(&both.iter().map(|b| b.1).collect::<Vec<_>>());
    both.iter()
        .enumerate()
        .filter(|(k, _)| !keep.contains(k))
        .map(|(_, &(to, from))| Moved { name: new[to].clone(), from, to, after: to.checked_sub(1).map(|i| new[i].clone()), why: vec![] })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn one_move_counts_once() {
        let old = ["A", "B", "C", "D", "E"];
        let new: Vec<String> = ["E", "A", "B", "C", "D"].map(String::from).into();
        let m = super::moves(&old, &new);
        assert_eq!((m.len(), m[0].name.as_str(), m[0].after.as_deref()), (1, "E", None));
        let new: Vec<String> = ["A", "C", "D", "B", "E"].map(String::from).into();
        let m = super::moves(&old, &new);
        assert_eq!((m.len(), m[0].name.as_str(), m[0].after.as_deref(), m[0].from, m[0].to), (1, "B", Some("D"), 1, 3));
        assert!(super::moves(&old, &old.map(String::from)).is_empty());
    }
}

/// A plugin LOOT's masterlist knows as dirty, in this very version (by its CRC).
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Dirty {
    pub plugin: String,
    /// Identical-to-master records, deleted references and deleted navmeshes. QuickAutoClean fixes
    /// the first two; deleted navmeshes take xEdit by hand.
    pub itm: u32,
    pub deleted_references: u32,
    pub deleted_navmeshes: u32,
}

/// LOOT's warnings and errors for enabled plugins, plus dirty-plugin reports, from the cached
/// masterlist. Nothing is downloaded; without a masterlist there is nothing to report.
pub fn issues(inst: &Instance) -> Result<Vec<Issue>> {
    Ok(scan(inst)?.0)
}

/// The enabled plugins LOOT's cached masterlist knows as dirty.
pub fn dirty_plugins(inst: &Instance) -> Result<Vec<Dirty>> {
    Ok(scan(inst)?.1)
}

fn scan(inst: &Instance) -> Result<(Vec<Issue>, Vec<Dirty>)> {
    if !dir(inst).join("masterlist.yaml").is_file() {
        return Ok(Default::default());
    }
    let profile = inst.profile()?;
    let infos = plugins::list(inst, &profile)?;
    let (mut game, _) = handle(inst, &profile)?;
    let current = load(&mut game, &infos, &mut Vec::new())?;
    let db = game.database();
    let db = db.read().map_err(|_| anyhow::anyhow!("LOOT database lock poisoned"))?;
    let mut issues = Vec::new();
    let mut dirty = Vec::new();
    let cfg = crate::settings::get().loot;
    let text = |m: &libloot::metadata::Message| select_message_content(m.content(), &cfg.language).map(|c| c.text().to_owned()).unwrap_or_default();
    let severity = |m: &libloot::metadata::Message| match m.message_type() {
        MessageType::Error => Some("error"),
        MessageType::Warn => Some("warning"),
        MessageType::Say if cfg.show_info => Some("info"),
        _ => None,
    };
    for m in db.general_messages(MergeMode::WithUserMetadata, EvalMode::Evaluate)? {
        if let Some(s) = severity(&m) {
            issues.push(Issue { severity: s, kind: "loot_message", subject: "LOOT".into(), message: text(&m) });
        }
    }
    for p in current.iter().filter(|p| p.enabled) {
        let Some(meta) = db.plugin_metadata(&p.name, MergeMode::WithUserMetadata, EvalMode::Evaluate)? else { continue };
        for m in meta.messages() {
            if let Some(s) = severity(m) {
                issues.push(Issue { severity: s, kind: "loot_message", subject: p.name.clone(), message: text(m) });
            }
        }
        for d in meta.dirty_info() {
            issues.push(Issue {
                severity: "warning",
                kind: "dirty_plugin",
                subject: p.name.clone(),
                message: format!(
                    "{} ITMs, {} deleted references, {} deleted navmeshes; clean it with xEdit QuickAutoClean (Problems' Clean dirty plugins…, or run_tool xedit-qac with plugin={})",
                    d.itm_count(),
                    d.deleted_reference_count(),
                    d.deleted_navmesh_count(),
                    p.name
                ),
            });
            dirty.push(Dirty { plugin: p.name.clone(), itm: d.itm_count(), deleted_references: d.deleted_reference_count(), deleted_navmeshes: d.deleted_navmesh_count() });
        }
    }
    Ok((issues, dirty))
}

/// A user rule for one plugin, kept in `loot/userlist.yaml` in LOOT's own format, so LOOT itself
/// reads the same rules.
#[derive(Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct Rule {
    pub plugin: String,
    /// Plugins this one must load after.
    #[serde(default)]
    pub after: Vec<String>,
    /// A LOOT group from the masterlist, e.g. "Late Loaders".
    #[serde(default)]
    pub group: Option<String>,
}

fn rule_of(m: &PluginMetadata) -> Rule {
    Rule { plugin: m.name().to_owned(), after: m.load_after_files().iter().map(|f| f.name().as_str().to_owned()).collect(), group: m.group().map(str::to_owned) }
}

/// The user rules for the profile's plugins.
pub fn rules(inst: &Instance) -> Result<Vec<Rule>> {
    let profile = inst.profile()?;
    let (game, _) = handle(inst, &profile)?;
    let db = game.database();
    let db = db.read().map_err(|_| anyhow::anyhow!("LOOT database lock poisoned"))?;
    let mut out = Vec::new();
    for p in plugins::list(inst, &profile)? {
        if let Some(m) = db.plugin_user_metadata(&p.name, EvalMode::DoNotEvaluate)? {
            out.push(rule_of(&m));
        }
    }
    Ok(out)
}

/// LOOT's group names (masterlist and userlist), for picking a plugin's group. Empty until a
/// masterlist is cached.
pub fn groups(inst: &Instance) -> Result<Vec<String>> {
    if !dir(inst).join("masterlist.yaml").is_file() {
        return Ok(vec![]);
    }
    let (game, _) = handle(inst, &inst.profile()?)?;
    let db = game.database();
    let db = db.read().map_err(|_| anyhow::anyhow!("LOOT database lock poisoned"))?;
    Ok(db.groups(MergeMode::WithUserMetadata).iter().map(|g| g.name().to_owned()).collect())
}

/// Replace a plugin's load-after list and group in the userlist; an empty rule removes them.
/// Other user metadata for the plugin (messages, tags) stays. Returns the previous rule, which
/// is how to undo this: the userlist isn't part of the profile history.
pub fn set_rule(inst: &Instance, rule: &Rule) -> Result<Option<Rule>> {
    let profile = inst.profile()?;
    let (game, _) = handle(inst, &profile)?;
    let db = game.database();
    let mut db = db.write().map_err(|_| anyhow::anyhow!("LOOT database lock poisoned"))?;
    if let Some(g) = &rule.group
        && dir(inst).join("masterlist.yaml").is_file()
        && !db.groups(MergeMode::WithUserMetadata).iter().any(|x| x.name() == g)
    {
        bail!("LOOT has no group '{g}'; groups come from the masterlist");
    }
    let old = db.plugin_user_metadata(&rule.plugin, EvalMode::DoNotEvaluate)?;
    let mut m = match &old {
        Some(m) => m.clone(),
        None => PluginMetadata::new(&rule.plugin)?,
    };
    m.set_load_after_files(rule.after.iter().map(|a| File::new(a.clone())).collect());
    match &rule.group {
        Some(g) => m.set_group(g.clone()),
        None => m.unset_group(),
    }
    db.discard_plugin_user_metadata(&rule.plugin);
    if !m.has_name_only() {
        db.set_plugin_user_metadata(m);
    }
    fs::create_dir_all(dir(inst))?;
    let mut opts = MetadataWriteOptions::new();
    opts.set_truncate(true);
    db.write_user_metadata(&dir(inst).join("userlist.yaml"), &opts)?;
    Ok(old.as_ref().map(rule_of))
}

#[cfg(test)]
mod place_tests {
    use super::*;

    #[test]
    fn only_new_plugins_move() {
        let e = |n: &str| PluginEntry { name: n.into(), enabled: true };
        let names = |v: Vec<PluginEntry>| v.into_iter().map(|e| e.name).collect::<Vec<_>>();
        // The user keeps C before A on purpose; LOOT would swap them and puts New after A.
        let mine = vec![e("C.esp"), e("A.esp"), e("B.esp"), e("New.esp")];
        let loot: Vec<String> = ["A.esp", "New.esp", "B.esp", "C.esp"].map(String::from).to_vec();
        assert_eq!(names(placed(&mine, &loot, &["new.esp".into()])), ["C.esp", "A.esp", "New.esp", "B.esp"]);
        // First in LOOT's order: to the top. Unseen by LOOT: left at the end.
        let loot: Vec<String> = ["New.esp", "A.esp", "B.esp", "C.esp"].map(String::from).to_vec();
        assert_eq!(names(placed(&mine, &loot, &["New.esp".into()])), ["New.esp", "C.esp", "A.esp", "B.esp"]);
        assert_eq!(names(placed(&mine, &loot[1..], &["New.esp".into()])), ["C.esp", "A.esp", "B.esp", "New.esp"]);
    }
}
