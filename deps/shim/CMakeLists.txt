cmake_minimum_required(VERSION 3.20)
project(riv_exr_shim CXX)

find_package(OpenEXR REQUIRED CONFIG)

add_library(riv_exr_shim STATIC exr_shim.cpp)
target_link_libraries(riv_exr_shim PRIVATE OpenEXR::OpenEXR)

install(TARGETS riv_exr_shim ARCHIVE DESTINATION lib)
