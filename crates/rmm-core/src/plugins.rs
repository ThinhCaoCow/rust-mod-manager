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

/// The plugins the game loads whatever the profile says (the base game's, Creation Club's).
pub(crate) fn implicit(inst: &Instance) -> Result<Vec<String>> {
    Ok(settings(inst)?.implicitly_active_plugins().to_vec())
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
    // A plugin the game can't read can't be ordered either: one stops the whole list (health
    // names it).
    let infos: Vec<&PluginInfo> = infos.iter().filter(|p| p.error.is_none()).collect();
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
fn walk_records(b: &[u8], kinds: &HashSet<[u8; 4]>, out: &mut Vec<([u8; 4], u32, String, Option<String>)>) -> Result<()> {
    each_record(b, &|k| kinds.contains(k), &mut |kind, id, data| {
        let (edid, name) = edid_and_name(data);
        out.push((kind, id, edid, name));
    })
}

/// Call `f` with (type, form id as in the file, unpacked data) for each record of a type `want`
/// takes, in a run of groups and records.
fn each_record(mut b: &[u8], want: &impl Fn(&[u8; 4]) -> bool, f: &mut impl FnMut([u8; 4], u32, &[u8])) -> Result<()> {
    let u32_at = |b: &[u8], i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
    while b.len() >= 24 {
        let kind: [u8; 4] = b[..4].try_into()?;
        let size = u32_at(b, 4) as usize;
        if &kind == b"GRUP" {
            let end = size.clamp(24, b.len());
            each_record(&b[24..end], want, f)?;
            b = &b[end..];
            continue;
        }
        let end = (24 + size).min(b.len());
        if want(&kind) {
            if u32_at(b, 8) & 0x0004_0000 != 0 && end >= 28 {
                let mut d = Vec::new();
                std::io::Read::read_to_end(&mut flate2::read::ZlibDecoder::new(&b[28..end]), &mut d).context("a compressed record doesn't unpack")?;
                f(kind, u32_at(b, 12), &d);
            } else {
                f(kind, u32_at(b, 12), &b[24..end]);
            }
        }
        b = &b[end..];
    }
    Ok(())
}

/// The record types of a plugin's top-level groups (STAT, CELL, WRLD, QUST…): what kinds of
/// records it has, read from the group headers alone.
pub(crate) fn top_groups(path: &std::path::Path) -> Result<HashSet<[u8; 4]>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let mut head = [0u8; 24];
    f.read_exact(&mut head)?;
    f.seek(SeekFrom::Current(u32::from_le_bytes(head[4..8].try_into()?) as i64))?;
    let mut out = HashSet::new();
    while f.read_exact(&mut head).is_ok() && &head[..4] == b"GRUP" {
        out.insert(head[8..12].try_into()?);
        f.seek(SeekFrom::Current((u32::from_le_bytes(head[4..8].try_into()?) as i64 - 24).max(0)))?;
    }
    Ok(out)
}

/// Another enabled plugin that changes records `record_conflicts`' plugin changes too.
#[derive(Serialize, Debug)]
pub struct RecordConflict {
    pub plugin: String,
    pub mod_id: Option<String>,
    /// It loads after the plugin, so the game uses its version of the shared records.
    pub wins: bool,
    /// How many records both change.
    pub records: usize,
    /// Of those, how many the later plugin copies unchanged from the earlier (ITPOs: identical to
    /// the previous override, harmless unless they undo a third plugin's change).
    pub identical: usize,
    /// The later plugin has the earlier as a master: a patch that overrides it on purpose.
    pub patch: bool,
    /// Shared records by type: NPC_, ARMO, CELL...
    pub kinds: std::collections::BTreeMap<String, usize>,
    /// Up to 20 shared records, by editor id (or form id when they have none).
    pub examples: Vec<String>,
}

/// What a plugin's records are, keyed by (the plugin that defines the record, its local id): its
/// type, editor id and a hash of its data.
type Records = std::collections::HashMap<(String, u32), ([u8; 4], String, u64)>;

fn read_records(p: &PluginInfo) -> Result<Records> {
    use std::hash::{Hash, Hasher};
    let bytes = std::fs::read(&p.path)?;
    let start = 24 + bytes.get(4..8).map_or(0, |s| u32::from_le_bytes(s.try_into().unwrap()) as usize);
    let mut out = Records::new();
    each_record(bytes.get(start..).unwrap_or_default(), &|_| true, &mut |kind, id, data| {
        let origin = p.masters.get((id >> 24) as usize).unwrap_or(&p.name).to_lowercase();
        let mut h = std::collections::hash_map::DefaultHasher::new();
        data.hash(&mut h);
        out.insert((origin, id & 0xFF_FFFF), (kind, edid_and_name(data).0, h.finish()));
    })?;
    Ok(out)
}

