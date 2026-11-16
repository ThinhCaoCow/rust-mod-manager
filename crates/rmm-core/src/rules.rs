//! Mod rules: Vortex's "load after", "requires" and "incompatible", kept as suggestions so they
//! never block manual ordering or enabling. The health check reports rules the setup breaks, and
//! `sort` reorders the mods to satisfy the load-after rules. Stored in `rules.toml` and shared by
//! every profile.
use crate::Instance;
use crate::instance::write_atomic;
use anyhow::{Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;

const FILE: &str = "rules.toml";

/// A mod's rules. All lists empty removes the rule.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default, PartialEq)]
pub struct ModRule {
    /// Mod id as shown by list_mods.
    pub id: String,
    /// Mods it loads after: it has higher priority and wins their file conflicts.
    #[serde(default)]
    pub after: Vec<String>,
    /// Mods that must be enabled when it is.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    /// Mods that must not be enabled together with it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub incompatible: Vec<String>,
}

impl ModRule {
    fn is_empty(&self) -> bool {
        self.after.is_empty() && self.requires.is_empty() && self.incompatible.is_empty()
    }
}

#[derive(Serialize, Deserialize, Default)]
struct RulesFile {
    #[serde(default)]
    rules: Vec<ModRule>,
}

pub fn rules(inst: &Instance) -> Result<Vec<ModRule>> {
    match fs::read_to_string(inst.root.join(FILE)) {
        Ok(text) => Ok(toml::from_str::<RulesFile>(&text)?.rules),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
        Err(e) => Err(e.into()),
    }
}

fn save(inst: &Instance, mut rules: Vec<ModRule>) -> Result<()> {
    rules.retain(|r| !r.is_empty());
    rules.sort_by(|a, b| a.id.cmp(&b.id));
    write_atomic(&inst.root.join(FILE), &toml::to_string_pretty(&RulesFile { rules })?)
}

/// Replace a mod's rule. Returns the previous one: rules are not in the profile's undo history.
pub fn set_rule(inst: &Instance, rule: &ModRule) -> Result<Option<ModRule>> {
    let mut rule = rule.clone();
    for list in [&mut rule.after, &mut rule.requires, &mut rule.incompatible] {
        list.sort();
        list.dedup();
    }
    for id in std::iter::once(&rule.id).chain(&rule.after).chain(&rule.requires).chain(&rule.incompatible) {
        if id == crate::instance::OVERWRITE || crate::instance::is_separator(id) || !inst.mod_dir(id).is_dir() {
            bail!("no mod '{id}'");
        }
    }
    if rule.after.contains(&rule.id) || rule.requires.contains(&rule.id) || rule.incompatible.contains(&rule.id) {
        bail!("a rule can't name the mod itself");
    }
    if let Some(both) = rule.requires.iter().find(|r| rule.incompatible.contains(r)) {
        bail!("{both} can't be both required and incompatible");
    }
    inst.locked(|| {
        let mut all = rules(inst)?;
        let previous = all.iter().position(|r| r.id == rule.id).map(|i| all.remove(i));
        all.push(rule);
        save(inst, all)?;
        Ok(previous)
    })
}

/// A tool run replaced mod `old` with `new`: carry the rules over.
pub(crate) fn rename(inst: &Instance, old: &str, new: &str) -> Result<()> {
    inst.locked(|| {
        let mut all = rules(inst)?;
        if !all.iter().any(|r| r.id == old || r.after.iter().chain(&r.requires).chain(&r.incompatible).any(|a| a == old)) {
            return Ok(());
        }
        for r in &mut all {
            for id in std::iter::once(&mut r.id).chain(&mut r.after).chain(&mut r.requires).chain(&mut r.incompatible) {
                if id == old {
                    *id = new.into();
                }
            }
        }
        save(inst, all)
    })
}

/// (mod, mod it should load after) for rules between enabled mods that the order breaks.
pub fn broken(inst: &Instance, profile: &crate::Profile) -> Result<Vec<(String, String)>> {
    let pos: HashMap<&str, usize> =
        profile.mods.iter().enumerate().filter(|(_, m)| m.enabled).map(|(i, m)| (m.id.as_str(), i)).collect();
    let mut out = Vec::new();
    for r in rules(inst)? {
        let Some(&i) = pos.get(r.id.as_str()) else { continue };
        for a in r.after {
            if pos.get(a.as_str()).is_some_and(|&j| j > i) {
                out.push((r.id.clone(), a));
            }
        }
    }
    Ok(out)
}

/// An enabled mod whose "requires" or "incompatible" rule the setup breaks.
#[derive(Serialize, Debug, PartialEq)]
pub struct Unmet {
    pub id: String,
    /// Required mods that are disabled or not installed.
    pub missing: Vec<String>,
    /// Incompatible mods that are enabled too.
    pub clashes: Vec<String>,
}

/// Enabled mods that need a disabled or missing mod, or run alongside an incompatible one.
/// Incompatibility goes both ways, so each pair is reported once, under the mod with the rule.
pub fn unmet(inst: &Instance, profile: &crate::Profile) -> Result<Vec<Unmet>> {
    let on: std::collections::HashSet<&str> = profile.mods.iter().filter(|m| m.enabled).map(|m| m.id.as_str()).collect();
    let mut out = Vec::new();
    for r in rules(inst)? {
        if !on.contains(r.id.as_str()) {
            continue;
        }
        let missing: Vec<String> = r.requires.iter().filter(|x| !on.contains(x.as_str())).cloned().collect();
        let clashes: Vec<String> = r.incompatible.iter().filter(|x| on.contains(x.as_str())).cloned().collect();
        if !missing.is_empty() || !clashes.is_empty() {
            out.push(Unmet { id: r.id, missing, clashes });
        }
    }
    Ok(out)
}

/// Where a new mod `id` goes in `order` (current ids, lowest priority first): at the end, but
/// before a trailing run of generated tool output (which stays last) and before any mod whose
/// rule says it loads after `id`, as long as that keeps `id` after the mods it loads after.
pub(crate) fn place(inst: &Instance, order: &[String], id: &str) -> Result<usize> {
    let mut at = order.len();
    while at > 0 && inst.mod_meta(&order[at - 1]).source.is_some_and(|s| s.starts_with("tool:")) {
        at -= 1;
    }
    let rules = rules(inst)?;
    let pos = |x: &str| order.iter().position(|o| o == x);
    let floor = rules.iter().filter(|r| r.id == id).flat_map(|r| &r.after).filter_map(|a| pos(a)).map(|i| i + 1).max().unwrap_or(0);
    let first_after = rules.iter().filter(|r| r.after.iter().any(|a| a == id)).filter_map(|r| pos(&r.id)).min();
    if let Some(i) = first_after {
        at = at.min(i);
    }
    Ok(at.max(floor).min(order.len()))
}

#[derive(Serialize, Debug)]
pub struct SortReport {
    pub dry_run: bool,
    /// Mod ids in the new order, lowest priority first.
    pub order: Vec<String>,
    /// Mods whose position in the list changed.
    pub moved: Vec<String>,
    /// Rules left out because they form a cycle: (mod, mod it should load after).
    pub cycles: Vec<(String, String)>,
}

/// Reorder the profile's mods (enabled or not) to satisfy the rules, keeping the current order
/// wherever no rule forces a change. Undoable when applied.
pub fn sort(inst: &Instance, dry_run: bool) -> Result<SortReport> {
    let rules = rules(inst)?;
    let plan = |p: &crate::Profile| {
        let ids: Vec<String> = p.mods.iter().map(|m| m.id.clone()).collect();
        let (order, cycles) = order(&ids, &rules);
        let moved = order.iter().enumerate().filter(|&(i, &j)| i != j).map(|(_, &j)| ids[j].clone()).collect();
        let order: Vec<String> = order.into_iter().map(|j| ids[j].clone()).collect();
        let cycles = cycles.into_iter().map(|(i, j)| (ids[i].clone(), ids[j].clone())).collect();
        SortReport { dry_run, order, moved, cycles }
    };
    if dry_run {
        return Ok(plan(&inst.profile()?));
    }
    inst.edit("sort mods by rules", |p| {
        let r = plan(p);
        p.mods.sort_by_key(|m| r.order.iter().position(|id| *id == m.id));
        Ok(r)
    })
}

/// Stable topological order: repeatedly take the first mod in the current order whose
/// load-after mods are all placed. Rules in a cycle are dropped first and returned.
// ponytail: O(n²) scan, fine for a few thousand mods; use a heap of ready indices if it shows.
fn order(ids: &[String], rules: &[ModRule]) -> (Vec<usize>, Vec<(usize, usize)>) {
    let pos: HashMap<&str, usize> = ids.iter().enumerate().map(|(i, id)| (id.as_str(), i)).collect();
    let mut preds = vec![Vec::new(); ids.len()];
    for r in rules {
        let Some(&i) = pos.get(r.id.as_str()) else { continue };
        preds[i].extend(r.after.iter().filter_map(|a| pos.get(a.as_str()).copied()));
    }
    // Depth-first search; an edge back to a mod still on the stack closes a cycle.
    fn visit(i: usize, preds: &mut [Vec<usize>], state: &mut [u8], dropped: &mut Vec<(usize, usize)>) {
        state[i] = 1;
        let mut k = 0;
        while k < preds[i].len() {
            let j = preds[i][k];
            if state[j] == 1 {
                dropped.push((i, j));
                preds[i].remove(k);
                continue;
            }
            if state[j] == 0 {
                visit(j, preds, state, dropped);
            }
            k += 1;
        }
        state[i] = 2;
    }
    let (mut state, mut cycles) = (vec![0u8; ids.len()], Vec::new());
    for i in 0..ids.len() {
        if state[i] == 0 {
            visit(i, &mut preds, &mut state, &mut cycles);
        }
    }
    let mut placed = vec![false; ids.len()];
    let mut out = Vec::with_capacity(ids.len());
    while out.len() < ids.len() {
        let i = (0..ids.len()).find(|&i| !placed[i] && preds[i].iter().all(|&j| placed[j])).expect("acyclic");
        placed[i] = true;
        out.push(i);
    }
    (out, cycles)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_with_few_moves_and_drops_cycles() {
        let ids: Vec<String> = ["A", "B", "C", "D"].map(String::from).into();
        let rule = |id: &str, after: &[&str]| ModRule { id: id.into(), after: after.iter().map(|s| s.to_string()).collect(), ..Default::default() };
        let names = |o: Vec<usize>| o.into_iter().map(|i| ids[i].as_str()).collect::<String>();
        // Already satisfied: unchanged. A rule naming an unknown mod is ignored.
        assert_eq!(names(order(&ids, &[rule("C", &["A", "Gone"])]).0), "ABCD");
        // A moves to just after D.
        assert_eq!(names(order(&ids, &[rule("A", &["D"])]).0), "BCDA");
        // A and B load after each other: one rule is dropped and reported, the other kept.
        // C after A is not part of the cycle and still holds.
        let (o, cycles) = order(&ids, &[rule("A", &["B"]), rule("B", &["A"]), rule("C", &["A"])]);
        assert_eq!((names(o), cycles), ("BACD".into(), vec![(1, 0)]));
    }
}
