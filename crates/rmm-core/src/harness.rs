//! `test_run`: deploy, start the game through the script extender, and see whether it holds up.
//!
//! With rmm's harness plugin deployed (`rmm_harness.dll` in SKSE/F4SE Plugins), the game connects
//! back over TCP on 127.0.0.1 (which works through Proton, unlike named pipes) and the run is
//! scripted: wait for the main menu, load a save, `coc` through cells (the first one can start the
//! game from the main menu) while sampling FPS, then quit. The port and a one-time token reach the plugin as the environment
//! variables `RMM_HARNESS_PORT` and `RMM_HARNESS_TOKEN`, which the loader passes on to the game.
//! The protocol is JSON lines:
//!   plugin -> rmm  {"ev":"hello","token":"…","runtime":"1.6.1170"}  {"ev":"data_loaded"}
//!                  {"ev":"main_menu"}  {"ev":"game_loaded","save":"…"} (a loading screen closed in the world)  {"ev":"heartbeat","fps":58.2,"cell":"…"}
//!                  {"ev":"result","id":N,"ok":true,"out":"…"}
//!   rmm -> plugin  {"id":N,"cmd":"load_save","name":"…"}
//!                  {"id":N,"cmd":"console","line":"coc Riverwood"}  {"id":N,"cmd":"screenshot","path":"…png"}
//!                  {"id":N,"cmd":"quit"}
//!
//! The game runs in a small window behind the others unless asked otherwise: the prefs INI is set
//! to a window and the main INI to `bAlwaysActive` for the run (both put back once the game exits),
//! and `RMM_HARNESS_BEHIND` tells the plugin to send its window to the back.
//!
//! Without the plugin it falls back to watching the process for a while. Either way a crash log
//! written during the run is read back to the mods it names, and so is the Papyrus log.
use crate::logs::{self, CrashReport, PapyrusReport};
use crate::{Game, Instance, deploy, launch};
use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Deserialize, JsonSchema, Default, Debug, Clone)]
pub struct Scenario {
    /// Load this save (its name without .ess/.fos) once the main menu is up. Harness only.
    #[serde(default)]
    pub save: Option<String>,
    /// Cells to visit with `coc`, staying `dwell` seconds in each and sampling FPS. Without a save
    /// the first `coc` starts the game from the main menu. Harness only.
    #[serde(default)]
    pub cells: Vec<String>,
    /// Seconds to stay after loading and in each cell (default 10).
    #[serde(default)]
    pub dwell: Option<u64>,
    /// Seconds to wait for the main menu and for each cell to load (harness), or for the game to
    /// keep running (without it). Default 180 and 120.
    #[serde(default)]
    pub timeout: Option<u64>,
    /// Leave the game running at the end instead of closing it.
    #[serde(default)]
    pub keep_open: bool,
    /// The game's window: "WxH" runs it windowed at that size, behind the other windows (default
    /// "960x540": small, and still readable in screenshots); "fullscreen" keeps the game's own
    /// display settings. The prefs INI is changed for the run and put back once the game exits.
    /// With keep_open the game keeps its own settings, since it saves them when it closes.
    #[serde(default)]
    pub window: Option<String>,
    /// Save a PNG of the game's window after loading the save and in each cell, into
    /// `test-runs/<time>/` in the instance. Harness only.
    #[serde(default)]
    pub screenshots: bool,
    /// What to do after the save and the cells, in order: a test plan. Each step is one of
    /// {"cell": "Riverwood"} (coc and wait for the load), {"console": "player.placeatme 00023ABA 2"},
    /// {"wait": 10} (seconds, sampling FPS), {"screenshot": "name"} or {"load": "save name"}.
    /// Harness only.
    #[serde(default)]
    pub steps: Vec<Step>,
}

/// One thing for a test to do in the game.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// Go to a cell with coc (its editor id) and wait for it to load. From the main menu this
    /// starts the game there.
    Cell(String),
    /// Run a console command: player.moveto, movetoqt, setstage, startquest, player.placeatme,
    /// player.additem, player.addspell, tgm, tai, fw, set gamehour to 22... Its text output isn't
    /// read back: follow it with wait and screenshot to see what it did.
    Console(String),
    /// Let the game run this many seconds (at most 600), sampling FPS.
    Wait(u64),
    /// A PNG of the game's window, named after this.
    Screenshot(String),
    /// Load a save by name (without .ess/.fos) and wait for it.
    Load(String),
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct StepResult {
    pub step: Step,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fps: Option<Fps>,
    /// The player's cell afterwards, as the plugin last reported it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cell: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<std::path::PathBuf>,
}

