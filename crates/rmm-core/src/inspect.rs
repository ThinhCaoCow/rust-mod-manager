//! Looking inside mods: a safety scan (Windows Defender, plus what each DLL and EXE imports and
//! contains) and reading a mod's files (text as text, compiled Papyrus scripts disassembled, DLLs
//! summarised), so an AI can judge what a mod does.
use crate::Instance;
use crate::instance::is_separator;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Something a scan found in one file.
#[derive(Serialize, Debug)]
pub struct Finding {
    pub file: String,
    /// "danger", "warning" or "info".
    pub severity: &'static str,
    pub message: String,
}

#[derive(Serialize, Debug)]
pub struct Scan {
    pub mod_id: String,
    /// "threat" (Defender found malware, or a program poses as another file type), "review"
    /// (something to look at) or "clean".
    pub verdict: &'static str,
    /// What Windows Defender said; None when it didn't run (scans of every mod skip it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defender: Option<Defender>,
    /// DLLs and EXEs: native code, which runs with the game's (or the user's) full rights.
    pub native_code: Vec<String>,
    /// Compiled Papyrus scripts: sandboxed in the game, read them for what they do.
    pub scripts: usize,
    pub findings: Vec<Finding>,
    /// The saved review, if the mod has one; with `current` true its files haven't changed since.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewState>,
}

/// What a reviewer (an AI or the user) concluded about a mod's code, kept with the mod.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, schemars::JsonSchema)]
pub struct Review {
    /// "clean", "suspicious" or "malicious".
    pub verdict: String,
    /// What the mod does and how, what was checked, and anything notable (scripts that poll or
    /// update often, what its DLLs do).
    pub notes: String,
    /// Where it belongs: mods and plugins to load after or before, patches it needs, conflicts.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub load_order: String,
    /// When, in Unix seconds.
    pub date: u64,
    /// The files it covers (`fingerprint`).
    pub fingerprint: String,
}

#[derive(Serialize, Debug)]
pub struct ReviewState {
    pub mod_id: String,
    pub name: String,
    #[serde(flatten)]
    pub review: Review,
    /// False once the mod's files changed (reinstalled or updated): review it again.
    pub current: bool,
}

/// The mod's files as a review sees them: their paths and sizes, plus the archive's sha256.
// ponytail: no file is read, so a same-size edit in place goes unnoticed; rmm only changes mod
// files by reinstalling, which changes the archive hash. Hash the code files if that changes.
pub fn fingerprint(inst: &Instance, id: &str) -> Result<String> {
    let files = crate::lockfile::content_hash(inst, id)?;
    Ok(match inst.mod_meta(id).sha256 {
        Some(archive) => format!("{files}:{archive}"),
        None => files,
    })
}

fn review_state(inst: &Instance, id: &str) -> Result<Option<ReviewState>> {
    let meta = inst.mod_meta(id);
    let Some(review) = meta.review else { return Ok(None) };
    let current = fingerprint(inst, id)? == review.fingerprint;
    Ok(Some(ReviewState { mod_id: id.into(), name: meta.name, review, current }))
}

/// Save a review of an installed mod, replacing its last one. Not in undo history.
pub fn save_review(inst: &Instance, id: &str, verdict: &str, notes: &str, load_order: &str) -> Result<ReviewState> {
    if is_separator(id) || !inst.mod_dir(id).is_dir() {
        bail!("mod '{id}' is not installed");
    }
    let verdict = verdict.trim().to_lowercase();
    if !matches!(verdict.as_str(), "clean" | "suspicious" | "malicious") {
        bail!("verdict is clean, suspicious or malicious");
    }
    if notes.trim().is_empty() {
        bail!("a review needs notes: what the mod does and what was checked");
    }
    let mut meta = inst.mod_meta(id);
    let date = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    meta.review = Some(Review { verdict, notes: notes.trim().into(), load_order: load_order.trim().into(), date, fingerprint: fingerprint(inst, id)? });
    inst.write_mod_meta(id, &meta)?;
    Ok(review_state(inst, id)?.expect("just saved"))
}

/// Saved reviews: one mod's, or every reviewed mod in the profile.
pub fn reviews(inst: &Instance, id: Option<&str>) -> Result<Vec<ReviewState>> {
    let ids = match id {
        Some(id) => vec![id.to_string()],
        None => inst.profile()?.mods.into_iter().map(|m| m.id).filter(|id| !is_separator(id)).collect(),
    };
    let mut out = Vec::new();
    for id in ids {
        out.extend(review_state(inst, &id)?);
    }
    Ok(out)
}

