use rmm::{agent, http, mcp};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rmm_core::{Game, Instance, addons, bisect, conflicts, deploy, game, harness, health, import, inspect, install, lockfile, logs, loot, nexus, ops, plugins, rules, saves, settings, tools};
use serde::Serialize;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "rmm", version, about = "Rust Mod Manager for Skyrim SE/AE and Fallout 4")]
struct Cli {
    /// Instance folder (defaults to $RMM_INSTANCE, then the current directory).
    #[arg(short, long, global = true, env = "RMM_INSTANCE")]
    instance: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Find installed games.
    Detect {
        /// Also look through every drive (slower): game folders outside Steam and the registry,
        /// and MO2 instances, portable ones too.
        #[arg(long)]
        search: bool,
    },
    /// Create an instance folder for a game.
    Init {
        dir: PathBuf,
        /// skyrim-se, fallout4, or a game the --addon folder defines
        #[arg(long)]
        game: String,
        /// An add-on folder defining the game (`[[games]]` in addon.toml); installed into the instance.
        #[arg(long)]
        addon: Option<PathBuf>,
        /// Game folder (auto-detected if omitted).
        #[arg(long)]
        game_path: Option<PathBuf>,
        /// Folder holding plugins.txt (auto-detected if omitted).
        #[arg(long)]
        local_path: Option<PathBuf>,
    },
    Info,
    /// List mods in priority order (last wins).
    Mods,
    Show { id: String },
    /// Install from a folder, .zip, .7z or .rar. It goes at the end (before generated tool output) or
    /// where mod rules place it, unless --at gives a priority.
    Install {
        path: PathBuf,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        at: Option<usize>,
        #[arg(long)]
        disabled: bool,
        /// FOMOD choices as JSON, or @file holding it; '{}' takes the installer's defaults.
        #[arg(long)]
        choices: Option<String>,
    },
    /// Show a FOMOD installer's steps and options; --choices previews a selection.
    Fomod {
        path: PathBuf,
        #[arg(long)]
        choices: Option<String>,
    },
    /// Install a mod again from where it came from; --choices changes its FOMOD options
    /// (default: the stored ones). Keeps its place; the old files go to trash/.
    Reinstall {
        id: String,
        #[arg(long)]
        choices: Option<String>,
    },
    /// Update a mod in place from a newer archive or folder: it keeps its id, place, notes,
    /// categories, colour and rules. The old files go to trash/; undo swaps them back.
    Update {
        id: String,
        path: PathBuf,
        /// FOMOD choices as JSON (default: the stored ones).
        #[arg(long)]
        choices: Option<String>,
    },
    /// Rename a mod (its id and folder stay). Prints the previous name.
    Rename { id: String, name: String },
    /// Uninstall mods (undoable) or remove separators.
    Uninstall {
        #[arg(required = true)]
        ids: Vec<String>,
    },
    Enable {
        #[arg(required = true)]
        ids: Vec<String>,
    },
    Disable {
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// Move mods as one block, in their current order, so the first lands at --to (0 = lowest).
    Move {
        #[arg(required = true)]
        ids: Vec<String>,
        #[arg(long)]
        to: usize,
    },
    /// Separators: headings that group mods in the list (ids start with "sep:").
    #[command(subcommand)]
    Separator(SeparatorCmd),
    /// Conflicts between mods, or every conflicting file of one mod and who wins it.
    Conflicts { id: Option<String> },
    /// One folder of Data as the game sees it: subfolders, and each file with the mods behind it.
    Data {
        /// A folder in Data, such as meshes/actors (default: Data itself).
        dir: Option<String>,
        /// Search every folder for paths containing this instead.
        #[arg(long)]
        find: Option<String>,
        /// Only files more than one mod provides.
        #[arg(long)]
        conflicts: bool,
        /// Also files inside the BSA/BA2 archives the game loads.
        #[arg(long)]
        archives: bool,
    },
    /// The profile's saves, each with the plugins it was made with that aren't loaded now.
    Saves {
        /// Enable the installed mods and plugins this save (file name) needs.
        #[arg(long)]
        fix: Option<String>,
        /// Copy the saves named (file names) to this profile's save folder, with their co-saves.
        #[arg(long, value_name = "PROFILE", conflicts_with_all = ["fix", "move_to"], requires = "files")]
        copy_to: Option<String>,
        /// Move them instead.
        #[arg(long, value_name = "PROFILE", conflicts_with = "fix", requires = "files")]
        move_to: Option<String>,
        /// Save file names, for --copy-to and --move-to.
        files: Vec<String>,
    },
    /// Archives in the instance's downloads folder and the mods installed from them.
    Downloads {
        /// Keep this instance's downloads in this folder from now on ("" for downloads/ in the
        /// instance). Files already downloaded stay where they are.
        #[arg(long)]
        folder: Option<String>,
        /// Delete this download (its file name) for good. Mods installed from it stay, but
        /// can't be reinstalled from it.
        #[arg(long, conflicts_with = "folder")]
        delete: Option<String>,
    },
    /// Files in overwrite/: what the game and tools wrote that no mod holds.
    Overwrite {
        /// Turn them into a new mod with this name, at the end of the list.
        #[arg(long)]
        to_mod: Option<String>,
        /// Clear overwrite/ (undo brings the files back, as a mod).
        #[arg(long, conflicts_with = "to_mod")]
        clear: bool,
    },
    /// Clean every plugin LOOT knows as dirty with xEdit QuickAutoClean (a tool from the xedit-qac
    /// preset), one run each. Lists them; --apply cleans.
    CleanPlugins {
        #[arg(long)]
        apply: bool,
    },
    /// Hide files or folders of a mod (MO2's .mohidden; a hidden plugin is an optional ESP): they
    /// leave deploys, conflicts and the plugin list, and stay hidden through reinstalls.
    Hide {
        #[arg(value_name = "MOD")]
        id: String,
        #[arg(required = true)]
        paths: Vec<String>,
        /// Show them again.
        #[arg(long)]
        show: bool,
    },
    /// Move files or folders from one mod into another; `overwrite` names Overwrite (MCM settings
    /// into a settings mod, say). --replace puts the target's copies of clashing files in trash.
    MoveFiles {
        from: String,
        to: String,
        #[arg(required = true)]
        paths: Vec<String>,
        #[arg(long)]
        replace: bool,
    },
    /// Creation Club content in the game's Data folder as mods (one per Creation, on in every
    /// profile, so each can be turned off), or with --restore back into Data. Shows what it
    /// would do; --apply does it.
    Creations {
        #[arg(long)]
        restore: bool,
        #[arg(long)]
        apply: bool,
    },
    /// Edits made to mod files, overwrite/ and INIs through rmm (the window's IDE and AI
    /// tools), newest first.
    Edits {
        /// Put back the file as it was before edit N.
        #[arg(long)]
        revert: Option<usize>,
    },
    /// rmm's settings, shared by the window and the MCP server: all of them, one by its name
    /// (deploy.method), or set one (`rmm settings loot.language de`; lists take commas or JSON).
    Settings {
        key: Option<String>,
        value: Option<String>,
        /// Put the named setting (or, without one, all of them) back to its default.
        #[arg(long, conflicts_with = "value")]
        reset: bool,
    },
    /// How much uninstalled mods take in trash/ (undo restores them from there).
    Trash {
        /// Delete it all for good; undo can't bring those mods back afterwards.
        #[arg(long)]
        empty: bool,
    },
    /// Set a mod's notes ("" clears them).
    Note { id: String, text: String },
    /// Add or remove category labels on mods (like MO2's categories).
    Category {
        ids: Vec<String>,
        #[arg(long)]
        add: Vec<String>,
        #[arg(long)]
        remove: Vec<String>,
    },
    /// Colour a mod or separator row: #rrggbb, or "" to remove it.
    Color { id: String, color: String },
    /// Lock a plugin where it is in the load order: sorts and moves leave it there.
    LockPlugin {
        name: String,
        #[arg(long)]
        unlock: bool,
    },
    /// Go back to before history entry N (0 = newest; see history); --apply to do it.
    Restore {
        n: usize,
        #[arg(long)]
        apply: bool,
    },
    /// The profile's plugins in load order; --enable and --disable turn plugins on or off first.
    Plugins {
        #[arg(long)]
        enable: Vec<String>,
        #[arg(long)]
        disable: Vec<String>,
        /// Move this plugin, to just after --after or just before --before.
        #[arg(long = "move", requires = "place")]
        moving: Option<String>,
        #[arg(long, group = "place")]
        after: Option<String>,
        #[arg(long, group = "place")]
        before: Option<String>,
    },
    /// Pairs of enabled plugins that change the same records, and which wins.
    PluginConflicts { plugin: Option<String> },
    /// Safety check of a mod (with Windows Defender), or a quick one of every mod.
    Scan { id: Option<String> },
    /// Show a file inside a mod: text, a disassembled Papyrus script, or a DLL's imports.
    Read {
        id: String,
        path: String,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long, default_value_t = 400)]
        limit: usize,
    },
    /// Records a plugin adds (quests, NPCs, items, spells, cells...) with the ids the console takes.
    Records {
        plugin: String,
        /// Record type (repeatable), such as QUST, NPC_ or WEAP; default: the ones tests use.
        #[arg(long = "kind")]
        kinds: Vec<String>,
        #[arg(long, default_value_t = 300)]
        max: usize,
    },
    /// Save a review of a mod's code, kept with the mod so it isn't checked again.
    Review {
        id: String,
        /// clean, suspicious or malicious.
        #[arg(long)]
        verdict: String,
        /// What it does and what was checked.
        #[arg(long)]
        notes: String,
        /// Where it belongs in the order, and why.
        #[arg(long, default_value = "")]
        load_order: String,
    },
    /// Saved reviews (one mod's, or every reviewed mod's), and whether the files changed since.
    Reviews { id: Option<String> },
    /// Sort plugins with LOOT; --apply saves the order to the profile.
    Sort {
        #[arg(long)]
        apply: bool,
    },
    /// Download the latest LOOT masterlist now (sort refreshes it daily by itself).
    Masterlist,
    /// LOOT user rules for the profile's plugins; with a plugin, replace its rule (no --after or
    /// --group removes it). Kept in loot/userlist.yaml, LOOT's own format.
    Rule {
        plugin: Option<String>,
        /// Plugin this one loads after (repeatable).
        #[arg(long)]
        after: Vec<String>,
        /// LOOT group from the masterlist.
        #[arg(long)]
        group: Option<String>,
    },
    /// Health check.
    Check,
    /// Deploy, check the script extender matches the game, and start it (through Proton on Linux); --apply to do it.
    /// --without-mods purges instead and starts the game's own exe, as when a game update broke
    /// the script extender (the next launch deploys the mods again).
    Launch {
        #[arg(long)]
        apply: bool,
        #[arg(long)]
        without_mods: bool,
        /// Switch to this profile first (what desktop shortcuts do).
        #[arg(long)]
        profile: Option<String>,
        /// On an error, wait for Enter before closing: a shortcut's window closes with rmm.
        #[arg(long)]
        pause: bool,
    },
    /// A desktop shortcut that switches to a profile (default: the active one), deploys and starts
    /// the game, like MO2's: it runs `rmm launch --profile <p> --apply --pause`. Shows it; --apply
    /// writes it to the Desktop, or --dir.
    Shortcut {
        profile: Option<String>,
        #[arg(long)]
        dir: Option<PathBuf>,
        #[arg(long)]
        apply: bool,
    },
    /// Whether Steam may update the game. --hold makes the game's Steam manifest read-only so it
    /// can't (a copy goes to ini-backup/), --release lets Steam update it again.
    SteamUpdates {
        #[arg(long, conflicts_with = "release")]
        hold: bool,
        #[arg(long)]
        release: bool,
    },
    /// Read a crash log (default: the newest) and rank the mods it points at, with what to try.
    /// --text prints a report to paste into a forum post or chat, without your Windows user name.
    CrashLog {
        path: Option<PathBuf>,
        #[arg(long)]
        text: bool,
    },
    /// Save a support bundle to attach to a forum post or bug report: a summary, the health
    /// check, the lockfile, the deploy plan, the crash, Papyrus and script extender logs and
    /// rmm's settings, without your Windows user name. It goes in --dir (default: support/ in
    /// the instance).
    SupportBundle {
        #[arg(long)]
        dir: Option<PathBuf>,
        /// A crash log (default: the newest).
        #[arg(long)]
        crash: Option<PathBuf>,
    },
    /// Summarise the Papyrus log: errors by message and by script, and its last lines.
    PapyrusLog {
        #[arg(long, default_value_t = 40)]
        tail: usize,
    },
    /// Which script extender plugins (SKSE/F4SE DLLs) loaded in the last game run, and why one
    /// didn't, from skse64.log or f4se.log.
    ScriptExtenderLog,
    /// Deploy, start the game and test it (scripted with the harness plugin, otherwise by
    /// watching the process), then read the logs back to mods; --apply to do it.
    TestRun {
        #[arg(long)]
        apply: bool,
        #[command(flatten)]
        scenario: ScenarioArgs,
    },
    /// Find the mod behind a crash: turn off half of the suspects at a time (with the mods that
    /// need them), play or test after each step and say whether it still crashed, until one is
    /// left. Each step is undone before the next.
    #[command(subcommand)]
    Bisect(BisectCmd),
    /// Show what deploying would change; --apply to do it.
    Deploy {
        #[arg(long)]
        apply: bool,
    },
    /// Show what purging would remove; --apply to do it.
    Purge {
        #[arg(long)]
        apply: bool,
    },
    /// Configure tools (xEdit, BodySlide, Synthesis, or anything else).
    #[command(subcommand)]
    Tool(ToolCmd),
    /// Run a tool and capture its output into a generated mod; --apply to do it.
    Run {
        id: String,
        /// Placeholder value, e.g. --var plugin=Update.esm (repeatable).
        #[arg(long = "var", value_parser = parse_var)]
        vars: Vec<(String, String)>,
        #[arg(long)]
        apply: bool,
        /// Stop the tool after this many seconds.
        #[arg(long)]
        timeout: Option<u64>,
    },
    /// Mod rules; with a mod, replace its rule (no flags removes it).
    ModRule {
        id: Option<String>,
        /// Mod it loads after, i.e. wins conflicts against (repeatable).
        #[arg(long)]
        after: Vec<String>,
        /// Mod that must be enabled with it (repeatable).
        #[arg(long)]
        requires: Vec<String>,
        /// Mod that must not be enabled with it (repeatable).
        #[arg(long)]
        incompatible: Vec<String>,
    },
    /// Reorder mods to satisfy the mod rules; --apply saves the order (undoable).
    SortMods {
        #[arg(long)]
        apply: bool,
    },
    /// Profiles: each has its own mod order, plugin order and undo history.
    #[command(subcommand)]
    Profile(ProfileCmd),
    /// Bring in a setup from MO2 or Vortex (into the active profile; undoable).
    #[command(subcommand)]
    Import(ImportCmd),
    /// Add-ons: sandboxed WebAssembly plugins in the instance's addons/ folder.
    #[command(subcommand)]
    Addon(AddonCmd),
    /// modlist.lock.toml: write the active profile down, or rebuild a setup from one.
    #[command(subcommand)]
    Lock(LockCmd),
    /// Linux: show the profile in the game's Data folder through a FUSE mount instead of hard
    /// links, until Ctrl+C. Writes go to overwrite/. Root files and plugins.txt deploy as usual;
    /// start the game from Steam while it runs, and restart it after changing the profile.
    #[cfg(target_os = "linux")]
    Vfs,
    /// Nexus Mods: API key, update checks, downloads.
    #[command(subcommand)]
    Nexus(NexusCmd),
    /// What the website's "Mod manager download" button runs, once `rmm nexus register-handler`
    /// has been run: hands the nxm:// link to the rmm window for the instance, starting it if it's
    /// closed, where FOMOD installers show their options. Without the window program beside rmm,
    /// it downloads and installs the file here (`rmm nexus install` always does).
    Nxm { link: String },
    Undo,
    Redo,
    History,
    /// rmm's own AI with your API key: OpenRouter, Claude, OpenAI (GPT), Kimi, DeepSeek or Qwen.
    #[command(subcommand)]
    Ai(AiCmd),
    /// Serve the Model Context Protocol on stdio for AI clients.
    Mcp {
        /// Serve over HTTP on this address instead (e.g. 127.0.0.1:47823), at /mcp. Clients
        /// send `Authorization: Bearer <token>`; the token is printed on start.
        #[arg(long)]
        http: Option<String>,
    },
}