#[derive(Serialize, Debug, Default)]
pub struct TestRun {
    pub passed: bool,
    pub summary: String,
    /// "harness" when the plugin connected, "process" when only the process was watched.
    pub mode: String,
    pub runtime: Option<String>,
    /// Seconds from the start to each milestone: hello, data_loaded, main_menu, game_loaded, end.
    pub timings: BTreeMap<String, f64>,
    pub cells: Vec<CellResult>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<StepResult>,
    pub fps: Option<Fps>,
    /// A crash log written during the run, read back to the mods it names.
    pub crash: Option<CrashReport>,
    /// The Papyrus log, when the game wrote one during the run.
    pub papyrus: Option<PapyrusReport>,
    pub launch: Option<launch::LaunchReport>,
    /// PNGs of the game's window (with `screenshots`).
    pub screenshots: Vec<std::path::PathBuf>,
    /// What didn't work without failing the run.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Serialize, Debug, Default, Clone, PartialEq)]
pub struct CellResult {
    pub cell: String,
    pub ok: bool,
    pub error: Option<String>,
    pub fps: Option<Fps>,
}

#[derive(Serialize, Debug, Default, Clone, Copy, PartialEq)]
pub struct Fps {
    pub avg: f64,
    pub min: f64,
    pub samples: usize,
}

fn fps(samples: &[f64]) -> Option<Fps> {
    (!samples.is_empty()).then(|| Fps {
        avg: (samples.iter().sum::<f64>() / samples.len() as f64 * 10.0).round() / 10.0,
        min: samples.iter().cloned().fold(f64::INFINITY, f64::min),
        samples: samples.len(),
    })
}

/// The harness plugin's DLL, relative to Data.
pub fn harness_dll(game: Game) -> &'static str {
    if game.base() == Game::SkyrimSe { "SKSE/Plugins/rmm_harness.dll" } else { "F4SE/Plugins/rmm_harness.dll" }
}

/// Whether the harness plugin is deployed (or will be), so test_run can script the game.
pub fn has_harness(inst: &Instance) -> Result<bool> {
    let dll = harness_dll(inst.config.game);
    Ok(crate::conflicts::file_map(inst, &inst.profile()?)?.winners().contains_key(&crate::path_key(dll)) || inst.data_dir().join(dll).is_file())
}

/// What a test run would do: the launch checks and deploy plan, and which mode it would use.
pub fn preview(inst: &Instance) -> Result<Value> {
    let harness = has_harness(inst)?;
    Ok(json!({
        "dry_run": true,
        "mode": if harness { "harness" } else { "process" },
        "launch": launch::launch(inst, true)?,
        "note": if harness { "the harness plugin is deployed: the run is scripted" } else { "no harness plugin: the run starts the game and watches the process and the logs" },
    }))
}

/// Deploy, launch, and test the game. See the module docs.
pub fn test_run(inst: &Instance, s: &Scenario) -> Result<TestRun> {
    let mut run = Run::begin(inst, s)?;
    let alive = run.alive();
    if has_harness(inst)? {
        let wants_shots = s.screenshots || s.steps.iter().any(|x| matches!(x, Step::Screenshot(_)));
        let (server, shots) = run.launch_harness(inst, wants_shots)?;
        match server.accept(Duration::from_secs(s.timeout.unwrap_or(180)), &alive) {
            Ok((conn, runtime)) => {
                run.r.runtime = runtime;
                run.r.timings.insert("hello".into(), secs(run.t0));
                drive(conn, s, run.t0, shots.as_deref(), &alive, &mut run.r);
            }
            Err(e) => run.r.summary = format!("{e:#}"),
        }
    } else {
        run.r.launch = Some(launch::launch(inst, false)?);
        run.r.mode = "process".into();
        watch(s, run.t0, &alive, &mut run.r);
    }
    run.end(inst, s.keep_open)
}

/// One test run's state from launch to the end: what has to be checked and put back afterwards.
struct Run {
    r: TestRun,
    t0: Instant,
    /// Unix seconds at the start (less 2), to tell this run's crash log from older ones.
    started: u64,
    before: Vec<std::path::PathBuf>,
    /// Put back once the game has closed.
    prefs: Option<Restore>,
    game: std::path::PathBuf,
    exe: String,
}

impl Run {
    fn begin(inst: &Instance, s: &Scenario) -> Result<Run> {
        let started = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs().saturating_sub(2);
        let before = logs::crash_logs(inst).into_iter().map(|l| l.path).collect();
        let mut r = TestRun::default();
        let prefs = match window_size(s)? {
            Some((w, h)) => windowed(inst, w, h).map_err(|e| r.warnings.push(format!("the game used its own display settings: {e:#}"))).ok(),
            None => None,
        };
        Ok(Run { r, t0: Instant::now(), started, before, prefs, game: inst.config.game_path.clone(), exe: inst.config.game.exe().into() })
    }

