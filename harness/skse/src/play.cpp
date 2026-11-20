// Playing the game for rmm's tests (SPEC.md "In-game test harness"): read its state, report what
// happens in it, call Papyrus, and drive dialogue, menus, attacks and controls. Each command
// comes as JSON from harness.cpp on the main thread and answers once through its reply.
#include "play.h"

#include "harness.h"

#include <RE/Skyrim.h>
#include <SKSE/SKSE.h>

#include <nlohmann/json.hpp>

#include <chrono>
#include <cmath>
#include <format>
#include <limits>
#include <set>
#include <numbers>
#include <thread>

using json = nlohmann::json;

namespace
{
	using Reply = rmm::Game::Reply;

	std::string hex(RE::FormID a_id)
	{
		return std::format("{:08X}", a_id);
	}

	bool same(std::string_view a, std::string_view b)
	{
		return a.size() == b.size() && _strnicmp(a.data(), b.data(), a.size()) == 0;
	}

	RE::TESQuest* quest_named(const std::string& a_name)
	{
		for (const auto q : RE::TESDataHandler::GetSingleton()->GetFormArray<RE::TESQuest>()) {
			if (q && same(q->GetFormEditorID(), a_name)) {
				return q;
			}
		}
		return nullptr;
	}

	// "player", a form id in hex as the game shows it (00012EB7, 0x12EB7), or an editor id.
	RE::TESForm* form_of(const json& a_v)
	{
		if (!a_v.is_string()) {
			return a_v.is_number_unsigned() ? RE::TESForm::LookupByID(a_v.get<RE::FormID>()) : nullptr;
		}
		auto s = a_v.get<std::string>();
		if (same(s, "player")) {
			return RE::PlayerCharacter::GetSingleton();
		}
		if (s.size() > 2 && same(s.substr(0, 2), "0x")) {
			s = s.substr(2);
		}
		if (!s.empty() && s.size() <= 8 && s.find_first_not_of("0123456789abcdefABCDEF") == std::string::npos) {
			if (const auto f = RE::TESForm::LookupByID(static_cast<RE::FormID>(std::stoul(s, nullptr, 16)))) {
				return f;
			}
		}
		if (const auto f = RE::TESForm::LookupByEditorID(s)) {
			return f;
		}
		return quest_named(s);
	}

	float av(RE::Actor* a_actor, RE::ActorValue a_av)
	{
		return a_actor->AsActorValueOwner()->GetActorValue(a_av);
	}

	float distance(RE::TESObjectREFR* a_ref)
	{
		const auto player = RE::PlayerCharacter::GetSingleton();
		return a_ref == player ? 0.0f : player->GetPosition().GetDistance(a_ref->GetPosition());
	}

	json ref_info(RE::TESObjectREFR* a_ref)
	{
		const auto pos = a_ref->GetPosition();
		const auto base = a_ref->GetBaseObject();
		json       j{ { "ref", hex(a_ref->GetFormID()) }, { "name", a_ref->GetDisplayFullName() }, { "base", base ? hex(base->GetFormID()) : "" },
				  { "distance", std::round(distance(a_ref)) }, { "pos", { std::round(pos.x), std::round(pos.y), std::round(pos.z) } }, { "disabled", a_ref->IsDisabled() } };
		if (const auto cell = a_ref->GetParentCell()) {
			j["cell"] = cell->GetFormEditorID();
		}
		if (const auto actor = a_ref->As<RE::Actor>()) {
			j["dead"] = actor->IsDead();
			j["in_combat"] = actor->IsInCombat();
			j["health"] = std::round(av(actor, RE::ActorValue::kHealth));
			if (actor != RE::PlayerCharacter::GetSingleton()) {
				j["hostile"] = actor->IsHostileToActor(RE::PlayerCharacter::GetSingleton());
			}
		} else if (base) {
			j["type"] = RE::FormTypeToString(base->GetFormType());
		}
		return j;
	}

	json open_menus()
	{
		json       menus = json::array();
		const auto ui = RE::UI::GetSingleton();
		for (const auto& [name, entry] : ui->menuMap) {
			const std::string_view n(name.c_str());
			if (entry.menu && ui->IsMenuOpen(n) && n != "HUD Menu" && n != "Cursor Menu" && n != "Fader Menu") {
				menus.push_back(n);
			}
		}
		return menus;
	}

