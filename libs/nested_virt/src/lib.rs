/*
 * Copyright 2022 The Android Open Source Project
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

//! Detection for nested virtualization.

use anyhow::Result;
use rustutils::system_properties;

/// Return whether we will be running our VM in a VM, which causes the nested VM to run very slowly.
pub fn is_nested_virtualization() -> Result<bool> {
    // Currently nested virtualization only occurs when we run KVM inside the cuttlefish VM.
    // So we just need to check for vsoc.
    if let Some(value) = system_properties::read("ro.product.vendor.device")? {
        // Fuzzy matching to allow for vsoc_x86, vsoc_x86_64, vsoc_x86_64_only, ...
        Ok(value.starts_with("vsoc_"))
    } else {
        Ok(false)
    }
}