    fn alive(&self) -> impl Fn() -> bool + Send + 'static {
        let (game, exe) = (self.game.clone(), self.exe.clone());
        move || deploy::game_running(&game, &exe)
    }

    /// Start the game with the harness's port and token; with `shots`, a folder for screenshots.
    fn launch_harness(&mut self, inst: &Instance, shots: bool) -> Result<(Server, Option<std::path::PathBuf>)> {
        let server = Server::start()?;
        let mut env = vec![("RMM_HARNESS_PORT", server.port.to_string()), ("RMM_HARNESS_TOKEN", server.token.clone())];
        if self.prefs.is_some() {
            env.push(("RMM_HARNESS_BEHIND", "1".into()));
        }
        // Absolute: the plugin resolves a relative path against the game's folder.
        let shots = shots.then(|| std::path::absolute(inst.root.join("test-runs").join(self.started.to_string()))).transpose()?;
        if let Some(dir) = &shots {
            std::fs::create_dir_all(dir)?;
        }
        self.r.launch = Some(launch::launch_with(inst, false, &env)?);
        self.r.mode = "harness".into();
        Ok((server, shots))
    }

    /// Close the game (unless `keep_open`), read back a crash log and the Papyrus log, and put
    /// the INIs back.
    fn end(mut self, inst: &Instance, keep_open: bool) -> Result<TestRun> {
        let alive = self.alive();
        if !keep_open && alive() {
            close(&self.game.join(&self.exe));
        }
        let mut r = std::mem::take(&mut self.r);
        r.timings.insert("end".into(), secs(self.t0));

        // A crash log written during the run fails it, whatever else happened. The logger may still
        // be writing it as the game goes, so a failed run waits a little for one.
        let new_log = || logs::crash_logs(inst).into_iter().find(|l| l.modified >= self.started && !self.before.contains(&l.path));
        let mut found = new_log();
        let grace = Instant::now();
        while found.is_none() && !r.passed && grace.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(250));
            found = new_log();
        }
        if let Some(new) = found {
            let report = logs::read_crash_log(inst, Some(&new.path))?;
            let top = report.suspects.iter().take(3).map(|x| x.mod_id.clone().unwrap_or_else(|| x.name.clone())).collect::<Vec<_>>();
            r.summary = format!("the game crashed{}", if top.is_empty() { String::new() } else { format!("; suspects: {}", top.join(", ")) });
            r.passed = false;
            r.crash = Some(report);
        }
        r.papyrus = logs::read_papyrus_log(inst, 20).ok().filter(|p| p.modified.is_some_and(|m| m >= self.started));
        Ok(r)
    }
}

/// A game started for a tester that sends steps a batch at a time, looking at the results (and
/// screenshots) before deciding the next, until `stop`. Dropping it closes the game.
pub struct Session {
    run: Option<Run>,
    conn: Conn,
    shots: std::path::PathBuf,
    timeout: Duration,
    alive: Box<dyn Fn() -> bool + Send>,
}

impl Session {
    /// Start the game with the harness plugin, wait for the main menu, then load `s.save`, visit
    /// `s.cells` and run `s.steps`. Screenshots go to `test-runs/<time>/`.
    pub fn start(inst: &Instance, s: &Scenario) -> Result<Session> {
        if !has_harness(inst)? {
            bail!("a live test needs the harness plugin deployed (harness/README.md); without it test_run can only watch the process");
        }
        let mut run = Run::begin(inst, &Scenario { keep_open: false, ..s.clone() })?;
        let alive = run.alive();
        let timeout = Duration::from_secs(s.timeout.unwrap_or(180));
        let up = (|| -> Result<(Conn, std::path::PathBuf)> {
            let (server, shots) = run.launch_harness(inst, true)?;
            let (mut c, runtime) = server.accept(timeout, &alive)?;
            run.r.runtime = runtime;
            run.r.timings.insert("hello".into(), secs(run.t0));
            for ev in ["data_loaded", "main_menu"] {
                c.until(ev, timeout, &alive)?;
                run.r.timings.insert(ev.into(), secs(run.t0));
            }
            Ok((c, shots.expect("asked for a folder")))
        })();
        match up {
            Ok((conn, shots)) => {
                let mut session = Session { run: Some(run), conn, shots, timeout, alive: Box::new(alive) };
                let first: Vec<Step> = s.save.iter().map(|x| Step::Load(x.clone())).chain(s.cells.iter().map(|c| Step::Cell(c.clone()))).chain(s.steps.iter().cloned()).collect();
                session.steps(&first);
                Ok(session)
            }
            Err(e) => {
                let r = run.end(inst, false)?;
                bail!("{e:#}{}", if r.crash.is_some() { format!("; {}", r.summary) } else { String::new() })
            }
        }
    }