	// The player, what's around, and what the command asks about: quests, items, refs.
	json state(const json& a_cmd)
	{
		const auto player = RE::PlayerCharacter::GetSingleton();
		const auto cell = player->GetParentCell();
		const auto pos = player->GetPosition();
		json       p{ { "cell", cell ? cell->GetFormEditorID() : "" }, { "cell_name", cell ? cell->GetName() : "" },
				  { "pos", { std::round(pos.x), std::round(pos.y), std::round(pos.z) } }, { "heading", std::round(player->data.angle.z * 180.0f / RE::NI_PI) },
				  { "level", player->GetLevel() }, { "in_combat", player->IsInCombat() }, { "dead", player->IsDead() },
				  { "weapon_drawn", player->AsActorState()->IsWeaponDrawn() } };
		if (const auto ws = player->GetWorldspace()) {
			p["worldspace"] = ws->GetFormEditorID();
		}
		for (const auto& [key, value] : { std::pair{ "health", RE::ActorValue::kHealth }, std::pair{ "magicka", RE::ActorValue::kMagicka }, std::pair{ "stamina", RE::ActorValue::kStamina } }) {
			p[key] = { std::round(av(player, value)), std::round(player->AsActorValueOwner()->GetPermanentActorValue(value)) };
		}
		json out{ { "player", p }, { "menus", open_menus() } };

		const float radius = a_cmd.value("radius", 3000.0f);
		json        actors = json::array();
		json        objects = json::array();
		for (auto& handle : RE::ProcessLists::GetSingleton()->highActorHandles) {
			const auto actor = handle.get();
			if (actor && actor.get() != player && distance(actor.get()) <= radius) {
				actors.push_back(ref_info(actor.get()));
			}
		}
		std::set<RE::FormID> seen;  // a reference on a cell border is listed by both cells
		RE::TES::GetSingleton()->ForEachReferenceInRange(player, radius, [&](RE::TESObjectREFR* a_ref) {
			const auto base = a_ref && seen.insert(a_ref->GetFormID()).second ? a_ref->GetBaseObject() : nullptr;
			using F = RE::FormType;
			static constexpr std::array kinds{ F::Door, F::Container, F::Activator, F::Furniture, F::Flora, F::Book, F::Weapon, F::Armor, F::Misc, F::Ingredient, F::AlchemyItem, F::KeyMaster, F::Scroll, F::Ammo, F::SoulGem };
			if (base && !a_ref->IsDisabled() && std::ranges::find(kinds, base->GetFormType()) != kinds.end()) {
				objects.push_back(ref_info(a_ref));
			}
			return RE::BSContainer::ForEachResult::kContinue;
		});
		const auto nearest = [](json& a_list, std::size_t a_keep) {
			std::sort(a_list.begin(), a_list.end(), [](const json& a, const json& b) { return a["distance"].get<float>() < b["distance"].get<float>(); });
			if (a_list.size() > a_keep) {
				a_list.erase(a_list.begin() + static_cast<std::ptrdiff_t>(a_keep), a_list.end());
			}
		};
		nearest(actors, 20);
		nearest(objects, 25);
		out["actors"] = actors;
		out["objects"] = objects;

		if (a_cmd.contains("quests")) {
			json quests = json::object();
			for (const auto& q : a_cmd["quests"]) {
				const auto quest = q.is_string() ? quest_named(q.get<std::string>()) : nullptr;
				quests[q.is_string() ? q.get<std::string>() : q.dump()] = quest ? json{ { "stage", quest->GetCurrentStageID() }, { "running", quest->IsRunning() }, { "completed", quest->IsCompleted() } } : json(nullptr);
			}
			out["quests"] = quests;
		}
		if (a_cmd.contains("items")) {
			const auto counts = player->GetInventoryCounts();
			json       items = json::object();
			for (const auto& i : a_cmd["items"]) {
				const auto form = form_of(i);
				const auto obj = form ? form->As<RE::TESBoundObject>() : nullptr;
				const auto it = obj ? counts.find(obj) : counts.end();
				items[i.is_string() ? i.get<std::string>() : i.dump()] = form ? json(it == counts.end() ? 0 : it->second) : json(nullptr);
			}
			out["items"] = items;
		}
		if (a_cmd.contains("refs")) {
			json refs = json::object();
			for (const auto& r : a_cmd["refs"]) {
				const auto form = form_of(r);
				const auto ref = form ? form->AsReference() : nullptr;
				refs[r.is_string() ? r.get<std::string>() : r.dump()] = ref ? ref_info(ref) : json(nullptr);
			}
			out["refs"] = refs;
		}
		return out;
	}

	// ---- Papyrus ----

	using Variable = RE::BSScript::Variable;

	json from_variable(const Variable& a_v)
	{
		if (a_v.IsNoneObject() || a_v.IsNoneArray()) {
			return nullptr;
		}
		if (a_v.IsObject()) {
			if (const auto form = a_v.Unpack<RE::TESForm*>()) {
				return { { "form", hex(form->GetFormID()) }, { "name", form->GetName() } };
			}
			return "object";
		}
		if (a_v.IsArray()) {
			json       out = json::array();
			const auto arr = a_v.GetArray();
			for (std::uint32_t i = 0; arr && i < arr->size(); ++i) {
				out.push_back(from_variable((*arr)[i]));
			}
			return out;
		}
		if (a_v.IsBool()) {
			return a_v.GetBool();
		}
		if (a_v.IsInt()) {
			return a_v.GetSInt();
		}
		if (a_v.IsFloat()) {
			return a_v.GetFloat();
		}
		if (a_v.IsString()) {
			return std::string(a_v.GetString());
		}
		return nullptr;
	}

