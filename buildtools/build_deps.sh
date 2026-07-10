#!/bin/sh
# C/C++ fallback 코덱 정적 빌드 (PORTING_PLAN §6.2-4) — clang-cl + xwin, /arch:AVX2.
# 레시피 원전: github.com/sohnyj/qView buildtools/packages (사용자 작성, 2026-07-10 승인).
# 산출물: buildtools/prefix/{lib,include} — riv의 build.rs가 링크한다.
set -e
cd "$(dirname "$0")"
ROOT=$PWD
PREFIX=$ROOT/prefix
# cmake 4는 3.5 미만 요구 프로젝트를 거부 — 하위 호환 하한 고정
export CMAKE_POLICY_VERSION_MINIMUM=3.5

clone() { # <디렉터리> <저장소> <브랜치>
    if [ ! -d "sources/$1" ]; then
        git clone --depth 1 --branch "$3" --filter=tree:0 "$2" "sources/$1"
    fi
}

configure_and_install() { # <디렉터리> [추가 cmake 인자...]
    directory=$1
    shift
    cmake -S "sources/$directory" -B "build/$directory" -G Ninja \
        -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_TOOLCHAIN_FILE="$ROOT/toolchain-clang-cl.cmake" \
        -DCMAKE_INSTALL_PREFIX="$PREFIX" \
        -DCMAKE_FIND_ROOT_PATH="$PREFIX" \
        -DCMAKE_PREFIX_PATH="$PREFIX" \
        -DCMAKE_POLICY_DEFAULT_CMP0091=NEW \
        -DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded \
        -DBUILD_SHARED_LIBS=OFF \
        "$@"
    ninja -C "build/$directory" install
}

mkdir -p sources build

# ── libwebp (+libwebpdemux — 기본 포함) — 애니 WebP 전담 (SPEC §10) ────────────
clone libwebp https://chromium.googlesource.com/webm/libwebp.git main
configure_and_install libwebp \
    -DWEBP_BUILD_ANIM_UTILS=OFF \
    -DWEBP_BUILD_CWEBP=OFF \
    -DWEBP_BUILD_DWEBP=OFF \
    -DWEBP_BUILD_EXTRAS=OFF \
    -DWEBP_BUILD_GIF2WEBP=OFF \
    -DWEBP_BUILD_IMG2WEBP=OFF \
    -DWEBP_BUILD_LIBWEBPMUX=OFF \
    -DWEBP_BUILD_VWEBP=OFF \
    -DWEBP_BUILD_WEBPINFO=OFF \
    -DWEBP_BUILD_WEBPMUX=OFF

# ── libde265 — HEVC 디코더 (HEIF fallback 하위) ────────────────────────────────
# ENABLE_DECODER는 dec265 CLI 빌드 옵션 — getopt가 없는 MSVC 타깃에서 실패하고
# 라이브러리에는 불필요해 qView 레시피(mingw)와 달리 OFF (2026-07-11)
clone libde265 https://github.com/strukturag/libde265.git master
configure_and_install libde265 \
    -DENABLE_SDL=OFF \
    -DENABLE_DECODER=OFF \
    -DENABLE_ENCODER=OFF

# ── libheif — WIC 부재 시 HEIF 런타임 fallback (PORTING_PLAN §5) ───────────────
# LIBDE265_STATIC_BUILD는 CMAKE_*_FLAGS 지정(툴체인 INIT 플래그를 덮어씀) 대신
# 환경 변수로 주입 — cmake가 INIT 플래그와 병합한다 (2026-07-11, clang-cl 경로 수정)
clone libheif https://github.com/strukturag/libheif.git master
(
    export CFLAGS="-DLIBDE265_STATIC_BUILD"
    export CXXFLAGS="-DLIBDE265_STATIC_BUILD"
    configure_and_install libheif \
        -DBUILD_TESTING=OFF \
        -DENABLE_PLUGIN_LOADING=OFF \
        -DWITH_AOM_DECODER=OFF \
        -DWITH_AOM_ENCODER=OFF \
        -DWITH_DAV1D=OFF \
        -DWITH_EXAMPLES=OFF \
        -DWITH_GDK_PIXBUF=OFF \
        -DWITH_LIBDE265=ON \
        -DWITH_X265=OFF
)

# ── Imath + libdeflate — OpenEXR 외부 의존 ─────────────────────────────────────
clone imath https://github.com/AcademySoftwareFoundation/Imath.git main
configure_and_install imath \
    -DBUILD_TESTING=OFF \
    -DIMATH_INSTALL_PKG_CONFIG=ON \
    -DPYTHON=OFF

clone libdeflate https://github.com/ebiggers/libdeflate.git master
configure_and_install libdeflate \
    -DLIBDEFLATE_BUILD_SHARED_LIB=OFF \
    -DLIBDEFLATE_BUILD_STATIC_LIB=ON \
    -DLIBDEFLATE_BUILD_GZIP=OFF \
    -DLIBDEFLATE_BUILD_TESTS=OFF

# ── OpenEXR — EXR 전담 (SPEC §10, 성능 우선 2026-07-10) ────────────────────────
clone openexr https://github.com/AcademySoftwareFoundation/openexr.git release
if ! git -C sources/openexr apply --reverse --check \
    "$ROOT/patches/openexr-0001-mingw-win32-semaphore.patch" 2>/dev/null; then
    git -C sources/openexr apply "$ROOT/patches/openexr-0001-mingw-win32-semaphore.patch"
fi
configure_and_install openexr \
    -DBUILD_TESTING=OFF \
    -DOPENEXR_BUILD_EXAMPLES=OFF \
    -DOPENEXR_BUILD_TOOLS=OFF \
    -DOPENEXR_INSTALL_PKG_CONFIG=ON \
    -DOPENEXR_INSTALL_TOOLS=OFF \
    -DOPENEXR_FORCE_INTERNAL_IMATH=OFF \
    -DOPENEXR_FORCE_INTERNAL_DEFLATE=OFF \
    -DOPENEXR_ENABLE_THREADING=ON

# ── EXR 심 — C++ RgbaInputFile을 extern "C"로 노출 ────────────────────────────
cmake -S shim -B build/shim -G Ninja \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_TOOLCHAIN_FILE="$ROOT/toolchain-clang-cl.cmake" \
    -DCMAKE_INSTALL_PREFIX="$PREFIX" \
    -DCMAKE_FIND_ROOT_PATH="$PREFIX" \
    -DCMAKE_PREFIX_PATH="$PREFIX" \
    -DCMAKE_POLICY_DEFAULT_CMP0091=NEW \
    -DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded
ninja -C build/shim install

echo "== fallback codecs installed to $PREFIX"
ls "$PREFIX/lib"
