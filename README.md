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
rmm data meshes/actors | rmm data --find skeleton.nif   # who provides what in Data
rmm saves | rmm saves --fix <file>                 # plugins a save needs; enable the installed ones
rmm downloads
rmm trash | rmm trash --empty                     # space uninstalled mods take; delete them for good
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
rmm nexus install https://www.nexusmods.com/skyrimspecialedition/mods/32444   # Premium: a mod page or id installs its newest main file
rmm scan "Some Mod"                                 # Windows Defender plus DLL checks; `rmm scan` triages every mod
rmm read "Some Mod" scripts/foo.pex                 # a file inside a mod; Papyrus scripts come disassembled
rmm plugin-conflicts                                # plugins that change the same records, and which wins
rmm records "Some Mod.esp"                          # what a plugin adds (quests, NPCs, items, cells) with console ids
rmm review "Some Mod" --verdict clean --notes "..." # keep a review with the mod; `rmm reviews` shows if the files changed since
rmm nexus search frost survival --category Immersion   # search Nexus (no key needed); `rmm nexus info 671` reads a mod's page
rmm test-run --save MySave --cell Riverwood --screenshots --apply   # scripted with the harness plugin, in a small window behind the others (harness/README.md)
rmm test-run --cell Riverwood --steps "[{\"console\":\"tgm\"},{\"wait\":10},{\"screenshot\":\"after\"}]" --apply   # a test plan
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

The mod list with drag and drop and conflict markers (select a mod to see who it wins and loses against), plugins, profiles, mod rules, problems and history, a deploy preview, Play, and an AI tab (see "Built-in AI"). Drop a .zip, .7z or .rar on the window to install it; a mod with a FOMOD installer opens a wizard for its options.

It also renames and reinstalls mods, keeps notes, categories and row colours, locks plugins, edits LOOT rules, runs tools, reads crash and Papyrus logs, restores history, purges, imports from MO2 and Vortex, writes and applies lockfiles, installs from Nexus and checks for updates, manages add-ons, and can let AI clients in over HTTP ("AI access").

Built for long lists: only the rows on screen are drawn, separators group mods and collapse, and filters (enabled, conflicted, overridden, broken rule, missing masters, recently installed and more) show "x of y mods". Select several rows with Shift or Ctrl, then use Space (enable or disable), Delete, Ctrl+Up/Down, F2 (rename a separator), Ctrl+A, Ctrl+F, or "Move to" (top, bottom, a group, after a mod, past every conflict). The window reloads when the CLI or an AI changes the instance. It is plain HTML and JavaScript in `crates/rmm-app/ui/`, with no build step; every action is one call into rmm-core (`crates/rmm-app/src/main.rs`).

## AI (MCP)

```
claude mcp add rmm -- D:\Games\Rust_Mod_Manager\target\release\rmm.exe -i D:\Mods\skyrim mcp
```

For clients that connect to a URL, `rmm -i D:\Mods\skyrim mcp --http 127.0.0.1:47823` serves the same tools at `http://127.0.0.1:47823/mcp` and prints the bearer token; the window's "AI access" does the same while it is open.

The server exposes 85 tools covering inspection (including the Data folder, saves and downloads), reviewing mods (a safety scan with Windows Defender and DLL checks, reading files with Papyrus scripts disassembled, record conflicts between plugins, saved reviews, what a plugin adds), editing mod files and INIs with every edit revertible, install (including FOMOD installers), profiles, ordering, separators and mod rules, LOOT sorting and rules, plugins, health checks, tool runs (also in the background), deploy, launch, test runs, live test sessions and logs, purge, Nexus search and installs, import and lockfiles, add-ons, saved chats, the mod team board, and undo/redo. Writes to the game folder, installs from Nexus and imports default to `dry_run: true`. Every change to a profile lands in its undo history, so anything the AI does to a setup can be reversed.

Each session of an AI app connected this way (Claude desktop, Claude Code, any MCP client) is saved as a chat in the instance (`ai/chats/`): every tool call and its answer, under the app's name. rmm never sees the conversation itself, so the server asks the app to save a short note when a task ends (`save_chat_note`: what you asked for, what it did and decided, what's left). The session then shows in the window's Chats list, and `list_chats` and `read_chat` let any AI read it, so rmm's own AI can carry on where the app left off.