    /// Run steps in order, stopping early if the game closes. Returns their results.
    pub fn steps(&mut self, steps: &[Step]) -> Vec<StepResult> {
        let Session { run, conn, shots, timeout, alive } = self;
        let run = run.as_mut().expect("a running session");
        let t0 = run.t0;
        let mut out = Vec::new();
        for step in steps {
            let res = run_step(conn, step, *timeout, Some(shots), alive);
            if res.ok && matches!(step, Step::Load(_) | Step::Cell(_)) {
                run.r.timings.entry("game_loaded".into()).or_insert_with(|| secs(t0));
            }
            run.r.screenshots.extend(res.screenshot.clone());
            run.r.steps.push(res.clone());
            out.push(res);
            if !alive() {
                break;
            }
        }
        out
    }

    /// Whether the game is still running.
    pub fn alive(&self) -> bool {
        (self.alive)()
    }

    /// The run so far: timings, every step's result, screenshots, warnings.
    pub fn progress(&self) -> &TestRun {
        &self.run.as_ref().expect("a running session").r
    }

    /// Quit the game and report the whole session, crash log and Papyrus log included.
    pub fn stop(mut self, inst: &Instance) -> Result<TestRun> {
        if (self.alive)() {
            let _ = self.conn.command(json!({ "cmd": "quit" }), Duration::from_secs(10), &self.alive);
            let end = Instant::now();
            while (self.alive)() && end.elapsed() < Duration::from_secs(30) {
                std::thread::sleep(Duration::from_millis(250));
            }
        }
        let mut run = self.run.take().expect("a running session");
        judge(&mut run.r);
        run.end(inst, false)
    }
}

