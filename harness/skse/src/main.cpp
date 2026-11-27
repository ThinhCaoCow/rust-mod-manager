// rmm's harness for Skyrim SE/AE (CommonLibSSE-NG): the game side of `test_run`.
#include "harness.h"
#include "play.h"

#include <RE/Skyrim.h>
#include <SKSE/SKSE.h>

#include <cstdarg>
#include <algorithm>
#include <cstring>
#include <format>
#include <sstream>

namespace
{
	std::atomic<bool> menu_seen{ false };
	std::string       loading;  // the save named by kPreLoadGame

	struct MenuWatch : RE::BSTEventSink<RE::MenuOpenCloseEvent>
	{
		RE::BSEventNotifyControl ProcessEvent(const RE::MenuOpenCloseEvent* a_event, RE::BSTEventSource<RE::MenuOpenCloseEvent>*) override
		{
			if (!a_event) {
				return RE::BSEventNotifyControl::kContinue;
			}
			if (a_event->opening && a_event->menuName == RE::MainMenu::MENU_NAME && !menu_seen.exchange(true)) {
				rmm::send_to_back();
				rmm::event("main_menu");
			}
			// A question (missing content, a damaged save) stops a load until it's answered.
			if (a_event->opening && a_event->menuName == RE::MessageBoxMenu::MENU_NAME) {
				rmm::event("message_box");
				play::message_box_opened();
			}
			// Closing a loading screen in the world is a finished load: of a save, or of a coc,
			// which also starts a game from the main menu (and fires no load message).
			const auto player = RE::PlayerCharacter::GetSingleton();
			if (!a_event->opening && a_event->menuName == RE::LoadingMenu::MENU_NAME && player && player->GetParentCell()) {
				rmm::event("game_loaded", loading);
				loading.clear();
			}
			return RE::BSEventNotifyControl::kContinue;
		}
	};

	// What console lines print. Every line goes through ConsoleLog::VPrint; a detour on it copies
	// the lines printed on this thread while rmm's command runs. Without the detour (another
	// runtime, or another plugin's hook already there) the lines the console log gained are read.
	using VPrint = void(RE::ConsoleLog*, const char*, std::va_list);
	VPrint*                   vprint = nullptr;  // the original, past the detour
	thread_local std::string* capturing = nullptr;

	void on_vprint(RE::ConsoleLog* a_log, const char* a_fmt, std::va_list a_args)
	{
		if (capturing && a_fmt && capturing->size() < 0x10000) {
			std::va_list copy;
			va_copy(copy, a_args);
			char      line[0x1000];
			const int n = std::vsnprintf(line, sizeof line, a_fmt, copy);
			va_end(copy);
			if (n > 0) {
				capturing->append(line);
				if (capturing->back() != '\n') {
					capturing->push_back('\n');
				}
			}
		}
		vprint(a_log, a_fmt, a_args);
	}

	// jmp [rip+0] with the address after it: 14 bytes.
	void write_jump(std::uint8_t* a_at, std::uintptr_t a_to)
	{
		const std::array<std::uint8_t, 6> op{ 0xFF, 0x25, 0, 0, 0, 0 };
		std::memcpy(a_at, op.data(), op.size());
		std::memcpy(a_at + op.size(), &a_to, sizeof a_to);
	}

	// Only when VPrint starts as it does on 1.6.1170 (the bytes Console Link checks for), so a
	// runtime or hook rmm doesn't know is left alone. Its first instruction, mov [rsp+18h], r8
	// (5 bytes, nothing relative to rip), moves into a stub that jumps back past it.
	bool hook_vprint()
	{
		constexpr std::array<std::uint8_t, 16> start{ 0x4C, 0x89, 0x44, 0x24, 0x18, 0x48, 0x89, 0x54, 0x24, 0x10, 0x53, 0x55, 0x56, 0x57, 0x41, 0x54 };
		const REL::Relocation<std::uintptr_t> fn{ RELOCATION_ID(50180, 51110) };
		const auto                            at = fn.address();
		if (std::memcmp(reinterpret_cast<const void*>(at), start.data(), start.size()) != 0) {
			return false;
		}
		auto&      tramp = SKSE::GetTrampoline();
		const auto stub = static_cast<std::uint8_t*>(tramp.allocate(5 + 14));
		const auto thunk = static_cast<std::uint8_t*>(tramp.allocate(14));
		const auto rel = reinterpret_cast<std::intptr_t>(thunk) - static_cast<std::intptr_t>(at + 5);
		if (rel < INT32_MIN || rel > INT32_MAX) {
			return false;
		}
		std::memcpy(stub, start.data(), 5);
		write_jump(stub + 5, at + 5);
		write_jump(thunk, reinterpret_cast<std::uintptr_t>(&on_vprint));
		vprint = reinterpret_cast<VPrint*>(stub);
		std::array<std::uint8_t, 5> jmp{ 0xE9 };
		const auto                  rel32 = static_cast<std::int32_t>(rel);
		std::memcpy(jmp.data() + 1, &rel32, sizeof rel32);
		REL::safe_write(at, jmp.data(), jmp.size());
		return true;
	}

