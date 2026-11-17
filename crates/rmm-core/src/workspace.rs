//! The files the IDE edits, as workspace paths: `mods/<id>/<path>`, `overwrite/<path>`, and
//! `ini/<file>` (the INIs the game reads now, in My Games). Every write keeps the file's old bytes
//! in `edits/` with a log entry, so any edit can be put back.
//!
//! Mod files are written in place: through deploy's hard links the game sees the change at once,
//! and deploy still knows the file as its own.
use crate::instance::{Instance, OVERWRITE};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};

#[derive(Serialize, Debug)]
pub struct Entry {
    pub name: String,
    /// Its workspace path.
    pub path: String,
    pub dir: bool,
    pub size: u64,
}

#[derive(Serialize, Debug)]
pub struct Doc {
    pub path: String,
    pub text: String,
    /// False for compiled scripts (shown disassembled), DLLs and other binaries.
    pub editable: bool,
    /// Of the bytes on disk; a write with an older one is refused.
    pub hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Edit {
    pub n: usize,
    pub path: String,
    /// Seconds since 1970.
    pub at: u64,
    /// "you", "AI", or "revert N".
    pub by: String,
    /// The file didn't exist before.
    pub created: bool,
    pub lines_before: usize,
    pub lines_after: usize,
}

const MAX: u64 = 8 << 20;

#[derive(Clone, Copy, PartialEq)]
enum Enc {
    Utf8,
    Utf8Bom,
    Utf16,
    /// Not UTF-8: one char per byte, as Windows' 8-bit INIs are.
    Latin1,
}

/// Text with how it was stored; None for binary (NULs near the start).
fn decode(b: &[u8]) -> Option<(String, Enc)> {
    if let Some(rest) = b.strip_prefix(&[0xFF, 0xFE]) {
        let units: Vec<u16> = rest.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
        return String::from_utf16(&units).ok().map(|t| (t, Enc::Utf16));
    }
    let (b, bom) = match b.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        Some(rest) => (rest, true),
        None => (b, false),
    };
    if b[..b.len().min(8192)].contains(&0) {
        return None;
    }
    Some(match String::from_utf8(b.to_vec()) {
        Ok(t) => (t, if bom { Enc::Utf8Bom } else { Enc::Utf8 }),
        Err(e) => (e.into_bytes().iter().map(|&c| c as char).collect(), Enc::Latin1),
    })
}

/// Shared with `inspect::read_file`.
pub(crate) fn decode_text(b: &[u8]) -> Option<String> {
    decode(b).map(|(t, _)| t)
}

fn encode(text: &str, enc: Enc) -> Result<Vec<u8>> {
    Ok(match enc {
        Enc::Utf8 => text.as_bytes().to_vec(),
        Enc::Utf8Bom => [&[0xEF, 0xBB, 0xBF][..], text.as_bytes()].concat(),
        Enc::Utf16 => [0xFF, 0xFE].into_iter().chain(text.encode_utf16().flat_map(u16::to_le_bytes)).collect(),
        Enc::Latin1 => text.chars().map(|c| u8::try_from(c as u32).map_err(|_| anyhow::anyhow!("{c:?} can't be saved in this file's 8-bit text"))).collect::<Result<_>>()?,
    })
}

fn hash(b: &[u8]) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    b.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Where a workspace path is: the file, and the mod and path in it (none for INIs).
fn resolve(inst: &Instance, path: &str) -> Result<(PathBuf, Option<(String, String)>)> {
    let parts: Vec<&str> = path.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
    if parts.iter().any(|p| !matches!(Path::new(p).components().next(), Some(Component::Normal(_))) || p.contains(':')) {
        bail!("{path:?} isn't a workspace path (mods/<mod>/..., overwrite/..., ini/<file>)");
    }
    match parts.as_slice() {
        ["mods", id, rest @ ..] => {
            if !inst.mod_dir(id).is_dir() {
                bail!("no mod '{id}' is installed");
            }
            Ok((inst.mod_dir(id).join(rest.join("/")), Some((id.to_string(), rest.join("/")))))
        }
        ["overwrite", rest @ ..] => Ok((inst.mod_dir(OVERWRITE).join(rest.join("/")), Some((OVERWRITE.into(), rest.join("/"))))),
        ["ini"] => Ok((inst.ini_dir().context("the game's INI folder isn't known")?, None)),
        ["ini", name] if name.to_ascii_lowercase().ends_with(".ini") => Ok((inst.ini_file(name).context("the game's INI folder isn't known")?, None)),
        ["ini", ..] => bail!("only the .ini files in the game's INI folder can be opened"),
        _ => bail!("{path:?} isn't a workspace path (mods/<mod>/..., overwrite/..., ini/<file>)"),
    }
}