	struct Args : RE::BSScript::IFunctionArguments
	{
		std::vector<Variable> values;
		bool                  operator()(RE::BSScrapArray<Variable>& a_dst) const override
		{
			a_dst.resize(static_cast<RE::BSScrapArray<Variable>::size_type>(values.size()));
			for (std::size_t i = 0; i < values.size(); ++i) {
				a_dst[static_cast<RE::BSScrapArray<Variable>::size_type>(i)] = values[i];
			}
			return true;
		}
	};

	class Done : public RE::BSScript::IStackCallbackFunctor
	{
	public:
		explicit Done(Reply a_reply) :
			reply(std::move(a_reply)) {}
		void operator()(Variable a_result) override { reply("", json{ { "value", from_variable(a_result) } }.dump()); }
		void SetObject(const RE::BSTSmartPointer<RE::BSScript::Object>&) override {}

		Reply reply;
		Args  args;
	};

	std::string type_name(const RE::BSScript::TypeInfo& a_t)
	{
		if (a_t.IsObject()) {
			const auto info = a_t.GetTypeInfo();
			return info ? info->GetName() : "Object";
		}
		switch (a_t.GetRawType()) {
		case RE::BSScript::TypeInfo::RawType::kInt:
			return "Int";
		case RE::BSScript::TypeInfo::RawType::kFloat:
			return "Float";
		case RE::BSScript::TypeInfo::RawType::kBool:
			return "Bool";
		case RE::BSScript::TypeInfo::RawType::kString:
			return "String";
		default:
			return "Array";
		}
	}

	// The function, looking in the class and then its parents for a method.
	RE::BSScript::IFunction* find_function(const std::string& a_class, const std::string& a_fn, bool a_static, std::string& a_owner)
	{
		const auto                                     vm = RE::BSScript::Internal::VirtualMachine::GetSingleton();
		RE::BSTSmartPointer<RE::BSScript::ObjectTypeInfo> info;
		if (!vm->GetScriptObjectType1(RE::BSFixedString(a_class), info) || !info) {
			return nullptr;
		}
		for (auto t = info.get(); t; t = a_static ? nullptr : t->GetParent()) {
			const auto n = a_static ? t->GetNumGlobalFuncs() : t->GetNumMemberFuncs();
			for (std::uint32_t i = 0; i < n; ++i) {
				const auto f = a_static ? t->GetGlobalFuncIter()[i].func.get() : t->GetMemberFuncIter()[i].func.get();
				if (f && same(f->GetName().c_str(), a_fn)) {
					a_owner = t->GetName();
					return f;
				}
			}
		}
		return nullptr;
	}

	std::string signature(RE::BSScript::IFunction* a_f)
	{
		std::string s = std::string(a_f->GetObjectTypeName().c_str()) + "." + a_f->GetName().c_str() + "(";
		for (std::uint32_t i = 0; i < a_f->GetParamCount(); ++i) {
			RE::BSFixedString        name;
			RE::BSScript::TypeInfo t;
			a_f->GetParam(i, name, t);
			s += (i ? ", " : "") + type_name(t) + " " + name.c_str();
		}
		return s + ")";
	}

