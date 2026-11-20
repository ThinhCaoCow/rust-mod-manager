#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <WinSock2.h>
#include <WS2tcpip.h>
#include <Windows.h>
#include <wincodec.h>
#include <wrl/client.h>

#include "harness.h"

#include <atomic>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <mutex>
#include <nlohmann/json.hpp>
#include <thread>
#include <vector>

namespace rmm
{
	namespace
	{
		Game              game;
		SOCKET            sock = INVALID_SOCKET;
		std::atomic<bool> up{ false };
		std::mutex        writing;
		// Events from before the connection is up (the game loads data while it connects).
		std::vector<std::string> backlog;
		// One heartbeat queued at a time, so a long load doesn't pile them up.
		std::atomic<bool> sampling{ false };

		void send_line(const std::string& a_line)
		{
			const char* p = a_line.data();
			auto        left = static_cast<int>(a_line.size());
			while (left > 0) {
				const int n = ::send(sock, p, left, 0);
				if (n <= 0) {
					up = false;
					return;
				}
				p += n;
				left -= n;
			}
		}

		void send(const nlohmann::json& a_json)
		{
			std::lock_guard lock(writing);
			if (up) {
				send_line(a_json.dump(-1, ' ', false, nlohmann::json::error_handler_t::replace) + "\n");
			} else if (sock == INVALID_SOCKET) {
				backlog.push_back(a_json.dump(-1, ' ', false, nlohmann::json::error_handler_t::replace) + "\n");
			}
		}

		// The game's main window: this process's largest visible top-level window.
		HWND game_window()
		{
			struct Best
			{
				HWND wnd = nullptr;
				long area = 0;
			} best;
			EnumWindows([](HWND a_wnd, LPARAM a_best) -> BOOL {
				DWORD pid = 0;
				GetWindowThreadProcessId(a_wnd, &pid);
				RECT rc{};
				if (pid == GetCurrentProcessId() && IsWindowVisible(a_wnd) && !GetWindow(a_wnd, GW_OWNER) && GetClientRect(a_wnd, &rc)) {
					auto& b = *reinterpret_cast<Best*>(a_best);
					if (const long area = rc.right * rc.bottom; area > b.area) {
						b = { a_wnd, area };
					}
				}
				return TRUE;
			},
				reinterpret_cast<LPARAM>(&best));
			return best.wnd;
		}

		std::string save_png(HBITMAP a_bmp, int a_width, int a_height, const std::string& a_path)
		{
			using Microsoft::WRL::ComPtr;
			std::wstring path(static_cast<std::size_t>(MultiByteToWideChar(CP_UTF8, 0, a_path.c_str(), -1, nullptr, 0)), L'\0');
			MultiByteToWideChar(CP_UTF8, 0, a_path.c_str(), -1, path.data(), static_cast<int>(path.size()));
			const HRESULT com = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
			HRESULT       hr = S_OK;
			{
				ComPtr<IWICImagingFactory>    wic;
				ComPtr<IWICBitmap>            bitmap;
				ComPtr<IWICStream>            stream;
				ComPtr<IWICBitmapEncoder>     encoder;
				ComPtr<IWICBitmapFrameEncode> frame;
				WICPixelFormatGUID            format = GUID_WICPixelFormat24bppBGR;
				hr = CoCreateInstance(CLSID_WICImagingFactory, nullptr, CLSCTX_INPROC_SERVER, IID_PPV_ARGS(&wic));
				if (SUCCEEDED(hr)) hr = wic->CreateBitmapFromHBITMAP(a_bmp, nullptr, WICBitmapIgnoreAlpha, &bitmap);
				if (SUCCEEDED(hr)) hr = wic->CreateStream(&stream);
				if (SUCCEEDED(hr)) hr = stream->InitializeFromFilename(path.c_str(), GENERIC_WRITE);
				if (SUCCEEDED(hr)) hr = wic->CreateEncoder(GUID_ContainerFormatPng, nullptr, &encoder);
				if (SUCCEEDED(hr)) hr = encoder->Initialize(stream.Get(), WICBitmapEncoderNoCache);
				if (SUCCEEDED(hr)) hr = encoder->CreateNewFrame(&frame, nullptr);
				if (SUCCEEDED(hr)) hr = frame->Initialize(nullptr);
				if (SUCCEEDED(hr)) hr = frame->SetSize(static_cast<UINT>(a_width), static_cast<UINT>(a_height));
				if (SUCCEEDED(hr)) hr = frame->SetPixelFormat(&format);
				if (SUCCEEDED(hr)) hr = frame->WriteSource(bitmap.Get(), nullptr);
				if (SUCCEEDED(hr)) hr = frame->Commit();
				if (SUCCEEDED(hr)) hr = encoder->Commit();
			}
			if (SUCCEEDED(com)) {
				CoUninitialize();
			}
			char why[64];
			std::snprintf(why, sizeof why, "couldn't write the PNG (0x%08lX)", static_cast<unsigned long>(hr));
			return SUCCEEDED(hr) ? std::string{} : std::string(why);
		}

