# rmm backlog, round two

What to build next, after the first backlog ([rmm quality of life backlog](rmm%20quality%20of%20life%20backlog.md)) was finished on 2026-11-19. Two inputs, both from using the real window on build `1093a3b`:
- a first-time player's setup: [Riley](../research_notes/rmm%20persona%20reviews%202/new_user_riley.md). Fourteen installs from real Nexus archives, four FOMOD wizards, a deploy, a crash log, undo and restore, New playthrough and the Simple view;
- a veteran curator's week: [Morgan](../research_notes/rmm%20persona%20reviews%202/veteran_morgan.md). A 325-mod collection setup, a dry-run import of a real 505-mod MO2 instance, conflicts, Overwrite triage, lockfiles and tool previews.

Both ran against scratch copies of the game. Neither played, signed in to Nexus or ran a tool. The bugs below were checked against the code before this list was written. A few leftovers from the research report ([MO2 and Vortex quality of life](MO2%20and%20Vortex%20quality%20of%20life.md)) are at the end of Later.

**For:** N = new users, V = veterans and list curators.
**Size:** S = a day or so, M = a few days, L = a week or more.

## Fix now (bugs, all seen in the window or CLI)

Done on 2026-11-19, each checked in the window or the CLI: Majestic Mountains installs through the wizard; a mod's plugins go in after their masters; Hide files and Move files are undo steps; Desktop shortcut… shows what it would make, with Cancel; the update offer tells files of one page apart; read-me's open in a viewer in the Simple view; relative `-i` paths deploy; and the small ones in row 8 (LOOT's links open the page, `https` only).

| # | Bug | Where | Size |
|---|---|---|---|
| 1 | **A FOMOD whose steps have no names can't be installed.** `walk` finds each step by name, so every unnamed step matches the first one: Majestic Mountains fails with "step '' has no group named 'Final Notes'", whatever is chosen. Match steps by position, and keep reading stored choices from older installs. The wizard's header then shows "Step 1 of 2 ·" with nothing after it. | `fomod.rs:246-251`, `app.js:3317` | S |
| 2 | **A mod's own plugins go in by archive order**, so Embers XD's Lanterns patch landed before Embers XD.esp. Problems says "the game crashes at start" until a LOOT sort, and the panel calls the master that's present "Missing". Place each new plugin after its masters at install. Say "loads after it" when the master is present but loads later. | `install.rs:402-408`, `app.js:622, 931` | S |
| 3 | **Hide files, Hide the plugin and Move files aren't History steps.** Ctrl+Z right after hiding a file undid the plugin lock before it, and the file stayed hidden. Make them undo steps (with the copies Move files replaced, now in `trash/.replaced`). | `install.rs:188-260` (no `inst.edit`) | S–M |
| 4 | **Desktop shortcut… writes the file on click.** The dialog after it has only OK, so there's no backing out (it put a stray shortcut on a OneDrive Desktop during the review). Say where and what first, with Cancel. | `app.js:2283-2293` | S |
| 5 | **The update offer matches on Nexus mod id alone, first match wins.** MfgFix and MfgFix MCM share an id, so a new MCM file would be offered as "Update MfgFix?". Seven pages in the review setup have several installed files. Match by file name or category too, and ask which mod when several share the page. | `install.rs:376-383`, `app.js:3200-3209` | S |
| 6 | **In the Simple view, a read-me in the mod panel opens the hidden AI tab** (editor, chat, "add your API key"), with no tab highlighted and long lines running off the screen. Open read-me's in a plain wrapped viewer. | `app.js:1866-1868` | S |
| 7 | **A relative `-i` path makes deploy refuse:** "deployed by another instance", naming itself. `Instance::open` keeps the path as given and `check_owner` compares raw paths. | `instance.rs:265-269`, `deploy.rs:404-410` | S |
| 8 | Small ones: each failed install adds another copy of the same raw error to Problems; LOOT messages show raw Markdown and the link can't be clicked; the Problems note and count disagree with the badge; Redo is never greyed out; New playthrough shows a literal `Saves\<name>\`; Ctrl+Up/Down on a locked plugin does nothing without a word; `rmm check` exits 0 with errors; the import toast says hidden files "stayed behind" (they come in hidden); the CLI's "has no ModOrganizer.ini" doesn't mention global MO2 instances or `rmm import sources`. | `app.js:1290, 1306, 409, 2343, 2000-2006, 2826`; `import.rs:97` | S |

## Now (P0): what each kind of user hits first

Items 1 to 3 were done on 2026-11-19 and checked in the window on the review's scratch setup. Items 4 and 5 were done the same day. The crash log reader gives `not_yours`, the reasons a log may not be this setup's (no Play yet, older than the first Play, a plugin count more than 2 off, another crash-logger version), and the dialog shows them above the suspects; a DLL no mod provides gets where it sits instead of the save advice. Find the culprit never turns off the script extender, Address Library or the crash logger. History shows each change's time, Deployed and Played markers, the undone steps Redo brings back, a note once `history.limit` drops old steps, and "to put in the trash" in Restore's preview; a run of moves of the same mods is one step; Undo and Redo's tooltips name their step. Also: Problems' note about LOOT's masterlist shows only while it isn't downloaded. Runs of plugin moves are one step too (2026-11-20): the window says which plugins it moved, so `set_plugin_order` labels the step "move SkyUI_SE.esp to 5" like a mod's; a whole new order (MCP, a sort) stays "reorder plugins".

- **Item 1.** Moving SkyUI to the bottom now shows as one move, with "moves after Embers XD.esm", not every plugin that shifted. "Deploy, then sort again" deploys and reopens the preview without the warning. Collections install with `sort: false`, then show the same preview with Cancel. That path wasn't run, because it needs Premium.
- **Item 2.** 302 of the 312 downloads show their mod under "On Nexus" from the file name. A Downloads-tab install read its requirements: the panel's Needs listed Nemesis as not installed. Read it from Nexus fills in mods installed before this change.
- **Item 3.** Play's dialog listed the three plugins. Sort with LOOT… fixed the Embers patch, and Play asked again about the two a sort can't fix. Turn them off then went on to start the game. That launch was made to fail, by renaming the scratch folder's exe. Problems rows have Sort with LOOT… or Turn it off.

| # | Item | For | The moment | Where | Size |
|---|---|---|---|---|---|
| 1 | **A LOOT preview you can trust.** List only the plugins whose order relative to the others changes (the `longest_increasing` helper already exists), in plain words ("SkyUI_SE.esp moves after X"). Turn the "plugins.txt differs" warning into a button that does what it asks. Show this preview, with Cancel, before a collection install or update sorts, instead of applying a full sort unseen. | N, V | One moved plugin showed as 84 lines; Riley applied "7 → 5" numbers blind; a collection update would re-sort hand-placed additions. | `loot.rs:192-199`, `instance.rs:133`, `collection.rs:251`, `app.js:2568, 2590` | M |
| 2 | **Requirements for every install with a known Nexus id.** Downloads-tab and dropped installs read the id from the file name but never ask Nexus what the mod needs. Look it up the same way (no key needed), fill the Downloads tab's "On Nexus" from it, and say "Needs: …" in the panel instead of "REQUIRES None." (which reads as "needs nothing"). | N | Fourteen installs, not one requirement checked: the free-account path is the one the check was built for. | `rmm-app/src/main.rs:355-364`, `install.rs:43-53, 658-679` | S–M |
| 3 | **Play stops for "crashes at start".** When Problems has missing or misordered masters, Play asks first: Sort with LOOT, Turn them off, Play anyway. Problems' master finding gets a Sort button, not just Ignore (today the fix is a toast that fades). | N | Red "the game crashes at start" errors, and Play would start the game anyway. | `launch.rs:117-165`, `app.js:2232, 4255` | S |
| 4 | **A crash dialog that knows which crashes are yours.** Mark a log older than rmm's first Play, or one whose plugin list or crash-logger version doesn't match the setup. For a DLL no mod provides, say where it sits and that something outside rmm put it there, instead of suggesting the save. Find the culprit keeps Address Library and the crash logger on. | N | A made-up crash from before Riley ever played told them their save may be the problem. | `logs.rs:99-130, 170`, `bisect.rs:72-80` | M |
| 5 | **History you can read and rely on.** Times on entries; Deployed and Played markers; the redo steps listed (a fourth Redo silently re-did an older uninstall); a run of moves of the same selection as one entry; a note when the 200-step cap drops old entries; "to trash", not "to remove from the list". | N, V | Riley couldn't find "the setup that worked yesterday"; seven nudges pushed seven of Morgan's steps out without a word. | `app.js` History, `settings.rs:76`, `instance.rs:725` | M |

## Next (P1)

All of P1 (items 6 to 16) was done on 2026-11-19 and checked in the window, the CLI or over MCP on the review's scratch setups:
- **Item 6.** Holding Ctrl+Down moved a mod 8 places as one step, with no toast. Plugins move the same way.
- **Item 7.** The USSEP's Record conflicts section lists 7 plugins, winner first, with identical copies counted. The Plugins tab has an Index column (`1A`, `FE 003`).
- **Item 8.** Each tool's output counts only what that tool reads, with plugin contents hashed. Morgan's imported Pandora output shows Unknown on the Tools tab and as an info finding in Problems. The Tools tab shows each tool's status and last run.
- **Item 9.** Simple view hides the advanced chips, + Separator, the import and lockfile menu items, and the AI, team, editor and test-run settings. Downloads has a Find box ("7 of 312"), a wider name column and a red Delete….
- **Item 10.** The import preview counts hidden files, MO2's Overwrite and the space copied rather than linked. It also lists name clashes with both versions, and the profile's mods that end up after the import. A real MO2 dry run showed 189 hidden files, 20 in Overwrite, 26 clashes and 299 mods after.
- **Item 11.** The mod menu has Update from Nexus, Update from a file…, Hide files…, Notes…, Open folder, Colour… and Categories…. Colour… and Categories… work on several rows, and several rows' colours are one undo step. A file-conflict row has Hide.
- **Item 12.** The `deploy.skip_extras` setting is on by default. It keeps `meta.ini`, read-me's, licences, docs, `fomod/`, `src/` and Vortex's instructions out of Data and the game root. Folders they leave empty are removed.
- **Item 13.** LOOT reads the profile's own plugins.txt (written to `loot/local`), so its `active()` conditions and the load order match the profile before a deploy. On Morgan's undeployed setup, Problems shows 5 LOOT messages. A plugin turned off only in the profile shows `undeployed: 0`.
- **Item 14.** The Update pill on a row, the panel's Update button, the mod menu and the update list's Update button all download the new file (Premium) and update the mod in place with its FOMOD choices. Free accounts get the mod's Files tab instead. What's new shows Nexus's changelog for the versions after the installed one, once signed in. Not checked: the Premium download itself, as the test setup has no Nexus key.
- **Item 15.** In the FOMOD wizard, Majestic Mountains on a 1.6.1170 game marks "AE version" as Your game version and "Non AE version" as Other game version. Lanterns marks its USSEP patch (from the initials) and its Cutting Room Floor patch as "You have …". Base-game plugins don't count.
- **Item 16.** Add a tool has a More section with keep, rewrites, wait for, the settings file and the keys to set. A grass-cache setup went into tools.toml whole. MCP has `remove_tool`.

| # | Item | For | The moment | Where | Size |
|---|---|---|---|---|---|
| 6 | **Keyboard moves that keep up.** Move rows in the window at once and send the result when the keys stop, instead of refusing presses while one move round-trips (about 330 ms). | V | Holding Ctrl+Up for a second moved a mod 4 places, with an error toast. | `app.js:128-129, 1095-1121` | M |
| 7 | **Record conflicts in the window, and the load-order index.** A Records section in the plugin panel: which plugins change which records, winner first, identical copies (ITPOs) marked. A hex index column on the Plugins tab (`1A`, `FE 003`). | V | ELFX against ELFX Shadows' 8,051 shared records was found only on the CLI; no FormID prefix to type into xEdit. | `plugins.rs:167-207`, `app.js:633, 638` | M–L |
| 8 | **Out-of-date output that's relevant and complete.** Per tool, only what it reads (behaviour files for Pandora, plugins and meshes for DynDOLOD); plugin contents hashed, so an xEdit edit under the same name counts; outputs with unknown inputs (every imported one) in Problems; status and last run on the Tools tab. | V | Installing a HUD widget would ask for an hour of DynDOLOD, while a patch edit DynDOLOD reads goes unseen. | `tools.rs:317-393, 423`, `app.js:2862-2872` | M |
| 9 | **A Simple view that's simpler, and a searchable Downloads tab.** Simple view also hides the advanced chips, + Separator, the profile menu's lockfile and import items, and Settings' AI, team, MCP, editor and test-run sections. Downloads gets a search box and a wider name column, and Delete stops looking like Install. | N | Scrolling 312 tall rows to find SkyUI; a "beginner view" that looks like the full one. | `app.js:58-70, 1406, 3112`, `index.html:134-140` | S–M |
| 10 | **An import preview that tells the whole cost.** Count hidden files (189 `.mohidden` showed as 0), MO2's Overwrite, the space to be copied rather than linked, name clashes with both versions, and where the profile's other mods end up (today after the import, winning every conflict). | V | Morgan couldn't judge the migration's cost or completeness. | `import.rs:184-195, 843-849, 866-867`, `app.js:2812-2820` | S–M |
| 11 | **Right-click depth.** The mod menu gets Update…, Hide files…, colour, categories, notes and Open folder, with more than four entries for several rows. A file-conflict row gets Hide. | V | MO2's menu is where Morgan's hands go; settling one file's conflict needs Hide. | `app.js:2088-2096, 1349` | S–M |
| 12 | **A clean game folder.** Never deploy `meta.ini`, `LICENSE*`, `README*`, `fomod/`, SKSE's `src/` and `vortex_override_instructions.json` by default, in Data and in the game root. | N, V | Six `meta.ini` files and a `fomod/` folder in Data; SKSE's sources in the game folder Riley was told stays clean. | `settings.rs:38`, deploy | S |
| 13 | **LOOT judges the profile, not the game folder.** Before a deploy, Problems' LOOT messages read what's in the game folder: SKSE "missing", XPMSE "needs SkyUI", Engine Fixes' preloader missing, all installed. Run LOOT's conditions against the profile's plugins, or hold its messages until a deploy. | N, V | False alarms that vanish after a deploy. | `loot.rs:188-191` | M |
| 14 | **Updates from the list.** A per-row Update (Premium downloads it; free accounts get the Files tab) and the changelog. | V | Forty updates a week, each through the website. | `app.js:2625-2627` | M |
| 15 | **FOMOD hints.** Mark the option that matches the game version rmm knows (AE or SE files), and the patches for mods that are installed. | N | Majestic Mountains defaulted to "Non AE version" on a 1.6.1170 game; Lanterns' long patch list had no marks. | the FOMOD dialog | S–M |
| 16 | **Add a tool, completely.** The dialog gets keep, rewrites, wait-for and settings fields; MCP gets `remove_tool`. | V | Grass cache, Nemesis and Synthesis setups mean hand-editing TOML. | `app.js:2879-2887`, `mcp.rs` | S |

## Later (P2)

Items 17 to 22 were done on 2026-11-19, checked in the window, the CLI and the tests:
- **17.** Row and separator colours get a solid stripe at the row's left edge and a stronger tint (24%, separators 30%).
- **18.** Clean dirty plugins has "All in one mod" (`--one-mod`, MCP `one_mod`). Each run adds to "Cleaned plugins", which stays after the originals. The flow test cleans two plugins into it.
- **19.**
  - Skyrim with a 1.6 exe is named Skyrim Anniversary Edition in the window's title and on the first-run screens.
  - Each game found shows its version, Steam or not, and its drive's free space.
  - The new-instance dialog gives the free space, in red under 20 GB.
  - Problems warns while a drive has less than `health.low_space_gb` (20 GB). Morgan's D: (9.3 GB) gets it; Riley's C: (56 GB) doesn't.
- **20.** File conflicts starts with a plain-words line per mod: the kinds of file, who wins, and Expected or Worth a look. On XPMSSE: Ragdoll Friction's 39 meshes are expected; the script fixes are patches that win.
- **21.** Names without an upload time, such as "Relationship Dialogue Overhaul - RDO Final-1187-Final" and "Reverb and Ambiance Overhaul-701-1-5", give the mod's id and version. "UIExtensions v1-2-0-17561-1-2-0" now gives 17561, not 2.
- **22.** `rmm move <ids> --after X` and `--before X`. `enable`, `disable`, `undo`, `redo` and `history` have help text.

| # | Item | For | The moment | Where | Size |
|---|---|---|---|---|---|
| 17 | **Readable colours.** MO2-style row and separator colours as more than a 12–14% tint. | V | Imported colour-coding doesn't read at a glance. | `style.css:141, 233` | S |
| 18 | **Cleaned plugins in one mod.** An option for Clean dirty plugins to put every cleaned copy in one mod instead of one each. | V | 30–40 extra rows in a big list. | `tools.rs:258-261` | S |
| 19 | **First-run polish.** Say "Anniversary Edition" where the game is named; tell Auto search's copies apart (version, Steam or not); the drive's free space in the new-instance dialog, and a low-space finding in Problems. | N | Three identical "New instance" buttons; 10 GB free with no warning. | `app.js:10, 288, 328-345`, `health.rs` | S |
| 20 | **Plain conflict words.** The conflict dialog says whether a conflict is expected (a patch, a texture overhaul) or worth a look, instead of "Providers load left to right… Archives lose to every loose file." | N | Riley couldn't tell whether a conflict was fine. | `app.js:1814` | S |
| 21 | **Old-style Nexus names.** Parse names whose version isn't a number ("RDO Final-1187-Final"), so the mod gets its id, page link and update checks. | N, V | An ugly name and no updates for RDO. | `install.rs:60-85` | S |
| 22 | **CLI parity.** `rmm move --after/--before` like `rmm plugins`; help text on `enable`, `disable`, `undo`, `redo` and `history`. | V | Scripting "move after X". | `rmm/src/main.rs` | S |
| 23 | **Sign in without an API key.** Nexus's SSO (the code has it, waiting for an application id from Nexus); until then, From Nexus… says a sign-in comes first, in plain words. | N | Still "paste your personal API key", and talk of Premium and nxm://. | `nexus.rs:28, 281-289` | M (blocked on Nexus) |
| 24 | **From the research, not yet built:** one mod order shared by several profiles with their own enabled mods; cleaning orphaned co-saves; a warning before updating a mod whose files were edited through rmm (`rmm edits` knows them). | V | MO2 and Vortex users ask for each. | `instance.rs`, `saves.rs`, `install.rs` | M each |

## Reach: what MO2 and Vortex have that decides whether people can use rmm at all

Items 23, 24, 26 and 27 were done on 2026-11-20, checked in the window (against a stand-in Nexus), the CLI and the tests:
- **27.**
  - The mod panel has "Your Nexus account" with Endorse, Abstain or Withdraw, and Track or Untrack. The right-click menu has them too, for the selected mods.
  - There are Endorsed and Tracked filters.
  - `rmm nexus endorse <id> [--abstain]`, `rmm nexus track <id> [--untrack]` and `rmm nexus account`. They aren't over MCP.
  - The account's endorsements and tracked mods are read at start, two requests.
- **26.**
  - For a free account, the collection dry run lists `to_download`: each file still to get, with its download page (`?tab=files&file_id=…&nmm=1`).
  - Install opens the pages 3 at a time. The Downloads tab shows how many are left, with Open the next ones, Pause and Stop.
  - nxm links that match a waiting file (by mod and file id) are downloaded, not installed.
  - When all are in, the normal collection install runs from Downloads, with the curator's choices, rules, plugin list and LOOT.
  - One difference from the plan: the mods install together at the end, not one by one as they arrive. The rules, plugin list and new profile need the whole set.
- **23.**
  - SSO is unchanged and still waits for Nexus's application id.
  - From Nexus… asks you to sign in first. The sign-in says plainly where the key is.
  - Free accounts aren't told about Premium or nxm://.
- **24.**
  - **Share another profile's order** (profile menu, advanced; `rmm profile share-order <name>`, `--stop`). Moving a mod or plugin in one profile moves it in the others, undo included. Each profile keeps its own mods on or off, and a new mod comes in off in the others.
  - **Clean co-saves** (Saves tab; `rmm saves --clean-cosaves [--apply]`). It moves co-saves whose save is gone into the instance's trash.
  - Updating or reinstalling a mod with files edited in rmm asks first. Afterwards the report names them. Edits now record a hash, so a revert back to the archive's text doesn't count as an edit.

Added on 2026-11-19 from the comparison ([rmm vs MO2 and Vortex](rmm%20vs%20MO2%20and%20Vortex.md)), not from a review. The installer (25) is the final stage, after everything else on this list. The comparison lists the smaller gaps (C# FOMODs and BAIN, deleting saves, per-mod INI tweaks, translations).

| # | Item | For | The moment | Where | Size |
|---|---|---|---|---|---|
| 26 | **Collections for free accounts.** Nexus lets free accounts download only through the website's button, so do what Vortex does. Open each missing file's download prompt (`?tab=files&file_id=<id>&nmm=1`) a few pages at a time. Match the nxm links that come back to the collection's files by mod and file id, and install each one with the curator's choices as it arrives, instead of as a mod of its own. Once all are in, apply the rules and plugin list, then LOOT. Show how many are left, with Pause and Open the next ones. | N | From Nexus… refuses a collection without Premium. Vortex installs collections for free accounts with a click per file, which is how most people install them. | `collection.rs:196-197`, `app.js:2585`, the nxm queue | M–L |
| 27 | **Endorse and track.** Endorse, Abstain and Track in the mod panel and the right-click menu, plus Endorsed and Tracked filters. Use Nexus's v1 endorse/abstain and tracked-mods endpoints. Add `rmm nexus endorse` and `rmm nexus track`, but not over MCP, because they act publicly on the user's account. | N, V | rmm shows a mod's endorsement count but can't endorse it. MO2 and Vortex both can. | `nexus.rs`, `app.js` mod panel and menu, `rmm/src/main.rs` | S–M |

## Final stage

Built on 2026-11-20 and released as v0.1.0:
- `tauri.release.conf.json` is the per-user NSIS installer. It bundles `rmm.exe` as a sidecar beside the window, which keeps its name `rmm-app.exe`. `installer-hooks.nsh` runs `rmm installer-path` (hidden; `userpath.rs`), which adds the install folder to the user's PATH and takes it off again before uninstall. Editing PATH in NSIS broke a long PATH: it reads at most 1024 characters, so it wrote back only its own folder.
- The updater plugin reads `latest.json` from the newest GitHub release. Settings has "rmm 0.1.0 · Check for updates", and the newer version is offered on a toast at start.
- `.github/workflows/release.yml` builds and drafts a release for each `v*` tag. It signs the installer when a certificate secret is set.
- Left for the user (README, "Installer and updates"):
  - Make the updater key pair and put the public key in the release config.
  - Add the secrets, and a certificate if you want one.
  - Make the repository public.
- Checked on 2026-11-20 with `cargo tauri build --debug`:
  - The installer builds (31 MB).
  - Its script installs per user to `%LOCALAPPDATA%\rmm`, with `rmm-app.exe` and `rmm.exe` side by side.
  - The PATH logic, run on ten sample PATHs, adds the folder once and removes it from any position. It leaves `…\rmm2` alone.
  - A build signed with a throwaway key made the `.sig`. Against a local `latest.json` saying 0.2.0, the window offered the update at start and in Settings.
- Installed and uninstalled on the user's machine with their OK, signed with a throwaway self-signed certificate through the workflow's own signing step (installer, uninstaller, `rmm.exe` and `rmm-app.exe` signed and timestamped):
  - A fresh terminal ran the installed `rmm --version` from PATH.
  - Uninstall left PATH byte for byte as before, with no folder and no Apps & features entry.
  - The first try, with the NSIS PATH code, emptied their 1908-character PATH. It was restored from the copy saved just before, and the fix above came from it.
  - The workflow's signing step had a bug, now fixed: Tauri splits a string `signCommand` on spaces, so it uses `{cmd, args}`.
- Released on 2026-11-20 as v0.1.0 ("rmm v0.1.0" on GitHub), built by the release workflow:
  - `releases/latest/download/latest.json` downloads, and its installer link serves the 21.6 MB setup exe.
  - A 0.0.9 debug build with the release key was offered 0.1.0 from the published feed, on a toast and in Settings.
- Trusted signing: the user chose to leave the installer unsigned for now (2026-11-20), so SmartScreen warns on first run. The workflow's signing step is ready for a certificate later.

| # | Item | For | The moment | Where | Size |
|---|---|---|---|---|---|
| 25 | **An installer that keeps rmm up to date.** Turn on Tauri's bundler: a per-user NSIS installer (no admin) with `rmm.exe` beside the window and on PATH. Tauri's updater plugin checks GitHub releases, which are signed with an updater key, and offers each new version at start; Settings gets "Check for updates". Sign the installer too, or SmartScreen warns on every download. | N | MO2 and Vortex ship installers that update themselves. rmm needs a Rust toolchain and cargo. | `rmm-app/tauri.conf.json:16` (`"active": false`), `rmm-app/Cargo.toml`, a release workflow | M, plus a code-signing certificate |

## Keep (both reviewers name these)

- **Undo and Restore:** "still rmm's best moment" for Riley; Morgan undid a week's work byte for byte.
- **The Getting started card** and the first-run screen: the checklist Riley wished for last time.
- **The MO2 import** now brings every profile, its INIs and saves, and Executables as tools.
- **Archive-aware conflicts** in the conflict dialog and the Data tab; **collapsed groups** with conflict badges; **Overwrite's Move to mod**.
- **The Plugins tab's keyboard control**, the **lockfile** with diff, pack and Markdown, **update in place** with Hold and Skip, **New playthrough**, and errors that stay.

Fix-now 1 to 3 and P0 1 to 3 are what would most likely lose each kind of user now: a popular mod that won't install, an undo that undoes the wrong thing, a LOOT preview that hides the one real move, and a Play button that starts a game Problems already says will crash.
