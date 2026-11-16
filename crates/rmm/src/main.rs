use rmm::{http, mcp};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rmm_core::{Game, Instance, addons, conflicts, deploy, game, harness, health, import, install, lockfile, logs, loot, nexus, ops, plugins, rules, tools};
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
    Detect,
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
    /// Install from a folder, .zip or .7z. It goes at the end (before generated tool output) or
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
    /// Set a mod's notes ("" clears them).
    Note { id: String, text: String },
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
    Plugins,
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
    Launch {
        #[arg(long)]
        apply: bool,
    },
    /// Read a crash log (default: the newest) and rank the mods it points at.
    CrashLog { path: Option<PathBuf> },
    /// Summarise the Papyrus log: errors by message and by script, and its last lines.
    PapyrusLog {
        #[arg(long, default_value_t = 40)]
        tail: usize,
    },
    /// Deploy, start the game and test it (scripted with the harness plugin, otherwise by
    /// watching the process), then read the logs back to mods; --apply to do it.
    TestRun {
        #[arg(long)]
        apply: bool,
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
    },
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
    /// Download and install the file an nxm:// link points at (what the website's "Mod manager
    /// download" button opens, once `rmm nexus register-handler` has been run).
    Nxm { link: String },
    Undo,
    Redo,
    History,
    /// Serve the Model Context Protocol on stdio for AI clients.
    Mcp {
        /// Serve over HTTP on this address instead (e.g. 127.0.0.1:47823), at /mcp. Clients
        /// send `Authorization: Bearer <token>`; the token is printed on start.
        #[arg(long)]
        http: Option<String>,
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
    /// An MO2 instance folder (holding ModOrganizer.ini); --apply to do it.
    Mo2 {
        dir: PathBuf,
        /// MO2 profile (default: the one MO2 has selected).
        #[arg(long)]
        profile: Option<String>,
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
    /// Mods with a newer file on Nexus (default: all with Nexus ids).
    Updates { ids: Vec<String> },
    /// Download and install a file by ids (Premium accounts) or nxm link.
    Install {
        /// An nxm:// link, or <mod id>/<file id>.
        what: String,
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

#[derive(Subcommand)]
enum LockCmd {
    /// Write the active profile to modlist.lock.toml (or --out).
    Export {
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Install what a lockfile needs from archives in --downloads, list what is missing, and set
    /// the order; --apply to do it. Mods you added that it doesn't list are kept.
    Apply {
        file: PathBuf,
        #[arg(long)]
        downloads: Vec<PathBuf>,
        #[arg(long)]
        apply: bool,
    },
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
    let cli = Cli::parse();
    let open = || -> Result<Instance> {
        let dir = match &cli.instance {
            Some(d) => d.clone(),
            None => std::env::current_dir()?,
        };
        Instance::open(&dir)
    };
    match cli.cmd {
        Cmd::Detect => print(&Game::ALL.iter().filter_map(|g| game::detect(*g)).collect::<Vec<_>>()),
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
        Cmd::Info => print(&ops::instance_info(&open()?)?),
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
        Cmd::Note { id, text } => print(&serde_json::json!({ "previous": ops::set_mod_note(&open()?, &id, &text)? })),
        Cmd::Color { id, color } => done(ops::set_color(&open()?, &id, &color)),
        Cmd::LockPlugin { name, unlock } => done(ops::lock_plugin(&open()?, &name, !unlock)),
        Cmd::Restore { n, apply: false } => print(&open()?.restore_preview(n)?),
        Cmd::Restore { n, apply: true } => print(&serde_json::json!({ "undone": open()?.restore(n)? })),
        Cmd::Plugins => {
            let inst = open()?;
            print(&plugins::list(&inst, &inst.profile()?)?)
        }
        Cmd::Sort { apply } => print(&loot::sort(&open()?, !apply)?),
        Cmd::Masterlist => print(&loot::update_masterlist(&open()?, true)?),
        Cmd::Rule { plugin: None, .. } => print(&loot::rules(&open()?)?),
        Cmd::Rule { plugin: Some(plugin), after, group } => print(&serde_json::json!({ "previous": loot::set_rule(&open()?, &loot::Rule { plugin, after, group })? })),
        Cmd::Check => print(&health::check(&open()?)?),
        Cmd::CrashLog { path } => print(&logs::read_crash_log(&open()?, path.as_deref())?),
        Cmd::PapyrusLog { tail } => print(&logs::read_papyrus_log(&open()?, tail)?),
        Cmd::TestRun { apply: false, .. } => print(&harness::preview(&open()?)?),
        Cmd::TestRun { apply: true, save, cells, dwell, timeout, keep_open, window, screenshots } => {
            print(&harness::test_run(&open()?, &harness::Scenario { save, cells, dwell, timeout, keep_open, window, screenshots })?)
        }
        Cmd::Deploy { apply } => print(&deploy::deploy(&open()?, !apply)?),
        Cmd::Launch { apply } => print(&rmm_core::launch::launch(&open()?, !apply)?),
        Cmd::Purge { apply } => print(&deploy::purge(&open()?, !apply)?),
        Cmd::Tool(ToolCmd::List) => print(&serde_json::json!({ "tools": tools::list(&open()?)?, "presets": tools::PRESETS })),
        Cmd::Tool(ToolCmd::Add { id, exe, preset, name, vars, rewrites, output_mod, keep, wait_for, args }) => {
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
        Cmd::Profile(ProfileCmd::Rename { from, to }) => done(open()?.rename_profile(&from, &to)),
        Cmd::Profile(ProfileCmd::Delete { name }) => done(open()?.delete_profile(&name)),
        Cmd::Profile(ProfileCmd::Options { local_inis, local_saves }) => {
            let inst = open()?;
            ops::set_profile_options(&inst, local_inis, local_saves)?;
            let p = inst.profile()?;
            print(&serde_json::json!({ "profile": inst.config.profile, "local_inis": p.local_inis, "local_saves": p.local_saves }))
        }
        Cmd::Import(ImportCmd::Sources) => print(&import::find_sources()),
        Cmd::Import(ImportCmd::Mo2 { dir, profile, apply }) => print(&import::import_mo2(&open()?, &dir, profile.as_deref(), !apply)?),
        Cmd::Import(ImportCmd::Vortex { backup, staging, profile, apply }) => {
            print(&import::import_vortex(&open()?, &backup, staging.as_deref(), profile.as_deref(), !apply)?)
        }
        Cmd::Addon(AddonCmd::List) => print(&addons::list(&open()?)?),
        Cmd::Addon(AddonCmd::Install { dir, apply }) => print(&addons::install(&open()?, &dir, !apply)?),
        Cmd::Addon(AddonCmd::Remove { name }) => print(&addons::remove(&open()?, &name)?),
        Cmd::Addon(AddonCmd::Call { name, function, input }) => {
            print(&addons::call(&open()?, &name, &function, &serde_json::from_str(&input).context("the input is not JSON")?)?)
        }
        Cmd::Lock(LockCmd::Export { out }) => print(&serde_json::json!({ "written": lockfile::write(&open()?, out.as_deref())? })),
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
        Cmd::Nexus(NexusCmd::Updates { ids }) => {
            print(&nexus::check_updates(&open()?, &nexus::Client::from_env()?, (!ids.is_empty()).then_some(ids.as_slice()))?)
        }
        Cmd::Nexus(NexusCmd::Install { what }) | Cmd::Nxm { link: what } => {
            let inst = open()?;
            let link = nexus::link_or_ids(&inst, &what)?;
            print(&nexus::install(&inst, &nexus::Client::from_env()?, &link, None)?)
        }
        Cmd::Nexus(NexusCmd::RegisterHandler) => {
            let inst = open()?;
            println!("{}", nexus::register_handler(&std::env::current_exe()?, &inst.root)?);
            Ok(())
        }
        #[cfg(target_os = "linux")]
        Cmd::Vfs => rmm_core::vfs::serve(&open()?, |data| eprintln!("Mounted the profile over {}. Ctrl+C unmounts.", data.display())),
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

fn print<T: Serialize + ?Sized>(v: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn parse_choices(arg: &str) -> Result<rmm_core::fomod::Choices> {
    let text = match arg.strip_prefix('@') {
        Some(file) => std::fs::read_to_string(file).with_context(|| format!("cannot read {file}"))?,
        None => arg.to_owned(),
    };
    serde_json::from_str(&text).context("--choices is not valid FOMOD choices JSON")
}

fn parse_var(s: &str) -> Result<(String, String), String> {
    s.split_once('=').map(|(k, v)| (k.to_owned(), v.to_owned())).ok_or_else(|| format!("expected name=value, got '{s}'"))
}

fn done(r: Result<()>) -> Result<()> {
    r?;
    println!("ok");
    Ok(())
}