#[derive(Subcommand)]
enum AiCmd {
    /// The providers, each with its model, server and whether a key is there.
    Status,
    /// Use this provider from now on. --model and --server change its model or API address
    /// ("" puts back the default).
    Use {
        provider: String,
        #[arg(long)]
        model: Option<String>,
        /// Another address for its API, such as Qwen's China region or a proxy.
        #[arg(long)]
        server: Option<String>,
    },
    /// Save a provider's API key, after checking it with the provider. Reads it from stdin, so it
    /// stays out of your shell history.
    Key { provider: String },
    /// Delete a provider's saved key.
    Forget { provider: String },
    /// The models the key can use (default: the provider in use).
    Models { provider: Option<String> },
    /// A provider's request settings; each one given replaces the saved one ("" clears it).
    Options {
        provider: String,
        /// Added to rmm's own instructions for the AI.
        #[arg(long)]
        instructions: Option<String>,
        /// Claude: low, medium, high (default), xhigh or max. Others: their reasoning effort.
        #[arg(long)]
        effort: Option<String>,
        /// 0 to 2 (not Claude).
        #[arg(long)]
        temperature: Option<String>,
        #[arg(long)]
        max_tokens: Option<String>,
    },
    /// Who does each mod team role. With a role (lead, researcher, installer, tester) and a
    /// provider, give it that provider (--model for another than its own), `mcp` for the AI app
    /// connected over MCP, or "" for the provider in use.
    Team {
        role: Option<String>,
        provider: Option<String>,
        #[arg(long)]
        model: Option<String>,
    },
    /// This instance's saved chats, newest first.
    Chats,
    /// Chat with the AI about this instance, through rmm's tools. Reads and dry runs run straight
    /// away; each change waits for your y (yes), n (no) or a (yes to all in this chat). After each
    /// answer, type the next message; an empty line ends.
    Chat {
        /// The first message; without it, the first line you type.
        words: Vec<String>,
        /// A mod team role for the first message (lead, researcher, installer, tester): it goes to
        /// the AI set for that role (`rmm ai team`) unless --provider says otherwise.
        #[arg(long)]
        role: Option<String>,
        /// A picture (PNG, JPEG, GIF, WebP) to send with the first message; repeat for more.
        #[arg(long)]
        image: Vec<PathBuf>,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        model: Option<String>,
        /// Run changes without asking.
        #[arg(long)]
        yes: bool,
        /// Go on with a saved chat (an id from `rmm ai chats`).
        #[arg(long)]
        resume: Option<String>,
    },
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)] // parsed once per process
enum ToolCmd {
    /// Configured tools and the available presets.
    List,
    /// Add or replace a tool. An id naming a preset (xedit, xedit-qac, bodyslide, synthesis) uses it.
    Add {
        id: String,
        #[arg(long)]
        exe: PathBuf,
        #[arg(long)]
        preset: Option<String>,
        #[arg(long)]
        name: Option<String>,
        /// Default placeholder value, e.g. --var groups=CBBE (repeatable).
        #[arg(long = "var", value_parser = parse_var)]
        vars: Vec<(String, String)>,
        /// Data-relative glob of files the tool rewrites in place (repeatable).
        #[arg(long = "rewrites")]
        rewrites: Vec<String>,
        /// Name of the generated mod.
        #[arg(long)]
        output_mod: Option<String>,
        /// Glob of files from the previous output copied into {output} before a run (repeatable).
        #[arg(long)]
        keep: Vec<String>,
        /// Program (relative to the game folder) a launcher starts; the run waits for it too.
        #[arg(long)]
        wait_for: Option<String>,
        /// xEdit-style: files the tool changes go back to the mods they come from, and new files
        /// into the output mod (an installed mod's id or name, or a new one), which stays enabled.
        /// The xedit preset does this; --edit-in-place=false turns it off.
        #[arg(long, num_args = 0..=1, default_missing_value = "true")]
        edit_in_place: Option<bool>,
        /// Arguments after --, replacing the preset's. Placeholders: {data} {game} {plugins} {ini}
        /// {ini_dir} {output} {state} and any --var.
        #[arg(last = true)]
        args: Vec<String>,
    },
    Remove { id: String },
}

