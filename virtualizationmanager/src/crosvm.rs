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

//! Functions for running instances of `crosvm`.

use crate::aidl::{remove_temporary_files, Cid, VirtualMachineCallbacks};
use crate::atom::{get_num_cpus, write_vm_exited_stats_sync};
use crate::debug_config::DebugConfig;
use anyhow::{anyhow, bail, Context, Error, Result};
use command_fds::CommandFdExt;
use lazy_static::lazy_static;
use libc::{sysconf, _SC_CLK_TCK};
use log::{debug, error, info};
use semver::{Version, VersionReq};
use nix::{fcntl::OFlag, unistd::pipe2, unistd::Uid, unistd::User};
use regex::{Captures, Regex};
use rustutils::system_properties;
use shared_child::SharedChild;
use std::borrow::Cow;
use std::cmp::max;
use std::fmt;
use std::fs::{read_to_string, File};
use std::io::{self, Read};
use std::mem;
use std::num::{NonZeroU16, NonZeroU32};
use std::os::unix::io::{AsRawFd, RawFd, FromRawFd};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime};
use std::thread::{self, JoinHandle};
use android_system_virtualizationcommon::aidl::android::system::virtualizationcommon::DeathReason::DeathReason;
use android_system_virtualizationservice::aidl::android::system::virtualizationservice::{
    MemoryTrimLevel::MemoryTrimLevel,
    VirtualMachineAppConfig::DebugLevel::DebugLevel
};
use android_system_virtualizationservice_internal::aidl::android::system::virtualizationservice_internal::IGlobalVmContext::IGlobalVmContext;
use binder::Strong;
use android_system_virtualmachineservice::aidl::android::system::virtualmachineservice::IVirtualMachineService::IVirtualMachineService;
use tombstoned_client::{TombstonedConnection, DebuggerdDumpType};
use rpcbinder::RpcServer;

/// external/crosvm
use base::AsRawDescriptor;
use base::UnixSeqpacketListener;
use vm_control::{BalloonControlCommand, VmRequest, VmResponse};

const CROSVM_PATH: &str = "/apex/com.android.virt/bin/crosvm";

/// Version of the platform that crosvm currently implements. The format follows SemVer. This
/// should be updated when there is a platform change in the crosvm side. Having this value here is
/// fine because virtualizationservice and crosvm are supposed to be updated together in the virt
/// APEX.
const CROSVM_PLATFORM_VERSION: &str = "1.0.0";

/// The exit status which crosvm returns when it has an error starting a VM.
const CROSVM_START_ERROR_STATUS: i32 = 1;
/// The exit status which crosvm returns when a VM requests a reboot.
const CROSVM_REBOOT_STATUS: i32 = 32;
/// The exit status which crosvm returns when it crashes due to an error.
const CROSVM_CRASH_STATUS: i32 = 33;
/// The exit status which crosvm returns when vcpu is stalled.
const CROSVM_WATCHDOG_REBOOT_STATUS: i32 = 36;
/// The size of memory (in MiB) reserved for ramdump
const RAMDUMP_RESERVED_MIB: u32 = 17;

const MILLIS_PER_SEC: i64 = 1000;

const SYSPROP_CUSTOM_PVMFW_PATH: &str = "hypervisor.pvmfw.path";

lazy_static! {
    /// If the VM doesn't move to the Started state within this amount time, a hang-up error is
    /// triggered.
    static ref BOOT_HANGUP_TIMEOUT: Duration = if nested_virt::is_nested_virtualization().unwrap() {
        // Nested virtualization is slow, so we need a longer timeout.
        Duration::from_secs(300)
    } else {
        Duration::from_secs(30)
    };
}

/// Configuration for a VM to run with crosvm.
#[derive(Debug)]
pub struct CrosvmConfig {
    pub cid: Cid,
    pub name: String,
    pub bootloader: Option<File>,
    pub kernel: Option<File>,
    pub initrd: Option<File>,
    pub disks: Vec<DiskFile>,
    pub params: Option<String>,
    pub protected: bool,
    pub debug_config: DebugConfig,
    pub memory_mib: Option<NonZeroU32>,
    pub cpus: Option<NonZeroU32>,
    pub host_cpu_topology: bool,
    pub task_profiles: Vec<String>,
    pub console_out_fd: Option<File>,
    pub console_in_fd: Option<File>,
    pub log_fd: Option<File>,
    pub ramdump: Option<File>,
    pub indirect_files: Vec<File>,
    pub platform_version: VersionReq,
    pub detect_hangup: bool,
    pub gdb_port: Option<NonZeroU16>,
    pub vfio_devices: Vec<VfioDevice>,
    pub dtbo: Option<File>,
    pub dtbo_vendor: Option<File>,
}

/// A disk image to pass to crosvm for a VM.
#[derive(Debug)]
pub struct DiskFile {
    pub image: File,
    pub writable: bool,
}

