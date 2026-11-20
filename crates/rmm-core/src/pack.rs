//! Pack a mod's loose files into Bethesda archives (MO2's BSA packer): Skyrim SE gets
//! `<plugin>.bsa` and `<plugin> - Textures.bsa`, Fallout 4 `<plugin> - Main.ba2` (general files,
//! zlib) and `<plugin> - Textures.ba2` (DDS textures). The archives are named after one of the
//! mod's plugins, which is what makes the game load them. One undo step.
use crate::{Game, Instance, archives, path_key};
use anyhow::{Context, Result, bail};
use flate2::{Compression, write::ZlibEncoder};
use serde::Serialize;
use std::fs::{self, File};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// Folders the game reads from archives. Everything else (SKSE and F4SE plugins, their settings,
/// tool folders, read-me's, video) stays loose.
const PACKED_DIRS: &[&str] = &[
    "meshes", "textures", "sound", "music", "scripts", "interface", "seq", "grass", "lodsettings",
    "strings", "terrain", "vis", "materials", "shadersfx",
];
/// Skyrim SE's archives use 32-bit offsets.
const BSA_MAX: u64 = 2_000_000_000;

#[derive(Debug, Serialize)]
pub struct Packed {
    pub name: String,
    pub files: usize,
    pub bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct PackReport {
    pub plugin: String,
    pub archives: Vec<Packed>,
    /// Files that stay loose: outside the archive folders, or textures Fallout 4's archives
    /// can't take.
    pub kept: Vec<String>,
    /// Files this mod wins now that another enabled mod has loose: packed, they lose to it,
    /// because every loose file beats every archive.
    pub loses: Vec<String>,
}

/// What a file goes into: the general archive or the textures one.
fn target(game: Game, rel: &str) -> Option<bool> {
    let low = rel.to_lowercase();
    let (top, _) = low.split_once('/')?;
    if !PACKED_DIRS.contains(&top) || low.starts_with("scripts/source/") || archives::is_archive(&low) {
        return None;
    }
    let textures = top == "textures" && (game.base() != Game::Fallout4 || low.ends_with(".dds"));
    Some(textures)
}

/// Pack mod `id`'s loose files into archives named after `plugin` (or its first plugin).
/// `dry_run` only reports.
pub fn pack(inst: &Instance, id: &str, plugin: Option<&str>, dry_run: bool) -> Result<PackReport> {
    if crate::instance::is_separator(id) || id == crate::instance::OVERWRITE || !inst.mod_dir(id).is_dir() {
        bail!("'{id}' is not an installed mod");
    }
    let game = inst.config.game;
    let files = inst.mod_files(id)?;
    let mut plugins: Vec<&String> = files.iter().filter(|f| !f.contains('/') && crate::install::is_plugin(f)).collect();
    // The main plugin, not an optional patch: masters first, then the shortest name.
    plugins.sort_by_key(|f| (!f.to_lowercase().ends_with(".esm"), f.len()));
    let plugin = match plugin {
        Some(p) => plugins.iter().find(|f| f.eq_ignore_ascii_case(p)).with_context(|| format!("{id} has no plugin {p}"))?.to_string(),
        None => plugins.first().with_context(|| format!("{id} has no plugin to load its archives; the game loads an archive only beside a plugin of the same name"))?.to_string(),
    };
    let stem = Path::new(&plugin).file_stem().unwrap_or_default().to_string_lossy().into_owned();
    let fo4 = game.base() == Game::Fallout4;
    let names = if fo4 { [format!("{stem} - Main.ba2"), format!("{stem} - Textures.ba2")] } else { [format!("{stem}.bsa"), format!("{stem} - Textures.bsa")] };

    let dir = inst.mod_dir(id);
    let (mut general, mut textures, mut kept) = (Vec::new(), Vec::new(), Vec::new());
    for f in &files {
        // Archives store Windows-1252 names; a path beyond plain ASCII stays loose rather than risk a wrong name.
        match target(game, f).filter(|_| f.is_ascii()) {
            Some(true) if fo4 && dds(&dir.join(f)).is_err() => kept.push(f.clone()),
            Some(true) => textures.push(f.clone()),
            Some(false) => general.push(f.clone()),
            None => kept.push(f.clone()),
        }
    }
    if general.is_empty() && textures.is_empty() {
        bail!("{id} has no loose files to pack");
    }
    for (name, list) in names.iter().zip([&general, &textures]) {
        if !list.is_empty() && files.iter().any(|f| f.eq_ignore_ascii_case(name)) {
            bail!("{id} already has {name}");
        }
    }

    // What changes hands: a loose file of another mod beats this mod's packed copy.
    let profile = inst.profile()?;
    let map = crate::conflicts::file_map(inst, &profile)?;
    let me = map.mods.iter().position(|m| m == id);
    let loses = general
        .iter()
        .chain(&textures)
        .filter(|f| map.files.get(&path_key(f)).is_some_and(|(_, owners)| owners.len() > 1 && me.is_some_and(|m| owners.last() == Some(&m))))
        .cloned()
        .collect();

    let size = |list: &[String]| list.iter().map(|f| fs::metadata(dir.join(f)).map_or(0, |m| m.len())).sum::<u64>();
    let mut report = PackReport { plugin, archives: vec![], kept, loses };
    for (name, list) in names.iter().zip([&general, &textures]) {
        if list.is_empty() {
            continue;
        }
        let bytes = size(list);
        if !fo4 && bytes > BSA_MAX {
            bail!("{name} would hold {} GB, more than a Skyrim SE archive can (2 GB)", bytes / 1_000_000_000);
        }
        report.archives.push(Packed { name: name.clone(), files: list.len(), bytes });
    }
    if dry_run {
        return Ok(report);
    }

    let keep = inst.keep_files(id)?;
    let label = format!("pack {id} into {}", report.archives.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(" and "));
    inst.edit_swapping(&label, &[(id, &keep)], |_| {
        for (name, list) in names.iter().zip([&general, &textures]) {
            if list.is_empty() {
                continue;
            }
            let out = dir.join(name);
            let written = match (fo4, list == &textures) {
                (false, _) => write_bsa(&out, &dir, list),
                (true, false) => write_gnrl(&out, &dir, list),
                (true, true) => write_dx10(&out, &dir, list),
            };
            if let Err(e) = written {
                let _ = fs::remove_file(&out);
                return Err(e.context(format!("cannot write {name}")));
            }
        }
        for f in general.iter().chain(&textures) {
            let p = dir.join(f);
            fs::remove_file(&p)?;
            crate::install::prune_empty(&dir, &p);
        }
        inst.forget_index(id);
        Ok(())
    })?;
    Ok(report)
}

/// Lower case, backslashes: how archives store and hash paths.
fn norm(rel: &str) -> String {
    rel.replace('/', "\\").to_lowercase()
}

// ---- Skyrim SE: BSA version 105, uncompressed ----

/// Bethesda's hash of a folder path or a file name (stem and extension apart).
fn bsa_hash(s: &[u8]) -> u64 {
    let crc = |b: &[u8]| b.iter().fold(0u32, |c, &x| u32::from(x).wrapping_add(c.wrapping_mul(0x1003F)));
    let n = s.len();
    let (mut first, mut last, mut last2) = (0u8, 0u8, 0u8);
    if n >= 1 {
        first = s[0];
        last = s[n - 1];
    }
    if n >= 3 {
        last2 = s[n - 2];
    }
    let c = if n > 3 { crc(&s[1..n - 2]) } else { 0 };
    u64::from(last) | u64::from(last2) << 8 | u64::from(n as u8) << 16 | u64::from(first) << 24 | u64::from(c) << 32
}

fn bsa_file_hash(name: &str) -> u64 {
    let (stem, ext) = name.rfind('.').map_or((name, ""), |i| name.split_at(i));
    let mut h = bsa_hash(stem.as_bytes());
    let crc = ext.bytes().fold(0u32, |c, x| u32::from(x).wrapping_add(c.wrapping_mul(0x1003F)));
    let (mut first, mut last, mut last2, len, c) = (h >> 24 & 0xFF, h & 0xFF, h >> 8 & 0xFF, h >> 16 & 0xFF, (h >> 32) as u32);
    let c = c.wrapping_add(crc);
    if let Some(i) = ["", ".nif", ".kf", ".dds", ".wav", ".adp"].iter().position(|e| *e == ext) {
        let i = i as u64;
        first = (first + 32 * (i & 0xFC)) & 0xFF;
        last = (last + ((i & 0xFE) << 6)) & 0xFF;
        last2 = (last2 + (i << 7)) & 0xFF;
    }
    h = last | last2 << 8 | len << 16 | first << 24 | u64::from(c) << 32;
    h
}

/// A BSA folder: hash, name, and its files (hash, name, path).
type Folder = (u64, String, Vec<(u64, String, String)>);

fn write_bsa(out: &Path, dir: &Path, files: &[String]) -> Result<()> {
    // folder -> files, both sorted by hash
    let mut folders: Vec<Folder> = Vec::new();
    for f in files {
        let n = norm(f);
        let (folder, name) = n.rsplit_once('\\').expect("packed files are in folders");
        let fh = bsa_hash(folder.as_bytes());
        let i = folders.iter().position(|(h, d, _)| *h == fh && d == folder).unwrap_or_else(|| {
            folders.push((fh, folder.to_owned(), vec![]));
            folders.len() - 1
        });
        folders[i].2.push((bsa_file_hash(name), name.to_owned(), f.clone()));
    }
    folders.sort_by_key(|(h, ..)| *h);
    for (_, _, list) in &mut folders {
        list.sort_by_key(|(h, ..)| *h);
    }
    let file_count: usize = folders.iter().map(|f| f.2.len()).sum();
    let folder_names: usize = folders.iter().map(|f| f.1.len() + 1).sum();
    let file_names: usize = folders.iter().flat_map(|f| &f.2).map(|f| f.1.len() + 1).sum();
    let mut flags = 0u16;
    for (_, d, _) in &folders {
        let top = d.split('\\').next().unwrap_or_default();
        flags |= match top {
            "meshes" => 1,
            "textures" => 2,
            "interface" => 4,
            "sound" if d.starts_with("sound\\voice") => 0x10,
            "sound" | "music" => 8,
            _ => 0x100,
        };
    }

    let mut w = BufWriter::new(File::create(out)?);
    w.write_all(b"BSA\0")?;
    for v in [105u32, 36, 3, folders.len() as u32, file_count as u32, folder_names as u32, file_names as u32] {
        w.write_all(&v.to_le_bytes())?;
    }
    w.write_all(&u32::from(flags).to_le_bytes())?;
    // Folder records; each offset points at the folder's name block, plus the file names' length
    // (Bethesda's quirk).
    let mut at = 36 + folders.len() * 24;
    for (h, d, list) in &folders {
        w.write_all(&h.to_le_bytes())?;
        w.write_all(&(list.len() as u32).to_le_bytes())?;
        w.write_all(&0u32.to_le_bytes())?;
        w.write_all(&((at + file_names) as u64).to_le_bytes())?;
        at += 1 + d.len() + 1 + 16 * list.len();
    }
    let mut data_at = (at + file_names) as u64;
    for (_, d, list) in &folders {
        w.write_all(&[(d.len() + 1) as u8])?;
        w.write_all(d.as_bytes())?;
        w.write_all(&[0])?;
        for (h, _, rel) in list {
            let len = fs::metadata(dir.join(rel))?.len();
            if len >= 0x4000_0000 {
                bail!("{rel} is too big for an archive");
            }
            w.write_all(&h.to_le_bytes())?;
            w.write_all(&(len as u32).to_le_bytes())?;
            w.write_all(&(data_at as u32).to_le_bytes())?;
            data_at += len;
        }
    }
    for (_, _, list) in &folders {
        for (_, name, _) in list {
            w.write_all(name.as_bytes())?;
            w.write_all(&[0])?;
        }
    }
    for (_, _, list) in &folders {
        for (_, _, rel) in list {
            std::io::copy(&mut File::open(dir.join(rel))?, &mut w)?;
        }
    }
    w.flush()?;
    Ok(())
}

// ---- Fallout 4: BA2 version 1, GNRL and DX10 ----

fn crc32(b: &[u8]) -> u32 {
    let mut c = 0u32;
    for &x in b {
        c ^= u32::from(x);
        for _ in 0..8 {
            c = if c & 1 != 0 { c >> 1 ^ 0xEDB8_8320 } else { c >> 1 };
        }
    }
    c
}

/// A BA2 record's hash: the stem's, the extension's first four bytes, the folder's.
fn ba2_hash(rel: &str) -> [u8; 12] {
    let n = norm(rel);
    let (folder, name) = n.rsplit_once('\\').unwrap_or(("", &n));
    let (stem, ext) = name.rsplit_once('.').unwrap_or((name, ""));
    let mut e = [0u8; 4];
    for (i, b) in ext.bytes().take(4).enumerate() {
        e[i] = b;
    }
    let mut out = [0u8; 12];
    out[..4].copy_from_slice(&crc32(stem.as_bytes()).to_le_bytes());
    out[4..8].copy_from_slice(&e);
    out[8..].copy_from_slice(&crc32(folder.as_bytes()).to_le_bytes());
    out
}

fn zlib(data: &[u8]) -> Result<Vec<u8>> {
    let mut z = ZlibEncoder::new(Vec::new(), Compression::default());
    z.write_all(data)?;
    Ok(z.finish()?)
}

/// Header, room for the records, then each file's data; the records and the name table follow
/// once the sizes are known.
fn write_ba2(out: &Path, kind: &[u8; 4], record: usize, files: &[String], mut each: impl FnMut(&str, &mut Vec<u8>, &mut BufWriter<File>, u64) -> Result<()>) -> Result<()> {
    let mut w = BufWriter::new(File::create(out)?);
    w.write_all(&vec![0; 24 + record * files.len()])?;
    let mut records = Vec::with_capacity(record * files.len());
    let mut at = (24 + record * files.len()) as u64;
    for f in files {
        let before = records.len();
        each(f, &mut records, &mut w, at)?;
        debug_assert_eq!(records.len() - before, record);
        at = w.stream_position()?;
    }
    let names_at = at;
    for f in files {
        let n = f.replace('/', "\\");
        w.write_all(&(n.len() as u16).to_le_bytes())?;
        w.write_all(n.as_bytes())?;
    }
    w.seek(SeekFrom::Start(0))?;
    w.write_all(b"BTDX")?;
    w.write_all(&1u32.to_le_bytes())?;
    w.write_all(kind)?;
    w.write_all(&(files.len() as u32).to_le_bytes())?;
    w.write_all(&names_at.to_le_bytes())?;
    w.write_all(&records)?;
    w.flush()?;
    Ok(())
}

/// Write `data` (zlib unless that doesn't help or it's sound) and its chunk sizes.
fn chunk(rec: &mut Vec<u8>, w: &mut BufWriter<File>, at: u64, data: &[u8], compress: bool) -> Result<()> {
    let packed = if compress { Some(zlib(data)?).filter(|z| z.len() < data.len()) } else { None };
    rec.extend(at.to_le_bytes());
    rec.extend((packed.as_ref().map_or(0, Vec::len) as u32).to_le_bytes());
    rec.extend((data.len() as u32).to_le_bytes());
    w.write_all(packed.as_deref().unwrap_or(data))?;
    Ok(())
}

fn write_gnrl(out: &Path, dir: &Path, files: &[String]) -> Result<()> {
    write_ba2(out, b"GNRL", 36, files, |f, rec, w, at| {
        let data = fs::read(dir.join(f))?;
        rec.extend(ba2_hash(f));
        rec.extend([0u8, 1]);
        rec.extend(0x10u16.to_le_bytes());
        let low = f.to_lowercase();
        chunk(rec, w, at, &data, !(low.ends_with(".wav") || low.ends_with(".xwm") || low.ends_with(".fuz")))?;
        rec.extend(0xBAAD_F00Du32.to_le_bytes());
        Ok(())
    })
}

/// What a DX10 record needs from a DDS file: size, mips, DXGI format, cubemap, and where the
/// pixel data starts.
struct Dds {
    height: u16,
    width: u16,
    mips: u8,
    format: u8,
    cube: bool,
    data_at: usize,
}

fn dds(path: &Path) -> Result<Dds> {
    let mut h = [0u8; 148];
    let n = File::open(path)?.read(&mut h)?;
    if n < 128 || &h[..4] != b"DDS " {
        bail!("not a DDS file");
    }
    let u = |i: usize| u32::from_le_bytes(h[i..i + 4].try_into().unwrap());
    let (height, width, mips, pf_flags, cc, bits, rmask, amask, caps2) = (u(12), u(16), u(28).max(1), u(80), &h[84..88], u(88), u(92), u(104), u(112));
    let (format, data_at, cube) = match cc {
        b"DX10" if n >= 148 => (u(128), 148, u(136) & 4 != 0),
        b"DXT1" => (71, 128, false),
        b"DXT3" => (74, 128, false),
        b"DXT5" => (77, 128, false),
        b"ATI1" | b"BC4U" => (80, 128, false),
        b"BC4S" => (81, 128, false),
        b"ATI2" | b"BC5U" => (83, 128, false),
        b"BC5S" => (84, 128, false),
        _ if pf_flags & 0x40 != 0 && bits == 32 => (if rmask == 0xFF { 28 } else if amask != 0 { 87 } else { 88 }, 128, false),
        _ => bail!("a texture format Fallout 4's archives don't take"),
    };
    if height > 0xFFFF || width > 0xFFFF || format > 0xFF {
        bail!("an unusual texture");
    }
    Ok(Dds { height: height as u16, width: width as u16, mips: mips.min(255) as u8, format: format as u8, cube: cube || caps2 & 0x200 != 0, data_at })
}

fn write_dx10(out: &Path, dir: &Path, files: &[String]) -> Result<()> {
    write_ba2(out, b"DX10", 48, files, |f, rec, w, at| {
        let p = dir.join(f);
        let d = dds(&p)?;
        let data = fs::read(&p)?;
        rec.extend(ba2_hash(f));
        rec.extend([0u8, 1]);
        rec.extend(0x18u16.to_le_bytes());
        rec.extend(d.height.to_le_bytes());
        rec.extend(d.width.to_le_bytes());
        rec.extend([d.mips, d.format, u8::from(d.cube), 8]);
        chunk(rec, w, at, &data[d.data_at..], true)?;
        rec.extend(0u16.to_le_bytes());
        rec.extend((u16::from(d.mips) - 1).to_le_bytes());
        rec.extend(0xBAAD_F00Du32.to_le_bytes());
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_match_bethesda() {
        // Known values from BSA tools.
        assert_eq!(bsa_hash(b"textures\\armor\\amuletsandrings\\elder council"), 0x04BC_422C_742C_696C);
        assert_eq!(bsa_hash(b"textures\\architecture\\windhelm"), 0xC1D9_7EBE_741E_6C6D);
        let h = ba2_hash("Sound/Voice/Fallout4.esm/RobotMrHandy/Mar\u{ed}a_M.fuz");
        // The test name isn't Latin-1 bytes here (UTF-8), so check only the folder and extension.
        assert_eq!(&h[4..8], b"fuz\0");
        assert_eq!(u32::from_le_bytes(h[8..].try_into().unwrap()), 0x8A9C_014E);
    }

    /// Against real archives (read only): RMM_REAL_BSA (Skyrim SE) and RMM_REAL_BA2 (Fallout 4
    /// GNRL). Every stored hash must be the one this module computes from the name.
    #[test]
    #[ignore]
    fn hashes_match_real_archives() {
        use std::io::BufReader;
        let u32_at = |b: &[u8], i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        if let Ok(p) = std::env::var("RMM_REAL_BSA") {
            let b = fs::read(&p).unwrap();
            let (folders, names_len) = (u32_at(&b, 16) as usize, u32_at(&b, 28) as usize);
            let mut at = 36 + 24 * folders;
            let mut recs = vec![];
            for i in 0..folders {
                let r = 36 + 24 * i;
                let count = u32_at(&b, r + 8) as usize;
                let len = b[at] as usize;
                let name = String::from_utf8_lossy(&b[at + 1..at + len]).into_owned();
                assert_eq!(bsa_hash(name.as_bytes()), u64::from_le_bytes(b[r..r + 8].try_into().unwrap()), "folder {name}");
                at += 1 + len;
                for _ in 0..count {
                    recs.push(u64::from_le_bytes(b[at..at + 8].try_into().unwrap()));
                    at += 16;
                }
            }
            let names: Vec<String> = b[at..at + names_len].split(|&c| c == 0).take(recs.len()).map(|n| String::from_utf8_lossy(n).to_lowercase()).collect();
            for (h, n) in recs.iter().zip(&names) {
                assert_eq!(bsa_file_hash(n), *h, "file {n}");
            }
            println!("{} folders, {} files match", folders, names.len());
        }
        if let Ok(p) = std::env::var("RMM_REAL_BA2") {
            let mut r = BufReader::new(File::open(&p).unwrap());
            let mut h = [0u8; 24];
            r.read_exact(&mut h).unwrap();
            let count = u32_at(&h, 12) as usize;
            let mut recs = vec![0u8; 36 * count];
            r.read_exact(&mut recs).unwrap();
            r.seek(SeekFrom::Start(u64::from_le_bytes(h[16..24].try_into().unwrap()))).unwrap();
            for i in 0..count {
                let mut l = [0u8; 2];
                r.read_exact(&mut l).unwrap();
                let mut n = vec![0u8; u16::from_le_bytes(l) as usize];
                r.read_exact(&mut n).unwrap();
                let name: String = n.iter().map(|&c| c as char).collect();
                assert_eq!(ba2_hash(&name), recs[36 * i..36 * i + 12], "file {name}");
            }
            println!("{count} BA2 files match");
        }
    }

    #[test]
    fn bsa_and_ba2_read_back() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path();
        for f in ["meshes/a/b.nif", "meshes/a/c.nif", "scripts/x.pex"] {
            fs::create_dir_all(src.join(f).parent().unwrap()).unwrap();
            fs::write(src.join(f), f.repeat(50)).unwrap();
        }
        let list: Vec<String> = ["meshes/a/b.nif", "meshes/a/c.nif", "scripts/x.pex"].map(String::from).to_vec();
        write_bsa(&src.join("t.bsa"), src, &list).unwrap();
        let mut got = archives::list(&src.join("t.bsa")).unwrap();
        got.sort();
        assert_eq!(got, ["meshes/a/b.nif", "meshes/a/c.nif", "scripts/x.pex"]);
        write_gnrl(&src.join("t.ba2"), src, &list).unwrap();
        assert_eq!(archives::list(&src.join("t.ba2")).unwrap(), list);
        // Each GNRL record's data comes back (zlib or stored) as the file was.
        let b = fs::read(src.join("t.ba2")).unwrap();
        let u32_at = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        for (i, f) in list.iter().enumerate() {
            let r = 24 + 36 * i;
            let (at, packed, size) = (u64::from_le_bytes(b[r + 16..r + 24].try_into().unwrap()) as usize, u32_at(r + 24) as usize, u32_at(r + 28) as usize);
            let data = if packed == 0 { b[at..at + size].to_vec() } else {
                let mut d = Vec::new();
                flate2::read::ZlibDecoder::new(&b[at..at + packed]).read_to_end(&mut d).unwrap();
                d
            };
            assert_eq!(data, fs::read(src.join(f)).unwrap(), "{f}");
            assert_eq!(u32_at(r + 32), 0xBAAD_F00D);
        }

        // A DXT1 texture: a DX10 record with its size, mips and format, and the pixels without
        // the DDS header.
        let mut dds = vec![0u8; 128];
        dds[..4].copy_from_slice(b"DDS ");
        dds[4..8].copy_from_slice(&124u32.to_le_bytes());
        dds[12..16].copy_from_slice(&8u32.to_le_bytes());
        dds[16..20].copy_from_slice(&4u32.to_le_bytes());
        dds[28..32].copy_from_slice(&2u32.to_le_bytes());
        dds[84..88].copy_from_slice(b"DXT1");
        dds.extend([7u8; 24]);
        fs::create_dir_all(src.join("textures")).unwrap();
        fs::write(src.join("textures/t.dds"), &dds).unwrap();
        write_dx10(&src.join("t - Textures.ba2"), src, &["textures/t.dds".to_owned()]).unwrap();
        let b = fs::read(src.join("t - Textures.ba2")).unwrap();
        assert_eq!(&b[8..12], b"DX10");
        let r = 24;
        assert_eq!((u16::from_le_bytes([b[r + 16], b[r + 17]]), u16::from_le_bytes([b[r + 18], b[r + 19]]), b[r + 20], b[r + 21]), (8, 4, 2, 71));
        let (at, packed, size) = (u64::from_le_bytes(b[r + 24..r + 32].try_into().unwrap()) as usize, u32::from_le_bytes(b[r + 32..r + 36].try_into().unwrap()) as usize, u32::from_le_bytes(b[r + 36..r + 40].try_into().unwrap()) as usize);
        let mut d = Vec::new();
        if packed == 0 { d = b[at..at + size].to_vec() } else { flate2::read::ZlibDecoder::new(&b[at..at + packed]).read_to_end(&mut d).unwrap(); }
        assert_eq!(d, [7u8; 24]);
        assert_eq!((u16::from_le_bytes([b[r + 40], b[r + 41]]), u16::from_le_bytes([b[r + 42], b[r + 43]])), (0, 1));
        assert_eq!(archives::list(&src.join("t - Textures.ba2")).unwrap(), ["textures/t.dds"]);

        assert_eq!(target(Game::SkyrimSe, "SKSE/Plugins/x.dll"), None);
        assert_eq!(target(Game::SkyrimSe, "textures/x.dds"), Some(true));
        assert_eq!(target(Game::Fallout4, "textures/x.txt"), Some(false));
    }
}
