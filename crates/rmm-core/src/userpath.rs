//! The user's PATH, for the installer: it puts its folder (with rmm.exe) on PATH and takes it off
//! again on uninstall (`rmm installer-path`). NSIS can't do it: it reads at most 1024 characters
//! and reads a longer PATH as empty, so it would write back only its own folder.

/// `path` with `dir` added as its last entry, or None when it has it already (letter case and a
/// trailing backslash aside). A PATH ending in ";" keeps it, so `without_dir` gives it back as it was.
pub fn with_dir(path: &str, dir: &str) -> Option<String> {
    if path.split(';').any(|e| same(e, dir)) {
        return None;
    }
    let (body, tail) = match path.strip_suffix(';') {
        Some(b) => (b, ";"),
        None => (path, ""),
    };
    Some(if body.is_empty() { format!("{dir}{tail}") } else { format!("{body};{dir}{tail}") })
}

/// `path` without `dir`, wherever it is, or None when it isn't there.
pub fn without_dir(path: &str, dir: &str) -> Option<String> {
    let kept: Vec<&str> = path.split(';').filter(|e| !same(e, dir)).collect();
    (kept.len() != path.split(';').count()).then(|| kept.join(";"))
}

fn same(entry: &str, dir: &str) -> bool {
    let norm = |s: &str| s.trim().trim_end_matches(['\\', '/']).to_lowercase();
    !entry.trim().is_empty() && norm(entry) == norm(dir)
}

/// Add `dir` to the user's PATH (HKCU\Environment), or take it off. The value keeps its type and
/// any %VARIABLES% in it. Returns whether it changed; open programs are told when it did.
#[cfg(windows)]
pub fn set(dir: &str, add: bool) -> anyhow::Result<bool> {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, RegType};
    use winreg::{RegKey, RegValue};
    let key = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags("Environment", KEY_READ | KEY_WRITE)?;
    let (path, vtype) = match key.get_raw_value("Path") {
        Ok(v) => {
            let wide: Vec<u16> = v.bytes.as_chunks::<2>().0.iter().map(|&c| u16::from_le_bytes(c)).take_while(|&c| c != 0).collect();
            (String::from_utf16(&wide)?, v.vtype)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (String::new(), RegType::REG_EXPAND_SZ),
        Err(e) => return Err(e.into()),
    };
    let Some(new) = (if add { with_dir(&path, dir) } else { without_dir(&path, dir) }) else { return Ok(false) };
    let bytes: Vec<u8> = new.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect();
    key.set_raw_value("Path", &RegValue { bytes: bytes.into(), vtype })?;
    broadcast();
    Ok(true)
}

/// Tell open programs (Explorer, so new terminals) that the environment changed.
#[cfg(windows)]
fn broadcast() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE};
    let what: Vec<u16> = "Environment".encode_utf16().chain([0]).collect();
    let mut result = 0;
    // SAFETY: a broadcast with a NUL-terminated string that outlives the call.
    unsafe { SendMessageTimeoutW(HWND_BROADCAST, WM_SETTINGCHANGE, 0, what.as_ptr() as isize, SMTO_ABORTIFHUNG, 5000, &mut result) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_once_and_removes_from_anywhere() {
        let d = r"C:\Apps\rmm";
        assert_eq!(with_dir("", d).as_deref(), Some(d));
        assert_eq!(with_dir(r"C:\A;C:\B", d).as_deref(), Some(r"C:\A;C:\B;C:\Apps\rmm"));
        for path in [r"C:\A;C:\B;", r"C:\A;C:\B", ""] {
            assert_eq!(without_dir(&with_dir(path, d).unwrap(), d).as_deref(), Some(path), "{path:?} round-trips");
        }
        assert_eq!(with_dir(r"C:\A;c:\apps\RMM\;C:\B", d), None);
        assert_eq!(with_dir(r"C:\Apps\rmm2", d).as_deref(), Some(r"C:\Apps\rmm2;C:\Apps\rmm"));
        let long = format!("{};%USERPROFILE%\\bin", (0..120).map(|i| format!(r"C:\Tools\t{i}")).collect::<Vec<_>>().join(";"));
        let added = with_dir(&long, d).unwrap();
        assert!(added.len() > 1024 && added.starts_with(&long) && added.ends_with(";C:\\Apps\\rmm"), "long PATHs keep every entry and their %VARIABLES%");
        assert_eq!(without_dir(&added, d).as_deref(), Some(long.as_str()));
        assert_eq!(without_dir(r"C:\A;C:\Apps\rmm;C:\B", d).as_deref(), Some(r"C:\A;C:\B"));
        assert_eq!(without_dir(r"C:\Apps\rmm", d).as_deref(), Some(""));
        assert_eq!(without_dir(r"C:\A;C:\Apps\rmm2", d), None);
    }
}
