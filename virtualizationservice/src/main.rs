// Copyright 2021, The Android Open Source Project
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Android VirtualizationService

mod aidl;
mod atom;
mod remote_provisioning;
mod rkpvm;

use crate::aidl::{
    remove_temporary_dir, BINDER_SERVICE_IDENTIFIER, TEMPORARY_DIRECTORY,
    VirtualizationServiceInternal
};
use android_logger::{Config, FilterBuilder};
use android_system_virtualizationservice_internal::aidl::android::system::virtualizationservice_internal::IVirtualizationServiceInternal::BnVirtualizationServiceInternal;
use anyhow::Error;
use binder::{register_lazy_service, BinderFeatures, ProcessState, ThreadState};
use log::{info, Level};
use std::fs::{create_dir, read_dir};
use std::os::unix::raw::{pid_t, uid_t};
use std::path::Path;

const LOG_TAG: &str = "VirtualizationService";
pub(crate) const REMOTELY_PROVISIONED_COMPONENT_SERVICE_NAME: &str =
    "android.hardware.security.keymint.IRemotelyProvisionedComponent/avf";

fn get_calling_pid() -> pid_t {
    ThreadState::get_calling_pid()
}

fn get_calling_uid() -> uid_t {
    ThreadState::get_calling_uid()
}

fn main() {
    android_logger::init_once(
        Config::default()
            .with_tag(LOG_TAG)
            .with_min_level(Level::Info)
            .with_log_id(android_logger::LogId::System)
            .with_filter(
                // Reduce logspam by silencing logs from the disk crate which don't provide much
                // information to us.
                FilterBuilder::new().parse("info,disk=off").build(),
            ),
    );

    clear_temporary_files().expect("Failed to delete old temporary files");

    let common_dir_path = Path::new(TEMPORARY_DIRECTORY).join("common");
    create_dir(common_dir_path).expect("Failed to create common directory");

    ProcessState::start_thread_pool();

    let service = VirtualizationServiceInternal::init();
    let service = BnVirtualizationServiceInternal::new_binder(service, BinderFeatures::default());
    register_lazy_service(BINDER_SERVICE_IDENTIFIER, service.as_binder()).unwrap();
    info!("Registered Binder service {}.", BINDER_SERVICE_IDENTIFIER);

    if cfg!(remote_attestation) {
        // The IRemotelyProvisionedComponent service is only supposed to be triggered by rkpd for
        // RKP VM attestation.
        let remote_provisioning_service = remote_provisioning::new_binder();
        register_lazy_service(
            REMOTELY_PROVISIONED_COMPONENT_SERVICE_NAME,
            remote_provisioning_service.as_binder(),
        )
        .unwrap();
        info!("Registered Binder service {}.", REMOTELY_PROVISIONED_COMPONENT_SERVICE_NAME);
    }

    ProcessState::join_thread_pool();
}

/// Remove any files under `TEMPORARY_DIRECTORY`.
fn clear_temporary_files() -> Result<(), Error> {
    for dir_entry in read_dir(TEMPORARY_DIRECTORY)? {
        remove_temporary_dir(&dir_entry?.path())?
    }
    Ok(())
}
