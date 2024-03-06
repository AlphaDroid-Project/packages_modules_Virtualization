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

//! High-level FDT functions.

use crate::bootargs::BootArgsIterator;
use crate::device_assignment::DeviceAssignmentInfo;
use crate::device_assignment::VmDtbo;
use crate::helpers::GUEST_PAGE_SIZE;
use crate::Box;
use crate::RebootReason;
use alloc::ffi::CString;
use alloc::vec::Vec;
use core::cmp::max;
use core::cmp::min;
use core::ffi::CStr;
use core::fmt;
use core::mem::size_of;
use core::ops::Range;
use cstr::cstr;
use fdtpci::PciMemoryFlags;
use fdtpci::PciRangeType;
use libfdt::AddressRange;
use libfdt::CellIterator;
use libfdt::Fdt;
use libfdt::FdtError;
use libfdt::FdtNode;
use libfdt::FdtNodeMut;
use log::debug;
use log::error;
use log::info;
use log::warn;
use tinyvec::ArrayVec;
use vmbase::fdt::SwiotlbInfo;
use vmbase::layout::{crosvm::MEM_START, MAX_VIRT_ADDR};
use vmbase::memory::SIZE_4KB;
use vmbase::util::flatten;
use vmbase::util::RangeExt as _;

/// An enumeration of errors that can occur during the FDT validation.
#[derive(Clone, Debug)]
pub enum FdtValidationError {
    /// Invalid CPU count.
    InvalidCpuCount(usize),
}

impl fmt::Display for FdtValidationError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::InvalidCpuCount(num_cpus) => write!(f, "Invalid CPU count: {num_cpus}"),
        }
    }
}

/// Extract from /config the address range containing the pre-loaded kernel. Absence of /config is
/// not an error.
fn read_kernel_range_from(fdt: &Fdt) -> libfdt::Result<Option<Range<usize>>> {
    let addr = cstr!("kernel-address");
    let size = cstr!("kernel-size");

    if let Some(config) = fdt.node(cstr!("/config"))? {
        if let (Some(addr), Some(size)) = (config.getprop_u32(addr)?, config.getprop_u32(size)?) {
            let addr = addr as usize;
            let size = size as usize;

            return Ok(Some(addr..(addr + size)));
        }
    }

    Ok(None)
}

/// Extract from /chosen the address range containing the pre-loaded ramdisk. Absence is not an
/// error as there can be initrd-less VM.
fn read_initrd_range_from(fdt: &Fdt) -> libfdt::Result<Option<Range<usize>>> {
    let start = cstr!("linux,initrd-start");
    let end = cstr!("linux,initrd-end");

    if let Some(chosen) = fdt.chosen()? {
        if let (Some(start), Some(end)) = (chosen.getprop_u32(start)?, chosen.getprop_u32(end)?) {
            return Ok(Some((start as usize)..(end as usize)));
        }
    }

    Ok(None)
}

fn patch_initrd_range(fdt: &mut Fdt, initrd_range: &Range<usize>) -> libfdt::Result<()> {
    let start = u32::try_from(initrd_range.start).unwrap();
    let end = u32::try_from(initrd_range.end).unwrap();

    let mut node = fdt.chosen_mut()?.ok_or(FdtError::NotFound)?;
    node.setprop(cstr!("linux,initrd-start"), &start.to_be_bytes())?;
    node.setprop(cstr!("linux,initrd-end"), &end.to_be_bytes())?;
    Ok(())
}

fn read_bootargs_from(fdt: &Fdt) -> libfdt::Result<Option<CString>> {
    if let Some(chosen) = fdt.chosen()? {
        if let Some(bootargs) = chosen.getprop_str(cstr!("bootargs"))? {
            // We need to copy the string to heap because the original fdt will be invalidated
            // by the templated DT
            let copy = CString::new(bootargs.to_bytes()).map_err(|_| FdtError::BadValue)?;
            return Ok(Some(copy));
        }
    }
    Ok(None)
}

fn patch_bootargs(fdt: &mut Fdt, bootargs: &CStr) -> libfdt::Result<()> {
    let mut node = fdt.chosen_mut()?.ok_or(FdtError::NotFound)?;
    // This function is called before the verification is done. So, we just copy the bootargs to
    // the new FDT unmodified. This will be filtered again in the modify_for_next_stage function
    // if the VM is not debuggable.
    node.setprop(cstr!("bootargs"), bootargs.to_bytes_with_nul())
}

