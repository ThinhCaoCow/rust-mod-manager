//! rmm-core: everything the CLI, MCP server and UI share. See SPEC.md.
pub mod conflicts;
pub mod deploy;
pub mod fomod;
pub mod game;
pub mod health;
pub mod install;
pub mod instance;
pub mod ops;
pub mod plugins;
pub mod tools;

pub use game::Game;
pub use instance::{Instance, ModEntry, PluginEntry, Profile};

/// Lowercase, forward-slash key used for every case-insensitive path comparison.
pub fn path_key(rel: &str) -> String {
    rel.replace('\\', "/").to_lowercase()
}