#[derive(Clone, Debug)]
pub struct VfioDevice {
    pub sysfs_path: PathBuf,
    pub dtbo_label: String,
}

/// The lifecycle state which the payload in the VM has reported itself to be in.
///
/// Note that the order of enum variants is significant; only forward transitions are allowed by
/// [`VmInstance::update_payload_state`].
#[derive(Copy, Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PayloadState {
    Starting,
    Started,
    Ready,
    Finished,
    Hangup, // Hasn't reached to Ready before timeout expires
}

/// The current state of the VM itself.
#[derive(Debug)]
pub enum VmState {
    /// The VM has not yet tried to start.
    NotStarted {
        ///The configuration needed to start the VM, if it has not yet been started.
        config: Box<CrosvmConfig>,
    },
    /// The VM has been started.
    Running {
        /// The crosvm child process.
        child: Arc<SharedChild>,
        /// The thread waiting for crosvm to finish.
        monitor_vm_exit_thread: Option<JoinHandle<()>>,
    },
    /// The VM died or was killed.
    Dead,
    /// The VM failed to start.
    Failed,
}

/// RSS values of VM and CrosVM process itself.
#[derive(Copy, Clone, Debug, Default)]
pub struct Rss {
    pub vm: i64,
    pub crosvm: i64,
}

/// Metrics regarding the VM.
#[derive(Debug, Default)]
pub struct VmMetric {
    /// Recorded timestamp when the VM is started.
    pub start_timestamp: Option<SystemTime>,
    /// Update most recent guest_time periodically from /proc/[crosvm pid]/stat while VM is
    /// running.
    pub cpu_guest_time: Option<i64>,
    /// Update maximum RSS values periodically from /proc/[crosvm pid]/smaps while VM is running.
    pub rss: Option<Rss>,
}

impl VmState {
    /// Tries to start the VM, if it is in the `NotStarted` state.
    ///
    /// Returns an error if the VM is in the wrong state, or fails to start.
    fn start(&mut self, instance: Arc<VmInstance>) -> Result<(), Error> {
        let state = mem::replace(self, VmState::Failed);
        if let VmState::NotStarted { config } = state {
            let config = *config;
            let detect_hangup = config.detect_hangup;
            let (failure_pipe_read, failure_pipe_write) = create_pipe()?;
            let vfio_devices = config.vfio_devices.clone();

            // If this fails and returns an error, `self` will be left in the `Failed` state.
            let child =
                Arc::new(run_vm(config, &instance.crosvm_control_socket_path, failure_pipe_write)?);

            let instance_monitor_status = instance.clone();
            let child_monitor_status = child.clone();
            thread::spawn(move || {
                instance_monitor_status.clone().monitor_vm_status(child_monitor_status);
            });

            let child_clone = child.clone();
            let instance_clone = instance.clone();
            let monitor_vm_exit_thread = Some(thread::spawn(move || {
                instance_clone.monitor_vm_exit(child_clone, failure_pipe_read, vfio_devices);
            }));

            if detect_hangup {
                let child_clone = child.clone();
                thread::spawn(move || {
                    instance.monitor_payload_hangup(child_clone);
                });
            }

            // If it started correctly, update the state.
            *self = VmState::Running { child, monitor_vm_exit_thread };
            Ok(())
        } else {
            *self = state;
            bail!("VM already started or failed")
        }
    }
}

/// Internal struct that holds the handles to globally unique resources of a VM.
#[derive(Debug)]
pub struct VmContext {
    #[allow(dead_code)] // Keeps the global context alive
    global_context: Strong<dyn IGlobalVmContext>,
    #[allow(dead_code)] // Keeps the server alive
    vm_server: RpcServer,
}

impl VmContext {
    /// Construct new VmContext.
    pub fn new(global_context: Strong<dyn IGlobalVmContext>, vm_server: RpcServer) -> VmContext {
        VmContext { global_context, vm_server }
    }
}

/// Information about a particular instance of a VM which may be running.
#[derive(Debug)]
pub struct VmInstance {
    /// The current state of the VM.
    pub vm_state: Mutex<VmState>,
    /// Global resources allocated for this VM.
    #[allow(dead_code)] // Keeps the context alive
    vm_context: VmContext,
    /// The CID assigned to the VM for vsock communication.
    pub cid: Cid,
    /// Path to crosvm control socket
    crosvm_control_socket_path: PathBuf,
    /// The name of the VM.
    pub name: String,
    /// Whether the VM is a protected VM.
    pub protected: bool,
    /// Directory of temporary files used by the VM while it is running.
    pub temporary_directory: PathBuf,
    /// The UID of the process which requested the VM.
    pub requester_uid: u32,
    /// The PID of the process which requested the VM. Note that this process may no longer exist
    /// and the PID may have been reused for a different process, so this should not be trusted.
    pub requester_debug_pid: i32,
    /// Callbacks to clients of the VM.
    pub callbacks: VirtualMachineCallbacks,
    /// VirtualMachineService binder object for the VM.
    pub vm_service: Mutex<Option<Strong<dyn IVirtualMachineService>>>,
    /// Recorded metrics of VM such as timestamp or cpu / memory usage.
    pub vm_metric: Mutex<VmMetric>,
    /// The latest lifecycle state which the payload reported itself to be in.
    payload_state: Mutex<PayloadState>,
    /// Represents the condition that payload_state was updated
    payload_state_updated: Condvar,
    /// The human readable name of requester_uid
    requester_uid_name: String,
}

