//! C# script installers (`fomod/script.cs`, from FOMM and NMM): the script runs as a program, so
//! rmm runs one only when the user says so (`allowing`), never for MCP, a collection or a pack.
//!
//! The script is compiled, with a stand-in for FOMM's script API (`BaseScript`), by the C#
//! compiler of the .NET Framework that comes with Windows. A small host program runs it in an
//! AppDomain that may only run code and show windows, as Vortex does: whatever the script wants
//! (the mod's files, the game's Data, dialogs, versions, INIs) goes through the host, which writes
//! down the files to install, the files it made and the INI settings it asked for. rmm then
//! installs those itself; the INI settings become an INI tweak of the mod, turned on.
use crate::instance::Instance;
use anyhow::{Context, Result, bail};
use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};

/// The INI tweak the script's INI settings go into (in the mod's INI Tweaks folder).
pub const TWEAK: &str = "Installer script.ini";

thread_local! {
    static ALLOWED: Cell<bool> = const { Cell::new(false) };
}

/// Run `f` with C# installer scripts allowed when `allow` (the user agreed to run this mod's).
pub fn allowing<R>(allow: bool, f: impl FnOnce() -> R) -> R {
    let was = ALLOWED.with(|a| a.replace(allow));
    let r = f();
    ALLOWED.with(|a| a.set(was));
    r
}

/// The script of an installer folder: `fomod/script.cs`.
pub(crate) fn script_in(root: &Path) -> Option<PathBuf> {
    let ci = |dir: &Path, name: &str| fs::read_dir(dir).ok()?.flatten().find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(name)).map(|e| e.path());
    ci(root, "fomod").and_then(|f| ci(&f, "script.cs")).filter(|p| p.is_file())
}

/// What a script asked for.
#[derive(Default, Debug)]
pub(crate) struct Outcome {
    /// (file in the mod's archive, where in Data), both with `/`.
    pub copies: Vec<(String, String)>,
    /// (where in Data, the file the script made).
    pub made: Vec<(String, PathBuf)>,
    /// INI settings, as an INI tweak's text.
    pub ini: String,
    pub warnings: Vec<String>,
}

/// The script with FOMM's names made rmm's: `using fomm.Scripting;` goes, and any base class
/// named `…BaseScript` (Fallout3BaseScript, fomm.Scripting.BaseScript) is rmm's `BaseScript`.
/// Also whether `OnActivate` is static.
fn prepare(code: &str) -> (String, bool) {
    let using = regex::Regex::new(r"(?m)^\s*using\s+fomm(\.[\w.]*)?\s*;").expect("regex");
    let base = regex::Regex::new(r"[\w.]*BaseScript\b").expect("regex");
    let code = base.replace_all(&using.replace_all(code, ""), "BaseScript").into_owned();
    let is_static = regex::Regex::new(r"\bstatic\b[^;{}()]*\bOnActivate\s*\(").expect("regex").is_match(&code);
    (code, is_static)
}

