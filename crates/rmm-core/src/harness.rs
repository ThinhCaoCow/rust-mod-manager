//! `test_run`: deploy, start the game through the script extender, and see whether it holds up.
//!
//! With rmm's harness plugin deployed (`rmm_harness.dll` in SKSE/F4SE Plugins), the game connects
//! back over TCP on 127.0.0.1 (which works through Proton, unlike named pipes) and the run is
//! scripted: wait for the main menu, load a save, `coc` through cells (the first one can start the
//! game from the main menu) while sampling FPS, then quit. The port and a one-time token reach the plugin as the environment
//! variables `RMM_HARNESS_PORT` and `RMM_HARNESS_TOKEN`, which the loader passes on to the game.
//! The protocol is JSON lines:
//!   plugin -> rmm  {"ev":"hello","token":"…","runtime":"1.6.1170","console_output":true}  {"ev":"data_loaded"}
//!                  {"ev":"main_menu"}  {"ev":"game_loaded","save":"…"} (a loading screen closed in the world)  {"ev":"heartbeat","fps":58.2,"cell":"…"}
//!                  {"ev":"result","id":N,"ok":true,"out":"…"} (what a console line printed, when the hello says console_output; else why it failed)
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
    /// What to do after the save and the cells, in order: a test plan of steps (see Step), such as
    /// {"cell": "Riverwood"}, {"console": "player.placeatme 00023ABA 2"}, {"wait": 10},
    /// {"screenshot": "name"}, {"load": "save name"}, {"state": {}}, {"move_to": "<ref>"},
    /// {"activate": "<ref>"}, {"wait_for": {"dead": "<ref>", "timeout": 60}}, {"call": {...}},
    /// {"dialogue": {}}, {"choose": "words"}, {"button": 1}, {"attack": {"target": "<ref>"}},
    /// {"press": {"control": "Jump"}}.
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
    /// player.additem, player.addspell, tgm, tai, fw, set gamehour to 22... What it prints comes
    /// back as the step's output, so queries work too: getstage <quest>, player.getav health,
    /// player.getpos x, sqv <quest>, help <word> (with a harness plugin built with console output;
    /// an older one gives none).
    Console(String),
    /// Let the game run this many seconds (at most 600), sampling FPS.
    Wait(u64),
    /// A PNG of the game's window, named after this.
    Screenshot(String),
    /// Load a save by name (without .ess/.fos) and wait for it.
    Load(String),
    /// The game's state as data: the player (cell, position, heading, health and
    /// magicka/stamina or action points as [now, max], level, combat), open menus, the nearest
    /// actors (ref, name, distance, dead, hostile, health) and objects (doors, containers, items:
    /// ref, type, name, distance; Fallout 4 lists the player's cell only), plus the quests, items
    /// (counts the player has; Skyrim) and refs asked for. Every step also carries the game's
    /// events since the last one: deaths, the player's activations, combat and quest stages
    /// (Skyrim), hits on or by the player (Fallout 4).
    State(StateQuery),
    /// Wait until every condition given holds (polling the state), or fail after `timeout`
    /// seconds (default 60).
    WaitFor(WaitFor),
    /// Call a Papyrus function: {"class": "Actor", "fn": "StartCombat", "self": "<ref>", "args":
    /// ["player"]}. `self` for methods (a ref, a form, or "player"), none for global functions;
    /// `args` in the function's order (forms by id or editor id). The step's data is its return
    /// value. A wrong count or type answers with the function's signature.
    Call(Call),
    /// Walk the player straight to a reference (the game's auto-move, turning toward it) until
    /// 100 units away, up to 120 seconds. When something is in the way the player is moved there
    /// instead, and the data says so (moved_from, note). No pathfinding: the game's own
    /// (Actor.PathToReference) doesn't move the player.
    MoveTo(String),
    /// Activate a reference as the player does: open a door, a container, pick up an item, start
    /// talking to an NPC.
    Activate(String),
    /// The Dialogue Menu's topics: [{index, text, new}], who's speaking, and whether it's open.
    /// Skyrim only.
    Dialogue {},
    /// Choose a dialogue topic by its index or by words in its text, as a click does, and wait
    /// for the answer: the data has what the speaker said (when the game shows dialogue
    /// subtitles) and the topics after it ("then"). Skyrim only.
    Choose(Choice),
    /// Press a message box's button by its index (0 first). Skyrim only.
    Button(u32),
    /// Call a menu's game callback, as its buttons do: {"menu": "LevelUp Menu", "callback":
    /// "addHealth", "args": []}; a callback starting with _root calls that ActionScript function
    /// itself ("_root.DialogueMenu_mc.onCancelPress" leaves a conversation). Skyrim only.
    MenuCall(MenuCall),
    /// Read a menu's ActionScript variable: {"menu": "Dialogue Menu", "path":
    /// "_root.DialogueMenu_mc.eMenuState"}; an object lists its members. Skyrim only.
    MenuGet(MenuGet),
    /// The player attacks (facing the target first, drawing the weapon if needed). kind: attack
    /// (default), power_attack, left_attack, left_power_attack, dual_attack, dual_power_attack,
    /// release (lets a drawn bow fire), left_release. Skyrim only.
    Attack(Attack),
    /// Turn the player to face a reference.
    Face(String),
    /// Hold a control for `ms` milliseconds (default 100) as the Controls menu names it: Forward,
    /// Back, Strafe Left, Strafe Right, Activate, Jump, Sprint, Sneak, Ready Weapon, Right
    /// Attack/Block, Left Attack/Block, Tween Menu, Journal, Wait. Works behind other windows,
    /// but held movement is slow and uneven (the key reaches the game on some frames only):
    /// walk with move_to. Skyrim only.
    Press(Press),
    /// Click a point of the game's window, in a screenshot's pixels: {"x": 640, "y": 360}, with
    /// "button": "right", "double": true, or "wheel": -3 (scroll there instead). It goes to the
    /// menu on top that shows a cursor (inventory, map, dialogue, a message box), never to the
    /// real mouse; the result names the menu. Skyrim only.
    Click(Click),
    /// Press a key for `ms` milliseconds (default 100) through the game's input: a letter or
    /// digit, Enter, Escape, Tab, Space, Backspace, Up, Down, Left, Right, Shift, Ctrl, Alt,
    /// Delete, Home, End, PageUp, PageDown, F1-F12. The game takes it as what the key is bound to
    /// where the player is (Escape is Cancel in a menu, E is Activate in the world); the result
    /// names that. Skyrim only.
    Key(KeyPress),
    /// Type text into the menu on top (a search box, a character's name). Skyrim only.
    Type(String),
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Default)]
pub struct StateQuery {
    /// Quest editor ids: their stage, running and completed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quests: Vec<String>,
    /// Items (form ids or editor ids): how many the player has.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
    /// References (ids): where they are, and for actors dead, combat, health.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<String>,
    /// How far to list actors and objects around the player (game units, default 3000; 0 for none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radius: Option<f64>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Default)]
pub struct WaitFor {
    /// This reference is dead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dead: Option<String>,
    /// This quest has reached at least this stage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<StageAt>,
    /// This menu is open ("Dialogue Menu", "MessageBoxMenu", "ContainerMenu", "LevelUp Menu"...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub menu: Option<String>,
    /// This menu is closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub menu_closed: Option<String>,
    /// The player is (true) or isn't (false) in combat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_combat: Option<bool>,
    /// The player is within `within` units of this reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub near: Option<Near>,
    /// The player is in this cell (editor id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell: Option<String>,
    /// Seconds before giving up (default 60, at most 600).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct StageAt {
    pub quest: String,
    pub at_least: u32,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct Near {
    #[serde(rename = "ref")]
    pub target: String,
    pub within: f64,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct Call {
    pub class: String,
    #[serde(rename = "fn")]
    pub function: String,
    #[serde(default, rename = "self", skip_serializing_if = "Option::is_none")]
    pub this: Option<String>,
    #[serde(default)]
    pub args: Vec<Value>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
#[serde(untagged)]
pub enum Choice {
    Index(u32),
    Text(String),
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct MenuCall {
    pub menu: String,
    pub callback: String,
    #[serde(default)]
    pub args: Vec<Value>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct MenuGet {
    pub menu: String,
    pub path: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Default)]
pub struct Attack {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Default)]
pub struct Click {
    pub x: f64,
    pub y: f64,
    /// left (default) or right.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub button: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub double: Option<bool>,
    /// Scroll the wheel this many notches here instead of clicking (negative: down).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wheel: Option<f64>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct KeyPress {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ms: Option<u64>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct Press {
    pub control: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ms: Option<u64>,
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
    /// What a console step printed ("" when it printed nothing).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// What the step returned: the state, a function's value, the dialogue topics...
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    /// What happened in the game during the step: deaths, combat starting and ending, quest stages,
    /// activations.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<Value>,
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
    /// Script extender plugins that didn't load in this run, from its log, with why and their mod.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub plugins_not_loaded: Vec<logs::SePlugin>,
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

/// Refuse to load a save the game doesn't have: Skyrim takes the name without a word and the
/// step would wait minutes for a load that never comes.
pub fn check_saves(inst: &Instance, steps: &[Step]) -> Result<()> {
    let Some(dir) = crate::profile_ini::save_dir(inst, &inst.profile()?).filter(|d| d.is_dir()) else { return Ok(()) };
    let ext = crate::saves::ext(inst.config.game);
    for step in steps {
        if let Step::Load(name) = step
            && !dir.join(format!("{name}.{ext}")).is_file()
            && !dir.join(name).is_file()
        {
            bail!("there's no save named '{name}' in {} (name it without .{ext})", dir.display());
        }
    }
    // A save that needs plugins the profile doesn't load makes the game ask before loading.
    let loads: Vec<&String> = steps.iter().filter_map(|s| if let Step::Load(n) = s { Some(n) } else { None }).collect();
    if !loads.is_empty() {
        for save in crate::saves::list(inst)?.saves {
            let stem = save.file.rsplit_once('.').map_or(save.file.as_str(), |(s, _)| s);
            if !save.missing.is_empty() && loads.iter().any(|n| n.eq_ignore_ascii_case(stem) || n.eq_ignore_ascii_case(&save.file)) {
                let names: Vec<&str> = save.missing.iter().take(8).map(|m| m.plugin.as_str()).collect();
                bail!("'{stem}' needs plugins this profile doesn't load ({}{}), so the game would ask before loading it; enable them or load another save", names.join(", "), if save.missing.len() > 8 { ", …" } else { "" });
            }
        }
    }
    Ok(())
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
        match server.accept(Duration::from_secs(s.timeout.unwrap_or_else(|| crate::settings::get().test.timeout)), &alive) {
            Ok((conn, runtime)) => {
                run.r.runtime = runtime;
                run.r.warnings.extend(conn.stale.clone());
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
        check_saves(inst, s.save.iter().map(|x| Step::Load(x.clone())).chain(s.steps.iter().cloned()).collect::<Vec<_>>().as_slice())?;
        let started = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs().saturating_sub(2);
        let before = logs::crash_logs(inst).into_iter().map(|l| l.path).collect();
        let mut r = TestRun::default();
        let prefs = match window_size(s)? {
            Some((w, h)) => windowed(inst, w, h).map_err(|e| r.warnings.push(format!("the game used its own display settings: {e:#}"))).ok(),
            None => None,
        };
        Ok(Run { r, t0: Instant::now(), started, before, prefs, game: inst.config.game_path.clone(), exe: inst.config.game.exe().into() })
    }

    fn alive(&self) -> impl Fn() -> GameState + Send + 'static {
        let (game, exe) = (self.game.clone(), self.exe.clone());
        // A box from the loader comes before the game runs, so it's looked for first.
        move || match message_box(&game) {
            Some(m) => GameState::Asking(m),
            None if deploy::game_running(&game, &exe) => GameState::Running,
            None => GameState::Closed,
        }
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
            prune_runs(&inst.root.join("test-runs"), crate::settings::get().test.keep_runs);
        }
        self.r.launch = Some(launch::launch_with(inst, false, &env)?);
        self.r.mode = "harness".into();
        Ok((server, shots))
    }

    /// Close the game (unless `keep_open`), read back a crash log and the Papyrus log, and put
    /// the INIs back.
    fn end(mut self, inst: &Instance, keep_open: bool) -> Result<TestRun> {
        let alive = self.alive();
        if !keep_open && alive() != GameState::Closed {
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
        let se = logs::script_extender_log(inst, false).ok().filter(|l| l.modified.is_some_and(|m| m >= self.started));
        r.plugins_not_loaded = se.map(|l| l.plugins).unwrap_or_default();
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
    alive: Box<dyn Fn() -> GameState + Send>,
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
        let timeout = Duration::from_secs(s.timeout.unwrap_or_else(|| crate::settings::get().test.timeout));
        let up = (|| -> Result<(Conn, std::path::PathBuf)> {
            let (server, shots) = run.launch_harness(inst, true)?;
            let (mut c, runtime) = server.accept(timeout, &alive)?;
            run.r.runtime = runtime;
            run.r.warnings.extend(c.stale.clone());
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
            if alive() != GameState::Running {
                break;
            }
        }
        out
    }

    /// Whether the game is still running, or stopped on a message box.
    pub fn state(&self) -> GameState {
        (self.alive)()
    }

    /// The run so far: timings, every step's result, screenshots, warnings.
    pub fn progress(&self) -> &TestRun {
        &self.run.as_ref().expect("a running session").r
    }

    /// Quit the game and report the whole session, crash log and Papyrus log included.
    pub fn stop(mut self, inst: &Instance) -> Result<TestRun> {
        if (self.alive)() == GameState::Running {
            let _ = self.conn.command(json!({ "cmd": "quit" }), Duration::from_secs(10), &self.alive);
            let end = Instant::now();
            while (self.alive)() == GameState::Running && end.elapsed() < Duration::from_secs(30) {
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
            if (self.alive)() != GameState::Closed {
                close(&run.game.join(&run.exe));
            }
            let end = Instant::now();
            while (self.alive)() != GameState::Closed && end.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    }
}

/// The window size a scenario asks for, or None for the game's own display settings.
/// Keep the newest `keep` test runs' screenshot folders (named by their start time); 0 keeps all.
fn prune_runs(dir: &std::path::Path, keep: usize) {
    if keep == 0 {
        return;
    }
    let mut runs: Vec<(u64, std::path::PathBuf)> = std::fs::read_dir(dir).into_iter().flatten().flatten().filter_map(|e| Some((e.file_name().to_str()?.parse().ok()?, e.path()))).collect();
    runs.sort();
    for (_, p) in runs.iter().take(runs.len().saturating_sub(keep)) {
        let _ = std::fs::remove_dir_all(p);
    }
}

fn window_size(s: &Scenario) -> Result<Option<(u32, u32)>> {
    let default = crate::settings::get().test.window;
    let w = s.window.as_deref().unwrap_or(&default);
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
fn screenshot(c: &mut Conn, dir: &Path, name: &str, alive: &dyn Fn() -> GameState) -> Result<std::path::PathBuf> {
    let file = dir.join(format!("{}.png", name.replace(|ch: char| !ch.is_ascii_alphanumeric() && ch != '-', "_")));
    let target = if cfg!(windows) { file.display().to_string() } else { crate::proton::windows_path(&file) };
    c.command(json!({ "cmd": "screenshot", "path": target }), Duration::from_secs(30), alive)?;
    Ok(file)
}

fn shot(c: &mut Conn, dir: Option<&Path>, name: &str, alive: &dyn Fn() -> GameState, r: &mut TestRun) {
    let Some(dir) = dir else { return };
    match screenshot(c, dir, name, alive) {
        Ok(file) => r.screenshots.push(file),
        Err(e) => r.warnings.push(format!("no screenshot of {name}: {e:#}")),
    }
}

/// Do one step; its failure is in the result.
fn run_step(c: &mut Conn, step: &Step, timeout: Duration, shots: Option<&Path>, alive: &dyn Fn() -> GameState) -> StepResult {
    let mut res = StepResult { step: step.clone(), ok: true, error: None, fps: None, cell: None, screenshot: None, output: None, data: None, events: vec![] };
    c.events.clear();
    let console = |c: &mut Conn, line: String| c.command(json!({ "cmd": "console", "line": line }), Duration::from_secs(60), alive);
    let done = match step {
        // A cell that doesn't exist never loads; the console doesn't say so.
        Step::Cell(cell) => console(c, format!("coc {cell}"))
            .and_then(|_| c.until("game_loaded", timeout, alive).map_err(|e| if alive() == GameState::Running { anyhow::anyhow!("{cell} didn't load") } else { e }))
            .map(drop),
        Step::Load(save) => c.command(json!({ "cmd": "load_save", "name": save }), Duration::from_secs(30), alive).and_then(|_| c.until("game_loaded", Duration::from_secs(300), alive)).map(drop),
        Step::Console(line) => console(c, line.clone()).map(|v| {
            if c.console_output {
                res.output = Some(v["out"].as_str().unwrap_or("").trim_end().to_owned());
            }
        }),
        Step::Wait(n) => c.sample(Duration::from_secs((*n).min(600)), alive).map(|f| res.fps = fps(&f)),
        Step::Screenshot(name) => match shots {
            Some(dir) => screenshot(c, dir, name, alive).map(|f| res.screenshot = Some(f)),
            None => Err(anyhow::anyhow!("screenshots are off for this run")),
        },
        Step::State(q) => {
            let mut cmd = serde_json::to_value(q).unwrap_or_default();
            cmd["cmd"] = json!("state");
            c.command(cmd, Duration::from_secs(30), alive).map(|v| res.data = data(v))
        }
        Step::WaitFor(w) => wait_for(c, w, alive).map(|v| res.data = Some(v)),
        Step::Call(call) => papyrus(c, call, Duration::from_secs(120), alive).map(|v| res.data = data(v)),
        Step::MoveTo(target) => move_to(c, target, alive).map(|v| res.data = Some(v)),
        Step::Activate(target) => c.command(json!({ "cmd": "activate", "target": target }), Duration::from_secs(30), alive).map(|v| res.data = data(v)),
        Step::Dialogue {} => c.command(json!({ "cmd": "dialogue" }), Duration::from_secs(30), alive).map(|v| res.data = data(v)),
        Step::Choose(choice) => {
            let cmd = match choice {
                Choice::Index(i) => json!({ "cmd": "choose", "index": i }),
                Choice::Text(t) => json!({ "cmd": "choose", "text": t }),
            };
            c.command(cmd, Duration::from_secs(75), alive).map(|v| res.data = data(v))
        }
        Step::Button(n) => c.command(json!({ "cmd": "menu_call", "menu": "MessageBoxMenu", "callback": "buttonPress", "args": [n] }), Duration::from_secs(30), alive).map(drop),
        Step::MenuCall(m) => c.command(json!({ "cmd": "menu_call", "menu": m.menu, "callback": m.callback, "args": m.args }), Duration::from_secs(30), alive).map(drop),
        Step::MenuGet(m) => c.command(json!({ "cmd": "menu_get", "menu": m.menu, "path": m.path }), Duration::from_secs(30), alive).map(|v| res.data = data(v)),
        Step::Attack(a) => c.command(json!({ "cmd": "act", "action": a.kind.as_deref().unwrap_or("attack"), "target": a.target }), Duration::from_secs(30), alive).map(drop),
        Step::Face(target) => c.command(json!({ "cmd": "face", "target": target }), Duration::from_secs(30), alive).map(drop),
        Step::Press(p) => {
            let ms = p.ms.unwrap_or(100).min(30_000);
            c.command(json!({ "cmd": "press", "control": p.control, "ms": ms }), Duration::from_millis(ms + 30_000), alive).map(|v| res.data = data(v))
        }
        Step::Click(k) => match k.button.as_deref() {
            None | Some("left" | "right") => {
                let button = u8::from(k.button.as_deref() == Some("right"));
                let mut cmd = json!({ "cmd": "click", "x": k.x, "y": k.y, "button": button, "double": k.double.unwrap_or(false) });
                if let Some(w) = k.wheel {
                    cmd["wheel"] = json!(w);
                }
                c.command(cmd, Duration::from_secs(30), alive).map(|v| res.data = data(v))
            }
            Some(b) => Err(anyhow::anyhow!("no mouse button '{b}' (left or right)")),
        },
        Step::Key(k) => {
            let ms = k.ms.unwrap_or(100).min(30_000);
            c.command(json!({ "cmd": "key", "key": k.key, "ms": ms }), Duration::from_millis(ms + 30_000), alive).map(|v| res.data = data(v))
        }
        Step::Type(text) => c.command(json!({ "cmd": "type", "text": text }), Duration::from_secs(60), alive).map(|v| res.data = data(v)),
    };
    res.events = std::mem::take(&mut c.events);
    if let Err(e) = done {
        res.ok = false;
        res.error = Some(format!("{e:#}"));
    }
    res.cell = c.cell.clone();
    res
}

fn data(v: Value) -> Option<Value> {
    v.get("data").cloned()
}

fn papyrus(c: &mut Conn, call: &Call, timeout: Duration, alive: &dyn Fn() -> GameState) -> Result<Value> {
    c.command(json!({ "cmd": "papyrus", "class": call.class, "fn": call.function, "self": call.this, "args": call.args }), timeout, alive)
}

/// The plugin walks the player straight at the target (Forward held, turning toward it).
fn move_to(c: &mut Conn, target: &str, alive: &dyn Fn() -> GameState) -> Result<Value> {
    Ok(c.command(json!({ "cmd": "walk", "target": target, "within": 100, "timeout": 120 }), Duration::from_secs(150), alive)?["data"].take())
}

/// Poll the state until every condition holds; the last state is the result.
fn wait_for(c: &mut Conn, w: &WaitFor, alive: &dyn Fn() -> GameState) -> Result<Value> {
    let refs: Vec<&String> = w.dead.iter().chain(w.near.as_ref().map(|n| &n.target)).collect();
    let quests: Vec<&String> = w.stage.as_ref().map(|s| &s.quest).into_iter().collect();
    let query = json!({ "cmd": "state", "radius": 0, "refs": refs, "quests": quests });
    let timeout = Duration::from_secs(w.timeout.unwrap_or(60).min(600));
    let start = Instant::now();
    loop {
        let state = match c.command(query.clone(), Duration::from_secs(30), alive) {
            Ok(mut v) => v["data"].take(),
            // The game gone ends the wait; anything else (a loading screen, a slow frame) is a
            // condition not met yet.
            Err(e) if alive() != GameState::Running => return Err(e),
            Err(_) if start.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
            Err(e) => return Err(e.context(format!("the state wasn't readable within {} seconds", timeout.as_secs()))),
        };
        let open = |m: &str| state["menus"].as_array().is_some_and(|ms| ms.iter().any(|x| x.as_str().is_some_and(|x| x.eq_ignore_ascii_case(m))));
        let mut unmet = vec![];
        if let Some(r) = &w.dead
            && state["refs"][r]["dead"] != true
        {
            unmet.push(format!("{r} isn't dead"));
        }
        if let Some(s) = &w.stage
            && state["quests"][&s.quest]["stage"].as_u64().is_none_or(|n| n < u64::from(s.at_least))
        {
            unmet.push(format!("{} is at stage {}", s.quest, state["quests"][&s.quest]["stage"]));
        }
        if let Some(m) = &w.menu
            && !open(m)
        {
            unmet.push(format!("{m} isn't open"));
        }
        if let Some(m) = &w.menu_closed
            && open(m)
        {
            unmet.push(format!("{m} is open"));
        }
        if let Some(b) = w.in_combat
            && state["player"]["in_combat"] != b
        {
            unmet.push(if b { "the player isn't in combat".into() } else { "the player is in combat".into() });
        }
        if let Some(n) = &w.near
            && state["refs"][&n.target]["distance"].as_f64().is_none_or(|d| d > n.within)
        {
            unmet.push(format!("{} is {} away", n.target, state["refs"][&n.target]["distance"]));
        }
        if let Some(cell) = &w.cell
            && !state["player"]["cell"].as_str().is_some_and(|x| x.eq_ignore_ascii_case(cell))
        {
            unmet.push(format!("the player is in {}", state["player"]["cell"]));
        }
        if unmet.is_empty() {
            return Ok(state);
        }
        if start.elapsed() > timeout {
            bail!("not within {} seconds: {}", timeout.as_secs(), unmet.join("; "));
        }
        let pause = Instant::now();
        while pause.elapsed() < Duration::from_millis(500) {
            c.read(Duration::from_millis(100))?;
        }
    }
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
fn watch(s: &Scenario, t0: Instant, alive: &dyn Fn() -> GameState, r: &mut TestRun) {
    let limit = Duration::from_secs(s.timeout.unwrap_or(120));
    let mut seen = false;
    while t0.elapsed() < limit {
        let state = alive();
        if let GameState::Asking(m) = state {
            r.summary = format!("the game stopped on a message box: {m}");
            return;
        }
        let up = state == GameState::Running;
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
fn drive(mut c: Conn, s: &Scenario, t0: Instant, shots: Option<&Path>, alive: &dyn Fn() -> GameState, r: &mut TestRun) {
    let dwell = Duration::from_secs(s.dwell.unwrap_or_else(|| crate::settings::get().test.dwell));
    let step = |c: &mut Conn, r: &mut TestRun| -> Result<()> {
        let timeout = Duration::from_secs(s.timeout.unwrap_or_else(|| crate::settings::get().test.timeout));
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
            if alive() != GameState::Running {
                break;
            }
        }
        if !s.keep_open && alive() == GameState::Running {
            let _ = c.command(json!({ "cmd": "quit" }), Duration::from_secs(10), alive);
            let end = Instant::now();
            while alive() == GameState::Running && end.elapsed() < Duration::from_secs(30) {
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
        let _ = crate::hidden("powershell").args(["-NoProfile", "-NonInteractive", "-Command", &script]).output();
    }
    #[cfg(not(windows))]
    {
        // Under Proton the game is a Wine process whose command line names the exe.
        let name = exe.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let _ = std::process::Command::new("pkill").args(["-f", &name]).output();
    }
}

/// What the game is doing, as the waits check it a few times a second.
#[derive(Debug, Clone, PartialEq)]
pub enum GameState {
    Running,
    Closed,
    /// A Windows message box from the game or its loader holds it up (a plugin that didn't load,
    /// a missing file): "title: text [buttons]".
    Asking(String),
}

/// Ends a wait when the game closed (`closed` says during what) or stopped on a message box.
fn going(alive: &dyn Fn() -> GameState, closed: impl FnOnce() -> String) -> Result<()> {
    match alive() {
        GameState::Running => Ok(()),
        GameState::Closed => bail!("{}", closed()),
        GameState::Asking(m) => bail!("the game stopped on a message box: {m}"),
    }
}

/// A Windows message box open in a program from the game's folder (the game, or its script
/// extender's loader), as "title: text [buttons]". Read from the window itself, so the game can
/// be behind other windows; nothing is clicked.
#[cfg(windows)]
pub fn message_box(folder: &Path) -> Option<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, HWND, LPARAM};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
    use windows_sys::Win32::UI::WindowsAndMessaging::{EnumChildWindows, EnumWindows, GetClassNameW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible};
    use windows_sys::core::BOOL;
    fn text(get: unsafe extern "system" fn(HWND, *mut u16, i32) -> i32, h: HWND) -> String {
        let mut buf = [0u16; 2048];
        let n = unsafe { get(h, buf.as_mut_ptr(), buf.len() as i32) };
        String::from_utf16_lossy(&buf[..n.max(0) as usize]).split_whitespace().collect::<Vec<_>>().join(" ")
    }
    unsafe extern "system" fn dialog(h: HWND, found: LPARAM) -> BOOL {
        let found = unsafe { &mut *(found as *mut Vec<HWND>) };
        if unsafe { IsWindowVisible(h) } != 0 && text(GetClassNameW, h) == "#32770" {
            found.push(h);
        }
        1
    }
    unsafe extern "system" fn part(h: HWND, parts: LPARAM) -> BOOL {
        let (lines, buttons) = unsafe { &mut *(parts as *mut (Vec<String>, Vec<String>)) };
        let t = text(GetWindowTextW, h);
        if !t.is_empty() {
            match text(GetClassNameW, h).as_str() {
                "Static" => lines.push(t),
                "Button" => buttons.push(t.replace('&', "")),
                _ => {}
            }
        }
        1
    }
    let mut found: Vec<HWND> = Vec::new();
    unsafe { EnumWindows(Some(dialog), &mut found as *mut _ as LPARAM) };
    let folder = folder.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase();
    found.into_iter().find_map(|h| {
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(h, &mut pid) };
        let p = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if p.is_null() {
            return None;
        }
        let (mut buf, mut n) = ([0u16; 1024], 1024u32);
        let ok = unsafe { QueryFullProcessImageNameW(p, 0, buf.as_mut_ptr(), &mut n) } != 0;
        unsafe { CloseHandle(p) };
        let exe = String::from_utf16_lossy(&buf[..n as usize]).to_lowercase();
        if !ok || exe.rsplit_once('\\').map(|(dir, _)| dir) != Some(folder.as_str()) {
            return None;
        }
        let mut parts = (Vec::<String>::new(), Vec::<String>::new());
        unsafe { EnumChildWindows(h, Some(part), &mut parts as *mut _ as LPARAM) };
        let buttons = if parts.1.is_empty() { String::new() } else { format!(" [{}]", parts.1.join(" / ")) };
        Some(format!("{}: {}{buttons}", text(GetWindowTextW, h), parts.0.join(" ")))
    })
}

#[cfg(not(windows))]
pub fn message_box(_folder: &Path) -> Option<String> {
    None
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
    pub fn accept(&self, timeout: Duration, alive: &dyn Fn() -> GameState) -> Result<(Conn, Option<String>)> {
        let start = Instant::now();
        let mut seen = false;
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false)?;
                    let mut c = Conn::new(stream)?;
                    let Ok(Some(hello)) = c.read(Duration::from_secs(10)) else { continue };
                    if hello["ev"] == "hello" && hello["token"] == self.token.as_str() {
                        c.console_output = hello["console_output"] == true;
                        c.stale = stale_plugin(hello["source"].as_str());
                        return Ok((c, hello["runtime"].as_str().map(str::to_owned)));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
            match alive() {
                GameState::Running => seen = true,
                GameState::Asking(m) => bail!("the game stopped on a message box before the harness connected: {m}"),
                GameState::Closed if seen => bail!("the game closed before the harness connected"),
                GameState::Closed => {}
            }
            if start.elapsed() > timeout {
                bail!("the harness didn't connect within {} seconds", timeout.as_secs());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Why the plugin in the game may not do what this rmm expects: it was built from other harness
/// source (or before plugins said theirs). build.rs hashes the source as the plugin's build does.
fn stale_plugin(source: Option<&str>) -> Option<String> {
    let ours = [env!("RMM_HARNESS_SKSE"), env!("RMM_HARNESS_F4SE")];
    if ours.iter().all(|s| s.is_empty()) || source.is_some_and(|s| ours.contains(&s)) {
        return None;
    }
    Some("the harness plugin in the game was built from other harness source than this rmm, so steps may be missing or behave differently: rebuild it (harness/README.md) and put the new DLL in the harness mod with `rmm update`".into())
}

pub struct Conn {
    /// Lines from the game, read as they come by a thread of rmm's: a socket left unread fills,
    /// and the plugin's next send would then hold up the game's main thread.
    lines: std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
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
    /// The plugin sends back what console lines print.
    console_output: bool,
    /// What happened in the game (deaths, combat, quest stages, activations) since the step began.
    events: Vec<Value>,
    /// Why the plugin may not match this rmm, from its hello.
    stale: Option<String>,
}

impl Drop for Conn {
    /// Ends the reading thread with the connection.
    fn drop(&mut self) {
        let _ = self.writer.shutdown(std::net::Shutdown::Both);
    }
}

impl Conn {
    fn new(stream: TcpStream) -> Result<Self> {
        let writer = stream.try_clone()?;
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stream);
            loop {
                let mut line = Vec::new();
                let got = reader.read_until(b'\n', &mut line);
                let end = !matches!(got, Ok(n) if n > 0);
                if tx.send(got.map(|_| line)).is_err() || end {
                    return;
                }
            }
        });
        Ok(Conn { writer, lines, next: 1, fps: vec![], early: vec![], notes: vec![], cell: None, console_output: false, events: vec![], stale: None })
    }

    /// The next event, or None when nothing came within `wait`.
    fn read(&mut self, wait: Duration) -> Result<Option<Value>> {
        use std::sync::mpsc::RecvTimeoutError;
        match self.lines.recv_timeout(wait) {
            Ok(Ok(line)) if line.is_empty() => bail!("the game closed the harness connection"),
            Err(RecvTimeoutError::Disconnected) => bail!("the game closed the harness connection"),
            Ok(Ok(line)) => {
                let line = String::from_utf8_lossy(&line).into_owned();
                let v: Value = serde_json::from_str(line.trim()).with_context(|| format!("bad harness message: {line}"))?;
                if v["ev"] == "heartbeat"
                    && let Some(f) = v["fps"].as_f64()
                {
                    self.fps.push(f);
                }
                if v["ev"] == "heartbeat" {
                    self.cell = v["cell"].as_str().filter(|c| !c.is_empty()).map(str::to_owned).or(self.cell.take());
                }
                if v["ev"] == "game" && self.events.len() < 200 {
                    let mut e = v.clone();
                    e.as_object_mut().map(|o| o.remove("ev"));
                    self.events.push(e);
                }
                if v["ev"] == "note" {
                    if self.notes.len() == 12 {
                        self.notes.remove(0);
                    }
                    self.notes.push(v["what"].as_str().unwrap_or_default().to_owned());
                }
                Ok(Some(v))
            }
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Ok(Err(e)) => Err(e.into()),
        }
    }

    /// Wait for an event, failing on timeout or when the game goes away.
    fn until(&mut self, ev: &str, timeout: Duration, alive: &dyn Fn() -> GameState) -> Result<Value> {
        // A load the game stops to ask about (a save that needs missing content, a damaged one)
        // never finishes on its own.
        let asks = |v: &Value| ev == "game_loaded" && v["ev"] == "message_box";
        if self.early.iter().any(asks) {
            bail!("the game asked a question instead of loading (a save that needs plugins that aren't loaded asks first); answer it with a button step, or load another save");
        }
        if let Some(i) = self.early.iter().position(|v| v["ev"] == ev) {
            return Ok(self.early.remove(i));
        }
        let start = Instant::now();
        loop {
            if let Some(v) = self.read(Duration::from_millis(500))? {
                if v["ev"] == ev {
                    return Ok(v);
                }
                if asks(&v) {
                    bail!("the game asked a question instead of loading (a save that needs plugins that aren't loaded asks first); answer it with a button step, or load another save");
                }
            }
            let last = if self.notes.is_empty() { String::new() } else { format!(" (the game's last steps: {})", self.notes.join(", ")) };
            going(alive, || format!("the game closed while waiting for {ev}{last}"))?;
            if start.elapsed() > timeout {
                bail!("no {ev} within {} seconds{last}", timeout.as_secs());
            }
        }
    }

    /// Send a command and wait for its result; a result with ok=false is an error.
    fn command(&mut self, mut cmd: Value, timeout: Duration, alive: &dyn Fn() -> GameState) -> Result<Value> {
        let id = self.next;
        self.next += 1;
        cmd["id"] = json!(id);
        // What came since the last step (a load that finished after it) belongs to that step.
        while self.read(Duration::from_millis(1))?.is_some() {}
        self.early.clear();
        writeln!(self.writer, "{cmd}")?;
        let start = Instant::now();
        loop {
            if let Some(v) = self.read(Duration::from_millis(500))? {
                if v["ev"] == "result" && v["id"] == id {
                    if v["ok"] == true {
                        return Ok(v);
                    }
                    bail!("{}", v["out"].as_str().unwrap_or("the command failed"));
                }
                if v["ev"] != "heartbeat" {
                    self.early.push(v);
                }
            }
            going(alive, || format!("the game closed during {}", cmd["cmd"]))?;
            if start.elapsed() > timeout {
                bail!("no answer to {} within {} seconds", cmd["cmd"], timeout.as_secs());
            }
        }
    }

    /// FPS from the heartbeats over the next `dwell`.
    fn sample(&mut self, dwell: Duration, alive: &dyn Fn() -> GameState) -> Result<Vec<f64>> {
        self.fps.clear();
        let start = Instant::now();
        while start.elapsed() < dwell {
            self.read(Duration::from_millis(200))?;
            going(alive, || "the game closed".into())?;
        }
        Ok(std::mem::take(&mut self.fps))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_plugin_built_from_other_source_is_named() {
        assert_eq!(stale_plugin(Some(env!("RMM_HARNESS_SKSE"))), None);
        assert_eq!(stale_plugin(Some(env!("RMM_HARNESS_F4SE"))), None);
        assert!(stale_plugin(Some("0123")).is_some());
        assert!(stale_plugin(None).is_some(), "a plugin from before the hello said its source");
    }


    /// A stand-in for the C++ plugin: answers the protocol from a thread.
    fn fake_plugin(port: u16, token: String, fail_cell: &'static str) -> std::thread::JoinHandle<Vec<String>> {
        std::thread::spawn(move || {
            // Another program guessing the port is turned away.
            let mut stranger = TcpStream::connect(("127.0.0.1", port)).unwrap();
            writeln!(stranger, "{}", json!({"ev": "hello", "token": "wrong"})).unwrap();
            let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let mut rd = BufReader::new(s.try_clone().unwrap());
            let mut send = |v: Value| writeln!(s, "{v}").unwrap();
            send(json!({"ev": "hello", "token": token, "runtime": "1.6.1170", "console_output": true}));
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
                        let out = if cmd["line"] == "player.getav health" { "GetActorValue: Health >> 120.00\n" } else { "" };
                        send(json!({"ev": "result", "id": id, "ok": true, "out": out}));
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
    fn clicks_and_keys_reach_the_plugin() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let game = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let mut c = Conn::new(l.accept().unwrap().0).unwrap();
        let plugin = std::thread::spawn(move || {
            let mut rd = BufReader::new(game.try_clone().unwrap());
            let mut w = game;
            let mut seen = vec![];
            for _ in 0..3 {
                let mut line = String::new();
                rd.read_line(&mut line).unwrap();
                let cmd: Value = serde_json::from_str(&line).unwrap();
                let out = if cmd["cmd"] == "key" { json!({"event": "Cancel"}) } else { json!({"menu": "InventoryMenu"}) };
                writeln!(w, "{}", json!({"ev": "result", "id": cmd["id"], "ok": true, "out": "", "data": out})).unwrap();
                seen.push(cmd);
            }
            seen
        });
        let alive = || GameState::Running;
        let click = run_step(&mut c, &Step::Click(Click { x: 640.0, y: 360.0, double: Some(true), ..Default::default() }), Duration::from_secs(5), None, &alive);
        assert!(click.ok, "{click:?}");
        assert_eq!(click.data, Some(json!({"menu": "InventoryMenu"})));
        let scroll = run_step(&mut c, &Step::Click(Click { x: 10.0, y: 20.0, button: Some("right".into()), wheel: Some(-3.0), ..Default::default() }), Duration::from_secs(5), None, &alive);
        assert!(scroll.ok);
        let key = run_step(&mut c, &Step::Key(KeyPress { key: "Escape".into(), ms: None }), Duration::from_secs(5), None, &alive);
        assert_eq!(key.data, Some(json!({"event": "Cancel"})));
        let bad = run_step(&mut c, &Step::Click(Click { button: Some("middle".into()), ..Default::default() }), Duration::from_secs(5), None, &alive);
        assert!(!bad.ok && bad.error.as_deref().unwrap_or_default().contains("middle"), "{bad:?}");
        let seen = plugin.join().unwrap();
        assert_eq!(seen[0], json!({"cmd": "click", "x": 640.0, "y": 360.0, "button": 0, "double": true, "id": seen[0]["id"]}));
        assert_eq!((seen[1]["button"].clone(), seen[1]["wheel"].clone()), (json!(1), json!(-3.0)));
        assert_eq!((seen[2]["cmd"].as_str(), seen[2]["key"].as_str(), seen[2]["ms"].as_u64()), (Some("key"), Some("Escape"), Some(100)));
        let steps: Vec<Step> = serde_json::from_str(r#"[{"click": {"x": 1, "y": 2}}, {"key": {"key": "E"}}, {"type": "iron"}]"#).unwrap();
        assert_eq!(steps[2], Step::Type("iron".into()), "steps read as the docs write them");
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "shows a message box on the desktop for a moment"]
    fn reads_a_message_box_from_the_game_folder() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, IDNO, MB_YESNO, MessageBoxW, PostMessageW, WM_COMMAND};
        let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
        let (title, text) = (wide("rmm test box"), wide("'KiENBExtender.dll' - DLL dependency\nmissing: KiHalloc.dll"));
        let t = title.clone();
        let shown = std::thread::spawn(move || unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), t.as_ptr(), MB_YESNO) });
        // The test's own folder stands in for the game's.
        let here = std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
        let start = Instant::now();
        let found = loop {
            if let Some(m) = message_box(&here) {
                break m;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "the box wasn't found");
            std::thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(message_box(Path::new(r"C:\Windows")), None, "only programs from the given folder count");
        unsafe { PostMessageW(FindWindowW(std::ptr::null(), title.as_ptr()), WM_COMMAND, IDNO as usize, 0) };
        assert_eq!(shown.join().unwrap(), IDNO);
        assert_eq!(found, "rmm test box: 'KiENBExtender.dll' - DLL dependency missing: KiHalloc.dll [Yes / No]");
    }

    #[test]
    fn a_message_split_by_a_timeout_comes_whole() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut game = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let mut c = Conn::new(l.accept().unwrap().0).unwrap();
        game.write_all(br#"{"ev":"res"#).unwrap();
        assert!(c.read(Duration::from_millis(100)).unwrap().is_none(), "half a message: wait for the rest");
        game.write_all(b"ult\",\"id\":1}\n").unwrap();
        assert_eq!(c.read(Duration::from_secs(2)).unwrap().unwrap()["ev"], "result");

        // A load the game stops to ask about fails at once, not at the timeout.
        game.write_all(b"{\"ev\":\"message_box\"}\n").unwrap();
        let t = Instant::now();
        let err = c.until("game_loaded", Duration::from_secs(60), &|| GameState::Running).unwrap_err().to_string();
        assert!(err.contains("asked a question") && t.elapsed() < Duration::from_secs(5), "{err}");

        // Between steps (the AI thinking) nothing asks for messages, yet the game's sends go
        // through: a full socket would hold up its main thread.
        game.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
        let beat = format!("{}\n", serde_json::json!({"ev": "heartbeat", "fps": 60.0, "cell": "x".repeat(200)}));
        for _ in 0..40_000 {
            game.write_all(beat.as_bytes()).expect("the game's send blocked");
        }
        drop(game);
        let mut n = 0;
        while c.read(Duration::from_secs(5)).is_ok_and(|v| v.is_some()) {
            n += 1;
        }
        assert_eq!(n, 40_000);
    }

    #[test]
    fn drives_a_scenario_over_the_protocol() {
        let server = Server::start().unwrap();
        let plugin = fake_plugin(server.port, server.token.clone(), "Nowhere");
        let alive = || if plugin.is_finished() { GameState::Closed } else { GameState::Running }; // the game quits when the plugin is told to
        let (conn, runtime) = server.accept(Duration::from_secs(10), &alive).unwrap();
        assert_eq!(runtime.as_deref(), Some("1.6.1170"));
        let steps = vec![Step::Console("tgm".into()), Step::Wait(1), Step::Screenshot("after tgm".into()), Step::Console("player.getav health".into())];
        let s = Scenario { cells: vec!["Riverwood".into(), "Nowhere".into()], dwell: Some(1), timeout: Some(2), steps, ..Default::default() };
        let mut r = TestRun::default();
        drive(conn, &s, Instant::now(), Some(Path::new("shots")), &alive, &mut r);
        assert_eq!(plugin.join().unwrap(), ["console coc Riverwood", "screenshot", "console coc Nowhere", "console tgm", "screenshot", "console player.getav health", "quit"]);
        assert_eq!(r.screenshots, [Path::new("shots").join("Riverwood.png"), Path::new("shots").join("after_tgm.png")]);
        assert!(!r.passed);
        assert_eq!(r.summary, "couldn't go to Nowhere");
        assert!(r.steps.iter().all(|s| s.ok && s.cell.as_deref() == Some("x")), "{:?}", r.steps);
        let json = serde_json::to_value(&s.steps).unwrap();
        assert_eq!(json, serde_json::json!([{"console": "tgm"}, {"wait": 1}, {"screenshot": "after tgm"}, {"console": "player.getav health"}]), "steps read as the docs say");
        let out: Vec<Option<&str>> = r.steps.iter().map(|s| s.output.as_deref()).collect();
        assert_eq!(out, [Some(""), None, None, Some("GetActorValue: Health >> 120.00")], "console steps carry what they printed");
        assert_eq!(r.cells[0].fps, Some(Fps { avg: 52.8, min: 45.5, samples: 2 }));
        assert_eq!(r.cells[1].error.as_deref(), Some("Nowhere didn't load"));
        assert!(["data_loaded", "main_menu", "game_loaded"].iter().all(|k| r.timings.contains_key(*k)), "{:?}", r.timings);
    }

    /// The playing steps: walk, wait for a death the state
    /// reports on the second poll, activate, and the game's events attached to the step.
    #[test]
    fn plays_through_papyrus_state_and_events() {
        let server = Server::start().unwrap();
        let (port, token) = (server.port, server.token.clone());
        let plugin = std::thread::spawn(move || {
            let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let mut rd = BufReader::new(s.try_clone().unwrap());
            writeln!(s, "{}", json!({"ev": "hello", "token": token, "runtime": "1.6.1170", "console_output": true})).unwrap();
            let (mut calls, mut polls) = (vec![], 0);
            loop {
                let mut line = String::new();
                if rd.read_line(&mut line).unwrap() == 0 {
                    return calls;
                }
                let cmd: Value = serde_json::from_str(&line).unwrap();
                let id = cmd["id"].clone();
                let reply = match cmd["cmd"].as_str().unwrap() {
                    "papyrus" => {
                        calls.push(format!("{}.{} {}", cmd["class"].as_str().unwrap(), cmd["fn"].as_str().unwrap(), cmd["args"]));
                        json!({"value": true})
                    }
                    "activate" => {
                        calls.push(format!("activate {}", cmd["target"]));
                        json!({"activated": true})
                    }
                    "walk" => {
                        calls.push(format!("walk {}", cmd["target"]));
                        writeln!(s, "{}", json!({"ev": "game", "what": "activate", "target": {"ref": "door"}})).unwrap();
                        json!({"arrived": true, "distance": 120})
                    }
                    "state" => {
                        polls += 1;
                        json!({"player": {"cell": "Riverwood"}, "menus": [], "refs": {"wolf": {"dead": polls > 1}}})
                    }
                    "quit" => return calls,
                    _ => json!(null),
                };
                writeln!(s, "{}", json!({"ev": "result", "id": id, "ok": true, "out": "", "data": reply})).unwrap();
            }
        });
        let (mut c, _) = server.accept(Duration::from_secs(5), &|| GameState::Running).unwrap();
        let alive = || GameState::Running;
        let steps: Vec<Step> = serde_json::from_value(json!([{"move_to": "door"}, {"wait_for": {"dead": "wolf", "timeout": 5}}, {"activate": "chest"}, {"dialogue": {}}, {"choose": "Rumors"}, {"choose": 1}])).unwrap();
        let r: Vec<StepResult> = steps.iter().take(3).map(|st| run_step(&mut c, st, Duration::from_secs(5), None, &alive)).collect();
        assert!(r.iter().all(|x| x.ok), "{r:?}");
        assert_eq!(r[0].data, Some(json!({"arrived": true, "distance": 120})));
        assert_eq!(r[0].events, [json!({"what": "activate", "target": {"ref": "door"}})]);
        assert_eq!(r[1].data.as_ref().unwrap()["refs"]["wolf"]["dead"], true);
        assert_eq!(r[2].data, Some(json!({"activated": true})));
        assert_eq!((&steps[4], &steps[5]), (&Step::Choose(Choice::Text("Rumors".into())), &Step::Choose(Choice::Index(1))));
        c.command(json!({"cmd": "quit"}), Duration::from_secs(1), &alive).ok();
        assert_eq!(plugin.join().unwrap(), ["walk \"door\"", "activate \"chest\""]);
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
        let (mut c, _) = server.accept(Duration::from_secs(5), &|| GameState::Running).unwrap();
        let err = c.until("main_menu", Duration::from_secs(1), &|| GameState::Running).unwrap_err().to_string();
        assert_eq!(err, "no main_menu within 1 seconds (the game's last steps: data loading, MainMenu opened)");
        // A message box ends the wait at once, with what it says.
        let t = Instant::now();
        let err = c.until("main_menu", Duration::from_secs(60), &|| GameState::Asking("Plugin error: … [Yes / No]".into())).unwrap_err().to_string();
        assert_eq!(err, "the game stopped on a message box: Plugin error: … [Yes / No]");
        assert!(t.elapsed() < Duration::from_secs(2));
        plugin.join().unwrap();
    }

    #[test]
    fn gives_up_when_the_game_is_gone() {
        let server = Server::start().unwrap();
        let err = server.accept(Duration::from_secs(1), &|| GameState::Closed).err().unwrap().to_string();
        assert!(err.contains("didn't connect") || err.contains("closed"), "{err}");
        // A box from the loader or a plugin comes before the harness can connect.
        let t = Instant::now();
        let err = server.accept(Duration::from_secs(60), &|| GameState::Asking("Plugin error: … [Yes / No]".into())).err().unwrap().to_string();
        assert_eq!(err, "the game stopped on a message box before the harness connected: Plugin error: … [Yes / No]");
        assert!(t.elapsed() < Duration::from_secs(2));
    }
}
