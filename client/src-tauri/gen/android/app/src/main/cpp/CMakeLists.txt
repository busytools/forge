# The in-app Node host: a JNI shim over the vendored libnode.so, so the
# browser driver runs inside the app process (Android has no second node
# binary to spawn, and the WebView is the engine).
#
# libnode.so itself is vendored (gitignored) by `just
# vendor-browser-stack-android` into jniLibs/, and its headers into
# cpp/nodejs-mobile/include/. Build the shim here; AGP packages both
# jniLibs .so files into the APK.
cmake_minimum_required(VERSION 3.22)

# The vendored libnode.so links libc++_shared; select the shared STL the same
# way, or the shim links a second, static libc++ and the runtime .so never
# reaches jniLibs (AGP packages libc++_shared.so only for a shared STL).
set(ANDROID_STL c++_shared)

project(nodehost CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

add_library(nodehost SHARED nodehost.cpp)

target_include_directories(nodehost PRIVATE nodejs-mobile/include/node)

target_link_libraries(nodehost
    "${CMAKE_SOURCE_DIR}/../jniLibs/${ANDROID_ABI}/libnode.so"
    log)