#[derive(Serialize, Debug)]
pub struct Defender {
    pub ran: bool,
    /// Threat names Defender reported, with the file.
    pub threats: Vec<String>,
    pub note: String,
}

/// Scripts Windows runs when opened; mods have no use for them in the game.
const RUNNABLE: &[&str] = &["bat", "cmd", "ps1", "psm1", "vbs", "vbe", "js", "jse", "wsf", "wsh", "hta", "reg", "scr", "com", "msi", "msp", "jar", "lnk", "pif", "cpl"];
const EXECUTABLE: &[&str] = &["dll", "exe", "asi", "sys", "ocx", "drv"];

/// Scan one mod (with Windows Defender too), or every mod (without Defender; only mods with
/// native code, runnable scripts or findings are listed).
pub fn scan(inst: &Instance, id: Option<&str>) -> Result<Vec<Scan>> {
    let ids: Vec<String> = match id {
        Some(id) => {
            if is_separator(id) || !inst.mod_dir(id).is_dir() {
                bail!("mod '{id}' is not installed");
            }
            vec![id.to_string()]
        }
        None => inst.profile()?.mods.into_iter().map(|m| m.id).filter(|id| !is_separator(id)).collect(),
    };
    let mut out = Vec::new();
    for (i, m) in ids.iter().enumerate() {
        crate::progress::report("scanning mods", i, ids.len());
        let mut s = scan_files(inst, m)?;
        s.review = review_state(inst, m)?;
        if id.is_some() {
            s.defender = Some(defender(&inst.mod_dir(m)));
        }
        let threat = s.defender.as_ref().is_some_and(|d| !d.threats.is_empty()) || s.findings.iter().any(|f| f.severity == "danger");
        s.verdict = if threat {
            "threat"
        } else if s.findings.iter().any(|f| f.severity == "warning") {
            "review"
        } else {
            "clean"
        };
        if id.is_some() || s.verdict != "clean" || !s.native_code.is_empty() || s.review.is_some() {
            out.push(s);
        }
    }
    Ok(out)
}

fn scan_files(inst: &Instance, id: &str) -> Result<Scan> {
    let dir = inst.mod_dir(id);
    let mut s = Scan { mod_id: id.into(), verdict: "clean", defender: None, native_code: vec![], scripts: 0, findings: vec![], review: None };
    let mut note = |file: &str, severity, message: String| s.findings.push(Finding { file: file.into(), severity, message });
    let mut native = Vec::new();
    for file in inst.mod_files(id)? {
        let low = file.to_lowercase();
        let ext = low.rsplit_once('.').map_or("", |(_, e)| e);
        if ext == "pex" {
            s.scripts += 1;
            continue;
        }
        if RUNNABLE.contains(&ext) {
            note(&file, "warning", format!("a .{ext} file runs outside the game when opened; mods rarely need one"));
        }
        if matches!(ext, "zip" | "7z" | "rar") {
            note(&file, "info", "an archive inside the mod; its contents aren't scanned".into());
        }
        let path = dir.join(&file);
        let mut head = [0u8; 2];
        let mz = fs::File::open(&path).and_then(|mut f| std::io::Read::read_exact(&mut f, &mut head)).is_ok() && head == *b"MZ";
        if !mz {
            continue;
        }
        if !EXECUTABLE.contains(&ext) {
            note(&file, "danger", format!("a Windows program posing as a .{ext} file"));
        }
        native.push(file.clone());
        let bytes = fs::read(&path)?;
        let pe = match pe_info(&bytes) {
            Ok(pe) => pe,
            Err(e) => {
                note(&file, "warning", format!("starts like a Windows program but can't be read as one: {e:#}"));
                continue;
            }
        };
        for (what, severity, calls) in &pe.capabilities {
            note(&file, severity, format!("{what} ({})", calls.join(", ")));
        }
        for section in pe.sections.iter().filter(|n| PACKERS.iter().any(|p| n.to_lowercase().starts_with(p))) {
            note(&file, "warning", format!("packed or obfuscated (section {section}), which hides what it does"));
        }
        for (severity, string) in &pe.notable {
            note(&file, severity, format!("contains \"{string}\""));
        }
        let plugin_dir = low.starts_with("skse/plugins/") || low.starts_with("f4se/plugins/");
        if plugin_dir && ext == "dll" && !pe.exports.iter().any(|e| e.ends_with("Plugin_Load") || e.ends_with("Plugin_Version")) {
            note(&file, "info", "exports no SKSEPlugin_Load/F4SEPlugin_Load, so the script extender won't load it; likely a library another plugin uses".into());
        }
        if low.starts_with("root/") && !low[5..].contains('/') {
            note(&file, "info", "goes next to the game's exe, where Windows loads DLLs into the game (ENB, ReShade and the script extender work this way)".into());
        }
    }
    s.native_code = native;
    Ok(s)
}

