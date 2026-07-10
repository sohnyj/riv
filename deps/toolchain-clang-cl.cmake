# clang-cl + xwin 크로스 툴체인 (PORTING_PLAN P4·§6.2) — WSL에서 msvc 타깃 정적 빌드.
# xwin 스플랫 위치는 XWIN_ROOT 환경 변수로 재지정 가능 (기본 ~/.xwin).

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

# xwin 스플랫은 VS 배치가 아니라 crt/·sdk/ 평탄 배치 — /winsysroot 대신 명시 경로.
# /arch:AVX2 = x86-64-v3 상당 (P6과 정합 — 성능 기준 선택의 전제)
set(XWIN_INCLUDE_FLAGS
    "-imsvc ${XWIN_ROOT}/crt/include -imsvc ${XWIN_ROOT}/sdk/include/ucrt -imsvc ${XWIN_ROOT}/sdk/include/um -imsvc ${XWIN_ROOT}/sdk/include/shared")
set(XWIN_LIBRARY_FLAGS
    "/libpath:${XWIN_ROOT}/crt/lib/x86_64 /libpath:${XWIN_ROOT}/sdk/lib/um/x86_64 /libpath:${XWIN_ROOT}/sdk/lib/ucrt/x86_64")
set(CMAKE_C_FLAGS_INIT "${XWIN_INCLUDE_FLAGS} /arch:AVX2")
set(CMAKE_CXX_FLAGS_INIT "${XWIN_INCLUDE_FLAGS} /arch:AVX2 /EHsc")
set(CMAKE_EXE_LINKER_FLAGS_INIT "${XWIN_LIBRARY_FLAGS}")
set(CMAKE_SHARED_LINKER_FLAGS_INIT "${XWIN_LIBRARY_FLAGS}")

# try-compile은 exe 링크까지 수행(기본값) — STATIC_LIBRARY로 두면 라이브러리 존재
# 검사(check_library_exists)가 전부 거짓 통과해 pthreads·m 같은 가짜 링크 의존이
# 주입된다 (OpenEXR website_src.exe 링크 실패 원인, 2026-07-11 확인)

set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_PACKAGE ONLY)
