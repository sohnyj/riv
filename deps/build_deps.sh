#!/bin/sh
# Static builds of the C/C++ fallback codecs with clang-cl + xwin (/arch:AVX2, thin LTO).
# Output: deps/prefix/{lib,include}, linked by build.rs.
# With no option, sync the sources and build.
# Options:
#   --sync
#   --build
#   --revisions-hash   Short digest of the checked-out commits, which keys the codec cache.
set -e
cd "$(dirname "$0")"
ROOT=$PWD
PREFIX=$ROOT/prefix

# cmake 4 rejects projects requiring <3.5; pin the compatibility floor.
export CMAKE_POLICY_VERSION_MINIMUM=3.5

# <directory> <repository> <branch>; moving branches by decision, so a sync follows them.
SOURCES="\
libwebp https://chromium.googlesource.com/webm/libwebp.git main
libde265 https://github.com/strukturag/libde265.git master
libheif https://github.com/strukturag/libheif.git master
dav1d https://github.com/videolan/dav1d.git master
imath https://github.com/AcademySoftwareFoundation/Imath.git main
libdeflate https://github.com/ebiggers/libdeflate.git master
openexr https://github.com/AcademySoftwareFoundation/openexr.git release"

sync_sources() {
    mkdir -p sources
    echo "$SOURCES" | while read -r directory repository branch; do
        if [ -d "sources/$directory/.git" ]; then
            git -C "sources/$directory" fetch --depth 1 origin "$branch"
            git -C "sources/$directory" reset --hard FETCH_HEAD
        else
            git clone --depth 1 --branch "$branch" --filter=tree:0 \
                "$repository" "sources/$directory"
        fi
    done
}

print_revisions_hash() {
    revisions=$(echo "$SOURCES" | while read -r directory _; do
        revision=$(git -C "sources/$directory" rev-parse HEAD)
        echo "$directory $revision"
    done)
    echo "$revisions" | sha256sum | cut -c1-16
}

case "${1:-}" in
    --sync) sync_sources; exit 0 ;;
    --build) ;;
    --revisions-hash) print_revisions_hash; exit 0 ;;
    "")
        previous=$(print_revisions_hash 2>/dev/null) || previous=""
        sync_sources
        if [ "$(print_revisions_hash)" != "$previous" ]; then
            rm -rf build prefix
        fi
        ;;
    *)
        echo "usage: $0 [--sync | --build | --revisions-hash]" >&2
        exit 2
        ;;
esac