/// The enabled plugins that change the same records as `plugin`, the one the game uses first
/// (the last to load), with how many each shares, of which types, and how many are identical
/// copies. The base game, DLC and Creation Club plugins are left out, as in `record_overlaps`.
// ponytail: identical means byte-identical data; a copy whose form ids point through a different
// master list counts as changed. Reads every enabled plugin whole on each call.
pub fn record_conflicts(inst: &Instance, profile: &Profile, plugin: &str) -> Result<Vec<RecordConflict>> {
    let all = list(inst, profile)?;
    let on: Vec<&PluginInfo> = all.iter().filter(|p| p.enabled && !p.implicit && p.error.is_none()).collect();
    let at = on.iter().position(|p| p.name.eq_ignore_ascii_case(plugin)).with_context(|| format!("{plugin} isn't an enabled plugin (the base game's and Creation Club's are left out)"))?;
    let mine = read_records(on[at])?;
    let mut out = Vec::new();
    for (i, q) in on.iter().enumerate().filter(|(i, _)| *i != at) {
        crate::progress::report("reading plugins", i, on.len());
        let Ok(theirs) = read_records(q) else { continue };
        let mut c = RecordConflict { plugin: q.name.clone(), mod_id: q.mod_id.clone(), wins: i > at, records: 0, identical: 0, patch: false, kinds: Default::default(), examples: vec![] };
        let (early, late) = if i > at { (on[at], *q) } else { (*q, on[at]) };
        c.patch = late.masters.iter().any(|m| m.eq_ignore_ascii_case(&early.name));
        for (key, (kind, edid, hash)) in &mine {
            let Some((_, _, other)) = theirs.get(key) else { continue };
            c.records += 1;
            c.identical += usize::from(hash == other);
            *c.kinds.entry(String::from_utf8_lossy(kind).into_owned()).or_default() += 1;
            if c.examples.len() < 20 {
                c.examples.push(if edid.is_empty() { format!("{} {:06X} in {}", String::from_utf8_lossy(kind), key.1, key.0) } else { edid.clone() });
            }
        }
        if c.records > 0 {
            out.push((i, c));
        }
    }
    // The last to load first: it's the version the game uses.
    out.sort_by_key(|(i, _)| std::cmp::Reverse(*i));
    Ok(out.into_iter().map(|(_, c)| c).collect())
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


/// Does the plugin add cells or worldspaces of its own (not changes to its masters')?
pub(crate) fn adds_cells(path: &std::path::Path, masters: usize) -> Result<bool> {
    let bytes = std::fs::read(path)?;
    let start = 24 + bytes.get(4..8).map_or(0, |s| u32::from_le_bytes(s.try_into().unwrap()) as usize);
    let mut found = false;
    each_record(bytes.get(start..).unwrap_or_default(), &|k| k == b"CELL" || k == b"WRLD", &mut |_, id, _| {
        found |= (id >> 24) as usize >= masters;
    })?;
    Ok(found)
}

/// Set or clear a plugin's light (ESL) flag. The plugin must come from a mod; its file is
/// replaced, never written in place (deployed copies are hard links), and the mod's old files
/// are kept for undo. Setting it is offered only for `light_candidates`, whose new records fit.
pub fn set_light(inst: &Instance, plugin: &str, light: bool) -> Result<()> {
    let profile = inst.profile()?;
    let all = list(inst, &profile)?;
    let p = all.iter().find(|p| p.name.eq_ignore_ascii_case(plugin)).with_context(|| format!("no plugin {plugin}"))?;
    let id = p.mod_id.clone().with_context(|| format!("{plugin} isn't from a mod; rmm changes only mods' plugins"))?;
    if p.light == light {
        anyhow::bail!("{plugin} is {} light already", if light { "" } else { "not" });
    }
    if light && !p.name.to_lowercase().ends_with(".esl") && !crate::health::esl_candidates(inst, &all).iter().any(|c| c.eq_ignore_ascii_case(plugin)) {
        anyhow::bail!("{plugin} can't be flagged light: its new records don't fit a light plugin's form ids, or it adds cells or worldspaces");
    }
    if !light && p.name.to_lowercase().ends_with(".esl") {
        anyhow::bail!("an .esl file is light by its name");
    }
    let mut bytes = std::fs::read(&p.path)?;
    if bytes.len() < 24 || &bytes[..4] != b"TES4" {
        anyhow::bail!("{plugin} isn't a plugin rmm can read");
    }
    let flags = u32::from_le_bytes(bytes[8..12].try_into()?);
    bytes[8..12].copy_from_slice(&(if light { flags | 0x200 } else { flags & !0x200 }).to_le_bytes());
    let keep = inst.keep_files(&id)?;
    let label = format!("{} {plugin}", if light { "flag light" } else { "unflag light" });
    inst.edit_swapping(&label, &[(&id, &keep)], |_| {
        let tmp = p.path.with_extension("rmm-tmp");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &p.path)?;
        Ok(())
    })
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
