//! MCP server: the same operations as the CLI, as tools an AI can call.
//! Every mutation lands in undo history; game-folder writes default to dry runs.
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
    transport::stdio,
};
use rmm_core::fomod::Choices;
use rmm_core::tools::ToolSpec;
use rmm_core::{Instance, addons, conflicts, deploy, harness, health, import, install, lockfile, logs, loot, nexus, ops, plugins, rules, tools};
use std::collections::BTreeMap;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const INSTRUCTIONS: &str = "\
rmm manages one Skyrim SE or Fallout 4 instance.
Model: mods are an ordered list; later (higher priority) mods win loose-file conflicts. \
Plugins (.esp/.esm/.esl) have their own load order; masters always load before non-masters.
Workflow: inspect (list_mods, list_conflicts, list_plugins, health_check) -> change (install_mod, \
set_mods_enabled, move_mods, set_mod_order, sort_plugins, set_plugin_order...) -> deploy with dry_run=true to \
preview, then dry_run=false to apply. Nothing reaches the game folder until deploy is applied.
FOMOD installers: fomod_options shows the steps, options and the resolved choices; edit those \
choices (calling fomod_options again shows steps they unlock) and pass them to install_mod.
Tools: add_tool registers a program (presets: xedit, xedit-qac, xlodgen, bodyslide, synthesis, pandora, pgpatcher, texgen, dyndolod); run_tool deploys, runs it, captures what it wrote (its {output} folder plus new or changed Data files) into a generated mod that replaces its previous output, and deploys again. Preview with dry_run=true first. Long runs (DynDOLOD, grass) can go in the background (run_tool background=true, then poll tool_job).
launch_game deploys and starts the game through its script extender after checking the versions match. \
test_run starts it and checks it holds up; read_crash_log and read_papyrus_log point at the mods behind a crash or script errors.
Separators (add_separator) are headings in the mod list with ids starting \"sep:\"; they hold no files and are never enabled. \
move_mods moves a separator and the mods under it when you list them all.
Mod rules (set_mod_rule: a mod loads after, requires, or is incompatible with other mods) are suggestions: health_check reports broken ones and sort_mods reorders the list to satisfy the load-after rules. set_mod_rule replaces the whole rule, so pass every list.
Profiles: each has its own enabled mods, mod order, plugin order and undo history. switch_profile changes the active one and previews the deploy that applies it; deploy to apply.
Every change can be reverted with undo; history lists them. Prefer small reversible steps and \
re-run health_check after changes.";

#[derive(Clone)]
pub struct Rmm {
    root: PathBuf,
    /// Background tool runs by tool id. In memory only; a finished run's report is also in
    /// tools/<id>/last-run.json.
    jobs: Arc<Mutex<BTreeMap<String, Job>>>,
    tool_router: ToolRouter<Self>,
}

#[derive(Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum Job {
    Running { started: u64 },
    Done { report: serde_json::Value },
    Failed { error: String },
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

