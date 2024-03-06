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

//! Android VM control tool.

mod create_idsig;
mod create_partition;
mod run;

use android_system_virtualizationservice::aidl::android::system::virtualizationservice::{
    CpuTopology::CpuTopology, IVirtualizationService::IVirtualizationService,
    PartitionType::PartitionType, VirtualMachineAppConfig::DebugLevel::DebugLevel,
};
use anyhow::{Context, Error};
use binder::{ProcessState, Strong};
use clap::{Args, Parser};
use create_idsig::command_create_idsig;
use create_partition::command_create_partition;
use glob::glob;
use run::{command_run, command_run_app, command_run_microdroid};
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};

#[derive(Debug)]
struct Idsigs(Vec<PathBuf>);

#[derive(Args)]
/// Collection of flags that are at VM level and therefore applicable to all subcommands
pub struct CommonConfig {
    /// Name of VM
    #[arg(long)]
    name: Option<String>,

    /// Run VM with vCPU topology matching that of the host. If unspecified, defaults to 1 vCPU.
    #[arg(long, default_value = "one_cpu", value_parser = parse_cpu_topology)]
    cpu_topology: CpuTopology,

    /// Comma separated list of task profile names to apply to the VM
    #[arg(long)]
    task_profiles: Vec<String>,

    /// Memory size (in MiB) of the VM. If unspecified, defaults to the value of `memory_mib`
    /// in the VM config file.
    #[arg(short, long)]
    mem: Option<u32>,

    /// Run VM in protected mode.
    #[arg(short, long)]
    protected: bool,
}

#[derive(Args)]
/// Collection of flags for debugging
pub struct DebugConfig {
    /// Debug level of the VM. Supported values: "full" (default), and "none".
    #[arg(long, default_value = "full", value_parser = parse_debug_level)]
    debug: DebugLevel,

    /// Path to file for VM console output.
    #[arg(long)]
    console: Option<PathBuf>,

    /// Path to file for VM console input.
    #[arg(long)]
    console_in: Option<PathBuf>,

    /// Path to file for VM log output.
    #[arg(long)]
    log: Option<PathBuf>,

    /// Port at which crosvm will start a gdb server to debug guest kernel.
    /// Note: this is only supported on Android kernels android14-5.15 and higher.
    #[arg(long)]
    gdb: Option<NonZeroU16>,
}

#[derive(Args)]
/// Collection of flags that are Microdroid specific
pub struct MicrodroidConfig {
    /// Path to the file backing the storage.
    /// Created if the option is used but the path does not exist in the device.
    #[arg(long)]
    storage: Option<PathBuf>,

    /// Size of the storage. Used only if --storage is supplied but path does not exist
    /// Default size is 10*1024*1024
    #[arg(long)]
    storage_size: Option<u64>,

    /// Path to disk image containing vendor-specific modules.
    #[cfg(vendor_modules)]
    #[arg(long)]
    vendor: Option<PathBuf>,

    /// SysFS nodes of devices to assign to VM
    #[cfg(device_assignment)]
    #[arg(long)]
    devices: Vec<PathBuf>,

    /// Version of GKI to use. If set, use instead of microdroid kernel
    #[cfg(vendor_modules)]
    #[arg(long)]
    gki: Option<String>,
}

impl MicrodroidConfig {
    #[cfg(vendor_modules)]
    fn vendor(&self) -> &Option<PathBuf> {
        &self.vendor
    }

    #[cfg(not(vendor_modules))]
    fn vendor(&self) -> Option<PathBuf> {
        None
    }

    #[cfg(vendor_modules)]
    fn gki(&self) -> Option<&str> {
        self.gki.as_deref()
    }

    #[cfg(not(vendor_modules))]
    fn gki(&self) -> Option<&str> {
        None
    }

    #[cfg(device_assignment)]
    fn devices(&self) -> &Vec<PathBuf> {
        &self.devices
    }

    #[cfg(not(device_assignment))]
    fn devices(&self) -> Vec<PathBuf> {
        Vec::new()
    }
}

#[derive(Args)]
/// Flags for the run_app subcommand
pub struct RunAppConfig {
    #[command(flatten)]
    common: CommonConfig,

    #[command(flatten)]
    debug: DebugConfig,

    #[command(flatten)]
    microdroid: MicrodroidConfig,

    /// Path to VM Payload APK
    apk: PathBuf,

    /// Path to idsig of the APK
    idsig: PathBuf,

    /// Path to the instance image. Created if not exists.
    instance: PathBuf,

    /// Path to VM config JSON within APK (e.g. assets/vm_config.json)
    #[arg(long)]
    config_path: Option<String>,

    /// Name of VM payload binary within APK (e.g. MicrodroidTestNativeLib.so)
    #[arg(long)]
    #[arg(alias = "payload_path")]
    payload_binary_name: Option<String>,

    /// Paths to extra idsig files.
    #[arg(long = "extra-idsig")]
    extra_idsigs: Vec<PathBuf>,
}

#[derive(Args)]
/// Flags for the run_microdroid subcommand
pub struct RunMicrodroidConfig {
    #[command(flatten)]
    common: CommonConfig,

    #[command(flatten)]
    debug: DebugConfig,

    #[command(flatten)]
    microdroid: MicrodroidConfig,

    /// Path to the directory where VM-related files (e.g. instance.img, apk.idsig, etc.) will
    /// be stored. If not specified a random directory under /data/local/tmp/microdroid will be
    /// created and used.
    #[arg(long)]
    work_dir: Option<PathBuf>,
}

#[derive(Args)]
/// Flags for the run subcommand
pub struct RunCustomVmConfig {
    #[command(flatten)]
    common: CommonConfig,

