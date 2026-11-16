// rmm's harness for Skyrim SE/AE (CommonLibSSE-NG): the game side of `test_run`.
#include "harness.h"

#include <RE/Skyrim.h>
#include <SKSE/SKSE.h>

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

	std::string console(const std::string& a_line)
	{
		const auto factory = RE::IFormFactory::GetConcreteFormFactoryByType<RE::Script>();
		const auto script = factory ? factory->Create() : nullptr;
		if (!script) {
			return "cannot make a console script";
		}
		script->SetCommand(a_line);
		script->CompileAndRun(nullptr);
		delete script;
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
	SKSE::Init(a_skse);
	rmm::Game game;
	game.on_main_thread = [](std::function<void()> a_task) { SKSE::GetTaskInterface()->AddTask(std::move(a_task)); };
	game.console = console;
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
		SKSE::GetMessagingInterface()->RegisterListener(on_message);
	}
	return true;
}