/// Reads and validates the memory range in the DT.
///
/// Only one memory range is expected with the crosvm setup for now.
fn read_and_validate_memory_range(fdt: &Fdt) -> Result<Range<usize>, RebootReason> {
    let mut memory = fdt.memory().map_err(|e| {
        error!("Failed to read memory range from DT: {e}");
        RebootReason::InvalidFdt
    })?;
    let range = memory.next().ok_or_else(|| {
        error!("The /memory node in the DT contains no range.");
        RebootReason::InvalidFdt
    })?;
    if memory.next().is_some() {
        warn!(
            "The /memory node in the DT contains more than one memory range, \
             while only one is expected."
        );
    }
    let base = range.start;
    if base != MEM_START {
        error!("Memory base address {:#x} is not {:#x}", base, MEM_START);
        return Err(RebootReason::InvalidFdt);
    }

    let size = range.len();
    if size % GUEST_PAGE_SIZE != 0 {
        error!("Memory size {:#x} is not a multiple of page size {:#x}", size, GUEST_PAGE_SIZE);
        return Err(RebootReason::InvalidFdt);
    }

    if size == 0 {
        error!("Memory size is 0");
        return Err(RebootReason::InvalidFdt);
    }
    Ok(range)
}

fn patch_memory_range(fdt: &mut Fdt, memory_range: &Range<usize>) -> libfdt::Result<()> {
    let size = memory_range.len() as u64;
    fdt.node_mut(cstr!("/memory"))?
        .ok_or(FdtError::NotFound)?
        .setprop_inplace(cstr!("reg"), flatten(&[MEM_START.to_be_bytes(), size.to_be_bytes()]))
}

/// Read the number of CPUs from DT
fn read_num_cpus_from(fdt: &Fdt) -> libfdt::Result<usize> {
    Ok(fdt.compatible_nodes(cstr!("arm,arm-v8"))?.count())
}

/// Validate number of CPUs
fn validate_num_cpus(num_cpus: usize) -> Result<(), FdtValidationError> {
    if num_cpus == 0 || DeviceTreeInfo::gic_patched_size(num_cpus).is_none() {
        Err(FdtValidationError::InvalidCpuCount(num_cpus))
    } else {
        Ok(())
    }
}

/// Patch DT by keeping `num_cpus` number of arm,arm-v8 compatible nodes, and pruning the rest.
fn patch_num_cpus(fdt: &mut Fdt, num_cpus: usize) -> libfdt::Result<()> {
    let cpu = cstr!("arm,arm-v8");
    let mut next = fdt.root_mut()?.next_compatible(cpu)?;
    for _ in 0..num_cpus {
        next = if let Some(current) = next {
            current.next_compatible(cpu)?
        } else {
            return Err(FdtError::NoSpace);
        };
    }
    while let Some(current) = next {
        next = current.delete_and_next_compatible(cpu)?;
    }
    Ok(())
}

fn read_vendor_public_key_from(fdt: &Fdt) -> libfdt::Result<Option<Vec<u8>>> {
    if let Some(avf_node) = fdt.node(cstr!("/avf"))? {
        if let Some(vendor_public_key) = avf_node.getprop(cstr!("vendor_public_key"))? {
            return Ok(Some(vendor_public_key.to_vec()));
        }
    }
    Ok(None)
}

fn patch_vendor_public_key(fdt: &mut Fdt, vendor_public_key: &[u8]) -> libfdt::Result<()> {
    let mut root_node = fdt.root_mut()?;
    let mut avf_node = root_node.add_subnode(cstr!("/avf"))?;
    avf_node.setprop(cstr!("vendor_public_key"), vendor_public_key)?;
    Ok(())
}

#[derive(Debug)]
struct PciInfo {
    ranges: [PciAddrRange; 2],
    irq_masks: ArrayVec<[PciIrqMask; PciInfo::MAX_IRQS]>,
    irq_maps: ArrayVec<[PciIrqMap; PciInfo::MAX_IRQS]>,
}

impl PciInfo {
    const IRQ_MASK_CELLS: usize = 4;
    const IRQ_MAP_CELLS: usize = 10;
    const MAX_IRQS: usize = 10;
}

type PciAddrRange = AddressRange<(u32, u64), u64, u64>;
type PciIrqMask = [u32; PciInfo::IRQ_MASK_CELLS];
type PciIrqMap = [u32; PciInfo::IRQ_MAP_CELLS];

/// Iterator that takes N cells as a chunk
struct CellChunkIterator<'a, const N: usize> {
    cells: CellIterator<'a>,
}

impl<'a, const N: usize> CellChunkIterator<'a, N> {
    fn new(cells: CellIterator<'a>) -> Self {
        Self { cells }
    }
}

impl<'a, const N: usize> Iterator for CellChunkIterator<'a, N> {
    type Item = [u32; N];
    fn next(&mut self) -> Option<Self::Item> {
        let mut ret: Self::Item = [0; N];
        for i in ret.iter_mut() {
            *i = self.cells.next()?;
        }
        Some(ret)
    }
}

