//! FOMOD XML installers (`fomod/ModuleConfig.xml`, schema 5.0 and earlier): parse, walk the steps
//! with a set of choices, and work out which files to install. The rules follow MO2's evaluator
//! (docs/internals/mo2.md §7). Choices use Vortex's `installerChoices` shape, so Vortex imports
//! and collections carry over.
use crate::path_key;
use anyhow::{Context, Result, anyhow, bail};
use roxmltree::{Document, Node};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

// ---- Choices: Vortex's IChoiceType ----

/// `{"type": "fomod", "options": [{"name": step, "groups": [{"name": group, "choices": [{"name": option}]}]}]}`.
/// Steps or groups left out take the installer's defaults, so `{}` installs the defaults.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct Choices {
    #[serde(rename = "type", default = "fomod_kind")]
    pub kind: String,
    #[serde(default)]
    pub options: Vec<StepChoice>,
}

impl Default for Choices {
    fn default() -> Self {
        Choices { kind: fomod_kind(), options: vec![] }
    }
}

fn fomod_kind() -> String {
    "fomod".into()
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct StepChoice {
    pub name: String,
    #[serde(default)]
    pub groups: Vec<GroupChoice>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct GroupChoice {
    pub name: String,
    /// Selected options; an empty list selects none.
    pub choices: Vec<PluginChoice>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct PluginChoice {
    pub name: String,
    /// Position in the group as displayed. Written for Vortex compatibility; matching uses `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idx: Option<usize>,
}

// ---- Parsed installer ----

pub struct Installer {
    module: String,
    deps: Option<Cond>,
    required: Vec<FileItem>,
    steps: Vec<Step>,
    conditional: Vec<(Cond, Vec<FileItem>)>,
}

struct Step {
    name: String,
    visible: Option<Cond>,
    groups: Vec<Group>,
}

struct Group {
    name: String,
    kind: GroupKind,
    plugins: Vec<Plugin>,
}

struct Plugin {
    name: String,
    description: String,
    image: Option<String>,
    files: Vec<FileItem>,
    flags: Vec<(String, String)>,
    /// Type when no pattern matches, then (condition, type) patterns; first match wins.
    default: Kind,
    patterns: Vec<(Cond, Kind)>,
}

#[derive(Clone, Debug)]
pub struct FileItem {
    source: String,
    destination: String,
    folder: bool,
    priority: i64,
    always: bool,
    if_usable: bool,
}

#[derive(Clone, Copy, PartialEq, Debug, Serialize)]
pub enum Kind {
    Required,
    Optional,
    Recommended,
    NotUsable,
    CouldBeUsable,
}

#[derive(Clone, Copy, PartialEq, Debug, Serialize)]
pub enum GroupKind {
    SelectAtLeastOne,
    SelectAtMostOne,
    SelectExactlyOne,
    SelectAll,
    SelectAny,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum FileState {
    Missing,
    Inactive,
    Active,
}

enum Cond {
    All(Vec<Cond>),
    Any(Vec<Cond>),
    Flag(String, String),
    File(String, FileState),
    ScriptExtender,
}

/// What conditions can ask about the setup the mod is installed into.
pub struct Env<'a> {
    pub file_state: &'a dyn Fn(&str) -> FileState,
    pub script_extender: bool,
}

type Flags = HashMap<String, String>;

impl Cond {
    fn holds(&self, flags: &Flags, env: &Env) -> bool {
        match self {
            Cond::All(v) => v.iter().all(|c| c.holds(flags, env)),
            Cond::Any(v) => v.iter().any(|c| c.holds(flags, env)),
            // An unset flag only matches an empty value.
            Cond::Flag(f, v) => flags.get(f).map_or("", String::as_str) == v,
            Cond::File(f, s) => (env.file_state)(f) == *s,
            Cond::ScriptExtender => env.script_extender,
        }
    }
}

// ---- What a walk produces ----

/// The installer as it plays out with a set of choices: every visible step, the full resolved
/// selection (pass it back to change it), and what gets installed.
#[derive(Serialize, Debug)]
pub struct Walk {
    pub module: String,
    pub steps: Vec<StepView>,
    pub choices: Choices,
    /// Files the selection installs (filled in once the archive is resolved).
    pub files: usize,
    pub warnings: Vec<String>,
    #[serde(skip)]
    install: Vec<FileItem>,
}

#[derive(Serialize, Debug)]
pub struct StepView {
    pub name: String,
    pub groups: Vec<GroupView>,
}

#[derive(Serialize, Debug)]
pub struct GroupView {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: GroupKind,
    pub options: Vec<OptionView>,
}

#[derive(Serialize, Debug)]
pub struct OptionView {
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(rename = "type")]
    pub kind: Kind,
    pub selected: bool,
}

impl Installer {
    pub fn parse(bytes: &[u8]) -> Result<Installer> {
        let text = decode(bytes);
        // The declaration may name an encoding the text no longer has; it carries nothing else.
        let body = match text.trim_start().strip_prefix("<?xml") {
            Some(rest) => rest.split_once("?>").map_or(rest, |(_, b)| b),
            None => &text,
        };
        let doc = Document::parse(body).context("ModuleConfig.xml is not valid XML")?;
        let root = doc.root_element();
        if root.tag_name().name() != "config" {
            bail!("ModuleConfig.xml has no <config> element");
        }
        let mut steps = Vec::new();
        if let Some(list) = kid(root, "installSteps") {
            for s in kids(list, "installStep") {
                let mut groups = Vec::new();
                if let Some(gl) = kid(s, "optionalFileGroups") {
                    for g in kids(gl, "group") {
                        let mut plugins = Vec::new();
                        if let Some(pl) = kid(g, "plugins") {
                            for p in kids(pl, "plugin") {
                                plugins.push(plugin(p)?);
                            }
                            plugins = ordered(plugins, pl, |p| &p.name)?;
                        }
                        let kind = group_kind(attr(g, "type")?)?;
                        groups.push(Group { name: g.attribute("name").unwrap_or_default().into(), kind, plugins });
                    }
                    groups = ordered(groups, gl, |g| &g.name)?;
                }
                let visible = kid(s, "visible").map(cond).transpose()?;
                steps.push(Step { name: s.attribute("name").unwrap_or_default().into(), visible, groups });
            }
            steps = ordered(steps, list, |s| &s.name)?;
        }
        let mut conditional = Vec::new();
        for p in kid(root, "conditionalFileInstalls").and_then(|c| kid(c, "patterns")).into_iter().flat_map(|ps| kids(ps, "pattern")) {
            let c = cond(kid(p, "dependencies").context("conditional install pattern without <dependencies>")?)?;
            conditional.push((c, files(kid(p, "files"))));
        }
        Ok(Installer {
            module: kid(root, "moduleName").and_then(|n| n.text()).unwrap_or_default().trim().into(),
            deps: kid(root, "moduleDependencies").map(cond).transpose()?,
            required: files(kid(root, "requiredInstallFiles")),
            steps,
            conditional,
        })
    }

    /// Play the steps in order with `choices`. Groups the choices leave out take their defaults.
    /// Steps and groups are matched by name, and ones sharing a name (often none at all) by order:
    /// the second step called "" takes the second choice called "".
    pub fn walk(&self, choices: &Choices, env: &Env) -> Result<Walk> {
        for sc in &choices.options {
            let steps: Vec<&Step> = self.steps.iter().filter(|s| s.name == sc.name).collect();
            if steps.is_empty() {
                bail!("the installer has no step named '{}'", sc.name);
            }
            if let Some(gc) = sc.groups.iter().find(|gc| !steps.iter().any(|s| s.groups.iter().any(|g| g.name == gc.name))) {
                bail!("step '{}' has no group named '{}'", sc.name, gc.name);
            }
        }
        let mut out = Walk { module: self.module.clone(), steps: vec![], choices: Choices::default(), files: 0, warnings: vec![], install: vec![] };
        if self.deps.as_ref().is_some_and(|d| !d.holds(&Flags::new(), env)) {
            out.warnings.push("the installer's own requirements (moduleDependencies) are not met; it may need other mods first".into());
        }
        let mut flags = Flags::new();
        let mut picked = Vec::new();
        let mut seen: HashMap<&str, usize> = HashMap::new();
        for step in &self.steps {
            if !step.visible.as_ref().is_none_or(|c| c.holds(&flags, env)) {
                continue;
            }
            let given = nth_named(&choices.options, &step.name, &mut seen, |c| &c.name);
            let mut seen_groups: HashMap<&str, usize> = HashMap::new();
            // Within a step the first selected option that sets a flag wins (MO2 scans backwards
            // through steps and forwards within one); later steps override earlier ones.
            let mut step_flags = Flags::new();
            let mut view = StepView { name: step.name.clone(), groups: vec![] };
            let mut chosen = StepChoice { name: step.name.clone(), groups: vec![] };
            for g in &step.groups {
                let kinds: Vec<Kind> = g.plugins.iter().map(|p| p.kind(&flags, env)).collect();
                let kind = g.effective_kind();
                let sel = match given.and_then(|sc| nth_named(&sc.groups, &g.name, &mut seen_groups, |c| &c.name)) {
                    Some(gc) => g.pick(kind, &kinds, gc).with_context(|| format!("step '{}'", step.name))?,
                    None => default_pick(kind, &kinds),
                };
                let mut gview = GroupView { name: g.name.clone(), kind, options: vec![] };
                let mut gchoice = GroupChoice { name: g.name.clone(), choices: vec![] };
                for (i, p) in g.plugins.iter().enumerate() {
                    if sel[i] {
                        for (f, v) in &p.flags {
                            step_flags.entry(f.clone()).or_insert_with(|| v.clone());
                        }
                        picked.extend(p.files.iter().cloned());
                        gchoice.choices.push(PluginChoice { name: p.name.clone(), idx: Some(i) });
                    } else {
                        picked.extend(p.files.iter().filter(|f| f.always || (f.if_usable && kinds[i] != Kind::NotUsable)).cloned());
                    }
                    gview.options.push(OptionView {
                        name: p.name.clone(),
                        description: p.description.clone(),
                        image: p.image.clone(),
                        kind: kinds[i],
                        selected: sel[i],
                    });
                }
                view.groups.push(gview);
                chosen.groups.push(gchoice);
            }
            flags.extend(step_flags);
            out.steps.push(view);
            out.choices.options.push(chosen);
        }
        let mut unused: HashMap<&str, usize> = HashMap::new();
        for sc in &choices.options {
            let n = unused.entry(&sc.name).or_default();
            *n += 1;
            if *n > out.steps.iter().filter(|s| s.name == sc.name).count() {
                out.warnings.push(format!("the choices for '{}' weren't used: the installer doesn't show that step with these choices", sc.name));
            }
        }
        let mut install = self.required.clone();
        for (c, f) in &self.conditional {
            if c.holds(&flags, env) {
                install.extend(f.iter().cloned());
            }
        }
        install.extend(picked);
        install.sort_by_key(|f| f.priority); // stable: on equal priority the later entry wins
        out.install = install;
        Ok(out)
    }
}

/// The next of `items` called `name`: the first the first time, the second the second time.
fn nth_named<'a, T>(items: &'a [T], name: &'a str, seen: &mut HashMap<&'a str, usize>, of: impl Fn(&T) -> &String) -> Option<&'a T> {
    let n = seen.entry(name).or_default();
    let r = items.iter().filter(|c| of(c) == name).nth(*n);
    *n += 1;
    r
}