impl fmt::Display for VmInstance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let adj = if self.protected { "Protected" } else { "Non-protected" };
        write!(
            f,
            "{} virtual machine \"{}\" (owner: {}, cid: {})",
            adj, self.name, self.requester_uid_name, self.cid
        )
    }
}

impl VmInstance {
    /// Validates the given config and creates a new `VmInstance` but doesn't start running it.
    pub fn new(
        config: CrosvmConfig,
        temporary_directory: PathBuf,
        requester_uid: u32,
        requester_debug_pid: i32,
        vm_context: VmContext,
    ) -> Result<VmInstance, Error> {
        validate_config(&config)?;
        let cid = config.cid;
        let name = config.name.clone();
        let protected = config.protected;
        let requester_uid_name = User::from_uid(Uid::from_raw(requester_uid))
            .ok()
            .flatten()
            .map_or_else(|| format!("{}", requester_uid), |u| u.name);
        let instance = VmInstance {
            vm_state: Mutex::new(VmState::NotStarted { config: Box::new(config) }),
            vm_context,
            cid,
            crosvm_control_socket_path: temporary_directory.join("crosvm.sock"),
            name,
            protected,
            temporary_directory,
            requester_uid,
            requester_debug_pid,
            callbacks: Default::default(),
            vm_service: Mutex::new(None),
            vm_metric: Mutex::new(Default::default()),
            payload_state: Mutex::new(PayloadState::Starting),
            payload_state_updated: Condvar::new(),
            requester_uid_name,
        };
        info!("{} created", &instance);
        Ok(instance)
    }

    /// Starts an instance of `crosvm` to manage the VM. The `crosvm` instance will be killed when
    /// the `VmInstance` is dropped.
    pub fn start(self: &Arc<Self>) -> Result<(), Error> {
        let mut vm_metric = self.vm_metric.lock().unwrap();
        vm_metric.start_timestamp = Some(SystemTime::now());
        let ret = self.vm_state.lock().unwrap().start(self.clone());
        if ret.is_ok() {
            info!("{} started", &self);
        }
        ret.with_context(|| format!("{} failed to start", &self))
    }

    /// Monitors the exit of the VM (i.e. termination of the `child` process). When that happens,
    /// handles the event by updating the state, noityfing the event to clients by calling
    /// callbacks, and removing temporary files for the VM.
    fn monitor_vm_exit(
        &self,
        child: Arc<SharedChild>,
        mut failure_pipe_read: File,
        vfio_devices: Vec<VfioDevice>,
    ) {
        let result = child.wait();
        match &result {
            Err(e) => error!("Error waiting for crosvm({}) instance to die: {}", child.id(), e),
            Ok(status) => {
                info!("crosvm({}) exited with status {}", child.id(), status);
                if let Some(exit_status_code) = status.code() {
                    if exit_status_code == CROSVM_WATCHDOG_REBOOT_STATUS {
                        info!("detected vcpu stall on crosvm");
                    }
                }
            }
        }

        let mut vm_state = self.vm_state.lock().unwrap();
        *vm_state = VmState::Dead;
        // Ensure that the mutex is released before calling the callbacks.
        drop(vm_state);
        info!("{} exited", &self);

        // Read the pipe to see if any failure reason is written
        let mut failure_reason = String::new();
        match failure_pipe_read.read_to_string(&mut failure_reason) {
            Err(e) => error!("Error reading VM failure reason from pipe: {}", e),
            Ok(len) if len > 0 => info!("VM returned failure reason '{}'", &failure_reason),
            _ => (),
        };

        // In case of hangup, the pipe doesn't give us any information because the hangup can't be
        // detected on the VM side (otherwise, it isn't a hangup), but in the
        // monitor_payload_hangup function below which updates the payload state to Hangup.
        let failure_reason =
            if failure_reason.is_empty() && self.payload_state() == PayloadState::Hangup {
                Cow::from("HANGUP")
            } else {
                Cow::from(failure_reason)
            };

        self.handle_ramdump().unwrap_or_else(|e| error!("Error handling ramdump: {}", e));

        let death_reason = death_reason(&result, &failure_reason);
        let exit_signal = exit_signal(&result);

        self.callbacks.callback_on_died(self.cid, death_reason);

        let vm_metric = self.vm_metric.lock().unwrap();
        write_vm_exited_stats_sync(
            self.requester_uid as i32,
            &self.name,
            death_reason,
            exit_signal,
            &vm_metric,
        );

        // Delete temporary files. The folder itself is removed by VirtualizationServiceInternal.
        remove_temporary_files(&self.temporary_directory).unwrap_or_else(|e| {
            error!("Error removing temporary files from {:?}: {}", self.temporary_directory, e);
        });

        // TODO(b/278008182): clean up assigned devices.
        for device in vfio_devices.iter() {
            info!("NOT RELEASING {device:?}");
        }
    }