/// Read pci host controller ranges, irq maps, and irq map masks from DT
fn read_pci_info_from(fdt: &Fdt) -> libfdt::Result<PciInfo> {
    let node =
        fdt.compatible_nodes(cstr!("pci-host-cam-generic"))?.next().ok_or(FdtError::NotFound)?;

    let mut ranges = node.ranges::<(u32, u64), u64, u64>()?.ok_or(FdtError::NotFound)?;
    let range0 = ranges.next().ok_or(FdtError::NotFound)?;
    let range1 = ranges.next().ok_or(FdtError::NotFound)?;

    let irq_masks = node.getprop_cells(cstr!("interrupt-map-mask"))?.ok_or(FdtError::NotFound)?;
    let mut chunks = CellChunkIterator::<{ PciInfo::IRQ_MASK_CELLS }>::new(irq_masks);
    let irq_masks = (&mut chunks).take(PciInfo::MAX_IRQS).collect();

    if chunks.next().is_some() {
        warn!("Input DT has more than {} PCI entries!", PciInfo::MAX_IRQS);
        return Err(FdtError::NoSpace);
    }

    let irq_maps = node.getprop_cells(cstr!("interrupt-map"))?.ok_or(FdtError::NotFound)?;
    let mut chunks = CellChunkIterator::<{ PciInfo::IRQ_MAP_CELLS }>::new(irq_maps);
    let irq_maps = (&mut chunks).take(PciInfo::MAX_IRQS).collect();

    if chunks.next().is_some() {
        warn!("Input DT has more than {} PCI entries!", PciInfo::MAX_IRQS);
        return Err(FdtError::NoSpace);
    }

    Ok(PciInfo { ranges: [range0, range1], irq_masks, irq_maps })
}

fn validate_pci_info(pci_info: &PciInfo, memory_range: &Range<usize>) -> Result<(), RebootReason> {
    for range in pci_info.ranges.iter() {
        validate_pci_addr_range(range, memory_range)?;
    }
    for irq_mask in pci_info.irq_masks.iter() {
        validate_pci_irq_mask(irq_mask)?;
    }
    for (idx, irq_map) in pci_info.irq_maps.iter().enumerate() {
        validate_pci_irq_map(irq_map, idx)?;
    }
    Ok(())
}

fn validate_pci_addr_range(
    range: &PciAddrRange,
    memory_range: &Range<usize>,
) -> Result<(), RebootReason> {
    let mem_flags = PciMemoryFlags(range.addr.0);
    let range_type = mem_flags.range_type();
    let prefetchable = mem_flags.prefetchable();
    let bus_addr = range.addr.1;
    let cpu_addr = range.parent_addr;
    let size = range.size;

    if range_type != PciRangeType::Memory64 {
        error!("Invalid range type {:?} for bus address {:#x} in PCI node", range_type, bus_addr);
        return Err(RebootReason::InvalidFdt);
    }
    if prefetchable {
        error!("PCI bus address {:#x} in PCI node is prefetchable", bus_addr);
        return Err(RebootReason::InvalidFdt);
    }
    // Enforce ID bus-to-cpu mappings, as used by crosvm.
    if bus_addr != cpu_addr {
        error!("PCI bus address: {:#x} is different from CPU address: {:#x}", bus_addr, cpu_addr);
        return Err(RebootReason::InvalidFdt);
    }

    let Some(bus_end) = bus_addr.checked_add(size) else {
        error!("PCI address range size {:#x} overflows", size);
        return Err(RebootReason::InvalidFdt);
    };
    if bus_end > MAX_VIRT_ADDR.try_into().unwrap() {
        error!("PCI address end {:#x} is outside of translatable range", bus_end);
        return Err(RebootReason::InvalidFdt);
    }

    let memory_start = memory_range.start.try_into().unwrap();
    let memory_end = memory_range.end.try_into().unwrap();

    if max(bus_addr, memory_start) < min(bus_end, memory_end) {
        error!(
            "PCI address range {:#x}-{:#x} overlaps with main memory range {:#x}-{:#x}",
            bus_addr, bus_end, memory_start, memory_end
        );
        return Err(RebootReason::InvalidFdt);
    }

    Ok(())
}

fn validate_pci_irq_mask(irq_mask: &PciIrqMask) -> Result<(), RebootReason> {
    const IRQ_MASK_ADDR_HI: u32 = 0xf800;
    const IRQ_MASK_ADDR_ME: u32 = 0x0;
    const IRQ_MASK_ADDR_LO: u32 = 0x0;
    const IRQ_MASK_ANY_IRQ: u32 = 0x7;
    const EXPECTED: PciIrqMask =
        [IRQ_MASK_ADDR_HI, IRQ_MASK_ADDR_ME, IRQ_MASK_ADDR_LO, IRQ_MASK_ANY_IRQ];
    if *irq_mask != EXPECTED {
        error!("Invalid PCI irq mask {:#?}", irq_mask);
        return Err(RebootReason::InvalidFdt);
    }
    Ok(())
}

