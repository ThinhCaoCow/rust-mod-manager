# rmm's harness plugin for Skyrim SE/AE. See harness/README.md for building.
cmake_minimum_required(VERSION 3.25)
project(rmm_harness VERSION 0.1.0 LANGUAGES CXX)

set(COMMONLIB_SSE_DIR "" CACHE PATH "A CommonLibSSE-NG checkout (github.com/alandtse/CommonLibVR, branch ng)")
if(NOT EXISTS "${COMMONLIB_SSE_DIR}/CMakeLists.txt")
	message(FATAL_ERROR "Set COMMONLIB_SSE_DIR to a CommonLibSSE-NG checkout")
endif()
set(BUILD_TESTS OFF CACHE BOOL "" FORCE)
set(ENABLE_SKYRIM_VR OFF CACHE BOOL "" FORCE)
set(SKSE_SUPPORT_PATCH_SAFETY OFF CACHE BOOL "" FORCE)  # no trampolines here, and it fetches MinHook
add_subdirectory("${COMMONLIB_SSE_DIR}" CommonLibSSE EXCLUDE_FROM_ALL)
include("${COMMONLIB_SSE_DIR}/cmake/CommonLibSSE.cmake")  # add_commonlibsse_plugin(); only the prebuilt mode includes it

find_package(nlohmann_json CONFIG REQUIRED)
add_commonlibsse_plugin(rmm_harness
	NAME rmm_harness
	AUTHOR rmm
	USE_ADDRESS_LIBRARY
	SOURCES src/main.cpp src/play.cpp ../common/harness.cpp)
target_compile_features(rmm_harness PRIVATE cxx_std_23)
target_include_directories(rmm_harness PRIVATE ../common)
target_link_libraries(rmm_harness PRIVATE nlohmann_json::nlohmann_json ws2_32 windowscodecs)
# The plugin-info file that helper generates expects a precompiled header with CommonLib in it.
# harness.cpp can't have one (CommonLib's Windows declarations clash with Winsock), so only that
# file gets src/plugin_info.h.
set_source_files_properties("${CMAKE_CURRENT_BINARY_DIR}/__rmm_harnessPlugin.cpp" PROPERTIES
	COMPILE_OPTIONS "/FI${CMAKE_CURRENT_SOURCE_DIR}/src/plugin_info.h")
