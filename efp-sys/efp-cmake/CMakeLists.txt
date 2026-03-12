cmake_minimum_required(VERSION 3.10)
project(efp_for_rust C CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_C_STANDARD 99)

set(EFP_SOURCE_DIR "${CMAKE_CURRENT_SOURCE_DIR}/../../vendor/efp")

add_library(efp STATIC
    ${EFP_SOURCE_DIR}/ElasticFrameProtocol.cpp
)

target_include_directories(efp PUBLIC
    ${EFP_SOURCE_DIR}
    ${EFP_SOURCE_DIR}/efp_c_api
)

# GCC 15+ no longer implicitly includes <algorithm> via other headers.
# The vendored EFP source uses std::copy_n without including it directly.
target_compile_options(efp PRIVATE $<$<COMPILE_LANGUAGE:CXX>:-include algorithm>)

find_package(Threads REQUIRED)
target_link_libraries(efp Threads::Threads)

install(TARGETS efp ARCHIVE DESTINATION lib)
install(FILES
    ${EFP_SOURCE_DIR}/efp_c_api/elastic_frame_protocol_c_api.h
    DESTINATION include
)