#[derive(Subcommand)]
enum SeparatorCmd {
    /// Add a separator at a priority (default: the end).
    Add {
        name: String,
        #[arg(long)]
        at: Option<usize>,
    },
    /// Rename a separator and/or set its colour (#rrggbb; "" removes it).
    Edit {
        id: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        color: Option<String>,
    },
}

#[derive(Subcommand)]
enum ImportCmd {
    /// MO2 instances and Vortex state backups in their usual places.
    Sources,
    /// An MO2 instance folder (holding ModOrganizer.ini); --apply to do it. Also brings in the
    /// profile's own INIs and saves, and MO2's Executables as tools.
    Mo2 {
        dir: PathBuf,
        /// MO2 profile (default: the one MO2 has selected).
        #[arg(long, conflicts_with = "all")]
        profile: Option<String>,
        /// Every MO2 profile, each into the rmm profile of its name (made when missing).
        #[arg(long)]
        all: bool,
        #[arg(long)]
        apply: bool,
    },
    /// Vortex's live state (%APPDATA%/Vortex/state.v2; close Vortex first) or a state backup
    /// (%APPDATA%/Vortex/temp/state_backups_full/*.json); --apply to do it.
    Vortex {
        backup: PathBuf,
        /// Vortex's staging folder (default: the one the backup names).
        #[arg(long)]
        staging: Option<PathBuf>,
        /// Vortex profile name or id (default: the last active one).
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Subcommand)]
enum NexusCmd {
    /// Save your API key (nexusmods.com > Site preferences > API Keys). Reads it from stdin, so
    /// it stays out of your shell history. --sso gets it by approving rmm on nexusmods.com instead
    /// (once Nexus has registered rmm).
    Login {
        #[arg(long)]
        sso: bool,
    },
    /// Forget the saved API key.
    Logout,
    /// Whether a key is saved, and whose it is.
    Status,
    /// Mods with a newer file on Nexus (default: all with Nexus ids), leaving out held ones.
    Updates { ids: Vec<String> },
    /// Hold back mods' updates: keep them at their versions (by default), --skip the newer file
    /// the last check found (a later one is offered), or --release to offer updates again.
    Hold {
        #[arg(required = true)]
        ids: Vec<String>,
        #[arg(long, conflicts_with = "release")]
        skip: bool,
        #[arg(long)]
        release: bool,
    },
    /// Look downloads up on Nexus by their MD5 (default: every one not identified yet), so
    /// installs from them, and the mods already made from them, get their Nexus ids for update
    /// checks.
    Identify { names: Vec<String> },
    /// Requirements the enabled mods' Nexus pages list that aren't installed, or are but are off
    /// (and DLC that isn't there). --install installs the ones on Nexus (Premium: each one's newest
    /// main file, installer defaults) and turns on the others, in one step.
    Requirements {
        #[arg(long)]
        install: bool,
    },
    /// Search the game's mods on Nexus by name and description (no key needed).
    Search {
        words: Vec<String>,
        /// A Nexus category, such as Armour or Patches.
        #[arg(long)]
        category: Option<String>,
        /// relevance, endorsements, downloads, updated or created.
        #[arg(long)]
        sort: Option<String>,
        #[arg(long)]
        count: Option<u32>,
        #[arg(long)]
        offset: Option<u32>,
        /// Include adult-only mods.
        #[arg(long)]
        adult: bool,
    },
    /// A mod's page: description, tags, what it needs and which mods need it.
    Info { mod_id: u64 },
    /// Download and install a file by ids (Premium accounts) or nxm link.
    Install {
        /// An nxm:// link, a mod page link, a mod id (its newest main file) or <mod id>/<file id>.
        /// All but nxm links need Premium.
        what: String,
    },
    /// Install a collection (Premium accounts): its mods with the curator's installer choices,
    /// its mod rules, plugin list and tools. Shows what it would install; --apply installs. In a
    /// profile with an earlier revision it updates: newer files in, dropped mods off, your own
    /// mods and the ones you turned off as they are.
    Collection {
        /// The collection's link or id; default: the active profile's collection.
        link: Option<String>,
        /// A revision number (default: the newest).
        #[arg(long)]
        revision: Option<u64>,
        /// Its optional mods too.
        #[arg(long)]
        optional: bool,
        /// Install into a new profile with this name, which becomes the active one (the mods
        /// already installed are in it, turned off). Default: the active profile.
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        apply: bool,
    },
    /// Make nxm:// links open `rmm nxm` for this instance. Changes the system's handler for
    /// nxm links (HKCU\Software\Classes\nxm on Windows, xdg-mime on Linux).
    RegisterHandler,
}