fn validate_pci_irq_map(irq_map: &PciIrqMap, idx: usize) -> Result<(), RebootReason> {
    const PCI_DEVICE_IDX: usize = 11;
    const PCI_IRQ_ADDR_ME: u32 = 0;
    const PCI_IRQ_ADDR_LO: u32 = 0;
    const PCI_IRQ_INTC: u32 = 1;
    const AARCH64_IRQ_BASE: u32 = 4; // from external/crosvm/aarch64/src/lib.rs
    const GIC_SPI: u32 = 0;
    const IRQ_TYPE_LEVEL_HIGH: u32 = 4;

    let pci_addr = (irq_map[0], irq_map[1], irq_map[2]);
    let pci_irq_number = irq_map[3];
    let _controller_phandle = irq_map[4]; // skipped.
    let gic_addr = (irq_map[5], irq_map[6]); // address-cells is <2> for GIC
                                             // interrupt-cells is <3> for GIC
    let gic_peripheral_interrupt_type = irq_map[7];
    let gic_irq_number = irq_map[8];
    let gic_irq_type = irq_map[9];

    let phys_hi: u32 = (0x1 << PCI_DEVICE_IDX) * (idx + 1) as u32;
    let expected_pci_addr = (phys_hi, PCI_IRQ_ADDR_ME, PCI_IRQ_ADDR_LO);

    if pci_addr != expected_pci_addr {
        error!("PCI device address {:#x} {:#x} {:#x} in interrupt-map is different from expected address \
               {:#x} {:#x} {:#x}",
               pci_addr.0, pci_addr.1, pci_addr.2, expected_pci_addr.0, expected_pci_addr.1, expected_pci_addr.2);
        return Err(RebootReason::InvalidFdt);
    }

    if pci_irq_number != PCI_IRQ_INTC {
        error!(
            "PCI INT# {:#x} in interrupt-map is different from expected value {:#x}",
            pci_irq_number, PCI_IRQ_INTC
        );
        return Err(RebootReason::InvalidFdt);
    }

    if gic_addr != (0, 0) {
        error!(
            "GIC address {:#x} {:#x} in interrupt-map is different from expected address \
               {:#x} {:#x}",
            gic_addr.0, gic_addr.1, 0, 0
        );
        return Err(RebootReason::InvalidFdt);
    }

    if gic_peripheral_interrupt_type != GIC_SPI {
        error!("GIC peripheral interrupt type {:#x} in interrupt-map is different from expected value \
               {:#x}", gic_peripheral_interrupt_type, GIC_SPI);
        return Err(RebootReason::InvalidFdt);
    }

    let irq_nr: u32 = AARCH64_IRQ_BASE + (idx as u32);
    if gic_irq_number != irq_nr {
        error!(
            "GIC irq number {:#x} in interrupt-map is unexpected. Expected {:#x}",
            gic_irq_number, irq_nr
        );
        return Err(RebootReason::InvalidFdt);
    }

    if gic_irq_type != IRQ_TYPE_LEVEL_HIGH {
        error!(
            "IRQ type in {:#x} is invalid. Must be LEVEL_HIGH {:#x}",
            gic_irq_type, IRQ_TYPE_LEVEL_HIGH
        );
        return Err(RebootReason::InvalidFdt);
    }
    Ok(())
}

fn patch_pci_info(fdt: &mut Fdt, pci_info: &PciInfo) -> libfdt::Result<()> {
    let mut node = fdt
        .root_mut()?
        .next_compatible(cstr!("pci-host-cam-generic"))?
        .ok_or(FdtError::NotFound)?;

    let irq_masks_size = pci_info.irq_masks.len() * size_of::<PciIrqMask>();
    node.trimprop(cstr!("interrupt-map-mask"), irq_masks_size)?;

    let irq_maps_size = pci_info.irq_maps.len() * size_of::<PciIrqMap>();
    node.trimprop(cstr!("interrupt-map"), irq_maps_size)?;

    node.setprop_inplace(
        cstr!("ranges"),
        flatten(&[pci_info.ranges[0].to_cells(), pci_info.ranges[1].to_cells()]),
    )
}

#[derive(Default, Debug)]
struct SerialInfo {
    addrs: ArrayVec<[u64; Self::MAX_SERIALS]>,
}

impl SerialInfo {
    const MAX_SERIALS: usize = 4;
}

fn read_serial_info_from(fdt: &Fdt) -> libfdt::Result<SerialInfo> {
    let mut addrs: ArrayVec<[u64; SerialInfo::MAX_SERIALS]> = Default::default();
    for node in fdt.compatible_nodes(cstr!("ns16550a"))?.take(SerialInfo::MAX_SERIALS) {
        let reg = node.first_reg()?;
        addrs.push(reg.addr);
    }
    Ok(SerialInfo { addrs })
}

/// Patch the DT by deleting the ns16550a compatible nodes whose address are unknown
fn patch_serial_info(fdt: &mut Fdt, serial_info: &SerialInfo) -> libfdt::Result<()> {
    let name = cstr!("ns16550a");
    let mut next = fdt.root_mut()?.next_compatible(name);
    while let Some(current) = next? {
        let reg = FdtNode::from_mut(&current)
            .reg()?
            .ok_or(FdtError::NotFound)?
            .next()
            .ok_or(FdtError::NotFound)?;
        next = if !serial_info.addrs.contains(&reg.addr) {
            current.delete_and_next_compatible(name)
        } else {
            current.next_compatible(name)
        }
    }
    Ok(())
}

