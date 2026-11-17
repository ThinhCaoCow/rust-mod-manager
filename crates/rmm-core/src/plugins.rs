//! Plugin discovery (enabled mods + vanilla Data), header parsing, and plugins.txt writing.
use crate::conflicts::file_map;
use crate::install::is_plugin;
use crate::{Instance, Profile, path_key};
use anyhow::{Context, Result};
use esplugin::{ParseOptions, Plugin};
use loadorder::GameSettings;
use serde::Serialize;
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Debug, Serialize)]
pub struct PluginInfo {
    pub name: String,
    /// Mod providing the plugin; None = game Data folder (base game, Creation Club, manual installs).
    pub mod_id: Option<String>,
    pub enabled: bool,
    /// Always active and loaded first by the game (base masters, Creation Club).
    pub implicit: bool,
    pub master: bool,
    pub light: bool,
    pub masters: Vec<String>,
    /// Masters that are not enabled earlier in the load order.
    pub missing_masters: Vec<String>,
    pub error: Option<String>,
    #[serde(skip)]
    pub path: PathBuf,
}

fn settings(inst: &Instance) -> Result<GameSettings> {
    let c = &inst.config;
    GameSettings::with_local_path(c.game.lo_id(), &c.game_path, &c.local_path).map_err(|e| match &e {
        // ERROR_CLOUD_FILE_PROVIDER_NOT_RUNNING: the game can't read it either.
        loadorder::Error::IoError(path, io) if io.raw_os_error() == Some(362) => anyhow::anyhow!(
            "{} is online-only in OneDrive, and OneDrive isn't running: start OneDrive, or make the My Games folder available offline",
            path.display()
        ),
        _ => e.into(),
    })
}

/// Plugins in the order the game will load them: implicit first, then masters, then the rest,
/// each group following the profile's plugin order.
pub fn list(inst: &Instance, profile: &Profile) -> Result<Vec<PluginInfo>> {
    let game = inst.config.game;
    let implicit: Vec<String> = settings(inst)?.implicitly_active_plugins().to_vec();
    let managed = crate::deploy::managed_keys(inst)?;

    let mut found: Vec<(String, Option<String>, PathBuf)> = Vec::new();
    for (_, (mod_id, rel)) in file_map(inst, profile)?.winners() {
        if !rel.contains('/') && is_plugin(&rel) {
            found.push((rel.clone(), Some(mod_id.clone()), inst.mod_dir(&mod_id).join(&rel)));
        }
    }
    let from_mods: HashSet<String> = found.iter().map(|f| path_key(&f.0)).collect();
    if let Ok(rd) = std::fs::read_dir(inst.data_dir()) {
        for e in rd.filter_map(|e| e.ok()) {
            let name = e.file_name().to_string_lossy().into_owned();
            let key = path_key(&name);
            if is_plugin(&name) && !managed.contains(&format!("data/{key}")) && !from_mods.contains(&key) {
                found.push((name, None, e.path()));
            }
        }
    }

    let rank = |name: &str| -> (usize, usize) {
        if let Some(i) = implicit.iter().position(|p| p.eq_ignore_ascii_case(name)) {
            return (0, i);
        }
        let i = profile.plugins.iter().position(|p| p.name.eq_ignore_ascii_case(name)).unwrap_or(usize::MAX);
        (1, i)
    };
    let mut infos: Vec<PluginInfo> = found
        .into_iter()
        .map(|(name, mod_id, path)| {
            let mut p = Plugin::new(game.esp_id(), &path);
            let parsed = p.parse_file(ParseOptions::header_only());
            let is_implicit = rank(&name).0 == 0;
            let enabled = is_implicit
                || profile.plugins.iter().find(|e| e.name.eq_ignore_ascii_case(&name)).is_some_and(|e| e.enabled);
            PluginInfo {
                enabled,
                implicit: is_implicit,
                master: parsed.is_ok() && p.is_master_file(),
                light: parsed.is_ok() && p.is_light_plugin(),
                masters: p.masters().unwrap_or_default(),
                missing_masters: vec![],
                error: parsed.err().map(|e| e.to_string()),
                name,
                mod_id,
                path,
            }
        })
        .collect();
    infos.sort_by_key(|p| (rank(&p.name).0, !p.master, rank(&p.name).1, p.name.to_lowercase()));

    let mut active: HashSet<String> = HashSet::new();
    for p in &mut infos {
        p.missing_masters = p.masters.iter().filter(|m| !active.contains(&m.to_lowercase())).cloned().collect();
        if p.enabled {
            active.insert(p.name.to_lowercase());
        }
    }
    Ok(infos)
}

