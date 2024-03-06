// Copyright 2022, The Android Open Source Project
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

//! Android Virtualization Manager

mod aidl;
mod atom;
mod composite;
mod crosvm;
mod debug_config;
mod payload;
mod selinux;

use crate::aidl::{GLOBAL_SERVICE, VirtualizationService};
use android_system_virtualizationservice::aidl::android::system::virtualizationservice::IVirtualizationService::BnVirtualizationService;
use anyhow::{bail, Context, Result};
use binder::{BinderFeatures, ProcessState};
use lazy_static::lazy_static;
use log::{info, Level};
use rpcbinder::{FileDescriptorTransportMode, RpcServer};
use std::os::unix::io::{FromRawFd, OwnedFd, RawFd};
use clap::Parser;
use nix::fcntl::{fcntl, F_GETFD, F_SETFD, FdFlag};
use nix::unistd::{Pid, Uid};
use std::os::unix::raw::{pid_t, uid_t};

const LOG_TAG: &str = "virtmgr";

lazy_static! {
    static ref PID_PARENT: Pid = Pid::parent();
    static ref UID_CURRENT: Uid = Uid::current();
}

fn get_calling_pid() -> pid_t {
    // The caller is the parent of this process.
    PID_PARENT.as_raw()
}

fn get_calling_uid() -> uid_t {
    // The caller and this process share the same UID.
    UID_CURRENT.as_raw()
}

#[derive(Parser)]
struct Args {
    /// File descriptor inherited from the caller to run RpcBinder server on.
    /// This should be one end of a socketpair() compatible with RpcBinder's
    /// UDS bootstrap transport.
    #[clap(long)]
    rpc_server_fd: RawFd,
    /// File descriptor inherited from the caller to signal RpcBinder server
    /// readiness. This should be one end of pipe() and the caller should be
    /// waiting for HUP on the other end.
    #[clap(long)]
    ready_fd: RawFd,
}

fn take_fd_ownership(raw_fd: RawFd, owned_fds: &mut Vec<RawFd>) -> Result<OwnedFd, anyhow::Error> {
    // Basic check that the integer value does correspond to a file descriptor.
    fcntl(raw_fd, F_GETFD).with_context(|| format!("Invalid file descriptor {raw_fd}"))?;

    // The file descriptor had CLOEXEC disabled to be inherited from the parent.
    // Re-enable it to make sure it is not accidentally inherited further.
    fcntl(raw_fd, F_SETFD(FdFlag::FD_CLOEXEC))
        .with_context(|| format!("Could not set CLOEXEC on file descriptor {raw_fd}"))?;

    // Creating OwnedFd for stdio FDs is not safe.
    if [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO].contains(&raw_fd) {
        bail!("File descriptor {raw_fd} is standard I/O descriptor");
    }

    // Reject RawFds that already have a corresponding OwnedFd.
    if owned_fds.contains(&raw_fd) {
        bail!("File descriptor {raw_fd} already owned");
    }
    owned_fds.push(raw_fd);

    // SAFETY: Initializing OwnedFd for a RawFd provided in cmdline arguments.
    // We checked that the integer value corresponds to a valid FD and that this
    // is the first argument to claim its ownership.
    Ok(unsafe { OwnedFd::from_raw_fd(raw_fd) })
}

fn check_vm_support() -> Result<()> {
    if hypervisor_props::is_any_vm_supported()? {
        Ok(())
    } else {
        // This should never happen, it indicates a misconfigured device where the virt APEX
        // is present but VMs are not supported. If it does happen, fail fast to avoid wasting
        // resources trying.
        bail!("Device doesn't support protected or non-protected VMs")
    }
}

fn main() {
    android_logger::init_once(
        android_logger::Config::default()
            .with_tag(LOG_TAG)
            .with_min_level(Level::Info)
            .with_log_id(android_logger::LogId::System),
    );

    check_vm_support().unwrap();

    let args = Args::parse();

    let mut owned_fds = vec![];
    let rpc_server_fd = take_fd_ownership(args.rpc_server_fd, &mut owned_fds)
        .expect("Failed to take ownership of rpc_server_fd");
    let ready_fd = take_fd_ownership(args.ready_fd, &mut owned_fds)
        .expect("Failed to take ownership of ready_fd");

    // Start thread pool for kernel Binder connection to VirtualizationServiceInternal.
    ProcessState::start_thread_pool();

    GLOBAL_SERVICE.removeMemlockRlimit().expect("Failed to remove memlock rlimit");

    let service = VirtualizationService::init();
    let service =
        BnVirtualizationService::new_binder(service, BinderFeatures::default()).as_binder();

    let server = RpcServer::new_unix_domain_bootstrap(service, rpc_server_fd)
        .expect("Failed to start RpcServer");
    server.set_supported_file_descriptor_transport_modes(&[FileDescriptorTransportMode::Unix]);

    info!("Started VirtualizationService RpcServer. Ready to accept connections");

    // Signal readiness to the caller by closing our end of the pipe.
    drop(ready_fd);

    server.join();
    info!("Shutting down VirtualizationService RpcServer");
}
