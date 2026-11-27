# A hash of the plugin's harness source (common/ and <game>/src/, .cpp and .h, in path order, each
# file as "<path>\n<text>" with carriage returns dropped), sent in the hello so rmm can say when the
# DLL was built from other source than itself. crates/rmm-core/build.rs hashes the same way.
function(rmm_harness_source target game)
	get_filename_component(root "${CMAKE_CURRENT_FUNCTION_LIST_DIR}/.." ABSOLUTE)
	file(GLOB files RELATIVE "${root}" "${root}/common/*.cpp" "${root}/common/*.h" "${root}/${game}/src/*.cpp" "${root}/${game}/src/*.h")
	list(SORT files)
	set(all "")
	foreach(f IN LISTS files)
		file(READ "${root}/${f}" text)
		string(REPLACE "\r" "" text "${text}")
		string(APPEND all "${f}\n${text}")
		# Configure again when a source changes, so the hash follows it.
		set_property(DIRECTORY APPEND PROPERTY CMAKE_CONFIGURE_DEPENDS "${root}/${f}")
	endforeach()
	string(SHA256 hash "${all}")
	target_compile_definitions(${target} PRIVATE RMM_HARNESS_SOURCE="${hash}")
endfunction()
