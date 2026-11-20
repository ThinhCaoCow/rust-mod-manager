//! Mods from sites other than Nexus: GitHub releases, ModDB, and direct links to an archive.
//! Sites that need the user's sign-in (LoversLab, AFK Mods, Patreon) and pages rmm can't read
//! are downloaded in the user's browser; rmm takes the finished file from the Downloads folder
//! (`browser_downloads`, `take_browser_download`) and never touches the user's sign-in.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::instance::Instance;

/// Where a mod comes from on the web.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, schemars::JsonSchema)]
pub struct WebRef {
    /// The page it came from: a GitHub repository, a ModDB download, a forum post.
    pub page: String,
    /// The GitHub release's tag, for update checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The release's file, so an update takes the same one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

/// Sites that need the user's sign-in to download: (domain, name).
const SIGN_IN: [(&str, &str); 3] = [("loverslab.com", "LoversLab"), ("afkmods.com", "AFK Mods"), ("patreon.com", "Patreon")];

/// What a link is and what it offers.
#[derive(Serialize, Debug, Clone)]
pub struct Plan {
    /// "GitHub", "ModDB", "LoversLab", "AFK Mods", "Patreon", or "link".
    pub site: String,
    pub page: String,
    /// The mod's name, when the site says.
    pub name: Option<String>,
    /// Files to choose from.
    pub files: Vec<Offer>,
    /// A GitHub release's tag.
    pub version: Option<String>,
    /// rmm can't download it: the user downloads it in their browser (a sign-in site, or a page
    /// that isn't a file).
    pub browser: bool,
}

#[derive(Serialize, Debug, Clone)]
pub struct Offer {
    pub name: String,
    pub size: Option<u64>,
    /// The file, or for ModDB its download page.
    pub url: String,
}

/// The host of an http(s) URL, lowercase and without "www.".
fn host(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
    let h = rest.split(['/', '?', '#']).next()?.rsplit('@').next()?.split(':').next()?.to_lowercase();
    Some(h.strip_prefix("www.").map(str::to_owned).unwrap_or(h)).filter(|h| !h.is_empty())
}

fn on(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// The path of a URL, split, percent-decoded.
fn segments(url: &str) -> Vec<String> {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let path = rest.split(['?', '#']).next().unwrap_or("");
    path.split('/').skip(1).filter(|s| !s.is_empty()).map(decode).collect()
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        match (b[i], b.get(i + 1).copied().and_then(hex), b.get(i + 2).copied().and_then(hex)) {
            (b'%', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            (b'+', ..) => {
                out.push(b' ');
                i += 1;
            }
            (c, ..) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn is_archive(name: &str) -> bool {
    let n = name.to_lowercase();
    [".zip", ".7z", ".rar"].iter().any(|e| n.ends_with(e))
}

/// A page's text, as rmm (some sites turn curl's own name away).
fn get(url: &str, accept: &str) -> Result<String> {
    let out = crate::hidden("curl")
        .args(["-fsSL", "--proto", "=https", "--proto-redir", "=https", "--max-time", "30", "-A", concat!("rmm/", env!("CARGO_PKG_VERSION")), "-H"])
        .arg(format!("Accept: {accept}"))
        .args(["--", url])
        .stdin(Stdio::null())
        .output()
        .context("cannot run curl")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("403") && url.starts_with("https://api.github.com/") {
            bail!("GitHub's limit for checks without an account (60 an hour) is used up; try again later");
        }
        if err.contains("404") {
            bail!("{url} isn't there (404)");
        }
        bail!("couldn't read {url}: {}", err.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// What `url` offers. Reads the site; nothing is downloaded.
pub fn plan(url: &str) -> Result<Plan> {
    let url = url.trim();
    let Some(h) = host(url) else { bail!("'{url}' isn't a web link (https://…)") };
    if on(&h, "nexusmods.com") {
        bail!("Nexus Mods links install through From Nexus, or the page's Mod manager download button");
    }
    let browser = |site: &str, name: Option<String>| Plan { site: site.into(), page: url.into(), name, files: vec![], version: None, browser: true };
    if let Some((_, site)) = SIGN_IN.iter().find(|(d, _)| on(&h, d)) {
        return Ok(browser(site, None));
    }
    if h == "github.com" {
        return github(url);
    }
    if on(&h, "moddb.com") {
        return moddb(url);
    }
    match segments(url).last().filter(|n| is_archive(n)) {
        Some(file) => Ok(Plan {
            site: "link".into(),
            page: url.into(),
            name: Path::new(file).file_stem().map(|s| s.to_string_lossy().into_owned()),
            files: vec![Offer { name: file.clone(), size: None, url: url.into() }],
            version: None,
            browser: false,
        }),
        None => Ok(browser("link", None)),
    }
}

/// owner, repo, tag, file from github.com/o/r[/releases[/tag/T | /latest | /download/T/file]].
fn github_parts(url: &str) -> Option<(String, String, Option<String>, Option<String>)> {
    let s = segments(url);
    let (owner, repo) = (s.first()?.clone(), s.get(1)?.trim_end_matches(".git").to_string());
    let rest: Vec<&str> = s[2..].iter().map(String::as_str).collect();
    let (tag, file) = match rest[..] {
        ["releases", "tag", t, ..] => (Some(t.to_string()), None),
        ["releases", "download", t, f, ..] if t != "latest" => (Some(t.to_string()), Some(f.to_string())),
        ["releases", "latest", "download", f, ..] => (None, Some(f.to_string())),
        _ => (None, None),
    };
    Some((owner, repo, tag, file))
}

fn github(url: &str) -> Result<Plan> {
    let Some((owner, repo, tag, file)) = github_parts(url) else { bail!("'{url}' isn't a GitHub repository") };
    let api = match &tag {
        Some(t) => format!("https://api.github.com/repos/{owner}/{repo}/releases/tags/{t}"),
        None => format!("https://api.github.com/repos/{owner}/{repo}/releases/latest"),
    };
    let none = || anyhow::anyhow!("{owner}/{repo} has no releases on GitHub to download");
    let (version, files) = match get(&api, "application/vnd.github+json") {
        Ok(text) => {
            let release: serde_json::Value = serde_json::from_str(&text)?;
            let files: Vec<Offer> = release["assets"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|a| Some(Offer { name: a["name"].as_str()?.into(), size: a["size"].as_u64(), url: a["browser_download_url"].as_str()?.into() }))
                .collect();
            (release["tag_name"].as_str().map(str::to_owned), files)
        }
        Err(e) if e.to_string().contains("404") => return Err(none()),
        // The API's hourly limit is used up: the release's web pages, which don't count (no sizes).
        Err(e) if e.to_string().contains("limit") => {
            let tag = match tag {
                Some(t) => t,
                None => {
                    let to = redirect(&format!("https://github.com/{owner}/{repo}/releases/latest"))?;
                    to.rsplit_once("/releases/tag/").map(|(_, t)| decode(t)).ok_or_else(none)?
                }
            };
            let html = get(&format!("https://github.com/{owner}/{repo}/releases/expanded_assets/{tag}"), "text/html")?;
            let re = regex::Regex::new(&format!(r#"href="(/{}/{}/releases/download/[^"]+)""#, regex::escape(&owner), regex::escape(&repo))).unwrap();
            let files = re.captures_iter(&html).filter_map(|c| Some(Offer { name: segments(&c[1]).pop()?, size: None, url: format!("https://github.com{}", &c[1]) })).collect();
            (Some(tag), files)
        }
        Err(e) => return Err(e),
    };
    let files: Vec<Offer> = files.into_iter().filter(|o: &Offer| is_archive(&o.name) && file.as_ref().is_none_or(|f| f.eq_ignore_ascii_case(&o.name))).collect();
    if files.is_empty() {
        bail!("{owner}/{repo}'s release {} has no .zip, .7z or .rar to install", version.as_deref().unwrap_or("?"));
    }
    Ok(Plan { site: "GitHub".into(), page: format!("https://github.com/{owner}/{repo}"), name: Some(repo), files, version, browser: false })
}

fn og_title(html: &str) -> Option<String> {
    let re = regex::Regex::new(r#"<meta property="og:title" content="([^"]*)""#).unwrap();
    Some(re.captures(html)?[1].replace("&amp;", "&").replace("&#039;", "'").replace("&quot;", "\""))
}

/// "SIRT 1.0.0 - WINDOWS file - Skyrim Iron-Man Ruleset Tool mod for Elder Scrolls V: Skyrim"
/// -> "Skyrim Iron-Man Ruleset Tool".
fn moddb_name(title: &str) -> String {
    let t = title.split_once(" file - ").map_or(title, |(_, m)| m);
    let t = t.strip_prefix("Downloads - ").unwrap_or(t);
    [" mod for ", " addon for ", " for "].iter().find_map(|s| t.rsplit_once(s)).map_or(t, |(m, _)| m).trim().to_string()
}

fn moddb(url: &str) -> Result<Plan> {
    let s = segments(url);
    // /<kind>/<slug>/downloads/<file>: one file; /<kind>/<slug>[/…]: its downloads to choose from.
    let one = s.len() >= 4 && s[2] == "downloads" && !matches!(s[3].as_str(), "add" | "page");
    let page = if one { format!("https://www.moddb.com/{}", s[..4].join("/")) } else if s.len() >= 2 { format!("https://www.moddb.com/{}/{}/downloads", s[0], s[1]) } else { bail!("'{url}' isn't a ModDB mod or download page") };
    let html = get(&page, "text/html")?;
    let name = og_title(&html).map(|t| moddb_name(&t));
    let files = if one {
        let (file, _) = moddb_file(&page, &html)?;
        vec![Offer { name: file, size: None, url: page.clone() }]
    } else {
        let re = regex::Regex::new(&format!(r#"href="/{}/{}/downloads/([a-z0-9-]+)""#, regex::escape(&s[0]), regex::escape(&s[1]))).unwrap();
        let mut seen = Vec::<String>::new();
        for c in re.captures_iter(&html) {
            if !matches!(&c[1], "add" | "page" | "feed") && !seen.contains(&c[1].to_string()) {
                seen.push(c[1].into());
            }
        }
        seen.into_iter().map(|f| Offer { url: format!("https://www.moddb.com/{}/{}/downloads/{f}", s[0], s[1]), name: f, size: None }).collect()
    };
    if files.is_empty() {
        bail!("no downloads on {page}");
    }
    Ok(Plan { site: "ModDB".into(), page, name, files, version: None, browser: false })
}

/// A ModDB download page's file: (file name, the mirror's link to it).
fn moddb_file(page: &str, html: &str) -> Result<(String, String)> {
    let start = regex::Regex::new(r#"/downloads/start/(\d+)"#).unwrap();
    let id = start.captures(html).with_context(|| format!("no download button on {page}"))?[1].to_string();
    let mirrors = get(&format!("https://www.moddb.com/downloads/start/{id}"), "text/html")?;
    let mirror = regex::Regex::new(r#"/downloads/mirror/\d+/\d+/[0-9a-f]+"#).unwrap();
    let m = mirror.find(&mirrors).with_context(|| format!("ModDB offers no mirror for {page}"))?.as_str();
    let to = redirect(&format!("https://www.moddb.com{m}"))?;
    let file = segments(&to).pop().filter(|_| to.starts_with("https://")).with_context(|| format!("ModDB's mirror for {page} didn't hand out the file"))?;
    Ok((file, to))
}

/// Where `url` redirects to (empty when it doesn't).
fn redirect(url: &str) -> Result<String> {
    let out = crate::hidden("curl")
        .args(["-sS", "--proto", "=https", "--max-time", "30", "-A", concat!("rmm/", env!("CARGO_PKG_VERSION")), "-o"])
        .arg(if cfg!(windows) { "NUL" } else { "/dev/null" })
        .args(["-w", "%{redirect_url}", "--", url])
        .stdin(Stdio::null())
        .output()
        .context("cannot run curl")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A file downloaded from a link.
#[derive(Serialize, Debug)]
pub struct Fetched {
    pub path: PathBuf,
    /// The mod's name, when the site says.
    pub name: Option<String>,
    pub web: WebRef,
}

/// Download `url`'s file (`file` picks one when the link offers several) into the downloads
/// folder, recording where it came from for the install.
pub fn download(inst: &Instance, url: &str, file: Option<&str>) -> Result<Fetched> {
    let plan = plan(url)?;
    if plan.browser {
        let why = if plan.site == "link" { "isn't a file rmm can download".to_string() } else { format!("needs your sign-in on {}", plan.site) };
        bail!("{} {why}: download it in your browser, and the window's From a link… takes it from your Downloads folder (or install it with `rmm install <file>`)", plan.page);
    }
    let names = || plan.files.iter().map(|o| o.name.as_str()).collect::<Vec<_>>().join(", ");
    let offer = match (file, &plan.files[..]) {
        (Some(f), all) => all.iter().find(|o| o.name.eq_ignore_ascii_case(f)).with_context(|| format!("no file '{f}' there; it offers {}", names()))?,
        (None, [one]) => one,
        (None, _) => bail!("{} offers several files; pick one: {}", plan.page, names()),
    };
    let (name, direct) = if plan.site == "ModDB" {
        moddb_file(&offer.url, &get(&offer.url, "text/html")?)?
    } else {
        (offer.name.clone(), offer.url.clone())
    };
    let name = crate::install::sanitize(&name);
    fs::create_dir_all(inst.downloads_dir())?;
    let dest = inst.downloads_dir().join(&name);
    crate::nexus::fetch_url(&direct, &dest)?;
    let web = WebRef { page: plan.page.clone(), version: plan.version.clone(), file: (plan.site == "GitHub").then(|| offer.name.clone()) };
    remember(inst, &name, &web)?;
    Ok(Fetched { path: dest, name: plan.name, web })
}

/// Download from a link and install it (FOMOD installers take their defaults unless `choices`
/// are given).
pub fn install(inst: &Instance, url: &str, file: Option<&str>, choices: Option<&crate::fomod::Choices>) -> Result<crate::install::InstallReport> {
    let f = download(inst, url, file)?;
    crate::install::install(inst, &f.path, f.name.as_deref(), true, Some(&choices.cloned().unwrap_or_default()))
}

const KNOWN: &str = ".web.json";

/// Downloads from the web, by file name (lowercase), in `downloads/.web.json`.
pub fn known(inst: &Instance) -> BTreeMap<String, WebRef> {
    fs::read_to_string(inst.downloads_dir().join(KNOWN)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn remember(inst: &Instance, file: &str, web: &WebRef) -> Result<()> {
    let mut all = known(inst);
    all.insert(file.to_lowercase(), web.clone());
    fs::write(inst.downloads_dir().join(KNOWN), serde_json::to_string_pretty(&all)?)?;
    Ok(())
}

/// Where the file a mod is installed from came from, when rmm fetched or took it.
pub fn of_source(inst: &Instance, source: &Path) -> Option<WebRef> {
    known(inst).remove(&source.file_name()?.to_string_lossy().to_lowercase())
}

/// A finished download in the user's Downloads folder.
#[derive(Serialize, Debug)]
pub struct BrowserDownload {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    /// Unix seconds.
    pub modified: u64,
}

fn secs(t: std::time::SystemTime) -> u64 {
    t.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Archives in the user's Downloads folder finished at or after `since` (Unix seconds), newest
/// first. Ones a browser is still writing (an empty file beside a .part) aren't listed.
pub fn browser_downloads(since: u64) -> Vec<BrowserDownload> {
    let Some(dir) = dirs::download_dir() else { return vec![] };
    let mut out: Vec<BrowserDownload> = fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let (name, m) = (e.file_name().to_string_lossy().into_owned(), e.metadata().ok()?);
            let modified = secs(m.modified().ok()?);
            let writing = dir.join(format!("{name}.part")).exists();
            (m.is_file() && is_archive(&name) && m.len() > 0 && modified >= since && !writing).then(|| BrowserDownload { path: e.path(), name, size: m.len(), modified })
        })
        .collect();
    out.sort_by_key(|d| std::cmp::Reverse(d.modified));
    out
}

/// Move an archive from the user's Downloads folder into rmm's, recording the page it came from.
/// Returns its new path.
pub fn take_browser_download(inst: &Instance, path: &Path, page: Option<&str>) -> Result<PathBuf> {
    let dir = dirs::download_dir().context("no Downloads folder")?;
    let inside = path.parent().and_then(|p| p.canonicalize().ok()).zip(dir.canonicalize().ok()).is_some_and(|(p, d)| p == d);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if !inside || !path.is_file() || !is_archive(&name) {
        bail!("{} isn't an archive in your Downloads folder", path.display());
    }
    fs::create_dir_all(inst.downloads_dir())?;
    let name = crate::install::sanitize(&name);
    let (stem, ext) = name.rsplit_once('.').unwrap_or((&name, ""));
    let dest = (1..).map(|n| if n == 1 { name.clone() } else { format!("{stem} ({n}).{ext}") }).map(|n| inst.downloads_dir().join(n)).find(|p| !p.exists()).expect("a free name");
    if fs::rename(path, &dest).is_err() {
        fs::copy(path, &dest)?; // another drive
        fs::remove_file(path)?;
    }
    if let Some(page) = page.filter(|p| host(p).is_some()) {
        remember(inst, &dest.file_name().unwrap().to_string_lossy(), &WebRef { page: page.into(), ..Default::default() })?;
    }
    Ok(dest)
}

/// A newer GitHub release of an installed mod.
#[derive(Serialize, Debug, Clone)]
pub struct WebUpdate {
    pub id: String,
    pub name: String,
    pub installed: Option<String>,
    pub latest: String,
    pub page: String,
    /// The release's file that replaces the installed one; None when none clearly does (pick one
    /// with `files`).
    pub file: Option<String>,
    pub files: Vec<String>,
}

/// Newer GitHub releases of the profile's mods (`ids` limits it to some). Held mods and skipped
/// versions aren't offered.
pub fn check_updates(inst: &Instance, ids: Option<&[String]>) -> Result<Vec<WebUpdate>> {
    let mut out = Vec::new();
    let mods: Vec<(String, crate::instance::ModMeta)> = inst
        .profile()?
        .mods
        .into_iter()
        .filter(|m| ids.is_none_or(|ids| ids.contains(&m.id)))
        .map(|m| (m.id.clone(), inst.mod_meta(&m.id)))
        .filter(|(_, meta)| meta.web.as_ref().is_some_and(|w| w.version.is_some() && host(&w.page).as_deref() == Some("github.com")))
        .collect();
    for (i, (id, meta)) in mods.iter().enumerate() {
        crate::progress::report("checking GitHub for updates", i, mods.len());
        let w = meta.web.as_ref().unwrap();
        let plan = match github(&w.page) {
            Ok(p) => p,
            Err(e) if e.to_string().contains("limit") => return Err(e),
            Err(_) => continue, // gone, or no archive in the latest release
        };
        let Some(latest) = plan.version.clone() else { continue };
        if Some(&latest) == w.version.as_ref() || meta.pinned || meta.skip_version.as_ref() == Some(&latest) {
            continue;
        }
        let names: Vec<String> = plan.files.iter().map(|o| o.name.clone()).collect();
        out.push(WebUpdate { id: id.clone(), name: meta.name.clone(), installed: w.version.clone(), latest, page: w.page.clone(), file: same_file(w.file.as_deref(), &names), files: names });
    }
    Ok(out)
}

/// The file in a new release that stands for `old`: the only one, the same name, or the one
/// whose name matches once version numbers are taken out.
fn same_file(old: Option<&str>, names: &[String]) -> Option<String> {
    if let [one] = names {
        return Some(one.clone());
    }
    let old = old?;
    let bare = |n: &str| n.to_lowercase().chars().filter(|c| !c.is_ascii_digit() && *c != '.').collect::<String>();
    names.iter().find(|n| n.eq_ignore_ascii_case(old)).or_else(|| names.iter().find(|n| bare(n) == bare(old))).cloned()
}

/// Update a mod to its GitHub repository's latest release (`file` picks the release's file when
/// the check couldn't), as one undoable step.
pub fn update(inst: &Instance, id: &str, file: Option<&str>, choices: Option<&crate::fomod::Choices>) -> Result<crate::install::InstallReport> {
    let w = inst.mod_meta(id).web.with_context(|| format!("{id} didn't come from a web link rmm knows"))?;
    let file = match file {
        Some(f) => Some(f.to_string()),
        None => same_file(w.file.as_deref(), &plan(&w.page)?.files.into_iter().map(|o| o.name).collect::<Vec<_>>()),
    };
    let f = download(inst, &w.page, file.as_deref())?;
    crate::install::update(inst, id, &f.path, choices)
}

/// Skip a mod's newer `version` (a later one is offered again).
pub fn skip(inst: &Instance, id: &str, version: &str) -> Result<()> {
    if !inst.mod_dir(id).join(crate::instance::MOD_META).is_file() {
        bail!("mod '{id}' is not installed");
    }
    inst.locked(|| inst.write_mod_meta(id, &crate::instance::ModMeta { skip_version: Some(version.into()), ..inst.mod_meta(id) }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_and_paths() {
        assert_eq!(host("https://www.LoversLab.com/files/file/1-x/").as_deref(), Some("loverslab.com"));
        assert_eq!(host("http://user@example.com:8080/a").as_deref(), Some("example.com"));
        assert_eq!(host("ftp://example.com"), None);
        assert!(on("dl.moddb.com", "moddb.com") && !on("notmoddb.com", "moddb.com"));
        assert_eq!(segments("https://x.org/a%20b/c.zip?d=1"), ["a b", "c.zip"]);
    }

    #[test]
    fn github_links() {
        let p = |u| github_parts(u).unwrap();
        assert_eq!(p("https://github.com/o/r"), ("o".into(), "r".into(), None, None));
        assert_eq!(p("https://github.com/o/r.git"), ("o".into(), "r".into(), None, None));
        assert_eq!(p("https://github.com/o/r/releases/tag/v1.2"), ("o".into(), "r".into(), Some("v1.2".into()), None));
        assert_eq!(p("https://github.com/o/r/releases/download/v1/m%20x.7z"), ("o".into(), "r".into(), Some("v1".into()), Some("m x.7z".into())));
        assert_eq!(p("https://github.com/o/r/releases/latest/download/m.zip"), ("o".into(), "r".into(), None, Some("m.zip".into())));
        assert!(github_parts("https://github.com/o").is_none());
    }

    #[test]
    fn sites_without_reading_them() {
        let p = plan("https://www.loverslab.com/files/file/123-some-mod/").unwrap();
        assert!(p.browser && p.site == "LoversLab");
        assert!(plan("https://afkmods.com/index.php?/files/file/2-x/").unwrap().browser);
        assert!(plan("https://example.com/about").unwrap().browser);
        let p = plan("https://example.com/files/My%20Mod%201.2.7z?x=1").unwrap();
        assert!(!p.browser && p.files[0].name == "My Mod 1.2.7z" && p.name.as_deref() == Some("My Mod 1.2"));
        assert!(plan("https://www.nexusmods.com/skyrimspecialedition/mods/1").is_err());
        assert!(plan("file:///C:/x.zip").is_err());
    }

    #[test]
    fn names() {
        assert_eq!(moddb_name("SIRT 1.0.0 - WINDOWS file - Skyrim Iron-Man Ruleset Tool mod for Elder Scrolls V: Skyrim"), "Skyrim Iron-Man Ruleset Tool");
        assert_eq!(moddb_name("Enderal mod for Elder Scrolls V: Skyrim"), "Enderal");
        assert_eq!(moddb_name("Downloads - Skyrim Iron-Man Ruleset Tool mod for Elder Scrolls V: Skyrim"), "Skyrim Iron-Man Ruleset Tool");
        let n = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(same_file(Some("Mod-1.2.zip"), &n(&["Mod-1.3.zip", "Mod-src-1.3.zip"])).as_deref(), Some("Mod-1.3.zip"));
        assert_eq!(same_file(None, &n(&["only.7z"])).as_deref(), Some("only.7z"));
        assert_eq!(same_file(Some("a.zip"), &n(&["b.zip", "c.zip"])), None);
    }
}