/// Write plugins.txt (and loadorder.txt where the game uses one) from the profile.
/// Must run after deploy so every plugin exists in Data.
/// The game loads at most this many full plugins, plus this many light ones.
pub const MAX_FULL_PLUGINS: usize = 254;
pub const MAX_LIGHT_PLUGINS: usize = 4096;

/// Why plugins.txt can't hold the enabled plugins, if it can't.
pub fn over_limit(infos: &[PluginInfo]) -> Option<String> {
    let full = infos.iter().filter(|p| p.enabled && !p.light).count();
    let light = infos.iter().filter(|p| p.enabled && p.light).count();
    if full > MAX_FULL_PLUGINS {
        Some(format!("{full} full plugins are enabled and the game loads {MAX_FULL_PLUGINS}: disable some, or flag small ones light (health_check lists candidates)"))
    } else if light > MAX_LIGHT_PLUGINS {
        Some(format!("{light} light plugins are enabled and the game loads {MAX_LIGHT_PLUGINS}"))
    } else {
        None
    }
}

/// Where the game reads its load order from.
pub fn plugins_txt(inst: &Instance) -> Result<std::path::PathBuf> {
    Ok(settings(inst)?.active_plugins_file().clone())
}

pub fn write_plugins_txt(inst: &Instance, profile: &Profile) -> Result<()> {
    std::fs::create_dir_all(&inst.config.local_path)?;
    let infos = list(inst, profile)?;
    let settings = settings(inst)?;
    crate::profile_ini::keep_plugins_txt(inst, settings.active_plugins_file())?;
    let mut lo = settings.into_load_order();
    lo.load()?;
    let names: Vec<&str> = infos.iter().map(|p| p.name.as_str()).collect();
    lo.set_load_order(&names)?;
    let active: Vec<&str> = infos.iter().filter(|p| p.enabled).map(|p| p.name.as_str()).collect();
    lo.set_active_plugins(&active)?;
    lo.save()?;
    Ok(())
}

/// Two enabled plugins that change the same records (what xEdit shows as conflicts).
#[derive(Debug, Serialize)]
pub struct RecordOverlap {
    /// The plugin that loads first.
    pub plugin: String,
    /// The one that loads later: the game uses its version of the shared records.
    pub wins: String,
    /// How many records both change.
    pub records: usize,
    /// The later plugin has the earlier as a master: a patch that overrides it on purpose.
    pub patch: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_mod: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wins_mod: Option<String>,
}

