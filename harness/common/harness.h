// rmm's in-game harness (SPEC.md "In-game test harness"). When `test_run` starts the game it sets
// RMM_HARNESS_PORT and RMM_HARNESS_TOKEN; the plugin then connects to rmm over TCP 127.0.0.1
// (which works through Proton, unlike named pipes), reports load milestones and runs rmm's
// commands on the game's main thread. Without those variables it does nothing.
//
// This part knows sockets and the protocol (JSON lines, crates/rmm-core/src/harness.rs) but no
// game: each script extender's plugin fills in a Game. It includes no CommonLib header, which
// keeps Winsock's declarations away from CommonLib's own Windows ones.
#pragma once
#include <functional>
#include <string>
#include <utility>

namespace rmm
{
	struct Game
	{
		// Queue work for the game's main thread (the script extender's task interface).
		std::function<void(std::function<void()>)> on_main_thread;
		// Run a console line. Main thread. "" when it ran, else why not.
		std::function<std::string(const std::string&)> console;
		// Start loading a save by name (no extension). Main thread. "" when it started.
		std::function<std::string(const std::string&)> load_save;
		// Frames per second and the player's cell ("" in menus). Main thread.
		std::function<std::pair<double, std::string>()> sample;
	};

	// Connect if rmm started the game; false, and nothing else happens, otherwise.
	bool start(Game a_game, const std::string& a_runtime);

	// If rmm asked for it (RMM_HARNESS_BEHIND), put the game's window behind the others without
	// activating it. Safe to call before the window exists, and again later.
	void send_to_back();

	// Report a milestone: "data_loaded", "main_menu", or "game_loaded" (a loading screen closed
	// with the player in the world, after a save or a coc; with the save's name, or "").
	void event(const char* a_event, const std::string& a_save = {});

	// Report a step that isn't a milestone (a menu opening, data starting to load). rmm lists the
	// last few when it gives up waiting for a milestone.
	void note(const std::string& a_what);
}