	// {"class":"Actor","fn":"PathToReference","self":"player","args":["0001A694",1.0]}: arguments in the
	// function's order, forms by id or editor id. Answers when the function returns (latent ones
	// such as PathToReference when they finish) with {"value": …}.
	void papyrus(const json& a_cmd, Reply a_reply)
	{
		const auto cls = a_cmd.value("class", std::string{});
		const auto fn = a_cmd.value("fn", std::string{});
		const bool method = a_cmd.contains("self") && !a_cmd["self"].is_null();
		RE::TESForm* self = method ? form_of(a_cmd["self"]) : nullptr;
		if (method && !self) {
			return a_reply("no form " + a_cmd["self"].dump(), "");
		}
		std::string owner;
		const auto  f = find_function(cls, fn, !method, owner);
		if (!f) {
			return a_reply(std::format("no {} function {}.{} (method calls need self; global ones don't)", method ? "member" : "global", cls, fn), "");
		}
		const auto args = a_cmd.value("args", json::array());
		if (args.size() != f->GetParamCount()) {
			return a_reply(std::format("{} takes {} arguments: {}", fn, f->GetParamCount(), signature(f)), "");
		}
		RE::BSTSmartPointer<Done> done(new Done(a_reply));
		for (std::uint32_t i = 0; i < f->GetParamCount(); ++i) {
			RE::BSFixedString        name;
			RE::BSScript::TypeInfo t;
			f->GetParam(i, name, t);
			const auto& a = args[i];
			Variable    v;
			if (t.IsObject()) {
				if (!a.is_null()) {
					const auto form = form_of(a);
					if (!form) {
						return a_reply(std::format("argument {} ({}): no form {}", i + 1, name.c_str(), a.dump()), "");
					}
					RE::BSScript::PackHandle(&v, form, static_cast<RE::VMTypeID>(form->GetFormType()));
				}
			} else {
				switch (t.GetRawType()) {
				case RE::BSScript::TypeInfo::RawType::kInt:
					if (!a.is_number()) {
						return a_reply(std::format("argument {} ({}) is an Int: {}", i + 1, name.c_str(), signature(f)), "");
					}
					v.SetSInt(a.get<std::int32_t>());
					break;
				case RE::BSScript::TypeInfo::RawType::kFloat:
					if (!a.is_number()) {
						return a_reply(std::format("argument {} ({}) is a Float: {}", i + 1, name.c_str(), signature(f)), "");
					}
					v.SetFloat(a.get<float>());
					break;
				case RE::BSScript::TypeInfo::RawType::kBool:
					v.SetBool(a.is_boolean() ? a.get<bool>() : a.is_number() && a.get<double>() != 0.0);
					break;
				case RE::BSScript::TypeInfo::RawType::kString:
					v.SetString(a.is_string() ? a.get<std::string>() : a.dump());
					break;
				default:
					return a_reply(std::format("argument {} ({}) is an array, which isn't supported: {}", i + 1, name.c_str(), signature(f)), "");
				}
			}
			done->args.values.push_back(v);
		}
		const auto                                        vm = RE::BSScript::Internal::VirtualMachine::GetSingleton();
		RE::BSTSmartPointer<RE::BSScript::IStackCallbackFunctor> cb(done.get());
		bool                                              ok = false;
		if (method) {
			const auto handle = vm->GetObjectHandlePolicy()->GetHandleForObject(static_cast<RE::VMTypeID>(self->GetFormType()), self);
			ok = vm->DispatchMethodCall2(handle, RE::BSFixedString(owner), RE::BSFixedString(fn), &done->args, cb);
		} else {
			ok = vm->DispatchStaticCall(RE::BSFixedString(owner), RE::BSFixedString(fn), &done->args, cb);
		}
		if (!ok) {
			a_reply(std::format("the game refused the call to {} (is {} a {}?)", signature(f), a_cmd.value("self", json()).dump(), cls), "");
		}
	}

	// ---- Dialogue and menus ----

	json dialogue()
	{
		const auto mtm = RE::MenuTopicManager::GetSingleton();
		json       topics = json::array();
		json       out{ { "open", RE::UI::GetSingleton()->IsMenuOpen(RE::DialogueMenu::MENU_NAME) } };
		if (!mtm) {
			return out;
		}
		REX::W32::EnterCriticalSection(&mtm->criticalSection);
		if (mtm->dialogueList) {
			int i = 0;
			for (const auto d : *mtm->dialogueList) {
				if (d) {
					topics.push_back({ { "index", i }, { "text", d->topicText.c_str() }, { "new", d->neverSaid } });
				}
				++i;
			}
		}
		REX::W32::LeaveCriticalSection(&mtm->criticalSection);
		if (const auto speaker = mtm->speaker.get()) {
			out["speaker"] = { { "ref", hex(speaker->GetFormID()) }, { "name", speaker->GetDisplayFullName() } };
		}
		out["topics"] = topics;
		return out;
	}

	// GameDelegate.call(callback, [args]) in a menu's movie: how its buttons report to the game.
	// Run on the main (or UI) thread and wait for it, from a worker thread.
	template <class F>
	void on_main(F a_f, bool a_ui = false)
	{
		std::atomic<bool> done{ false };
		const auto        task = [&] {
            a_f();
            done = true;
		};
		if (a_ui) {
			SKSE::GetTaskInterface()->AddUITask(task);
		} else {
			SKSE::GetTaskInterface()->AddTask(task);
		}
		while (!done) {
			std::this_thread::sleep_for(std::chrono::milliseconds(1));
		}
	}

	std::string menu_call(const std::string& a_menu, const std::string& a_callback, const json& a_args)
	{
		const auto ui = RE::UI::GetSingleton();
		if (!ui->IsMenuOpen(a_menu)) {
			return a_menu + " isn't open";
		}
		const auto movie = ui->GetMovieView(a_menu);
		if (!movie) {
			return a_menu + " has no movie";
		}
		RE::GFxValue list;
		movie->CreateArray(&list);
		for (const auto& a : a_args) {
			if (a.is_number()) {
				list.PushBack(RE::GFxValue(a.get<double>()));
			} else if (a.is_boolean()) {
				list.PushBack(RE::GFxValue(a.get<bool>()));
			} else {
				RE::GFxValue s;
				movie->CreateString(&s, a.is_string() ? a.get<std::string>().c_str() : a.dump().c_str());
				list.PushBack(s);
			}
		}
		if (a_callback.starts_with("_root") || a_callback.starts_with("_global")) {
			std::vector<RE::GFxValue> direct(list.GetArraySize());
			for (std::uint32_t i = 0; i < direct.size(); ++i) {
				list.GetElement(i, &direct[i]);
			}
			return movie->Invoke(a_callback.c_str(), nullptr, direct.data(), static_cast<std::uint32_t>(direct.size())) ? std::string{} : "no function " + a_callback + " in " + a_menu;
		}
		RE::GFxValue args[2];
		movie->CreateString(&args[0], a_callback.c_str());
		args[1] = list;
		return movie->Invoke("gfx.io.GameDelegate.call", nullptr, args, 2) ? std::string{} : "the menu has no GameDelegate";
	}

