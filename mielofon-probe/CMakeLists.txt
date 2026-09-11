# mielofon-probe - resident measurement probe daemon (C++23)
#
# One build definition for both targets (mirrors obserwrt):
#   - OpenWrt (via $(INCLUDE_DIR)/cmake.mk + the feed Makefile): links
#     libubus/libubox/libjson-c from the target staging, -fno-exceptions
#     -fno-rtti, image flags authoritative.
#   - plain Linux (default): for local build/test; finds libubus-dev etc.
#
# The daemon is config-free at runtime: all dynamic state (links, params) is
# pushed by the ucode agent over ubus; only the Prometheus textfile path and
# log level arrive via getopt() from procd.
cmake_minimum_required(VERSION 3.20)
project(mielofon-probe VERSION 0.1.0 LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 23)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

if(NOT CMAKE_BUILD_TYPE)
  set(CMAKE_BUILD_TYPE Release)
endif()

set(MIELOFON_PROBE_OS "${CMAKE_SYSTEM_NAME}" CACHE STRING "build_info os label")
set(MIELOFON_PROBE_ARCH "${CMAKE_SYSTEM_PROCESSOR}" CACHE STRING "build_info arch label")
set(MIELOFON_PROBE_VERSION "${PROJECT_VERSION}" CACHE STRING "build_info version")

set(MIELOFON_PROBE_CXX_FLAGS -Wall -Wextra -Werror
    -fno-exceptions -fno-rtti -fno-threadsafe-statics
    # libubus's ubus_object/type structs are aggregate-initialised by
    # designators and leave the per-connection fields (avl/id/path/...) unset;
    # -Wmissing-field-initializers would fire on every OpenWrt ubus object.
    -Wno-missing-field-initializers)

# libubus (base) + libubox + libjson-c (packages). OpenWrt staging provides
# these via cmake.mk's CPPFLAGS/LDFLAGS; on plain Linux resolve them.
find_path(UBUS_INCLUDE_DIR NAMES libubus.h)
if(UBUS_INCLUDE_DIR)
  include_directories(${UBUS_INCLUDE_DIR})
endif()
find_library(UBUS_LIBRARY NAMES ubus)
if(UBUS_LIBRARY)
  set(MIELOFON_PROBE_LIBS ${MIELOFON_PROBE_LIBS} ${UBUS_LIBRARY})
else()
  set(MIELOFON_PROBE_LIBS ${MIELOFON_PROBE_LIBS} ubus)
endif()

find_path(UBOX_INCLUDE_DIR NAMES libubox/blobmsg_json.h libubox/uloop.h)
if(UBOX_INCLUDE_DIR)
  include_directories(${UBOX_INCLUDE_DIR})
endif()
find_library(UBOX_LIBRARY NAMES ubox)
if(UBOX_LIBRARY)
  set(MIELOFON_PROBE_LIBS ${MIELOFON_PROBE_LIBS} ${UBOX_LIBRARY})
else()
  set(MIELOFON_PROBE_LIBS ${MIELOFON_PROBE_LIBS} ubox)
endif()

find_library(JSONC_LIBRARY NAMES json-c)
if(JSONC_LIBRARY)
  set(MIELOFON_PROBE_LIBS ${MIELOFON_PROBE_LIBS} ${JSONC_LIBRARY})
else()
  set(MIELOFON_PROBE_LIBS ${MIELOFON_PROBE_LIBS} json-c)
endif()

# Install unconditionally: on OpenWrt the feed Makefile reads the binary from
# $(PKG_INSTALL_DIR)/usr/bin/ (cmake.mk sets the prefix); plain-Linux uses the
# same /usr/bin target. OpenWrt installs the procd init/config/capabilities
# from mielofon-probe/files/ via its own Makefile.
install(TARGETS mielofon-probe RUNTIME DESTINATION bin)

add_executable(mielofon-probe src/main.cpp)
target_compile_options(mielofon-probe PRIVATE ${MIELOFON_PROBE_CXX_FLAGS})
target_compile_definitions(mielofon-probe PRIVATE
  MIELOFON_PROBE_VERSION="${MIELOFON_PROBE_VERSION}"
  MIELOFON_PROBE_OS="${MIELOFON_PROBE_OS}"
  MIELOFON_PROBE_ARCH="${MIELOFON_PROBE_ARCH}")
target_link_libraries(mielofon-probe PRIVATE ${MIELOFON_PROBE_LIBS})