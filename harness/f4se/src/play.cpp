// Fallout 4's side of rmm's playing steps. The Skyrim plugin has the same commands; dialogue, menus,
// attacks and controls are Skyrim-only for now and answer with an error naming what works here.
#include "play.h"

#include "harness.h"

#include <F4SE/F4SE.h>
#include <RE/Fallout.h>

#include <nlohmann/json.hpp>

#include <chrono>
#include <cmath>
#include <format>
#include <limits>
#include <numbers>
#include <set>
#include <thread>

using json = nlohmann::json;

// Declared by CommonLibF4 and not defined anywhere it links.
RE::BSScript::IStackCallbackFunctor::~IStackCallbackFunctor() = default;

namespace
{
	using Reply = rmm::Game::Reply;
	using Variable = RE::BSScript::Variable;

	std::string hex(std::uint32_t a_id)
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

	// "player", a form id in hex as the game shows it (0001A4D7, 0x1A4D7), or an editor id.
	RE::TESForm* form_of(const json& a_v)
	{
		if (!a_v.is_string()) {
			return a_v.is_number_unsigned() ? RE::TESForm::GetFormByID(a_v.get<std::uint32_t>()) : nullptr;
		}
		auto s = a_v.get<std::string>();
		if (same(s, "player")) {
			return RE::PlayerCharacter::GetSingleton();
		}
		if (s.size() > 2 && same(s.substr(0, 2), "0x")) {
			s = s.substr(2);
		}
		if (!s.empty() && s.size() <= 8 && s.find_first_not_of("0123456789abcdefABCDEF") == std::string::npos) {
			if (const auto f = RE::TESForm::GetFormByID(static_cast<std::uint32_t>(std::stoul(s, nullptr, 16)))) {
				return f;
			}
		}
		if (const auto f = RE::TESForm::GetFormByEditorID(RE::BSFixedString(s))) {
			return f;
		}
		return quest_named(s);
	}

	RE::TESObjectREFR* ref_of(const json& a_v)
	{
		const auto form = form_of(a_v);
		return form ? form->As<RE::TESObjectREFR>() : nullptr;
	}

	float av(RE::Actor* a_actor, RE::ActorValueInfo* a_info, bool a_max = false)
	{
		return a_info ? (a_max ? a_actor->GetPermanentActorValue(*a_info) : a_actor->GetActorValue(*a_info)) : 0.0f;
	}

	float distance(RE::TESObjectREFR* a_ref)
	{
		const auto player = RE::PlayerCharacter::GetSingleton();
		return a_ref == player ? 0.0f : player->GetPosition().GetDistance(a_ref->GetPosition());
	}

	json ref_info(RE::TESObjectREFR* a_ref)
	{
		const auto pos = a_ref->GetPosition();
		const auto base = a_ref->data.objectReference;
		json       j{ { "ref", hex(a_ref->GetFormID()) }, { "name", a_ref->GetDisplayFullName() }, { "base", base ? hex(base->GetFormID()) : "" },
				  { "distance", std::round(distance(a_ref)) }, { "pos", { std::round(pos.x), std::round(pos.y), std::round(pos.z) } }, { "disabled", a_ref->IsDisabled() } };
		if (const auto cell = a_ref->GetParentCell()) {
			j["cell"] = cell->GetFormEditorID();
		}
		if (const auto actor = a_ref->As<RE::Actor>()) {
			j["dead"] = actor->IsDead(false);
			j["in_combat"] = actor->IsInCombat();
			j["health"] = std::round(av(actor, RE::ActorValue::GetSingleton()->health));
		} else if (base) {
			j["type"] = base->GetFormTypeString();
		}
		return j;
	}

	json open_menus()
	{
		json       menus = json::array();
		const auto ui = RE::UI::GetSingleton();
		const RE::BSAutoReadLock lock{ RE::UI::GetMenuMapRWLock() };
		for (const auto& [name, entry] : ui->menuMap) {
			const std::string_view n(name.c_str());
			if (entry.menu && ui->GetMenuOpen(name) && n != "HUDMenu" && n != "CursorMenu" && n != "FaderMenu") {
				menus.push_back(n);
			}
		}
		return menus;
	}