    /// Waits until payload is started, or timeout expires. When timeout occurs, kill
    /// the VM to prevent indefinite hangup and update the payload_state accordingly.
    fn monitor_payload_hangup(&self, child: Arc<SharedChild>) {
        debug!("Starting to monitor hangup for Microdroid({})", child.id());
        let (_, result) = self
            .payload_state_updated
            .wait_timeout_while(self.payload_state.lock().unwrap(), *BOOT_HANGUP_TIMEOUT, |s| {
                *s < PayloadState::Started
            })
            .unwrap();
        let child_still_running = child.try_wait().ok() == Some(None);
        if result.timed_out() && child_still_running {
            error!(
                "Microdroid({}) failed to start payload within {} secs timeout. Shutting down.",
                child.id(),
                BOOT_HANGUP_TIMEOUT.as_secs()
            );
            self.update_payload_state(PayloadState::Hangup).unwrap();
            if let Err(e) = self.kill() {
                error!("Error stopping timed-out VM with CID {}: {:?}", child.id(), e);
            }
        }
    }

    fn monitor_vm_status(&self, child: Arc<SharedChild>) {
        let pid = child.id();

        loop {
            {
                // Check VM state
                let vm_state = &*self.vm_state.lock().unwrap();
                if let VmState::Dead = vm_state {
                    break;
                }

                let mut vm_metric = self.vm_metric.lock().unwrap();

                // Get CPU Information
                if let Ok(guest_time) = get_guest_time(pid) {
                    vm_metric.cpu_guest_time = Some(guest_time);
                } else {
                    error!("Failed to parse /proc/[pid]/stat");
                }

                // Get Memory Information
                if let Ok(rss) = get_rss(pid) {
                    vm_metric.rss = match &vm_metric.rss {
                        Some(x) => Some(Rss::extract_max(x, &rss)),
                        None => Some(rss),
                    }
                } else {
                    error!("Failed to parse /proc/[pid]/smaps");
                }
            }

            thread::sleep(Duration::from_secs(1));
        }
    }

    /// Returns the last reported state of the VM payload.
    pub fn payload_state(&self) -> PayloadState {
        *self.payload_state.lock().unwrap()
    }

    /// Updates the payload state to the given value, if it is a valid state transition.
    pub fn update_payload_state(&self, new_state: PayloadState) -> Result<(), Error> {
        let mut state_locked = self.payload_state.lock().unwrap();
        // Only allow forward transitions, e.g. from starting to started or finished, not back in
        // the other direction.
        if new_state > *state_locked {
            *state_locked = new_state;
            self.payload_state_updated.notify_all();
            Ok(())
        } else {
            bail!("Invalid payload state transition from {:?} to {:?}", *state_locked, new_state)
        }
    }

    /// Kills the crosvm instance, if it is running.
    pub fn kill(&self) -> Result<(), Error> {
        let monitor_vm_exit_thread = {
            let vm_state = &mut *self.vm_state.lock().unwrap();
            if let VmState::Running { child, monitor_vm_exit_thread } = vm_state {
                let id = child.id();
                debug!("Killing crosvm({})", id);
                // TODO: Talk to crosvm to shutdown cleanly.
                child.kill().with_context(|| format!("Error killing crosvm({id}) instance"))?;
                monitor_vm_exit_thread.take()
            } else {
                bail!("VM is not running")
            }
        };

        // Wait for monitor_vm_exit() to finish. Must release vm_state lock
        // first, as monitor_vm_exit() takes it as well.
        monitor_vm_exit_thread.map(JoinHandle::join);

        // Now that the VM has been killed, shut down the VirtualMachineService
        // server to eagerly free up the server threads.
        self.vm_context.vm_server.shutdown()?;

        Ok(())
    }