/// Compile and run the script of the installer at `root` for `inst`.
pub(crate) fn run(inst: &Instance, root: &Path, name: &str) -> Result<Outcome> {
    if !ALLOWED.with(Cell::get) {
        bail!("this mod's installer is a C# script: a program its author wrote. Only you can start it: install it in the window, which asks first, or with `rmm install --run-script`");
    }
    let script = script_in(root).context("fomod/script.cs is missing")?;
    let text = fs::read(&script)?;
    let text = String::from_utf8_lossy(&text);
    let (code, is_static) = prepare(text.trim_start_matches('\u{feff}'));
    let work = inst.root.join(".staging").join("csharp");
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work)?;
    let call = if is_static { "Script.OnActivate()" } else { "new Script().OnActivate()" };
    fs::write(work.join("shim.cs"), SHIM.replace("/*CALL*/", call))?;
    fs::write(work.join("script.cs"), code)?;
    fs::write(work.join("host.cs"), HOST)?;
    let csc = compiler()?;
    compile(&csc, &work, &["/target:library", "/out:script.dll", "shim.cs", "script.cs"]).context("the mod's installer script doesn't compile")?;
    compile(&csc, &work, &["/target:winexe", "/out:host.exe", "/r:script.dll", "host.cs"]).context("rmm's script host doesn't compile")?;
    fs::write(work.join("info.txt"), info(inst, name)?)?;

    crate::progress::report("running the mod's installer", 0, 0);
    let status = std::process::Command::new(work.join("host.exe")).arg(&work).arg(root).status().context("cannot start the script host")?;
    let Ok(result) = fs::read_to_string(work.join("result.txt")) else {
        bail!("the mod's installer closed before it finished ({status}), so nothing was installed")
    };
    match result.lines().next().unwrap_or_default() {
        "ok" => {}
        "declined" => bail!("the mod's installer script stopped the install (its OnActivate said no)"),
        _ if result.contains("SecurityException") => bail!(
            "the mod's installer script tried to reach outside the mod (a file, the registry or the network), which its sandbox doesn't allow. Install the mod by hand: unpack it and install its folder.\n{}",
            short(&result)
        ),
        _ => bail!("the mod's installer script failed:\n{}", short(&result)),
    }
    let mut out = Outcome::default();
    let mut ini: Vec<(String, String, String)> = Vec::new();
    for line in fs::read_to_string(work.join("out.txt"))?.lines() {
        let p: Vec<&str> = line.split('\t').collect();
        let slash = |s: &str| s.replace('\\', "/");
        match p.as_slice() {
            ["copy", from, to] => out.copies.push((slash(from), slash(to))),
            ["gen", to, file] => out.made.push((slash(to), work.join("gen").join(file))),
            ["plugin", plugin, "0"] => out.warnings.push(format!("the installer script asked to turn {plugin} off; rmm leaves that to you")),
            ["ini", _, section, key, value] => {
                ini.retain(|(s, k, _)| !(s.eq_ignore_ascii_case(section) && k.eq_ignore_ascii_case(key)));
                ini.push((section.to_string(), key.to_string(), value.to_string()));
            }
            _ => {}
        }
    }
    ini.sort_by_key(|(s, ..)| s.to_lowercase());
    let mut section = None;
    for (s, k, v) in ini {
        if section.as_ref() != Some(&s) {
            out.ini.push_str(&format!("[{s}]\r\n"));
            section = Some(s);
        }
        out.ini.push_str(&format!("{k}={v}\r\n"));
    }
    Ok(out)
}

fn short(result: &str) -> String {
    result.lines().skip(1).take(12).collect::<Vec<_>>().join("\n")
}

