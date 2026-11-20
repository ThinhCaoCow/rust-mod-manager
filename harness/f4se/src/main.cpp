// rmm's harness for Fallout 4 (CommonLibF4): the game side of `test_run`.
#include "harness.h"

#include <F4SE/F4SE.h>
#include <RE/Fallout.h>

namespace
{
	// The main menu counts once it is open and the game's data has loaded, whichever comes last:
	// Fallout 4 can open it before loading finishes, and before kGameDataReady.
	std::atomic<bool> menu_open{ false };
	std::atomic<bool> data_ready{ false };
	std::atomic<bool> announced{ false };
	std::string       loading;  // the save named by kPreLoadGame

	void main_menu_when_ready()
	{
		if (menu_open && data_ready && !announced.exchange(true)) {
			rmm::send_to_back();
			rmm::event("main_menu");
		}
	}

	struct MenuWatch : RE::BSTEventSink<RE::MenuOpenCloseEvent>
	{
		RE::BSEventNotifyControl ProcessEvent(const RE::MenuOpenCloseEvent& a_event, RE::BSTEventSource<RE::MenuOpenCloseEvent>*) override
		{
			const std::string name(a_event.menuName.c_str());
			rmm::note(name + (a_event.opening ? " opened" : " closed"));
			if (a_event.opening && name == "MainMenu") {
				menu_open = true;
				main_menu_when_ready();
			}
			// Closing a loading screen in the world is a finished load: of a save, or of a coc,
			// which also starts a game from the main menu (and fires no load message).
			const auto player = RE::PlayerCharacter::GetSingleton();
			if (!a_event.opening && a_event.menuName == RE::LoadingMenu::MENU_NAME && player && player->GetParentCell()) {
				rmm::event("game_loaded", loading);
				loading.clear();
			}
			return RE::BSEventNotifyControl::kContinue;
		}
	};

	// Watch menus from the first message that has a UI; a main menu already open counts.
	void watch_menus()
	{
		static std::atomic<bool> watching{ false };
		const auto               ui = RE::UI::GetSingleton();
		if (!ui) {
			return;
		}
		if (!watching.exchange(true)) {
			static MenuWatch watch;
			ui->RegisterSink<RE::MenuOpenCloseEvent>(&watch);
		}
		if (ui->GetMenuOpen("MainMenu")) {
			menu_open = true;
		}
	}

	// What it printed: the text the console log gained while it ran.
	std::string console(const std::string& a_line, std::string& a_out)
	{
		const auto        log = RE::ConsoleLog::GetSingleton();
		const std::size_t before = log ? log->buffer.length() : 0;
		RE::Console::ExecuteCommand(a_line.c_str());
		if (log) {
			const std::string_view all(log->buffer.c_str(), log->buffer.length());
			a_out = all.size() > before ? std::string(all.substr(before)) : std::string{};
		}
		return {};
	}

	void on_message(F4SE::MessagingInterface::Message* a_msg)
	{
		switch (a_msg->type) {
		case F4SE::MessagingInterface::kInputLoaded:
			rmm::send_to_back();
			rmm::note("input loaded");
			watch_menus();
			break;
		case F4SE::MessagingInterface::kGameDataReady:
			{
				// Sent twice: false as loading starts, true once it is done.
				const bool ready = static_cast<bool>(a_msg->data);
				rmm::note(ready ? "data ready" : "data loading");
				rmm::send_to_back();
				// The game pauses while its window isn't focused, which stops the main-thread tasks that
				// run rmm's commands; a test run usually starts behind other windows. Memory only.
				if (const auto s = RE::GetINISetting("bAlwaysActive:General")) {
					s->SetBinary(true);
				}
				watch_menus();
				if (ready && !data_ready.exchange(true)) {
					rmm::event("data_loaded");
					main_menu_when_ready();
				}
				break;
			}
		case F4SE::MessagingInterface::kPreLoadGame:
			loading = a_msg->data ? std::string(static_cast<const char*>(a_msg->data), a_msg->dataLen) : std::string{};
			break;
		default:
			break;
		}
	}
}

// Next-gen and later runtimes read this; the original 1.10.163 asks F4SEPlugin_Query.
F4SE_EXPORT constinit auto F4SEPlugin_Version = []() noexcept {
	F4SE::PluginVersionData v{};
	v.PluginVersion({ 0, 1, 0 });
	v.PluginName("rmm_harness");
	v.AuthorName("rmm");
	v.UsesAddressLibrary(true);
	v.UsesSigScanning(false);
	v.IsLayoutDependent(true);
	v.HasNoStructUse(false);
	v.CompatibleVersions({ F4SE::RUNTIME_LATEST });
	return v;
}();

F4SE_EXPORT bool F4SEPlugin_Query(const F4SE::QueryInterface* a_f4se, F4SE::PluginInfo* a_info)
{
	a_info->infoVersion = F4SE::PluginInfo::kVersion;
	a_info->name = "rmm_harness";
	a_info->version = 1;
	return !a_f4se->IsEditor();
}

F4SE_PLUGIN_LOAD(const F4SE::LoadInterface* a_f4se)
{
	F4SE::Init(a_f4se, false);
	rmm::Game game;
	game.on_main_thread = [](std::function<void()> a_task) { F4SE::GetTaskInterface()->AddTask(std::move(a_task)); };
	game.console = console;
	game.console_output = true;
	game.load_save = [](const std::string& a_name) {
		std::string out;
		return console("load \"" + a_name + "\"", out);
	};
	game.sample = [] {
		const auto timer = RE::BSTimer::GetSingleton();
		const double fps = timer && timer->realTimeDelta > 0 ? 1.0 / timer->realTimeDelta : 0.0;
		const auto player = RE::PlayerCharacter::GetSingleton();
		const auto cell = player ? player->GetParentCell() : nullptr;
		return std::pair{ fps, std::string(cell ? cell->GetFormEditorID() : "") };
	};
	if (rmm::start(std::move(game), REL::Module::get().version().string("."))) {
		F4SE::GetMessagingInterface()->RegisterListener(on_message);
	}
	return true;
}