fn validate_swiotlb_info(
    swiotlb_info: &SwiotlbInfo,
    memory: &Range<usize>,
) -> Result<(), RebootReason> {
    let size = swiotlb_info.size;
    let align = swiotlb_info.align;

    if size == 0 || (size % GUEST_PAGE_SIZE) != 0 {
        error!("Invalid swiotlb size {:#x}", size);
        return Err(RebootReason::InvalidFdt);
    }

    if let Some(align) = align.filter(|&a| a % GUEST_PAGE_SIZE != 0) {
        error!("Invalid swiotlb alignment {:#x}", align);
        return Err(RebootReason::InvalidFdt);
    }

    if let Some(addr) = swiotlb_info.addr {
        if addr.checked_add(size).is_none() {
            error!("Invalid swiotlb range: addr:{addr:#x} size:{size:#x}");
            return Err(RebootReason::InvalidFdt);
        }
    }
    if let Some(range) = swiotlb_info.fixed_range() {
        if !range.is_within(memory) {
            error!("swiotlb range {range:#x?} not part of memory range {memory:#x?}");
            return Err(RebootReason::InvalidFdt);
        }
    }

    Ok(())
}

fn patch_swiotlb_info(fdt: &mut Fdt, swiotlb_info: &SwiotlbInfo) -> libfdt::Result<()> {
    let mut node =
        fdt.root_mut()?.next_compatible(cstr!("restricted-dma-pool"))?.ok_or(FdtError::NotFound)?;

    if let Some(range) = swiotlb_info.fixed_range() {
        node.setprop_addrrange_inplace(
            cstr!("reg"),
            range.start.try_into().unwrap(),
            range.len().try_into().unwrap(),
        )?;
        node.nop_property(cstr!("size"))?;
        node.nop_property(cstr!("alignment"))?;
    } else {
        node.nop_property(cstr!("reg"))?;
        node.setprop_inplace(cstr!("size"), &swiotlb_info.size.to_be_bytes())?;
        node.setprop_inplace(cstr!("alignment"), &swiotlb_info.align.unwrap().to_be_bytes())?;
    }

    Ok(())
}

fn patch_gic(fdt: &mut Fdt, num_cpus: usize) -> libfdt::Result<()> {
    let node = fdt.compatible_nodes(cstr!("arm,gic-v3"))?.next().ok_or(FdtError::NotFound)?;
    let mut ranges = node.reg()?.ok_or(FdtError::NotFound)?;
    let range0 = ranges.next().ok_or(FdtError::NotFound)?;
    let mut range1 = ranges.next().ok_or(FdtError::NotFound)?;

    let addr = range0.addr;
    // `validate_num_cpus()` checked that this wouldn't panic
    let size = u64::try_from(DeviceTreeInfo::gic_patched_size(num_cpus).unwrap()).unwrap();

    // range1 is just below range0
    range1.addr = addr - size;
    range1.size = Some(size);

    let range0 = range0.to_cells();
    let range1 = range1.to_cells();
    let value = [
        range0.0,          // addr
        range0.1.unwrap(), //size
        range1.0,          // addr
        range1.1.unwrap(), //size
    ];

    let mut node =
        fdt.root_mut()?.next_compatible(cstr!("arm,gic-v3"))?.ok_or(FdtError::NotFound)?;
    node.setprop_inplace(cstr!("reg"), flatten(&value))
}

fn patch_timer(fdt: &mut Fdt, num_cpus: usize) -> libfdt::Result<()> {
    const NUM_INTERRUPTS: usize = 4;
    const CELLS_PER_INTERRUPT: usize = 3;
    let node = fdt.compatible_nodes(cstr!("arm,armv8-timer"))?.next().ok_or(FdtError::NotFound)?;
    let interrupts = node.getprop_cells(cstr!("interrupts"))?.ok_or(FdtError::NotFound)?;
    let mut value: ArrayVec<[u32; NUM_INTERRUPTS * CELLS_PER_INTERRUPT]> =
        interrupts.take(NUM_INTERRUPTS * CELLS_PER_INTERRUPT).collect();

    let num_cpus: u32 = num_cpus.try_into().unwrap();
    let cpu_mask: u32 = (((0x1 << num_cpus) - 1) & 0xff) << 8;
    for v in value.iter_mut().skip(2).step_by(CELLS_PER_INTERRUPT) {
        *v |= cpu_mask;
    }
    for v in value.iter_mut() {
        *v = v.to_be();
    }

    // SAFETY: array size is the same
    let value = unsafe {
        core::mem::transmute::<
            [u32; NUM_INTERRUPTS * CELLS_PER_INTERRUPT],
            [u8; NUM_INTERRUPTS * CELLS_PER_INTERRUPT * size_of::<u32>()],
        >(value.into_inner())
    };

    let mut node =
        fdt.root_mut()?.next_compatible(cstr!("arm,armv8-timer"))?.ok_or(FdtError::NotFound)?;
    node.setprop_inplace(cstr!("interrupts"), value.as_slice())
}

