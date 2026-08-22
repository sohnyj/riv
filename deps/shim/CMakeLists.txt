# Static builds of the EXR and HEIF shims, installed into the deps prefix.
cmake_minimum_required(VERSION 3.20)
project(riv_shims C CXX)

find_package(OpenEXR REQUIRED CONFIG)
find_package(libheif REQUIRED CONFIG)

add_library(riv_exr_shim STATIC exr_shim.cpp)
target_link_libraries(riv_exr_shim PRIVATE OpenEXR::OpenEXR)

add_library(riv_heif_shim STATIC heif_shim.c)
target_link_libraries(riv_heif_shim PRIVATE heif)

install(TARGETS riv_exr_shim riv_heif_shim ARCHIVE DESTINATION lib)