	json from_gfx(const RE::GFxValue& a_v, int a_depth)
	{
		if (a_v.IsBool()) {
			return a_v.GetBool();
		}
		if (a_v.IsNumber()) {
			return a_v.GetNumber();
		}
		if (a_v.IsString()) {
			return a_v.GetString();
		}
		if (a_v.IsArray()) {
			json out = json::array();
			for (std::uint32_t i = 0; i < a_v.GetArraySize() && i < 50; ++i) {
				RE::GFxValue e;
				a_v.GetElement(i, &e);
				out.push_back(a_depth > 0 ? from_gfx(e, a_depth - 1) : json("..."));
			}
			return out;
		}
		if (a_v.IsObject()) {
			json out = json::object();
			a_v.VisitMembers([&](const char* a_name, const RE::GFxValue& a_m) {
				if (out.size() < 100) {
					out[a_name] = a_depth > 0 ? from_gfx(a_m, a_depth - 1) : json(a_m.IsObject() ? "{...}" : a_m.IsNumber() || a_m.IsString() || a_m.IsBool() ? from_gfx(a_m, 0) : json(nullptr));
				}
			});
			return out;
		}
		return nullptr;
	}

	// Read a menu's ActionScript variable: "_root.DialogueMenu_mc.eMenuState". Objects list their members.
	json menu_get(const std::string& a_menu, const std::string& a_path)
	{
		const auto movie = RE::UI::GetSingleton()->GetMovieView(a_menu);
		RE::GFxValue v;
		if (!movie || !movie->GetVariable(&v, a_path.c_str())) {
			return json{ { "error", movie ? "no " + a_path + " in " + a_menu : a_menu + " isn't open" } };
		}
		return json{ { "value", from_gfx(v, 1) } };
	}

	// Pick a topic by its index or by words in its text.
	void choose(const json& a_cmd, Reply a_reply)
	{
		const auto d = dialogue();
		if (!d["open"].get<bool>()) {
			return a_reply("the Dialogue Menu isn't open", "");
		}
		int index = -1;
		if (a_cmd.contains("index")) {
			index = a_cmd["index"].get<int>();
		} else {
			const auto want = a_cmd.value("text", std::string{});
			for (const auto& t : d["topics"]) {
				const auto text = t["text"].get<std::string>();
				const auto hit = std::search(text.begin(), text.end(), want.begin(), want.end(), [](char a, char b) { return std::tolower(static_cast<unsigned char>(a)) == std::tolower(static_cast<unsigned char>(b)); });
				if (!want.empty() && hit != text.end()) {
					index = t["index"].get<int>();
					break;
				}
			}
		}
		if (index < 0 || index >= static_cast<int>(d["topics"].size())) {
			return a_reply("no such topic; the topics are " + d["topics"].dump(), "");
		}
		const auto text = d["topics"][static_cast<std::size_t>(index)]["text"].get<std::string>();
		// Through the menu's own click handler: calling the game's TopicClicked directly leaves the
		// menu waiting for a click it never saw. Then wait for the answer to finish (the topic list
		// is back, or the menu closed), keeping what the speaker said from the menu's subtitle.
		std::thread([index, text, a_reply] {
			static constexpr auto menu = RE::DialogueMenu::MENU_NAME;
			bool                  picked = false;
			on_main([&] {
				const auto movie = RE::UI::GetSingleton()->GetMovieView(menu);
				picked = movie && movie->SetVariable("_root.DialogueMenu_mc.TopicList.selectedIndex", RE::GFxValue(static_cast<double>(index))) &&
				         movie->Invoke("_root.DialogueMenu_mc.onSelectionClick", nullptr, nullptr, 0);
			}, true);
			if (!picked) {
				return a_reply("the Dialogue Menu didn't take the click", "");
			}
			json       said = json::array();
			int        state = 2;
			const auto start = std::chrono::steady_clock::now();
			while (state != 1 && state != -1 && std::chrono::steady_clock::now() - start < std::chrono::seconds(60)) {
				std::this_thread::sleep_for(std::chrono::milliseconds(250));
				std::string line;
				on_main([&] {
					const auto   movie = RE::UI::GetSingleton()->GetMovieView(menu);
					RE::GFxValue v;
					state = movie && movie->GetVariable(&v, "_root.DialogueMenu_mc.eMenuState") && v.IsNumber() ? static_cast<int>(v.GetNumber()) : -1;
					if (movie && movie->GetVariable(&v, "_root.DialogueMenu_mc.SubtitleText.text") && v.IsString()) {
						line = v.GetString();
					}
				}, true);
				line.erase(0, line.find_first_not_of(" \r\n"));
				line.erase(line.find_last_not_of(" \r\n") + 1);
				if (!line.empty() && (said.empty() || said.back() != line)) {
					said.push_back(line);
				}
			}
			json out{ { "chose", text }, { "said", said } };
			on_main([&] { out["then"] = dialogue(); });
			a_reply(state == 1 || state == -1 ? "" : "the answer didn't finish within 60 seconds", out.dump());
		}).detach();
	}