#[derive(Subcommand)]
enum AddonCmd {
    /// Installed add-ons: what they may do, their hooks, tools and panels.
    List,
    /// Install an add-on folder (addon.toml + addon.wasm). Shows what it asks for; --apply installs.
    Install {
        dir: PathBuf,
        #[arg(long)]
        apply: bool,
    },
    Remove { name: String },
    /// Call one of an add-on's tools or panels with JSON input.
    Call {
        name: String,
        function: String,
        #[arg(default_value = "{}")]
        input: String,
    },
}

/// What a test run does (`rmm test-run`, `rmm bisect run`).
#[derive(clap::Args)]
struct ScenarioArgs {
    /// Save to load (name without extension). Harness only.
    #[arg(long)]
    save: Option<String>,
    /// Cell to visit with coc (repeatable); without --save the first one starts the game.
    /// Harness only.
    #[arg(long = "cell")]
    cells: Vec<String>,
    /// Seconds in each cell (default 10).
    #[arg(long)]
    dwell: Option<u64>,
    /// Seconds to wait for the main menu and each cell, or to keep running without the harness.
    #[arg(long)]
    timeout: Option<u64>,
    /// Leave the game running afterwards.
    #[arg(long)]
    keep_open: bool,
    /// The game's window: WxH (default 960x540, kept behind other windows) or fullscreen.
    #[arg(long)]
    window: Option<String>,
    /// Save a PNG of the game's window in each cell. Harness only.
    #[arg(long)]
    screenshots: bool,
    /// Steps after the save and the cells, as JSON or @file, such as
    /// [{"console": "tgm"}, {"wait": 10}, {"screenshot": "after"}]. Harness only.
    #[arg(long)]
    steps: Option<String>,
}

impl ScenarioArgs {
    fn scenario(self) -> Result<harness::Scenario> {
        let ScenarioArgs { save, cells, dwell, timeout, keep_open, window, screenshots, steps } = self;
        let steps = match steps {
            Some(arg) => serde_json::from_str(&json_arg(&arg)?).context("--steps is a JSON list of steps, such as [{\"cell\": \"Riverwood\"}, {\"wait\": 10}]")?,
            None => vec![],
        };
        Ok(harness::Scenario { save, cells, dwell, timeout, keep_open, window, screenshots, steps })
    }
}

#[derive(Subcommand)]
enum BisectCmd {
    /// Start: turn off half of the mods the newest crash log names, the mods given, or with
    /// --all every enabled mod but the script extender and the test harness.
    Start {
        mods: Vec<String>,
        #[arg(long)]
        all: bool,
    },
    /// The game still crashed with this step's mods off.
    Crashed,
    /// The game didn't crash with them off.
    Ok,
    /// Play every step with a test run until the bisect ends; --apply to do it. A failed run
    /// counts as a crash.
    Run {
        #[arg(long)]
        apply: bool,
        #[command(flatten)]
        scenario: ScenarioArgs,
    },
    /// Where the bisect is: the mods off now, the suspects left, the steps so far.
    Status,
    /// End it and turn this step's mods back on.
    Stop,
}

#[derive(Subcommand)]
enum LockCmd {
    /// Write the active profile to modlist.lock.toml (or --out). --pack writes a zip of it with the
    /// files of mods no archive brings back (patches you made, mods made from Overwrite or a
    /// folder); --markdown a readable list for a post.
    Export {
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long, conflicts_with = "markdown")]
        pack: bool,
        #[arg(long)]
        markdown: bool,
    },
    /// Install what a lockfile (or pack) needs from archives in --downloads and the pack, list
    /// what is missing, and set the order; --apply to do it. Mods you added that it doesn't list
    /// are kept. Warns when the game or script extender version differs from the lockfile's.
    Apply {
        file: PathBuf,
        #[arg(long)]
        downloads: Vec<PathBuf>,
        #[arg(long)]
        apply: bool,
    },
    /// What changed from a lockfile to the active profile, or to a second lockfile: versions, mods
    /// and plugins added, removed, changed, turned on or off, and moved.
    Diff { file: PathBuf, other: Option<PathBuf> },
}

