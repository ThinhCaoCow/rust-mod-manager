mod mcp;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rmm_core::{Game, Instance, conflicts, deploy, game, health, install, ops, plugins};
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
    },
    Uninstall { id: String },
    Enable { id: String },
    Disable { id: String },
    /// Move a mod to a priority (0 = lowest).
    Move { id: String, priority: usize },
    Conflicts,
    Plugins,
    /// Health check.
    Check,
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
    Undo,
    Redo,
    History,
    /// Serve the Model Context Protocol on stdio for AI clients.
    Mcp,
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
        Cmd::Install { path, name, disabled } => print(&install::install(&open()?, &path, name.as_deref(), !disabled)?),
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
        Cmd::Check => print(&health::check(&open()?)?),
        Cmd::Deploy { apply } => print(&deploy::deploy(&open()?, !apply)?),
        Cmd::Purge { apply } => print(&deploy::purge(&open()?, !apply)?),
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

fn done(r: Result<()>) -> Result<()> {
    r?;
    println!("ok");
    Ok(())
}