#[derive(Debug)]
pub struct DeviceTreeInfo {
    pub kernel_range: Option<Range<usize>>,
    pub initrd_range: Option<Range<usize>>,
    pub memory_range: Range<usize>,
    bootargs: Option<CString>,
    num_cpus: usize,
    pci_info: PciInfo,
    serial_info: SerialInfo,
    pub swiotlb_info: SwiotlbInfo,
    device_assignment: Option<DeviceAssignmentInfo>,
    vendor_public_key: Option<Vec<u8>>,
}

impl DeviceTreeInfo {
    fn gic_patched_size(num_cpus: usize) -> Option<usize> {
        const GIC_REDIST_SIZE_PER_CPU: usize = 32 * SIZE_4KB;

        GIC_REDIST_SIZE_PER_CPU.checked_mul(num_cpus)
    }
}

pub fn sanitize_device_tree(
    fdt: &mut [u8],
    vm_dtbo: Option<&mut [u8]>,
) -> Result<DeviceTreeInfo, RebootReason> {
    let fdt = Fdt::from_mut_slice(fdt).map_err(|e| {
        error!("Failed to load FDT: {e}");
        RebootReason::InvalidFdt
    })?;

    let vm_dtbo = match vm_dtbo {
        Some(vm_dtbo) => Some(VmDtbo::from_mut_slice(vm_dtbo).map_err(|e| {
            error!("Failed to load VM DTBO: {e}");
            RebootReason::InvalidFdt
        })?),
        None => None,
    };

    let info = parse_device_tree(fdt, vm_dtbo.as_deref())?;

    fdt.copy_from_slice(pvmfw_fdt_template::RAW).map_err(|e| {
        error!("Failed to instantiate FDT from the template DT: {e}");
        RebootReason::InvalidFdt
    })?;

    fdt.unpack().map_err(|e| {
        error!("Failed to unpack DT for patching: {e}");
        RebootReason::InvalidFdt
    })?;

    if let Some(device_assignment_info) = &info.device_assignment {
        let vm_dtbo = vm_dtbo.unwrap();
        device_assignment_info.filter(vm_dtbo).map_err(|e| {
            error!("Failed to filter VM DTBO: {e}");
            RebootReason::InvalidFdt
        })?;
        // SAFETY: Damaged VM DTBO isn't used in this API after this unsafe block.
        // VM DTBO can't be reused in any way as Fdt nor VmDtbo outside of this API because
        // it can only be instantiated after validation.
        unsafe {
            fdt.apply_overlay(vm_dtbo.as_mut()).map_err(|e| {
                error!("Failed to apply filtered VM DTBO: {e}");
                RebootReason::InvalidFdt
            })?;
        }
    }

    patch_device_tree(fdt, &info)?;

    fdt.pack().map_err(|e| {
        error!("Failed to unpack DT after patching: {e}");
        RebootReason::InvalidFdt
    })?;

    Ok(info)
}

fn parse_device_tree(fdt: &Fdt, vm_dtbo: Option<&VmDtbo>) -> Result<DeviceTreeInfo, RebootReason> {
    let kernel_range = read_kernel_range_from(fdt).map_err(|e| {
        error!("Failed to read kernel range from DT: {e}");
        RebootReason::InvalidFdt
    })?;

    let initrd_range = read_initrd_range_from(fdt).map_err(|e| {
        error!("Failed to read initrd range from DT: {e}");
        RebootReason::InvalidFdt
    })?;

    let memory_range = read_and_validate_memory_range(fdt)?;

    let bootargs = read_bootargs_from(fdt).map_err(|e| {
        error!("Failed to read bootargs from DT: {e}");
        RebootReason::InvalidFdt
    })?;

    let num_cpus = read_num_cpus_from(fdt).map_err(|e| {
        error!("Failed to read num cpus from DT: {e}");
        RebootReason::InvalidFdt
    })?;
    validate_num_cpus(num_cpus).map_err(|e| {
        error!("Failed to validate num cpus from DT: {e}");
        RebootReason::InvalidFdt
    })?;

    let pci_info = read_pci_info_from(fdt).map_err(|e| {
        error!("Failed to read pci info from DT: {e}");
        RebootReason::InvalidFdt
    })?;
    validate_pci_info(&pci_info, &memory_range)?;

    let serial_info = read_serial_info_from(fdt).map_err(|e| {
        error!("Failed to read serial info from DT: {e}");
        RebootReason::InvalidFdt
    })?;

    let swiotlb_info = SwiotlbInfo::new_from_fdt(fdt).map_err(|e| {
        error!("Failed to read swiotlb info from DT: {e}");
        RebootReason::InvalidFdt
    })?;
    validate_swiotlb_info(&swiotlb_info, &memory_range)?;

    let device_assignment = match vm_dtbo {
        Some(vm_dtbo) => DeviceAssignmentInfo::parse(fdt, vm_dtbo).map_err(|e| {
            error!("Failed to parse device assignment from DT and VM DTBO: {e}");
            RebootReason::InvalidFdt
        })?,
        None => None,
    };

    // TODO(b/285854379) : A temporary solution lives. This is for enabling
    // microdroid vendor partition for non-protected VM as well. When passing
    // DT path containing vendor_public_key via fstab, init stage will check
    // if vendor_public_key exists in the init stage, regardless the protection.
    // Adding this temporary solution will prevent fatal in init stage for
    // protected VM. However, this data is not trustable without validating
    // with vendor public key value comes from ABL.
    let vendor_public_key = read_vendor_public_key_from(fdt).map_err(|e| {
        error!("Failed to read vendor_public_key from DT: {e}");
        RebootReason::InvalidFdt
    })?;

    Ok(DeviceTreeInfo {
        kernel_range,
        initrd_range,
        memory_range,
        bootargs,
        num_cpus,
        pci_info,
        serial_info,
        swiotlb_info,
        device_assignment,
        vendor_public_key,
    })
}