	// ---- Attacks, facing and controls ----

	void face(RE::TESObjectREFR* a_target)
	{
		const auto player = RE::PlayerCharacter::GetSingleton();
		const auto d = a_target->GetPosition() - player->GetPosition();
		// The engine's setters, so the camera turns with the player.
		player->SetHeading(std::atan2(d.x, d.y));
		player->SetLooking(0.0f);
	}

	// ActionRightAttack and the rest (BGSAction form ids), performed as the player's controls would.
	std::string act(const json& a_cmd)
	{
		static const std::map<std::string, RE::FormID> actions{
			{ "attack", 0x13005 }, { "left_attack", 0x13004 }, { "dual_attack", 0x50C96 }, { "power_attack", 0x13383 },
			{ "left_power_attack", 0x2E2F6 }, { "dual_power_attack", 0x2E2F7 }, { "left_release", 0x13451 }, { "release", 0x13454 }
		};
		const auto name = a_cmd.value("action", std::string("attack"));
		const auto it = actions.find(name);
		const auto action = it == actions.end() ? nullptr : RE::TESForm::LookupByID<RE::BGSAction>(it->second);
		if (!action) {
			return "no action " + name + "; one of attack, power_attack, left_attack, left_power_attack, dual_attack, dual_power_attack, release (bow), left_release";
		}
		const auto player = RE::PlayerCharacter::GetSingleton();
		RE::TESObjectREFR* target = nullptr;
		if (a_cmd.contains("target") && !a_cmd["target"].is_null()) {
			const auto form = form_of(a_cmd["target"]);
			target = form ? form->AsReference() : nullptr;
			if (!target) {
				return "no reference " + a_cmd["target"].dump();
			}
			face(target);
		}
		if (!player->AsActorState()->IsWeaponDrawn()) {
			player->DrawWeaponMagicHands(true);
		}
		// ponytail: the action data isn't freed (0x60 bytes per attack, test runs only).
		const auto data = RE::TESActionData::Create();
		data->source = RE::NiPointer<RE::TESObjectREFR>(player);
		data->target = RE::NiPointer<RE::TESObjectREFR>(target);
		data->action = action;
		using perform_t = bool(RE::TESActionData*);
		static REL::Relocation<perform_t> perform{ RELOCATION_ID(40551, 41557) };
		return perform(data) ? std::string{} : "the game didn't start the " + name + " (drawing the weapon takes a moment; try again)";
	}

	// A control (as the Controls menu names it: Forward, Activate, Jump, Sprint, Sneak, Ready Weapon,
	// Right Attack/Block, Tween Menu, Wait...) pressed for a_ms, sent to the game's input handlers
	// each frame. Works behind other windows: nothing goes through Windows' input.
	void button(RE::INPUT_DEVICE a_device, const std::string& a_control, std::uint32_t a_key, float a_value, float a_held)
	{
		on_main([&] {
			if (const auto e = RE::ButtonEvent::Create(a_device, a_control, a_key, a_value, a_held)) {
				RE::InputEvent* list = e;
				RE::BSInputDeviceManager::GetSingleton()->SendEvent(&list);
				RE::free(e);
			}
		});
	}

	void press(const json& a_cmd, Reply a_reply)
	{
		const auto control = a_cmd.value("control", std::string{});
		const auto ms = std::clamp(a_cmd.value("ms", 100), 0, 30000);
		const auto map = RE::ControlMap::GetSingleton();
		auto       key = map->GetMappedKey(control, RE::INPUT_DEVICE::kKeyboard);
		auto       device = RE::INPUT_DEVICE::kKeyboard;
		if (key == RE::ControlMap::kInvalid) {
			key = map->GetMappedKey(control, RE::INPUT_DEVICE::kMouse);
			device = RE::INPUT_DEVICE::kMouse;
		}
		if (key == RE::ControlMap::kInvalid) {
			return a_reply("no control '" + control + "' (names as in the Controls menu: Forward, Back, Strafe Left, Activate, Jump, Sprint, Sneak, Ready Weapon, Right Attack/Block, Left Attack/Block, Tween Menu, Journal, Wait, Favorites)", "");
		}
		std::thread([control, ms, key, device, a_reply] {
			const auto start = std::chrono::steady_clock::now();
			const auto held = [&] { return std::chrono::duration<float>(std::chrono::steady_clock::now() - start).count(); };
			button(device, control, key, 1.0f, 0.0f);
			while (held() * 1000.0f < static_cast<float>(ms)) {
				std::this_thread::sleep_for(std::chrono::milliseconds(16));
				button(device, control, key, 1.0f, held());
			}
			button(device, control, key, 0.0f, held());
			a_reply("", "");
		}).detach();
	}

