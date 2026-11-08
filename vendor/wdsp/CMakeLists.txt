# CMakeLists.txt  --  builds WDSP with CMake, on Linux, macOS and Windows.
#
# This file is part of a program that implements a Software-Defined Radio.
#
# Copyright (C) 2026 Warren Pratt, NR0V
#
# This program is free software; you can redistribute it and/or
# modify it under the terms of the GNU General Public License
# as published by the Free Software Foundation; either version 2
# of the License, or (at your option) any later version.
#
# This program is distributed in the hope that it will be useful,
# but WITHOUT ANY WARRANTY; without even the implied warranty of
# MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
# GNU General Public License for more details.
#
# You should have received a copy of the GNU General Public License
# along with this program; if not, write to the Free Software
# Foundation, Inc., 51 Franklin Street, Fifth Floor, Boston, MA  02110-1301, USA.
#
# The author can be reached by email at
#
# warren@wpratt.com
#
# ------------------------------------------------------------------------------------------
#
# WHAT IT BUILDS
#
#   The WDSP library:  libwdsp.so (Linux), libwdsp.dylib (macOS), or wdsp.dll with its
#   import library wdsp.lib (Windows).  -DBUILD_SHARED_LIBS=OFF builds a static library
#   instead.
#
#   The library exports exactly the functions marked PORT in the sources, on every
#   platform, as the Windows DLL always has.  wdsp.h declares them;  it is the only
#   header that is installed.
#
#   [On Windows, wdsp.vcxproj remains the usual way to build WDSP as part of Thetis.
#   This file builds the same sources with the same main settings, for those who use
#   CMake there.]
#
# REQUIREMENTS
#
#   CMake 3.16 or later, a C compiler, and FFTW 3 (double precision, "libfftw3").
#
#       Debian, Ubuntu, Raspberry Pi OS:    sudo apt install cmake libfftw3-dev
#       Fedora:                             sudo dnf install cmake fftw-devel
#       macOS (Homebrew):                   brew install cmake fftw
#       Windows:                            the FFTW DLL and import library that Thetis
#                                           uses (lib\fftw_x64), found automatically when
#                                           this folder is in the Thetis source tree
#
# BUILDING  (from this folder)
#
#   Linux and macOS:
#       cmake -S . -B build
#       cmake --build build -j
#       sudo cmake --install build          (optional:  library, wdsp.h, wdsp.pc, CMake
#                                            package files;  --prefix to put them elsewhere)
#
#   Windows (in any command prompt, with Visual Studio installed):
#       cmake -S . -B build -A x64
#       cmake --build build --config Release
#
# OPTIONS  (-D<name>=<value> on the first cmake command)
#
#   CMAKE_BUILD_TYPE        Release (the default here), Debug, RelWithDebInfo.  [Ignored by
#                           Visual Studio, which takes --config at build time.]
#   BUILD_SHARED_LIBS       ON (default):  a shared library.  OFF:  a static library.
#   WDSP_NATIVE_ARCH        OFF (default).  ON:  optimize for the processor of the computer
#                           doing the build (-march=native).  The result may not run on an
#                           older processor.
#   WDSP_MSVC_AVX2          ON (default):  with Visual Studio for x64, /arch:AVX2, as in
#                           wdsp.vcxproj.
#   WDSP_BUILD_TESTS        OFF (default).  ON:  also build tests/wdsp_tests and register
#                           it with CTest (see tests/CMakeLists.txt).
#   FFTW3_ROOT              a folder to search first for FFTW (its lib, bin and the folder
#                           itself are searched).
#
# USING THE INSTALLED LIBRARY
#
#   CMake:          find_package (WDSP REQUIRED)
#                   target_link_libraries (myprogram PRIVATE WDSP::wdsp)
#   pkg-config:     cc myprogram.c $(pkg-config --cflags --libs wdsp)
#   Or add this folder to another CMake project with add_subdirectory (), and link
#   WDSP::wdsp the same way.
#
# ------------------------------------------------------------------------------------------

cmake_minimum_required (VERSION 3.16)

