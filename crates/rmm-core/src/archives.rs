//! What is inside Bethesda archives (Skyrim SE .bsa, Fallout 4 .ba2) and which plugin loads
//! which archive. Only headers and name tables are read, never file data.
use crate::Game;
use anyhow::{Context, Result, bail};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

/// Data-relative paths (`/`-separated) of the files inside an archive.
pub fn list(path: &Path) -> Result<Vec<String>> {
    let mut r = BufReader::new(File::open(path)?);
    read(&mut r).with_context(|| format!("cannot read {}", path.display()))
}

fn read(r: &mut (impl Read + Seek)) -> Result<Vec<String>> {
    match &bytes::<4>(r)? {
        b"BSA\0" => bsa(r),
        b"BTDX" => ba2(r),
        _ => bail!("not a BSA or BA2 archive"),
    }
}

/// Does the game load `archive` (a file name) for the active plugin `plugin`? Same rules as
/// libloot: Skyrim SE loads `<plugin>.bsa` and `<plugin> - Textures.bsa`, Fallout 4 loads
/// `<plugin> - <anything>.ba2`.
pub fn loaded_by(game: Game, plugin: &str, archive: &str) -> bool {
    let stem = Path::new(plugin).file_stem().unwrap_or_default().to_string_lossy().to_lowercase();
    let a = archive.to_lowercase();
    if game.base() == Game::Fallout4 {
        a.ends_with(".ba2") && a.starts_with(&format!("{stem} - "))
    } else {
        a == format!("{stem}.bsa") || a == format!("{stem} - textures.bsa")
    }
}

pub fn is_archive(name: &str) -> bool {
    let n = name.to_lowercase();
    !n.contains('/') && (n.ends_with(".bsa") || n.ends_with(".ba2"))
}

fn bsa(r: &mut impl Read) -> Result<Vec<String>> {
    let h: Vec<u32> = (0..8).map(|_| u32_le(r)).collect::<Result<_>>()?;
    let (version, flags, folders, files, file_names_len) = (h[0], h[2], h[3] as usize, h[4] as usize, h[6] as usize);
    let record = match version {
        103 | 104 => 16,
        105 => 24,
        v => bail!("unsupported BSA version {v}"),
    };
    if flags & 3 != 3 {
        bail!("the archive doesn't store folder and file names");
    }
    // The counts come from the file: a damaged one mustn't size the memory asked for.
    let mut counts = Vec::with_capacity(folders.min(1 << 16));
    for _ in 0..folders {
        let mut rec = vec![0; record];
        r.read_exact(&mut rec)?;
        counts.push(u32::from_le_bytes(rec[8..12].try_into()?) as usize);
    }
    // Then, per folder: a length-prefixed name and 16-byte file records; then every file name.
    let mut dirs = Vec::with_capacity(counts.len());
    for n in counts {
        let mut name = vec![0; bytes::<1>(r)?[0] as usize];
        r.read_exact(&mut name)?;
        dirs.push((latin1(name.strip_suffix(&[0]).unwrap_or(&name)), n));
        std::io::copy(&mut r.take(16 * n as u64), &mut std::io::sink())?;
    }
    let mut names = Vec::new();
    r.take(file_names_len as u64).read_to_end(&mut names)?;
    if names.len() < file_names_len {
        bail!("the archive ends early");
    }
    let mut names = names.split(|&c| c == 0);
    let mut out = Vec::with_capacity(files.min(1 << 20));
    for (dir, n) in dirs {
        let dir = dir.replace('\\', "/");
        for _ in 0..n {
            let file = latin1(names.next().context("file names end early")?);
            out.push(if dir.is_empty() || dir == "." { file } else { format!("{dir}/{file}") });
        }
    }
    Ok(out)
}

fn ba2(r: &mut (impl Read + Seek)) -> Result<Vec<String>> {
    let _version = u32_le(r)?;
    let kind = bytes::<4>(r)?;
    if &kind != b"GNRL" && &kind != b"DX10" {
        bail!("unsupported BA2 type {}", String::from_utf8_lossy(&kind));
    }
    let count = u32_le(r)?;
    let names_at = u64::from_le_bytes(bytes::<8>(r)?);
    r.seek(SeekFrom::Start(names_at))?;
    (0..count)
        .map(|_| {
            let mut name = vec![0; u16::from_le_bytes(bytes::<2>(r)?) as usize];
            r.read_exact(&mut name)?;
            Ok(latin1(&name).replace('\\', "/"))
        })
        .collect()
}

fn bytes<const N: usize>(r: &mut impl Read) -> Result<[u8; N]> {
    let mut b = [0; N];
    r.read_exact(&mut b)?;
    Ok(b)
}

fn u32_le(r: &mut impl Read) -> Result<u32> {
    Ok(u32::from_le_bytes(bytes::<4>(r)?))
}

/// Archive names are Windows-1252; Latin-1 is the same for every letter that matters here.
fn latin1(b: &[u8]) -> String {
    b.iter().map(|&c| c as char).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn reads_ba2_names_and_plugin_rules() {
        let mut b = b"BTDX".to_vec();
        b.extend(1u32.to_le_bytes());
        b.extend(b"GNRL");
        b.extend(2u32.to_le_bytes());
        b.extend(24u64.to_le_bytes());
        for n in [r"Meshes\a.nif", r"Textures\b.dds"] {
            b.extend((n.len() as u16).to_le_bytes());
            b.extend(n.as_bytes());
        }
        assert_eq!(read(&mut Cursor::new(b)).unwrap(), ["Meshes/a.nif", "Textures/b.dds"]);

        assert!(loaded_by(Game::SkyrimSe, "Foo.esp", "foo - textures.bsa") && loaded_by(Game::SkyrimSe, "Foo.esm", "Foo.bsa"));
        assert!(!loaded_by(Game::SkyrimSe, "Foo.esp", "Foo - Meshes.bsa"));
        assert!(loaded_by(Game::Fallout4, "Foo.esp", "Foo - Main.ba2") && !loaded_by(Game::Fallout4, "Foo.esp", "FooBar - Main.ba2"));
    }
}