/// The .NET Framework 4 C# compiler, which comes with Windows.
fn compiler() -> Result<PathBuf> {
    let win = std::env::var_os("WINDIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    ["Framework64", "Framework"]
        .iter()
        .map(|f| win.join("Microsoft.NET").join(f).join("v4.0.30319").join("csc.exe"))
        .find(|p| p.is_file())
        .context("C# installers need the .NET Framework 4 that comes with Windows, and its compiler (csc.exe) isn't there")
}

fn compile(csc: &Path, work: &Path, args: &[&str]) -> Result<()> {
    let mut cmd = crate::hidden(csc);
    cmd.current_dir(work).arg("/nologo").args(args);
    let out = cmd.output().context("cannot run the C# compiler")?;
    if !out.status.success() {
        let text = String::from_utf8_lossy(&out.stdout);
        let errors: Vec<&str> = text.lines().filter(|l| l.contains("error")).take(8).collect();
        bail!("{}", if errors.is_empty() { text.trim().to_string() } else { errors.join("\n") });
    }
    Ok(())
}

/// What the host tells the script: names and versions, the INI folder, the profile's plugins and
/// the files in Data (the winning mod files and the game's own), each with where it is.
fn info(inst: &Instance, name: &str) -> Result<String> {
    let game = inst.config.game;
    let version = |v: [u32; 4]| v.map(|n| n.to_string()).join(".");
    let se = crate::launch::exe_version(&inst.config.game_path.join(game.loader())).ok().map(version).unwrap_or_default();
    let main = game.ini_name();
    let mut s = String::new();
    for (k, v) in [
        ("name", name.to_string()),
        ("game", crate::launch::game_version(&inst.config.game_path, game).map(version).unwrap_or_default()),
        ("se", se),
        ("fomm", "0.14.11.0".into()),
        ("ini_dir", inst.ini_dir().map(|d| d.display().to_string()).unwrap_or_default()),
        ("main_ini", main.into()),
        ("prefs_ini", main.replace(".ini", "Prefs.ini")),
    ] {
        s.push_str(&format!("{k}\t{v}\n"));
    }
    let p = inst.profile()?;
    for e in &p.plugins {
        s.push_str(&format!("plugin\t{}\t{}\n", e.name, e.enabled as u8));
    }
    let mut data: std::collections::BTreeMap<String, (String, PathBuf)> = std::collections::BTreeMap::new();
    let dir = inst.data_dir();
    for e in walkdir::WalkDir::new(&dir).into_iter().flatten().filter(|e| e.file_type().is_file()) {
        let rel = e.path().strip_prefix(&dir).unwrap_or(e.path()).to_string_lossy().replace('/', "\\");
        data.insert(rel.to_lowercase(), (rel, e.path().to_path_buf()));
    }
    for (_, (id, rel)) in crate::conflicts::file_map(inst, &p)?.winners() {
        if rel.starts_with("root/") {
            continue;
        }
        let r = rel.replace('/', "\\");
        data.insert(r.to_lowercase(), (r, inst.mod_dir(&id).join(&rel)));
    }
    for (rel, path) in data.into_values() {
        s.push_str(&format!("data\t{rel}\t{}\n", path.display()));
    }
    Ok(s)
}

/// rmm's stand-in for FOMM's script API, compiled with the mod's script into script.dll, which
/// runs in the sandbox. Each call goes to the host's bridge. `/*CALL*/` runs the script.
const SHIM: &str = r#"using System;
using System.Drawing;
using System.Windows.Forms;

[Serializable]
public struct SelectOption
{
    public string Item;
    public string Preview;
    public string Desc;
    public SelectOption(string item, string preview, string desc) { Item = item; Preview = preview; Desc = desc; }
}

public interface IRmmBridge
{
    bool Install(string from, string to);
    bool InstallAll();
    string[] ModFiles();
    byte[] ModFile(string path);
    string[] DataFiles(string path, string pattern, bool all);
    bool DataExists(string path);
    byte[] DataFile(string path);
    bool Generate(string path, byte[] data);
    int Message(string text, string title, int buttons, string details);
    int[] Select(string[] items, string[] previews, string[] descs, string title, bool many);
    string Version(string what);
    string[] Plugins(bool active);
    void Activate(string plugin, bool on);
    string IniGet(string file, string section, string key);
    bool IniSet(string file, string section, string key, string value);
}

public class BaseScript
{
    internal static IRmmBridge B;
    static Version V(string s) { return string.IsNullOrEmpty(s) ? null : new Version(s); }
    static int I(string s) { int v; int.TryParse(s, out v); return v; }

    public static string GetLastError() { return null; }
    public static bool PerformBasicInstall() { return B.InstallAll(); }
    public static bool CopyDataFile(string from, string to) { return B.Install(from, to); }
    public static bool InstallFileFromMod(string from, string to) { return B.Install(from, to); }
    public static bool InstallFileFromMod(string file) { return B.Install(file, file); }
    public static bool InstallFileFromFomod(string from, string to) { return B.Install(from, to); }
    public static bool InstallFileFromFomod(string file) { return B.Install(file, file); }
    public static string[] GetModFileList() { return B.ModFiles(); }
    public static string[] GetFomodFileList() { return B.ModFiles(); }
    public static byte[] GetFileFromMod(string file) { return B.ModFile(file); }
    public static byte[] GetFileFromFomod(string file) { return B.ModFile(file); }
    public static string[] GetExistingDataFileList(string path, string pattern, bool allFolders) { return B.DataFiles(path, pattern, allFolders); }
    public static bool DataFileExists(string path) { return B.DataExists(path); }
    public static byte[] GetExistingDataFile(string path) { return B.DataFile(path); }
    public static bool GenerateDataFile(string path, byte[] data) { return B.Generate(path, data); }

    public static void MessageBox(string message) { B.Message(message, null, 0, null); }
    public static void MessageBox(string message, string title) { B.Message(message, title, 0, null); }
    public static DialogResult MessageBox(string message, string title, MessageBoxButtons buttons) { return (DialogResult)B.Message(message, title, (int)buttons, null); }
    public static void ExtendedMessageBox(string message, string title, string details) { B.Message(message, title, 0, details); }
    public static void ExtendedMessageBox(string message, string title, string details, MessageBoxButtons buttons, MessageBoxIcon icon) { B.Message(message, title, (int)buttons, details); }
    public static int[] Select(SelectOption[] options, string title, bool selectMany)
    {
        options = options ?? new SelectOption[0];
        var items = new string[options.Length]; var previews = new string[options.Length]; var descs = new string[options.Length];
        for (int i = 0; i < options.Length; i++) { items[i] = options[i].Item; previews[i] = options[i].Preview; descs[i] = options[i].Desc; }
        return B.Select(items, previews, descs, title, selectMany);
    }
    public static int[] Select(string[] items, string[] previews, string[] descs, string title, bool selectMany) { return B.Select(items, previews, descs, title, selectMany); }
    public static int[] ImageSelect(string[] items, Image[] previews, string[] descs, string title, bool selectMany) { return B.Select(items, null, descs, title, selectMany); }
    public static Form CreateCustomForm() { return new Form(); }

    public static Version GetModManagerVersion() { return V(B.Version("fomm")); }
    public static Version GetFommVersion() { return V(B.Version("fomm")); }
    public static Version GetGameVersion() { return V(B.Version("game")); }
    public static Version GetFalloutVersion() { return V(B.Version("game")); }
    public static Version GetOblivionVersion() { return V(B.Version("game")); }
    public static Version GetSkseVersion() { return V(B.Version("se")); }
    public static Version GetFoseVersion() { return V(B.Version("se")); }
    public static Version GetNvseVersion() { return V(B.Version("se")); }
    public static Version GetObseVersion() { return V(B.Version("se")); }
    public static Version GetScriptExtenderVersion() { return V(B.Version("se")); }
    public static bool ScriptExtenderPresent() { return B.Version("se") != null; }

    public static string[] GetAllPlugins() { return B.Plugins(false); }
    public static string[] GetActivePlugins() { return B.Plugins(true); }
    public static void SetPluginActivation(string plugin, bool activate) { B.Activate(plugin, activate); }
    public static void SetPluginOrderIndex(string plugin, int index) { }
    public static void SetLoadOrder(int[] plugins) { }
    public static void SetLoadOrder(int[] plugins, int position) { }

    public static string GetIniString(string file, string section, string key) { return B.IniGet(file, section, key); }
    public static int GetIniInt(string file, string section, string key) { return I(B.IniGet(file, section, key)); }
    public static string GetFalloutIniString(string section, string key) { return B.IniGet("main", section, key); }
    public static int GetFalloutIniInt(string section, string key) { return I(B.IniGet("main", section, key)); }
    public static string GetPrefsIniString(string section, string key) { return B.IniGet("prefs", section, key); }
    public static int GetPrefsIniInt(string section, string key) { return I(B.IniGet("prefs", section, key)); }
    public static bool EditIni(string file, string section, string key, string value) { return B.IniSet(file, section, key, value); }
    public static bool EditFalloutINI(string section, string key, string value, bool saveOld) { return B.IniSet("main", section, key, value); }
    public static bool EditPrefsINI(string section, string key, string value, bool saveOld) { return B.IniSet("prefs", section, key, value); }
}

public class RmmLoader : MarshalByRefObject
{
    public string Run(IRmmBridge bridge)
    {
        BaseScript.B = bridge;
        try { return /*CALL*/ ? "ok" : "declined"; }
        catch (Exception e) { return "error\n" + e; }
    }
}
"#;

/// The host: runs script.dll in an AppDomain that may only run code and show windows, answers its
/// calls, and writes what it asked for to out.txt (and how it ended to result.txt).
const HOST: &str = r#"using System;
using System.Collections.Generic;
using System.Drawing;
using System.IO;
using System.Linq;
using System.Security;
using System.Security.Permissions;
using System.Text;
using System.Text.RegularExpressions;
using System.Windows.Forms;

// The sandbox is on the stack of every call from the script: the host asserts its own trust,
// so its file reads and dialogs aren't refused for it.
[PermissionSet(SecurityAction.Assert, Unrestricted = true)]
class Bridge : MarshalByRefObject, IRmmBridge
{
    readonly string root, gen;
    readonly Dictionary<string, string> info = new Dictionary<string, string>();
    readonly Dictionary<string, string> data = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase);
    readonly List<string> plugins = new List<string>(), active = new List<string>();
    public readonly StringBuilder Log = new StringBuilder();
    int made;

    public Bridge(string work, string root)
    {
        this.root = Path.GetFullPath(root).TrimEnd('\\');
        gen = Path.Combine(work, "gen");
        Directory.CreateDirectory(gen);
        foreach (var line in File.ReadAllLines(Path.Combine(work, "info.txt"), Encoding.UTF8))
        {
            var p = line.Split('\t');
            if (p[0] == "data" && p.Length == 3) data[p[1]] = p[2];
            else if (p[0] == "plugin" && p.Length == 3) { plugins.Add(p[1]); if (p[2] == "1") active.Add(p[1]); }
            else if (p.Length == 2) info[p[0]] = p[1];
        }
    }

    public override object InitializeLifetimeService() { return null; }

    string Info(string key) { string v; return info.TryGetValue(key, out v) ? v : ""; }

    static string Rel(string p)
    {
        p = (p ?? "").Replace('/', '\\').Trim().TrimStart('\\');
        if (Path.IsPathRooted(p) || p.Split('\\').Any(s => s == "..")) throw new ArgumentException("not a path inside the mod: " + p);
        return p;
    }

    static string Line(params string[] parts) { return string.Join("\t", parts.Select(s => (s ?? "").Replace('\t', ' ').Replace('\r', ' ').Replace('\n', ' '))); }

    public bool Install(string from, string to)
    {
        var f = Rel(from);
        if (!File.Exists(Path.Combine(root, f))) return false;
        Log.AppendLine(Line("copy", f, Rel(to)));
        return true;
    }

    public bool InstallAll()
    {
        foreach (var f in ModFiles())
            if (!f.StartsWith("fomod\\", StringComparison.OrdinalIgnoreCase)) Log.AppendLine(Line("copy", f, f));
        return true;
    }

    public string[] ModFiles() { return Directory.GetFiles(root, "*", SearchOption.AllDirectories).Select(p => p.Substring(root.Length + 1)).ToArray(); }

    public byte[] ModFile(string path) { var f = Path.Combine(root, Rel(path)); return File.Exists(f) ? File.ReadAllBytes(f) : null; }

    public string[] DataFiles(string path, string pattern, bool all)
    {
        var dir = Rel(path).TrimEnd('\\');
        var prefix = dir.Length > 0 ? dir + "\\" : "";
        var re = new Regex("^" + Regex.Escape(string.IsNullOrEmpty(pattern) ? "*" : pattern).Replace("\\*", ".*").Replace("\\?", ".") + "$", RegexOptions.IgnoreCase);
        return data.Keys.Where(k => k.StartsWith(prefix, StringComparison.OrdinalIgnoreCase) && (all || k.IndexOf('\\', prefix.Length) < 0) && re.IsMatch(Path.GetFileName(k))).ToArray();
    }

    public bool DataExists(string path) { return data.ContainsKey(Rel(path)); }

    public byte[] DataFile(string path) { string abs; return data.TryGetValue(Rel(path), out abs) && File.Exists(abs) ? File.ReadAllBytes(abs) : null; }

    public bool Generate(string path, byte[] bytes)
    {
        var name = (made++).ToString();
        File.WriteAllBytes(Path.Combine(gen, name), bytes ?? new byte[0]);
        Log.AppendLine(Line("gen", Rel(path), name));
        return true;
    }

    public int Message(string text, string title, int buttons, string details)
    {
        if (!string.IsNullOrEmpty(details)) text += "\n\n" + details;
        return (int)MessageBox.Show(text ?? "", string.IsNullOrEmpty(title) ? Info("name") : title, (MessageBoxButtons)buttons, MessageBoxIcon.None, MessageBoxDefaultButton.Button1, MessageBoxOptions.DefaultDesktopOnly);
    }

    public int[] Select(string[] items, string[] previews, string[] descs, string title, bool many)
    {
        items = items ?? new string[0];
        using (var f = new Form { Text = string.IsNullOrEmpty(title) ? Info("name") : title, Width = 760, Height = 500, StartPosition = FormStartPosition.CenterScreen, TopMost = true, MinimizeBox = false, MaximizeBox = false, ShowIcon = false })
        {
            var list = many ? new CheckedListBox { CheckOnClick = true } : new ListBox();
            list.Dock = DockStyle.Left; list.Width = 300; list.IntegralHeight = false;
            list.Items.AddRange(items);
            var desc = new TextBox { Multiline = true, ReadOnly = true, Dock = DockStyle.Bottom, Height = 120, ScrollBars = ScrollBars.Vertical };
            var pic = new PictureBox { Dock = DockStyle.Fill, SizeMode = PictureBoxSizeMode.Zoom };
            var ok = new Button { Text = "OK", Dock = DockStyle.Bottom, Height = 34, DialogResult = DialogResult.OK };
            f.AcceptButton = ok;
            list.SelectedIndexChanged += (s, e) =>
            {
                int i = list.SelectedIndex;
                desc.Text = i >= 0 && descs != null && i < descs.Length ? (descs[i] ?? "").Replace("\n", "\r\n") : "";
                var old = pic.Image; pic.Image = null; if (old != null) old.Dispose();
                if (i < 0 || previews == null || i >= previews.Length || string.IsNullOrEmpty(previews[i])) return;
                try
                {
                    var p = Path.Combine(root, Rel(previews[i]));
                    if (File.Exists(p)) using (var m = new MemoryStream(File.ReadAllBytes(p))) using (var img = Image.FromStream(m)) pic.Image = new Bitmap(img);
                }
                catch { }
            };
            f.Controls.Add(pic); f.Controls.Add(desc); f.Controls.Add(list); f.Controls.Add(ok);
            if (items.Length > 0) list.SelectedIndex = 0;
            if (f.ShowDialog() != DialogResult.OK) return new int[0];
            if (many) return ((CheckedListBox)list).CheckedIndices.Cast<int>().ToArray();
            return list.SelectedIndex >= 0 ? new[] { list.SelectedIndex } : new int[0];
        }
    }

    public string Version(string what) { var v = Info(what); return v.Length > 0 ? v : null; }

    public string[] Plugins(bool onlyActive) { return (onlyActive ? active : plugins).ToArray(); }

    public void Activate(string plugin, bool on) { Log.AppendLine(Line("plugin", plugin, on ? "1" : "0")); }

    public string IniGet(string file, string section, string key)
    {
        var dir = Info("ini_dir");
        if (dir.Length == 0) return null;
        var path = Path.Combine(dir, file == "main" ? Info("main_ini") : file == "prefs" ? Info("prefs_ini") : Path.GetFileName(file ?? ""));
        if (!File.Exists(path)) return null;
        bool inside = false;
        foreach (var raw in File.ReadAllLines(path))
        {
            var l = raw.Trim();
            if (l.StartsWith("[") && l.EndsWith("]")) inside = string.Equals(l.Substring(1, l.Length - 2).Trim(), section, StringComparison.OrdinalIgnoreCase);
            else if (inside)
            {
                int eq = l.IndexOf('=');
                if (eq > 0 && string.Equals(l.Substring(0, eq).Trim(), key, StringComparison.OrdinalIgnoreCase)) return l.Substring(eq + 1).Trim();
            }
        }
        return null;
    }

    public bool IniSet(string file, string section, string key, string value) { Log.AppendLine(Line("ini", Path.GetFileName(file ?? ""), section, key, value)); return true; }
}