/// Pairs of enabled plugins that change the same records, most shared first. The base game, DLC
/// and Creation Club plugins are left out: every mod changes their records on purpose. `only`
/// keeps the pairs with that plugin.
// ponytail: reads every enabled plugin whole on each call; cache record ids by size and mtime if it gets slow.
pub fn record_overlaps(inst: &Instance, profile: &Profile, only: Option<&str>) -> Result<Vec<RecordOverlap>> {
    use esplugin::plugins_metadata;
    let game = inst.config.game.esp_id();
    let all = list(inst, profile)?;
    let headers: Vec<Plugin> = all
        .iter()
        .filter_map(|p| {
            let mut h = Plugin::new(game, &p.path);
            h.parse_file(ParseOptions::header_only()).ok().map(|_| h)
        })
        .collect();
    let meta = plugins_metadata(&headers.iter().collect::<Vec<_>>())?;
    let wanted: Vec<&PluginInfo> = all.iter().filter(|p| p.enabled && !p.implicit && p.error.is_none()).collect();
    if let Some(o) = only
        && !wanted.iter().any(|p| p.name.eq_ignore_ascii_case(o))
    {
        anyhow::bail!("{o} isn't an enabled plugin (the base game's and Creation Club's are left out)");
    }
    let mut read = Vec::new();
    for (i, p) in wanted.iter().enumerate() {
        crate::progress::report("reading plugins", i, wanted.len());
        let mut whole = Plugin::new(game, &p.path);
        if whole.parse_file(ParseOptions::whole_plugin()).is_ok() && whole.resolve_record_ids(&meta).is_ok() {
            read.push((*p, whole));
        }
    }
    let mut out = Vec::new();
    for (i, (a, pa)) in read.iter().enumerate() {
        for (b, pb) in &read[i + 1..] {
            if only.is_some_and(|o| !a.name.eq_ignore_ascii_case(o) && !b.name.eq_ignore_ascii_case(o)) || !pa.overlaps_with(pb)? {
                continue;
            }
            out.push(RecordOverlap {
                plugin: a.name.clone(),
                wins: b.name.clone(),
                records: pa.overlap_size(&[pb])?,
                patch: b.masters.iter().any(|m| m.eq_ignore_ascii_case(&a.name)),
                plugin_mod: a.mod_id.clone(),
                wins_mod: b.mod_id.clone(),
            });
        }
    }
    out.sort_by_key(|o| std::cmp::Reverse(o.records));
    Ok(out)
}

/// A record a plugin adds (not a change to a master's), with the ids the console takes.
#[derive(Serialize, Debug, PartialEq)]
pub struct NewRecord {
    /// Record type: QUST, NPC_, WEAP, ARMO, SPEL, CELL, GLOB...
    pub kind: String,
    /// Editor id: what coc (cells), startquest, setstage and movetoqt (quests) and set (globals) take.
    pub editor_id: String,
    /// In-game name; None for localized plugins, whose names live in string files.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Form id in the current load order, for player.placeatme, player.additem, player.addspell;
    /// None while the plugin isn't enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub form_id: Option<String>,
}

#[derive(Serialize, Debug)]
pub struct PluginRecords {
    pub plugin: String,
    pub mod_id: Option<String>,
    pub enabled: bool,
    /// New records of each listed type.
    pub counts: std::collections::BTreeMap<String, usize>,
    pub records: Vec<NewRecord>,
    /// More records than `max` matched.
    pub truncated: bool,
}

/// The record types a test reaches through the console.
pub const TEST_KINDS: &[&str] =
    &["QUST", "NPC_", "LVLN", "WEAP", "ARMO", "AMMO", "SPEL", "SHOU", "PERK", "BOOK", "MISC", "ALCH", "INGR", "SCRL", "KEYM", "CONT", "FURN", "ACTI", "CELL", "WRLD", "GLOB"];

/// The records a plugin adds, of the given types (default `TEST_KINDS`), up to `max`.
pub fn new_records(inst: &Instance, profile: &Profile, plugin: &str, kinds: &[String], max: usize) -> Result<PluginRecords> {
    let all = list(inst, profile)?;
    let p = all.iter().find(|p| p.name.eq_ignore_ascii_case(plugin)).with_context(|| format!("no plugin {plugin} (list_plugins shows them)"))?;
    // Where the game puts it: full plugins count up from 00, light ones share FE.
    let (mut full, mut light, mut slot) = (0u32, 0u32, None);
    for q in all.iter().filter(|q| q.enabled) {
        if std::ptr::eq(q, p) {
            slot = Some(if q.light { 0xFE00_0000 | (light << 12) } else { full << 24 });
        }
        if q.light { light += 1 } else { full += 1 }
    }
    let kinds: HashSet<[u8; 4]> = if kinds.is_empty() { TEST_KINDS.iter().map(|k| k.as_bytes().try_into().unwrap()).collect() } else {
        kinds.iter().map(|k| {
            let k = format!("{:_<4}", k.trim().to_uppercase());
            k.as_bytes().try_into().map_err(|_| anyhow::anyhow!("a record type has 4 letters, like NPC_ or QUST, not {k}"))
        }).collect::<Result<_>>()?
    };
    let bytes = std::fs::read(&p.path)?;
    let mut found = Vec::new();
    let localized = bytes.len() >= 24 && &bytes[..4] == b"TES4" && u32::from_le_bytes(bytes[8..12].try_into()?) & 0x80 != 0;
    let start = 24 + bytes.get(4..8).map_or(0, |s| u32::from_le_bytes(s.try_into().unwrap()) as usize);
    walk_records(bytes.get(start..).unwrap_or_default(), &kinds, &mut found)?;
    let masters = p.masters.len() as u32;
    let mut out = PluginRecords { plugin: p.name.clone(), mod_id: p.mod_id.clone(), enabled: p.enabled, counts: Default::default(), records: vec![], truncated: false };
    for (kind, id, edid, full_name) in found {
        if id >> 24 < masters {
            continue; // a change to a master's record
        }
        let kind = String::from_utf8_lossy(&kind).into_owned();
        *out.counts.entry(kind.clone()).or_default() += 1;
        if kind == "CELL" && edid.is_empty() {
            continue; // an unnamed exterior cell; coc can't go there by name
        }
        if out.records.len() == max {
            out.truncated = true;
            continue;
        }
        let local = if p.light { id & 0xFFF } else { id & 0xFF_FFFF };
        out.records.push(NewRecord { kind, editor_id: edid, name: full_name.filter(|_| !localized), form_id: slot.map(|s| format!("{:08X}", s | local)) });
    }
    Ok(out)
}