It also offers a mod team as four prompts, which clients show as commands:
- `find_mods`, the researcher: finds Nexus mods that match a description or images, work with a mod you name, or fit your list, ranked by confidence.
- `install_safely`, the installer: downloads your pick, checks it isn't malicious (or reuses its saved review), reviews its code, places it in the order, and deploys.
- `test_mods`, the tester: plays the game in a small window behind yours, step by step (teleports, fights, quest stages, items), and reports crashes, script errors and FPS.
- `mod_team`, the lead: runs all three and guides the tester.

Your AI client plays the roles; rmm supplies the tools. Or rmm plays them itself with your own API key (below), and you can mix the two: in the window's **Settings**, give each role to a provider or to your AI app. A step for your app waits on rmm's team board (`team_tasks`) until you ask the app to do its rmm team tasks (`finish_team_task` hands its report back). An app that leads can hand steps to the roles on a provider (`ask_teammate`); those run in the background and can read and preview but not change anything, so they report changes for the app to make.

## Built-in AI

rmm can run the AI itself, with your API key for OpenRouter, Claude (Anthropic), GPT (OpenAI), Kimi (Moonshot AI), DeepSeek or Qwen (Alibaba Cloud). It works through the same tools and mod team roles as an MCP client. Reads and dry runs run straight away; each change waits for your yes.

```
rmm ai use claude                        # or openrouter, openai, kimi, deepseek, qwen
rmm ai key claude                        # paste the key; rmm checks it with the provider, then keeps it in your user folder
rmm ai use qwen --model qwen3-max --server https://dashscope.aliyuncs.com/compatible-mode/v1   # another model or region
rmm ai models                            # what the key can use
rmm ai status                            # each provider's model, server, settings and whether a key is there
rmm ai options claude --effort max --instructions "Prefer mods with an ESL flag."   # per provider; "" clears one
rmm ai team researcher deepseek          # who does each role: a provider (--model for another), mcp for your AI app, "" for the one in use
rmm -i D:\Mods\skyrim ai chat --role researcher "snow that piles up on armour" --image look.png
rmm -i D:\Mods\skyrim ai chats                       # saved chats; `ai chat --resume <id>` goes on with one
```

In the window, the **Settings** tab holds the providers (API key, model, and **Options…**: your own instructions for the AI, effort, temperature and the most output tokens per reply) and the mod team: who answers a message without a role (Chat), and who is the Lead, Researcher, Installer and Tester. Each role goes to a provider with its own model, or to your AI app over MCP. The Lead hands each step to its role's AI (`ask_teammate`); that teammate works in a chat of its own, you allow its changes in the same panel, and its cost adds to the lead's.

The **AI** tab is an IDE for working with your AI:
- **Files:** every mod's folder, `overwrite/` and the game's INI files, in a tree, opened in an editor with tabs (Ctrl+S saves). Compiled scripts open disassembled and read-only. Every save keeps the old file, and **Edits…** puts any edit back.
- **Chat:** choose Chat or a role (Researcher, Installer, Tester, Lead; each says whose AI answers), attach or drop pictures, and **Ask AI** sends the selected lines of the open file along. Allow or Decline each change as it comes ("Allow all in this chat" stops the asking); the AI's file edits come as a diff.
- **Chats** are saved in the instance (`ai/chats/`) to open and go on with later. Sessions of AI apps connected over MCP are listed too; opening one shows its calls and notes, and a message sent from it starts a new chat that reads that session first.
- **Inspector:** each request to the provider and its answer, with tokens and, for Claude and OpenRouter, the cost; the toolbar shows the chat's total.

 A key in the provider's usual environment variable (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `OPENROUTER_API_KEY`, `MOONSHOT_API_KEY`, `DEEPSEEK_API_KEY`, `DASHSCOPE_API_KEY`) wins over a saved one. Your messages, pictures and the tools' answers go to the provider you pick, billed to your account.

## Layout

```
crates/rmm-core   game detection, instances, install, fomod, conflicts, archives, deploy, vfs, plugins, loot, health, tools, launch,
                  proton, rules, nexus, import, lockfile, logs, harness, addons, ops
crates/rmm        CLI + MCP server (stdio and HTTP; one binary)
crates/rmm-app    desktop app (Tauri 2): src/main.rs bridges to rmm-core, ui/ is the window
harness/          the SKSE and F4SE test harness plugins (C++, built separately; see harness/README.md)
```

Licence: GPL-3.0-or-later, as required by libloadorder and esplugin.