/// Data-relative destination key -> (destination, source file).
pub type Resolved = BTreeMap<String, (String, PathBuf)>;

impl Walk {
    /// Map the chosen entries onto the unpacked mod at `root`. Later entries overwrite earlier
    /// ones. Missing sources and paths that would leave the mod folder become warnings.
    pub fn resolve(&self, root: &Path) -> Result<(Resolved, Vec<String>)> {
        let mut index: HashMap<String, (String, PathBuf)> = HashMap::new();
        for e in walkdir::WalkDir::new(root).min_depth(1) {
            let e = e?;
            if e.file_type().is_file() {
                let rel = e.path().strip_prefix(root)?.to_string_lossy().replace('\\', "/");
                index.insert(rel.to_lowercase(), (rel, e.path().to_path_buf()));
            }
        }
        let mut out = BTreeMap::new();
        let mut warnings = Vec::new();
        for f in &self.install {
            let (Some(src), Some(dest)) = (clean(&f.source), clean(&f.destination)) else {
                warnings.push(format!("skipped '{}' -> '{}': the path leaves the mod folder", f.source, f.destination));
                continue;
            };
            let mut found = false;
            if f.folder {
                let depth = if src.is_empty() { 0 } else { src.split('/').count() };
                let prefix = if src.is_empty() { String::new() } else { format!("{}/", src.to_lowercase()) };
                for (key, (rel, path)) in &index {
                    if key.starts_with(&prefix) {
                        let tail = rel.splitn(depth + 1, '/').last().unwrap_or_default();
                        let d = if dest.is_empty() { tail.to_owned() } else { format!("{dest}/{tail}") };
                        out.insert(path_key(&d), (d, path.clone()));
                        found = true;
                    }
                }
            } else if let Some((rel, path)) = index.get(&src.to_lowercase()) {
                let name = rel.rsplit('/').next().unwrap_or_default();
                let d = match dest.as_str() {
                    "" => name.to_owned(),
                    _ if f.destination.ends_with(['/', '\\']) => format!("{dest}/{name}"),
                    _ => dest,
                };
                out.insert(path_key(&d), (d, path.clone()));
                found = true;
            }
            if !found {
                warnings.push(format!("'{}' is not in the archive", f.source));
            }
        }
        Ok((out, warnings))
    }
}