configure_and_install() { # <directory> [extra cmake args...]
    directory=$1
    shift
    cmake -S "sources/$directory" -B "build/$directory" -G Ninja \
        -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_C_FLAGS_RELEASE="/clang:-O3 /clang:-flto=thin /DNDEBUG" \
        -DCMAKE_CXX_FLAGS_RELEASE="/clang:-O3 /clang:-flto=thin /DNDEBUG" \
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

mkdir -p build

# libwebp (+libwebpdemux) for animated WebP
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

# libde265 (HEVC for the HEIF fallback): its -Wall means -Weverything to clang-cl
(
    export CFLAGS="-Wno-everything"
    export CXXFLAGS="-Wno-everything"
    configure_and_install libde265 \
        -DENABLE_SDL=OFF \
        -DENABLE_DECODER=OFF \
        -DENABLE_ENCODER=OFF \
        -DENABLE_SIMD=ON \
        -DENABLE_AVX2=ON \
        -DENABLE_AVX512=ON
)

# dav1d (AV1 for animated AVIF)
XWIN_ROOT=${XWIN_ROOT:-$HOME/.xwin}
sed "s|@XWIN_ROOT@|$XWIN_ROOT|g" cross-clang-cl.ini > build/dav1d-cross.ini
DAV1D_SETUP="--cross-file build/dav1d-cross.ini \
    --buildtype release \
    --default-library static \
    --prefix $PREFIX \
    --libdir lib \
    -Db_vscrt=mt \
    -Db_lto=true \
    -Db_lto_mode=thin \
    -Denable_tools=false \
    -Denable_tests=false \
    -Dxxhash_muxer=disabled"
# Meson applies neither new options nor a changed cross file to an existing setup.
{ echo "$DAV1D_SETUP"; cat build/dav1d-cross.ini; } > build/dav1d-setup.txt
if ! cmp -s build/dav1d-setup.txt build/dav1d/setup.txt; then
    rm -rf build/dav1d
    meson setup build/dav1d sources/dav1d $DAV1D_SETUP
    cp build/dav1d-setup.txt build/dav1d/setup.txt
fi
ninja -C build/dav1d install

# libheif (HEIF runtime fallback + AVIF sequences on the dav1d above)
(
    export CFLAGS="-DLIBDE265_STATIC_BUILD"
    export CXXFLAGS="-DLIBDE265_STATIC_BUILD"
    # find_library looks for *.lib only, so the meson archive is pinned directly.
    configure_and_install libheif \
        -DBUILD_TESTING=OFF \
        -DDAV1D_INCLUDE_DIR="$PREFIX/include" \
        -DDAV1D_LIBRARY="$PREFIX/lib/libdav1d.a" \
        -DENABLE_PLUGIN_LOADING=OFF \
        -DWITH_AOM_DECODER=OFF \
        -DWITH_AOM_ENCODER=OFF \
        -DWITH_DAV1D=ON \
        -DWITH_EXAMPLES=OFF \
        -DWITH_GDK_PIXBUF=OFF \
        -DWITH_LIBDE265=ON \
        -DWITH_X265=OFF
)

# Imath + libdeflate (OpenEXR dependencies)
configure_and_install imath \
    -DBUILD_TESTING=OFF \
    -DIMATH_INSTALL_PKG_CONFIG=ON \
    -DPYTHON=OFF

configure_and_install libdeflate \
    -DLIBDEFLATE_BUILD_SHARED_LIB=OFF \
    -DLIBDEFLATE_BUILD_STATIC_LIB=ON \
    -DLIBDEFLATE_BUILD_GZIP=OFF \
    -DLIBDEFLATE_BUILD_TESTS=OFF

# OpenEXR: its CMake adds a /MP that clang-cl ignores and reports once per file
(
    export CFLAGS="-Wno-unused-command-line-argument"
    export CXXFLAGS="-Wno-unused-command-line-argument"
    configure_and_install openexr \
        -DBUILD_TESTING=OFF \
        -DOPENEXR_BUILD_EXAMPLES=OFF \
        -DOPENEXR_BUILD_TOOLS=OFF \
        -DOPENEXR_INSTALL_PKG_CONFIG=ON \
        -DOPENEXR_INSTALL_TOOLS=OFF \
        -DOPENEXR_FORCE_INTERNAL_IMATH=OFF \
        -DOPENEXR_FORCE_INTERNAL_DEFLATE=OFF \
        -DOPENEXR_ENABLE_THREADING=ON
)

# EXR and HEIF shims
cmake -S shim -B build/shim -G Ninja \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_C_FLAGS_RELEASE="/clang:-O3 /clang:-flto=thin /DNDEBUG" \
    -DCMAKE_CXX_FLAGS_RELEASE="/clang:-O3 /clang:-flto=thin /DNDEBUG" \
    -DCMAKE_TOOLCHAIN_FILE="$ROOT/toolchain-clang-cl.cmake" \
    -DCMAKE_INSTALL_PREFIX="$PREFIX" \
    -DCMAKE_FIND_ROOT_PATH="$PREFIX" \
    -DCMAKE_PREFIX_PATH="$PREFIX" \
    -DCMAKE_POLICY_DEFAULT_CMP0091=NEW \
    -DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded
ninja -C build/shim install

echo "fallback codecs installed to $PREFIX"
ls "$PREFIX/lib"
