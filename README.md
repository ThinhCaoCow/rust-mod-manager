# rmm: Rust Mod Manager

A mod manager for Skyrim SE/AE and Fallout 4 on Windows that an AI can drive over MCP. The design is in [SPEC.md](SPEC.md). The research behind it is in [reports/](reports/).

**Status: Windows only for now, with milestones 1 to 6 done for it.** The core library, CLI, MCP server (stdio and HTTP), desktop app, FOMOD installers, LOOT sorting and rules, archive conflicts, profiles, mod rules, tool runs, launching through the script extender, Nexus downloads and update checks, add-ons, MO2 and Vortex import, lockfiles and test runs work. The test harness has run in Skyrim and Fallout 4 (in a small window behind the others, with screenshots). Nexus single sign-on is built and tested, and switches on once Nexus registers rmm and issues its application name.

**Linux is on hold.** Its code (Proton for launching and tools, the FUSE VFS mode, letter-case handling) stays in the tree, builds, and passes its tests in WSL, but it isn't supported: it has never taken a real game to the main menu, because WSL can't draw the game and there was no Linux machine with a GPU to test on.

## Build

```
cargo build --release              # CLI and MCP server
cargo build --release -p rmm-app   # desktop app (Tauri 2, WebView2)
```

Plain `cargo build` and `cargo test` skip the desktop app, so they need no web view libraries. The Linux code (on hold) is checked and tested in WSL, with a separate target folder that keeps the Windows build intact. Cross-checking from Windows doesn't work: Zstandard, which zips and the add-on runtime's cache use, is C code that needs the target's C headers.

```
wsl -- bash -lc "cd /mnt/d/Games/Rust_Mod_Manager && CARGO_TARGET_DIR=\$HOME/rmm-target cargo test"
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
rmm move "SkyUI_5_2_SE" --to 0                     # priority 0 = lowest; higher wins conflicts
rmm separator add "UI" --at 0                      # a heading; moving it moves its group
rmm deploy                                         # preview what changes in the game folder
rmm deploy --apply                                 # hard-link mods in, write plugins.txt
rmm tool add xedit-qac --exe "D:\Tools\SSEEdit\SSEEdit.exe"   # presets: xedit, xedit-qac, xlodgen, bodyslide, synthesis, pandora, pgpatcher, texgen, dyndolod
rmm run xedit-qac --var plugin=Update.esm --apply  # deploy, run, capture output as the mod "Cleaned Update.esm"
rmm launch --apply                                 # deploy, check the SKSE/F4SE build matches the game, start it
rmm rule MyPatch.esp --after "Some Mod.esp"       # LOOT user rule; `rmm rule` lists them
rmm profile create Lean --empty && rmm profile switch Lean   # per-profile mods and plugin order
rmm profile options --local-inis true --local-saves true    # and its own INIs and saves
rmm mod-rule "My Patch" --after "Big Mod"         # suggestion; `rmm sort-mods --apply` satisfies them
rmm undo                                           # every change is undoable
rmm purge --apply                                  # restore the vanilla game folder
rmm import mo2 "D:\MO2\Skyrim" --apply             # or: rmm import vortex <state.v2 or backup.json>; rmm import sources
rmm lock export                                    # modlist.lock.toml; `rmm lock apply <file> --downloads <dir>` rebuilds
rmm nexus login                                    # paste your API key (or --sso once Nexus registers rmm); then `rmm nexus updates`, `rmm nxm <link>`
rmm test-run --save MySave --cell Riverwood --screenshots --apply   # scripted with the harness plugin, in a small window behind the others (harness/README.md)
rmm addon install .\my-addon --apply               # a sandboxed WebAssembly add-on
rmm vfs                                            # Linux (on hold): a FUSE view of Data instead of hard links, until Ctrl+C
```

Keep the instance on the same drive as the game so deploys use hard links rather than copies.

How deploys behave:
- **Overwrite.** Files a tool or the game replaces in the game folder are copied to the instance's `overwrite/` folder, which is deployed above every mod, so they are never lost.
- **When rmm refuses.** It won't deploy while the game is running, or into a game folder that Vortex still manages.
- **Tool runs.** `rmm run` deploys, runs the tool, then captures what it wrote into a generated mod: its `{output}` folder plus new or changed Data files. That mod replaces the tool's previous output. Files a tool rewrites in place (its `rewrites`) are swapped from hard links to copies first, so your installed mods aren't changed through the link. Pandora and PGPatcher get their paths written into their JSON settings, and keep their mod list or plugin between runs. New plugins are sorted with LOOT if you already sort with it.
- **Lost manifest.** If `deploy.json` is lost, `rmm purge --apply` still finds rmm's hard links by file identity.

## Desktop app

```
rmm-app --instance D:\Mods\skyrim     # or pick or create an instance on the welcome screen
```

The mod list with drag and drop and conflict markers (select a mod to see who it wins and loses against), plugins, profiles, mod rules, problems and history, a deploy preview, and Play. Drop a .zip or .7z on the window to install it; a mod with a FOMOD installer opens a wizard for its options.

It also renames and reinstalls mods, keeps notes and row colours, locks plugins, edits LOOT rules, runs tools, reads crash and Papyrus logs, restores history, purges, imports from MO2 and Vortex, writes and applies lockfiles, installs from Nexus and checks for updates, manages add-ons, and can let AI clients in over HTTP ("AI access").

Built for long lists: only the rows on screen are drawn, separators group mods and collapse, and filters (enabled, conflicted, overridden, broken rule, missing masters, recently installed and more) show "x of y mods". Select several rows with Shift or Ctrl, then use Space (enable or disable), Delete, Ctrl+Up/Down, F2 (rename a separator), Ctrl+A, Ctrl+F, or "Move to" (top, bottom, a group, after a mod, past every conflict). The window reloads when the CLI or an AI changes the instance. It is plain HTML and JavaScript in `crates/rmm-app/ui/`, with no build step; every action is one call into rmm-core (`crates/rmm-app/src/main.rs`).

## AI (MCP)

```
claude mcp add rmm -- D:\Games\Rust_Mod_Manager\target\release\rmm.exe -i D:\Mods\skyrim mcp
```

For clients that connect to a URL, `rmm -i D:\Mods\skyrim mcp --http 127.0.0.1:47823` serves the same tools at `http://127.0.0.1:47823/mcp` and prints the bearer token; the window's "AI access" does the same while it is open.

The server exposes 58 tools covering inspection, install (including FOMOD installers), profiles, ordering, separators and mod rules, LOOT sorting and rules, plugins, health checks, tool runs (also in the background), deploy, launch, test runs and logs, purge, Nexus, import and lockfiles, add-ons, and undo/redo. Writes to the game folder, installs from Nexus and imports default to `dry_run: true`. Every change to a profile lands in its undo history, so anything the AI does to a setup can be reversed.

## Layout

```
crates/rmm-core   game detection, instances, install, fomod, conflicts, archives, deploy, vfs, plugins, loot, health, tools, launch,
                  proton, rules, nexus, import, lockfile, logs, harness, addons, ops
crates/rmm        CLI + MCP server (stdio and HTTP; one binary)
crates/rmm-app    desktop app (Tauri 2): src/main.rs bridges to rmm-core, ui/ is the window
harness/          the SKSE and F4SE test harness plugins (C++, built separately; see harness/README.md)
```

Licence: GPL-3.0-or-later, as required by libloadorder and esplugin.
