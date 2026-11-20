# rmm's harness plugin for Fallout 4. See harness/README.md for building.
cmake_minimum_required(VERSION 3.25)
project(rmm_harness VERSION 0.1.0 LANGUAGES CXX)

set(COMMONLIB_F4_DIR "" CACHE PATH "A CommonLibF4 checkout (github.com/alandtse/CommonLibF4)")
if(NOT EXISTS "${COMMONLIB_F4_DIR}/CommonLibF4/CMakeLists.txt")
	message(FATAL_ERROR "Set COMMONLIB_F4_DIR to a CommonLibF4 checkout")
endif()
set(BUILD_TESTS OFF CACHE BOOL "" FORCE)
set(ENABLE_FALLOUT_VR OFF CACHE BOOL "" FORCE)
add_subdirectory("${COMMONLIB_F4_DIR}/CommonLibF4" CommonLibF4 EXCLUDE_FROM_ALL)

find_package(nlohmann_json CONFIG REQUIRED)
add_library(rmm_harness SHARED src/main.cpp src/play.cpp ../common/harness.cpp)
target_compile_features(rmm_harness PRIVATE cxx_std_23)
target_include_directories(rmm_harness PRIVATE ../common)
target_link_libraries(rmm_harness PRIVATE CommonLibF4::CommonLibF4 nlohmann_json::nlohmann_json ws2_32 windowscodecs)
