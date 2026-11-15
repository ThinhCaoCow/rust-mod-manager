# rmm: Rust Mod Manager

A mod manager for Skyrim SE/AE and Fallout 4 that an AI can drive over MCP. The design is in [SPEC.md](SPEC.md). The research behind it is in [reports/](reports/).

**Status: milestone 1 of 6.** The core library, CLI and MCP server work. The UI, LOOT sorting, FOMOD, the Nexus integration, add-ons and the in-game test harness come next (see SPEC.md "Milestones").

## Build

```
cargo build --release
```

## Use

```
rmm detect                                         # find installed games
rmm init D:\Mods\skyrim --game skyrim-se           # create an instance (paths auto-detected)
set RMM_INSTANCE=D:\Mods\skyrim                    # or pass -i <dir> to every command
rmm install "C:\Downloads\SkyUI_5_2_SE.7z"
rmm mods | rmm conflicts | rmm plugins | rmm check
rmm move "SkyUI_5_2_SE" 0                          # priority 0 = lowest; higher wins conflicts
rmm deploy                                         # preview what changes in the game folder
rmm deploy --apply                                 # hard-link mods in, write plugins.txt
rmm undo                                           # every change is undoable
rmm purge --apply                                  # restore the vanilla game folder
```

Keep the instance on the same drive as the game so deploys use hard links rather than copies.

How deploys behave:
- **Overwrite.** Files a tool or the game replaces in the game folder are copied to the instance's `overwrite/` folder, which is deployed above every mod, so they are never lost.
- **When rmm refuses.** It won't deploy while the game is running, or into a game folder that Vortex still manages.
- **Lost manifest.** If `deploy.json` is lost, `rmm purge --apply` still finds rmm's hard links by file identity.

## AI (MCP)

```
claude mcp add rmm -- D:\Games\Rust_Mod_Manager\target\release\rmm.exe -i D:\Mods\skyrim mcp
```

The server exposes 18 tools covering inspection, install, ordering, plugins, health checks, deploy, purge and undo/redo. Writes to the game folder default to `dry_run: true`. Every change lands in undo history, so anything the AI does can be reversed.

## Layout

```
crates/rmm-core   game detection, instances, install, conflicts, deploy, plugins, health, ops
crates/rmm        CLI + MCP server (one binary)
```

Licence: GPL-3.0-or-later, as required by libloadorder and esplugin.