static class Host
{
    [STAThread]
    static int Main(string[] args)
    {
        string work = args[0], root = args[1];
        Application.EnableVisualStyles();
        var bridge = new Bridge(work, root);
        string result;
        try
        {
            var grant = new PermissionSet(PermissionState.None);
            grant.AddPermission(new SecurityPermission(SecurityPermissionFlag.Execution));
            grant.AddPermission(new UIPermission(PermissionState.Unrestricted));
            var setup = new AppDomainSetup { ApplicationBase = work };
            var sandbox = AppDomain.CreateDomain("mod installer script", null, setup, grant);
            var loader = (RmmLoader)sandbox.CreateInstanceAndUnwrap("script", "RmmLoader");
            result = loader.Run(bridge);
        }
        catch (Exception e) { result = "error\n" + e; }
        File.WriteAllText(Path.Combine(work, "out.txt"), bridge.Log.ToString(), new UTF8Encoding(false));
        File.WriteAllText(Path.Combine(work, "result.txt"), result, new UTF8Encoding(false));
        return 0;
    }
}
"#;

#[cfg(test)]
mod tests {
    #[test]
    fn fomm_names_become_rmms() {
        let (code, is_static) = super::prepare("using System;\r\nusing fomm.Scripting;\r\nclass Script : Fallout3BaseScript {\r\n  public static bool OnActivate() { return true; }\r\n}\r\nclass Other : fomm.Scripting.BaseScript {}\r\n");
        assert!(!code.contains("fomm") && code.contains("class Script : BaseScript") && code.contains("class Other : BaseScript"), "{code}");
        assert!(is_static);
        assert!(!super::prepare("class Script : BaseScript { public bool OnActivate() { return true; } }").1);
    }
}