fn patch_device_tree(fdt: &mut Fdt, info: &DeviceTreeInfo) -> Result<(), RebootReason> {
    if let Some(initrd_range) = &info.initrd_range {
        patch_initrd_range(fdt, initrd_range).map_err(|e| {
            error!("Failed to patch initrd range to DT: {e}");
            RebootReason::InvalidFdt
        })?;
    }
    patch_memory_range(fdt, &info.memory_range).map_err(|e| {
        error!("Failed to patch memory range to DT: {e}");
        RebootReason::InvalidFdt
    })?;
    if let Some(bootargs) = &info.bootargs {
        patch_bootargs(fdt, bootargs.as_c_str()).map_err(|e| {
            error!("Failed to patch bootargs to DT: {e}");
            RebootReason::InvalidFdt
        })?;
    }
    patch_num_cpus(fdt, info.num_cpus).map_err(|e| {
        error!("Failed to patch cpus to DT: {e}");
        RebootReason::InvalidFdt
    })?;
    patch_pci_info(fdt, &info.pci_info).map_err(|e| {
        error!("Failed to patch pci info to DT: {e}");
        RebootReason::InvalidFdt
    })?;
    patch_serial_info(fdt, &info.serial_info).map_err(|e| {
        error!("Failed to patch serial info to DT: {e}");
        RebootReason::InvalidFdt
    })?;
    patch_swiotlb_info(fdt, &info.swiotlb_info).map_err(|e| {
        error!("Failed to patch swiotlb info to DT: {e}");
        RebootReason::InvalidFdt
    })?;
    patch_gic(fdt, info.num_cpus).map_err(|e| {
        error!("Failed to patch gic info to DT: {e}");
        RebootReason::InvalidFdt
    })?;
    patch_timer(fdt, info.num_cpus).map_err(|e| {
        error!("Failed to patch timer info to DT: {e}");
        RebootReason::InvalidFdt
    })?;
    if let Some(device_assignment) = &info.device_assignment {
        // Note: We patch values after VM DTBO is overlaid because patch may require more space
        // then VM DTBO's underlying slice is allocated.
        device_assignment.patch(fdt).map_err(|e| {
            error!("Failed to patch device assignment info to DT: {e}");
            RebootReason::InvalidFdt
        })?;
    }
    if let Some(vendor_public_key) = &info.vendor_public_key {
        patch_vendor_public_key(fdt, vendor_public_key).map_err(|e| {
            error!("Failed to patch vendor_public_key to DT: {e}");
            RebootReason::InvalidFdt
        })?;
    }

    Ok(())
}

/// Modifies the input DT according to the fields of the configuration.
pub fn modify_for_next_stage(
    fdt: &mut Fdt,
    bcc: &[u8],
    new_instance: bool,
    strict_boot: bool,
    debug_policy: Option<&mut [u8]>,
    debuggable: bool,
    kaslr_seed: u64,
) -> libfdt::Result<()> {
    if let Some(debug_policy) = debug_policy {
        let backup = Vec::from(fdt.as_slice());
        fdt.unpack()?;
        let backup_fdt = Fdt::from_slice(backup.as_slice()).unwrap();
        if apply_debug_policy(fdt, backup_fdt, debug_policy)? {
            info!("Debug policy applied.");
        } else {
            // apply_debug_policy restored fdt to backup_fdt so unpack it again.
            fdt.unpack()?;
        }
    } else {
        info!("No debug policy found.");
        fdt.unpack()?;
    }

    patch_dice_node(fdt, bcc.as_ptr() as usize, bcc.len())?;

    if let Some(mut chosen) = fdt.chosen_mut()? {
        empty_or_delete_prop(&mut chosen, cstr!("avf,strict-boot"), strict_boot)?;
        empty_or_delete_prop(&mut chosen, cstr!("avf,new-instance"), new_instance)?;
        chosen.setprop_inplace(cstr!("kaslr-seed"), &kaslr_seed.to_be_bytes())?;
    };
    if !debuggable {
        if let Some(bootargs) = read_bootargs_from(fdt)? {
            filter_out_dangerous_bootargs(fdt, &bootargs)?;
        }
    }

    fdt.pack()?;

    Ok(())
}