#[derive(Subcommand)]
enum ProfileCmd {
    /// Profiles and the active one.
    List,
    /// New profile, copied from the active one (--empty: every mod disabled).
    Create {
        name: String,
        #[arg(long)]
        empty: bool,
    },
    /// Make a profile active and show the deploy that would apply it.
    Switch { name: String },
    /// A new playthrough: a copy of the active profile with its own saves (Saves/<name>/), made
    /// active. Shows the deploy that would apply it.
    Playthrough { name: String },
    Rename { from: String, to: String },
    /// Delete a profile other than the active one (kept in profiles/.deleted/).
    Delete { name: String },
    /// The active profile's own INIs (profiles/<name>/ini/) and saves (Saves/<name>/), from the
    /// next deploy. Without flags: shows them.
    Options {
        #[arg(long)]
        local_inis: Option<bool>,
        #[arg(long)]
        local_saves: Option<bool>,
    },
}

fn main() -> Result<()> {
    // The command match is one big stack frame: unoptimised builds overflow Windows's 1 MB main stack.
    std::thread::Builder::new().stack_size(16 << 20).spawn(run)?.join().unwrap_or_else(|e| std::panic::resume_unwind(e))
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let open = || -> Result<Instance> {
        let dir = match &cli.instance {
            Some(d) => d.clone(),
            None => std::env::current_dir()?,
        };
        Instance::open(&dir)
    };
    match cli.cmd {
        Cmd::Detect { search: false } => print(&Game::ALL.iter().filter_map(|g| game::detect(*g)).collect::<Vec<_>>()),
        Cmd::Detect { search: true } => print(&game::search_drives()),
        Cmd::Init { dir, game: g, addon, game_path, local_path } => {
            if let Some(a) = &addon {
                addons::games_in(a)?;
            }
            let g: Game = g.parse().map_err(|e: String| anyhow::anyhow!(e))?;
            let found = game::detect(g);
            let game_path = game_path
                .or_else(|| found.as_ref().map(|d| d.game_path.clone()))
                .context("game not found; pass --game-path")?;
            let local_path = local_path
                .or_else(|| game::local_path(g, &game_path))
                .context("cannot work out where plugins.txt lives; pass --local-path")?;
            let inst = Instance::create(&dir, g, &game_path, &local_path)?;
            if let Some(a) = &addon {
                addons::install(&inst, a, false)?;
            }
            print(&ops::instance_info(&inst)?)
        }
        Cmd::Info => {
            let inst = open()?;
            let mut v = ops::instance_info(&inst)?;
            v["essentials"] = serde_json::to_value(rmm_core::launch::essentials(&inst)?)?;
            print(&v)
        }
        Cmd::Mods => print(&ops::list_mods(&open()?)?),
        Cmd::Show { id } => print(&ops::mod_details(&open()?, &id, usize::MAX)?),
        Cmd::Install { path, name, at, disabled, choices } => {
            let choices = choices.as_deref().map(parse_choices).transpose()?;
            print(&install::install_at(&open()?, &path, name.as_deref(), !disabled, choices.as_ref(), at)?)
        }
        Cmd::Fomod { path, choices } => {
            let choices = choices.as_deref().map(parse_choices).transpose()?;
            print(&install::fomod_options(&open()?, &path, choices.as_ref())?)
        }
        Cmd::Reinstall { id, choices } => {
            let choices = choices.as_deref().map(parse_choices).transpose()?;
            print(&install::reinstall(&open()?, &id, choices.as_ref())?)
        }
        Cmd::Update { id, path, choices } => {
            let choices = choices.as_deref().map(parse_choices).transpose()?;
            print(&install::update(&open()?, &id, &path, choices.as_ref())?)
        }
        Cmd::Rename { id, name } => print(&serde_json::json!({ "previous": ops::rename_mod(&open()?, &id, &name)? })),
        Cmd::Uninstall { ids } => done(install::uninstall_mods(&open()?, &ids)),
        Cmd::Enable { ids } => done(ops::set_mods_enabled(&open()?, &ids, true)),
        Cmd::Disable { ids } => done(ops::set_mods_enabled(&open()?, &ids, false)),
        Cmd::Move { ids, to } => done(ops::move_mods(&open()?, &ids, to)),
        Cmd::Separator(SeparatorCmd::Add { name, at }) => {
            print(&serde_json::json!({ "id": ops::add_separator(&open()?, &name, at)? }))
        }
        Cmd::Separator(SeparatorCmd::Edit { id, name, color }) => {
            print(&serde_json::json!({ "id": ops::edit_separator(&open()?, &id, name.as_deref(), color.as_deref())? }))
        }
        Cmd::Conflicts { id: None } => {
            let inst = open()?;
            print(&conflicts::conflicts(&inst, &inst.profile()?)?)
        }
        Cmd::Conflicts { id: Some(id) } => {
            let inst = open()?;
            print(&conflicts::file_conflicts(&inst, &inst.profile()?, &id)?)
        }
        Cmd::Data { dir, find, conflicts, archives } => {
            let inst = open()?;
            let q = conflicts::DataQuery { dir: dir.unwrap_or_default(), find: find.unwrap_or_default(), conflicts_only: conflicts, archives };
            print(&conflicts::data_view(&inst, &inst.profile()?, &q)?)
        }
        Cmd::Saves { fix: Some(file), .. } => print(&serde_json::json!({ "enabled": saves::enable_missing(&open()?, &file)? })),
        Cmd::Saves { copy_to: Some(to), files, .. } => print(&saves::transfer(&open()?, &files, &to, true)?),
        Cmd::Saves { move_to: Some(to), files, .. } => print(&saves::transfer(&open()?, &files, &to, false)?),
        Cmd::Saves { .. } => print(&saves::list(&open()?)?),
        Cmd::Downloads { delete: Some(name), .. } => done(install::delete_download(&open()?, &name)),
        Cmd::Downloads { folder: None, .. } => print(&install::downloads(&open()?)?),
        Cmd::Downloads { folder: Some(f), .. } => {
            let mut inst = open()?;
            inst.set_downloads_dir((!f.is_empty()).then(|| PathBuf::from(&f)))?;
            print(&serde_json::json!({ "downloads": inst.downloads_dir() }))
        }
        Cmd::Settings { key, value, reset } => {
            if reset {
                print(&settings::reset(key.as_deref())?)
            } else if let (Some(k), Some(v)) = (&key, &value) {
                print(&settings::set_one(k, v)?)
            } else {
                let all = serde_json::to_value(settings::get())?;
                match key {
                    Some(k) => print(k.split('.').try_fold(&all, |v, p| v.get(p)).with_context(|| format!("no setting {k}"))?),
                    None => print(&all),
                }
            }
        }
        Cmd::Overwrite { to_mod: Some(name), .. } => print(&install::mod_from_overwrite(&open()?, &name)?),
        Cmd::Overwrite { clear: true, .. } => print(&serde_json::json!({ "cleared": install::clear_overwrite(&open()?)? })),
        Cmd::Overwrite { .. } => print(&open()?.mod_files(rmm_core::instance::OVERWRITE)?),
        Cmd::CleanPlugins { apply } => print(&rmm_core::tools::clean_dirty(&open()?, !apply)?),
        Cmd::Hide { id, paths, show } => print(&install::hide_files(&open()?, &id, &paths, !show)?),
        Cmd::MoveFiles { from, to, paths, replace } => {
            let ow = |s: String| if s.eq_ignore_ascii_case("overwrite") { rmm_core::instance::OVERWRITE.to_string() } else { s };
            print(&install::move_files(&open()?, &ow(from), &paths, &ow(to), replace)?)
        }
        Cmd::Creations { restore: false, apply } => print(&rmm_core::import::adopt_creations(&open()?, !apply)?),
        Cmd::Creations { restore: true, apply } => print(&rmm_core::import::restore_creations(&open()?, !apply)?),
        Cmd::Edits { revert: Some(n) } => print(&rmm_core::workspace::revert(&open()?, n)?),
        Cmd::Edits { revert: None } => print(&rmm_core::workspace::edits(&open()?)?),
        Cmd::Trash { empty: false } => print(&install::trash(&open()?)?),
        Cmd::Trash { empty: true } => print(&install::empty_trash(&open()?)?),
        Cmd::Note { id, text } => print(&serde_json::json!({ "previous": ops::set_mod_note(&open()?, &id, &text)? })),
        Cmd::Category { ids, add, remove } => print(&ops::edit_categories(&open()?, &ids, &add, &remove)?),
        Cmd::Color { id, color } => done(ops::set_color(&open()?, &id, &color)),
        Cmd::LockPlugin { name, unlock } => done(ops::lock_plugin(&open()?, &name, !unlock)),
        Cmd::Restore { n, apply: false } => print(&open()?.restore_preview(n)?),
        Cmd::Restore { n, apply: true } => print(&serde_json::json!({ "undone": open()?.restore(n)? })),
        Cmd::PluginConflicts { plugin } => {
            let inst = open()?;
            print(&plugins::record_overlaps(&inst, &inst.profile()?, plugin.as_deref())?)
        }
        Cmd::Scan { id } => print(&inspect::scan(&open()?, id.as_deref())?),
        Cmd::Records { plugin, kinds, max } => {
            let inst = open()?;
            print(&plugins::new_records(&inst, &inst.profile()?, &plugin, &kinds, max)?)
        }
        Cmd::Review { id, verdict, notes, load_order } => print(&inspect::save_review(&open()?, &id, &verdict, &notes, &load_order)?),
        Cmd::Reviews { id } => print(&inspect::reviews(&open()?, id.as_deref())?),
        Cmd::Read { id, path, offset, limit } => {
            let v = inspect::read_file(&open()?, &id, &path, offset, limit)?;
            match &v.text {
                Some(text) => {
                    println!("{text}");
                    Ok(())
                }
                None => print(&v),
            }
        }
        Cmd::Plugins { enable, disable, moving, after, before } => {
            let inst = open()?;
            for (names, on) in [(enable, true), (disable, false)] {
                ops::set_plugins_enabled(&inst, &names, on)?;
            }
            if let Some(m) = moving {
                let all = inst.profile()?.plugins;
                let m = all.iter().find(|p| p.name.eq_ignore_ascii_case(&m)).with_context(|| format!("no plugin {m} in the profile"))?.name.clone();
                let mut order: Vec<String> = all.into_iter().map(|p| p.name).filter(|n| *n != m).collect();
                let (other, past) = after.map(|a| (a, 1)).or(before.map(|b| (b, 0))).context("--move needs --after or --before")?;
                let at = order.iter().position(|n| n.eq_ignore_ascii_case(&other)).with_context(|| format!("no plugin {other} in the profile"))?;
                order.insert(at + past, m);
                ops::set_plugin_order(&inst, &order)?;
            }
            print(&plugins::list(&inst, &inst.profile()?)?)
        }
        Cmd::Sort { apply } => print(&loot::sort(&open()?, !apply)?),
        Cmd::Masterlist => print(&loot::update_masterlist(&open()?, true)?),
        Cmd::Rule { plugin: None, .. } => print(&loot::rules(&open()?)?),
        Cmd::Rule { plugin: Some(plugin), after, group } => print(&serde_json::json!({ "previous": loot::set_rule(&open()?, &loot::Rule { plugin, after, group })? })),
        Cmd::Check => print(&health::check(&open()?)?),
        Cmd::CrashLog { path, text } => {
            let inst = open()?;
            let r = logs::read_crash_log(&inst, path.as_deref())?;
            if !text {
                return print(&r);
            }
            print!("{}", rmm_core::support::crash_text(&inst, &r));
            Ok(())
        }
        Cmd::SupportBundle { dir, crash } => print(&rmm_core::support::bundle(&open()?, dir.as_deref(), crash.as_deref())?),
        Cmd::PapyrusLog { tail } => print(&logs::read_papyrus_log(&open()?, tail)?),
        Cmd::ScriptExtenderLog => print(&logs::script_extender_log(&open()?, true)?),
        Cmd::TestRun { apply: false, .. } => print(&harness::preview(&open()?)?),
        Cmd::TestRun { apply: true, scenario } => print(&harness::test_run(&open()?, &scenario.scenario()?)?),
        Cmd::Bisect(BisectCmd::Start { mods, all }) => {
            let inst = open()?;
            let mods = if mods.is_empty() { bisect::suspects(&inst, all)? } else { mods };
            print(&bisect::start(&inst, &mods)?)
        }
        Cmd::Bisect(BisectCmd::Crashed) => print(&bisect::mark(&open()?, true)?),
        Cmd::Bisect(BisectCmd::Ok) => print(&bisect::mark(&open()?, false)?),
        Cmd::Bisect(BisectCmd::Run { apply: false, .. }) => print(&harness::preview(&open()?)?),
        Cmd::Bisect(BisectCmd::Run { apply: true, scenario }) => {
            let s = scenario.scenario()?;
            print(&bisect::run(&open()?, |i| harness::test_run(i, &s).map(|r| (r.passed, r.summary)))?)
        }
        Cmd::Bisect(BisectCmd::Status) => print(&bisect::status(&open()?)?),
        Cmd::Bisect(BisectCmd::Stop) => print(&bisect::stop(&open()?)?),
        Cmd::Deploy { apply } => print(&deploy::deploy(&open()?, !apply)?),
        Cmd::Launch { apply, without_mods, profile, pause } => {
            let run = || -> Result<rmm_core::launch::LaunchReport> {
                let mut inst = open()?;
                if let Some(p) = &profile {
                    // A dry run looks at the profile without switching to it.
                    if apply { inst.switch_profile(p)? } else { inst.config.profile = inst.find_profile(p)? }
                }
                if without_mods { rmm_core::launch::launch_vanilla(&inst, !apply) } else { rmm_core::launch::launch(&inst, !apply) }
            };
            match run() {
                Err(e) if pause => {
                    eprintln!("rmm couldn't start the game: {e:#}\n\nPress Enter to close this window.");
                    let _ = std::io::stdin().read_line(&mut String::new());
                    Err(e)
                }
                r => print(&r?),
            }
        }
        Cmd::Shortcut { profile, dir, apply } => {
            let inst = open()?;
            let p = profile.unwrap_or_else(|| inst.config.profile.clone());
            print(&rmm_core::launch::shortcut(&inst, &p, &std::env::current_exe()?, dir.as_deref(), !apply)?)
        }
        Cmd::SteamUpdates { hold: false, release: false } => print(&rmm_core::launch::steam_updates(&open()?)),
        Cmd::SteamUpdates { hold, .. } => print(&rmm_core::launch::hold_steam_updates(&open()?, hold)?),
        Cmd::Purge { apply } => print(&deploy::purge(&open()?, !apply)?),
        Cmd::Tool(ToolCmd::List) => print(&serde_json::json!({ "tools": tools::list(&open()?)?, "presets": tools::PRESETS })),
        Cmd::Tool(ToolCmd::Add { id, exe, preset, name, vars, rewrites, output_mod, keep, wait_for, edit_in_place, args }) => {
            let spec = tools::ToolSpec {
                exe,
                preset,
                name,
                args: (!args.is_empty()).then_some(args),
                vars: vars.into_iter().collect(),
                rewrites: (!rewrites.is_empty()).then_some(rewrites),
                output_mod,
                keep: (!keep.is_empty()).then_some(keep),
                settings: None, // JSON settings: presets, MCP, or tools.toml
                wait_for,
                edit_in_place,
            };
            print(&tools::add(&open()?, &id, spec)?)
        }
        Cmd::Tool(ToolCmd::Remove { id }) => done(tools::remove(&open()?, &id)),
        Cmd::Run { id, vars, apply, timeout } => {
            let vars = vars.into_iter().collect();
            print(&tools::run(&open()?, &id, &vars, !apply, timeout.map(std::time::Duration::from_secs))?)
        }
        Cmd::ModRule { id: None, .. } => print(&rules::rules(&open()?)?),
        Cmd::ModRule { id: Some(id), after, requires, incompatible } => {
            print(&serde_json::json!({ "previous": rules::set_rule(&open()?, &rules::ModRule { id, after, requires, incompatible })? }))
        }
        Cmd::SortMods { apply } => print(&rules::sort(&open()?, !apply)?),
        Cmd::Profile(ProfileCmd::List) => {
            let inst = open()?;
            print(&serde_json::json!({ "active": inst.config.profile, "profiles": inst.profiles()? }))
        }
        Cmd::Profile(ProfileCmd::Create { name, empty }) => done(open()?.create_profile(&name, !empty)),
        Cmd::Profile(ProfileCmd::Switch { name }) => {
            let mut inst = open()?;
            inst.switch_profile(&name)?;
            print(&deploy::deploy(&inst, true)?)
        }
        Cmd::Profile(ProfileCmd::Playthrough { name }) => {
            let mut inst = open()?;
            ops::new_playthrough(&mut inst, &name)?;
            print(&deploy::deploy(&inst, true)?)
        }
        Cmd::Profile(ProfileCmd::Rename { from, to }) => done(open()?.rename_profile(&from, &to)),
        Cmd::Profile(ProfileCmd::Delete { name }) => done(open()?.delete_profile(&name)),
        Cmd::Profile(ProfileCmd::Options { local_inis, local_saves }) => {
            let inst = open()?;
            ops::set_profile_options(&inst, local_inis, local_saves)?;
            let p = inst.profile()?;
            print(&serde_json::json!({ "profile": inst.config.profile, "local_inis": p.local_inis, "local_saves": p.local_saves }))
        }
        Cmd::Import(ImportCmd::Sources) => print(&import::find_sources()),
        Cmd::Import(ImportCmd::Mo2 { dir, all: true, apply, .. }) => print(&import::import_mo2_all(&open()?, &dir, !apply)?),
        Cmd::Import(ImportCmd::Mo2 { dir, profile, apply, .. }) => print(&import::import_mo2(&open()?, &dir, profile.as_deref(), !apply)?),
        Cmd::Import(ImportCmd::Vortex { backup, staging, profile, apply }) => {
            print(&import::import_vortex(&open()?, &backup, staging.as_deref(), profile.as_deref(), !apply)?)
        }
        Cmd::Addon(AddonCmd::List) => print(&addons::list(&open()?)?),
        Cmd::Addon(AddonCmd::Install { dir, apply }) => print(&addons::install(&open()?, &dir, !apply)?),
        Cmd::Addon(AddonCmd::Remove { name }) => print(&addons::remove(&open()?, &name)?),
        Cmd::Addon(AddonCmd::Call { name, function, input }) => {
            print(&addons::call(&open()?, &name, &function, &serde_json::from_str(&input).context("the input is not JSON")?)?)
        }
        Cmd::Lock(LockCmd::Export { out, pack, markdown }) => {
            let format = if pack { lockfile::Format::Pack } else if markdown { lockfile::Format::Markdown } else { lockfile::Format::Toml };
            print(&lockfile::write(&open()?, out.as_deref(), format)?)
        }
        Cmd::Lock(LockCmd::Diff { file, other }) => {
            let old = lockfile::read(&file)?;
            let new = match other {
                Some(o) => lockfile::read(&o)?,
                None => lockfile::export(&open()?)?,
            };
            print(&lockfile::diff(&old, &new))
        }
        Cmd::Lock(LockCmd::Apply { file, downloads, apply }) => print(&lockfile::apply(&open()?, &lockfile::read(&file)?, &downloads, !apply)?),
        Cmd::Nexus(NexusCmd::Login { sso: true }) => print(&nexus::sso(&|url| {
            eprintln!("Approve rmm on Nexus in your browser: {url}");
            nexus::open_in_browser(url)
        })?),
        Cmd::Nexus(NexusCmd::Login { sso: false }) => {
            eprint!("Paste your Nexus API key: ");
            let mut key = String::new();
            std::io::stdin().read_line(&mut key)?;
            print(&nexus::set_key(&key)?)
        }
        Cmd::Nexus(NexusCmd::Logout) => print(&serde_json::json!({ "forgot": nexus::forget_key()? })),
        Cmd::Nexus(NexusCmd::Status) => print(&nexus::status()),
        Cmd::Nexus(NexusCmd::Search { words, category, sort, count, offset, adult }) => {
            print(&nexus::search(&open()?, &nexus::Client::any(), &nexus::Search { query: words.join(" "), category, sort, count, offset, adult })?)
        }
        Cmd::Nexus(NexusCmd::Requirements { install: false }) => {
            let inst = open()?;
            print(&nexus::lacking_requirements(&inst, &inst.profile()?))
        }
        Cmd::Nexus(NexusCmd::Requirements { install: true }) => print(&nexus::install_lacking(&open()?, &nexus::Client::from_env()?)?),
        Cmd::Nexus(NexusCmd::Identify { names }) => print(&nexus::identify(&open()?, &nexus::Client::from_env()?, &names)?),
        Cmd::Nexus(NexusCmd::Info { mod_id }) => print(&nexus::mod_info(&open()?, &nexus::Client::any(), mod_id)?),
        Cmd::Nexus(NexusCmd::Updates { ids }) => {
            print(&nexus::check_updates(&open()?, &nexus::Client::from_env()?, (!ids.is_empty()).then_some(ids.as_slice()))?)
        }
        Cmd::Nexus(NexusCmd::Hold { ids, skip, release }) => {
            let hold = if release { nexus::Hold::Release } else if skip { nexus::Hold::Skip } else { nexus::Hold::Pin };
            nexus::hold_updates(&open()?, &ids, hold)
        }
        Cmd::Nxm { link } => {
            let inst = open()?;
            let app = std::env::current_exe()?.with_file_name(if cfg!(windows) { "rmm-app.exe" } else { "rmm-app" });
            match nexus::hand_to_window(&inst.root, &link, &app)? {
                nexus::Handed::ToWindow => println!("Sent to the rmm window for {}.", inst.root.display()),
                nexus::Handed::OpenedWindow => println!("Opening the rmm window for {}; it installs the file.", inst.root.display()),
                nexus::Handed::NoWindow => {
                    let client = nexus::Client::from_env()?;
                    print(&nexus::install(&inst, &client, &nexus::parse_nxm(&link)?, None)?)?
                }
            }
            Ok(())
        }
        Cmd::Nexus(NexusCmd::Install { what }) => {
            let inst = open()?;
            let client = nexus::Client::from_env()?;
            let link = nexus::resolve(&inst, &client, &what)?;
            print(&nexus::install(&inst, &client, &link, None)?)
        }
        Cmd::Nexus(NexusCmd::Collection { link, revision, optional, profile, apply }) => {
            print(&rmm_core::collection::install(&mut open()?, &nexus::Client::from_env()?, &link.unwrap_or_default(), revision, optional, profile.as_deref(), !apply)?)
        }
        Cmd::Nexus(NexusCmd::RegisterHandler) => {
            let inst = open()?;
            println!("{}", nexus::register_handler(&std::env::current_exe()?, &inst.root)?);
            Ok(())
        }
        #[cfg(target_os = "linux")]
        Cmd::Vfs => rmm_core::vfs::serve(&open()?, |data| eprintln!("Mounted the profile over {}. Ctrl+C unmounts.", data.display())),
        Cmd::Ai(AiCmd::Status) => print(&agent::status()),
        Cmd::Ai(AiCmd::Use { provider, model, server }) => print(&agent::configure(&provider, model.as_deref(), server.as_deref(), true)?),
        Cmd::Ai(AiCmd::Key { provider }) => {
            let p = agent::provider(&provider)?;
            eprint!("Paste your {} API key: ", p.name);
            let mut key = String::new();
            std::io::stdin().read_line(&mut key)?;
            let n = agent::set_key(p.id, &key)?;
            eprintln!("Saved. {} took the key ({n} models).", p.name);
            Ok(())
        }
        Cmd::Ai(AiCmd::Forget { provider }) => print(&serde_json::json!({ "forgot": agent::forget_key(&provider)? })),
        Cmd::Ai(AiCmd::Models { provider }) => print(&agent::Conn::open(provider.as_deref(), None)?.models()?),
        Cmd::Ai(AiCmd::Options { provider, instructions, effort, temperature, max_tokens }) => {
            let id = agent::provider(&provider)?.id;
            let mut o = agent::status().into_iter().find(|s| s.id == id).map(|s| s.options).unwrap_or_default();
            let num = |v: &str| -> Result<Option<f64>> { Ok(if v.trim().is_empty() { None } else { Some(v.trim().parse().context("not a number")?) }) };
            if let Some(v) = instructions {
                o.instructions = v;
            }
            if let Some(v) = effort {
                o.effort = Some(v);
            }
            if let Some(v) = temperature {
                o.temperature = num(&v)?;
            }
            if let Some(v) = max_tokens {
                o.max_tokens = num(&v)?.map(|n| n as u32);
            }
            print(&agent::set_options(id, o)?.options)
        }
        Cmd::Ai(AiCmd::Team { role, provider, model }) => match (role, provider) {
            (Some(r), Some(p)) => {
                agent::set_seat(&r, &p, model.as_deref().unwrap_or(""))?;
                print(&agent::team())
            }
            (None, None) => print(&agent::team()),
            _ => anyhow::bail!("give a role and a provider (mcp for the AI app connected over MCP, \"\" for the provider in use)"),
        },
        Cmd::Ai(AiCmd::Chats) => print(&agent::chats(&open()?.root)?),
        Cmd::Ai(AiCmd::Chat { words, role, image, provider, model, yes, resume }) => {
            let inst = open()?;
            let saved = resume.map(|id| agent::load_chat(&inst.root, &id)).transpose()?;
            // A role goes to its AI unless the provider is named; the AI app over MCP gets it as a team task.
            let seat = match (&provider, &saved, &role) {
                (None, None, Some(r)) => Some(agent::seat(agent::role(r)?.0)),
                _ => None,
            };
            if seat.as_ref().is_some_and(|s| s.provider == agent::MCP) {
                eprintln!("{}", agent::hand_to_app(&inst.root, role.as_deref().unwrap_or_default(), &words.join(" "))?);
                return Ok(());
            }
            let conn = match seat {
                Some(s) => agent::Conn::for_seat(&s)?,
                None => agent::Conn::open(provider.as_deref().or(saved.as_ref().map(|c| c.provider.as_str())), model.as_deref())?,
            };
            let mut images = image.iter().map(|p| agent::Image::load(p)).collect::<Result<Vec<_>>>()?;
            eprintln!("{} · {}. An empty line ends the chat.", conn.provider.name, conn.model);
            let mut ai = match saved {
                Some(chat) => agent::Agent::resume(conn, inst.root.clone(), chat)?,
                None => agent::Agent::new(conn, inst.root.clone())?,
            };
            ai.trusted = yes;
            let (mut text, mut role) = (words.join(" "), role);
            loop {
                if text.trim().is_empty() {
                    eprint!("you> ");
                    text.clear();
                    if std::io::stdin().read_line(&mut text)? == 0 || text.trim().is_empty() {
                        return Ok(());
                    }
                }
                if let Err(e) = ai.send(role.take().as_deref(), text.trim(), &std::mem::take(&mut images), &mut Term) {
                    eprintln!("error: {e:#}");
                }
                text.clear();
            }
        }
        Cmd::Undo => print(&open()?.undo()?),
        Cmd::Redo => print(&open()?.redo()?),
        Cmd::History => print(&open()?.history_details()?),
        Cmd::Mcp { http: addr } => {
            let dir = match cli.instance {
                Some(d) => d,
                None => std::env::current_dir()?,
            };
            Instance::open(&dir)?; // fail fast on a bad path
            if let Some(addr) = addr {
                let token = http::token()?;
                let server = http::start(dir, &addr, token.clone())?;
                eprintln!("MCP on http://{}/mcp (header Authorization: Bearer {token}). Ctrl+C stops it.", server.addr);
                loop {
                    std::thread::park();
                }
            }
            tokio::runtime::Runtime::new()?.block_on(mcp::serve(dir))
        }
    }
}