/// A folder's contents, folders first. "" lists the three roots.
pub fn list(inst: &Instance, path: &str) -> Result<Vec<Entry>> {
    let path = path.trim_matches(['/', '\\']);
    if path.is_empty() {
        let mut roots = vec![("mods", true), ("overwrite", true)];
        roots.extend(inst.ini_dir().map(|_| ("ini", true)));
        return Ok(roots.into_iter().map(|(n, dir)| Entry { name: n.into(), path: n.into(), dir, size: 0 }).collect());
    }
    let dir = if path == "mods" { inst.mods_dir() } else { resolve(inst, path)?.0 };
    if path == "overwrite" && !dir.exists() {
        return Ok(vec![]);
    }
    let mut out: Vec<Entry> = fs::read_dir(&dir)
        .with_context(|| format!("{path} isn't a folder"))?
        .flatten()
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            let name = e.file_name().to_string_lossy().into_owned();
            let keep = match path {
                "mods" => meta.is_dir(),
                "ini" => meta.is_file() && name.to_ascii_lowercase().ends_with(".ini"),
                _ => true,
            };
            keep.then(|| Entry { path: format!("{path}/{name}"), dir: meta.is_dir(), size: if meta.is_dir() { 0 } else { meta.len() }, name })
        })
        .collect();
    out.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(out)
}

/// A file as text. Compiled Papyrus scripts come disassembled and DLLs as a summary, read-only.
pub fn read(inst: &Instance, path: &str) -> Result<Doc> {
    let (file, inside) = resolve(inst, path)?;
    let meta = fs::metadata(&file).with_context(|| format!("there's no file {path}"))?;
    if meta.is_dir() {
        bail!("{path} is a folder");
    }
    if meta.len() > MAX {
        bail!("{path} is over 8 MB, too large to open");
    }
    let bytes = fs::read(&file)?;
    let doc = |text: String, editable, note: Option<String>| Doc { path: path.into(), text, editable, hash: hash(&bytes), note };
    let compiled = bytes.starts_with(b"MZ") || bytes.starts_with(&[0xFA, 0x57, 0xC0, 0xDE]) || bytes.starts_with(&[0xDE, 0xC0, 0x57, 0xFA]);
    if let (true, Some((id, rel))) = (compiled, &inside) {
        let v = crate::inspect::read_file(inst, id, rel, 0, usize::MAX)?;
        let text = v.text.clone().unwrap_or_else(|| serde_json::to_string_pretty(&v.native).unwrap_or_default());
        return Ok(doc(text, false, Some(if v.kind == "papyrus" { "A compiled script, disassembled; edit its .psc source instead." } else { "A program; this is what it imports and contains." }.into())));
    }
    Ok(match decode(&bytes) {
        Some((text, _)) => doc(text, true, None),
        None => doc(String::new(), false, Some("A binary file; it can't be shown as text.".into())),
    })
}

fn edits_dir(inst: &Instance) -> PathBuf {
    inst.root.join("edits")
}

/// Every edit, newest first.
pub fn edits(inst: &Instance) -> Result<Vec<Edit>> {
    let mut log: Vec<Edit> = match fs::read_to_string(edits_dir(inst).join("log.json")) {
        Ok(t) => serde_json::from_str(&t)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
        Err(e) => return Err(e.into()),
    };
    log.reverse();
    Ok(log)
}