		// A PNG of the game's window. PW_RENDERFULLCONTENT reads what the GPU drew, even while
		// other windows cover it (not while it is minimised).
		std::string capture(const std::string& a_path)
		{
			const HWND wnd = game_window();
			RECT       rc{};
			if (a_path.empty()) {
				return "screenshot needs a path";
			}
			if (!wnd || !GetClientRect(wnd, &rc) || rc.right <= 0 || rc.bottom <= 0) {
				return "no game window to capture";
			}
			const HDC     screen = GetDC(wnd);
			const HDC     mem = CreateCompatibleDC(screen);
			const HBITMAP bmp = CreateCompatibleBitmap(screen, rc.right, rc.bottom);
			const auto    old = SelectObject(mem, bmp);
			const BOOL    printed = PrintWindow(wnd, mem, PW_CLIENTONLY | 0x2 /* PW_RENDERFULLCONTENT */);
			SelectObject(mem, old);
			DeleteDC(mem);
			ReleaseDC(wnd, screen);
			const auto error = printed ? save_png(bmp, rc.right, rc.bottom, a_path) : std::string("the window couldn't be captured");
			DeleteObject(bmp);
			return error;
		}

		// "out" is the error when there is one, else what the command printed.
		void reply(std::int64_t a_id, const std::string& a_error, const std::string& a_out = {})
		{
			send({ { "ev", "result" }, { "id", a_id }, { "ok", a_error.empty() }, { "out", a_error.empty() ? a_out : a_error } });
		}

		void reply_data(std::int64_t a_id, const std::string& a_error, const std::string& a_data)
		{
			nlohmann::json j{ { "ev", "result" }, { "id", a_id }, { "ok", a_error.empty() }, { "out", a_error } };
			if (!a_data.empty()) {
				j["data"] = nlohmann::json::parse(a_data, nullptr, false);
			}
			send(j);
		}

		std::string console(const std::string& a_line)
		{
			std::string out;
			return game.console(a_line, out);
		}

		void handle(const nlohmann::json& a_cmd)
		{
			const auto id = a_cmd.value("id", std::int64_t{ 0 });
			const auto cmd = a_cmd.value("cmd", std::string{});
			if (cmd == "console") {
				game.on_main_thread([id, line = a_cmd.value("line", std::string{})] {
					if (game.console_later) {
						return game.console_later(line, [id](const std::string& a_error, const std::string& a_out) { reply(id, a_error, a_out); });
					}
					std::string out;
					const auto  error = game.console(line, out);
					reply(id, error, out);
				});
			} else if (cmd == "load_save") {
				game.on_main_thread([id, name = a_cmd.value("name", std::string{})] { reply(id, game.load_save(name)); });
			} else if (cmd == "quit") {
				reply(id, "");
				game.on_main_thread([] { console("qqq"); });
			} else if (cmd == "screenshot") {
				reply(id, capture(a_cmd.value("path", std::string{})));
			} else if (game.command) {
				game.on_main_thread([id, raw = a_cmd.dump(-1, ' ', false, nlohmann::json::error_handler_t::replace)] {
					game.command(raw, [id](const std::string& a_error, const std::string& a_data) { reply_data(id, a_error, a_data); });
				});
			} else {
				reply(id, "unknown command '" + cmd + "'");
			}
		}