/// The chat in a terminal: the AI's words on stdout, its tool calls on stderr.
struct Term;

fn first_line(s: &str, max: usize) -> String {
    let line = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    match line.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line.into(),
    }
}

impl agent::Ui for Term {
    fn event(&mut self, e: agent::Event) {
        match e {
            agent::Event::User { .. } => {}
            agent::Event::Text { text } => println!("{text}\n"),
            agent::Event::Call { name, args } => eprintln!("> {name} {}", first_line(&args.to_string(), 160)),
            agent::Event::Result { ok, text, .. } => eprintln!("  {} {}", if ok { "ok:" } else { "failed:" }, first_line(&text, 160)),
            agent::Event::Notice { text } => eprintln!("! {text}"),
        }
    }

    fn approve(&mut self, name: &str, args: &serde_json::Value) -> agent::Answer {
        eprint!("Run {name} {args}? [y]es, [n]o, [a]ll changes in this chat: ");
        let mut a = String::new();
        let _ = std::io::stdin().read_line(&mut a);
        match a.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => agent::Answer::Yes,
            "a" | "all" => agent::Answer::Always,
            _ => agent::Answer::No,
        }
    }
}

fn print<T: Serialize + ?Sized>(v: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

/// A JSON argument, or `@file` for one in a file.
fn json_arg(arg: &str) -> Result<String> {
    Ok(match arg.strip_prefix('@') {
        Some(file) => std::fs::read_to_string(file).with_context(|| format!("cannot read {file}"))?,
        None => arg.to_owned(),
    })
}

fn parse_choices(arg: &str) -> Result<rmm_core::fomod::Choices> {
    serde_json::from_str(&json_arg(arg)?).context("--choices is not valid FOMOD choices JSON")
}

fn parse_var(s: &str) -> Result<(String, String), String> {
    s.split_once('=').map(|(k, v)| (k.to_owned(), v.to_owned())).ok_or_else(|| format!("expected name=value, got '{s}'"))
}

fn done(r: Result<()>) -> Result<()> {
    r?;
    println!("ok");
    Ok(())
}