/// Section names packers and protectors leave.
const PACKERS: &[&str] = &["upx", ".themida", ".vmp", ".enigma", ".aspack", ".petite", "mpress", ".winlice"];

/// Windows Defender's command-line scanner on a folder, without quarantining anything.
fn defender(dir: &Path) -> Defender {
    let exe = std::env::var_os("ProgramFiles")
        .map(|p| PathBuf::from(p).join("Windows Defender").join("MpCmdRun.exe"))
        .filter(|p| p.is_file());
    let Some(exe) = exe else {
        return Defender { ran: false, threats: vec![], note: "Windows Defender's scanner (MpCmdRun.exe) isn't here".into() };
    };
    crate::progress::report("Windows Defender is scanning the mod", 0, 0);
    let out = crate::hidden(exe).args(["-Scan", "-ScanType", "3", "-DisableRemediation", "-File"]).arg(dir).output();
    let Ok(out) = out else {
        return Defender { ran: false, threats: vec![], note: "couldn't start Windows Defender's scanner".into() };
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let field = |l: &str, k: &str| l.trim().strip_prefix(k).and_then(|r| r.trim_start().strip_prefix(':')).map(|v| v.trim().to_string());
    let mut threats = Vec::new();
    let mut current = None;
    for line in text.lines() {
        if let Some(t) = field(line, "Threat") {
            current = Some(t);
        } else if let (Some(f), Some(t)) = (field(line, "file"), &current) {
            threats.push(format!("{t}: {f}"));
        }
    }
    match out.status.code() {
        Some(0) => Defender { ran: true, threats, note: "found no threats".into() },
        Some(2) => Defender { ran: true, threats, note: "found threats (nothing was quarantined)".into() },
        c => Defender {
            ran: false,
            threats,
            note: format!("the scanner stopped with code {} (another antivirus may have replaced Defender): {}", c.map_or("none".into(), |c| c.to_string()), text.trim().lines().last().unwrap_or("")),
        },
    }
}

// ---- DLLs and EXEs ------------------------------------------------------------------------------

#[derive(Serialize, Debug)]
pub struct PeInfo {
    pub machine: String,
    pub dll: bool,
    /// Carries an Authenticode signature (present, not verified).
    pub signed: bool,
    /// Library -> functions it calls there.
    pub imports: BTreeMap<String, Vec<String>>,
    pub exports: Vec<String>,
    pub sections: Vec<String>,
    /// What its imports let it do: (what, severity, the calls).
    pub capabilities: Vec<(&'static str, &'static str, Vec<String>)>,
    /// Strings worth a look: addresses, commands, autostart paths. (severity, string)
    pub notable: Vec<(&'static str, String)>,
}

/// Imports that say what a program can do. `lib:func`, `*` for any library; a trailing `*` on
/// the function matches its A/W/Ex variants.
const CAPABILITIES: &[(&str, &str, &[&str])] = &[
    ("connects to the network", "warning", &["ws2_32.dll:*", "wsock32.dll:*", "wininet.dll:*", "winhttp.dll:*", "urlmon.dll:urldownloadto*", "dnsapi.dll:*"]),
    ("starts other programs", "warning", &["*:createprocess*", "*:shellexecute*", "*:winexec", "*:system", "*:_wsystem", "*:_popen", "*:_wpopen"]),
    ("writes to the registry", "warning", &["advapi32.dll:regsetvalue*", "advapi32.dll:regcreatekey*", "advapi32.dll:regsetkeyvalue*"]),
    ("writes into other programs' memory", "warning", &["*:writeprocessmemory", "*:createremotethread*", "*:virtualallocex*", "*:ntwritevirtualmemory", "*:queueuserapc"]),
    ("reads the keyboard or clipboard (hotkeys use this too)", "info", &["user32.dll:getasynckeystate", "user32.dll:setwindowshookex*", "user32.dll:getclipboarddata", "user32.dll:getrawinputdata"]),
    ("encrypts data", "info", &["advapi32.dll:cryptencrypt", "bcrypt.dll:bcryptencrypt", "ncrypt.dll:*"]),
];

/// Read a DLL or EXE's imports, exports, sections and notable strings.
pub fn pe_info(b: &[u8]) -> Result<PeInfo> {
    use object::read::pe::{PeFile32, PeFile64};
    match object::FileKind::parse(b)? {
        object::FileKind::Pe64 => from_pe(&PeFile64::parse(b)?, b),
        object::FileKind::Pe32 => from_pe(&PeFile32::parse(b)?, b),
        k => bail!("not a Windows program or DLL ({k:?})"),
    }
}

fn from_pe<Pe: object::read::pe::ImageNtHeaders>(f: &object::read::pe::PeFile<'_, Pe>, b: &[u8]) -> Result<PeInfo> {
    use object::{Object, ObjectSection};
    let text = |s: &[u8]| String::from_utf8_lossy(s).into_owned();
    let name = |n: object::NameOrOrdinal<&[u8]>| match n {
        object::NameOrOrdinal::Name(n) => text(n),
        object::NameOrOrdinal::Ordinal(o) => format!("#{o}"),
    };
    let mut imports: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for i in f.imports()? {
        let i = i?;
        imports.entry(text(i.library()).to_lowercase()).or_default().push(name(i.name()));
    }
    let mut capabilities = Vec::new();
    for (what, severity, rules) in CAPABILITIES {
        let calls: Vec<String> = imports
            .iter()
            .flat_map(|(lib, funcs)| funcs.iter().map(move |f| (lib, f)))
            .filter(|(lib, func)| {
                let func = func.to_lowercase();
                rules.iter().any(|r| {
                    let (rl, rf) = r.split_once(':').unwrap();
                    (rl == "*" || rl == lib.as_str()) && (rf == "*" || rf.strip_suffix('*').map_or(func == rf, |p| func.starts_with(p)))
                })
            })
            .map(|(lib, func)| format!("{lib}!{func}"))
            .collect();
        if !calls.is_empty() {
            capabilities.push((*what, *severity, calls));
        }
    }
    Ok(PeInfo {
        machine: format!("{:?}", f.architecture()),
        dll: f.kind() == object::ObjectKind::Dynamic,
        signed: f.data_directory(object::pe::IMAGE_DIRECTORY_ENTRY_SECURITY).is_some_and(|d| d.size.get(object::LittleEndian) > 0),
        exports: f.exports()?.map(|e| e.map(|e| name(e.name()))).collect::<Result<_, _>>()?,
        sections: f.sections().filter_map(|s| s.name().ok().map(str::to_owned)).collect(),
        imports,
        capabilities,
        notable: notable_strings(b),
    })
}

/// Commands, autostart places and paste/webhook sites are warnings; other addresses are info.
const SUSPICIOUS: &[&str] = &[
    "powershell", "cmd.exe", "cmd /c", "\\currentversion\\run", "\\start menu\\programs\\startup", "discord.com/api/webhooks", "discordapp.com/api/webhooks",
    ".onion", "pastebin.com", "schtasks", "bitsadmin", "certutil", "-encodedcommand", "mshta", "rundll32", "regsvr32", "wscript",
];

/// Printable strings (ASCII and UTF-16) that point at the network or at running commands.
fn notable_strings(b: &[u8]) -> Vec<(&'static str, String)> {
    let url = regex::Regex::new(r"(?i)\b(?:https?|ftp)://[^\s\x22'<>]{4,200}").unwrap();
    let ip = regex::Regex::new(r"\b(?:\d{1,3}\.){3}\d{1,3}\b").unwrap();
    let mut out: BTreeSet<(&'static str, String)> = BTreeSet::new();
    let mut look = |s: &str| {
        let low = s.to_lowercase();
        if SUSPICIOUS.iter().any(|k| low.contains(k)) {
            out.insert(("warning", s.chars().take(200).collect()));
        }
        for m in url.find_iter(s) {
            out.insert(("info", m.as_str().to_string()));
        }
        for m in ip.find_iter(s) {
            // ponytail: all-single-digit quads (1.0.1.1) and x.x.x.0 are taken for versions, which hides 8.8.8.8-style addresses.
            let octets: Vec<&str> = m.as_str().split('.').collect();
            let ok = octets.iter().all(|o| o.parse::<u16>().is_ok_and(|n| n <= 255))
                && octets.iter().any(|o| o.len() > 1)
                && !m.as_str().ends_with(".0")
                && !m.as_str().starts_with("0.");
            if ok {
                out.insert(("info", m.as_str().to_string()));
            }
        }
    };
    // ASCII runs, then UTF-16LE runs, of six characters or more.
    let printable = |c: u8| c == b'\t' || (0x20..0x7f).contains(&c);
    for run in b.split(|&c| !printable(c)).filter(|r| r.len() >= 6) {
        look(&String::from_utf8_lossy(run));
    }
    let mut wide = String::new();
    for pair in b.as_chunks::<2>().0 {
        if pair[1] == 0 && printable(pair[0]) {
            wide.push(pair[0] as char);
            continue;
        }
        if wide.len() >= 6 {
            look(&wide);
        }
        wide.clear();
    }
    // ponytail: first 60 by kind and text; a DLL with more is worth reading whole with read_mod_file anyway.
    let mut v: Vec<_> = out.into_iter().collect();
    v.sort_by_key(|(sev, _)| *sev != "warning");
    v.truncate(60);
    v
}

// ---- Reading files ------------------------------------------------------------------------------

#[derive(Serialize, Debug)]
pub struct FileView {
    pub path: String,
    pub size: u64,
    /// "text", "papyrus" (a compiled script, disassembled), "native" (DLL/EXE) or "binary".
    pub kind: &'static str,
    /// Text, or the disassembly, from line `first_line` (0-based) on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_lines: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native: Option<PeInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

const TEXT_MAX: u64 = 8 << 20;

/// Read one file of a mod (`path` relative to the mod folder, as mod_details lists it). Text and
/// disassembled scripts come `limit` lines at a time from line `offset`.
pub fn read_file(inst: &Instance, id: &str, path: &str, offset: usize, limit: usize) -> Result<FileView> {
    if is_separator(id) || !inst.mod_dir(id).is_dir() {
        bail!("mod '{id}' is not installed");
    }
    if Path::new(path).components().any(|c| !matches!(c, Component::Normal(_))) {
        bail!("give a path inside the mod, like scripts/foo.pex");
    }
    let full = inst.mod_dir(id).join(path);
    let size = fs::metadata(&full).with_context(|| format!("mod '{id}' has no file {path}"))?.len();
    let mut v = FileView { path: path.into(), size, kind: "binary", text: None, first_line: None, total_lines: None, native: None, note: None };
    let head = {
        let mut h = Vec::new();
        std::io::Read::read_to_end(&mut std::io::Read::take(fs::File::open(&full)?, 4), &mut h)?;
        h
    };
    let text = if head.starts_with(b"MZ") {
        v.kind = "native";
        v.native = Some(pe_info(&fs::read(&full)?)?);
        return Ok(v);
    } else if head == [0xFA, 0x57, 0xC0, 0xDE] || head == [0xDE, 0xC0, 0x57, 0xFA] {
        v.kind = "papyrus";
        disassemble_pex(&fs::read(&full)?)?
    } else if size > TEXT_MAX {
        v.note = Some("too large to read as text".into());
        return Ok(v);
    } else {
        let b = fs::read(&full)?;
        match decode_text(&b) {
            Some(t) => {
                v.kind = "text";
                t
            }
            None => {
                v.note = Some(format!("a binary file (starts {:02x?}); not readable as text", &b[..b.len().min(8)]));
                return Ok(v);
            }
        }
    };
    let lines: Vec<&str> = text.lines().collect();
    v.total_lines = Some(lines.len());
    v.first_line = Some(offset.min(lines.len()));
    v.text = Some(lines.iter().skip(offset).take(limit).copied().collect::<Vec<_>>().join("\n"));
    Ok(v)
}

use crate::workspace::decode_text;

// ---- Papyrus ------------------------------------------------------------------------------------

/// Mnemonic, fixed argument count, and whether a counted list of arguments follows.
const OPS: &[(&str, u8, bool)] = &[
    ("nop", 0, false),
    ("iadd", 3, false),
    ("fadd", 3, false),
    ("isub", 3, false),
    ("fsub", 3, false),
    ("imul", 3, false),
    ("fmul", 3, false),
    ("idiv", 3, false),
    ("fdiv", 3, false),
    ("imod", 3, false),
    ("not", 2, false),
    ("ineg", 2, false),
    ("fneg", 2, false),
    ("assign", 2, false),
    ("cast", 2, false),
    ("cmp_eq", 3, false),
    ("cmp_lt", 3, false),
    ("cmp_le", 3, false),
    ("cmp_gt", 3, false),
    ("cmp_ge", 3, false),
    ("jmp", 1, false),
    ("jmpt", 2, false),
    ("jmpf", 2, false),
    ("callmethod", 3, true),
    ("callparent", 2, true),
    ("callstatic", 3, true),
    ("return", 1, false),
    ("strcat", 3, false),
    ("propget", 3, false),
    ("propset", 3, false),
    ("array_create", 2, false),
    ("array_length", 2, false),
    ("array_getelement", 3, false),
    ("array_setelement", 3, false),
    ("array_findelement", 4, false),
    ("array_rfindelement", 4, false),
    // Fallout 4
    ("is", 3, false),
    ("struct_create", 1, false),
    ("struct_get", 3, false),
    ("struct_set", 3, false),
    ("array_findstruct", 5, false),
    ("array_rfindstruct", 5, false),
    ("array_add", 3, false),
    ("array_insert", 3, false),
    ("array_removelast", 1, false),
    ("array_remove", 3, false),
    ("array_clear", 1, false),
];

/// A reader over a script: Skyrim's are big-endian, Fallout 4's little-endian.
struct Rd<'a> {
    b: &'a [u8],
    i: usize,
    le: bool,
    strings: Vec<String>,
}

impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let s = self.b.get(self.i..self.i + n).context("the script ends early")?;
        self.i += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        let s = self.take(2)?.try_into()?;
        Ok(if self.le { u16::from_le_bytes(s) } else { u16::from_be_bytes(s) })
    }
    fn u32(&mut self) -> Result<u32> {
        let s = self.take(4)?.try_into()?;
        Ok(if self.le { u32::from_le_bytes(s) } else { u32::from_be_bytes(s) })
    }
    fn wstr(&mut self) -> Result<String> {
        let n = self.u16()? as usize;
        Ok(String::from_utf8_lossy(self.take(n)?).into_owned())
    }
    fn skip16s(&mut self) -> Result<()> {
        let n = self.u16()? as usize;
        self.take(n * 2).map(|_| ())
    }
    /// A string table entry.
    fn s(&mut self) -> Result<String> {
        let i = self.u16()? as usize;
        Ok(self.strings.get(i).cloned().unwrap_or_else(|| format!("<string {i}>")))
    }
    /// A value: None, an identifier, a string, an int, a float or a bool.
    fn value(&mut self) -> Result<String> {
        Ok(match self.u8()? {
            0 => "None".into(),
            1 => self.s()?,
            2 => format!("{:?}", self.s()?),
            3 => (self.u32()? as i32).to_string(),
            4 => format!("{:?}", f32::from_bits(self.u32()?)),
            5 => (self.u8()? != 0).to_string(),
            t => bail!("unknown value type {t} at byte {}", self.i - 1),
        })
    }
    fn count(&mut self) -> Result<usize> {
        match self.u8()? {
            3 => Ok(self.u32()? as i32 as usize),
            t => bail!("expected a count, got value type {t}"),
        }
    }
}

/// A compiled Papyrus script (.pex, Skyrim or Fallout 4) as readable text: its objects with their
/// variables, properties and states, and each function's instructions.
pub fn disassemble_pex(b: &[u8]) -> Result<String> {
    let le = match b.get(..4) {
        Some([0xFA, 0x57, 0xC0, 0xDE]) => false,
        Some([0xDE, 0xC0, 0x57, 0xFA]) => true,
        _ => bail!("not a compiled Papyrus script"),
    };
    let mut r = Rd { b, i: 4, le, strings: vec![] };
    let (major, minor) = (r.u8()?, r.u8()?);
    let game = r.u16()?;
    r.take(8)?; // compile time
    let source = r.wstr()?;
    r.wstr()?; // user name and machine: the author's, not needed to read the script
    r.wstr()?;
    let n = r.u16()?;
    r.strings = (0..n).map(|_| r.wstr()).collect::<Result<_>>()?;
    let fo4 = game == 2;
    if r.u8()? != 0 {
        r.take(8)?;
        for _ in 0..r.u16()? {
            r.take(7)?; // object, state, function, type
            r.skip16s()?; // line numbers
        }
        if fo4 {
            for _ in 0..r.u16()? {
                r.take(10)?; // property groups: object, name, doc, flags
                r.skip16s()?;
            }
            for _ in 0..r.u16()? {
                r.take(4)?; // struct member orders
                r.skip16s()?;
            }
        }
    }
    for _ in 0..r.u16()? {
        r.take(3)?; // user flag names
    }
    let mut out = format!("; {source}: Papyrus {major}.{minor} for {}\n", if fo4 { "Fallout 4" } else { "Skyrim" });
    for _ in 0..r.u16()? {
        let name = r.s()?;
        r.u32()?; // size
        let parent = r.s()?;
        r.u16()?; // doc
        if fo4 {
            r.u8()?; // const
        }
        r.u32()?; // user flags
        let auto = r.s()?;
        let _ = writeln!(out, "ScriptName {name}{}", if parent.is_empty() { String::new() } else { format!(" extends {parent}") });
        if fo4 {
            for _ in 0..r.u16()? {
                let _ = writeln!(out, "Struct {}", r.s()?);
                for _ in 0..r.u16()? {
                    let (n, t) = (r.s()?, r.s()?);
                    r.u32()?;
                    let v = r.value()?;
                    r.take(3)?; // const, doc
                    let _ = writeln!(out, "  {t} {n} = {v}");
                }
                out.push_str("EndStruct\n");
            }
        }
        for _ in 0..r.u16()? {
            let (n, t) = (r.s()?, r.s()?);
            r.u32()?;
            let v = r.value()?;
            if fo4 {
                r.u8()?;
            }
            let _ = writeln!(out, "{t} {n} = {v}");
        }
        for _ in 0..r.u16()? {
            let (n, t) = (r.s()?, r.s()?);
            r.u16()?;
            r.u32()?;
            let flags = r.u8()?;
            if flags & 4 != 0 {
                let _ = writeln!(out, "{t} Property {n} Auto ; stored in {}", r.s()?);
                continue;
            }
            let _ = writeln!(out, "{t} Property {n}");
            if flags & 1 != 0 {
                function(&mut r, "Get", &mut out)?;
            }
            if flags & 2 != 0 {
                function(&mut r, "Set", &mut out)?;
            }
            out.push_str("EndProperty\n");
        }
        for _ in 0..r.u16()? {
            let state = r.s()?;
            if !state.is_empty() {
                let _ = writeln!(out, "{}State {state}", if state == auto { "Auto " } else { "" });
            }
            for _ in 0..r.u16()? {
                let f = r.s()?;
                function(&mut r, &f, &mut out)?;
            }
            if !state.is_empty() {
                out.push_str("EndState\n");
            }
        }
    }
    if r.i != b.len() {
        let _ = writeln!(out, "; {} bytes after the last object weren't read", b.len() - r.i);
    }
    Ok(out)
}

fn function(r: &mut Rd, name: &str, out: &mut String) -> Result<()> {
    let ret = r.s()?;
    r.u16()?; // doc
    r.u32()?; // user flags
    let flags = r.u8()?;
    // (name, type) pairs, shown as "type name".
    let pairs = |r: &mut Rd| -> Result<Vec<String>> { (0..r.u16()?).map(|_| Ok(format!("{1} {0}", r.s()?, r.s()?))).collect() };
    let params = pairs(r)?;
    let locals = pairs(r)?;
    let ret = if ret.is_empty() || ret.eq_ignore_ascii_case("none") { String::new() } else { format!("{ret} ") };
    let _ = writeln!(
        out,
        "{ret}Function {name}({}){}{}",
        params.join(", "),
        if flags & 1 != 0 { " Global" } else { "" },
        if flags & 2 != 0 { " Native" } else { "" }
    );
    for l in &locals {
        let _ = writeln!(out, "  ; local {l}");
    }
    for i in 0..r.u16()? {
        let op = r.u8()?;
        let &(name, fixed, more) = OPS.get(op as usize).with_context(|| format!("unknown instruction {op} at byte {}", r.i - 1))?;
        let mut args = (0..fixed).map(|_| r.value()).collect::<Result<Vec<_>>>()?;
        if more {
            let n = r.count()?;
            for _ in 0..n {
                args.push(r.value()?);
            }
        }
        let _ = writeln!(out, "  {i:>4}  {name} {}", args.join(" "));
    }
    if flags & 2 == 0 {
        out.push_str("EndFunction\n");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny Skyrim script: `ScriptName T extends Quest`, `Int Property Count Auto` and an
    /// event that calls RegisterForSingleUpdate(5.0).
    fn script(le: bool) -> Vec<u8> {
        let mut b = Vec::new();
        let u16 = |b: &mut Vec<u8>, v: u16| b.extend(if le { v.to_le_bytes() } else { v.to_be_bytes() });
        let u32 = |b: &mut Vec<u8>, v: u32| b.extend(if le { v.to_le_bytes() } else { v.to_be_bytes() });
        let s = |b: &mut Vec<u8>, t: &str| {
            u16(b, t.len() as u16);
            b.extend(t.as_bytes());
        };
        b.extend(if le { [0xDE, 0xC0, 0x57, 0xFA] } else { [0xFA, 0x57, 0xC0, 0xDE] });
        b.extend([3, 2]);
        u16(&mut b, 1); // Skyrim (the byte order is what's being tested)
        b.extend([0; 8]);
        for t in ["T.psc", "me", "pc"] {
            s(&mut b, t);
        }
        let strings = ["T", "Quest", "", "Count", "Int", "::Count_var", "OnUpdate", "None", "self", "RegisterForSingleUpdate", "::NoneVar"];
        u16(&mut b, strings.len() as u16);
        for t in strings {
            s(&mut b, t);
        }
        b.push(0); // no debug info
        u16(&mut b, 0); // user flags
        u16(&mut b, 1); // objects
        u16(&mut b, 0); // T
        u32(&mut b, 0); // size
        u16(&mut b, 1); // extends Quest
        u16(&mut b, 2); // doc
        u32(&mut b, 0);
        u16(&mut b, 2); // auto state ""
        u16(&mut b, 0); // variables
        u16(&mut b, 1); // properties
        u16(&mut b, 3);
        u16(&mut b, 4);
        u16(&mut b, 2);
        u32(&mut b, 0);
        b.push(7); // read, write, auto
        u16(&mut b, 5);
        u16(&mut b, 1); // states
        u16(&mut b, 2); // ""
        u16(&mut b, 1); // functions
        u16(&mut b, 6); // OnUpdate
        u16(&mut b, 7); // returns None
        u16(&mut b, 2);
        u32(&mut b, 0);
        b.push(0);
        u16(&mut b, 0); // params
        u16(&mut b, 0); // locals
        u16(&mut b, 1); // instructions
        b.push(23); // callmethod RegisterForSingleUpdate self ::NoneVar 5.0
        b.push(1);
        u16(&mut b, 9);
        b.push(1);
        u16(&mut b, 8);
        b.push(1);
        u16(&mut b, 10);
        b.push(3);
        u32(&mut b, 1);
        b.push(4);
        u32(&mut b, 5.0f32.to_bits());
        b
    }

    #[test]
    fn disassembles_papyrus() {
        for le in [false, true] {
            let text = disassemble_pex(&script(le)).unwrap();
            assert!(text.contains("ScriptName T extends Quest\nInt Property Count Auto ; stored in ::Count_var\nFunction OnUpdate()\n"), "{text}");
            assert!(text.contains("0  callmethod RegisterForSingleUpdate self ::NoneVar 5.0\nEndFunction"), "{text}");
            assert!(!text.contains("weren't read"), "{text}");
        }
        assert!(disassemble_pex(b"MZ..").is_err());
        let cut = script(false);
        assert!(disassemble_pex(&cut[..cut.len() - 3]).is_err());
    }

    /// Every .pex under RMM_PEX_DIRS (`;`-separated) disassembles to its last byte.
    #[test]
    #[ignore]
    fn real_scripts() {
        let dirs = std::env::var("RMM_PEX_DIRS").expect("set RMM_PEX_DIRS");
        let (mut ok, mut bad) = (0, Vec::new());
        for dir in dirs.split(';') {
            for e in walkdir::WalkDir::new(dir).into_iter().flatten() {
                if !e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("pex")) {
                    continue;
                }
                match disassemble_pex(&fs::read(e.path()).unwrap()) {
                    Ok(t) if !t.contains("weren't read") => ok += 1,
                    Ok(_) => bad.push(format!("{}: leftover bytes", e.path().display())),
                    Err(err) => bad.push(format!("{}: {err:#}", e.path().display())),
                }
            }
        }
        assert!(bad.is_empty(), "{ok} fine, {} not:\n{}", bad.len(), bad.iter().take(20).cloned().collect::<Vec<_>>().join("\n"));
        eprintln!("{ok} scripts disassembled");
    }

    #[test]
    fn reads_text_and_flags_strings() {
        assert_eq!(decode_text(b"\xEF\xBB\xBFhi").as_deref(), Some("hi"));
        assert_eq!(decode_text(&[0xFF, 0xFE, b'h', 0, b'i', 0]).as_deref(), Some("hi"));
        assert_eq!(decode_text(b"caf\xe9").as_deref(), Some("café"));
        assert!(decode_text(b"\x00\x01DDS").is_none());
        let mut bin = b"\x00\x00powershell -EncodedCommand AAAA\x00\x00https://example.com/x.dll\x00 v1.5.97.0 \x00 10.1.2.3 \x00".to_vec();
        bin.extend("cmd.exe /c del".encode_utf16().flat_map(|c| c.to_le_bytes()));
        bin.extend([0, 0]);
        let n = notable_strings(&bin);
        assert_eq!(n.iter().filter(|(s, _)| *s == "warning").count(), 2, "{n:?}");
        assert!(n.contains(&("info", "https://example.com/x.dll".into())) && n.contains(&("info", "10.1.2.3".into())), "{n:?}");
        assert!(!n.iter().any(|(_, s)| s == "1.5.97.0"), "versions aren't addresses: {n:?}");
    }
}