	// Walk the player straight at a reference with the game's auto-move (what the Auto-Move key
	// sets), turning toward it, until within reach. Keys sent from outside the input loop only move
	// the player on some frames. When the player stops getting closer (a rock, a wall), the player
	// is moved next to it instead and the result says so. The game's own pathing
	// (Actor.PathToReference) doesn't move the player, even AI-driven.
	void walk(const json& a_cmd, Reply a_reply)
	{
		const auto form = form_of(a_cmd.value("target", json()));
		const auto ref = form ? form->AsReference() : nullptr;
		if (!ref) {
			return a_reply("no reference " + a_cmd.value("target", json()).dump(), "");
		}
		const auto within = a_cmd.value("within", 150.0f);
		const auto limit = std::chrono::seconds(std::clamp(a_cmd.value("timeout", 60), 1, 600));
		std::thread([handle = ref->CreateRefHandle(), within, limit, a_reply] {
			const auto  start = std::chrono::steady_clock::now();
			auto        best = (std::numeric_limits<float>::max)();
			auto        best_at = start;
			float       dist = 0.0f;
			std::string error;
			float       stuck = 0.0f;
			const auto  move = [](bool a_on) { on_main([=] { RE::PlayerControls::GetSingleton()->data.autoMove = a_on; }); };
			for (;;) {
				bool gone = false;
				on_main([&] {
					const auto target = handle.get();
					if (!target) {
						gone = true;
						return;
					}
					const auto player = RE::PlayerCharacter::GetSingleton();
					const auto d = target->GetPosition() - player->GetPosition();
					dist = player->GetPosition().GetDistance(target->GetPosition());
					const auto off = std::remainder(std::atan2(d.x, d.y) - player->GetAngleZ(), 2.0f * std::numbers::pi_v<float>);
					if (dist > within && std::abs(off) > 0.17f) {
						face(target.get());
					}
					// Set every time: turning, a menu or the player's own keys can turn it off.
					RE::PlayerControls::GetSingleton()->data.autoMove = dist > within;
				});
				const auto now = std::chrono::steady_clock::now();
				if (gone) {
					error = "the target is gone";
				} else if (dist <= within) {
					break;
				} else if (dist < best - 30.0f) {
					best = dist;
					best_at = now;
				} else if (now - best_at > std::chrono::seconds(3)) {
					stuck = dist;
					break;
				}
				if (error.empty() && now - start > limit) {
					error = std::format("still {:.0f} away after {} seconds", dist, limit.count());
				}
				if (!error.empty()) {
					break;
				}
				std::this_thread::sleep_for(std::chrono::milliseconds(50));
			}
			move(false);
			if (stuck > 0.0f) {
				on_main([&] {
					if (const auto target = handle.get()) {
						RE::PlayerCharacter::GetSingleton()->MoveTo(target.get());
					}
				});
				std::this_thread::sleep_for(std::chrono::milliseconds(500));
				on_main([&] {
					if (const auto target = handle.get()) {
						dist = RE::PlayerCharacter::GetSingleton()->GetPosition().GetDistance(target->GetPosition());
					}
				});
			}
			json out{ { "arrived", true }, { "distance", std::round(dist) } };
			if (stuck > 0.0f) {
				out["moved_from"] = std::round(stuck);
				out["note"] = "something was in the way, so the player was moved there";
			}
			a_reply(error, error.empty() ? out.dump() : "");
		}).detach();
	}

	// ---- Events ----

