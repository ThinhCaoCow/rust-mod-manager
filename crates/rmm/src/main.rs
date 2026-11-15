mod mcp;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rmm_core::{Game, Instance, conflicts, deploy, game, health, install, loot, ops, plugins, tools};
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
        /// skyrim-se or fallout4
        #[arg(long)]
        game: Game,
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
    /// Install from a folder, .zip or .7z.
    Install {
        path: PathBuf,
        #[arg(long)]
        name: Option<String>,
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
    Uninstall { id: String },
    Enable { id: String },
    Disable { id: String },
    /// Move a mod to a priority (0 = lowest).
    Move { id: String, priority: usize },
    Conflicts,
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
    /// Deploy, check the script extender matches the game, and start it (Windows); --apply to do it.
    Launch {
        #[arg(long)]
        apply: bool,
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
    Undo,
    Redo,
    History,
    /// Serve the Model Context Protocol on stdio for AI clients.
    Mcp,
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
        Cmd::Init { dir, game: g, game_path, local_path } => {
            let found = game::detect(g);
            let game_path = game_path
                .or_else(|| found.as_ref().map(|d| d.game_path.clone()))
                .context("game not found; pass --game-path")?;
            let local_path = local_path
                .or_else(|| game::local_path(g, &game_path))
                .context("cannot work out where plugins.txt lives; pass --local-path")?;
            let inst = Instance::create(&dir, g, &game_path, &local_path)?;
            print(&ops::instance_info(&inst)?)
        }
        Cmd::Info => print(&ops::instance_info(&open()?)?),
        Cmd::Mods => print(&ops::list_mods(&open()?)?),
        Cmd::Show { id } => print(&ops::mod_details(&open()?, &id, usize::MAX)?),
        Cmd::Install { path, name, disabled, choices } => {
            let choices = choices.as_deref().map(parse_choices).transpose()?;
            print(&install::install(&open()?, &path, name.as_deref(), !disabled, choices.as_ref())?)
        }
        Cmd::Fomod { path, choices } => {
            let choices = choices.as_deref().map(parse_choices).transpose()?;
            print(&install::fomod_options(&open()?, &path, choices.as_ref())?)
        }
        Cmd::Uninstall { id } => done(install::uninstall(&open()?, &id)),
        Cmd::Enable { id } => done(ops::set_mod_enabled(&open()?, &id, true)),
        Cmd::Disable { id } => done(ops::set_mod_enabled(&open()?, &id, false)),
        Cmd::Move { id, priority } => done(ops::move_mod(&open()?, &id, priority)),
        Cmd::Conflicts => {
            let inst = open()?;
            print(&conflicts::conflicts(&inst, &inst.profile()?)?)
        }
        Cmd::Plugins => {
            let inst = open()?;
            print(&plugins::list(&inst, &inst.profile()?)?)
        }
        Cmd::Sort { apply } => print(&loot::sort(&open()?, !apply)?),
        Cmd::Masterlist => print(&loot::update_masterlist(&open()?, true)?),
        Cmd::Rule { plugin: None, .. } => print(&loot::rules(&open()?)?),
        Cmd::Rule { plugin: Some(plugin), after, group } => print(&serde_json::json!({ "previous": loot::set_rule(&open()?, &loot::Rule { plugin, after, group })? })),
        Cmd::Check => print(&health::check(&open()?)?),
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
        Cmd::Undo => print(&open()?.undo()?),
        Cmd::Redo => print(&open()?.redo()?),
        Cmd::History => print(&open()?.history()?),
        Cmd::Mcp => {
            let dir = match cli.instance {
                Some(d) => d,
                None => std::env::current_dir()?,
            };
            Instance::open(&dir)?; // fail fast on a bad path
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