	// getgs/setgs with a game setting that doesn't exist crashes the game in its lookup, so the
	// name is checked first. "" when the line is fine.
	std::string unknown_setting(const std::string& a_line)
	{
		std::istringstream in(a_line);
		std::string        cmd, name;
		in >> cmd >> name;
		std::ranges::transform(cmd, cmd.begin(), [](unsigned char c) { return static_cast<char>(std::tolower(c)); });
		if (name.empty() || (cmd != "getgs" && cmd != "getgamesetting" && cmd != "setgs" && cmd != "setgamesetting")) {
			return {};
		}
		if (name.size() > 1 && name.front() == '"' && name.back() == '"') {
			name = name.substr(1, name.size() - 2);
		}
		const auto settings = RE::GameSettingCollection::GetSingleton();
		if (!settings || settings->GetSetting(name.c_str())) {
			return {};
		}
		return std::format("there is no game setting '{}' (`help {} 4` lists the names); {} with an unknown name crashes the game", name, name, cmd);
	}

	std::string console(const std::string& a_line, std::string& a_out)
	{
		if (auto why = unknown_setting(a_line); !why.empty()) {
			return why;
		}
		const auto factory = RE::IFormFactory::GetConcreteFormFactoryByType<RE::Script>();
		const auto script = factory ? factory->Create() : nullptr;
		if (!script) {
			return "cannot make a console script";
		}
		script->SetCommand(a_line);
		const auto        log = RE::ConsoleLog::GetSingleton();
		const std::string before = log && log->buffer.c_str() ? log->buffer.c_str() : "";
		// As when typed into the console: some commands print only in console mode.
		auto&       mode = RE::GetStaticTLSData()->consoleMode;
		const bool  was = mode;
		std::string printed;
		mode = true;
		capturing = vprint ? &printed : nullptr;
		script->CompileAndRun(nullptr);
		capturing = nullptr;
		mode = was;
		delete script;
		// The hook catches nothing when another plugin prints the console's lines another way
		// (More Informative Console, a replacement console); the log may still have them.
		if (!printed.empty()) {
			a_out = std::move(printed);
		} else if (log && log->buffer.c_str()) {
			// The log keeps only its last part, dropping the oldest lines as it fills, so where it
			// was is found by the text it ended with.
			const std::string_view all(log->buffer.c_str());
			const auto             tail = std::string_view(before).substr(before.size() - (std::min)(before.size(), std::size_t{ 256 }));
			const auto             at = tail.empty() ? 0 : all.rfind(tail);
			a_out = at == std::string_view::npos ? std::string(all) : std::string(all.substr(at + tail.size()));
		}
		return {};
	}

	void on_message(SKSE::MessagingInterface::Message* a_msg)
	{
		switch (a_msg->type) {
		case SKSE::MessagingInterface::kInputLoaded:
			rmm::send_to_back();
			break;
		case SKSE::MessagingInterface::kDataLoaded:
			{
				rmm::send_to_back();
				// The game pauses while its window isn't focused, which stops the main-thread tasks that
				// run rmm's commands; a test run usually starts behind other windows. Memory only.
				const std::array<RE::INISettingCollection*, 2> inis{ RE::INIPrefSettingCollection::GetSingleton(), RE::INISettingCollection::GetSingleton() };
				for (const auto ini : inis) {
					if (const auto s = ini ? ini->GetSetting("bAlwaysActive:General") : nullptr) {
						s->SetBool(true);
						break;
					}
				}
				static MenuWatch watch;
				RE::UI::GetSingleton()->AddEventSink<RE::MenuOpenCloseEvent>(&watch);
				play::watch_events();
				rmm::event("data_loaded");
				break;
			}
		case SKSE::MessagingInterface::kPreLoadGame:
			loading = a_msg->data ? std::string(static_cast<const char*>(a_msg->data), a_msg->dataLen) : std::string{};
			break;
		default:
			break;
		}
	}
}

SKSEPluginLoad(const SKSE::LoadInterface* a_skse)
{
	SKSE::InitInfo init;
	init.trampoline = true;
	init.trampolineSize = 64;
	SKSE::Init(a_skse, init);
	rmm::Game game;
	game.on_main_thread = [](std::function<void()> a_task) { SKSE::GetTaskInterface()->AddTask(std::move(a_task)); };
	game.console = console;
	game.console_output = true;
	game.command = play::command;
	game.load_save = [](const std::string& a_name) -> std::string {
		const auto saves = RE::BGSSaveLoadManager::GetSingleton();
		if (!saves) {
			return "the save manager isn't ready";
		}
		saves->Load(a_name.c_str());
		return {};
	};
	game.sample = [] {
		const auto timer = RE::BSTimer::GetSingleton();
		const double fps = timer && timer->realTimeDelta > 0 ? 1.0 / timer->realTimeDelta : 0.0;
		const auto player = RE::PlayerCharacter::GetSingleton();
		const auto cell = player ? player->GetParentCell() : nullptr;
		return std::pair{ fps, std::string(cell ? cell->GetFormEditorID() : "") };
	};
	if (rmm::start(std::move(game), REL::Module::get().version().string("."))) {
		// Only in a game rmm started: a normal game stays unpatched.
		rmm::note(hook_vprint() ? "console output: hooked" : "console output: read from the console log");
		SKSE::GetMessagingInterface()->RegisterListener(on_message);
	}
	return true;
}