    /// Responds to memory-trimming notifications by inflating the virtio
    /// balloon to reclaim guest memory.
    pub fn trim_memory(&self, level: MemoryTrimLevel) -> Result<(), Error> {
        let request = VmRequest::BalloonCommand(BalloonControlCommand::Stats {});
        match vm_control::client::handle_request(&request, &self.crosvm_control_socket_path) {
            Ok(VmResponse::BalloonStats { stats, balloon_actual: _ }) => {
                if let Some(total_memory) = stats.total_memory {
                    // Reclaim up to 50% of total memory assuming worst case
                    // most memory is anonymous and must be swapped to zram
                    // with an approximate 2:1 compression ratio.
                    let pct = match level {
                        MemoryTrimLevel::TRIM_MEMORY_RUNNING_CRITICAL => 50,
                        MemoryTrimLevel::TRIM_MEMORY_RUNNING_LOW => 30,
                        MemoryTrimLevel::TRIM_MEMORY_RUNNING_MODERATE => 10,
                        _ => bail!("Invalid memory trim level {:?}", level),
                    };
                    let command = BalloonControlCommand::Adjust {
                        num_bytes: total_memory * pct / 100,
                        wait_for_success: false,
                    };
                    if let Err(e) = vm_control::client::handle_request(
                        &VmRequest::BalloonCommand(command),
                        &self.crosvm_control_socket_path,
                    ) {
                        bail!("Error sending balloon adjustment: {:?}", e);
                    }
                }
            }
            Ok(VmResponse::Err(e)) => {
                // ENOTSUP is returned when the balloon protocol is not initialized. This
                // can occur for numerous reasons: Guest is still booting, guest doesn't
                // support ballooning, host doesn't support ballooning. We don't log or
                // raise an error in this case: trim is just a hint and we can ignore it.
                if e.errno() != libc::ENOTSUP {
                    bail!("Errno return when requesting balloon stats: {}", e.errno())
                }
            }
            e => bail!("Error requesting balloon stats: {:?}", e),
        }
        Ok(())
    }

    /// Checks if ramdump has been created. If so, send it to tombstoned.
    fn handle_ramdump(&self) -> Result<(), Error> {
        let ramdump_path = self.temporary_directory.join("ramdump");
        if !ramdump_path.as_path().try_exists()? {
            return Ok(());
        }
        if std::fs::metadata(&ramdump_path)?.len() > 0 {
            Self::send_ramdump_to_tombstoned(&ramdump_path)?;
        }
        Ok(())
    }

    fn send_ramdump_to_tombstoned(ramdump_path: &Path) -> Result<(), Error> {
        let mut input = File::open(ramdump_path)
            .context(format!("Failed to open ramdump {:?} for reading", ramdump_path))?;

        let pid = std::process::id() as i32;
        let conn = TombstonedConnection::connect(pid, DebuggerdDumpType::Tombstone)
            .context("Failed to connect to tombstoned")?;
        let mut output = conn
            .text_output
            .as_ref()
            .ok_or_else(|| anyhow!("Could not get file to write the tombstones on"))?;

        std::io::copy(&mut input, &mut output).context("Failed to send ramdump to tombstoned")?;
        info!("Ramdump {:?} sent to tombstoned", ramdump_path);

        conn.notify_completion()?;
        Ok(())
    }
}

impl Rss {
    fn extract_max(x: &Rss, y: &Rss) -> Rss {
        Rss { vm: max(x.vm, y.vm), crosvm: max(x.crosvm, y.crosvm) }
    }
}

// Get guest time from /proc/[crosvm pid]/stat
fn get_guest_time(pid: u32) -> Result<i64> {
    let file = read_to_string(format!("/proc/{}/stat", pid))?;
    let data_list: Vec<_> = file.split_whitespace().collect();

    // Information about guest_time is at 43th place of the file split with the whitespace.
    // Example of /proc/[pid]/stat :
    // 6603 (kworker/104:1H-kblockd) I 2 0 0 0 -1 69238880 0 0 0 0 0 88 0 0 0 -20 1 0 1845 0 0
    // 18446744073709551615 0 0 0 0 0 0 0 2147483647 0 0 0 0 17 104 0 0 0 0 0 0 0 0 0 0 0 0 0
    if data_list.len() < 43 {
        bail!("Failed to parse command result for getting guest time : {}", file);
    }

    let guest_time_ticks = data_list[42].parse::<i64>()?;
    // SAFETY: It just returns an integer about CPU tick information.
    let ticks_per_sec = unsafe { sysconf(_SC_CLK_TCK) };
    Ok(guest_time_ticks * MILLIS_PER_SEC / ticks_per_sec)
}

// Get rss from /proc/[crosvm pid]/smaps
fn get_rss(pid: u32) -> Result<Rss> {
    let file = read_to_string(format!("/proc/{}/smaps", pid))?;
    let lines: Vec<_> = file.split('\n').collect();

    let mut rss_vm_total = 0i64;
    let mut rss_crosvm_total = 0i64;
    let mut is_vm = false;
    for line in lines {
        if line.contains("crosvm_guest") {
            is_vm = true;
        } else if line.contains("Rss:") {
            let data_list: Vec<_> = line.split_whitespace().collect();
            if data_list.len() < 2 {
                bail!("Failed to parse command result for getting rss :\n{}", line);
            }
            let rss = data_list[1].parse::<i64>()?;

            if is_vm {
                rss_vm_total += rss;
                is_vm = false;
            }
            rss_crosvm_total += rss;
        }
    }

    Ok(Rss { vm: rss_vm_total, crosvm: rss_crosvm_total })
}