		void run(std::uint16_t a_port, std::string a_token, std::string a_runtime)
		{
			WSADATA wsa{};
			if (WSAStartup(MAKEWORD(2, 2), &wsa) != 0) {
				return;
			}
			sockaddr_in addr{};
			addr.sin_family = AF_INET;
			addr.sin_port = htons(a_port);
			inet_pton(AF_INET, "127.0.0.1", &addr.sin_addr);
			SOCKET s = INVALID_SOCKET;
			for (int i = 0; i < 60; ++i) {
				s = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
				if (connect(s, reinterpret_cast<sockaddr*>(&addr), sizeof addr) == 0) {
					break;
				}
				closesocket(s);
				s = INVALID_SOCKET;
				std::this_thread::sleep_for(std::chrono::milliseconds(500));
			}
			{
				std::lock_guard lock(writing);
				if (s == INVALID_SOCKET) {
					backlog.clear();
					return;
				}
				sock = s;
				up = true;
				send_line(nlohmann::json{ { "ev", "hello" }, { "token", a_token }, { "runtime", a_runtime }, { "console_output", game.console_output } }.dump(-1, ' ', false, nlohmann::json::error_handler_t::replace) + "\n");
				for (const auto& line : backlog) {
					send_line(line);
				}
				backlog.clear();
			}
			std::thread([] {
				while (up) {
					std::this_thread::sleep_for(std::chrono::seconds(1));
					if (!sampling.exchange(true)) {
						game.on_main_thread([] {
							const auto [fps, cell] = game.sample();
							send({ { "ev", "heartbeat" }, { "fps", fps }, { "cell", cell } });
							sampling = false;
						});
					}
				}
			}).detach();
			std::string buf;
			char        chunk[4096];
			while (up) {
				const int n = recv(sock, chunk, sizeof chunk, 0);
				if (n <= 0) {
					break;
				}
				buf.append(chunk, static_cast<std::size_t>(n));
				for (std::size_t nl; (nl = buf.find('\n')) != std::string::npos; buf.erase(0, nl + 1)) {
					const auto cmd = nlohmann::json::parse(buf.substr(0, nl), nullptr, false);
					if (cmd.is_object()) {
						handle(cmd);
					}
				}
			}
			up = false;
		}
	}

	bool start(Game a_game, const std::string& a_runtime)
	{
		const char* port = std::getenv("RMM_HARNESS_PORT");
		const char* token = std::getenv("RMM_HARNESS_TOKEN");
		if (!port || !token) {
			return false;
		}
		game = std::move(a_game);
		std::thread(run, static_cast<std::uint16_t>(std::atoi(port)), std::string(token), a_runtime).detach();
		return true;
	}

	void send_to_back()
	{
		if (std::getenv("RMM_HARNESS_BEHIND")) {
			if (const HWND wnd = game_window()) {
				SetWindowPos(wnd, HWND_BOTTOM, 0, 0, 0, 0, SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE);
			}
		}
	}

	void event(const char* a_event, const std::string& a_save)
	{
		nlohmann::json j{ { "ev", a_event } };
		if (std::string_view(a_event) == "game_loaded") {
			j["save"] = a_save;
		}
		send(j);
	}

	void note(const std::string& a_what)
	{
		send({ { "ev", "note" }, { "what", a_what } });
	}

	void game_event(const std::string& a_json)
	{
		auto j = nlohmann::json::parse(a_json, nullptr, false);
		if (j.is_object()) {
			j["ev"] = "game";
			send(j);
		}
	}
}
