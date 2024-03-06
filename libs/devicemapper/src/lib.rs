/*
 * Copyright (C) 2021 The Android Open Source Project
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

// `dm` module implements part of the `device-mapper` ioctl interfaces. It currently supports
// creation and deletion of the mapper device. It doesn't support other operations like querying
// the status of the mapper device. And there's no plan to extend the support unless it is
// required.
//
// Why in-house development? [`devicemapper`](https://crates.io/crates/devicemapper) is a public
// Rust implementation of the device mapper APIs. However, it doesn't provide any abstraction for
// the target-specific tables. User has to manually craft the table. Ironically, the library
// provides a lot of APIs for the features that are not required for `apkdmverity` such as listing
// the device mapper block devices that are currently listed in the kernel. Size is an important
// criteria for Microdroid.

//! A library to create device mapper spec & issue ioctls.

#![allow(missing_docs)]
#![cfg_attr(test, allow(unused))]

use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::mem::size_of;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use zerocopy::AsBytes;
use zerocopy::FromZeroes;

/// Exposes DmCryptTarget & related builder
pub mod crypt;
/// Expose util functions
pub mod util;
/// Exposes the DmVerityTarget & related builder
pub mod verity;
// Expose loopdevice
pub mod loopdevice;

mod sys;
use crypt::DmCryptTarget;
use sys::*;
use util::*;
use verity::DmVerityTarget;

nix::ioctl_readwrite!(_dm_dev_create, DM_IOCTL, Cmd::DM_DEV_CREATE, DmIoctl);
nix::ioctl_readwrite!(_dm_dev_suspend, DM_IOCTL, Cmd::DM_DEV_SUSPEND, DmIoctl);
nix::ioctl_readwrite!(_dm_table_load, DM_IOCTL, Cmd::DM_TABLE_LOAD, DmIoctl);
nix::ioctl_readwrite!(_dm_dev_remove, DM_IOCTL, Cmd::DM_DEV_REMOVE, DmIoctl);

/// Create a new (mapper) device
fn dm_dev_create(dm: &DeviceMapper, ioctl: *mut DmIoctl) -> Result<i32> {
    // SAFETY: `ioctl` is copied into the kernel. It modifies the state in the kernel, not the
    // state of this process in any way.
    Ok(unsafe { _dm_dev_create(dm.0.as_raw_fd(), ioctl) }?)
}

fn dm_dev_suspend(dm: &DeviceMapper, ioctl: *mut DmIoctl) -> Result<i32> {
    // SAFETY: `ioctl` is copied into the kernel. It modifies the state in the kernel, not the
    // state of this process in any way.
    Ok(unsafe { _dm_dev_suspend(dm.0.as_raw_fd(), ioctl) }?)
}

fn dm_table_load(dm: &DeviceMapper, ioctl: *mut DmIoctl) -> Result<i32> {
    // SAFETY: `ioctl` is copied into the kernel. It modifies the state in the kernel, not the
    // state of this process in any way.
    Ok(unsafe { _dm_table_load(dm.0.as_raw_fd(), ioctl) }?)
}

fn dm_dev_remove(dm: &DeviceMapper, ioctl: *mut DmIoctl) -> Result<i32> {
    // SAFETY: `ioctl` is copied into the kernel. It modifies the state in the kernel, not the
    // state of this process in any way.
    Ok(unsafe { _dm_dev_remove(dm.0.as_raw_fd(), ioctl) }?)
}

// `DmTargetSpec` is the header of the data structure for a device-mapper target. When doing the
// ioctl, one of more `DmTargetSpec` (and its body) are appened to the `DmIoctl` struct.
#[repr(C)]
#[derive(Copy, Clone, AsBytes, FromZeroes)]
struct DmTargetSpec {
    sector_start: u64,
    length: u64, // number of 512 sectors
    status: i32,
    next: u32,
    target_type: [u8; DM_MAX_TYPE_NAME],
}

impl DmTargetSpec {
    fn new(target_type: &str) -> Result<Self> {
        let mut spec = Self::new_zeroed();
        spec.target_type.as_mut().write_all(target_type.as_bytes())?;
        Ok(spec)
    }
}

impl DmIoctl {
    fn new(name: &str) -> Result<DmIoctl> {
        let mut data: Self = Self::new_zeroed();
        data.version[0] = DM_VERSION_MAJOR;
        data.version[1] = DM_VERSION_MINOR;
        data.version[2] = DM_VERSION_PATCHLEVEL;
        data.data_size = size_of::<Self>() as u32;
        data.data_start = 0;
        data.name.as_mut().write_all(name.as_bytes())?;
        Ok(data)
    }

    fn set_uuid(&mut self, uuid: &str) -> Result<()> {
        let mut dst = self.uuid.as_mut();
        dst.fill(0);
        dst.write_all(uuid.as_bytes())?;
        Ok(())
    }
}

/// `DeviceMapper` is the entry point for the device mapper framework. It essentially is a file
/// handle to "/dev/mapper/control".
pub struct DeviceMapper(File);

#[cfg(not(target_os = "android"))]
const MAPPER_CONTROL: &str = "/dev/mapper/control";
#[cfg(not(target_os = "android"))]
const MAPPER_DEV_ROOT: &str = "/dev/mapper";

#[cfg(target_os = "android")]
const MAPPER_CONTROL: &str = "/dev/device-mapper";
#[cfg(target_os = "android")]
const MAPPER_DEV_ROOT: &str = "/dev/block/mapper";

impl DeviceMapper {
    /// Constructs a new `DeviceMapper` entrypoint. This is essentially the same as opening
    /// "/dev/mapper/control".
    pub fn new() -> Result<DeviceMapper> {
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .open(MAPPER_CONTROL)
            .context(format!("failed to open {}", MAPPER_CONTROL))?;
        Ok(DeviceMapper(f))
    }

    /// Creates a (crypt) device and configure it according to the `target` specification.
    /// The path to the generated device is "/dev/mapper/<name>".
    pub fn create_crypt_device(&self, name: &str, target: &DmCryptTarget) -> Result<PathBuf> {
        self.create_device(name, target.as_slice(), uuid("crypto".as_bytes())?, true)
    }

    /// Creates a (verity) device and configure it according to the `target` specification.
    /// The path to the generated device is "/dev/mapper/<name>".
    pub fn create_verity_device(&self, name: &str, target: &DmVerityTarget) -> Result<PathBuf> {
        self.create_device(name, target.as_slice(), uuid("apkver".as_bytes())?, false)
    }

    /// Removes a mapper device.
    pub fn delete_device_deferred(&self, name: &str) -> Result<()> {
        let mut data = DmIoctl::new(name)?;
        data.flags |= Flag::DM_DEFERRED_REMOVE;
        dm_dev_remove(self, &mut data)
            .context(format!("failed to remove device with name {}", &name))?;
        Ok(())
    }

    fn create_device(
        &self,
        name: &str,
        target: &[u8],
        uid: String,
        writable: bool,
    ) -> Result<PathBuf> {
        // Step 1: create an empty device
        let mut data = DmIoctl::new(name)?;
        data.set_uuid(&uid)?;
        dm_dev_create(self, &mut data)
            .context(format!("failed to create an empty device with name {}", &name))?;

        // Step 2: load table onto the device
        let payload_size = size_of::<DmIoctl>() + target.len();

        let mut data = DmIoctl::new(name)?;
        data.data_size = payload_size as u32;
        data.data_start = size_of::<DmIoctl>() as u32;
        data.target_count = 1;

        if !writable {
            data.flags |= Flag::DM_READONLY_FLAG;
        }

        let mut payload = Vec::with_capacity(payload_size);
        payload.extend_from_slice(data.as_bytes());
        payload.extend_from_slice(target);
        dm_table_load(self, payload.as_mut_ptr() as *mut DmIoctl)
            .context("failed to load table")?;

        // Step 3: activate the device (note: the term 'suspend' might be misleading, but it
        // actually activates the table. See include/uapi/linux/dm-ioctl.h
        let mut data = DmIoctl::new(name)?;
        dm_dev_suspend(self, &mut data).context("failed to activate")?;

        // Step 4: wait unti the device is created and return the device path
        let path = Path::new(MAPPER_DEV_ROOT).join(name);
        wait_for_path(&path)?;
        Ok(path)
    }
}

/// Used to derive a UUID that uniquely identifies a device mapper device when creating it.
fn uuid(node_id: &[u8]) -> Result<String> {
    use std::time::{SystemTime, UNIX_EPOCH};
    use uuid::v1::{Context, Timestamp};
    use uuid::Uuid;

    let context = Context::new(0);
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?;
    let ts = Timestamp::from_unix(context, now.as_secs(), now.subsec_nanos());
    let uuid = Uuid::new_v1(ts, node_id.try_into()?);
    Ok(String::from(uuid.hyphenated().encode_lower(&mut Uuid::encode_buffer())))
}

#[cfg(test)]
rdroidtest::test_main!();

#[cfg(test)]
mod tests {
    use super::*;
    use crypt::{CipherType, DmCryptTargetBuilder};
    use rdroidtest::test;
    use rustutils::system_properties;
    use std::fs::{read, File, OpenOptions};
    use std::io::Write;

    // Just a logical set of keys to make testing easy. This has no real meaning.
    struct KeySet<'a> {
        cipher: CipherType,
        key: &'a [u8],
        different_key: &'a [u8],
    }

    const KEY_SET_XTS: KeySet = KeySet {
        cipher: CipherType::AES256XTS,
        key: b"sixtyfourbyteslongsentencearerarebutletsgiveitatrycantbethathard",
        different_key: b"drahtahtebtnacyrtatievigsteltuberareraecnetnesgnolsetybruofytxis",
    };
    const KEY_SET_HCTR2: KeySet = KeySet {
        cipher: CipherType::AES256HCTR2,
        key: b"thirtytwobyteslongreallylongword",
        different_key: b"drowgnolyllaergnolsetybowtytriht",
    };

    // Create a file in given temp directory with given size
    fn prepare_tmpfile(test_dir: &Path, filename: &str, sz: u64) -> PathBuf {
        let filepath = test_dir.join(filename);
        let f = File::create(&filepath).unwrap();
        f.set_len(sz).unwrap();
        filepath
    }

    fn write_to_dev(path: &Path, data: &[u8]) {
        let mut f = OpenOptions::new().read(true).write(true).open(path).unwrap();
        f.write_all(data).unwrap();
    }

    // TODO(b/250880499): delete_device() doesn't really delete it even without DM_DEFERRED_REMOVE.
    // Hence, we have to create a new device with a different name for each test. Retrying
    // the test on same machine without reboot will also fail.
    fn delete_device(dm: &DeviceMapper, name: &str) -> Result<()> {
        dm.delete_device_deferred(name)?;
        wait_for_path_disappears(Path::new(MAPPER_DEV_ROOT).join(name))?;
        Ok(())
    }

    fn is_hctr2_supported() -> bool {
        // hctr2 is NOT enabled in kernel 5.10 or lower. We run Microdroid tests on kernel versions
        // 5.10 or above & therefore,  we don't really care to skip test on other versions.
        if let Some(version) = system_properties::read("ro.kernel.version")
            .expect("Unable to read system property ro.kernel.version")
        {
            version != "5.10"
        } else {
            panic!("Could not read property: kernel.version!!");
        }
    }

    test!(mapping_again_keeps_data_xts);
    fn mapping_again_keeps_data_xts() {
        mapping_again_keeps_data(&KEY_SET_XTS, "name1");
    }

    test!(mapping_again_keeps_data_hctr2, ignore_if: !is_hctr2_supported());
    fn mapping_again_keeps_data_hctr2() {
        mapping_again_keeps_data(&KEY_SET_HCTR2, "name2");
    }

    test!(data_inaccessible_with_diff_key_xts);
    fn data_inaccessible_with_diff_key_xts() {
        data_inaccessible_with_diff_key(&KEY_SET_XTS, "name3");
    }

    test!(data_inaccessible_with_diff_key_hctr2, ignore_if: !is_hctr2_supported());
    fn data_inaccessible_with_diff_key_hctr2() {
        data_inaccessible_with_diff_key(&KEY_SET_HCTR2, "name4");
    }

    fn mapping_again_keeps_data(keyset: &KeySet, device: &str) {
        // This test creates 2 different crypt devices using same key backed by same data_device
        // -> Write data on dev1 -> Check the data is visible & same on dev2
        let dm = DeviceMapper::new().unwrap();
        let inputimg = include_bytes!("../testdata/rand8k");
        let sz = inputimg.len() as u64;

        let test_dir = tempfile::TempDir::new().unwrap();
        let backing_file = prepare_tmpfile(test_dir.path(), "storage", sz);
        let data_device = loopdevice::attach(
            backing_file,
            0,
            sz,
            /*direct_io*/ true,
            /*writable*/ true,
        )
        .unwrap();
        let device_diff = device.to_owned() + "_diff";

        scopeguard::defer! {
            loopdevice::detach(&data_device).unwrap();
            let _ignored1 = delete_device(&dm, device);
            let _ignored2 = delete_device(&dm, &device_diff);
        }

        let target = DmCryptTargetBuilder::default()
            .data_device(&data_device, sz)
            .cipher(keyset.cipher)
            .key(keyset.key)
            .build()
            .unwrap();

        let mut crypt_device = dm.create_crypt_device(device, &target).unwrap();
        write_to_dev(&crypt_device, inputimg);

        // Recreate another device using same target spec & check if the content is the same
        crypt_device = dm.create_crypt_device(&device_diff, &target).unwrap();

        let crypt = read(crypt_device).unwrap();
        assert_eq!(inputimg.len(), crypt.len()); // fail early if the size doesn't match
        assert_eq!(inputimg, crypt.as_slice());
    }

    fn data_inaccessible_with_diff_key(keyset: &KeySet, device: &str) {
        // This test creates 2 different crypt devices using different keys backed
        // by same data_device -> Write data on dev1 -> Check the data is visible but not the same on dev2
        let dm = DeviceMapper::new().unwrap();
        let inputimg = include_bytes!("../testdata/rand8k");
        let sz = inputimg.len() as u64;

        let test_dir = tempfile::TempDir::new().unwrap();
        let backing_file = prepare_tmpfile(test_dir.path(), "storage", sz);
        let data_device = loopdevice::attach(
            backing_file,
            0,
            sz,
            /*direct_io*/ true,
            /*writable*/ true,
        )
        .unwrap();
        let device_diff = device.to_owned() + "_diff";
        scopeguard::defer! {
            loopdevice::detach(&data_device).unwrap();
            let _ignored1 = delete_device(&dm, device);
            let _ignored2 = delete_device(&dm, &device_diff);
        }

        let target = DmCryptTargetBuilder::default()
            .data_device(&data_device, sz)
            .cipher(keyset.cipher)
            .key(keyset.key)
            .build()
            .unwrap();
        let target2 = DmCryptTargetBuilder::default()
            .data_device(&data_device, sz)
            .cipher(keyset.cipher)
            .key(keyset.different_key)
            .build()
            .unwrap();

        let mut crypt_device = dm.create_crypt_device(device, &target).unwrap();

        write_to_dev(&crypt_device, inputimg);

        // Recreate the crypt device again diff key & check if the content is changed
        crypt_device = dm.create_crypt_device(&device_diff, &target2).unwrap();
        let crypt = read(crypt_device).unwrap();
        assert_ne!(inputimg, crypt.as_slice());
    }
}
