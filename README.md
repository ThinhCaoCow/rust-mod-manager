# rmm: Rust Mod Manager

A mod manager for Skyrim SE/AE and Fallout 4 that an AI can drive over MCP. The design is in [SPEC.md](SPEC.md). The research behind it is in [reports/](reports/).

**Status: milestone 2 of 6 in progress.** The core library, CLI, MCP server, FOMOD installers, LOOT sorting, archive conflicts, tool runs and launching through the script extender work on Windows. Proton launching and tool runs finish milestone 2; the UI, the Nexus integration, add-ons and the in-game test harness come next (see SPEC.md "Milestones").

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
rmm fomod "C:\Downloads\SomeFomodMod.7z"           # show a FOMOD installer's steps and defaults
rmm install "C:\Downloads\SomeFomodMod.7z" --choices "{}"   # defaults; or JSON / @file from 'rmm fomod'
rmm mods | rmm conflicts | rmm plugins | rmm check
rmm sort                                           # LOOT order preview; --apply saves it (undoable)
rmm move "SkyUI_5_2_SE" 0                          # priority 0 = lowest; higher wins conflicts
rmm deploy                                         # preview what changes in the game folder
rmm deploy --apply                                 # hard-link mods in, write plugins.txt
rmm tool add xedit-qac --exe "D:\Tools\SSEEdit\SSEEdit.exe"   # presets: xedit, xedit-qac, xlodgen, bodyslide, synthesis, pandora, pgpatcher, texgen, dyndolod
rmm run xedit-qac --var plugin=Update.esm --apply  # deploy, run, capture output as the mod "Cleaned Update.esm"
rmm launch --apply                                 # deploy, check the SKSE/F4SE build matches the game, start it
rmm undo                                           # every change is undoable
rmm purge --apply                                  # restore the vanilla game folder
```

Keep the instance on the same drive as the game so deploys use hard links rather than copies.

How deploys behave:
- **Overwrite.** Files a tool or the game replaces in the game folder are copied to the instance's `overwrite/` folder, which is deployed above every mod, so they are never lost.
- **When rmm refuses.** It won't deploy while the game is running, or into a game folder that Vortex still manages.
- **Tool runs.** `rmm run` deploys, runs the tool, then captures what it wrote into a generated mod: its `{output}` folder plus new or changed Data files. That mod replaces the tool's previous output. Files a tool rewrites in place (its `rewrites`) are swapped from hard links to copies first, so your installed mods aren't changed through the link. Pandora and PGPatcher get their paths written into their JSON settings, and keep their mod list or plugin between runs. New plugins are sorted with LOOT if you already sort with it.
- **Lost manifest.** If `deploy.json` is lost, `rmm purge --apply` still finds rmm's hard links by file identity.

## AI (MCP)

```
claude mcp add rmm -- D:\Games\Rust_Mod_Manager\target\release\rmm.exe -i D:\Mods\skyrim mcp
```

The server exposes 25 tools covering inspection, install (including FOMOD installers), ordering, LOOT sorting, plugins, health checks, tool runs, deploy, launch, purge and undo/redo. Writes to the game folder default to `dry_run: true`. Every change lands in undo history, so anything the AI does can be reversed.

## Layout

```
crates/rmm-core   game detection, instances, install, fomod, conflicts, archives, deploy, plugins, loot, health, tools, launch, ops
crates/rmm        CLI + MCP server (one binary)
```

Licence: GPL-3.0-or-later, as required by libloadorder and esplugin.
