//! rmm-core: everything the CLI, MCP server and UI share. See SPEC.md.
pub mod addons;
pub mod archives;
pub mod conflicts;
pub mod deploy;
pub mod fomod;
pub mod game;
pub mod harness;
pub mod import;
pub mod lockfile;
pub mod nexus;
pub mod health;
pub mod install;
pub mod instance;
pub mod launch;
pub mod logs;
pub mod loot;
pub mod ops;
pub mod plugins;
pub mod profile_ini;
pub mod proton;
pub mod rules;
pub mod tools;
#[cfg(target_os = "linux")]
pub mod vfs;

pub use game::Game;

/// Progress of long jobs (deploys, LOOT sorts, installs, health checks) for a front end to show.
/// The CLI and MCP server don't set a hook; the window does.
pub mod progress {
    use std::sync::RwLock;
    type Hook = Box<dyn Fn(&str, usize, usize) + Send + Sync>;
    static HOOK: RwLock<Option<Hook>> = RwLock::new(None);

    pub fn set_hook(f: impl Fn(&str, usize, usize) + Send + Sync + 'static) {
        *HOOK.write().unwrap() = Some(Box::new(f));
    }

    /// `stage`, with `done` of `total` steps (total 0: no count).
    pub fn report(stage: &str, done: usize, total: usize) {
        if let Some(f) = HOOK.read().unwrap().as_ref() {
            f(stage, done, total);
        }
    }
}
pub use instance::{Instance, ModEntry, PluginEntry, Profile};

/// Lowercase, forward-slash key used for every case-insensitive path comparison.
/// A file in rmm's folder in the user's config folder (`%APPDATA%\rmm` on Windows).
pub fn config_file(name: &str) -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("rmm").join(name))
}

/// Write a secret (API key, token) readable only by the user on Unix.
pub fn write_private(path: &std::path::Path, data: &[u8]) -> anyhow::Result<()> {
    std::fs::create_dir_all(path.parent().unwrap_or(path))?;
    let mut open = std::fs::OpenOptions::new();
    open.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        open.mode(0o600);
        // An older file keeps its mode through `open`.
        if path.exists() {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    std::io::Write::write_all(&mut open.open(path)?, data)?;
    Ok(())
}

pub fn path_key(rel: &str) -> String {
    rel.replace('\\', "/").to_lowercase()
}