fn death_reason(result: &Result<ExitStatus, io::Error>, mut failure_reason: &str) -> DeathReason {
    if let Some((reason, info)) = failure_reason.split_once('|') {
        // Separator indicates extra context information is present after the failure name.
        error!("Failure info: {info}");
        failure_reason = reason;
    }
    if let Ok(status) = result {
        match failure_reason {
            "PVM_FIRMWARE_PUBLIC_KEY_MISMATCH" => {
                return DeathReason::PVM_FIRMWARE_PUBLIC_KEY_MISMATCH
            }
            "PVM_FIRMWARE_INSTANCE_IMAGE_CHANGED" => {
                return DeathReason::PVM_FIRMWARE_INSTANCE_IMAGE_CHANGED
            }
            "MICRODROID_FAILED_TO_CONNECT_TO_VIRTUALIZATION_SERVICE" => {
                return DeathReason::MICRODROID_FAILED_TO_CONNECT_TO_VIRTUALIZATION_SERVICE
            }
            "MICRODROID_PAYLOAD_HAS_CHANGED" => return DeathReason::MICRODROID_PAYLOAD_HAS_CHANGED,
            "MICRODROID_PAYLOAD_VERIFICATION_FAILED" => {
                return DeathReason::MICRODROID_PAYLOAD_VERIFICATION_FAILED
            }
            "MICRODROID_INVALID_PAYLOAD_CONFIG" => {
                return DeathReason::MICRODROID_INVALID_PAYLOAD_CONFIG
            }
            "MICRODROID_UNKNOWN_RUNTIME_ERROR" => {
                return DeathReason::MICRODROID_UNKNOWN_RUNTIME_ERROR
            }
            "HANGUP" => return DeathReason::HANGUP,
            _ => {}
        }
        match status.code() {
            None => DeathReason::KILLED,
            Some(0) => DeathReason::SHUTDOWN,
            Some(CROSVM_START_ERROR_STATUS) => DeathReason::START_FAILED,
            Some(CROSVM_REBOOT_STATUS) => DeathReason::REBOOT,
            Some(CROSVM_CRASH_STATUS) => DeathReason::CRASH,
            Some(CROSVM_WATCHDOG_REBOOT_STATUS) => DeathReason::WATCHDOG_REBOOT,
            Some(_) => DeathReason::UNKNOWN,
        }
    } else {
        DeathReason::INFRASTRUCTURE_ERROR
    }
}

fn exit_signal(result: &Result<ExitStatus, io::Error>) -> Option<i32> {
    match result {
        Ok(status) => status.signal(),
        Err(_) => None,
    }
}

const SYSFS_PLATFORM_DEVICES_PATH: &str = "/sys/devices/platform/";
const VFIO_PLATFORM_DRIVER_PATH: &str = "/sys/bus/platform/drivers/vfio-platform";

fn vfio_argument_for_platform_device(device: &VfioDevice) -> Result<String, Error> {
    // Check platform device exists
    let path = device.sysfs_path.canonicalize()?;
    if !path.starts_with(SYSFS_PLATFORM_DEVICES_PATH) {
        bail!("{path:?} is not a platform device");
    }

    // Check platform device is bound to VFIO driver
    let dev_driver_path = path.join("driver").canonicalize()?;
    if dev_driver_path != Path::new(VFIO_PLATFORM_DRIVER_PATH) {
        bail!("{path:?} is not bound to VFIO-platform driver");
    }

    if let Some(p) = path.to_str() {
        Ok(format!("--vfio={p},iommu=pkvm-iommu,dt-symbol={0}", device.dtbo_label))
    } else {
        bail!("invalid path {path:?}");
    }
}

fn append_platform_devices(
    command: &mut Command,
    preserved_fds: &mut Vec<RawFd>,
    config: &CrosvmConfig,
) -> Result<(), Error> {
    if config.vfio_devices.is_empty() {
        return Ok(());
    }

    let Some(dtbo) = &config.dtbo else {
        bail!("VFIO devices assigned but no DTBO available");
    };
    command.arg(format!("--device-tree-overlay={},filter", add_preserved_fd(preserved_fds, dtbo)));

    for device in &config.vfio_devices {
        command.arg(vfio_argument_for_platform_device(device)?);
    }
    Ok(())
}