/// Records in a run of groups and records: (type, form id as in the file, editor id, name).
fn walk_records(mut b: &[u8], kinds: &HashSet<[u8; 4]>, out: &mut Vec<([u8; 4], u32, String, Option<String>)>) -> Result<()> {
    let u32_at = |b: &[u8], i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
    while b.len() >= 24 {
        let kind: [u8; 4] = b[..4].try_into()?;
        let size = u32_at(b, 4) as usize;
        if &kind == b"GRUP" {
            let end = size.clamp(24, b.len());
            walk_records(&b[24..end], kinds, out)?;
            b = &b[end..];
            continue;
        }
        let end = (24 + size).min(b.len());
        if kinds.contains(&kind) {
            let flags = u32_at(b, 8);
            let data = if flags & 0x0004_0000 != 0 && end >= 28 {
                let mut d = Vec::new();
                std::io::Read::read_to_end(&mut flate2::read::ZlibDecoder::new(&b[28..end]), &mut d).context("a compressed record doesn't unpack")?;
                d
            } else {
                b[24..end].to_vec()
            };
            let (edid, name) = edid_and_name(&data);
            out.push((kind, u32_at(b, 12), edid, name));
        }
        b = &b[end..];
    }
    Ok(())
}

/// EDID and FULL from a record's subrecords.
fn edid_and_name(mut d: &[u8]) -> (String, Option<String>) {
    // Windows-1252 read as Latin-1: close enough for names.
    let text = |s: &[u8]| s.iter().take_while(|&&c| c != 0).map(|&c| c as char).collect::<String>();
    let (mut edid, mut name, mut next_size) = (String::new(), None, None);
    while d.len() >= 6 {
        let size = next_size.take().unwrap_or(u16::from_le_bytes([d[4], d[5]]) as usize);
        let body = &d[6..(6 + size).min(d.len())];
        match &d[..4] {
            b"XXXX" if body.len() >= 4 => next_size = Some(u32::from_le_bytes(body[..4].try_into().unwrap()) as usize),
            b"EDID" => edid = text(body),
            b"FULL" if name.is_none() => name = Some(text(body)),
            _ => {}
        }
        d = &d[(6 + size).min(d.len())..];
    }
    (edid, name)
}

#[cfg(test)]
mod tests {
    #[test]
    fn subrecords_with_a_long_size() {
        let mut d = Vec::new();
        d.extend(b"EDID");
        d.extend(4u16.to_le_bytes());
        d.extend(b"Foo\0");
        d.extend(b"XXXX");
        d.extend(4u16.to_le_bytes());
        d.extend(6u32.to_le_bytes());
        d.extend(b"FULL");
        d.extend(0u16.to_le_bytes());
        d.extend(b"Sword\0");
        assert_eq!(super::edid_and_name(&d), ("Foo".into(), Some("Sword".into())));
    }
}