impl Group {
    /// MO2 treats a one-option "exactly/at least one" group as forced and "at most one" as a checkbox.
    fn effective_kind(&self) -> GroupKind {
        match (self.plugins.len(), self.kind) {
            (1, GroupKind::SelectAtLeastOne | GroupKind::SelectExactlyOne) => GroupKind::SelectAll,
            (1, GroupKind::SelectAtMostOne) => GroupKind::SelectAny,
            (_, k) => k,
        }
    }

    fn pick(&self, kind: GroupKind, kinds: &[Kind], given: &GroupChoice) -> Result<Vec<bool>> {
        if kind == GroupKind::SelectAll {
            return Ok(vec![true; kinds.len()]);
        }
        let mut sel = vec![false; kinds.len()];
        for c in &given.choices {
            // The position tells options of the same name apart; the name finds one that moved.
            let at = c.idx.filter(|&i| self.plugins.get(i).is_some_and(|p| p.name == c.name));
            let i = at.or_else(|| self.plugins.iter().position(|p| p.name == c.name)).ok_or_else(|| anyhow!("group '{}' has no option '{}'", self.name, c.name))?;
            if kinds[i] == Kind::NotUsable {
                bail!("option '{}' in group '{}' can't be used with this setup", c.name, self.name);
            }
            sel[i] = true;
        }
        for (s, k) in sel.iter_mut().zip(kinds) {
            *s |= *k == Kind::Required;
        }
        let n = sel.iter().filter(|s| **s).count();
        let (ok, rule) = match kind {
            GroupKind::SelectExactlyOne => (n == 1, "exactly one"),
            GroupKind::SelectAtMostOne => (n <= 1, "at most one"),
            GroupKind::SelectAtLeastOne => (n >= 1, "at least one"),
            _ => (true, ""),
        };
        if !ok {
            bail!("group '{}' takes {rule} option, counting required ones; got {n}", self.name);
        }
        Ok(sel)
    }
}

