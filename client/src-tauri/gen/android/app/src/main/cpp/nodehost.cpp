// JNI shim that starts the vendored libnode inside the app process and runs
// the browser driver's bootstrap under it. The argv-buffer and the
// logcat-redirection shape follow nodejs-mobile-react-native's
// native-lib.cpp, the reference embedding of this fork.
//
// The JDK's GetStringUTFChars gives modified UTF-8; node's argv wants plain
// bytes and the paths here are ASCII, so the copy is direct.

#include <jni.h>
#include <cstdlib>
#include <cstring>
#include <pthread.h>
#include <unistd.h>
#include <android/log.h>

#include "node.h"

#define LOG_TAG "FORGE-NODE"
#define LOGI(...) __android_log_print(ANDROID_LOG_INFO, LOG_TAG, __VA_ARGS__)
#define LOGE(...) __android_log_print(ANDROID_LOG_ERROR, LOG_TAG, __VA_ARGS__)

static int pipe_stdout[2];
static int pipe_stderr[2];
static pthread_t thread_stdout;
static pthread_t thread_stderr;

static void* thread_stderr_func(void*) {
    ssize_t n;
    char buf[2048];
    while ((n = read(pipe_stderr[0], buf, sizeof buf - 1)) > 0) {
        if (buf[n - 1] == '\n') --n;
        buf[n] = 0;
        __android_log_write(ANDROID_LOG_ERROR, "FORGE-NODE-E", buf);
    }
    return nullptr;
}

static void* thread_stdout_func(void*) {
    ssize_t n;
    char buf[2048];
    while ((n = read(pipe_stdout[0], buf, sizeof buf - 1)) > 0) {
        if (buf[n - 1] == '\n') --n;
        buf[n] = 0;
        __android_log_write(ANDROID_LOG_INFO, "FORGE-NODE-O", buf);
    }
    return nullptr;
}

static int start_redirecting_stdout_stderr() {
    setvbuf(stdout, nullptr, _IONBF, 0);
    pipe(pipe_stdout);
    dup2(pipe_stdout[1], STDOUT_FILENO);

    setvbuf(stderr, nullptr, _IONBF, 0);
    pipe(pipe_stderr);
    dup2(pipe_stderr[1], STDERR_FILENO);

    if (pthread_create(&thread_stdout, nullptr, thread_stdout_func, nullptr) == -1) return -1;
    pthread_detach(thread_stdout);
    if (pthread_create(&thread_stderr, nullptr, thread_stderr_func, nullptr) == -1) return -1;
    pthread_detach(thread_stderr);
    return 0;
}

extern "C" JNIEXPORT jint JNICALL
Java_dev_vedhavyas_forge_NodeHost_startNode(
        JNIEnv* env, jclass /* clazz */, jobjectArray arguments, jstring modulesPath) {
    const char* pathPath = env->GetStringUTFChars(modulesPath, nullptr);
    setenv("NODE_PATH", pathPath, 1);
    env->ReleaseStringUTFChars(modulesPath, pathPath);

    jsize argumentCount = env->GetArrayLength(arguments);
    int cArgumentsSize = 0;
    for (int i = 0; i < argumentCount; i++) {
        jstring js = (jstring) env->GetObjectArrayElement(arguments, i);
        cArgumentsSize += strlen(env->GetStringUTFChars(js, nullptr)) + 1;
    }

    char* argsBuffer = (char*) calloc(cArgumentsSize, sizeof(char));
    char** argv = (char**) calloc(argumentCount, sizeof(char*));
    char* pos = argsBuffer;
    for (int i = 0; i < argumentCount; i++) {
        jstring js = (jstring) env->GetObjectArrayElement(arguments, i);
        const char* arg = env->GetStringUTFChars(js, nullptr);
        strncpy(pos, arg, strlen(arg));
        argv[i] = pos;
        pos += strlen(pos) + 1;
    }

    if (start_redirecting_stdout_stderr() == -1)
        LOGE("could not start stdout/stderr redirection");

    int exit_code = node::Start(argumentCount, argv);
    LOGI("node::Start returned %d", exit_code);
    return exit_code;
}
