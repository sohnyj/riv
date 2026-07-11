# clang-cl + xwin cross toolchain for MSVC-target static builds.
# Override the xwin splat location with XWIN_ROOT (default ~/.xwin).

set(CMAKE_SYSTEM_NAME Windows)
set(CMAKE_SYSTEM_PROCESSOR AMD64)

if(DEFINED ENV{XWIN_ROOT})
    set(XWIN_ROOT "$ENV{XWIN_ROOT}")
else()
    set(XWIN_ROOT "$ENV{HOME}/.xwin")
endif()

set(CMAKE_C_COMPILER clang-cl)
set(CMAKE_CXX_COMPILER clang-cl)
set(CMAKE_C_COMPILER_TARGET x86_64-pc-windows-msvc)
set(CMAKE_CXX_COMPILER_TARGET x86_64-pc-windows-msvc)
set(CMAKE_AR llvm-lib)
set(CMAKE_LINKER lld-link)
set(CMAKE_RC_COMPILER llvm-rc)
set(CMAKE_MT llvm-mt)

# xwin lays out crt/ and sdk/ flat, so use explicit paths instead of /winsysroot.
# /arch:AVX2 matches the x86-64-v3 baseline.
set(XWIN_INCLUDE_FLAGS
    "-imsvc ${XWIN_ROOT}/crt/include -imsvc ${XWIN_ROOT}/sdk/include/ucrt -imsvc ${XWIN_ROOT}/sdk/include/um -imsvc ${XWIN_ROOT}/sdk/include/shared")
set(XWIN_LIBRARY_FLAGS
    "/libpath:${XWIN_ROOT}/crt/lib/x86_64 /libpath:${XWIN_ROOT}/sdk/lib/um/x86_64 /libpath:${XWIN_ROOT}/sdk/lib/ucrt/x86_64")
set(CMAKE_C_FLAGS_INIT "${XWIN_INCLUDE_FLAGS} /arch:AVX2")
set(CMAKE_CXX_FLAGS_INIT "${XWIN_INCLUDE_FLAGS} /arch:AVX2 /EHsc")
set(CMAKE_EXE_LINKER_FLAGS_INIT "${XWIN_LIBRARY_FLAGS}")
set(CMAKE_SHARED_LINKER_FLAGS_INIT "${XWIN_LIBRARY_FLAGS}")

# Keep try-compile linking executables: STATIC_LIBRARY lets check_library_exists
# pass falsely and injects phantom pthreads/m link dependencies.

set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_PACKAGE ONLY)