/// The installer's own defaults: Required and Recommended options, else the first usable one
/// where the group needs a selection.
fn default_pick(kind: GroupKind, kinds: &[Kind]) -> Vec<bool> {
    let first = |k: Kind| kinds.iter().position(|x| *x == k);
    let fallback = || first(Kind::Optional).or_else(|| first(Kind::CouldBeUsable)).unwrap_or(0);
    let mut sel = vec![false; kinds.len()];
    if kinds.is_empty() {
        return sel;
    }
    match kind {
        GroupKind::SelectAll => sel.fill(true),
        GroupKind::SelectExactlyOne | GroupKind::SelectAtMostOne => {
            let mut i = first(Kind::Required).or_else(|| first(Kind::Recommended));
            if i.is_none() && kind == GroupKind::SelectExactlyOne {
                i = Some(fallback());
            }
            if let Some(i) = i {
                sel[i] = true;
            }
        }
        GroupKind::SelectAtLeastOne | GroupKind::SelectAny => {
            for (s, k) in sel.iter_mut().zip(kinds) {
                *s = matches!(k, Kind::Required | Kind::Recommended);
            }
            if kind == GroupKind::SelectAtLeastOne && !sel.contains(&true) {
                sel[fallback()] = true;
            }
        }
    }
    sel
}

