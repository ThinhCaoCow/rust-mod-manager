//! Assisted bisect: find the mod behind a crash by turning half of the suspects off at a time.
//!
//! Each step turns off half of the mods still suspected, with the enabled mods that need them
//! (their plugins' masters, "requires" rules), as one undoable change. After the game is played
//! (or a test run), `mark` says whether it crashed: the step is undone and the suspects halve.
//! When one mod is left and a step with it off didn't crash, that's the culprit. Until some step
//! passes, the last mod left gets a step of its own, so a crash the suspects don't cause ends
//! with "not found" instead of blaming the last one. The state is `profiles/<name>/bisect.json`.
use crate::Instance;
use crate::instance::{is_separator, write_atomic};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;

const FILE: &str = "bisect.json";

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct Bisect {
    /// Mods that may hold the culprit, in load order.
    pub left: Vec<String>,
    /// Mods turned off for this step: suspects, and enabled mods that need them.
    pub off: Vec<String>,
    /// The suspects among `off`.
    pub testing: Vec<String>,
    /// A step has shown the culprit is among `left` (the game didn't crash with them off).
    pub confirmed: bool,
    pub steps: Vec<StepRecord>,
    /// The undo label of this step's change.
    pub label: String,
    pub done: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StepRecord {
    pub off: Vec<String>,
    pub crashed: bool,
    /// A test run's summary.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

#[derive(Serialize, Debug)]
pub struct Status {
    #[serde(flatten)]
    pub state: Bisect,
    pub culprit: Option<String>,
    /// About how many more games to play.
    pub steps_left: usize,
    /// What to do now, in plain words.
    pub message: String,
}

fn file(inst: &Instance) -> std::path::PathBuf {
    inst.profile_dir().join(FILE)
}

fn load(inst: &Instance) -> Result<Option<Bisect>> {
    match fs::read_to_string(file(inst)) {
        Ok(t) => Ok(Some(serde_json::from_str(&t)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// The bisect under way in the active profile, if any.
pub fn status(inst: &Instance) -> Result<Option<Status>> {
    Ok(load(inst)?.map(|b| report(inst, b)))
}

/// The mods to test: those the newest crash log names, or with `all` every enabled mod. Either
/// way not the script extender, Address Library, the crash logger or the test harness: the game
/// needs them to load the rest, write a crash log, or run a test. In load order.
pub fn suspects(inst: &Instance, all: bool) -> Result<Vec<String>> {
    let p = inst.profile()?;
    let mut keep: BTreeSet<String> = crate::launch::essential_mods(inst)?.into_iter().collect();
    let dll = crate::path_key(crate::harness::harness_dll(inst.config.game));
    keep.extend(p.mods.iter().filter(|m| inst.mod_files(&m.id).is_ok_and(|f| f.iter().any(|f| crate::path_key(f) == dll))).map(|m| m.id.clone()));
    let wanted: BTreeSet<String> = if all {
        p.mods.iter().map(|m| m.id.clone()).collect()
    } else {
        crate::logs::read_crash_log(inst, None)?.suspects.into_iter().filter_map(|s| s.mod_id).collect()
    };
    Ok(p.mods.iter().filter(|m| m.enabled && !is_separator(&m.id) && wanted.contains(&m.id) && !keep.contains(&m.id)).map(|m| m.id.clone()).collect())
}

/// Start a bisect over these enabled mods and turn off the first half.
pub fn start(inst: &Instance, mods: &[String]) -> Result<Status> {
    if load(inst)?.is_some() {
        bail!("a bisect is under way in this profile; stop it first");
    }
    let p = inst.profile()?;
    for id in mods {
        if !p.mods.iter().any(|m| &m.id == id && m.enabled && !is_separator(id)) {
            bail!("'{id}' is not an enabled mod");
        }
    }
    let left: Vec<String> = p.mods.iter().filter(|m| mods.contains(&m.id)).map(|m| m.id.clone()).collect();
    if left.is_empty() {
        bail!("no mods to test: name some, or use the crash log's suspects or all mods");
    }
    let mut b = Bisect { left, ..Default::default() };
    next(inst, &mut b)?;
    write_atomic(&file(inst), &serde_json::to_string_pretty(&b)?)?;
    Ok(report(inst, b))
}

/// Record whether the game crashed with this step's mods off, turn them back on, and turn off
/// the next half (or finish).
pub fn mark(inst: &Instance, crashed: bool) -> Result<Status> {
    mark_with(inst, crashed, String::new())
}

fn mark_with(inst: &Instance, crashed: bool, note: String) -> Result<Status> {
    let mut b = load(inst)?.context("no bisect under way in this profile")?;
    if b.done {
        bail!("the bisect is finished; stop it to clear it");
    }
    turn_back_on(inst, &b)?;
    b.steps.push(StepRecord { off: b.off.clone(), crashed, note });
    let testing = std::mem::take(&mut b.testing);
    if crashed {
        b.left.retain(|m| !testing.contains(m));
    } else {
        b.left = testing;
        b.confirmed = true;
    }
    next(inst, &mut b)?;
    write_atomic(&file(inst), &serde_json::to_string_pretty(&b)?)?;
    Ok(report(inst, b))
}

/// Play each step with `test` (true when the game held up, and a note) until the bisect ends.
pub fn run(inst: &Instance, mut test: impl FnMut(&Instance) -> Result<(bool, String)>) -> Result<Status> {
    loop {
        let b = load(inst)?.context("no bisect under way in this profile")?;
        if b.done {
            return Ok(report(inst, b));
        }
        let (passed, note) = test(inst)?;
        mark_with(inst, !passed, note)?;
    }
}

/// End the bisect, turning this step's mods back on.
pub fn stop(inst: &Instance) -> Result<Option<Status>> {
    let Some(b) = load(inst)? else { return Ok(None) };
    let back = if b.done { Ok(()) } else { turn_back_on(inst, &b) };
    fs::remove_file(file(inst))?;
    let mut s = report(inst, Bisect { done: true, ..b });
    s.message = match back {
        Ok(()) => "Stopped. The mods it turned off are back on.".into(),
        Err(e) => format!("Stopped. {e:#}"),
    };
    Ok(Some(s))
}

/// Undo this step's change. Fine if it was undone already; refused if something changed since.
fn turn_back_on(inst: &Instance, b: &Bisect) -> Result<()> {
    if b.off.is_empty() {
        return Ok(());
    }
    if inst.history()?.first() == Some(&b.label) {
        inst.undo()?;
        return Ok(());
    }
    let p = inst.profile()?;
    if p.mods.iter().filter(|m| b.off.contains(&m.id)).all(|m| m.enabled) {
        return Ok(());
    }
    bail!("the setup changed after this step turned mods off, so rmm can't undo it: undo the newer changes first, or turn these back on in History: {}", names(inst, &b.off))
}

/// Pick the next half of `left` and turn it off, or finish.
fn next(inst: &Instance, b: &mut Bisect) -> Result<()> {
    b.off.clear();
    b.testing.clear();
    if b.left.is_empty() || (b.left.len() == 1 && b.confirmed) {
        b.done = true;
        return Ok(());
    }
    let needs = dependents(inst)?;
    let closure = |from: &str, into: &mut BTreeSet<String>| {
        let mut todo = vec![from.to_string()];
        while let Some(m) = todo.pop() {
            if into.insert(m.clone()) {
                todo.extend(needs.get(&m).into_iter().flatten().cloned());
            }
        }
    };
    let mut off = BTreeSet::new();
    if b.left.len() == 1 {
        closure(&b.left[0], &mut off);
        b.testing = b.left.clone();
    } else {
        // From the end of the list: later mods are the ones that need earlier ones.
        for c in b.left.iter().rev() {
            if off.contains(c) {
                continue;
            }
            let mut more = off.clone();
            closure(c, &mut more);
            let testing: Vec<String> = b.left.iter().filter(|m| more.contains(*m)).cloned().collect();
            if testing.len() == b.left.len() {
                continue; // that would test them all at once
            }
            (off, b.testing) = (more, testing);
            if b.testing.len() * 2 >= b.left.len() {
                break;
            }
        }
        if b.testing.is_empty() {
            b.done = true; // they all need each other
            return Ok(());
        }
    }
    let p = inst.profile()?;
    b.off = p.mods.iter().filter(|m| off.contains(&m.id)).map(|m| m.id.clone()).collect();
    b.label = format!("bisect step {}: turn off {}", b.steps.len() + 1, names(inst, &b.off));
    crate::ops::set_enabled_as(inst, &b.off, false, &b.label)
}

/// For each enabled mod, the enabled mods that need it: their plugins use its plugins as
/// masters, or their rules require it.
fn dependents(inst: &Instance) -> Result<HashMap<String, Vec<String>>> {
    let p = inst.profile()?;
    let plugins = crate::plugins::list(inst, &p)?;
    let from: HashMap<String, &str> = plugins.iter().filter(|x| x.enabled).filter_map(|x| Some((x.name.to_lowercase(), x.mod_id.as_deref()?))).collect();
    let mut needs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for pl in plugins.iter().filter(|x| x.enabled) {
        let Some(m) = &pl.mod_id else { continue };
        for master in &pl.masters {
            if let Some(&src) = from.get(&master.to_lowercase()).filter(|s| **s != m.as_str()) {
                needs.entry(src.to_string()).or_default().insert(m.clone());
            }
        }
    }
    let on: BTreeSet<&str> = p.mods.iter().filter(|m| m.enabled).map(|m| m.id.as_str()).collect();
    for r in crate::rules::rules(inst)?.into_iter().filter(|r| on.contains(r.id.as_str())) {
        for q in r.requires {
            needs.entry(q).or_default().insert(r.id.clone());
        }
    }
    Ok(needs.into_iter().map(|(k, v)| (k, v.into_iter().collect())).collect())
}

fn names(inst: &Instance, ids: &[String]) -> String {
    ids.iter().map(|id| inst.mod_meta(id).name).collect::<Vec<_>>().join(", ")
}

fn report(inst: &Instance, b: Bisect) -> Status {
    let culprit = (b.done && b.confirmed && b.left.len() == 1).then(|| b.left[0].clone());
    let steps_left = if b.done { 0 } else { (b.left.len() as f64).log2().ceil() as usize + usize::from(!b.confirmed) };
    let message = if let Some(c) = &culprit {
        format!("Found it: {}. The game didn't crash with it off. It's on again now: disable it, or look for an update or a patch for it.", inst.mod_meta(c).name)
    } else if b.done && b.left.is_empty() {
        "Not found: the game crashed with each of these mods off, so the cause is elsewhere (another mod, or the save). Try again with all mods.".into()
    } else if b.done {
        format!("These mods need each other, so rmm can't test them apart: {}.", names(inst, &b.left))
    } else {
        let n = b.steps.len() + 1;
        let extra = b.off.len() - b.testing.len();
        let also = if extra > 0 { format!(" ({extra} of them need the others)") } else { String::new() };
        format!(
            "Step {n}: {} mod{} off{also}: {}. Play to where the game crashed, or run a test, then say whether it still crashes. {} suspect{} left, about {steps_left} step{} to go.",
            b.off.len(),
            if b.off.len() == 1 { " is" } else { "s are" },
            names(inst, &b.off),
            b.left.len(),
            if b.left.len() == 1 { "" } else { "s" },
            if steps_left == 1 { "" } else { "s" },
        )
    };
    Status { state: b, culprit, steps_left, message }
}