    #[command(flatten)]
    debug: DebugConfig,

    /// Path to VM config JSON
    config: PathBuf,
}

#[derive(Parser)]
enum Opt {
    /// Run a virtual machine with a config in APK
    RunApp {
        #[command(flatten)]
        config: RunAppConfig,
    },
    /// Run a virtual machine with Microdroid inside
    RunMicrodroid {
        #[command(flatten)]
        config: RunMicrodroidConfig,
    },
    /// Run a virtual machine
    Run {
        #[command(flatten)]
        config: RunCustomVmConfig,
    },
    /// List running virtual machines
    List,
    /// Print information about virtual machine support
    Info,
    /// Create a new empty partition to be used as a writable partition for a VM
    CreatePartition {
        /// Path at which to create the image file
        path: PathBuf,

        /// The desired size of the partition, in bytes.
        size: u64,

        /// Type of the partition
        #[arg(short = 't', long = "type", default_value = "raw",
               value_parser = parse_partition_type)]
        partition_type: PartitionType,
    },
    /// Creates or update the idsig file by digesting the input APK file.
    CreateIdsig {
        /// Path to VM Payload APK
        apk: PathBuf,

        /// Path to idsig of the APK
        path: PathBuf,
    },
}

fn parse_debug_level(s: &str) -> Result<DebugLevel, String> {
    match s {
        "none" => Ok(DebugLevel::NONE),
        "full" => Ok(DebugLevel::FULL),
        _ => Err(format!("Invalid debug level {}", s)),
    }
}

fn parse_partition_type(s: &str) -> Result<PartitionType, String> {
    match s {
        "raw" => Ok(PartitionType::RAW),
        "instance" => Ok(PartitionType::ANDROID_VM_INSTANCE),
        _ => Err(format!("Invalid partition type {}", s)),
    }
}

fn parse_cpu_topology(s: &str) -> Result<CpuTopology, String> {
    match s {
        "one_cpu" => Ok(CpuTopology::ONE_CPU),
        "match_host" => Ok(CpuTopology::MATCH_HOST),
        _ => Err(format!("Invalid cpu topology {}", s)),
    }
}

fn get_service() -> Result<Strong<dyn IVirtualizationService>, Error> {
    let virtmgr =
        vmclient::VirtualizationService::new().context("Failed to spawn VirtualizationService")?;
    virtmgr.connect().context("Failed to connect to VirtualizationService")
}

fn main() -> Result<(), Error> {
    env_logger::init();
    let opt = Opt::parse();

    // We need to start the thread pool for Binder to work properly, especially link_to_death.
    ProcessState::start_thread_pool();

    match opt {
        Opt::RunApp { config } => command_run_app(config),
        Opt::RunMicrodroid { config } => command_run_microdroid(config),
        Opt::Run { config } => command_run(config),
        Opt::List => command_list(get_service()?.as_ref()),
        Opt::Info => command_info(),
        Opt::CreatePartition { path, size, partition_type } => {
            command_create_partition(get_service()?.as_ref(), &path, size, partition_type)
        }
        Opt::CreateIdsig { apk, path } => {
            command_create_idsig(get_service()?.as_ref(), &apk, &path)
        }
    }
}

/// List the VMs currently running.
fn command_list(service: &dyn IVirtualizationService) -> Result<(), Error> {
    let vms = service.debugListVms().context("Failed to get list of VMs")?;
    println!("Running VMs: {:#?}", vms);
    Ok(())
}

fn extract_gki_version(gki_config: &Path) -> Option<&str> {
    let name = gki_config.file_name()?;
    let name_str = name.to_str()?;
    name_str.strip_prefix("microdroid_gki-")?.strip_suffix(".json")
}

/// Print information about supported VM types.
fn command_info() -> Result<(), Error> {
    let non_protected_vm_supported = hypervisor_props::is_vm_supported()?;
    let protected_vm_supported = hypervisor_props::is_protected_vm_supported()?;
    match (non_protected_vm_supported, protected_vm_supported) {
        (false, false) => println!("VMs are not supported."),
        (false, true) => println!("Only protected VMs are supported."),
        (true, false) => println!("Only non-protected VMs are supported."),
        (true, true) => println!("Both protected and non-protected VMs are supported."),
    }

    if let Some(version) = hypervisor_props::version()? {
        println!("Hypervisor version: {}", version);
    } else {
        println!("Hypervisor version not set.");
    }

    if Path::new("/dev/kvm").exists() {
        println!("/dev/kvm exists.");
    } else {
        println!("/dev/kvm does not exist.");
    }

    if Path::new("/dev/vfio/vfio").exists() {
        println!("/dev/vfio/vfio exists.");
    } else {
        println!("/dev/vfio/vfio does not exist.");
    }

    if Path::new("/sys/bus/platform/drivers/vfio-platform").exists() {
        println!("VFIO-platform is supported.");
    } else {
        println!("VFIO-platform is not supported.");
    }

    let devices = get_service()?.getAssignableDevices()?;
    let devices = devices.into_iter().map(|x| x.node).collect::<Vec<_>>();
    println!("Assignable devices: {}", serde_json::to_string(&devices)?);

    let gki_configs =
        glob("/apex/com.android.virt/etc/microdroid_gki-*.json")?.collect::<Result<Vec<_>, _>>()?;
    let gki_versions =
        gki_configs.iter().filter_map(|x| extract_gki_version(x)).collect::<Vec<_>>();
    println!("Available gki versions: {}", serde_json::to_string(&gki_versions)?);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn verify_app() {
        // Check that the command parsing has been configured in a valid way.
        Opt::command().debug_assert();
    }
}
