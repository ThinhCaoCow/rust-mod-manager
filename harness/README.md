# rmm harness plugins

`rmm_harness.dll` for Skyrim SE/AE (SKSE, `skse/`) and Fallout 4 (F4SE, `f4se/`). It is the game side of `rmm test-run`: it reports when data, the main menu and each load are done, sends FPS and the player's cell every second, and runs rmm's commands (load a save, console lines, screenshot, quit) on the game's main thread. `common/` holds the socket and protocol code both share. SPEC.md "In-game test harness" has the protocol.

The plugin does nothing unless rmm started the game: it needs `RMM_HARNESS_PORT` and `RMM_HARNESS_TOKEN` in its environment.

Status: both build with MSVC 19.50 (Visual Studio 2026). The SKSE plugin has run in Skyrim AE 1.6.1170 (SKSE 2.2.6): windowed behind other windows, `coc` through cells with FPS, screenshots and quit all work; under Proton in WSL it loads and connects back to rmm. The F4SE plugin has run in Fallout 4 1.10.163 (F4SE 0.6.23) the same way: main menu, `coc` into RedRocketExt and Vault111Ext with FPS, screenshots and quit. Console output (each console line's result carries what it printed) works in Skyrim 1.6.1170; the Fallout 4 side builds but hasn't run in the game yet.

## Build (Windows)

You need Visual Studio 2022 or later with the C++ desktop workload, CMake 3.25 or later, git, and:

```
git clone https://github.com/microsoft/vcpkg target/vcpkg
target\vcpkg\bootstrap-vcpkg.bat -disableMetrics
git clone -b ng https://github.com/alandtse/CommonLibVR target/harness-deps/CommonLibSSE
git clone https://github.com/alandtse/CommonLibF4 target/harness-deps/CommonLibF4
```

`vcpkg.json` pins the vcpkg baseline; check out the commit it names (`git -C target/vcpkg checkout <builtin-baseline>`) or update the baseline. Then, from the repository root:

```
cmake -S harness/skse -B target/harness-skse -G "Visual Studio 18 2026" -A x64 ^
  -DCMAKE_TOOLCHAIN_FILE=target/vcpkg/scripts/buildsystems/vcpkg.cmake -DVCPKG_TARGET_TRIPLET=x64-windows-static-md ^
  -DCOMMONLIB_SSE_DIR=target/harness-deps/CommonLibSSE
cmake --build target/harness-skse --config Release

cmake -S harness/f4se -B target/harness-f4se -G "Visual Studio 18 2026" -A x64 ^
  -DCMAKE_TOOLCHAIN_FILE=target/vcpkg/scripts/buildsystems/vcpkg.cmake -DVCPKG_TARGET_TRIPLET=x64-windows-static-md ^
  -DCOMMONLIB_F4_DIR=target/harness-deps/CommonLibF4
cmake --build target/harness-f4se --config Release
```

Use `-G "Visual Studio 17 2022"` with Visual Studio 2022. The first configure builds the dependencies with vcpkg, and the first build compiles CommonLib, which takes a while.

The DLLs land in `target/harness-skse/Release/` and `target/harness-f4se/Release/`. Both use Address Library. The Skyrim one is built for SE and AE (CommonLibSSE-NG's runtime selection); the Fallout 4 one answers both the original runtime's `F4SEPlugin_Query` and the next-gen `F4SEPlugin_Version`.

## Install

Make a folder holding `SKSE/Plugins/rmm_harness.dll` (Fallout 4: `F4SE/Plugins/rmm_harness.dll`) and install it as a mod (`rmm install <folder>`). It needs Address Library for SKSE Plugins (or for F4SE Plugins). Then:

```
rmm test-run --save MySave --cell Riverwood --apply
```

With the plugin deployed, `test-run` drives the game over it; without it, it only watches the process and the logs.

The game runs in a 960x540 window behind your other windows (`--window 1280x720` for another size, `--window fullscreen` for your own settings); rmm changes the game's INIs for the run and puts them back afterwards. `--screenshots` saves a PNG in each cell into the instance's `test-runs/` folder.