	// The player, what's around, and what the command asks about: quests, refs.
	json state(const json& a_cmd)
	{
		const auto player = RE::PlayerCharacter::GetSingleton();
		const auto cell = player->GetParentCell();
		const auto pos = player->GetPosition();
		const auto avs = RE::ActorValue::GetSingleton();
		json       p{ { "cell", cell ? cell->GetFormEditorID() : "" }, { "pos", { std::round(pos.x), std::round(pos.y), std::round(pos.z) } },
				  { "heading", std::round(player->data.angle.z * 180.0f / std::numbers::pi_v<float>) }, { "level", player->GetLevel() },
				  { "in_combat", player->IsInCombat() }, { "dead", player->IsDead(false) }, { "weapon_drawn", player->GetWeaponMagicDrawn() } };
		for (const auto& [key, value] : { std::pair{ "health", avs->health }, std::pair{ "action_points", avs->actionPoints } }) {
			p[key] = { std::round(av(player, value)), std::round(av(player, value, true)) };
		}
		json out{ { "player", p }, { "menus", open_menus() } };

		const float radius = a_cmd.value("radius", 3000.0f);
		json        actors = json::array();
		json        objects = json::array();
		for (const auto& handle : RE::ProcessLists::GetSingleton()->highActorHandles) {
			const auto actor = handle.get();
			if (actor && actor.get() != player && distance(actor.get()) <= radius) {
				actors.push_back(ref_info(actor.get()));
			}
		}
		// The player's cell only: Fallout 4 has no range search to call.
		if (cell && radius > 0.0f) {
			using F = RE::ENUM_FORM_ID;
			static constexpr std::array kinds{ F::kDOOR, F::kCONT, F::kACTI, F::kFURN, F::kFLOR, F::kBOOK, F::kWEAP, F::kARMO, F::kMISC, F::kINGR, F::kALCH, F::kKEYM, F::kAMMO, F::kTERM, F::kNOTE };
			for (const auto& r : cell->references) {
				const auto ref = r.get();
				const auto base = ref ? ref->data.objectReference : nullptr;
				if (base && !ref->IsDisabled() && distance(ref) <= radius && std::ranges::find(kinds, base->GetFormType()) != kinds.end()) {
					objects.push_back(ref_info(ref));
				}
			}
		}
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
				// QUEST_DATA flags: 1 running, 2 completed.
				quests[q.is_string() ? q.get<std::string>() : q.dump(-1, ' ', false, nlohmann::json::error_handler_t::replace)] = quest ? json{ { "stage", quest->currentStage }, { "running", (quest->data.flags & 1) != 0 }, { "completed", (quest->data.flags & 2) != 0 } } : json(nullptr);
			}
			out["quests"] = quests;
		}
		if (a_cmd.contains("refs")) {
			json refs = json::object();
			for (const auto& r : a_cmd["refs"]) {
				const auto ref = ref_of(r);
				refs[r.is_string() ? r.get<std::string>() : r.dump(-1, ' ', false, nlohmann::json::error_handler_t::replace)] = ref ? ref_info(ref) : json(nullptr);
			}
			out["refs"] = refs;
		}
		return out;
	}

	// ---- Papyrus ----

	RE::BSTSmartPointer<RE::BSScript::IVirtualMachine> vm()
	{
		const auto game = RE::GameVM::GetSingleton();
		return game ? game->GetVM() : nullptr;
	}

	// A form as the script object of its own type (ObjectReference, Actor, Quest...).
	bool pack(Variable& a_var, RE::TESForm* a_form)
	{
		const auto                                      v = vm();
		const auto                                      type = static_cast<std::uint32_t>(a_form->GetFormType());
		RE::BSTSmartPointer<RE::BSScript::ObjectTypeInfo> info;
		if (!v || !v->GetScriptObjectType(type, info) || !info) {
			return false;
		}
		auto&      handles = v->GetObjectHandlePolicy();
		const auto handle = handles.GetHandleForObject(type, a_form);
		if (handle == handles.EmptyHandle()) {
			return false;
		}
		RE::BSTSmartPointer<RE::BSScript::Object> object;
		if (!v->FindBoundObject(handle, info->name.c_str(), false, object, false) && v->CreateObject(info->name, object) && object) {
			v->GetObjectBindPolicy().BindObject(object, handle);
		}
		if (!object) {
			return false;
		}
		a_var = std::move(object);
		return true;
	}

	json from_variable(const Variable& a_v)
	{
		if (a_v.is<std::nullptr_t>()) {
			return nullptr;
		}
		if (a_v.is<RE::BSScript::Object>()) {
			const auto object = RE::BSScript::get<RE::BSScript::Object>(a_v);
			const auto v = vm();
			if (object && v) {
				const auto handle = object->GetHandle();
				auto&      handles = v->GetObjectHandlePolicy();
				for (const auto type : { static_cast<std::uint32_t>(RE::ENUM_FORM_ID::kREFR), static_cast<std::uint32_t>(RE::ENUM_FORM_ID::kACHR) }) {
					if (handles.HandleIsType(type, handle)) {
						if (const auto ref = static_cast<RE::TESForm*>(handles.GetObjectForHandle(type, handle))) {
							return { { "form", hex(ref->GetFormID()) }, { "name", ref->GetFormEditorID() } };
						}
					}
				}
				// Forms that aren't references: the handle's low 32 bits are the form id.
				if (const auto form = RE::TESForm::GetFormByID(static_cast<std::uint32_t>(handle & 0xFFFFFFFF))) {
					return { { "form", hex(form->GetFormID()) }, { "name", form->GetFormEditorID() } };
				}
			}
			return "object";
		}
		if (a_v.is<RE::BSScript::Array>()) {
			json       out = json::array();
			const auto arr = RE::BSScript::get<RE::BSScript::Array>(a_v);
			for (std::uint32_t i = 0; arr && i < arr->size(); ++i) {
				out.push_back(from_variable((*arr)[i]));
			}
			return out;
		}
		if (a_v.is<bool>()) {
			return RE::BSScript::get<bool>(a_v);
		}
		if (a_v.is<std::int32_t>()) {
			return RE::BSScript::get<std::int32_t>(a_v);
		}
		if (a_v.is<float>()) {
			return RE::BSScript::get<float>(a_v);
		}
		if (a_v.is<RE::BSFixedString>()) {
			return std::string(RE::BSScript::get<RE::BSFixedString>(a_v).c_str());
		}
		return nullptr;
	}

	// Arguments the original runtime's VM takes: its own function object, made by the game from
	// these values (Papyrus::FunctionArgs does this for arguments known at compile time).
	class Arguments : public Papyrus::detail::FunctionArgsBase
	{
	public:
		Arguments(RE::BSScript::IVirtualMachine* a_vm, const std::vector<Variable>& a_values) :
			FunctionArgsBase(a_vm)
		{
			scrap.resize(static_cast<std::uint32_t>(a_values.size()));
			for (std::size_t i = 0; i < a_values.size(); ++i) {
				scrap[static_cast<std::uint32_t>(i)] = a_values[i];
			}
			args = new RE::BSScript::ArrayWrapper<Variable>(scrap, *vm);
		}
		Arguments(const Arguments&) = delete;
		Arguments& operator=(const Arguments&) = delete;
		~Arguments() { delete args; }
		auto get() { return Papyrus::detail::CreateThreadScrapFunction(*this); }

	private:
		RE::BSScrapArray<Variable> scrap;
	};

	class Done : public RE::BSScript::IStackCallbackFunctor
	{
	public:
		explicit Done(Reply a_reply) :
			reply(std::move(a_reply)) {}
		void CallQueued() override {}
		void CallCanceled() override { reply("the game canceled the call", ""); }
		void StartMultiDispatch() override {}
		void EndMultiDispatch() override {}
		void operator()(Variable a_result) override { reply("", json{ { "value", from_variable(a_result) } }.dump(-1, ' ', false, nlohmann::json::error_handler_t::replace)); }

		Reply reply;
	};

	std::string type_name(const RE::BSScript::TypeInfo& a_t)
	{
		using R = RE::BSScript::TypeInfo::RawType;
		if (a_t.IsObject()) {
			const auto info = a_t.GetObjectTypeInfo();
			return info ? info->name.c_str() : "Object";
		}
		switch (a_t.GetRawType()) {
		case R::kInt:
			return "Int";
		case R::kFloat:
			return "Float";
		case R::kBool:
			return "Bool";
		case R::kString:
			return "String";
		default:
			return "Array or Struct";
		}
	}

	RE::BSScript::IFunction* find_function(const std::string& a_class, const std::string& a_fn, bool a_static, std::string& a_owner)
	{
		const auto                                      v = vm();
		RE::BSTSmartPointer<RE::BSScript::ObjectTypeInfo> info;
		if (!v || !v->GetScriptObjectType(RE::BSFixedString(a_class), info) || !info) {
			return nullptr;
		}
		for (auto t = info.get(); t; t = a_static ? nullptr : t->GetParent()) {
			const auto n = a_static ? t->GetNumGlobalFuncs() : t->GetNumMemberFuncs();
			for (std::uint32_t i = 0; i < n; ++i) {
				const auto f = a_static ? t->GetGlobalFuncIter()[i].func.get() : t->GetMemberFuncIter()[i].func.get();
				if (f && same(f->GetName().c_str(), a_fn)) {
					a_owner = t->name.c_str();
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
			RE::BSFixedString      name;
			RE::BSScript::TypeInfo t;
			a_f->GetParam(i, name, t);
			s += (i ? ", " : "") + type_name(t) + " " + name.c_str();
		}
		return s + ")";
	}

	// {"class":"Actor","fn":"GetValue","self":"player","args":["Health"]}: arguments in the
	// function's order, forms by id or editor id. Answers when the function returns.
	void papyrus(const json& a_cmd, Reply a_reply)
	{
		const auto   cls = a_cmd.value("class", std::string{});
		const auto   fn = a_cmd.value("fn", std::string{});
		const bool   method = a_cmd.contains("self") && !a_cmd["self"].is_null();
		RE::TESForm* self = method ? form_of(a_cmd["self"]) : nullptr;
		if (method && !self) {
			return a_reply("no form " + a_cmd["self"].dump(-1, ' ', false, nlohmann::json::error_handler_t::replace), "");
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
		std::vector<Variable> values;
		for (std::uint32_t i = 0; i < f->GetParamCount(); ++i) {
			RE::BSFixedString      name;
			RE::BSScript::TypeInfo t;
			f->GetParam(i, name, t);
			const auto& a = args[i];
			Variable    v;
			using R = RE::BSScript::TypeInfo::RawType;
			if (t.IsObject()) {
				if (!a.is_null()) {
					const auto form = form_of(a);
					if (!form || !pack(v, form)) {
						return a_reply(std::format("argument {} ({}): no form {}", i + 1, name.c_str(), a.dump(-1, ' ', false, nlohmann::json::error_handler_t::replace)), "");
					}
				}
			} else if (t.GetRawType() == R::kInt && a.is_number()) {
				v = a.get<std::int32_t>();
			} else if (t.GetRawType() == R::kFloat && a.is_number()) {
				v = a.get<float>();
			} else if (t.GetRawType() == R::kBool) {
				v = a.is_boolean() ? a.get<bool>() : a.is_number() && a.get<double>() != 0.0;
			} else if (t.GetRawType() == R::kString) {
				v = RE::BSFixedString(a.is_string() ? a.get<std::string>() : a.dump(-1, ' ', false, nlohmann::json::error_handler_t::replace));
			} else {
				return a_reply(std::format("argument {} ({}) doesn't fit: {}", i + 1, name.c_str(), signature(f)), "");
			}
			values.push_back(std::move(v));
		}
		const auto                                             v = vm();
		RE::BSTSmartPointer<RE::BSScript::IStackCallbackFunctor> cb(new Done(a_reply));
		const auto fill = [values](RE::BSScrapArray<Variable>& a_out) {
			a_out.resize(static_cast<std::uint32_t>(values.size()));
			for (std::size_t i = 0; i < values.size(); ++i) {
				a_out[static_cast<std::uint32_t>(i)] = values[i];
			}
			return true;
		};
		bool       ok = false;
		const auto handle = method ? v->GetObjectHandlePolicy().GetHandleForObject(static_cast<std::uint32_t>(self->GetFormType()), self) : 0;
		if (REL::Module::IsNG()) {
			ok = method ? v->DispatchMethodCall(handle, RE::BSFixedString(owner), RE::BSFixedString(fn), fill, cb) : v->DispatchStaticCall(RE::BSFixedString(owner), RE::BSFixedString(fn), fill, cb);
		} else {
			Arguments args(v.get(), values);
			ok = method ? Papyrus::detail::DispatchMethodCallOG(v.get(), handle, RE::BSFixedString(owner), RE::BSFixedString(fn), args.get(), cb) :
			              Papyrus::detail::DispatchStaticCallOG(v.get(), RE::BSFixedString(owner), RE::BSFixedString(fn), args.get(), cb);
		}
		if (!ok) {
			a_reply(std::format("the game refused the call to {} (is {} a {}?)", signature(f), a_cmd.value("self", json()).dump(-1, ' ', false, nlohmann::json::error_handler_t::replace), cls), "");
		}
	}

	// ---- Walking and facing ----

	// Run on the main thread and wait for it, from a worker thread.
	template <class F>
	void on_main(F a_f)
	{
		std::atomic<bool> done{ false };
		F4SE::GetTaskInterface()->AddTask([&] {
			a_f();
			done = true;
		});
		while (!done) {
			std::this_thread::sleep_for(std::chrono::milliseconds(1));
		}
	}

	void face(RE::TESObjectREFR* a_target)
	{
		const auto player = RE::PlayerCharacter::GetSingleton();
		const auto d = a_target->GetPosition() - player->GetPosition();
		player->SetHeading(std::atan2(d.x, d.y));
		player->data.angle.x = 0.0f;
	}

	// Walk straight at a reference with the game's auto-move, turning toward it, until within
	// reach; when something is in the way, move the player there and say so.
	void walk(const json& a_cmd, Reply a_reply)
	{
		const auto ref = ref_of(a_cmd.value("target", json()));
		if (!ref) {
			return a_reply("no reference " + a_cmd.value("target", json()).dump(-1, ' ', false, nlohmann::json::error_handler_t::replace), "");
		}
		const auto within = a_cmd.value("within", 150.0f);
		const auto limit = std::chrono::seconds(std::clamp(a_cmd.value("timeout", 60), 1, 600));
		std::thread([handle = ref->GetHandle(), id = ref->GetFormID(), within, limit, a_reply] {
			const auto  start = std::chrono::steady_clock::now();
			auto        best = (std::numeric_limits<float>::max)();
			auto        best_at = start;
			float       dist = 0.0f;
			float       stuck = 0.0f;
			std::string error;
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
					const auto off = std::remainder(std::atan2(d.x, d.y) - player->data.angle.z, 2.0f * std::numbers::pi_v<float>);
					if (dist > within && std::abs(off) > 0.17f) {
						face(target.get());
					}
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
			on_main([] { RE::PlayerControls::GetSingleton()->data.autoMove = false; });
			if (stuck > 0.0f) {
				on_main([&] { RE::Console::ExecuteCommand(std::format("player.moveto {:08X}", id).c_str()); });
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
			a_reply(error, error.empty() ? out.dump(-1, ' ', false, nlohmann::json::error_handler_t::replace) : "");
		}).detach();
	}

	// ---- Events ----

	struct Events :
		RE::BSTEventSink<RE::TESDeathEvent>,
		RE::BSTEventSink<RE::TESActivateEvent>,
		RE::BSTEventSink<RE::TESHitEvent>
	{
		static json who(const RE::NiPointer<RE::TESObjectREFR>& a_ref)
		{
			return a_ref ? json{ { "ref", hex(a_ref->GetFormID()) }, { "name", a_ref->GetDisplayFullName() } } : json(nullptr);
		}

		RE::BSEventNotifyControl ProcessEvent(const RE::TESDeathEvent& a_e, RE::BSTEventSource<RE::TESDeathEvent>*) override
		{
			if (a_e.dead) {
				rmm::game_event(json{ { "what", "death" }, { "actor", who(a_e.actorDying) }, { "killer", who(a_e.actorKiller) } }.dump(-1, ' ', false, nlohmann::json::error_handler_t::replace));
			}
			return RE::BSEventNotifyControl::kContinue;
		}
		RE::BSEventNotifyControl ProcessEvent(const RE::TESActivateEvent& a_e, RE::BSTEventSource<RE::TESActivateEvent>*) override
		{
			// Only the player's: NPCs open doors all over the world all the time.
			if (a_e.actionRef && a_e.actionRef.get() == RE::PlayerCharacter::GetSingleton()) {
				rmm::game_event(json{ { "what", "activate" }, { "target", who(a_e.objectActivated) }, { "by", who(a_e.actionRef) } }.dump(-1, ' ', false, nlohmann::json::error_handler_t::replace));
			}
			return RE::BSEventNotifyControl::kContinue;
		}
		RE::BSEventNotifyControl ProcessEvent(const RE::TESHitEvent& a_e, RE::BSTEventSource<RE::TESHitEvent>*) override
		{
			// Hits the player gives or takes: the fight a test starts.
			const auto player = RE::PlayerCharacter::GetSingleton();
			if (a_e.target.get() == player || a_e.cause.get() == player) {
				rmm::game_event(json{ { "what", "hit" }, { "target", who(a_e.target) }, { "by", who(a_e.cause) } }.dump(-1, ' ', false, nlohmann::json::error_handler_t::replace));
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
		RE::TESDeathEvent::GetEventSource()->RegisterSink(&events);
		RE::TESActivateEvent::GetEventSource()->RegisterSink(&events);
		RE::TESHitEvent::GetEventSource()->RegisterSink(&events);
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
				a_reply("", state(cmd).dump(-1, ' ', false, nlohmann::json::error_handler_t::replace));
			} else if (name == "papyrus") {
				papyrus(cmd, a_reply);
			} else if (name == "walk") {
				walk(cmd, a_reply);
			} else if (name == "face") {
				const auto ref = ref_of(cmd.value("target", json()));
				if (ref) {
					face(ref);
				}
				a_reply(ref ? "" : "no reference " + cmd.value("target", json()).dump(-1, ' ', false, nlohmann::json::error_handler_t::replace), "");
			} else if (name == "activate") {
				const auto ref = ref_of(cmd.value("target", json()));
				if (!ref) {
					return a_reply("no reference " + cmd.value("target", json()).dump(-1, ' ', false, nlohmann::json::error_handler_t::replace), "");
				}
				const auto done = ref->ActivateRef(RE::PlayerCharacter::GetSingleton(), nullptr, 1, false, false, false);
				a_reply(done ? "" : "the game didn't activate " + cmd.value("target", json()).dump(-1, ' ', false, nlohmann::json::error_handler_t::replace), json{ { "activated", true } }.dump(-1, ' ', false, nlohmann::json::error_handler_t::replace));
			} else {
				a_reply("dialogue, choose, button, menu_call, menu_get, attack and press are Skyrim-only for now; in Fallout 4 the harness has state, wait_for, call (Papyrus), move_to, activate and face, plus console commands", "");
			}
		} catch (const std::exception& e) {
			a_reply(std::string("bad command: ") + e.what(), "");
		}
	}
}