#[derive(Deserialize, JsonSchema)]
struct ProfileName {
    /// Profile name as shown by profiles.
    name: String,
}
#[derive(Deserialize, JsonSchema)]
struct CreateProfileArgs {
    /// Folder name: up to 64 characters, no <>:"/\|?* and no leading dot.
    name: String,
    /// Copy the active profile (default true); false starts with every mod disabled.
    copy: Option<bool>,
}
#[derive(Deserialize, JsonSchema)]
struct ProfileOptionsArgs {
    /// The profile keeps its own copies of the game's INIs.
    local_inis: Option<bool>,
    /// The profile's saves go to Saves/<profile>/.
    local_saves: Option<bool>,
}
#[derive(Deserialize, JsonSchema)]
struct RenameProfileArgs {
    from: String,
    to: String,
}
#[derive(Deserialize, JsonSchema)]
struct DetailsArgs {
    id: String,
    /// Max files to list (default 200).
    max_files: Option<usize>,
}
#[derive(Deserialize, JsonSchema)]
struct InstallArgs {
    /// Path to a mod folder, .zip or .7z on this machine.
    path: String,
    /// Display name (defaults to the file name).
    name: Option<String>,
    /// Enable after install (default true).
    enable: Option<bool>,
    /// Required for mods with a FOMOD installer: the choices from fomod_options, edited as
    /// needed. {} takes the installer's defaults.
    choices: Option<Choices>,
    /// Priority to install at (0 = lowest). Default: the end, before generated tool output, or
    /// where mod rules place it.
    priority: Option<usize>,
}
#[derive(Deserialize, JsonSchema)]
struct FomodArgs {
    /// Path to a mod folder, .zip or .7z on this machine.
    path: String,
    /// Choices to preview; steps and groups left out take their defaults.
    choices: Option<Choices>,
}
#[derive(Deserialize, JsonSchema)]
struct CrashLogArgs {
    /// A crash log's path; default: the newest.
    path: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct PapyrusArgs {
    /// How many of the last lines to include (default 40).
    tail: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct TestRunArgs {
    /// Default true: show the launch plan and which mode the run would use.
    dry_run: Option<bool>,
    /// Return at once and report through tool_job (id "test_run").
    background: Option<bool>,
    #[serde(flatten)]
    scenario: harness::Scenario,
}

#[derive(Deserialize, JsonSchema)]
struct ReinstallArgs {
    id: String,
    /// New FOMOD choices (see fomod_options with the mod's source path); omit to keep the stored ones.
    choices: Option<Choices>,
}

#[derive(Deserialize, JsonSchema)]
struct RenameArgs {
    id: String,
    name: String,
}

#[derive(Deserialize, JsonSchema)]
struct ModIds {
    /// Mod ids as shown by list_mods.
    ids: Vec<String>,
}
#[derive(Deserialize, JsonSchema)]
struct EnableArgs {
    /// Mod ids as shown by list_mods; separators are skipped.
    ids: Vec<String>,
    enabled: bool,
}
#[derive(Deserialize, JsonSchema)]
struct MoveArgs {
    /// Mods and separators, moved as one block in their current order.
    ids: Vec<String>,
    /// Priority of the block's first mod in the resulting list, 0 = lowest. Higher wins conflicts.
    priority: usize,
}
#[derive(Deserialize, JsonSchema)]
struct AddSeparatorArgs {
    name: String,
    /// Priority to insert at (default: the end, i.e. highest).
    priority: Option<usize>,
}
#[derive(Deserialize, JsonSchema)]
struct AddonCallArgs {
    /// The add-on's name (list_addons).
    addon: String,
    /// One of its tools or panels.
    function: String,
    /// Input matching the tool's schema; default {}.
    input: Option<serde_json::Value>,
}
#[derive(Deserialize, JsonSchema)]
struct UpdatesArgs {
    /// Mod ids to check; default: every mod with Nexus ids.
    ids: Option<Vec<String>>,
}
#[derive(Deserialize, JsonSchema)]
struct NexusInstallArgs {
    /// An nxm:// link from the website's "Mod manager download" button. Or give mod_id and
    /// file_id, which only Premium accounts can download.
    link: Option<String>,
    mod_id: Option<u64>,
    file_id: Option<u64>,
    /// Default true: show the file without downloading it.
    dry_run: Option<bool>,
}
#[derive(Deserialize, JsonSchema)]
struct Mo2Args {
    /// The MO2 instance folder, holding ModOrganizer.ini (find_import_sources lists them).
    dir: String,
    /// MO2 profile; default: the one MO2 has selected.
    profile: Option<String>,
    /// Default true: report what would come in.
    dry_run: Option<bool>,
}
#[derive(Deserialize, JsonSchema)]
struct VortexArgs {
    /// Vortex's state.v2 folder or a state backup (JSON); find_import_sources lists them, the
    /// live state first.
    backup: String,
    /// Vortex's staging folder; default: the one the backup names.
    staging: Option<String>,
    /// Vortex profile name or id; default: the last active one.
    profile: Option<String>,
    /// Default true: report what would come in.
    dry_run: Option<bool>,
}
#[derive(Deserialize, JsonSchema)]
struct ExportArgs {
    /// Where to write it; default: modlist.lock.toml in the instance.
    path: Option<String>,
}
#[derive(Deserialize, JsonSchema)]
struct ApplyLockArgs {
    /// The lockfile.
    path: String,
    /// Folders (and their subfolders) to find the archives in.
    #[serde(default)]
    downloads: Vec<String>,
    /// Default true: report what would be installed, what is missing and what is kept.
    dry_run: Option<bool>,
}
#[derive(Deserialize, JsonSchema)]
struct ConflictArgs {
    /// A mod id: list every file it shares with other mods, who provides it and who wins.
    /// Omit for the summary of all mods.
    id: Option<String>,
}
#[derive(Deserialize, JsonSchema)]
struct NoteArgs {
    id: String,
    /// The notes; "" clears them.
    note: String,
}
#[derive(Deserialize, JsonSchema)]
struct ColorArgs {
    /// Mod or separator id.
    id: String,
    /// #rrggbb; "" removes it.
    color: String,
}
#[derive(Deserialize, JsonSchema)]
struct LockArgs {
    /// Plugin file name.
    name: String,
    /// false unlocks it.
    lock: bool,
}
#[derive(Deserialize, JsonSchema)]
struct RestoreArgs {
    /// History entry (0 = the newest, as history lists them): go back to before it.
    n: usize,
    /// Preview only (default true).
    dry_run: Option<bool>,
}
#[derive(Deserialize, JsonSchema)]
struct EditSeparatorArgs {
    /// Separator id, "sep:<name>".
    id: String,
    /// New name; the id follows it.
    name: Option<String>,
    /// Colour as #rrggbb; "" removes it.
    color: Option<String>,
}
#[derive(Deserialize, JsonSchema)]
struct OrderArgs {
    /// Every mod id, lowest priority first.
    ids: Vec<String>,
}
#[derive(Deserialize, JsonSchema)]
struct PluginEnableArgs {
    /// Plugin file name, e.g. "Unofficial Skyrim Special Edition Patch.esp".
    name: String,
    enabled: bool,
}
#[derive(Deserialize, JsonSchema)]
struct PluginOrderArgs {
    /// Plugin names in the desired order; unlisted plugins keep their order after these.
    names: Vec<String>,
}
#[derive(Deserialize, JsonSchema)]
struct DryRun {
    /// Preview only (default true). Set false to change the game folder.
    dry_run: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
struct AddToolArgs {
    /// Tool id: letters, digits, '-' and '_'. An id naming a preset uses that preset.
    id: String,
    #[serde(flatten)]
    spec: ToolSpec,
}
#[derive(Deserialize, JsonSchema)]
struct RunToolArgs {
    /// Tool id from list_tools.
    id: String,
    /// Placeholder values for this run, e.g. {"plugin": "Update.esm"}.
    #[serde(default)]
    vars: BTreeMap<String, String>,
    /// Preview only (default true). Set false to deploy, run the tool and capture its output.
    dry_run: Option<bool>,
    /// Stop the tool after this many seconds.
    timeout_secs: Option<u64>,
    /// Return at once and run in the background (default false); poll tool_job for the report.
    /// Changes to the setup are refused until it finishes.
    background: Option<bool>,
}
#[derive(Deserialize, JsonSchema)]
struct ToolJobArgs {
    /// Tool id; omit for every background run this server started.
    id: Option<String>,
}

fn out<T: Serialize>(r: anyhow::Result<T>) -> Result<String, String> {
    r.and_then(|v| Ok(serde_json::to_string_pretty(&v)?)).map_err(|e| format!("{e:#}"))
}

// ponytail: tools call blocking core functions directly; fine for one client on stdio.
// Move to spawn_blocking if the UI ever hosts this over HTTP for several clients.
#[tool_router]
impl Rmm {
    fn inst(&self) -> anyhow::Result<Instance> {
        Instance::open(&self.root)
    }

    /// The instance for a change: refused while a background tool run owns the game folder.
    fn inst_mut(&self) -> anyhow::Result<Instance> {
        let jobs = self.jobs.lock().unwrap();
        if let Some((id, _)) = jobs.iter().find(|(_, j)| matches!(j, Job::Running { .. })) {
            anyhow::bail!("tool '{id}' is running in the background; wait for it (tool_job) before changing the setup");
        }
        self.inst()
    }

    fn inst_for(&self, dry_run: bool) -> anyhow::Result<Instance> {
        if dry_run { self.inst() } else { self.inst_mut() }
    }

    #[tool(description = "Game, paths, profile and mod counts for this instance.")]
    async fn instance_info(&self) -> Result<String, String> {
        out(self.inst().and_then(|i| ops::instance_info(&i)))
    }

    #[tool(description = "All mods in priority order (later wins conflicts) with enabled state, file and plugin counts.")]
    async fn list_mods(&self) -> Result<String, String> {
        out(self.inst().and_then(|i| ops::list_mods(&i)))
    }

    #[tool(description = "One mod: metadata, files and its conflicts.")]
    async fn mod_details(&self, Parameters(a): Parameters<DetailsArgs>) -> Result<String, String> {
        out(self.inst().and_then(|i| ops::mod_details(&i, &a.id, a.max_files.unwrap_or(200))))
    }

    #[tool(description = "Conflicts between enabled mods: loose files (higher priority wins) and files inside loaded BSA/BA2 archives (the archive whose plugin loads later wins; any loose file beats every archive). Who overwrites whom, and mods that are fully overridden. With a mod id: each file it shares, every mod providing it in load order (with the archive for archived files) and the winner.")]
    async fn list_conflicts(&self, Parameters(a): Parameters<ConflictArgs>) -> Result<String, String> {
        match a.id {
            Some(id) => out(self.inst().and_then(|i| conflicts::file_conflicts(&i, &i.profile()?, &id))),
            None => out(self.inst().and_then(|i| conflicts::conflicts(&i, &i.profile()?))),
        }
    }

    #[tool(description = "Plugins in load order with master/light flags, masters and missing masters.")]
    async fn list_plugins(&self) -> Result<String, String> {
        out(self.inst().and_then(|i| plugins::list(&i, &i.profile()?)))
    }

    #[tool(description = "Problems to fix: missing masters, plugin limits, missing script extender, fully overridden mods, archives no plugin loads, undeployed or externally changed files, LOOT warnings and dirty plugins.")]
    async fn health_check(&self) -> Result<String, String> {
        out(self.inst().and_then(|i| health::check(&i)))
    }

    #[tool(description = "Install a mod from a local folder, .zip or .7z. Detects the Data root and game-root files. Mods with a FOMOD installer need choices (see fomod_options).")]
    async fn install_mod(&self, Parameters(a): Parameters<InstallArgs>) -> Result<String, String> {
        out(self.inst_mut().and_then(|i| install::install_at(&i, a.path.as_ref(), a.name.as_deref(), a.enable.unwrap_or(true), a.choices.as_ref(), a.priority)))
    }

    #[tool(description = "Show a mod's FOMOD installer as it plays out with the given choices: visible steps, groups, options (type, description, selected), the full resolved choices, and the file count. Conditions are checked against the current setup.")]
    async fn fomod_options(&self, Parameters(a): Parameters<FomodArgs>) -> Result<String, String> {
        out(self.inst().and_then(|i| install::fomod_options(&i, a.path.as_ref(), a.choices.as_ref())))
    }

    #[tool(description = "Install a mod again from the archive or folder it came from (mod_details shows meta.source and meta.fomod), optionally with new FOMOD choices. It keeps its id, name, place and enabled state; the old files go to trash. Not undoable.")]
    async fn reinstall_mod(&self, Parameters(a): Parameters<ReinstallArgs>) -> Result<String, String> {
        out(self.inst_mut().and_then(|i| install::reinstall(&i, &a.id, a.choices.as_ref())))
    }

    #[tool(description = "Rename a mod (its display name; the id stays). Returns the previous name. Not undoable.")]
    async fn rename_mod(&self, Parameters(a): Parameters<RenameArgs>) -> Result<String, String> {
        out(self.inst_mut().and_then(|i| ops::rename_mod(&i, &a.id, &a.name)))
    }

    #[tool(description = "Uninstall mods and remove separators in one undo step. Mod files go to trash and come back on undo.")]
    async fn uninstall_mods(&self, Parameters(a): Parameters<ModIds>) -> Result<String, String> {
        out(self.inst_mut().and_then(|i| install::uninstall_mods(&i, &a.ids)))
    }

    #[tool(description = "Enable or disable mods (their plugins follow) in one undo step.")]
    async fn set_mods_enabled(&self, Parameters(a): Parameters<EnableArgs>) -> Result<String, String> {
        out(self.inst_mut().and_then(|i| ops::set_mods_enabled(&i, &a.ids, a.enabled)))
    }

    #[tool(description = "Move mods and separators as one block, keeping their order, to a new priority.")]
    async fn move_mods(&self, Parameters(a): Parameters<MoveArgs>) -> Result<String, String> {
        out(self.inst_mut().and_then(|i| ops::move_mods(&i, &a.ids, a.priority)))
    }

    #[tool(description = "Add a separator, a heading for the mods below it in the list. Returns its id.")]
    async fn add_separator(&self, Parameters(a): Parameters<AddSeparatorArgs>) -> Result<String, String> {
        out(self.inst_mut().and_then(|i| ops::add_separator(&i, &a.name, a.priority)))
    }

    #[tool(description = "Rename a separator and/or set its colour. Returns its id, which follows the name.")]
    async fn edit_separator(&self, Parameters(a): Parameters<EditSeparatorArgs>) -> Result<String, String> {
        out(self.inst_mut().and_then(|i| ops::edit_separator(&i, &a.id, a.name.as_deref(), a.color.as_deref())))
    }

    #[tool(description = "Set or clear a mod's notes (shown in the mod list). Not in undo history; returns the previous notes.")]
    async fn set_mod_note(&self, Parameters(a): Parameters<NoteArgs>) -> Result<String, String> {
        out(self.inst_mut().and_then(|i| ops::set_mod_note(&i, &a.id, &a.note)))
    }

    #[tool(description = "Colour a mod or separator row (#rrggbb, \"\" removes it). Undoable.")]
    async fn set_color(&self, Parameters(a): Parameters<ColorArgs>) -> Result<String, String> {
        out(self.inst_mut().and_then(|i| ops::set_color(&i, &a.id, &a.color)))
    }

    #[tool(description = "Lock a plugin where it is in the load order (move it first with set_plugin_order), so sorting and moving other plugins leave it there; lock false unlocks. Undoable.")]
    async fn lock_plugin(&self, Parameters(a): Parameters<LockArgs>) -> Result<String, String> {
        out(self.inst_mut().and_then(|i| ops::lock_plugin(&i, &a.name, a.lock)))
    }

    #[tool(description = "Set the complete mod order in one step.")]
    async fn set_mod_order(&self, Parameters(a): Parameters<OrderArgs>) -> Result<String, String> {
        out(self.inst_mut().and_then(|i| ops::set_mod_order(&i, &a.ids)))
    }

    #[tool(description = "Enable or disable a single plugin.")]
    async fn set_plugin_enabled(&self, Parameters(a): Parameters<PluginEnableArgs>) -> Result<String, String> {
        out(self.inst_mut().and_then(|i| ops::set_plugin_enabled(&i, &a.name, a.enabled)))
    }

    #[tool(description = "Sort plugins with LOOT (masterlist refreshed daily, plus loot/userlist.yaml). dry_run defaults to true and returns the new order and which plugins move; otherwise the order is saved to the profile (undoable) and reaches plugins.txt on the next deploy.")]
    async fn sort_plugins(&self, Parameters(a): Parameters<DryRun>) -> Result<String, String> {
        out(self.inst_for(a.dry_run.unwrap_or(true)).and_then(|i| loot::sort(&i, a.dry_run.unwrap_or(true))))
    }

    #[tool(description = "The user's LOOT rules (loot/userlist.yaml) for the profile's plugins: load-after lists and groups.")]
    async fn loot_rules(&self) -> Result<String, String> {
        out(self.inst().and_then(|i| loot::rules(&i)))
    }

    #[tool(description = "Replace one plugin's LOOT user rule: the plugins it loads after and its group (from the masterlist). An empty rule removes it. Saved to loot/userlist.yaml in LOOT's format and used by the next sort_plugins. Not in undo history: returns the previous rule, so set that to revert.")]
    async fn set_loot_rule(&self, Parameters(a): Parameters<loot::Rule>) -> Result<String, String> {
        out(self.inst_mut().and_then(|i| Ok(serde_json::json!({ "previous": loot::set_rule(&i, &a)? }))))
    }

    #[tool(description = "Download the latest LOOT masterlist and prelude now. Returns warnings, if any.")]
    async fn update_masterlist(&self) -> Result<String, String> {
        out(self.inst().and_then(|i| loot::update_masterlist(&i, true)))
    }

    #[tool(description = "Mod rules (rules.toml, shared by all profiles): each mod and the mods it loads after, i.e. wins file conflicts against.")]
    async fn mod_rules(&self) -> Result<String, String> {
        out(self.inst().and_then(|i| rules::rules(&i)))
    }

    #[tool(description = "Replace a mod's rule: the mods it loads after, requires (must be enabled with it) and is incompatible with (must not be). All lists empty removes it. Rules never block ordering or enabling; health_check reports broken ones and sort_mods applies the load-after lists. Not in undo history, so it returns the previous rule.")]
    async fn set_mod_rule(&self, Parameters(a): Parameters<rules::ModRule>) -> Result<String, String> {
        out(self.inst_mut().and_then(|i| rules::set_rule(&i, &a)))
    }

    #[tool(description = "Reorder the profile's mods to satisfy the mod rules, moving only what a rule forces. dry_run defaults to true and returns the new order, the mods that move and rules dropped because they form a cycle; otherwise the order is saved (undoable).")]
    async fn sort_mods(&self, Parameters(a): Parameters<DryRun>) -> Result<String, String> {
        out(self.inst_for(a.dry_run.unwrap_or(true)).and_then(|i| rules::sort(&i, a.dry_run.unwrap_or(true))))
    }

    #[tool(description = "Reorder plugins. Masters still load before non-masters regardless.")]
    async fn set_plugin_order(&self, Parameters(a): Parameters<PluginOrderArgs>) -> Result<String, String> {
        out(self.inst_mut().and_then(|i| ops::set_plugin_order(&i, &a.names)))
    }

    #[tool(description = "Apply the profile to the game folder (hard links, incremental) and write plugins.txt. dry_run defaults to true and returns the plan.")]
    async fn deploy(&self, Parameters(a): Parameters<DryRun>) -> Result<String, String> {
        out(self.inst_for(a.dry_run.unwrap_or(true)).and_then(|i| deploy::deploy(&i, a.dry_run.unwrap_or(true))))
    }

    #[tool(description = "Deploy, check that the script extender's runtime DLL matches the game version, and start the game through the script extender's loader (or the game exe without one), through Proton on Linux. dry_run defaults to true and returns the deploy plan, game version and checks.")]
    async fn launch_game(&self, Parameters(a): Parameters<DryRun>) -> Result<String, String> {
        out(self.inst_for(a.dry_run.unwrap_or(true)).and_then(|i| rmm_core::launch::launch(&i, a.dry_run.unwrap_or(true))))
    }

    #[tool(description = "Remove all deployed files and restore originals. dry_run defaults to true.")]
    async fn purge(&self, Parameters(a): Parameters<DryRun>) -> Result<String, String> {
        out(self.inst_for(a.dry_run.unwrap_or(true)).and_then(|i| deploy::purge(&i, a.dry_run.unwrap_or(true))))
    }

    #[tool(description = "Configured tools (exe, argument template, vars, rewrites) and the available presets.")]
    async fn list_tools(&self) -> Result<String, String> {
        out(self.inst().and_then(|i| Ok(serde_json::json!({ "tools": tools::list(&i)?, "presets": tools::PRESETS }))))
    }

    #[tool(description = "Register or replace a tool. Give the exe path; a preset fills in the arguments, which args, vars and rewrites override.")]
    async fn add_tool(&self, Parameters(a): Parameters<AddToolArgs>) -> Result<String, String> {
        out(self.inst_mut().and_then(|i| tools::add(&i, &a.id, a.spec)))
    }

    #[tool(description = "Run a tool against the deployed setup and capture its output into a generated mod (replacing its previous output, undoable). dry_run defaults to true and returns the command and plan. Blocks until the tool exits, unless background is true: then it returns at once and tool_job reports progress and the result.")]
    async fn run_tool(&self, Parameters(a): Parameters<RunToolArgs>) -> Result<String, String> {
        let timeout = a.timeout_secs.map(std::time::Duration::from_secs);
        let dry_run = a.dry_run.unwrap_or(true);
        if dry_run || !a.background.unwrap_or(false) {
            return out(self.inst_for(dry_run).and_then(|i| tools::run(&i, &a.id, &a.vars, dry_run, timeout)));
        }
        let inst = self.inst_mut().map_err(|e| format!("{e:#}"))?;
        self.jobs.lock().unwrap().insert(a.id.clone(), Job::Running { started: now() });
        let jobs = self.jobs.clone();
        std::thread::spawn(move || {
            let job = match tools::run(&inst, &a.id, &a.vars, false, timeout) {
                Ok(r) => Job::Done { report: serde_json::to_value(r).unwrap_or_default() },
                Err(e) => Job::Failed { error: format!("{e:#}") },
            };
            jobs.lock().unwrap().insert(a.id, job);
        });
        out(Ok(serde_json::json!({ "state": "running", "next": "poll tool_job" })))
    }

    #[tool(description = "State of background tool runs: running (with its start time), done (with the run report) or failed. With an id and no run started by this server, the last finished run's report from disk.")]
    async fn tool_job(&self, Parameters(a): Parameters<ToolJobArgs>) -> Result<String, String> {
        let jobs = self.jobs.lock().unwrap().clone();
        let Some(id) = a.id else { return out(Ok(jobs)) };
        if let Some(j) = jobs.get(&id) {
            return out(Ok(j.clone()));
        }
        out(self.inst().and_then(|i| {
            let text = std::fs::read_to_string(i.root.join("tools").join(&id).join("last-run.json")).map_err(|_| anyhow::anyhow!("tool '{id}' has no run on record"))?;
            Ok(Job::Done { report: serde_json::from_str(&text)? })
        }))
    }

    #[tool(description = "Profiles and the active one. Each profile has its own enabled mods, mod order, plugin order and undo history; installed mods, overwrite/, tools and LOOT rules are shared.")]
    async fn profiles(&self) -> Result<String, String> {
        out(self.inst().and_then(|i| Ok(serde_json::json!({ "active": i.config.profile, "profiles": i.profiles()? }))))
    }

    #[tool(description = "Create a profile from the active one: a copy (copy defaults to true) or empty, with every installed mod disabled. Doesn't switch to it.")]
    async fn create_profile(&self, Parameters(a): Parameters<CreateProfileArgs>) -> Result<String, String> {
        out(self.inst_mut().and_then(|i| i.create_profile(&a.name, a.copy.unwrap_or(true))))
    }

    #[tool(description = "Make a profile active. Mods installed under another profile are added to it disabled. Returns the deploy plan (dry run) that would apply it; the game folder changes only on deploy.")]
    async fn switch_profile(&self, Parameters(a): Parameters<ProfileName>) -> Result<String, String> {
        out(self.inst_mut().and_then(|mut i| {
            i.switch_profile(&a.name)?;
            deploy::deploy(&i, true)
        }))
    }

    #[tool(description = "The active profile's own INIs and saves (MO2's profile options); omitted ones stay as they are. Deploy applies them: the profile's INIs (profiles/<name>/ini/, first copied from the user's) go into My Games and what the game changes there goes back to the profile; saves go to Saves/<profile>/ through sLocalSavePath. Purge puts the user's own INIs and save folder back. Undoable. Returns both options.")]
    async fn set_profile_options(&self, Parameters(a): Parameters<ProfileOptionsArgs>) -> Result<String, String> {
        out(self.inst_mut().and_then(|i| {
            ops::set_profile_options(&i, a.local_inis, a.local_saves)?;
            let p = i.profile()?;
            Ok(serde_json::json!({ "profile": i.config.profile, "local_inis": p.local_inis, "local_saves": p.local_saves }))
        }))
    }

    #[tool(description = "Rename a profile (the active one too).")]
    async fn rename_profile(&self, Parameters(a): Parameters<RenameProfileArgs>) -> Result<String, String> {
        out(self.inst_mut().and_then(|mut i| i.rename_profile(&a.from, &a.to)))
    }

    #[tool(description = "Delete a profile other than the active one. It moves to profiles/.deleted/, which keeps the last deleted profile of each name.")]
    async fn delete_profile(&self, Parameters(a): Parameters<ProfileName>) -> Result<String, String> {
        out(self.inst_mut().and_then(|i| i.delete_profile(&a.name)))
    }

    #[tool(description = "Read a crash log (Crash Logger SSE, Buffout 4, Trainwreck, NetScriptFramework; default: the newest) and rank the DLLs, plugins and files it names, with the mod providing each. The call stack counts most. Lists the other crash logs too.")]
    async fn read_crash_log(&self, Parameters(a): Parameters<CrashLogArgs>) -> Result<String, String> {
        out(self.inst().and_then(|i| logs::read_crash_log(&i, a.path.as_deref().map(std::path::Path::new))))
    }

    #[tool(description = "Summarise the Papyrus log (Logs/Script/Papyrus.0.log): whether logging is on, errors grouped by message, scripts in error stacks with the mod providing each, and the last lines.")]
    async fn read_papyrus_log(&self, Parameters(a): Parameters<PapyrusArgs>) -> Result<String, String> {
        out(self.inst().and_then(|i| logs::read_papyrus_log(&i, a.tail.unwrap_or(40))))
    }

    #[tool(description = "Deploy, start the game through the script extender and test it. With rmm's harness plugin deployed the run is scripted: main menu, a save, coc through cells with FPS, PNGs of the window with screenshots. The game runs in a small window behind the others unless window is \"fullscreen\". Without it, the game must keep running for `timeout` seconds. A crash log written during the run fails it and is read back to mods; the Papyrus log is summarised. Closes the game afterwards unless keep_open. dry_run defaults to true. Long runs can go in the background (then poll tool_job with id test_run).")]
    async fn test_run(&self, Parameters(a): Parameters<TestRunArgs>) -> Result<String, String> {
        if a.dry_run.unwrap_or(true) {
            return out(self.inst().and_then(|i| harness::preview(&i)));
        }
        let inst = self.inst_mut().map_err(|e| format!("{e:#}"))?;
        if !a.background.unwrap_or(false) {
            return out(harness::test_run(&inst, &a.scenario));
        }
        self.jobs.lock().unwrap().insert("test_run".into(), Job::Running { started: now() });
        let jobs = self.jobs.clone();
        std::thread::spawn(move || {
            let job = match harness::test_run(&inst, &a.scenario) {
                Ok(r) => Job::Done { report: serde_json::to_value(r).unwrap_or_default() },
                Err(e) => Job::Failed { error: format!("{e:#}") },
            };
            jobs.lock().unwrap().insert("test_run".into(), job);
        });
        out(Ok(serde_json::json!({ "state": "running", "next": "poll tool_job with id test_run" })))
    }

    #[tool(description = "Whether the user has saved a Nexus API key, and the account's name and Premium status. The user saves the key themselves (`rmm nexus login` or the window); it never passes through MCP.")]
    async fn nexus_status(&self) -> Result<String, String> {
        out(Ok(nexus::status()))
    }

    #[tool(description = "Mods with a newer file on Nexus: installed and newest version, the new file's id and name, and its page. Uses the mods' Nexus ids (from installs through Nexus, MO2 or Vortex imports). Asks Nexus which mods changed in the last month and re-reads only those, so repeated checks are cheap. Needs the user's API key.")]
    async fn check_mod_updates(&self, Parameters(a): Parameters<UpdatesArgs>) -> Result<String, String> {
        out(self.inst().and_then(|i| nexus::check_updates(&i, &nexus::Client::from_env()?, a.ids.as_deref())))
    }

    #[tool(description = "Download a file from Nexus into the instance's downloads/ and install it (FOMOD installers take their defaults; reinstall_mod changes them), recording its Nexus ids. Without Premium, Nexus only allows downloads through an nxm:// link from the website's button. dry_run defaults to true: shows the file.")]
    async fn install_from_nexus(&self, Parameters(a): Parameters<NexusInstallArgs>) -> Result<String, String> {
        let inst = self.inst_mut().map_err(|e| format!("{e:#}"))?;
        let link = match (&a.link, a.mod_id, a.file_id) {
            (Some(l), _, _) => nexus::parse_nxm(l),
            (None, Some(m), Some(f)) => Ok(nexus::premium_link(&inst, m, f)),
            _ => Err(anyhow::anyhow!("give an nxm link, or mod_id and file_id")),
        };
        out(link.and_then(|link| {
            let client = nexus::Client::from_env()?;
            if a.dry_run.unwrap_or(true) {
                return client.get(&format!("/v1/games/{}/mods/{}/files/{}.json", inst.config.game.nexus_domain(), link.mod_id, link.file_id));
            }
            Ok(serde_json::to_value(nexus::install(&inst, &client, &link, None)?)?)
        }))
    }

    #[tool(description = "Installed add-ons (sandboxed WebAssembly plugins): their capabilities, the hooks they run at (after_install, before_deploy, after_deploy, after_tool_run, health_check), and the tools and panels they offer, with input schemas. The user installs add-ons themselves.")]
    async fn list_addons(&self) -> Result<String, String> {
        out(self.inst().and_then(|i| addons::list(&i)))
    }

    #[tool(description = "Call one of an add-on's tools or panels (list_addons) with JSON input; returns its JSON answer. Add-ons can only do what their capabilities allow; their changes land in undo history like any other.")]
    async fn call_addon(&self, Parameters(a): Parameters<AddonCallArgs>) -> Result<String, String> {
        out(self.inst_mut().and_then(|i| addons::call(&i, &a.addon, &a.function, &a.input.unwrap_or_else(|| serde_json::json!({})))))
    }

    #[tool(description = "MO2 instances (folders with ModOrganizer.ini under %LOCALAPPDATA%/ModOrganizer) and Vortex state backups (%APPDATA%/Vortex/temp/state_backups_full, newest first) found on this machine. A portable MO2 lives wherever the user put it.")]
    async fn find_import_sources(&self) -> Result<String, String> {
        out(Ok(import::find_sources()))
    }

    #[tool(description = "Bring in an MO2 profile: mod folders (hard-linked on the same volume, else copied; meta.ini stays behind, Root/ becomes root/), names, Nexus ids, comments as notes, mod order (reversed from modlist.txt), enabled state, separators, plugin order and locks, and overwrite/. Goes into the active profile as one undoable step; mods already installed under the same name are left as they are. dry_run defaults to true.")]
    async fn import_mo2(&self, Parameters(a): Parameters<Mo2Args>) -> Result<String, String> {
        out(self.inst_mut().and_then(|i| import::import_mo2(&i, std::path::Path::new(&a.dir), a.profile.as_deref(), a.dry_run.unwrap_or(true))))
    }

    #[tool(description = "Bring in a Vortex profile from Vortex's live state (the state.v2 folder; read from a copy, ask the user to close Vortex first) or a state backup: mods from the staging folder (dinput/enb mods go under root/), names, Nexus ids, FOMOD choices, enabled state, and Vortex's rules as mod rules, which then order the list; plugins from plugins.txt/loadorder.txt and Vortex's LOOT userlist. dry_run defaults to true. Purge Vortex's deployment before rmm deploys.")]
    async fn import_vortex(&self, Parameters(a): Parameters<VortexArgs>) -> Result<String, String> {
        out(self.inst_mut().and_then(|i| {
            import::import_vortex(&i, std::path::Path::new(&a.backup), a.staging.as_deref().map(std::path::Path::new), a.profile.as_deref(), a.dry_run.unwrap_or(true))
        }))
    }

    #[tool(description = "Write the active profile as modlist.lock.toml: every mod with its archive name and sha256, FOMOD choices, Nexus ids and a fingerprint of its files, plus the order, enabled state, colours, plugin order and mod rules. Returns the path.")]
    async fn export_lockfile(&self, Parameters(a): Parameters<ExportArgs>) -> Result<String, String> {
        out(self.inst().and_then(|i| lockfile::write(&i, a.path.as_deref().map(std::path::Path::new))))
    }

    #[tool(description = "Rebuild a setup from a lockfile: install missing mods from archives in `downloads` (matched by name, checked by sha256, with the locked FOMOD choices), list what still has to be downloaded (with Nexus links), then set order, enabled state, colours, plugin order and mod rules. Mods the profile has that the lockfile doesn't list are kept after the mod they followed. dry_run defaults to true.")]
    async fn apply_lockfile(&self, Parameters(a): Parameters<ApplyLockArgs>) -> Result<String, String> {
        let downloads: Vec<std::path::PathBuf> = a.downloads.iter().map(std::path::PathBuf::from).collect();
        out(self.inst_mut().and_then(|i| lockfile::apply(&i, &lockfile::read(std::path::Path::new(&a.path))?, &downloads, a.dry_run.unwrap_or(true))))
    }

    #[tool(description = "Undo the last change to the profile. Returns its label, or null if nothing to undo.")]
    async fn undo(&self) -> Result<String, String> {
        out(self.inst_mut().and_then(|i| i.undo()))
    }

    #[tool(description = "Redo the last undone change.")]
    async fn redo(&self) -> Result<String, String> {
        out(self.inst_mut().and_then(|i| i.redo()))
    }

    #[tool(description = "Undoable changes, newest first, each with what it did: mods enabled, disabled, added, removed or moved, and how many plugins changed.")]
    async fn history(&self) -> Result<String, String> {
        out(self.inst().and_then(|i| i.history_details()))
    }

    #[tool(description = "Go back to the state before history entry n (0 = the newest): n + 1 undos at once, so redo can step forward again. dry_run (default true) shows what would change.")]
    async fn restore(&self, Parameters(a): Parameters<RestoreArgs>) -> Result<String, String> {
        if a.dry_run.unwrap_or(true) {
            return out(self.inst().and_then(|i| i.restore_preview(a.n)));
        }
        out(self.inst_mut().and_then(|i| i.restore(a.n)))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Rmm {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("rmm", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

impl Rmm {
    pub fn new(root: PathBuf) -> Rmm {
        Rmm { root, jobs: Default::default(), tool_router: Rmm::tool_router() }
    }
}

pub async fn serve(root: PathBuf) -> anyhow::Result<()> {
    Rmm::new(root).serve(stdio()).await?.waiting().await?;
    Ok(())
}