impl Drop for Session {
    /// A session nobody stopped (the AI client went away): close the game, then put the INIs back.
    fn drop(&mut self) {
        if let Some(run) = self.run.take() {
            if (self.alive)() {
                close(&run.game.join(&run.exe));
            }
            let end = Instant::now();
            while (self.alive)() && end.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    }
}

/// The window size a scenario asks for, or None for the game's own display settings.
fn window_size(s: &Scenario) -> Result<Option<(u32, u32)>> {
    let w = s.window.as_deref().unwrap_or("960x540");
    if s.keep_open || w.eq_ignore_ascii_case("fullscreen") {
        return Ok(None);
    }
    let (a, b) = w.split_once(['x', 'X']).context("window is \"WxH\" or \"fullscreen\"")?;
    Ok(Some((a.trim().parse()?, b.trim().parse()?)))
}

/// INIs as they were before the run (None: there was none); put back when dropped.
struct Restore(Vec<(std::path::PathBuf, Option<Vec<u8>>)>);

impl Drop for Restore {
    fn drop(&mut self) {
        for (path, before) in &self.0 {
            let _ = match before {
                Some(b) => std::fs::write(path, b),
                None => std::fs::remove_file(path),
            };
        }
    }
}

/// Make the game windowed at `w`x`h` for this run (`[Display]` in `<game>Prefs.ini`), and keep it
/// running behind other windows (`bAlwaysActive` in `<game>.ini`: the game reads it only there, at
/// start; without it the clock stops and nothing is drawn). `<game>Custom.ini` is only kept: a
/// windowed Fallout 4 writes its window position there.
fn windowed(inst: &Instance, w: u32, h: u32) -> Result<Restore> {
    let main = inst.config.game.ini_name();
    let display = [("Display", "bFull Screen", Some("0".to_string())), ("Display", "bBorderless", Some("0".into())), ("Display", "iSize W", Some(w.to_string())), ("Display", "iSize H", Some(h.to_string()))];
    let mut restore = Restore(Vec::new());
    for (name, set) in [(main.replace(".ini", "Prefs.ini"), &display[..]), (main.to_string(), &[("General", "bAlwaysActive", Some("1".to_string()))][..])] {
        let path = inst.ini_file(&name).context("no My Games folder for the game's INIs")?;
        let before = std::fs::read(&path).with_context(|| format!("there is no {name} yet; start the game once"))?;
        std::fs::write(&path, crate::health::ini_set(&before, set))?;
        restore.0.push((path, Some(before)));
    }
    if let Some(path) = inst.ini_file(&main.replace(".ini", "Custom.ini")) {
        let before = std::fs::read(&path).ok();
        restore.0.push((path, before));
    }
    Ok(restore)
}

/// Ask the plugin for a PNG of the game's window.
fn screenshot(c: &mut Conn, dir: &Path, name: &str, alive: &dyn Fn() -> bool) -> Result<std::path::PathBuf> {
    let file = dir.join(format!("{}.png", name.replace(|ch: char| !ch.is_ascii_alphanumeric() && ch != '-', "_")));
    let target = if cfg!(windows) { file.display().to_string() } else { crate::proton::windows_path(&file) };
    c.command(json!({ "cmd": "screenshot", "path": target }), Duration::from_secs(30), alive)?;
    Ok(file)
}

fn shot(c: &mut Conn, dir: Option<&Path>, name: &str, alive: &dyn Fn() -> bool, r: &mut TestRun) {
    let Some(dir) = dir else { return };
    match screenshot(c, dir, name, alive) {
        Ok(file) => r.screenshots.push(file),
        Err(e) => r.warnings.push(format!("no screenshot of {name}: {e:#}")),
    }
}

/// Do one step; its failure is in the result.
fn run_step(c: &mut Conn, step: &Step, timeout: Duration, shots: Option<&Path>, alive: &dyn Fn() -> bool) -> StepResult {
    let mut res = StepResult { step: step.clone(), ok: true, error: None, fps: None, cell: None, screenshot: None };
    let console = |c: &mut Conn, line: String| c.command(json!({ "cmd": "console", "line": line }), Duration::from_secs(60), alive);
    let done = match step {
        // A cell that doesn't exist never loads; the console doesn't say so.
        Step::Cell(cell) => console(c, format!("coc {cell}"))
            .and_then(|_| c.until("game_loaded", timeout, alive).map_err(|e| if alive() { anyhow::anyhow!("{cell} didn't load") } else { e }))
            .map(drop),
        Step::Load(save) => c.command(json!({ "cmd": "load_save", "name": save }), Duration::from_secs(30), alive).and_then(|_| c.until("game_loaded", Duration::from_secs(300), alive)).map(drop),
        Step::Console(line) => console(c, line.clone()).map(drop),
        Step::Wait(n) => c.sample(Duration::from_secs((*n).min(600)), alive).map(|f| res.fps = fps(&f)),
        Step::Screenshot(name) => match shots {
            Some(dir) => screenshot(c, dir, name, alive).map(|f| res.screenshot = Some(f)),
            None => Err(anyhow::anyhow!("screenshots are off for this run")),
        },
    };
    if let Err(e) = done {
        res.ok = false;
        res.error = Some(format!("{e:#}"));
    }
    res.cell = c.cell.clone();
    res
}

/// Passed when every cell and step worked; the summary names the lowest FPS or what failed.
fn judge(r: &mut TestRun) {
    let cells: Vec<&str> = r.cells.iter().filter(|c| !c.ok).map(|c| c.cell.as_str()).collect();
    let steps: Vec<String> = r.steps.iter().enumerate().filter(|(_, s)| !s.ok).map(|(i, s)| format!("step {} ({})", i + 1, s.error.as_deref().unwrap_or("failed"))).collect();
    r.passed = cells.is_empty() && steps.is_empty();
    let mut failed = Vec::new();
    if !cells.is_empty() {
        failed.push(format!("couldn't go to {}", cells.join(", ")));
    }
    if !steps.is_empty() {
        failed.push(format!("failed: {}", steps.join("; ")));
    }
    r.summary = if r.passed {
        let f = r.fps.iter().chain(r.cells.iter().filter_map(|c| c.fps.as_ref())).chain(r.steps.iter().filter_map(|s| s.fps.as_ref())).map(|f| f.min).fold(f64::INFINITY, f64::min);
        format!("passed{}", if f.is_finite() { format!("; lowest FPS {f:.0}") } else { String::new() })
    } else {
        failed.join("; ")
    };
}

fn secs(t0: Instant) -> f64 {
    (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0
}

/// Without the harness: the game has to show up and still be running when the time is up.
fn watch(s: &Scenario, t0: Instant, alive: &dyn Fn() -> bool, r: &mut TestRun) {
    let limit = Duration::from_secs(s.timeout.unwrap_or(120));
    let mut seen = false;
    while t0.elapsed() < limit {
        let up = alive();
        if up && !seen {
            seen = true;
            r.timings.insert("running".into(), secs(t0));
        }
        if seen && !up {
            r.summary = format!("the game closed after {:.0} seconds", t0.elapsed().as_secs_f64());
            return;
        }
        if !seen && t0.elapsed() > Duration::from_secs(30) {
            r.summary = "the game didn't start, or closed at once".into();
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    if !seen {
        r.summary = "the game didn't start, or closed at once".into();
        return;
    }
    r.passed = true;
    r.summary = format!("the game ran for {} seconds (no harness plugin, so only the process and the logs were checked)", limit.as_secs());
}

/// Play the scenario over a connected harness. Failures end up in `r.summary`.
fn drive(mut c: Conn, s: &Scenario, t0: Instant, shots: Option<&Path>, alive: &dyn Fn() -> bool, r: &mut TestRun) {
    let dwell = Duration::from_secs(s.dwell.unwrap_or(10));
    let step = |c: &mut Conn, r: &mut TestRun| -> Result<()> {
        let timeout = Duration::from_secs(s.timeout.unwrap_or(180));
        for ev in ["data_loaded", "main_menu"] {
            c.until(ev, timeout, alive)?;
            r.timings.insert(ev.into(), secs(t0));
        }
        if let Some(save) = &s.save {
            c.command(json!({ "cmd": "load_save", "name": save }), Duration::from_secs(30), alive)?;
            c.until("game_loaded", Duration::from_secs(300), alive)?;
            r.timings.insert("game_loaded".into(), secs(t0));
            r.fps = fps(&c.sample(dwell, alive)?);
            shot(c, shots, "save", alive, r);
        }
        for cell in &s.cells {
            let mut res = CellResult { cell: cell.clone(), ..Default::default() };
            let went = c.command(json!({ "cmd": "console", "line": format!("coc {cell}") }), Duration::from_secs(60), alive).and_then(|_| {
                // A cell that doesn't exist never loads; the console doesn't say so.
                c.until("game_loaded", timeout, alive).map_err(|_| anyhow::anyhow!("{cell} didn't load"))
            });
            match went {
                Ok(_) => {
                    r.timings.entry("game_loaded".into()).or_insert_with(|| secs(t0));
                    res.ok = true;
                    res.fps = fps(&c.sample(dwell, alive)?);
                    shot(c, shots, cell, alive, r);
                }
                Err(e) => res.error = Some(format!("{e:#}")),
            }
            r.cells.push(res);
        }
        for step in &s.steps {
            let res = run_step(c, step, timeout, shots, alive);
            r.screenshots.extend(res.screenshot.clone());
            r.steps.push(res);
            if !alive() {
                break;
            }
        }
        if !s.keep_open {
            let _ = c.command(json!({ "cmd": "quit" }), Duration::from_secs(10), alive);
            let end = Instant::now();
            while alive() && end.elapsed() < Duration::from_secs(30) {
                std::thread::sleep(Duration::from_millis(250));
            }
        }
        Ok(())
    };
    match step(&mut c, r) {
        Ok(()) => judge(r),
        Err(e) => r.summary = format!("{e:#}"),
    }
}

/// Close the game: the process running this exact exe, not any program with its name.
fn close(exe: &Path) {
    #[cfg(windows)]
    {
        // Windows reports the path with backslashes; game_path may have forward ones.
        let path = exe.display().to_string().replace('/', "\\").replace('\'', "''");
        let script = format!("Get-Process | Where-Object {{ $_.Path -eq '{path}' }} | Stop-Process -Force");
        let _ = std::process::Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", &script]).output();
    }
    #[cfg(not(windows))]
    {
        // Under Proton the game is a Wine process whose command line names the exe.
        let name = exe.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let _ = std::process::Command::new("pkill").args(["-f", &name]).output();
    }
}

/// The listening side, on a free port with a one-time token.
pub struct Server {
    listener: TcpListener,
    pub port: u16,
    pub token: String,
}

impl Server {
    pub fn start() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        // Not a secret against the user, only against other programs guessing the port.
        let token = {
            use std::hash::{BuildHasher, Hasher};
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u128(SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos());
            h.write_u32(std::process::id());
            format!("{:016x}", h.finish())
        };
        Ok(Server { listener, port, token })
    }

    /// Wait for the plugin's hello with the right token. Gives up when the game has gone.
    pub fn accept(&self, timeout: Duration, alive: &dyn Fn() -> bool) -> Result<(Conn, Option<String>)> {
        let start = Instant::now();
        let mut seen = false;
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false)?;
                    let mut c = Conn::new(stream)?;
                    let Some(hello) = c.read(Duration::from_secs(10))? else { continue };
                    if hello["ev"] == "hello" && hello["token"] == self.token.as_str() {
                        return Ok((c, hello["runtime"].as_str().map(str::to_owned)));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
            let up = alive();
            seen |= up;
            if seen && !up {
                bail!("the game closed before the harness connected");
            }
            if start.elapsed() > timeout {
                bail!("the harness didn't connect within {} seconds", timeout.as_secs());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

pub struct Conn {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    next: u64,
    /// FPS from heartbeats seen while waiting for something else.
    fps: Vec<f64>,
    /// Events that came while waiting for the last command's result: a coc's load finishes, and
    /// is reported, before the console line returns.
    early: Vec<Value>,
    /// The plugin's last notes (menus opening, data loading), for a wait that gives up.
    notes: Vec<String>,
    /// The player's cell in the last heartbeat.
    cell: Option<String>,
}

impl Conn {
    fn new(stream: TcpStream) -> Result<Self> {
        Ok(Conn { writer: stream.try_clone()?, reader: BufReader::new(stream), next: 1, fps: vec![], early: vec![], notes: vec![], cell: None })
    }

    /// The next event, or None when nothing came within `wait`.
    fn read(&mut self, wait: Duration) -> Result<Option<Value>> {
        self.reader.get_ref().set_read_timeout(Some(wait.max(Duration::from_millis(1))))?;
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => bail!("the game closed the harness connection"),
            Ok(_) => {
                let v: Value = serde_json::from_str(line.trim()).with_context(|| format!("bad harness message: {line}"))?;
                if v["ev"] == "heartbeat"
                    && let Some(f) = v["fps"].as_f64()
                {
                    self.fps.push(f);
                }
                if v["ev"] == "heartbeat" {
                    self.cell = v["cell"].as_str().filter(|c| !c.is_empty()).map(str::to_owned).or(self.cell.take());
                }
                if v["ev"] == "note" {
                    if self.notes.len() == 12 {
                        self.notes.remove(0);
                    }
                    self.notes.push(v["what"].as_str().unwrap_or_default().to_owned());
                }
                Ok(Some(v))
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Wait for an event, failing on timeout or when the game goes away.
    fn until(&mut self, ev: &str, timeout: Duration, alive: &dyn Fn() -> bool) -> Result<Value> {
        if let Some(i) = self.early.iter().position(|v| v["ev"] == ev) {
            return Ok(self.early.remove(i));
        }
        let start = Instant::now();
        loop {
            if let Some(v) = self.read(Duration::from_millis(500))?
                && v["ev"] == ev
            {
                return Ok(v);
            }
            let last = if self.notes.is_empty() { String::new() } else { format!(" (the game's last steps: {})", self.notes.join(", ")) };
            if !alive() {
                bail!("the game closed while waiting for {ev}{last}");
            }
            if start.elapsed() > timeout {
                bail!("no {ev} within {} seconds{last}", timeout.as_secs());
            }
        }
    }

    /// Send a command and wait for its result; a result with ok=false is an error.
    fn command(&mut self, mut cmd: Value, timeout: Duration, alive: &dyn Fn() -> bool) -> Result<Value> {
        let id = self.next;
        self.next += 1;
        cmd["id"] = json!(id);
        self.early.clear();
        writeln!(self.writer, "{cmd}")?;
        let start = Instant::now();
        loop {
            if let Some(v) = self.read(Duration::from_millis(500))? {
                if v["ev"] != "result" || v["id"] != id {
                    if v["ev"] != "heartbeat" {
                        self.early.push(v);
                    }
                    continue;
                }
                if v["ok"] == true {
                    return Ok(v);
                }
                bail!("{}", v["out"].as_str().unwrap_or("the command failed"));
            }
            if !alive() {
                bail!("the game closed during {}", cmd["cmd"]);
            }
            if start.elapsed() > timeout {
                bail!("no answer to {} within {} seconds", cmd["cmd"], timeout.as_secs());
            }
        }
    }

    /// FPS from the heartbeats over the next `dwell`.
    fn sample(&mut self, dwell: Duration, alive: &dyn Fn() -> bool) -> Result<Vec<f64>> {
        self.fps.clear();
        let start = Instant::now();
        while start.elapsed() < dwell {
            self.read(Duration::from_millis(200))?;
            if !alive() {
                bail!("the game closed");
            }
        }
        Ok(std::mem::take(&mut self.fps))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for the C++ plugin: answers the protocol from a thread.
    fn fake_plugin(port: u16, token: String, fail_cell: &'static str) -> std::thread::JoinHandle<Vec<String>> {
        std::thread::spawn(move || {
            // Another program guessing the port is turned away.
            let mut stranger = TcpStream::connect(("127.0.0.1", port)).unwrap();
            writeln!(stranger, "{}", json!({"ev": "hello", "token": "wrong"})).unwrap();
            let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let mut rd = BufReader::new(s.try_clone().unwrap());
            let mut send = |v: Value| writeln!(s, "{v}").unwrap();
            send(json!({"ev": "hello", "token": token, "runtime": "1.6.1170"}));
            send(json!({"ev": "data_loaded"}));
            send(json!({"ev": "main_menu"}));
            let mut seen = vec![];
            loop {
                let mut line = String::new();
                if rd.read_line(&mut line).unwrap() == 0 {
                    return seen;
                }
                let cmd: Value = serde_json::from_str(&line).unwrap();
                let id = cmd["id"].clone();
                let what = cmd["cmd"].as_str().unwrap().to_string();
                seen.push(format!("{what} {}", cmd["line"].as_str().or(cmd["name"].as_str()).unwrap_or("")).trim().to_string());
                match what.as_str() {
                    "load_save" => {
                        send(json!({"ev": "result", "id": id, "ok": true}));
                        send(json!({"ev": "game_loaded", "save": cmd["name"]}));
                    }
                    "console" => {
                        // Like the game: the console takes any line, but a bad cell never loads,
                        // and a good one has loaded before the line returns.
                        if !cmd["line"].as_str().unwrap().ends_with(fail_cell) {
                            send(json!({"ev": "game_loaded", "save": ""}));
                        }
                        send(json!({"ev": "result", "id": id, "ok": true, "out": ""}));
                    }
                    "screenshot" => send(json!({"ev": "result", "id": id, "ok": true})),
                    "quit" => {
                        send(json!({"ev": "result", "id": id, "ok": true}));
                        return seen;
                    }
                    _ => send(json!({"ev": "result", "id": id, "ok": false})),
                }
                for f in [60.0, 45.5] {
                    send(json!({"ev": "heartbeat", "fps": f, "cell": "x"}));
                }
            }
        })
    }

    #[test]
    fn drives_a_scenario_over_the_protocol() {
        let server = Server::start().unwrap();
        let plugin = fake_plugin(server.port, server.token.clone(), "Nowhere");
        let alive = || !plugin.is_finished(); // the game quits when the plugin is told to
        let (conn, runtime) = server.accept(Duration::from_secs(10), &alive).unwrap();
        assert_eq!(runtime.as_deref(), Some("1.6.1170"));
        let steps = vec![Step::Console("tgm".into()), Step::Wait(1), Step::Screenshot("after tgm".into())];
        let s = Scenario { cells: vec!["Riverwood".into(), "Nowhere".into()], dwell: Some(1), timeout: Some(2), steps, ..Default::default() };
        let mut r = TestRun::default();
        drive(conn, &s, Instant::now(), Some(Path::new("shots")), &alive, &mut r);
        assert_eq!(plugin.join().unwrap(), ["console coc Riverwood", "screenshot", "console coc Nowhere", "console tgm", "screenshot", "quit"]);
        assert_eq!(r.screenshots, [Path::new("shots").join("Riverwood.png"), Path::new("shots").join("after_tgm.png")]);
        assert!(!r.passed);
        assert_eq!(r.summary, "couldn't go to Nowhere");
        assert!(r.steps.iter().all(|s| s.ok && s.cell.as_deref() == Some("x")), "{:?}", r.steps);
        let json = serde_json::to_value(&s.steps).unwrap();
        assert_eq!(json, serde_json::json!([{"console": "tgm"}, {"wait": 1}, {"screenshot": "after tgm"}]), "steps read as the docs say");
        assert_eq!(r.cells[0].fps, Some(Fps { avg: 52.8, min: 45.5, samples: 2 }));
        assert_eq!(r.cells[1].error.as_deref(), Some("Nowhere didn't load"));
        assert!(["data_loaded", "main_menu", "game_loaded"].iter().all(|k| r.timings.contains_key(*k)), "{:?}", r.timings);
    }

    #[test]
    fn display_settings_for_a_window() {
        let set = |ini: &[u8], s: &[(&str, &str, &str)]| String::from_utf8(crate::health::ini_set(ini, &s.iter().map(|(a, b, c)| (*a, *b, Some(c.to_string()))).collect::<Vec<_>>())).unwrap();
        let ini = b"[General]\r\nsLanguage=ENGLISH\r\n[Display]\r\nbBorderless=1\r\nbFull Screen=1\r\niSize H=1080\r\niSize W=1920\r\nfGamma=1.0\r\n";
        let display = [("Display", "bFull Screen", "0"), ("Display", "bBorderless", "0"), ("Display", "iSize W", "1280"), ("Display", "iSize H", "720")];
        assert_eq!(set(ini, &display), "[General]\r\nsLanguage=ENGLISH\r\n[Display]\r\nbFull Screen=0\r\nbBorderless=0\r\niSize W=1280\r\niSize H=720\r\nfGamma=1.0\r\n");
        let active = [("General", "bAlwaysActive", "1")];
        assert_eq!(set(b"[General]\nbAlwaysActive=0\nx=1\n", &active), "[General]\nbAlwaysActive=1\nx=1\n");
        assert_eq!(set(b"[Display]\nx=1", &active), "[Display]\nx=1\n[General]\nbAlwaysActive=1\n");
        let keep = Scenario { keep_open: true, ..Default::default() };
        assert_eq!((window_size(&Scenario::default()).unwrap(), window_size(&keep).unwrap()), (Some((960, 540)), None));
    }

    /// A wait that gives up names the plugin's last notes.
    #[test]
    fn a_timeout_names_the_last_steps() {
        let server = Server::start().unwrap();
        let (port, token) = (server.port, server.token.clone());
        let plugin = std::thread::spawn(move || {
            let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            for v in [json!({"ev": "hello", "token": token, "runtime": "1.10.163"}), json!({"ev": "note", "what": "data loading"}), json!({"ev": "note", "what": "MainMenu opened"})] {
                writeln!(s, "{v}").unwrap();
            }
            std::thread::sleep(Duration::from_secs(3));
        });
        let (mut c, _) = server.accept(Duration::from_secs(5), &|| true).unwrap();
        let err = c.until("main_menu", Duration::from_secs(1), &|| true).unwrap_err().to_string();
        assert_eq!(err, "no main_menu within 1 seconds (the game's last steps: data loading, MainMenu opened)");
        plugin.join().unwrap();
    }

    #[test]
    fn gives_up_when_the_game_is_gone() {
        let server = Server::start().unwrap();
        let err = server.accept(Duration::from_secs(1), &|| false).err().unwrap().to_string();
        assert!(err.contains("didn't connect") || err.contains("closed"), "{err}");
    }
}
