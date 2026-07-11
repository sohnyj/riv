#!/bin/sh
# Static builds of the C/C++ fallback codecs with clang-cl + xwin (/arch:AVX2).
# Output: deps/prefix/{lib,include}, linked by build.rs.
set -e
cd "$(dirname "$0")"
ROOT=$PWD
PREFIX=$ROOT/prefix
# cmake 4 rejects projects requiring <3.5; pin the compatibility floor.
export CMAKE_POLICY_VERSION_MINIMUM=3.5

clone() { # <directory> <repository> <branch>
    if [ ! -d "sources/$1" ]; then
        git clone --depth 1 --branch "$3" --filter=tree:0 "$2" "sources/$1"
    fi
}

configure_and_install() { # <directory> [extra cmake args...]
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

# libwebp (+libwebpdemux) for animated WebP
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

# libde265 (HEVC for the HEIF fallback). ENABLE_DECODER only builds the dec265
# CLI, which fails on MSVC targets without getopt; the library does not need it.
clone libde265 https://github.com/strukturag/libde265.git master
configure_and_install libde265 \
    -DENABLE_SDL=OFF \
    -DENABLE_DECODER=OFF \
    -DENABLE_ENCODER=OFF

# libheif (HEIF runtime fallback). LIBDE265_STATIC_BUILD goes through the
# environment so cmake merges it with the toolchain INIT flags instead of
# overwriting them.
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

# Imath + libdeflate (OpenEXR dependencies)
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

# OpenEXR. clang-cl (MSVC target) already takes the Win32 semaphore branch,
# so no patching is needed.
clone openexr https://github.com/AcademySoftwareFoundation/openexr.git release
configure_and_install openexr \
    -DBUILD_TESTING=OFF \
    -DOPENEXR_BUILD_EXAMPLES=OFF \
    -DOPENEXR_BUILD_TOOLS=OFF \
    -DOPENEXR_INSTALL_PKG_CONFIG=ON \
    -DOPENEXR_INSTALL_TOOLS=OFF \
    -DOPENEXR_FORCE_INTERNAL_IMATH=OFF \
    -DOPENEXR_FORCE_INTERNAL_DEFLATE=OFF \
    -DOPENEXR_ENABLE_THREADING=ON

# EXR shim: expose the C++ RgbaInputFile through extern "C"
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
