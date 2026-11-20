// Playing the game for tests in Fallout 4: state, events, Papyrus, walking, activating.
#pragma once
#include "harness.h"

namespace play
{
	// Answer one of rmm's game commands (main thread).
	void command(const std::string& a_cmd, rmm::Game::Reply a_reply);
	// Report deaths, hits and the player's activations as game events (once data has loaded).
	void watch_events();
}