impl Plugin {
    fn kind(&self, flags: &Flags, env: &Env) -> Kind {
        self.patterns.iter().find(|(c, _)| c.holds(flags, env)).map_or(self.default, |(_, k)| *k)
    }
}

// ---- XML helpers ----

fn kids<'a, 'i>(n: Node<'a, 'i>, tag: &'static str) -> impl Iterator<Item = Node<'a, 'i>> {
    n.children().filter(move |c| c.is_element() && c.tag_name().name() == tag)
}

fn kid<'a, 'i>(n: Node<'a, 'i>, tag: &'static str) -> Option<Node<'a, 'i>> {
    kids(n, tag).next()
}

fn attr<'a>(n: Node<'a, '_>, name: &str) -> Result<&'a str> {
    n.attribute(name).ok_or_else(|| anyhow!("<{}> is missing its '{name}' attribute", n.tag_name().name()))
}

fn plugin(p: Node) -> Result<Plugin> {
    let td = kid(p, "typeDescriptor");
    let (default, patterns) = match td.and_then(|t| kid(t, "dependencyType")) {
        Some(dt) => {
            let default = kind(kid(dt, "defaultType").and_then(|d| d.attribute("name")));
            let mut patterns = Vec::new();
            for pt in kid(dt, "patterns").into_iter().flat_map(|ps| kids(ps, "pattern")) {
                let c = cond(kid(pt, "dependencies").context("type pattern without <dependencies>")?)?;
                patterns.push((c, kind(kid(pt, "type").and_then(|t| t.attribute("name")))));
            }
            (default, patterns)
        }
        None => (kind(td.and_then(|t| kid(t, "type")).and_then(|t| t.attribute("name"))), vec![]),
    };
    let flags = kid(p, "conditionFlags")
        .into_iter()
        .flat_map(|f| kids(f, "flag"))
        .map(|f| (f.attribute("name").unwrap_or_default().to_owned(), f.text().unwrap_or_default().trim().to_owned()))
        .collect();
    Ok(Plugin {
        name: attr(p, "name")?.into(),
        description: kid(p, "description").and_then(|d| d.text()).unwrap_or_default().trim().into(),
        image: kid(p, "image").and_then(|i| i.attribute("path")).map(str::to_owned),
        files: files(kid(p, "files")),
        flags,
        default,
        patterns,
    })
}

fn files(list: Option<Node>) -> Vec<FileItem> {
    list.into_iter()
        .flat_map(|n| n.children())
        .filter_map(|c| {
            let folder = match c.tag_name().name() {
                "file" => false,
                "folder" => true,
                _ => return None,
            };
            // An empty source is how some authors write "this option installs nothing" (MO2 ignores it too).
            let source = c.attribute("source").filter(|s| !s.is_empty())?;
            Some(FileItem {
                source: source.into(),
                destination: c.attribute("destination").unwrap_or(source).into(),
                folder,
                priority: c.attribute("priority").and_then(|p| p.trim().parse().ok()).unwrap_or(0),
                always: c.attribute("alwaysInstall") == Some("true"),
                if_usable: c.attribute("installIfUsable") == Some("true"),
            })
        })
        .collect()
}

