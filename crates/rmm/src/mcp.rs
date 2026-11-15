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
use rmm_core::{Instance, conflicts, deploy, health, install, ops, plugins};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const INSTRUCTIONS: &str = "\
rmm manages one Skyrim SE or Fallout 4 instance.
Model: mods are an ordered list; later (higher priority) mods win loose-file conflicts. \
Plugins (.esp/.esm/.esl) have their own load order; masters always load before non-masters.
Workflow: inspect (list_mods, list_conflicts, list_plugins, health_check) -> change (install_mod, \
set_mod_enabled, move_mod, set_mod_order, set_plugin_order...) -> deploy with dry_run=true to \
preview, then dry_run=false to apply. Nothing reaches the game folder until deploy is applied.
FOMOD installers: fomod_options shows the steps, options and the resolved choices; edit those \
choices (calling fomod_options again shows steps they unlock) and pass them to install_mod.
Every change can be reverted with undo; history lists them. Prefer small reversible steps and \
re-run health_check after changes.";

#[derive(Clone)]
pub struct Rmm {
    root: PathBuf,
    tool_router: ToolRouter<Self>,
}

#[derive(Deserialize, JsonSchema)]
struct ModId {
    /// Mod id as shown by list_mods.
    id: String,
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
}
#[derive(Deserialize, JsonSchema)]
struct FomodArgs {
    /// Path to a mod folder, .zip or .7z on this machine.
    path: String,
    /// Choices to preview; steps and groups left out take their defaults.
    choices: Option<Choices>,
}
#[derive(Deserialize, JsonSchema)]
struct EnableArgs {
    id: String,
    enabled: bool,
}
#[derive(Deserialize, JsonSchema)]
struct MoveArgs {
    id: String,
    /// New priority, 0 = lowest. Higher wins conflicts.
    priority: usize,
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

    #[tool(description = "Loose-file conflicts between enabled mods: who overwrites whom, and mods that are fully overridden.")]
    async fn list_conflicts(&self) -> Result<String, String> {
        out(self.inst().and_then(|i| conflicts::conflicts(&i, &i.profile()?)))
    }

    #[tool(description = "Plugins in load order with master/light flags, masters and missing masters.")]
    async fn list_plugins(&self) -> Result<String, String> {
        out(self.inst().and_then(|i| plugins::list(&i, &i.profile()?)))
    }

    #[tool(description = "Problems to fix: missing masters, plugin limits, missing script extender, fully overridden mods, undeployed or externally changed files.")]
    async fn health_check(&self) -> Result<String, String> {
        out(self.inst().and_then(|i| health::check(&i)))
    }

    #[tool(description = "Install a mod from a local folder, .zip or .7z. Detects the Data root and game-root files. Mods with a FOMOD installer need choices (see fomod_options).")]
    async fn install_mod(&self, Parameters(a): Parameters<InstallArgs>) -> Result<String, String> {
        out(self.inst().and_then(|i| install::install(&i, a.path.as_ref(), a.name.as_deref(), a.enable.unwrap_or(true), a.choices.as_ref())))
    }

    #[tool(description = "Show a mod's FOMOD installer as it plays out with the given choices: visible steps, groups, options (type, description, selected), the full resolved choices, and the file count. Conditions are checked against the current setup.")]
    async fn fomod_options(&self, Parameters(a): Parameters<FomodArgs>) -> Result<String, String> {
        out(self.inst().and_then(|i| install::fomod_options(&i, a.path.as_ref(), a.choices.as_ref())))
    }

    #[tool(description = "Uninstall a mod. Its files go to trash and come back on undo.")]
    async fn uninstall_mod(&self, Parameters(a): Parameters<ModId>) -> Result<String, String> {
        out(self.inst().and_then(|i| install::uninstall(&i, &a.id)))
    }

    #[tool(description = "Enable or disable a mod (its plugins follow).")]
    async fn set_mod_enabled(&self, Parameters(a): Parameters<EnableArgs>) -> Result<String, String> {
        out(self.inst().and_then(|i| ops::set_mod_enabled(&i, &a.id, a.enabled)))
    }

    #[tool(description = "Move one mod to a new priority.")]
    async fn move_mod(&self, Parameters(a): Parameters<MoveArgs>) -> Result<String, String> {
        out(self.inst().and_then(|i| ops::move_mod(&i, &a.id, a.priority)))
    }

    #[tool(description = "Set the complete mod order in one step.")]
    async fn set_mod_order(&self, Parameters(a): Parameters<OrderArgs>) -> Result<String, String> {
        out(self.inst().and_then(|i| ops::set_mod_order(&i, &a.ids)))
    }

    #[tool(description = "Enable or disable a single plugin.")]
    async fn set_plugin_enabled(&self, Parameters(a): Parameters<PluginEnableArgs>) -> Result<String, String> {
        out(self.inst().and_then(|i| ops::set_plugin_enabled(&i, &a.name, a.enabled)))
    }

    #[tool(description = "Reorder plugins. Masters still load before non-masters regardless.")]
    async fn set_plugin_order(&self, Parameters(a): Parameters<PluginOrderArgs>) -> Result<String, String> {
        out(self.inst().and_then(|i| ops::set_plugin_order(&i, &a.names)))
    }

    #[tool(description = "Apply the profile to the game folder (hard links, incremental) and write plugins.txt. dry_run defaults to true and returns the plan.")]
    async fn deploy(&self, Parameters(a): Parameters<DryRun>) -> Result<String, String> {
        out(self.inst().and_then(|i| deploy::deploy(&i, a.dry_run.unwrap_or(true))))
    }

    #[tool(description = "Remove all deployed files and restore originals. dry_run defaults to true.")]
    async fn purge(&self, Parameters(a): Parameters<DryRun>) -> Result<String, String> {
        out(self.inst().and_then(|i| deploy::purge(&i, a.dry_run.unwrap_or(true))))
    }

    #[tool(description = "Undo the last change to the profile. Returns its label, or null if nothing to undo.")]
    async fn undo(&self) -> Result<String, String> {
        out(self.inst().and_then(|i| i.undo()))
    }

    #[tool(description = "Redo the last undone change.")]
    async fn redo(&self) -> Result<String, String> {
        out(self.inst().and_then(|i| i.redo()))
    }

    #[tool(description = "Undoable changes, newest first.")]
    async fn history(&self) -> Result<String, String> {
        out(self.inst().and_then(|i| i.history()))
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

pub async fn serve(root: PathBuf) -> anyhow::Result<()> {
    let server = Rmm { root, tool_router: Rmm::tool_router() };
    server.serve(stdio()).await?.waiting().await?;
    Ok(())
}
