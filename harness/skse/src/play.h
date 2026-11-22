// Playing the game for tests: state, events, Papyrus, dialogue, menus, attacks, controls.
#pragma once
#include "harness.h"

namespace play
{
	// Answer one of rmm's game commands (main thread).
	void command(const std::string& a_cmd, rmm::Game::Reply a_reply);
	// Report deaths, combat, quest stages and activations as game events (once data has loaded).
	void watch_events();
	// Report the message box that just opened, with its text and buttons, as a game event.
	void message_box_opened();
}