/// Save `text` to `path` (made if it doesn't exist, except INIs), keeping the old bytes. With
/// `expect`, refuses when the file changed since it was read with that hash.
pub fn write(inst: &Instance, path: &str, text: &str, expect: Option<&str>, by: &str) -> Result<Edit> {
    let (file, inside) = resolve(inst, path)?;
    if inside.as_ref().is_some_and(|(_, rel)| rel.is_empty()) {
        bail!("give a file inside the mod");
    }
    let old = match fs::read(&file) {
        Ok(b) => Some(b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("cannot read {path}")),
    };
    if old.is_none() && inside.is_none() {
        bail!("{path} doesn't exist; the game makes its own INIs");
    }
    if let (Some(want), Some(b)) = (expect, &old)
        && hash(b) != want
    {
        bail!("{path} changed since it was opened; open it again");
    }
    let (enc, before) = match &old {
        Some(b) if b.starts_with(b"MZ") || b.starts_with(&[0xFA, 0x57, 0xC0, 0xDE]) || b.starts_with(&[0xDE, 0xC0, 0x57, 0xFA]) => bail!("{path} is compiled; only text files can be edited"),
        Some(b) => match decode(b) {
            Some((t, enc)) => (enc, t.lines().count()),
            None => bail!("{path} is binary; only text files can be edited"),
        },
        None => (Enc::Utf8, 0),
    };
    let bytes = encode(text, enc)?;
    let mut log = edits(inst)?;
    log.reverse();
    let n = log.last().map_or(1, |e| e.n + 1);
    fs::create_dir_all(edits_dir(inst))?;
    if let Some(b) = &old {
        fs::write(edits_dir(inst).join(format!("{n}.bak")), b)?;
    }
    fs::create_dir_all(file.parent().context("no folder")?)?;
    // In place, not through a temp file: a deployed hard link keeps pointing at this file.
    fs::write(&file, bytes)?;
    let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let edit = Edit { n, path: path.trim_matches(['/', '\\']).replace('\\', "/"), at, by: by.into(), created: old.is_none(), lines_before: before, lines_after: text.lines().count() };
    log.push(edit.clone());
    crate::instance::write_atomic(&edits_dir(inst).join("log.json"), &serde_json::to_string_pretty(&log)?)?;
    Ok(edit)
}

/// Replace exactly one occurrence of `old` with `new` (the AI's edits). Fails when `old` isn't
/// there or is there more than once, so the edit lands where it was meant to.
pub fn replace(inst: &Instance, path: &str, old: &str, new: &str, by: &str) -> Result<Edit> {
    let doc = read(inst, path)?;
    if !doc.editable {
        bail!("{path} can't be edited as text");
    }
    match doc.text.matches(old).count() {
        0 => bail!("the text to replace isn't in {path}; read the file again"),
        1 => write(inst, path, &doc.text.replacen(old, new, 1), Some(&doc.hash), by),
        n => bail!("the text to replace is in {path} {n} times; include more of the lines around it"),
    }
}

/// Put back the file as it was before edit `n` (a file the edit made is moved into `edits/`).
/// The revert is an edit of its own, so it can be reverted too.
pub fn revert(inst: &Instance, n: usize) -> Result<Edit> {
    let e = edits(inst)?.into_iter().find(|e| e.n == n).with_context(|| format!("there's no edit {n}"))?;
    let by = format!("revert {n}");
    if !e.created {
        let old = fs::read(edits_dir(inst).join(format!("{n}.bak"))).with_context(|| format!("edit {n}'s backup is missing"))?;
        let text = decode(&old).map(|(t, _)| t).context("the backup isn't text")?;
        return write(inst, &e.path, &text, None, &by);
    }
    let (file, _) = resolve(inst, &e.path)?;
    let doc = read(inst, &e.path)?;
    let mut edit = write(inst, &e.path, &doc.text, None, &by)?; // backs it up first
    fs::remove_file(&file)?;
    edit.lines_after = 0;
    let mut log = edits(inst)?;
    log.reverse();
    if let Some(last) = log.last_mut() {
        last.lines_after = 0;
    }
    crate::instance::write_atomic(&edits_dir(inst).join("log.json"), &serde_json::to_string_pretty(&log)?)?;
    Ok(edit)
}