# The version comes from version.c, so there is only one place to change it.
# [GetWDSPVersion () returns, for example, 211 for version 2.11.]
file (STRINGS "${CMAKE_CURRENT_SOURCE_DIR}/version.c" _wdsp_version_line REGEX "const int version = [0-9]+;")
string (REGEX MATCH "[0-9]+" _wdsp_version_number "${_wdsp_version_line}")
if (NOT _wdsp_version_number)
	message (FATAL_ERROR "WDSP:  can't read the version number from version.c")
endif ()
math (EXPR _wdsp_version_major "${_wdsp_version_number} / 100")
math (EXPR _wdsp_version_minor "${_wdsp_version_number} % 100")

project (WDSP
	VERSION ${_wdsp_version_major}.${_wdsp_version_minor}
	DESCRIPTION "WDSP:  digital signal processing for software-defined radio"
	LANGUAGES C)

include (GNUInstallDirs)
include (CheckCCompilerFlag)
include (CMakePackageConfigHelpers)

option (BUILD_SHARED_LIBS	"Build WDSP as a shared library (OFF:  static)"							ON)
option (WDSP_NATIVE_ARCH	"Optimize for the processor of the computer doing the build"				OFF)
option (WDSP_MSVC_AVX2		"Visual Studio, x64:  compile with /arch:AVX2, as wdsp.vcxproj does"		ON)
option (WDSP_BUILD_TESTS	"Also build the regression tests (tests/)"								OFF)

# Release unless asked otherwise.  [Only for single-configuration generators (Makefiles,
# Ninja);  Visual Studio and Xcode choose the configuration at build time.]
get_property (_wdsp_multi_config GLOBAL PROPERTY GENERATOR_IS_MULTI_CONFIG)
if (NOT _wdsp_multi_config AND NOT CMAKE_BUILD_TYPE)
	set (CMAKE_BUILD_TYPE Release CACHE STRING "Build type" FORCE)
endif ()

# ------------------------------------------------------------------------------------------
# Sources
# ------------------------------------------------------------------------------------------

# The same files as wdsp.vcxproj.  [A file added to WDSP must be added here too.  The check
# below warns about any .c file in this folder that isn't listed.]
set (WDSP_SOURCES
	RXA.c TXA.c amd.c ammod.c amsq.c analyzer.c anf.c anr.c apfshadow.c bandpass.c calcc.c
	calculus.c cblock.c cfcomp.c cfir.c channel.c cmath.c compress.c delay.c dexp.c div.c
	doublepole.c eer.c emnr.c emph.c eq.c extrapolate.c fcurve.c fir.c firmin.c fmd.c fmmod.c
	fmsq.c gain.c gaussian.c gen.c icfir.c iir.c impulse_cache.c iobuffs.c iqc.c lmath.c main.c
	matchedCW.c meter.c meterlog10.c nbp.c nnet.c nnio.c nnr.c nnr_model_0.c nnr_model_1.c
	nnr_model_2.c nnr_model_3.c nnrbypass.c nob.c nobII.c nurbs.c nurbs_fit.c nurbs_spline.c
	osctrl.c patchpanel.c phrot.c resample.c reshb.c rmatch.c sender.c shift.c siphon.c slew.c
	snb.c snoop.c ssql.c syncbuffs.c utilities.c varsamp.c version.c wbfm.c wcpAGC.c wisdom.c
	zetaHat.c)

file (GLOB _wdsp_found RELATIVE "${CMAKE_CURRENT_SOURCE_DIR}" "${CMAKE_CURRENT_SOURCE_DIR}/*.c")
foreach (_f IN LISTS _wdsp_found)
	if (NOT _f IN_LIST WDSP_SOURCES AND NOT _f STREQUAL "wdsp_platform_posix.c")
		message (WARNING "WDSP:  ${_f} is in the source folder but not in WDSP_SOURCES;  it isn't built")
	endif ()
endforeach ()

# The platform layer.  [wdsp_platform_posix.c on Linux and macOS.  On Windows, the version
# resource, as in wdsp.vcxproj;  wdsp.rc is UTF-16, which only Microsoft's resource
# compiler reads.]
if (WIN32)
	if (MSVC AND EXISTS "${CMAKE_CURRENT_SOURCE_DIR}/wdsp.rc")
		list (APPEND WDSP_SOURCES wdsp.rc)
	endif ()
else ()
	list (APPEND WDSP_SOURCES wdsp_platform_posix.c)
endif ()