/// Starts an instance of `crosvm` to manage a new VM.
fn run_vm(
    config: CrosvmConfig,
    crosvm_control_socket_path: &Path,
    failure_pipe_write: File,
) -> Result<SharedChild, Error> {
    validate_config(&config)?;

    let mut command = Command::new(CROSVM_PATH);
    // TODO(qwandor): Remove --disable-sandbox.
    command
        .arg("--extended-status")
        // Configure the logger for the crosvm process to silence logs from the disk crate which
        // don't provide much information to us (but do spamming us).
        .arg("--log-level")
        .arg("info,disk=warn")
        .arg("run")
        .arg("--disable-sandbox")
        .arg("--cid")
        .arg(config.cid.to_string());

    if system_properties::read_bool("hypervisor.memory_reclaim.supported", false)? {
        command.arg("--balloon-page-reporting");
    } else {
        command.arg("--no-balloon");
    }

    if config.protected {
        match system_properties::read(SYSPROP_CUSTOM_PVMFW_PATH)? {
            Some(pvmfw_path) if !pvmfw_path.is_empty() => {
                command.arg("--protected-vm-with-firmware").arg(pvmfw_path)
            }
            _ => command.arg("--protected-vm"),
        };

        // 3 virtio-console devices + vsock = 4.
        let virtio_pci_device_count = 4 + config.disks.len();
        // crosvm virtio queue has 256 entries, so 2 MiB per device (2 pages per entry) should be
        // enough.
        let swiotlb_size_mib = 2 * virtio_pci_device_count as u32;
        command.arg("--swiotlb").arg(swiotlb_size_mib.to_string());

        // Workaround to keep crash_dump from trying to read protected guest memory.
        // Context in b/238324526.
        command.arg("--unmap-guest-memory-on-fork");

        if config.ramdump.is_some() {
            // Protected VM needs to reserve memory for ramdump here. Note that we reserve more
            // memory for the restricted dma pool.
            let ramdump_reserve = RAMDUMP_RESERVED_MIB + swiotlb_size_mib;
            command.arg("--params").arg(format!("crashkernel={ramdump_reserve}M"));
        }
    } else if config.ramdump.is_some() {
        command.arg("--params").arg(format!("crashkernel={RAMDUMP_RESERVED_MIB}M"));
    }
    if config.debug_config.debug_level == DebugLevel::NONE
        && config.debug_config.should_prepare_console_output()
    {
        // bootconfig.normal will be used, but we need log.
        command.arg("--params").arg("printk.devkmsg=on");
        command.arg("--params").arg("console=hvc0");
    }

    if let Some(memory_mib) = config.memory_mib {
        command.arg("--mem").arg(memory_mib.to_string());
    }

    if let Some(cpus) = config.cpus {
        command.arg("--cpus").arg(cpus.to_string());
    }

    if config.host_cpu_topology {
        // TODO(b/266664564): replace with --host-cpu-topology once available
        if let Some(cpus) = get_num_cpus() {
            command.arg("--cpus").arg(cpus.to_string());
        } else {
            bail!("Could not determine the number of CPUs in the system");
        }
    }

    if !config.task_profiles.is_empty() {
        command.arg("--task-profiles").arg(config.task_profiles.join(","));
    }

    if let Some(gdb_port) = config.gdb_port {
        command.arg("--gdb").arg(gdb_port.to_string());
    }

    // Keep track of what file descriptors should be mapped to the crosvm process.
    let mut preserved_fds = config.indirect_files.iter().map(|file| file.as_raw_fd()).collect();

    // Setup the serial devices.
    // 1. uart device: used as the output device by bootloaders and as early console by linux
    // 2. uart device: used to report the reason for the VM failing.
    // 3. virtio-console device: used as the console device where kmsg is redirected to
    // 4. virtio-console device: used as the ramdump output
    // 5. virtio-console device: used as the logcat output
    //
    // When [console|log]_fd is not specified, the devices are attached to sink, which means what's
    // written there is discarded.
    let console_out_arg = format_serial_out_arg(&mut preserved_fds, &config.console_out_fd);
    let console_in_arg = config
        .console_in_fd
        .as_ref()
        .map(|fd| format!(",input={}", add_preserved_fd(&mut preserved_fds, fd)))
        .unwrap_or_default();
    let log_arg = format_serial_out_arg(&mut preserved_fds, &config.log_fd);
    let failure_serial_path = add_preserved_fd(&mut preserved_fds, &failure_pipe_write);
    let ramdump_arg = format_serial_out_arg(&mut preserved_fds, &config.ramdump);

    // Warning: Adding more serial devices requires you to shift the PCI device ID of the boot
    // disks in bootconfig.x86_64. This is because x86 crosvm puts serial devices and the block
    // devices in the same PCI bus and serial devices comes before the block devices. Arm crosvm
    // doesn't have the issue.
    // /dev/ttyS0
    command.arg(format!("--serial={},hardware=serial,num=1", &console_out_arg));
    // /dev/ttyS1
    command.arg(format!("--serial=type=file,path={},hardware=serial,num=2", &failure_serial_path));
    // /dev/hvc0
    command.arg(format!(
        "--serial={}{},hardware=virtio-console,num=1",
        &console_out_arg, &console_in_arg
    ));
    // /dev/hvc1
    command.arg(format!("--serial={},hardware=virtio-console,num=2", &ramdump_arg));
    // /dev/hvc2
    command.arg(format!("--serial={},hardware=virtio-console,num=3", &log_arg));

    if let Some(bootloader) = &config.bootloader {
        command.arg("--bios").arg(add_preserved_fd(&mut preserved_fds, bootloader));
    }

    if let Some(initrd) = &config.initrd {
        command.arg("--initrd").arg(add_preserved_fd(&mut preserved_fds, initrd));
    }

    if let Some(params) = &config.params {
        command.arg("--params").arg(params);
    }

    for disk in &config.disks {
        command
            .arg(if disk.writable { "--rwdisk" } else { "--disk" })
            .arg(add_preserved_fd(&mut preserved_fds, &disk.image));
    }

    if let Some(kernel) = &config.kernel {
        command.arg(add_preserved_fd(&mut preserved_fds, kernel));
    }

    let control_server_socket = UnixSeqpacketListener::bind(crosvm_control_socket_path)
        .context("failed to create control server")?;
    command
        .arg("--socket")
        .arg(add_preserved_fd(&mut preserved_fds, &control_server_socket.as_raw_descriptor()));

    // TODO(b/285855436): Pass dtbo_vendor after --device-tree-overlay crosvm option is supported.

    append_platform_devices(&mut command, &mut preserved_fds, &config)?;

    debug!("Preserving FDs {:?}", preserved_fds);
    command.preserved_fds(preserved_fds);

    print_crosvm_args(&command);

    let result = SharedChild::spawn(&mut command)?;
    debug!("Spawned crosvm({}).", result.id());
    Ok(result)
}