/// Patch the "google,open-dice"-compatible reserved-memory node to point to the bcc range
fn patch_dice_node(fdt: &mut Fdt, addr: usize, size: usize) -> libfdt::Result<()> {
    // We reject DTs with missing reserved-memory node as validation should have checked that the
    // "swiotlb" subnode (compatible = "restricted-dma-pool") was present.
    let node = fdt.node_mut(cstr!("/reserved-memory"))?.ok_or(libfdt::FdtError::NotFound)?;

    let mut node = node.next_compatible(cstr!("google,open-dice"))?.ok_or(FdtError::NotFound)?;

    let addr: u64 = addr.try_into().unwrap();
    let size: u64 = size.try_into().unwrap();
    node.setprop_inplace(cstr!("reg"), flatten(&[addr.to_be_bytes(), size.to_be_bytes()]))
}

fn empty_or_delete_prop(
    fdt_node: &mut FdtNodeMut,
    prop_name: &CStr,
    keep_prop: bool,
) -> libfdt::Result<()> {
    if keep_prop {
        fdt_node.setprop_empty(prop_name)
    } else {
        fdt_node
            .delprop(prop_name)
            .or_else(|e| if e == FdtError::NotFound { Ok(()) } else { Err(e) })
    }
}

/// Apply the debug policy overlay to the guest DT.
///
/// Returns Ok(true) on success, Ok(false) on recovered failure and Err(_) on corruption of the DT.
fn apply_debug_policy(
    fdt: &mut Fdt,
    backup_fdt: &Fdt,
    debug_policy: &[u8],
) -> libfdt::Result<bool> {
    let mut debug_policy = Vec::from(debug_policy);
    let overlay = match Fdt::from_mut_slice(debug_policy.as_mut_slice()) {
        Ok(overlay) => overlay,
        Err(e) => {
            warn!("Corrupted debug policy found: {e}. Not applying.");
            return Ok(false);
        }
    };

    // SAFETY: on failure, the corrupted DT is restored using the backup.
    if let Err(e) = unsafe { fdt.apply_overlay(overlay) } {
        warn!("Failed to apply debug policy: {e}. Recovering...");
        fdt.copy_from_slice(backup_fdt.as_slice())?;
        // A successful restoration is considered success because an invalid debug policy
        // shouldn't DOS the pvmfw
        Ok(false)
    } else {
        Ok(true)
    }
}

fn has_common_debug_policy(fdt: &Fdt, debug_feature_name: &CStr) -> libfdt::Result<bool> {
    if let Some(node) = fdt.node(cstr!("/avf/guest/common"))? {
        if let Some(value) = node.getprop_u32(debug_feature_name)? {
            return Ok(value == 1);
        }
    }
    Ok(false) // if the policy doesn't exist or not 1, don't enable the debug feature
}

fn filter_out_dangerous_bootargs(fdt: &mut Fdt, bootargs: &CStr) -> libfdt::Result<()> {
    let has_crashkernel = has_common_debug_policy(fdt, cstr!("ramdump"))?;
    let has_console = has_common_debug_policy(fdt, cstr!("log"))?;

    let accepted: &[(&str, Box<dyn Fn(Option<&str>) -> bool>)] = &[
        ("panic", Box::new(|v| if let Some(v) = v { v == "=-1" } else { false })),
        ("crashkernel", Box::new(|_| has_crashkernel)),
        ("console", Box::new(|_| has_console)),
    ];

    // parse and filter out unwanted
    let mut filtered = Vec::new();
    for arg in BootArgsIterator::new(bootargs).map_err(|e| {
        info!("Invalid bootarg: {e}");
        FdtError::BadValue
    })? {
        match accepted.iter().find(|&t| t.0 == arg.name()) {
            Some((_, pred)) if pred(arg.value()) => filtered.push(arg),
            _ => debug!("Rejected bootarg {}", arg.as_ref()),
        }
    }

    // flatten into a new C-string
    let mut new_bootargs = Vec::new();
    for (i, arg) in filtered.iter().enumerate() {
        if i != 0 {
            new_bootargs.push(b' '); // separator
        }
        new_bootargs.extend_from_slice(arg.as_ref().as_bytes());
    }
    new_bootargs.push(b'\0');

    let mut node = fdt.chosen_mut()?.ok_or(FdtError::NotFound)?;
    node.setprop(cstr!("bootargs"), new_bootargs.as_slice())
}