# ------------------------------------------------------------------------------------------
# FFTW
# ------------------------------------------------------------------------------------------

# WDSP compiles against its own copy of fftw3.h, so only the library is needed.  It is
# looked for in FFTW3_ROOT, then where pkg-config says it is, then in the usual places
# (on macOS, Homebrew's and MacPorts' too), and on Windows in the Thetis tree
# (..\..\lib\fftw_x64 or fftw_x86).
# [FFTW's own CMake package files are not used:  some distributions ship them broken.]
set (FFTW3_ROOT "" CACHE PATH "A folder to search first for the FFTW library")

set (_fftw_hints "")
if (FFTW3_ROOT)
	list (APPEND _fftw_hints "${FFTW3_ROOT}" "${FFTW3_ROOT}/lib" "${FFTW3_ROOT}/bin")
endif ()
find_package (PkgConfig QUIET)
if (PKG_CONFIG_FOUND)
	pkg_check_modules (_FFTW3_PC QUIET fftw3)
	if (_FFTW3_PC_FOUND)
		list (APPEND _fftw_hints ${_FFTW3_PC_LIBRARY_DIRS})
	endif ()
endif ()
if (APPLE)
	list (APPEND _fftw_hints /opt/homebrew/lib /usr/local/lib /opt/local/lib)		# [Homebrew, Homebrew (Intel), MacPorts]
endif ()
if (WIN32)
	if (CMAKE_SIZEOF_VOID_P EQUAL 8)
		list (APPEND _fftw_hints "${CMAKE_CURRENT_SOURCE_DIR}/../../lib/fftw_x64")
	else ()
		list (APPEND _fftw_hints "${CMAKE_CURRENT_SOURCE_DIR}/../../lib/fftw_x86")
	endif ()
endif ()

find_library (FFTW3_LIBRARY NAMES fftw3 libfftw3-3 fftw3-3 HINTS ${_fftw_hints}
	DOC "The FFTW library (double precision)")
if (NOT FFTW3_LIBRARY)
	message (FATAL_ERROR "WDSP:  FFTW 3 (libfftw3) was not found.  Install it (see the top of "
		"this file), or give its folder with -DFFTW3_ROOT=<folder>.")
endif ()
message (STATUS "WDSP:  FFTW library:  ${FFTW3_LIBRARY}")

# [On Windows, the FFTW DLL, so that the tests can find it at run time.]
if (WIN32)
	get_filename_component (_fftw_dir "${FFTW3_LIBRARY}" DIRECTORY)
	find_file (FFTW3_DLL NAMES libfftw3-3.dll fftw3.dll HINTS "${_fftw_dir}" ${_fftw_hints}
		DOC "The FFTW DLL (Windows)")
endif ()

find_package (Threads REQUIRED)

# ------------------------------------------------------------------------------------------
# The library
# ------------------------------------------------------------------------------------------

add_library (wdsp ${WDSP_SOURCES})
add_library (WDSP::wdsp ALIAS wdsp)

# EXPORTS.  Only the functions marked PORT:  everything else is hidden, as in the Windows
# DLL.  [On Linux and macOS, -fvisibility=hidden, and PORT makes a function visible again
# (see wdsp_platform.h).  On Windows, PORT is __declspec (dllexport).]
set_target_properties (wdsp PROPERTIES
	C_VISIBILITY_PRESET hidden
	POSITION_INDEPENDENT_CODE ON)

# [No VERSION or SOVERSION:  the shared library is plain libwdsp.so (libwdsp.dylib), as in
# the existing Linux builds, so a program finds whichever WDSP is installed.]

# Windows:  wdsp.dll and wdsp.lib, the names Thetis uses, with any compiler.  [MinGW would
# otherwise make libwdsp.dll.  A static MinGW library keeps its usual name, libwdsp.a.]
if (WIN32 AND BUILD_SHARED_LIBS)
	set_target_properties (wdsp PROPERTIES PREFIX "" IMPORT_PREFIX "")
endif ()

