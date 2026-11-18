//! MCP server: the same operations as the CLI, as tools an AI can call.
//! Every mutation lands in undo history; game-folder writes default to dry runs.
use crate::agent::{self, Event};
use rmcp::{
    ErrorData, Peer, RoleServer, ServerHandler, ServiceExt,
    handler::server::{
        router::{prompt::PromptRouter, tool::ToolRouter},
        tool::ToolCallContext,
        wrapper::Parameters,
    },
    model::{CallToolRequestParams, CallToolResponse, Implementation, PromptMessage, Role, ServerCapabilities, ServerConfig},
    prompt, prompt_handler, prompt_router,
    service::RequestContext,
    tool, tool_handler, tool_router,
    transport::stdio,
};
use rmm_core::fomod::Choices;
use rmm_core::tools::ToolSpec;
use rmm_core::{Instance, addons, conflicts, deploy, harness, health, import, inspect, install, lockfile, logs, loot, nexus, ops, plugins, rules, saves, tools, workspace};
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
test_run starts it and checks it holds up; read_crash_log and read_papyrus_log point at the mods behind a crash or script errors, and read_script_extender_log says which SKSE/F4SE plugins loaded.
Separators (add_separator) are headings in the mod list with ids starting \"sep:\"; they hold no files and are never enabled. \
move_mods moves a separator and the mods under it when you list them all.
Mod rules (set_mod_rule: a mod loads after, requires, or is incompatible with other mods) are suggestions: health_check reports broken ones and sort_mods reorders the list to satisfy the load-after rules. set_mod_rule replaces the whole rule, so pass every list.
Profiles: each has its own enabled mods, mod order, plugin order and undo history. switch_profile changes the active one and previews the deploy that applies it; deploy to apply.
Files: list_files, read_file and edit_file work on mod files, overwrite/ and the game's INIs by workspace path \
(mods/<mod id>/..., overwrite/..., ini/<file>); every edit keeps the old file (file_edits, revert_edit).
Chats: list_chats and read_chat show the saved chats, rmm's own AI's and those of AI apps connected over MCP. \
An app connected over MCP (not rmm's own AI) has its tool calls kept as a chat, but not the conversation: \
call save_chat_note when a task ends with what the user asked for, what you did and decided, and what's left.
Every change can be reverted with undo; history lists them. Prefer small reversible steps and \
re-run health_check after changes.
Mod team (prompts find_mods, install_safely, test_mods and mod_team): search_nexus and nexus_mod_info find mods; \
scan_mod, read_mod_file and save_review check a mod once (mod_reviews has the saved reviews, current while the files \
are unchanged); plugin_records gives the ids the console takes; test_session_start, test_session_step and \
test_session_stop play the game step by step in a small window behind the user's.
The team: the user gives each role (lead, researcher, installer, tester) to one of rmm's AI providers or to you, \
the AI app connected over MCP, in rmm's Settings. team_tasks shows who does what and the steps handed over: \
ask_teammate gives a step to a role on a provider, and a task waiting for you is yours to do and finish_team_task.";

#[derive(Clone)]
pub struct Rmm {
    root: PathBuf,
    /// Background tool runs by tool id. In memory only; a finished run's report is also in
    /// tools/<id>/last-run.json.
    jobs: Arc<Mutex<BTreeMap<String, Job>>>,
    /// The live test (test_session_*); dropping it closes the game.
    session: Arc<Mutex<Option<harness::Session>>>,
    /// Some when the client's calls are kept as a chat (an AI app's session; see `recorded`).
    log: Option<Arc<Mutex<Option<agent::Chat>>>>,
    tool_router: ToolRouter<Self>,
    prompt_router: PromptRouter<Self>,
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
    /// Path to a mod folder, .zip, .7z or .rar on this machine.
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
    /// Path to a mod folder, .zip, .7z or .rar on this machine.
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
struct UpdateArgs {
    /// The installed mod to update.
    id: String,
    /// The new version: a mod folder, .zip, .7z or .rar on this machine.
    path: String,
    /// FOMOD choices for the new file; omit to keep the stored ones.
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
    /// An nxm:// link from the website's "Mod manager download" button, or a mod page link
    /// (nexusmods.com/<game>/mods/<id>). Or give mod_id, and file_id unless the newest main file
    /// will do. All but nxm links download only with Premium.
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
struct SaveArgs {
    /// The save's file name, as list_saves gives it.
    file: String,
}
#[derive(Deserialize, JsonSchema)]
struct NoteArgs {
    id: String,
    /// The notes; "" clears them.
    note: String,
}
#[derive(Deserialize, JsonSchema)]
struct NexusModArgs {
    /// The mod's id on Nexus (search_nexus's mod_id, the number in its page's address).
    mod_id: u64,
}
#[derive(Deserialize, JsonSchema)]
struct ReviewArgs {
    /// Mod id.
    id: String,
    /// "clean", "suspicious" or "malicious".
    verdict: String,
    /// What the mod does and how, what you checked (scan, DLLs, scripts), and what it costs.
    notes: String,
    /// Where it belongs in the order and why: what it loads after or before, patches it needs.
    load_order: Option<String>,
}
#[derive(Deserialize, JsonSchema)]
struct ReviewsArgs {
    /// A mod id; omit for every reviewed mod.
    id: Option<String>,
}
#[derive(Deserialize, JsonSchema)]
struct RecordsArgs {
    /// Plugin file name, such as "Frostfall.esp".
    plugin: String,
    /// Record types (four letters: QUST, NPC_, WEAP, ARMO, SPEL, CELL, GLOB...); default: the
    /// ones tests reach through the console.
    kinds: Option<Vec<String>>,
    /// At most this many records (default 300).
    max: Option<usize>,
}
#[derive(Deserialize, JsonSchema)]
struct SessionStartArgs {
    /// Default true: show the launch plan instead of starting the game.
    dry_run: Option<bool>,
    #[serde(flatten)]
    scenario: harness::Scenario,
}
#[derive(Deserialize, JsonSchema)]
struct SessionStepArgs {
    /// Steps in order: {"cell": id}, {"console": command}, {"wait": seconds}, {"screenshot": name}, {"load": save}.
    steps: Vec<harness::Step>,
}
#[derive(Deserialize, JsonSchema)]
struct ScanArgs {
    /// A mod id: scan it with Windows Defender too. Leave out to triage every mod (no Defender).
    id: Option<String>,
}
#[derive(Deserialize, JsonSchema)]
struct ReadFileArgs {
    /// Mod id.
    id: String,
    /// The file's path inside the mod, as mod_details lists it (scripts/foo.pex).
    path: String,
    /// First line to return (0-based) for text and scripts. Default 0.
    offset: Option<usize>,
    /// Lines to return. Default 400.
    limit: Option<usize>,
}
#[derive(Deserialize, JsonSchema)]
struct ListFilesArgs {
    /// A folder's workspace path: mods, mods/<mod id>/<folder>, overwrite/<folder>, or ini. Empty lists the roots.
    #[serde(default)]
    path: String,
}
#[derive(Deserialize, JsonSchema)]
struct WorkspaceReadArgs {
    /// mods/<mod id>/<path in the mod>, overwrite/<path>, or ini/<file>.
    path: String,
    /// First line to return (0-based). Default 0.
    offset: Option<usize>,
    /// Lines to return. Default 400.
    limit: Option<usize>,
}
#[derive(Deserialize, JsonSchema)]
struct EditFileArgs {
    /// mods/<mod id>/<path in the mod>, overwrite/<path>, or ini/<file>.
    path: String,
    /// Text to replace: exact, with enough lines around it to be found once. Leave out to write the whole file.
    old: Option<String>,
    /// The replacement for `old`, or the whole file.
    new: String,
}
#[derive(Deserialize, JsonSchema)]
struct TeammateArgs {
    /// researcher, installer or tester.
    role: String,
    /// What to do, with everything the teammate needs to know.
    task: String,
}
#[derive(Deserialize, JsonSchema)]
struct FinishTaskArgs {
    /// The task's number, from team_tasks.
    n: usize,
    /// Your report: what you did and found.
    answer: String,
}
#[derive(Deserialize, JsonSchema)]
struct ChatNoteArgs {
    /// A few lines: what the user asked for, what you did and decided, and what's left.
    text: String,
}
#[derive(Deserialize, JsonSchema)]
struct ChatIdArgs {
    /// The chat's id, from list_chats.
    id: String,
}
#[derive(Deserialize, JsonSchema)]
struct RevertEditArgs {
    /// The edit's number, from file_edits.
    n: usize,
}
#[derive(Deserialize, JsonSchema)]
struct PluginConflictArgs {
    /// Only the pairs with this plugin.
    plugin: Option<String>,
}
#[derive(Deserialize, JsonSchema)]
struct CategoryArgs {
    /// Mod ids.
    ids: Vec<String>,
    /// Categories to add (any text; list_mods shows the ones in use).
    #[serde(default)]
    add: Vec<String>,
    /// Categories to remove (case doesn't matter). To replace a mod's categories, remove its current ones.
    #[serde(default)]
    remove: Vec<String>,
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

    #[tool(description = "Game, paths, profile and mod counts for this instance.", annotations(read_only_hint = true))]
    async fn instance_info(&self) -> Result<String, String> {
        out(self.inst().and_then(|i| ops::instance_info(&i)))
    }

    #[tool(description = "All mods in priority order (later wins conflicts) with enabled state, file and plugin counts.", annotations(read_only_hint = true))]
    async fn list_mods(&self) -> Result<String, String> {
        out(self.inst().and_then(|i| ops::list_mods(&i)))
    }

    #[tool(description = "One mod: metadata, files and its conflicts.", annotations(read_only_hint = true))]
    async fn mod_details(&self, Parameters(a): Parameters<DetailsArgs>) -> Result<String, String> {
        out(self.inst().and_then(|i| ops::mod_details(&i, &a.id, a.max_files.unwrap_or(200))))
    }

    #[tool(description = "Conflicts between enabled mods: loose files (higher priority wins) and files inside loaded BSA/BA2 archives (the archive whose plugin loads later wins; any loose file beats every archive). Who overwrites whom, and mods that are fully overridden. With a mod id: each file it shares, every mod providing it in load order (with the archive for archived files) and the winner.", annotations(read_only_hint = true))]
    async fn list_conflicts(&self, Parameters(a): Parameters<ConflictArgs>) -> Result<String, String> {
        match a.id {
            Some(id) => out(self.inst().and_then(|i| conflicts::file_conflicts(&i, &i.profile()?, &id))),
            None => out(self.inst().and_then(|i| conflicts::conflicts(&i, &i.profile()?))),
        }
    }

    #[tool(description = "One folder of the Data folder as the game sees it after a deploy: its subfolders (with file counts), then each file with the mods providing it in load order (the last wins; none means the game's own file). `find` searches every folder instead (first 1000 paths); `conflicts_only` keeps files more than one mod provides; `archives` adds files inside loaded BSA/BA2 archives.", annotations(read_only_hint = true))]
    async fn list_data(&self, Parameters(q): Parameters<conflicts::DataQuery>) -> Result<String, String> {
        out(self.inst().and_then(|i| conflicts::data_view(&i, &i.profile()?, &q)))
    }

    #[tool(description = "The profile's saves, newest first: character, level, location, and the plugins each save was made with that the profile doesn't load now, with the installed mod that has each one. Loading such a save loses what those plugins added.", annotations(read_only_hint = true))]
    async fn list_saves(&self) -> Result<String, String> {
        out(self.inst().and_then(|i| saves::list(&i)))
    }

    #[tool(description = "Enable the installed mods and plugins a save needs (those list_saves names with a mod), as one undoable change. Returns the plugins enabled. Deploy afterwards.")]
    async fn enable_save_plugins(&self, Parameters(a): Parameters<SaveArgs>) -> Result<String, String> {
        out(self.inst().and_then(|i| saves::enable_missing(&i, &a.file)))
    }

    #[tool(description = "Archives in the instance's downloads folder (where Nexus downloads go), newest first, with the mods installed from each.", annotations(read_only_hint = true))]
    async fn list_downloads(&self) -> Result<String, String> {
        out(self.inst().and_then(|i| install::downloads(&i)))
    }

    #[tool(description = "Safety check of a mod's files. With an id, Windows Defender scans the mod's folder (nothing is quarantined). Every DLL and EXE is read for what its imports let it do (network, starting programs, registry writes, writing into other programs), packers, and strings such as URLs, addresses and shell commands; files that are programs posing as another type, scripts that run outside the game, and archives inside the mod are flagged too. Verdict: threat, review or clean. These are leads, not proof: read flagged files with read_mod_file, and a clean result isn't a guarantee. Without an id, every mod is triaged without Defender and only mods with native code or findings are listed.", annotations(read_only_hint = true))]
    async fn scan_mod(&self, Parameters(a): Parameters<ScanArgs>) -> Result<String, String> {
        out(self.inst().and_then(|i| inspect::scan(&i, a.id.as_deref())))
    }

    #[tool(description = "Read a file inside a mod. Text (Papyrus sources .psc, INI, JSON, XML, FOMOD config, readmes) comes as text. Compiled Papyrus scripts (.pex, Skyrim and Fallout 4) come disassembled: the script and what it extends, variables, properties, states, and each function's instructions (calls, property reads and writes, jumps), which shows what a script does and what it costs (OnUpdate loops, short RegisterForUpdate intervals, many calls per update, polling instead of events). DLLs and EXEs come as their imports, exports, sections and notable strings. Text and scripts are paged by line (offset, limit). mod_details lists a mod's files.", annotations(read_only_hint = true))]
    async fn read_mod_file(&self, Parameters(a): Parameters<ReadFileArgs>) -> Result<String, String> {
        out(self.inst().and_then(|i| inspect::read_file(&i, &a.id, &a.path, a.offset.unwrap_or(0), a.limit.unwrap_or(400))))
    }

    #[tool(description = "The folders and files the IDE and edit_file work on: mods/<mod id>/... (each installed mod's folder), overwrite/... (what tools and the game wrote) and ini (the INI files the game reads, in My Games). Give a folder's path; empty lists those three.", annotations(read_only_hint = true))]
    async fn list_files(&self, Parameters(a): Parameters<ListFilesArgs>) -> Result<String, String> {
        out(self.inst().and_then(|i| workspace::list(&i, &a.path)))
    }

    #[tool(description = "Read a file by its workspace path (see list_files), some lines at a time. Compiled Papyrus scripts come disassembled and DLLs as a summary; those are read-only. Read a file before edit_file changes it.", annotations(read_only_hint = true))]
    async fn read_file(&self, Parameters(a): Parameters<WorkspaceReadArgs>) -> Result<String, String> {
        out(self.inst().and_then(|i| workspace::read(&i, &a.path)).map(|d| {
            let lines: Vec<&str> = d.text.lines().collect();
            let first = a.offset.unwrap_or(0).min(lines.len());
            let text = lines.iter().skip(first).take(a.limit.unwrap_or(400)).copied().collect::<Vec<_>>().join("\n");
            serde_json::json!({ "path": d.path, "editable": d.editable, "note": d.note, "total_lines": lines.len(), "first_line": first, "text": text })
        }))
    }

    #[tool(description = "Change a text file: replace `old` (exact, found once in the file) with `new`, or without `old` write the whole file (making it if it's new, in a mod or overwrite/). The file's old content is kept: file_edits lists edits and revert_edit puts one back. A mod's file changes in place, so a deployed mod's change reaches the game at once; INIs apply the next time the game starts. Only text: for a compiled script, edit its .psc source. The user sees the change as a diff and approves it.")]
    async fn edit_file(&self, Parameters(a): Parameters<EditFileArgs>) -> Result<String, String> {
        out(self.inst_mut().and_then(|i| match &a.old {
            Some(old) => workspace::replace(&i, &a.path, old, &a.new, "AI"),
            None => workspace::write(&i, &a.path, &a.new, None, "AI"),
        }))
    }

    #[tool(description = "Edits made to files (by the user in the IDE, an AI, or a revert), newest first: number, path, when, by whom, lines before and after.", annotations(read_only_hint = true))]
    async fn file_edits(&self) -> Result<String, String> {
        out(self.inst().and_then(|i| workspace::edits(&i)))
    }

    #[tool(description = "Put a file back as it was before edit n (from file_edits). A file the edit made is removed. The revert is an edit too, so it can be reverted.")]
    async fn revert_edit(&self, Parameters(a): Parameters<RevertEditArgs>) -> Result<String, String> {
        out(self.inst_mut().and_then(|i| workspace::revert(&i, a.n)))
    }

    #[tool(description = "rmm's mod team: who does each role (one of rmm's AI providers with the user's key, or \"mcp\": the AI app connected over MCP; the user picks in rmm's Settings), and the team board: steps handed to a teammate, newest first, with their answers. A task waiting for you (an AI app over MCP) is yours: do it with its role's prompt, then finish_team_task.", annotations(read_only_hint = true))]
    async fn team_tasks(&self) -> Result<String, String> {
        out(agent::tasks(&self.root).map(|mut t| {
            t.reverse();
            serde_json::json!({ "team": agent::team(), "tasks": t })
        }))
    }

    #[tool(description = "Hand a step to a teammate the user gave to one of rmm's AI providers (team_tasks shows who): the researcher, installer or tester works on it in the background with its role's prompt, and its answer comes in team_tasks. It can read and preview but not change anything, so it reports changes for you to make. Give it all it needs: the request, the mod picked, what to test.")]
    async fn ask_teammate(&self, peer: Peer<RoleServer>, Parameters(a): Parameters<TeammateArgs>) -> Result<String, String> {
        let from = app_name(&peer).unwrap_or_else(|| "an AI app".into());
        agent::start_teammate(self.root.clone(), &a.role, &a.task, &from).map_err(|e| format!("{e:#}"))
    }

    #[tool(description = "Finish a team task that waited for you (team_tasks): your report goes on the board for the lead.")]
    async fn finish_team_task(&self, Parameters(a): Parameters<FinishTaskArgs>) -> Result<String, String> {
        agent::finish_task(&self.root, a.n, &a.answer).map(|()| format!("Team task {} is done.", a.n)).map_err(|e| format!("{e:#}"))
    }

    #[tool(description = "Saved chats in this instance, newest first: rmm's own AI chats and the sessions of AI apps connected over MCP (provider \"mcp\", model: the app). read_chat shows one.", annotations(read_only_hint = true))]
    async fn list_chats(&self) -> Result<String, String> {
        out(agent::chats(&self.root))
    }

    #[tool(description = "A saved chat as text: the user's and the AI's words (for an MCP app, the notes it saved), each tool call and the start of its answer. A long chat keeps its end. Read an earlier session before carrying on from it.", annotations(read_only_hint = true))]
    async fn read_chat(&self, Parameters(a): Parameters<ChatIdArgs>) -> Result<String, String> {
        agent::load_chat(&self.root, &a.id).map(|c| c.transcript(40_000)).map_err(|e| format!("{e:#}"))
    }

    #[tool(description = "For AI apps connected over MCP: rmm keeps your tool calls as a chat in the instance, but not the conversation. When a task ends, save a short note to it: what the user asked for, what you did and decided, and what's left. rmm's own AI and later sessions read it with read_chat.")]
    async fn save_chat_note(&self, peer: Peer<RoleServer>, Parameters(a): Parameters<ChatNoteArgs>) -> Result<String, String> {
        let text = a.text.trim().to_string();
        if text.is_empty() {
            return Err("the note is empty".into());
        }
        self.session_chat(app_name(&peer), |c| {
            // The first note names the session in the chat list.
            if !c.log.iter().any(|e| matches!(e, Event::Text { .. })) {
                c.title = text.lines().next().unwrap_or_default().chars().take(70).collect();
            }
            c.log.push(Event::Text { text });
        })
        .map(|id| format!("Saved to chat {id}."))
        .ok_or_else(|| "nothing to save to: rmm's own AI keeps its chats itself, and outside an instance there are none".into())
    }

    #[tool(description = "Record conflicts between enabled plugins (what xEdit shows): each pair of plugins that change the same records, how many, which one's version the game uses (the later in load order, `wins`), and whether the later one is a patch for the earlier (has it as a master, so the override is intended). The base game, DLC and Creation Club are left out, since every mod overrides them. With plugin: only its pairs. Where a plugin should go: compare with sort_plugins (dry run, LOOT's order) and the masters in list_plugins; loose-file and archive conflicts between mods are in list_conflicts.", annotations(read_only_hint = true))]
    async fn plugin_conflicts(&self, Parameters(a): Parameters<PluginConflictArgs>) -> Result<String, String> {
        out(self.inst().and_then(|i| plugins::record_overlaps(&i, &i.profile()?, a.plugin.as_deref())))
    }

    #[tool(description = "Search the game's mods on Nexus by words in their names and descriptions; no API key needed. Filter by Nexus category, sort (relevance, endorsements, downloads, updated, created), page with count and offset. Each result has its Nexus id, summary, category, author, endorsements, downloads, last update, page and picture, and the installed mod with that id, if any. Adult-only mods are left out unless adult is true. Search several times with different words; nexus_mod_info reads one mod's page.", annotations(read_only_hint = true))]
    async fn search_nexus(&self, Parameters(a): Parameters<nexus::Search>) -> Result<String, String> {
        out(self.inst().and_then(|i| nexus::search(&i, &nexus::Client::any(), &a)))
    }

    #[tool(description = "One Nexus mod's page: its description as text (compatibility notes, the patches it links), tags, version, the mods and DLC it requires (each with the installed mod, if any), and the mods that require it, which are its patches and add-ons (the first 100, and the total). No API key needed.", annotations(read_only_hint = true))]
    async fn nexus_mod_info(&self, Parameters(a): Parameters<NexusModArgs>) -> Result<String, String> {
        out(self.inst().and_then(|i| nexus::mod_info(&i, &nexus::Client::any(), a.mod_id)))
    }

    #[tool(description = "Save a review of a mod's code with the mod: verdict (clean, suspicious or malicious), notes on what it does, what was checked and what it costs, and where it belongs in the order. Replaces its last review. scan_mod and mod_reviews show it, with current=false once the mod's files change (reinstall, update), so a mod is checked again only when it changed. Not in undo history.")]
    async fn save_review(&self, Parameters(a): Parameters<ReviewArgs>) -> Result<String, String> {
        out(self.inst().and_then(|i| inspect::save_review(&i, &a.id, &a.verdict, &a.notes, a.load_order.as_deref().unwrap_or_default())))
    }

    #[tool(description = "Saved mod reviews: one mod's, or every reviewed mod's in the profile, each with current (its files are still the ones reviewed). Read these before reviewing a mod again.", annotations(read_only_hint = true))]
    async fn mod_reviews(&self, Parameters(a): Parameters<ReviewsArgs>) -> Result<String, String> {
        out(self.inst().and_then(|i| inspect::reviews(&i, a.id.as_deref())))
    }

    #[tool(description = "The records a plugin adds (not its changes to other plugins' records): by default quests, NPCs, leveled NPC lists, weapons, armour, ammo, spells, shouts, perks, books, misc items, potions, ingredients, scrolls, keys, containers, furniture, activators, cells, worldspaces and globals. Each has its editor id (what coc, startquest, setstage, movetoqt and set take), its name, and its form id in the current load order (what player.placeatme, player.additem and player.addspell take; none while the plugin is disabled). counts has how many of each type. What a test reaches through the console.", annotations(read_only_hint = true))]
    async fn plugin_records(&self, Parameters(a): Parameters<RecordsArgs>) -> Result<String, String> {
        out(self.inst().and_then(|i| plugins::new_records(&i, &i.profile()?, &a.plugin, &a.kinds.unwrap_or_default(), a.max.unwrap_or(300))))
    }

    #[tool(description = "Plugins in load order with master/light flags, masters and missing masters.", annotations(read_only_hint = true))]
    async fn list_plugins(&self) -> Result<String, String> {
        out(self.inst().and_then(|i| plugins::list(&i, &i.profile()?)))
    }

    #[tool(description = "Problems to fix: missing masters, plugin limits, missing script extender, fully overridden mods, archives no plugin loads, undeployed or externally changed files, LOOT warnings and dirty plugins.", annotations(read_only_hint = true))]
    async fn health_check(&self) -> Result<String, String> {
        out(self.inst().and_then(|i| health::check(&i)))
    }

    #[tool(description = "Install a mod from a local folder, .zip, .7z or .rar. Detects the Data root and game-root files. Mods with a FOMOD installer need choices (see fomod_options).")]
    async fn install_mod(&self, Parameters(a): Parameters<InstallArgs>) -> Result<String, String> {
        out(self.inst_mut().and_then(|i| install::install_at(&i, a.path.as_ref(), a.name.as_deref(), a.enable.unwrap_or(true), a.choices.as_ref(), a.priority)))
    }

    #[tool(description = "Show a mod's FOMOD installer as it plays out with the given choices: visible steps, groups, options (type, description, selected), the full resolved choices, and the file count. Conditions are checked against the current setup.", annotations(read_only_hint = true))]
    async fn fomod_options(&self, Parameters(a): Parameters<FomodArgs>) -> Result<String, String> {
        out(self.inst().and_then(|i| install::fomod_options(&i, a.path.as_ref(), a.choices.as_ref())))
    }

    #[tool(description = "Install a mod again from the archive or folder it came from (mod_details shows meta.source and meta.fomod), optionally with new FOMOD choices. It keeps its id, name, place and enabled state; the old files go to trash, and undo swaps them back.")]
    async fn reinstall_mod(&self, Parameters(a): Parameters<ReinstallArgs>) -> Result<String, String> {
        out(self.inst_mut().and_then(|i| install::reinstall(&i, &a.id, a.choices.as_ref())))
    }

    #[tool(description = "Update an installed mod in place from a newer archive or folder (a new download of the same mod): it keeps its id, name, place, enabled state, notes, categories, colour, rules and Nexus ids. A FOMOD installer takes the stored choices unless new ones are given (fomod_options on the new file). The old files go to trash, and undo swaps them back.")]
    async fn update_mod(&self, Parameters(a): Parameters<UpdateArgs>) -> Result<String, String> {
        out(self.inst_mut().and_then(|i| install::update(&i, &a.id, std::path::Path::new(&a.path), a.choices.as_ref())))
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

    #[tool(description = "Add or remove category labels (like MO2's categories) on mods. A mod without its own shows its Nexus category, which the first change keeps. Not in undo history. list_mods shows each mod's categories.")]
    async fn set_mod_categories(&self, Parameters(a): Parameters<CategoryArgs>) -> Result<String, String> {
        out(self.inst_mut().and_then(|i| ops::edit_categories(&i, &a.ids, &a.add, &a.remove)))
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

    #[tool(description = "The user's LOOT rules (loot/userlist.yaml) for the profile's plugins: load-after lists and groups.", annotations(read_only_hint = true))]
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

    #[tool(description = "Mod rules (rules.toml, shared by all profiles): each mod and the mods it loads after, i.e. wins file conflicts against.", annotations(read_only_hint = true))]
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

    #[tool(description = "Configured tools (exe, argument template, vars, rewrites) and the available presets.", annotations(read_only_hint = true))]
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

    #[tool(description = "State of background tool runs: running (with its start time), done (with the run report) or failed. With an id and no run started by this server, the last finished run's report from disk.", annotations(read_only_hint = true))]
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

    #[tool(description = "Profiles and the active one. Each profile has its own enabled mods, mod order, plugin order and undo history; installed mods, overwrite/, tools and LOOT rules are shared.", annotations(read_only_hint = true))]
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

    #[tool(description = "Read a crash log (Crash Logger SSE, Buffout 4, Trainwreck, NetScriptFramework; default: the newest) and rank the DLLs, plugins and files it names, with the mod providing each. The call stack counts most. Lists the other crash logs too.", annotations(read_only_hint = true))]
    async fn read_crash_log(&self, Parameters(a): Parameters<CrashLogArgs>) -> Result<String, String> {
        out(self.inst().and_then(|i| logs::read_crash_log(&i, a.path.as_deref().map(std::path::Path::new))))
    }

    #[tool(description = "Summarise the Papyrus log (Logs/Script/Papyrus.0.log): whether logging is on, errors grouped by message, scripts in error stacks with the mod providing each, and the last lines.", annotations(read_only_hint = true))]
    async fn read_papyrus_log(&self, Parameters(a): Parameters<PapyrusArgs>) -> Result<String, String> {
        out(self.inst().and_then(|i| logs::read_papyrus_log(&i, a.tail.unwrap_or(40))))
    }

    #[tool(description = "Which script extender plugins (SKSE/F4SE DLLs) loaded in the last game run, read from skse64.log or f4se.log: each with the mod that provides it and, when it didn't load, the log's reason (an Address Library or game version mismatch, a missing dependency). changed marks plugins whose mod was installed or updated after that run.")]
    async fn read_script_extender_log(&self) -> Result<String, String> {
        out(self.inst().and_then(|i| logs::script_extender_log(&i, true)))
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

    #[tool(description = "Start the game for a live test you drive step by step: deploy, start it through the script extender in a small window behind the user's windows (window \"960x540\" unless set; \"fullscreen\" keeps the game's own settings), wait for the main menu, then load the save, visit the cells and run the steps. Needs rmm's harness plugin deployed. Then send test_session_step batches, looking at each result (cell, FPS, screenshot PNG) before the next, and end with test_session_stop. One live test at a time. dry_run defaults to true: shows the launch plan. Ask the user first: the game uses their Steam account.")]
    async fn test_session_start(&self, Parameters(a): Parameters<SessionStartArgs>) -> Result<String, String> {
        if a.dry_run.unwrap_or(true) {
            return out(self.inst().and_then(|i| harness::preview(&i)));
        }
        let mut slot = self.session.lock().unwrap();
        if slot.is_some() {
            return Err("a live test is running; end it with test_session_stop first".into());
        }
        out(self.inst_mut().and_then(|i| {
            let session = harness::Session::start(&i, &a.scenario)?;
            let report = serde_json::to_value(session.progress())?;
            *slot = Some(session);
            Ok(report)
        }))
    }

    #[tool(description = "Run steps in the live test's game, in order: {\"cell\": \"Riverwood\"} (coc, then wait for the load), {\"console\": \"player.placeatme 0001A694 2\"} (any console command; its text output isn't read back), {\"wait\": 10} (seconds, sampling FPS), {\"screenshot\": \"name\"} (a PNG of the window; open it to see the game), {\"load\": \"save name\"}. Each result has ok or its error, FPS, the player's cell and the screenshot's path. If the game closes, the live test ends and the answer has the whole run with the crash log read back to mods.")]
    async fn test_session_step(&self, Parameters(a): Parameters<SessionStepArgs>) -> Result<String, String> {
        let mut slot = self.session.lock().unwrap();
        let Some(session) = slot.as_mut() else {
            return Err("no live test is running; start one with test_session_start".into());
        };
        let steps = session.steps(&a.steps);
        if session.alive() {
            return out(Ok(serde_json::json!({ "steps": steps })));
        }
        let session = slot.take().expect("checked above");
        out(self.inst().and_then(|i| Ok(serde_json::json!({ "steps": steps, "ended": "the game closed", "run": session.stop(&i)? }))))
    }

    #[tool(description = "End the live test: quit the game and report the whole session: every step, timings, FPS, screenshots, a crash log read back to mods, and the Papyrus log's errors with their mods. The INIs changed for the small window are put back.")]
    async fn test_session_stop(&self) -> Result<String, String> {
        let Some(session) = self.session.lock().unwrap().take() else {
            return Err("no live test is running".into());
        };
        out(self.inst().and_then(|i| session.stop(&i)))
    }

    #[tool(description = "Whether the user has saved a Nexus API key, and the account's name and Premium status. The user saves the key themselves (`rmm nexus login` or the window); it never passes through MCP.", annotations(read_only_hint = true))]
    async fn nexus_status(&self) -> Result<String, String> {
        out(Ok(nexus::status()))
    }

    #[tool(description = "Mods with a newer file on Nexus: installed and newest version, the new file's id and name, and its page. Uses the mods' Nexus ids (from installs through Nexus, MO2 or Vortex imports). Asks Nexus which mods changed in the last month and re-reads only those, so repeated checks are cheap. It also reads the requirements on the pages it re-reads, which health_check compares with what's installed. Needs the user's API key.", annotations(read_only_hint = true))]
    async fn check_mod_updates(&self, Parameters(a): Parameters<UpdatesArgs>) -> Result<String, String> {
        out(self.inst().and_then(|i| nexus::check_updates(&i, &nexus::Client::from_env()?, a.ids.as_deref())))
    }

    #[tool(description = "Download a file from Nexus with the user's account into the instance's downloads/ and install it (FOMOD installers take their defaults; reinstall_mod changes them), recording its Nexus ids. Give an nxm:// link, a mod page link, or a mod id (the newest main file unless file_id names one). Without Premium, Nexus only allows downloads through an nxm:// link from the website's button. Needs the user signed in (nexus_status). dry_run defaults to true: shows the file.")]
    async fn install_from_nexus(&self, Parameters(a): Parameters<NexusInstallArgs>) -> Result<String, String> {
        let inst = self.inst_mut().map_err(|e| format!("{e:#}"))?;
        let what = match (&a.link, a.mod_id, a.file_id) {
            (Some(l), _, _) => Ok(l.clone()),
            (None, Some(m), Some(f)) => Ok(format!("{m}/{f}")),
            (None, Some(m), None) => Ok(m.to_string()),
            _ => Err(anyhow::anyhow!("give a link or a mod_id")),
        };
        out(what.and_then(|what| {
            let client = nexus::Client::from_env()?;
            let link = nexus::resolve(&inst, &client, &what)?;
            if a.dry_run.unwrap_or(true) {
                return client.get(&format!("/v1/games/{}/mods/{}/files/{}.json", inst.config.game.nexus_domain(), link.mod_id, link.file_id));
            }
            Ok(serde_json::to_value(nexus::install(&inst, &client, &link, None)?)?)
        }))
    }

    #[tool(description = "Installed add-ons (sandboxed WebAssembly plugins): their capabilities, the hooks they run at (after_install, before_deploy, after_deploy, after_tool_run, health_check), and the tools and panels they offer, with input schemas. The user installs add-ons themselves.", annotations(read_only_hint = true))]
    async fn list_addons(&self) -> Result<String, String> {
        out(self.inst().and_then(|i| addons::list(&i)))
    }

    #[tool(description = "Call one of an add-on's tools or panels (list_addons) with JSON input; returns its JSON answer. Add-ons can only do what their capabilities allow; their changes land in undo history like any other.")]
    async fn call_addon(&self, Parameters(a): Parameters<AddonCallArgs>) -> Result<String, String> {
        out(self.inst_mut().and_then(|i| addons::call(&i, &a.addon, &a.function, &a.input.unwrap_or_else(|| serde_json::json!({})))))
    }

    #[tool(description = "MO2 instances (folders with ModOrganizer.ini under %LOCALAPPDATA%/ModOrganizer) and Vortex state backups (%APPDATA%/Vortex/temp/state_backups_full, newest first) found on this machine. A portable MO2 lives wherever the user put it.", annotations(read_only_hint = true))]
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

    #[tool(description = "Undoable changes, newest first, each with what it did: mods enabled, disabled, added, removed or moved, and how many plugins changed.", annotations(read_only_hint = true))]
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


// The mod team's roles, as MCP prompts. The user's AI client plays them (one agent each where it
// can start sub-agents); rmm provides the tools.

const RESEARCHER: &str = "\
You are the researcher in rmm's mod team for this {game} setup. The user wants: {request}

1. Understand the request. If the user attached images or video frames, describe what they show (armour style, weather, interface, animation...) and turn that into search words. You can't watch a video: ask for a few screenshots or a description instead.
2. Know the setup: instance_info, then list_mods (names, categories, Nexus ids). When the request names a mod (\"compatible with X\", \"a patch for X\"), find it: installed (list_mods) or on Nexus (search_nexus).
3. Search with search_nexus several times with different words (synonyms, the kind of mod, a category), by relevance and by endorsements. For \"works with X\", nexus_mod_info on X: its required_by lists the mods that need X, which are its patches and add-ons.
4. Check each candidate with nexus_mod_info: what it does, its compatibility notes and known incompatibilities, its requirements (installed or not), DLC, last update, endorsements. Compare with the user's mods: two mods doing the same job (two weather or combat overhauls), mods its page says it conflicts with, patches it needs for mods the user has.
5. Score each candidate 0-100 for how well it matches what the user asked for, and say separately how it fits the list (fits / needs patch X / conflicts with Y). Leave out anything under 60 unless nothing scores higher, and say so.
6. Report a ranked short list (at most 8): name, Nexus page, confidence and why, what else it needs, how it fits the current list, risks. Install nothing: the user picks, and the installer (the install_safely prompt) takes the pick.";

const INSTALLER: &str = "\
You are the installer in rmm's mod team for this {game} setup. Install {what} safely and give it its place.

1. Before downloading: nexus_mod_info (search_nexus first if you were given a name). Install its missing requirements first, the same way. install_from_nexus with dry_run=true shows the file, then dry_run=false installs it. Without Premium, Nexus only allows downloads through the website's \"Mod manager download\" button: ask the user to click it (rmm takes nxm links if they set that up), or to download the archive and give you its path for install_mod.
2. Right after installing, disable the mod (set_mods_enabled) until it is reviewed. Nothing reaches the game folder before a deploy.
3. Review it, unless mod_reviews has a review with current=true (same files): then use that one and go to step 4.
   - scan_mod with its id: Windows Defender plus the DLL and EXE checks. Verdict \"threat\": stop, tell the user, uninstall it (uninstall_mods).
   - read_mod_file on what the scan flagged, on every DLL (what its imports let it do: network, starting programs, registry writes) and on its scripts: what they do and what they cost (OnUpdate loops, RegisterForUpdate intervals of a few seconds, polling instead of events, many calls per update). Compiled code can't be changed here, so name the settings (its MCM, INI or JSON) that lower the cost.
   - save_review: verdict clean, suspicious or malicious; notes on what it does, what you checked and its cost; load_order. The next reviewer reads this instead of checking again.
   Malicious: uninstall it and stop. Suspicious: ask the user before enabling it.
4. Enable it and place it:
   - Files: list_conflicts with its id shows who it overwrites and who overwrites it. move_mods so the right mod wins (patches after what they patch), and set_mod_rule to record why, so sort_mods keeps it.
   - Plugins: plugin_conflicts with its plugin shows the records it shares and which plugin wins. sort_plugins with dry_run=true is LOOT's order; apply it if it agrees, and set_loot_rule for a rule LOOT lacks.
   - health_check: missing masters and requirements.
   Put the order you chose, and why, in the review's load_order (save_review again).
5. deploy with dry_run=true, then deploy. Hand over to the tester (the test_mods prompt) with what to test: the mod's features from its page and scripts, and its plugin_records (quests, NPCs, items, cells and their ids).
Every change is in history and can be undone.";

const TESTER: &str = "\
You are the tester in rmm's mod team for this {game} setup. Test: {focus}. Start from: {save}.
The lead (the user, or the lead agent) says what to test; follow its plan and report back to it.

The game runs in a small window behind the user's windows, so you see it only through screenshots. Ask the user before starting the game: it uses their Steam account. test_session_start says if the harness plugin isn't deployed.

1. Plan, unless the lead gave you one. For each mod under test read mod_reviews (what it does), nexus_mod_info (its page) and plugin_records (its quests, NPCs, items, spells and cells, with the ids the console takes). Then steps per mechanic:
   - Places: {\"cell\": \"<cell editor id>\"}, or {\"console\": \"player.moveto <reference id>\"}; then {\"wait\": 10} and {\"screenshot\": \"...\"}.
   - Combat: {\"console\": \"tgm\"} unless survival is the point, {\"console\": \"player.placeatme <NPC form id> 2\"} (the mod's NPCs or the game's own enemies), {\"wait\": 20}, a screenshot.
   - Quests: {\"console\": \"startquest <quest editor id>\"} or {\"console\": \"setstage <quest editor id> <stage>\"}, {\"console\": \"movetoqt <quest editor id>\"}, wait, screenshot.
   - Items and magic: player.additem <form id> 1, player.equipitem <form id>, player.addspell <form id>; wait, screenshot.
   - World: fw <weather form id>, set gamehour to 22, set timescale to 200 to let time pass; wait.
   - Scripts: wait long enough for them to run (30 to 60 seconds) and read the Papyrus log at the end.
2. test_session_start with the save (or a first cell, which starts the game from the main menu) and the first steps. Then test_session_step in small batches. Open every screenshot before the next batch: did the move land (the step's cell), does the fight happen, did the quest start, is the item there? The FPS of each wait shows the cost.
3. If the game closes, the step's answer ends the session with the crash log read back to the mods it names: report that to the lead and stop.
4. test_session_stop when done: crash, Papyrus errors with their mods, FPS.
5. Report per mechanic: worked, didn't, or couldn't tell, with the screenshots; crashes and script errors with the mods behind them; the lowest FPS and where. Don't change the setup: fixing is the installer's job.
Limits: console commands run, but their text output isn't read back, and you can't press keys or walk except through the console (coc, player.moveto, player.setpos).";

const LEAD: &str = "\
You lead rmm's mod team for this {game} setup. The user wants: {request}

The team has three roles, each with its own prompt on this server: researcher (find_mods), installer (install_safely) and tester (test_mods). The user gives each role to an AI in rmm's Settings (team_tasks shows who): hand a role's steps to it with ask_teammate, with everything it needs from you, and play the roles given to you yourself.
1. Researcher: a ranked list of candidates with confidence scores. Show it to the user, who picks. Never pick for them.
2. Installer, for each pick: download, safety review (or the saved one), placement, deploy.
3. Tester, guided by you: from the new mods' reviews, pages and plugin_records, decide what to test and where: combat, quests, items, magic, NPCs, weather and world, interface, and performance in heavy places (cities, big battles). Send the tester the plan, read its reports and screenshots, and send follow-up steps (another place, the next quest stage, a bigger fight) until each mechanic works or clearly fails.
4. Failures: a crash or script errors point at mods (read_crash_log, read_papyrus_log). Have the installer fix the order or rules or disable the culprit, or ask the user, then test again.
5. Report to the user: what was installed, the review verdicts, order changes, what was tested and how it went.
Ask the user before: downloads that need their click, starting the game (it uses their Steam account), and removing mods they had before.";

#[derive(Deserialize, JsonSchema)]
struct WantArgs {
    /// What the user wants: a description of the mod, "works with <mod>", or "fits my list".
    request: String,
}
#[derive(Deserialize, JsonSchema)]
struct InstallPromptArgs {
    /// A Nexus mod id or page link, or a mod's name to look up.
    what: String,
}
#[derive(Deserialize, JsonSchema)]
struct TestPromptArgs {
    /// Mods or mechanics to test (default: the mods installed last).
    focus: Option<String>,
    /// A save to start from (default: a cell, from the main menu).
    save: Option<String>,
}

#[prompt_router]
impl Rmm {
    fn role(&self, who: &str, text: &str, fills: &[(&str, &str)]) -> Vec<PromptMessage> {
        let game = self.inst().map_or_else(|_| "the game".to_string(), |i| i.config.game.steam_dir().to_string());
        let mut text = fills.iter().fold(text.replace("{game}", &game), |t, (k, v)| t.replace(&format!("{{{k}}}"), v));
        if let Some(mine) = rmm_core::settings::get().ai.role_instructions.get(who).filter(|t| !t.trim().is_empty()) {
            text.push_str(&format!("\n\nThe user's instructions for this role:\n{}", mine.trim()));
        }
        vec![PromptMessage::new_text(Role::User, text)]
    }

    #[prompt(description = "Researcher: find Nexus mods that match what the user describes (in words or images), work with a mod they name, or fit their list, ranked by confidence.")]
    async fn find_mods(&self, Parameters(a): Parameters<WantArgs>) -> Vec<PromptMessage> {
        self.role("researcher", RESEARCHER, &[("request", &a.request)])
    }

    #[prompt(description = "Installer: download a mod, check it isn't malicious (or reuse its saved review), review its code, place it in the mod and plugin order, and deploy.")]
    async fn install_safely(&self, Parameters(a): Parameters<InstallPromptArgs>) -> Vec<PromptMessage> {
        self.role("installer", INSTALLER, &[("what", &a.what)])
    }

    #[prompt(description = "Tester: play the game in a small background window through a live test session (teleports, fights, quests, items) and report crashes, script errors and FPS.")]
    async fn test_mods(&self, Parameters(a): Parameters<TestPromptArgs>) -> Vec<PromptMessage> {
        let focus = a.focus.unwrap_or_else(|| "the mods installed last".into());
        let save = a.save.unwrap_or_else(|| "a cell, from the main menu".into());
        self.role("tester", TESTER, &[("focus", &focus), ("save", &save)])
    }

    #[prompt(description = "Lead: run the whole mod team for a request: research, the user's pick, install and review, then guide the tester through each mechanic.")]
    async fn mod_team(&self, Parameters(a): Parameters<WantArgs>) -> Vec<PromptMessage> {
        self.role("lead", LEAD, &[("request", &a.request)])
    }
}

#[tool_handler(router = self.tool_router)]
#[prompt_handler(router = self.prompt_router)]
impl ServerHandler for Rmm {
    async fn call_tool(&self, request: CallToolRequestParams, context: RequestContext<RoleServer>) -> Result<CallToolResponse, ErrorData> {
        // An app's calls go to its session's chat; save_chat_note adds its note itself.
        let call = (self.log.is_some() && request.name != "save_chat_note")
            .then(|| (request.name.to_string(), request.arguments.clone().map(serde_json::Value::Object).unwrap_or_default(), app_name(&context.peer)));
        let r = self.tool_router.call(ToolCallContext::new(self, request, context)).await;
        if let Some((name, args, app)) = call {
            let (ok, text) = match &r {
                Ok(CallToolResponse::Complete(res)) => (res.is_error != Some(true), res.content.iter().filter_map(|c| c.as_text()).map(|t| t.text.as_str()).collect::<Vec<_>>().join("\n")),
                Ok(_) => (true, String::new()),
                Err(e) => (false, e.message.to_string()),
            };
            self.session_chat(app, |c| {
                c.log.push(Event::Call { name: name.clone(), args });
                c.log.push(Event::Result { name, ok, text: agent::cut(&text, 2000) });
            });
        }
        r
    }

    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().enable_prompts().build())
            .with_server_info(Implementation::new("rmm", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

impl Rmm {
    pub fn new(root: PathBuf) -> Rmm {
        Rmm { root, jobs: Default::default(), session: Default::default(), log: None, tool_router: Rmm::tool_router(), prompt_router: Rmm::prompt_router() }
    }

    /// This server for one outside AI app's session: its calls are kept as a chat in the instance.
    /// rmm's own AI uses `new`, as it keeps its chats itself.
    pub fn recorded(mut self) -> Rmm {
        self.log = Some(Default::default());
        self
    }

    /// `recorded` unless the user turned keeping sessions off (Settings).
    pub fn recorded_if_set(self) -> Rmm {
        if rmm_core::settings::get().mcp.record_sessions { self.recorded() } else { self }
    }

    /// Changes the session's chat (made on first use, in an instance only) and saves it; None when
    /// nothing is kept. Keeping it never fails a call.
    fn session_chat(&self, app: Option<String>, change: impl FnOnce(&mut agent::Chat)) -> Option<String> {
        let mut log = self.log.as_ref()?.lock().unwrap();
        if log.is_none() {
            Instance::open(&self.root).ok()?;
        }
        let chat = log.get_or_insert_with(|| agent::Chat::external(app.as_deref().unwrap_or("An AI app")));
        change(chat);
        chat.updated = now();
        chat.save(&self.root).ok()?;
        Some(chat.id.clone())
    }
}

/// The connected app's name, from its handshake.
fn app_name(peer: &Peer<RoleServer>) -> Option<String> {
    peer.peer_info().map(|i| i.client_info.title.clone().unwrap_or_else(|| i.client_info.name.clone()))
}

pub async fn serve(root: PathBuf) -> anyhow::Result<()> {
    Rmm::new(root).recorded_if_set().serve(stdio()).await?.waiting().await?;
    Ok(())
}