	struct Events :
		RE::BSTEventSink<RE::TESDeathEvent>,
		RE::BSTEventSink<RE::TESCombatEvent>,
		RE::BSTEventSink<RE::TESQuestStageEvent>,
		RE::BSTEventSink<RE::TESActivateEvent>
	{
		static json who(const RE::NiPointer<RE::TESObjectREFR>& a_ref)
		{
			return a_ref ? json{ { "ref", hex(a_ref->GetFormID()) }, { "name", a_ref->GetDisplayFullName() } } : json(nullptr);
		}

		RE::BSEventNotifyControl ProcessEvent(const RE::TESDeathEvent* a_e, RE::BSTEventSource<RE::TESDeathEvent>*) override
		{
			if (a_e && a_e->dead) {
				rmm::game_event(json{ { "what", "death" }, { "actor", who(a_e->actorDying) }, { "killer", who(a_e->actorKiller) } }.dump());
			}
			return RE::BSEventNotifyControl::kContinue;
		}
		RE::BSEventNotifyControl ProcessEvent(const RE::TESCombatEvent* a_e, RE::BSTEventSource<RE::TESCombatEvent>*) override
		{
			static constexpr std::array states{ "none", "combat", "searching" };
			const auto                  s = static_cast<std::size_t>(a_e ? a_e->newState.get() : RE::ACTOR_COMBAT_STATE::kNone);
			if (a_e) {
				rmm::game_event(json{ { "what", "combat" }, { "actor", who(a_e->actor) }, { "target", who(a_e->targetActor) }, { "state", s < states.size() ? states[s] : "?" } }.dump());
			}
			return RE::BSEventNotifyControl::kContinue;
		}
		RE::BSEventNotifyControl ProcessEvent(const RE::TESQuestStageEvent* a_e, RE::BSTEventSource<RE::TESQuestStageEvent>*) override
		{
			const auto q = a_e ? RE::TESForm::LookupByID<RE::TESQuest>(a_e->formID) : nullptr;
			if (q) {
				rmm::game_event(json{ { "what", "stage" }, { "quest", q->GetFormEditorID() }, { "stage", a_e->stage } }.dump());
			}
			return RE::BSEventNotifyControl::kContinue;
		}
		RE::BSEventNotifyControl ProcessEvent(const RE::TESActivateEvent* a_e, RE::BSTEventSource<RE::TESActivateEvent>*) override
		{
			// Only the player's: NPCs open doors all over the world all the time.
			if (a_e && a_e->actionRef && a_e->actionRef->IsPlayerRef()) {
				rmm::game_event(json{ { "what", "activate" }, { "target", who(a_e->objectActivated) }, { "by", who(a_e->actionRef) } }.dump());
			}
			return RE::BSEventNotifyControl::kContinue;
		}
	};
}

namespace play
{
	void watch_events()
	{
		static Events events;
		const auto    holder = RE::ScriptEventSourceHolder::GetSingleton();
		holder->AddEventSink<RE::TESDeathEvent>(&events);
		holder->AddEventSink<RE::TESCombatEvent>(&events);
		holder->AddEventSink<RE::TESQuestStageEvent>(&events);
		holder->AddEventSink<RE::TESActivateEvent>(&events);
	}

	void command(const std::string& a_raw, rmm::Game::Reply a_reply)
	{
		const auto cmd = json::parse(a_raw, nullptr, false);
		const auto name = cmd.value("cmd", std::string{});
		try {
			if (!RE::PlayerCharacter::GetSingleton() || !RE::PlayerCharacter::GetSingleton()->GetParentCell()) {
				return a_reply("not in the game yet (load a save or coc first)", "");
			}
			if (name == "state") {
				a_reply("", state(cmd).dump());
			} else if (name == "papyrus") {
				papyrus(cmd, a_reply);
			} else if (name == "dialogue") {
				a_reply("", dialogue().dump());
			} else if (name == "choose") {
				choose(cmd, a_reply);
			} else if (name == "menu_call") {
				SKSE::GetTaskInterface()->AddUITask([cmd, a_reply] { a_reply(menu_call(cmd.value("menu", std::string{}), cmd.value("callback", std::string{}), cmd.value("args", json::array())), ""); });
			} else if (name == "menu_get") {
				SKSE::GetTaskInterface()->AddUITask([cmd, a_reply] {
					const auto v = menu_get(cmd.value("menu", std::string{}), cmd.value("path", std::string{}));
					a_reply(v.contains("error") ? v["error"].get<std::string>() : "", v.dump());
				});
			} else if (name == "act") {
				a_reply(act(cmd), "");
			} else if (name == "face") {
				const auto form = form_of(cmd.value("target", json()));
				const auto ref = form ? form->AsReference() : nullptr;
				if (ref) {
					face(ref);
				}
				a_reply(ref ? "" : "no reference " + cmd.value("target", json()).dump(), "");
			} else if (name == "press") {
				press(cmd, a_reply);
			} else if (name == "walk") {
				walk(cmd, a_reply);
			} else if (name == "activate") {
				// As the player's Activate key does: talks to people, opens doors and containers.
				const auto form = form_of(cmd.value("target", json()));
				const auto ref = form ? form->AsReference() : nullptr;
				if (!ref) {
					return a_reply("no reference " + cmd.value("target", json()).dump(), "");
				}
				const auto done = ref->ActivateRef(RE::PlayerCharacter::GetSingleton(), 0, nullptr, 1, false);
				a_reply(done ? "" : "the game didn't activate " + cmd.value("target", json()).dump(), json{ { "activated", true } }.dump());
			} else {
				a_reply("unknown command '" + name + "'", "");
			}
		} catch (const std::exception& e) {
			a_reply(std::string("bad command: ") + e.what(), "");
		}
	}
}
