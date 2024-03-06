/*
 * Copyright 2021 The Android Open Source Project
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

#define LOG_TAG "VirtualMachine"

#include <aidl/android/system/virtualizationservice/IVirtualMachine.h>
#include <android/binder_auto_utils.h>
#include <android/binder_ibinder_jni.h>
#include <jni.h>
#include <log/log.h>

#include <binder_rpc_unstable.hpp>
#include <tuple>

#include "common.h"

extern "C" JNIEXPORT jobject JNICALL
Java_android_system_virtualmachine_VirtualMachine_nativeConnectToVsockServer(
        JNIEnv* env, [[maybe_unused]] jclass clazz, jobject vmBinder, jint port) {
    using aidl::android::system::virtualizationservice::IVirtualMachine;
    using ndk::ScopedFileDescriptor;
    using ndk::SpAIBinder;

    auto vm = IVirtualMachine::fromBinder(SpAIBinder{AIBinder_fromJavaBinder(env, vmBinder)});

    std::tuple args{env, vm.get(), port};
    using Args = decltype(args);

    auto requestFunc = [](void* param) {
        auto [env, vm, port] = *static_cast<Args*>(param);

        ScopedFileDescriptor fd;
        if (auto status = vm->connectVsock(port, &fd); !status.isOk()) {
            env->ThrowNew(env->FindClass("android/system/virtualmachine/VirtualMachineException"),
                          ("Failed to connect vsock: " + status.getDescription()).c_str());
            return -1;
        }

        // take ownership
        int ret = fd.get();
        *fd.getR() = -1;

        return ret;
    };

    RpcSessionHandle session;
    // We need a thread pool to be able to support linkToDeath, or callbacks
    // (b/268335700). These threads are currently created eagerly, so we don't
    // want too many. The number 1 is chosen after some discussion, and to match
    // the server-side default (mMaxThreads on RpcServer).
    ARpcSession_setMaxIncomingThreads(session.get(), 1);
    auto client = ARpcSession_setupPreconnectedClient(session.get(), requestFunc, &args);
    return AIBinder_toJavaBinder(env, client);
}