fn cond(n: Node) -> Result<Cond> {
    let mut parts = Vec::new();
    for c in n.children().filter(Node::is_element) {
        parts.push(match c.tag_name().name() {
            "flagDependency" => Cond::Flag(attr(c, "flag")?.into(), c.attribute("value").unwrap_or_default().into()),
            "fileDependency" => {
                let state = match attr(c, "state")? {
                    "Active" => FileState::Active,
                    "Inactive" => FileState::Inactive,
                    "Missing" => FileState::Missing,
                    s => bail!("unknown file state '{s}'"),
                };
                Cond::File(attr(c, "file")?.into(), state)
            }
            "dependencies" => cond(c)?,
            "foseDependency" | "nvseDependency" | "skseDependency" | "f4seDependency" => Cond::ScriptExtender,
            // ponytail: game and FOMM versions aren't read from the exe; treated as met. Parse the
            // exe's version resource if installers turn up that branch on it.
            "gameDependency" | "fommDependency" => Cond::All(vec![]),
            other => bail!("unknown condition <{other}>"),
        });
    }
    Ok(if n.attribute("operator") == Some("Or") { Cond::Any(parts) } else { Cond::All(parts) })
}

fn kind(name: Option<&str>) -> Kind {
    match name {
        Some("Required") => Kind::Required,
        Some("Recommended") => Kind::Recommended,
        Some("NotUsable") => Kind::NotUsable,
        Some("CouldBeUsable") => Kind::CouldBeUsable,
        _ => Kind::Optional, // MO2 falls back the same way
    }
}

fn group_kind(s: &str) -> Result<GroupKind> {
    Ok(match s {
        "SelectAtLeastOne" => GroupKind::SelectAtLeastOne,
        "SelectAtMostOne" => GroupKind::SelectAtMostOne,
        "SelectExactlyOne" => GroupKind::SelectExactlyOne,
        "SelectAll" => GroupKind::SelectAll,
        "SelectAny" => GroupKind::SelectAny,
        _ => bail!("unknown group type '{s}'"),
    })
}

/// Apply the list's `order` attribute; the schema default is Ascending (by name).
fn ordered<T>(mut v: Vec<T>, list: Node, name: fn(&T) -> &String) -> Result<Vec<T>> {
    match list.attribute("order").unwrap_or("Ascending") {
        "Explicit" => {}
        "Ascending" => v.sort_by(|a, b| name(a).cmp(name(b))),
        "Descending" => v.sort_by(|a, b| name(b).cmp(name(a))),
        o => bail!("unknown order '{o}'"),
    }
    Ok(v)
}

/// FOMOD files come as UTF-16 (often without a BOM), UTF-8, or a legacy code page.
fn decode(b: &[u8]) -> String {
    let utf16 = |b: &[u8], be: bool| {
        let units: Vec<u16> = b.as_chunks::<2>().0.iter().map(|&c| if be { u16::from_be_bytes(c) } else { u16::from_le_bytes(c) }).collect();
        String::from_utf16_lossy(&units)
    };
    let utf8 = |b: &[u8]| String::from_utf8(b.to_vec()).unwrap_or_else(|_| b.iter().map(|&c| c as char).collect());
    match b {
        [0xFF, 0xFE, rest @ ..] => utf16(rest, false),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, true),
        [0xEF, 0xBB, 0xBF, rest @ ..] => utf8(rest),
        [_, 0, ..] => utf16(b, false),
        [0, _, ..] => utf16(b, true),
        _ => utf8(b),
    }
}

/// `a\b/./c` -> `a/b/c`. None if the path climbs out with `..` or names a drive.
fn clean(p: &str) -> Option<String> {
    let parts: Vec<&str> = p.split(['/', '\\']).filter(|s| !s.is_empty() && *s != ".").collect();
    if parts.iter().any(|s| *s == ".." || s.contains(':')) {
        return None;
    }
    Some(parts.join("/"))
}