# The interface:  wdsp.h only.  [It is copied into the build folder, so that a project that
# uses this folder through add_subdirectory () sees wdsp.h and none of WDSP's internal
# headers, whose names (filter.h, meter.h, delay.h, ...) could hide its own.]
configure_file (wdsp.h "${CMAKE_CURRENT_BINARY_DIR}/include/wdsp.h" COPYONLY)
target_include_directories (wdsp
	PUBLIC
		"$<BUILD_INTERFACE:${CMAKE_CURRENT_BINARY_DIR}/include>"
		"$<INSTALL_INTERFACE:${CMAKE_INSTALL_INCLUDEDIR}>"
	PRIVATE
		"${CMAKE_CURRENT_SOURCE_DIR}")

# wdsp.h declares the functions __declspec (dllimport) on Windows.  A program that links a
# static WDSP needs them declared plainly.
if (WIN32 AND NOT BUILD_SHARED_LIBS)
	target_compile_definitions (wdsp INTERFACE "WDSP_API=")
endif ()

# C dialect and compiler options.
if (MSVC)
	# As wdsp.vcxproj (Release):  /O2 and /fp:precise come from the configuration;  the
	# rest is here.  [No /std:c11:  the project compiles with MSVC's default C.]
	target_compile_options (wdsp PRIVATE /fp:precise "$<$<CONFIG:Release>:/Oi;/Ot>")
	if (WDSP_MSVC_AVX2 AND CMAKE_SIZEOF_VOID_P EQUAL 8)
		target_compile_options (wdsp PRIVATE /arch:AVX2)
	endif ()
	# [Whole-program optimization, as wdsp.vcxproj (/GL and /LTCG).]
	set_property (TARGET wdsp PROPERTY INTERPROCEDURAL_OPTIMIZATION_RELEASE ON)
else ()
	set_target_properties (wdsp PROPERTIES C_STANDARD 11 C_EXTENSIONS ON)		# -std=gnu11

	# -fno-strict-aliasing:  WDSP is written for MSVC, which never assumes that pointers of
	# different types can't refer to the same memory.  GCC and Clang do at -O2 unless told
	# otherwise, and some code relies on the MSVC behaviour (varsamp.c reads a double
	# through a uint64_t pointer, for example).
	target_compile_options (wdsp PRIVATE -fno-strict-aliasing)

	if (WDSP_NATIVE_ARCH)
		check_c_compiler_flag (-march=native _wdsp_has_march_native)
		check_c_compiler_flag (-mcpu=native _wdsp_has_mcpu_native)
		if (_wdsp_has_march_native)
			target_compile_options (wdsp PRIVATE -march=native)
		elseif (_wdsp_has_mcpu_native)
			target_compile_options (wdsp PRIVATE -mcpu=native)		# [Apple Silicon, some ARM compilers]
		else ()
			message (WARNING "WDSP:  WDSP_NATIVE_ARCH:  the compiler accepts neither -march=native nor -mcpu=native")
		endif ()
	endif ()
endif ()

# Libraries.  [FFTW is private:  a program that uses the shared library doesn't link it
# itself.  For a static WDSP, CMake and pkg-config pass FFTW on to the program.]
target_link_libraries (wdsp PRIVATE "${FFTW3_LIBRARY}" Threads::Threads)
if (WIN32)
	target_link_libraries (wdsp PRIVATE avrt)		# AvSetMmThreadCharacteristics (), as wdsp.vcxproj
else ()
	target_link_libraries (wdsp PRIVATE m)
endif ()

# ------------------------------------------------------------------------------------------
# Install:  the library, wdsp.h, a pkg-config file and CMake package files
# ------------------------------------------------------------------------------------------

install (TARGETS wdsp EXPORT WDSPTargets
	RUNTIME DESTINATION "${CMAKE_INSTALL_BINDIR}"
	LIBRARY DESTINATION "${CMAKE_INSTALL_LIBDIR}"
	ARCHIVE DESTINATION "${CMAKE_INSTALL_LIBDIR}")
install (FILES wdsp.h DESTINATION "${CMAKE_INSTALL_INCLUDEDIR}")

# pkg-config.  [For a static library, Libs.private lists what a program must also link.]
set (_wdsp_pc_libs_private "")
if (NOT BUILD_SHARED_LIBS)
	get_filename_component (_fftw_libdir "${FFTW3_LIBRARY}" DIRECTORY)
	set (_wdsp_pc_libs_private "-L${_fftw_libdir} -lfftw3 -lm -pthread")
