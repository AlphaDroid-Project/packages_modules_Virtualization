// Copyright 2023, The Android Open Source Project
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

//! Access to hypervisor capabilities via system properties set by the bootloader.

use anyhow::{Error, Result};
use rustutils::system_properties;

/// Returns whether there is a hypervisor present that supports non-protected VMs.
pub fn is_vm_supported() -> Result<bool> {
    system_properties::read_bool("ro.boot.hypervisor.vm.supported", false).map_err(Error::new)
}

/// Returns whether there is a hypervisor present that supports protected VMs.
pub fn is_protected_vm_supported() -> Result<bool> {
    system_properties::read_bool("ro.boot.hypervisor.protected_vm.supported", false)
        .map_err(Error::new)
}

/// Returns whether there is a hypervisor present that supports any sort of VM, either protected
/// or non-protected.
pub fn is_any_vm_supported() -> Result<bool> {
    is_vm_supported().and_then(|ok| if ok { Ok(true) } else { is_protected_vm_supported() })
}

/// Returns the version of the hypervisor, if there is one.
pub fn version() -> Result<Option<String>> {
    system_properties::read("ro.boot.hypervisor.version").map_err(Error::new)
}