/// Ensure that the configuration has a valid combination of fields set, or return an error if not.
fn validate_config(config: &CrosvmConfig) -> Result<(), Error> {
    if config.bootloader.is_none() && config.kernel.is_none() {
        bail!("VM must have either a bootloader or a kernel image.");
    }
    if config.bootloader.is_some() && (config.kernel.is_some() || config.initrd.is_some()) {
        bail!("Can't have both bootloader and kernel/initrd image.");
    }
    let version = Version::parse(CROSVM_PLATFORM_VERSION).unwrap();
    if !config.platform_version.matches(&version) {
        bail!(
            "Incompatible platform version. The config is compatible with platform version(s) \
              {}, but the actual platform version is {}",
            config.platform_version,
            version
        );
    }

    Ok(())
}

/// Print arguments of the crosvm command. In doing so, /proc/self/fd/XX is annotated with the
/// actual file path if the FD is backed by a regular file. If not, the /proc path is printed
/// unmodified.
fn print_crosvm_args(command: &Command) {
    let re = Regex::new(r"/proc/self/fd/[\d]+").unwrap();
    info!(
        "Running crosvm with args: {:?}",
        command
            .get_args()
            .map(|s| s.to_string_lossy())
            .map(|s| {
                re.replace_all(&s, |caps: &Captures| {
                    let path = &caps[0];
                    if let Ok(realpath) = std::fs::canonicalize(path) {
                        format!("{} ({})", path, realpath.to_string_lossy())
                    } else {
                        path.to_owned()
                    }
                })
                .into_owned()
            })
            .collect::<Vec<_>>()
    );
}

/// Adds the file descriptor for `file` to `preserved_fds`, and returns a string of the form
/// "/proc/self/fd/N" where N is the file descriptor.
fn add_preserved_fd(preserved_fds: &mut Vec<RawFd>, file: &dyn AsRawFd) -> String {
    let fd = file.as_raw_fd();
    preserved_fds.push(fd);
    format!("/proc/self/fd/{}", fd)
}

/// Adds the file descriptor for `file` (if any) to `preserved_fds`, and returns the appropriate
/// string for a crosvm `--serial` flag. If `file` is none, creates a dummy sink device.
fn format_serial_out_arg(preserved_fds: &mut Vec<RawFd>, file: &Option<File>) -> String {
    if let Some(file) = file {
        format!("type=file,path={}", add_preserved_fd(preserved_fds, file))
    } else {
        "type=sink".to_string()
    }
}

/// Creates a new pipe with the `O_CLOEXEC` flag set, and returns the read side and write side.
fn create_pipe() -> Result<(File, File), Error> {
    let (raw_read, raw_write) = pipe2(OFlag::O_CLOEXEC)?;
    // SAFETY: We are the sole owner of this FD as we just created it, and it is valid and open.
    let read_fd = unsafe { File::from_raw_fd(raw_read) };
    // SAFETY: We are the sole owner of this FD as we just created it, and it is valid and open.
    let write_fd = unsafe { File::from_raw_fd(raw_write) };
    Ok((read_fd, write_fd))
}