endif ()
# [The prefix is given relative to the file's own folder (${pcfiledir}), so the file is
# right wherever the files are installed, including with cmake --install --prefix.]
if (IS_ABSOLUTE "${CMAKE_INSTALL_LIBDIR}")
	set (_wdsp_pc_prefix "${CMAKE_INSTALL_PREFIX}")
else ()
	file (RELATIVE_PATH _wdsp_pc_up "/_/${CMAKE_INSTALL_LIBDIR}/pkgconfig" "/_")
	string (REGEX REPLACE "/$" "" _wdsp_pc_up "${_wdsp_pc_up}")
	set (_wdsp_pc_prefix "\${pcfiledir}/${_wdsp_pc_up}")
endif ()
if (IS_ABSOLUTE "${CMAKE_INSTALL_LIBDIR}")
	set (_wdsp_pc_libdir "${CMAKE_INSTALL_LIBDIR}")
else ()
	set (_wdsp_pc_libdir "\${prefix}/${CMAKE_INSTALL_LIBDIR}")
endif ()
if (IS_ABSOLUTE "${CMAKE_INSTALL_INCLUDEDIR}")
	set (_wdsp_pc_includedir "${CMAKE_INSTALL_INCLUDEDIR}")
else ()
	set (_wdsp_pc_includedir "\${prefix}/${CMAKE_INSTALL_INCLUDEDIR}")
endif ()
file (WRITE "${CMAKE_CURRENT_BINARY_DIR}/wdsp.pc.in"
"prefix=@_wdsp_pc_prefix@
libdir=@_wdsp_pc_libdir@
includedir=@_wdsp_pc_includedir@

Name: WDSP
Description: @PROJECT_DESCRIPTION@
Version: @PROJECT_VERSION@
Libs: -L\${libdir} -lwdsp
Libs.private: @_wdsp_pc_libs_private@
Cflags: -I\${includedir}
")
configure_file ("${CMAKE_CURRENT_BINARY_DIR}/wdsp.pc.in" "${CMAKE_CURRENT_BINARY_DIR}/wdsp.pc" @ONLY)
install (FILES "${CMAKE_CURRENT_BINARY_DIR}/wdsp.pc" DESTINATION "${CMAKE_INSTALL_LIBDIR}/pkgconfig")

# CMake:  find_package (WDSP) gives the target WDSP::wdsp.
install (EXPORT WDSPTargets NAMESPACE WDSP:: DESTINATION "${CMAKE_INSTALL_LIBDIR}/cmake/WDSP")
file (WRITE "${CMAKE_CURRENT_BINARY_DIR}/WDSPConfig.cmake.in"
"@PACKAGE_INIT@
include (CMakeFindDependencyMacro)
find_dependency (Threads)
include (\"\${CMAKE_CURRENT_LIST_DIR}/WDSPTargets.cmake\")
check_required_components (WDSP)
")
configure_package_config_file ("${CMAKE_CURRENT_BINARY_DIR}/WDSPConfig.cmake.in"
	"${CMAKE_CURRENT_BINARY_DIR}/WDSPConfig.cmake"
	INSTALL_DESTINATION "${CMAKE_INSTALL_LIBDIR}/cmake/WDSP")
# [find_package (WDSP 2.11) accepts any WDSP 2.x from 2.11 on.]
write_basic_package_version_file ("${CMAKE_CURRENT_BINARY_DIR}/WDSPConfigVersion.cmake"
	COMPATIBILITY SameMajorVersion)
install (FILES "${CMAKE_CURRENT_BINARY_DIR}/WDSPConfig.cmake" "${CMAKE_CURRENT_BINARY_DIR}/WDSPConfigVersion.cmake"
	DESTINATION "${CMAKE_INSTALL_LIBDIR}/cmake/WDSP")

# ------------------------------------------------------------------------------------------
# Tests
# ------------------------------------------------------------------------------------------

if (WDSP_BUILD_TESTS)
	enable_testing ()
	add_subdirectory (tests)
endif ()

if (BUILD_SHARED_LIBS)
	message (STATUS "WDSP ${PROJECT_VERSION}:  ${CMAKE_SYSTEM_NAME}, shared library")
else ()
	message (STATUS "WDSP ${PROJECT_VERSION}:  ${CMAKE_SYSTEM_NAME}, static library")
endif ()
