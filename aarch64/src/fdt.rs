// Copyright 2018 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::PathBuf;

use arch::CpuSet;
use arch::SERIAL_ADDR;
use cros_fdt::Error;
use cros_fdt::Fdt;
use cros_fdt::Result;
// This is a Battery related constant
use devices::bat::GOLDFISHBAT_MMIO_LEN;
use devices::pl030::PL030_AMBA_ID;
use devices::PciAddress;
use devices::PciInterruptPin;
use hypervisor::PsciVersion;
use hypervisor::PSCI_0_2;
use hypervisor::PSCI_1_0;
use rand::rngs::OsRng;
use rand::RngCore;
use vm_memory::GuestAddress;
use vm_memory::GuestMemory;

// These are GIC address-space location constants.
use crate::AARCH64_GIC_CPUI_BASE;
use crate::AARCH64_GIC_CPUI_SIZE;
use crate::AARCH64_GIC_DIST_BASE;
use crate::AARCH64_GIC_DIST_SIZE;
use crate::AARCH64_GIC_REDIST_SIZE;
use crate::AARCH64_PMU_IRQ;
use crate::AARCH64_PROTECTED_VM_FW_START;
// These are RTC related constants
use crate::AARCH64_RTC_ADDR;
use crate::AARCH64_RTC_IRQ;
use crate::AARCH64_RTC_SIZE;
// These are serial device related constants.
use crate::AARCH64_SERIAL_1_3_IRQ;
use crate::AARCH64_SERIAL_2_4_IRQ;
use crate::AARCH64_SERIAL_SIZE;
use crate::AARCH64_SERIAL_SPEED;
use crate::AARCH64_VIRTFREQ_BASE;
use crate::AARCH64_VIRTFREQ_SIZE;

// This is an arbitrary number to specify the node for the GIC.
// If we had a more complex interrupt architecture, then we'd need an enum for
// these.
const PHANDLE_GIC: u32 = 1;
const PHANDLE_RESTRICTED_DMA_POOL: u32 = 2;

// CPUs are assigned phandles starting with this number.
const PHANDLE_CPU0: u32 = 0x100;

const PHANDLE_OPP_DOMAIN_BASE: u32 = 0x1000;

// These are specified by the Linux GIC bindings
const GIC_FDT_IRQ_NUM_CELLS: u32 = 3;
const GIC_FDT_IRQ_TYPE_SPI: u32 = 0;
const GIC_FDT_IRQ_TYPE_PPI: u32 = 1;
const GIC_FDT_IRQ_PPI_CPU_SHIFT: u32 = 8;
const GIC_FDT_IRQ_PPI_CPU_MASK: u32 = 0xff << GIC_FDT_IRQ_PPI_CPU_SHIFT;
const IRQ_TYPE_EDGE_RISING: u32 = 0x00000001;
const IRQ_TYPE_LEVEL_HIGH: u32 = 0x00000004;
const IRQ_TYPE_LEVEL_LOW: u32 = 0x00000008;

fn create_memory_node(fdt: &mut Fdt, guest_mem: &GuestMemory) -> Result<()> {
    let mut mem_reg_prop = Vec::new();
    let mut previous_memory_region_end = None;
    let mut regions = guest_mem.guest_memory_regions();
    regions.sort();
    for region in regions {
        if region.0.offset() == AARCH64_PROTECTED_VM_FW_START {
            continue;
        }
        // Merge with the previous region if possible.
        if let Some(previous_end) = previous_memory_region_end {
            if region.0 == previous_end {
                *mem_reg_prop.last_mut().unwrap() += region.1 as u64;
                previous_memory_region_end =
                    Some(previous_end.checked_add(region.1 as u64).unwrap());
                continue;
            }
            assert!(region.0 > previous_end, "Memory regions overlap");
        }

        mem_reg_prop.push(region.0.offset());
        mem_reg_prop.push(region.1 as u64);
        previous_memory_region_end = Some(region.0.checked_add(region.1 as u64).unwrap());
    }

    let memory_node = fdt.begin_node("memory")?;
    fdt.set_prop("device_type", "memory")?;
    fdt.set_prop("reg", mem_reg_prop)?;
    fdt.end_node(memory_node)?;

    Ok(())
}

fn create_resv_memory_node(
    fdt: &mut Fdt,
    resv_addr_and_size: (Option<GuestAddress>, u64),
) -> Result<u32> {
    let (resv_addr, resv_size) = resv_addr_and_size;

    let resv_memory_node = fdt.begin_node("reserved-memory")?;
    fdt.set_prop("#address-cells", 0x2u32)?;
    fdt.set_prop("#size-cells", 0x2u32)?;
    fdt.set_prop("ranges", ())?;

    let restricted_dma_pool = if let Some(resv_addr) = resv_addr {
        let node = fdt.begin_node(&format!("restricted_dma_reserved@{:x}", resv_addr.0))?;
        fdt.set_prop("reg", &[resv_addr.0, resv_size])?;
        node
    } else {
        let node = fdt.begin_node("restricted_dma_reserved")?;
        fdt.set_prop("size", resv_size)?;
        node
    };
    fdt.set_prop("phandle", PHANDLE_RESTRICTED_DMA_POOL)?;
    fdt.set_prop("compatible", "restricted-dma-pool")?;
    fdt.set_prop("alignment", base::pagesize() as u64)?;
    fdt.end_node(restricted_dma_pool)?;

    fdt.end_node(resv_memory_node)?;
    Ok(PHANDLE_RESTRICTED_DMA_POOL)
}

fn create_cpu_nodes(
    fdt: &mut Fdt,
    num_cpus: u32,
    cpu_clusters: Vec<CpuSet>,
    cpu_capacity: BTreeMap<usize, u32>,
    dynamic_power_coefficient: BTreeMap<usize, u32>,
    cpu_frequencies: BTreeMap<usize, Vec<u32>>,
) -> Result<()> {
    let cpus_node = fdt.begin_node("cpus")?;
    fdt.set_prop("#address-cells", 0x1u32)?;
    fdt.set_prop("#size-cells", 0x0u32)?;

    for cpu_id in 0..num_cpus {
        let cpu_name = format!("cpu@{:x}", cpu_id);
        let cpu_node = fdt.begin_node(&cpu_name)?;
        fdt.set_prop("device_type", "cpu")?;
        fdt.set_prop("compatible", "arm,arm-v8")?;
        if num_cpus > 1 {
            fdt.set_prop("enable-method", "psci")?;
        }
        fdt.set_prop("reg", cpu_id)?;
        fdt.set_prop("phandle", PHANDLE_CPU0 + cpu_id)?;

        if let Some(pwr_coefficient) = dynamic_power_coefficient.get(&(cpu_id as usize)) {
            fdt.set_prop("dynamic-power-coefficient", *pwr_coefficient)?;
        }
        if let Some(capacity) = cpu_capacity.get(&(cpu_id as usize)) {
            fdt.set_prop("capacity-dmips-mhz", *capacity)?;
        }

        if !cpu_frequencies.is_empty() {
            fdt.set_prop("operating-points-v2", PHANDLE_OPP_DOMAIN_BASE + cpu_id)?;
        }
        fdt.end_node(cpu_node)?;
    }

    if !cpu_clusters.is_empty() {
        let cpu_map_node = fdt.begin_node("cpu-map")?;
        for (cluster_idx, cpus) in cpu_clusters.iter().enumerate() {
            let cluster_node = fdt.begin_node(&format!("cluster{}", cluster_idx))?;
            for (core_idx, cpu_id) in cpus.iter().enumerate() {
                let core_node = fdt.begin_node(&format!("core{}", core_idx))?;
                fdt.set_prop("cpu", PHANDLE_CPU0 + *cpu_id as u32)?;
                fdt.end_node(core_node)?;
            }
            fdt.end_node(cluster_node)?;
        }
        fdt.end_node(cpu_map_node)?;
    }

    fdt.end_node(cpus_node)?;

    if !cpu_frequencies.is_empty() {
        for cpu_id in 0..num_cpus {
            if let Some(frequencies) = cpu_frequencies.get(&(cpu_id as usize)) {
                let opp_table_node = fdt.begin_node(&format!("opp_table{}", cpu_id))?;
                fdt.set_prop("phandle", PHANDLE_OPP_DOMAIN_BASE + cpu_id)?;
                fdt.set_prop("compatible", "operating-points-v2")?;
                for freq in frequencies.iter() {
                    let opp_hz = (*freq) as u64 * 1000;
                    let opp_node = fdt.begin_node(&format!("opp{}", opp_hz))?;
                    fdt.set_prop("opp-hz", opp_hz)?;
                    fdt.end_node(opp_node)?;
                }
                fdt.end_node(opp_table_node)?;
            }
        }
    }

    Ok(())
}

fn create_gic_node(fdt: &mut Fdt, is_gicv3: bool, num_cpus: u64) -> Result<()> {
    let mut gic_reg_prop = [AARCH64_GIC_DIST_BASE, AARCH64_GIC_DIST_SIZE, 0, 0];

    let intc_node = fdt.begin_node("intc")?;
    if is_gicv3 {
        fdt.set_prop("compatible", "arm,gic-v3")?;
        gic_reg_prop[2] = AARCH64_GIC_DIST_BASE - (AARCH64_GIC_REDIST_SIZE * num_cpus);
        gic_reg_prop[3] = AARCH64_GIC_REDIST_SIZE * num_cpus;
    } else {
        fdt.set_prop("compatible", "arm,cortex-a15-gic")?;
        gic_reg_prop[2] = AARCH64_GIC_CPUI_BASE;
        gic_reg_prop[3] = AARCH64_GIC_CPUI_SIZE;
    }
    fdt.set_prop("#interrupt-cells", GIC_FDT_IRQ_NUM_CELLS)?;
    fdt.set_prop("interrupt-controller", ())?;
    fdt.set_prop("reg", &gic_reg_prop)?;
    fdt.set_prop("phandle", PHANDLE_GIC)?;
    fdt.set_prop("#address-cells", 2u32)?;
    fdt.set_prop("#size-cells", 2u32)?;
    fdt.end_node(intc_node)?;

    Ok(())
}

fn create_timer_node(fdt: &mut Fdt, num_cpus: u32) -> Result<()> {
    // These are fixed interrupt numbers for the timer device.
    let irqs = [13, 14, 11, 10];
    let compatible = "arm,armv8-timer";
    let cpu_mask: u32 =
        (((1 << num_cpus) - 1) << GIC_FDT_IRQ_PPI_CPU_SHIFT) & GIC_FDT_IRQ_PPI_CPU_MASK;

    let mut timer_reg_cells = Vec::new();
    for &irq in &irqs {
        timer_reg_cells.push(GIC_FDT_IRQ_TYPE_PPI);
        timer_reg_cells.push(irq);
        timer_reg_cells.push(cpu_mask | IRQ_TYPE_LEVEL_LOW);
    }

    let timer_node = fdt.begin_node("timer")?;
    fdt.set_prop("compatible", compatible)?;
    fdt.set_prop("interrupts", timer_reg_cells)?;
    fdt.set_prop("always-on", ())?;
    fdt.end_node(timer_node)?;

    Ok(())
}

fn create_virt_cpufreq_node(fdt: &mut Fdt, num_cpus: u64) -> Result<()> {
    let compatible = "virtual,kvm-cpufreq";
    let vcf_node = fdt.begin_node("cpufreq")?;
    let reg = [AARCH64_VIRTFREQ_BASE, AARCH64_VIRTFREQ_SIZE * num_cpus];

    fdt.set_prop("compatible", compatible)?;
    fdt.set_prop("reg", &reg)?;
    fdt.end_node(vcf_node)?;
    Ok(())
}

fn create_pmu_node(fdt: &mut Fdt, num_cpus: u32) -> Result<()> {
    let compatible = "arm,armv8-pmuv3";
    let cpu_mask: u32 =
        (((1 << num_cpus) - 1) << GIC_FDT_IRQ_PPI_CPU_SHIFT) & GIC_FDT_IRQ_PPI_CPU_MASK;
    let irq = [
        GIC_FDT_IRQ_TYPE_PPI,
        AARCH64_PMU_IRQ,
        cpu_mask | IRQ_TYPE_LEVEL_HIGH,
    ];

    let pmu_node = fdt.begin_node("pmu")?;
    fdt.set_prop("compatible", compatible)?;
    fdt.set_prop("interrupts", &irq)?;
    fdt.end_node(pmu_node)?;
    Ok(())
}

fn create_serial_node(fdt: &mut Fdt, addr: u64, irq: u32) -> Result<()> {
    let serial_reg_prop = [addr, AARCH64_SERIAL_SIZE];
    let irq = [GIC_FDT_IRQ_TYPE_SPI, irq, IRQ_TYPE_EDGE_RISING];

    let serial_node = fdt.begin_node(&format!("U6_16550A@{:x}", addr))?;
    fdt.set_prop("compatible", "ns16550a")?;
    fdt.set_prop("reg", &serial_reg_prop)?;
    fdt.set_prop("clock-frequency", AARCH64_SERIAL_SPEED)?;
    fdt.set_prop("interrupts", &irq)?;
    fdt.end_node(serial_node)?;

    Ok(())
}

fn create_serial_nodes(fdt: &mut Fdt) -> Result<()> {
    // Note that SERIAL_ADDR contains the I/O port addresses conventionally used
    // for serial ports on x86. This uses the same addresses (but on the MMIO bus)
    // to simplify the shared serial code.
    create_serial_node(fdt, SERIAL_ADDR[0], AARCH64_SERIAL_1_3_IRQ)?;
    create_serial_node(fdt, SERIAL_ADDR[1], AARCH64_SERIAL_2_4_IRQ)?;
    create_serial_node(fdt, SERIAL_ADDR[2], AARCH64_SERIAL_1_3_IRQ)?;
    create_serial_node(fdt, SERIAL_ADDR[3], AARCH64_SERIAL_2_4_IRQ)?;

    Ok(())
}

fn psci_compatible(version: &PsciVersion) -> Vec<&str> {
    // The PSCI kernel driver only supports compatible strings for the following
    // backward-compatible versions.
    let supported = [(PSCI_1_0, "arm,psci-1.0"), (PSCI_0_2, "arm,psci-0.2")];

    let mut compatible: Vec<_> = supported
        .iter()
        .filter(|&(v, _)| *version >= *v)
        .map(|&(_, c)| c)
        .collect();

    // The PSCI kernel driver also supports PSCI v0.1, which is NOT forward-compatible.
    if compatible.is_empty() {
        compatible = vec!["arm,psci"];
    }

    compatible
}

fn create_psci_node(fdt: &mut Fdt, version: &PsciVersion) -> Result<()> {
    let compatible = psci_compatible(version);
    let psci_node = fdt.begin_node("psci")?;
    fdt.set_prop("compatible", compatible.as_slice())?;
    // Only support aarch64 guest
    fdt.set_prop("method", "hvc")?;
    fdt.end_node(psci_node)?;

    Ok(())
}

fn create_chosen_node(
    fdt: &mut Fdt,
    cmdline: &str,
    initrd: Option<(GuestAddress, usize)>,
) -> Result<()> {
    let chosen_node = fdt.begin_node("chosen")?;
    fdt.set_prop("linux,pci-probe-only", 1u32)?;
    fdt.set_prop("bootargs", cmdline)?;
    // Used by android bootloader for boot console output
    fdt.set_prop("stdout-path", format!("/U6_16550A@{:x}", SERIAL_ADDR[0]))?;

    let mut kaslr_seed_bytes = [0u8; 8];
    OsRng.fill_bytes(&mut kaslr_seed_bytes);
    let kaslr_seed = u64::from_le_bytes(kaslr_seed_bytes);
    fdt.set_prop("kaslr-seed", kaslr_seed)?;

    let mut rng_seed_bytes = [0u8; 256];
    OsRng.fill_bytes(&mut rng_seed_bytes);
    fdt.set_prop("rng-seed", &rng_seed_bytes)?;

    if let Some((initrd_addr, initrd_size)) = initrd {
        let initrd_start = initrd_addr.offset() as u32;
        let initrd_end = initrd_start + initrd_size as u32;
        fdt.set_prop("linux,initrd-start", initrd_start)?;
        fdt.set_prop("linux,initrd-end", initrd_end)?;
    }
    fdt.end_node(chosen_node)?;

    Ok(())
}

fn create_config_node(fdt: &mut Fdt, (addr, size): (GuestAddress, usize)) -> Result<()> {
    let addr: u32 = addr
        .offset()
        .try_into()
        .map_err(|_| Error::PropertyValueTooLarge)?;
    let size: u32 = size.try_into().map_err(|_| Error::PropertyValueTooLarge)?;

    let config_node = fdt.begin_node("config")?;
    fdt.set_prop("kernel-address", addr)?;
    fdt.set_prop("kernel-size", size)?;
    fdt.end_node(config_node)?;

    Ok(())
}

fn create_kvm_cpufreq_node(fdt: &mut Fdt) -> Result<()> {
    let vcf_node = fdt.begin_node("cpufreq")?;

    fdt.set_prop("compatible", "virtual,kvm-cpufreq")?;
    fdt.end_node(vcf_node)?;

    Ok(())
}

/// PCI host controller address range.
///
/// This represents a single entry in the "ranges" property for a PCI host controller.
///
/// See [PCI Bus Binding to Open Firmware](https://www.openfirmware.info/data/docs/bus.pci.pdf)
/// and https://www.kernel.org/doc/Documentation/devicetree/bindings/pci/host-generic-pci.txt
/// for more information.
#[derive(Copy, Clone)]
pub struct PciRange {
    pub space: PciAddressSpace,
    pub bus_address: u64,
    pub cpu_physical_address: u64,
    pub size: u64,
    pub prefetchable: bool,
}

/// PCI address space.
#[derive(Copy, Clone)]
#[allow(dead_code)]
pub enum PciAddressSpace {
    /// PCI configuration space
    Configuration = 0b00,
    /// I/O space
    Io = 0b01,
    /// 32-bit memory space
    Memory = 0b10,
    /// 64-bit memory space
    Memory64 = 0b11,
}

/// Location of memory-mapped PCI configuration space.
#[derive(Copy, Clone)]
pub struct PciConfigRegion {
    /// Physical address of the base of the memory-mapped PCI configuration region.
    pub base: u64,
    /// Size of the PCI configuration region in bytes.
    pub size: u64,
}

/// Location of memory-mapped vm watchdog
#[derive(Copy, Clone)]
pub struct VmWdtConfig {
    /// Physical address of the base of the memory-mapped vm watchdog region.
    pub base: u64,
    /// Size of the vm watchdog region in bytes.
    pub size: u64,
    /// The internal clock frequency of the watchdog.
    pub clock_hz: u32,
    /// The expiration timeout measured in seconds.
    pub timeout_sec: u32,
}

fn create_pci_nodes(
    fdt: &mut Fdt,
    pci_irqs: Vec<(PciAddress, u32, PciInterruptPin)>,
    cfg: PciConfigRegion,
    ranges: &[PciRange],
    dma_pool_phandle: Option<u32>,
) -> Result<()> {
    // Add devicetree nodes describing a PCI generic host controller.
    // See Documentation/devicetree/bindings/pci/host-generic-pci.txt in the kernel
    // and "PCI Bus Binding to IEEE Std 1275-1994".
    let ranges: Vec<u32> = ranges
        .iter()
        .flat_map(|r| {
            let ss = r.space as u32;
            let p = r.prefetchable as u32;
            [
                // BUS_ADDRESS(3) encoded as defined in OF PCI Bus Binding
                (ss << 24) | (p << 30),
                (r.bus_address >> 32) as u32,
                r.bus_address as u32,
                // CPU_PHYSICAL(2)
                (r.cpu_physical_address >> 32) as u32,
                r.cpu_physical_address as u32,
                // SIZE(2)
                (r.size >> 32) as u32,
                r.size as u32,
            ]
        })
        .collect();

    let bus_range = [0u32, 0u32]; // Only bus 0
    let reg = [cfg.base, cfg.size];

    let mut interrupts: Vec<u32> = Vec::new();
    let mut masks: Vec<u32> = Vec::new();

    for (address, irq_num, irq_pin) in pci_irqs.iter() {
        // PCI_DEVICE(3)
        interrupts.push(address.to_config_address(0, 8));
        interrupts.push(0);
        interrupts.push(0);

        // INT#(1)
        interrupts.push(irq_pin.to_mask() + 1);

        // CONTROLLER(PHANDLE)
        interrupts.push(PHANDLE_GIC);
        interrupts.push(0);
        interrupts.push(0);

        // CONTROLLER_DATA(3)
        interrupts.push(GIC_FDT_IRQ_TYPE_SPI);
        interrupts.push(*irq_num);
        interrupts.push(IRQ_TYPE_LEVEL_HIGH);

        // PCI_DEVICE(3)
        masks.push(0xf800); // bits 11..15 (device)
        masks.push(0);
        masks.push(0);

        // INT#(1)
        masks.push(0x7); // allow INTA#-INTD# (1 | 2 | 3 | 4)
    }

    let pci_node = fdt.begin_node("pci")?;
    fdt.set_prop("compatible", "pci-host-cam-generic")?;
    fdt.set_prop("device_type", "pci")?;
    fdt.set_prop("ranges", ranges)?;
    fdt.set_prop("bus-range", &bus_range)?;
    fdt.set_prop("#address-cells", 3u32)?;
    fdt.set_prop("#size-cells", 2u32)?;
    fdt.set_prop("reg", &reg)?;
    fdt.set_prop("#interrupt-cells", 1u32)?;
    fdt.set_prop("interrupt-map", interrupts)?;
    fdt.set_prop("interrupt-map-mask", masks)?;
    fdt.set_prop("dma-coherent", ())?;
    if let Some(dma_pool_phandle) = dma_pool_phandle {
        fdt.set_prop("memory-region", dma_pool_phandle)?;
    }
    fdt.end_node(pci_node)?;

    Ok(())
}

fn create_rtc_node(fdt: &mut Fdt) -> Result<()> {
    // the kernel driver for pl030 really really wants a clock node
    // associated with an AMBA device or it will fail to probe, so we
    // need to make up a clock node to associate with the pl030 rtc
    // node and an associated handle with a unique phandle value.
    const CLK_PHANDLE: u32 = 24;
    let clock_node = fdt.begin_node("pclk@3M")?;
    fdt.set_prop("#clock-cells", 0u32)?;
    fdt.set_prop("compatible", "fixed-clock")?;
    fdt.set_prop("clock-frequency", 3141592u32)?;
    fdt.set_prop("phandle", CLK_PHANDLE)?;
    fdt.end_node(clock_node)?;

    let rtc_name = format!("rtc@{:x}", AARCH64_RTC_ADDR);
    let reg = [AARCH64_RTC_ADDR, AARCH64_RTC_SIZE];
    let irq = [GIC_FDT_IRQ_TYPE_SPI, AARCH64_RTC_IRQ, IRQ_TYPE_LEVEL_HIGH];

    let rtc_node = fdt.begin_node(&rtc_name)?;
    fdt.set_prop("compatible", "arm,primecell")?;
    fdt.set_prop("arm,primecell-periphid", PL030_AMBA_ID)?;
    fdt.set_prop("reg", &reg)?;
    fdt.set_prop("interrupts", &irq)?;
    fdt.set_prop("clocks", CLK_PHANDLE)?;
    fdt.set_prop("clock-names", "apb_pclk")?;
    fdt.end_node(rtc_node)?;
    Ok(())
}

/// Create a flattened device tree node for Goldfish Battery device.
///
/// # Arguments
///
/// * `fdt` - A Fdt in which the node is created
/// * `mmio_base` - The MMIO base address of the battery
/// * `irq` - The IRQ number of the battery
fn create_battery_node(fdt: &mut Fdt, mmio_base: u64, irq: u32) -> Result<()> {
    let reg = [mmio_base, GOLDFISHBAT_MMIO_LEN];
    let irqs = [GIC_FDT_IRQ_TYPE_SPI, irq, IRQ_TYPE_LEVEL_HIGH];
    let bat_node = fdt.begin_node("goldfish_battery")?;
    fdt.set_prop("compatible", "google,goldfish-battery")?;
    fdt.set_prop("reg", &reg)?;
    fdt.set_prop("interrupts", &irqs)?;
    fdt.end_node(bat_node)?;
    Ok(())
}

fn create_vmwdt_node(fdt: &mut Fdt, vmwdt_cfg: VmWdtConfig) -> Result<()> {
    let vmwdt_name = format!("vmwdt@{:x}", vmwdt_cfg.base);
    let reg = [vmwdt_cfg.base, vmwdt_cfg.size];
    let vmwdt_node = fdt.begin_node(&vmwdt_name)?;
    fdt.set_prop("compatible", "qemu,vcpu-stall-detector")?;
    fdt.set_prop("reg", &reg)?;
    fdt.set_prop("clock-frequency", vmwdt_cfg.clock_hz)?;
    fdt.set_prop("timeout-sec", vmwdt_cfg.timeout_sec)?;
    fdt.end_node(vmwdt_node)?;
    Ok(())
}

/// Creates a flattened device tree containing all of the parameters for the
/// kernel and loads it into the guest memory at the specified offset.
///
/// # Arguments
///
/// * `fdt_max_size` - The amount of space reserved for the device tree
/// * `guest_mem` - The guest memory object
/// * `pci_irqs` - List of PCI device address to PCI interrupt number and pin mappings
/// * `pci_cfg` - Location of the memory-mapped PCI configuration space.
/// * `pci_ranges` - Memory ranges accessible via the PCI host controller.
/// * `num_cpus` - Number of virtual CPUs the guest will have
/// * `fdt_address` - The offset into physical memory for the device tree
/// * `cmdline` - The kernel commandline
/// * `initrd` - An optional tuple of initrd guest physical address and size
/// * `android_fstab` - An optional file holding Android fstab entries
/// * `is_gicv3` - True if gicv3, false if v2
/// * `psci_version` - the current PSCI version
/// * `swiotlb` - Reserve a memory pool for DMA. Tuple of base address and size.
/// * `bat_mmio_base_and_irq` - The battery base address and irq number
/// * `vmwdt_cfg` - The virtual watchdog configuration
/// * `dump_device_tree_blob` - Option path to write DTB to
/// * `vm_generator` - Callback to add additional nodes to DTB. create_vm uses Aarch64Vm::create_fdt
pub fn create_fdt(
    fdt_max_size: usize,
    guest_mem: &GuestMemory,
    pci_irqs: Vec<(PciAddress, u32, PciInterruptPin)>,
    pci_cfg: PciConfigRegion,
    pci_ranges: &[PciRange],
    num_cpus: u32,
    cpu_clusters: Vec<CpuSet>,
    cpu_capacity: BTreeMap<usize, u32>,
    cpu_frequencies: BTreeMap<usize, Vec<u32>>,
    fdt_address: GuestAddress,
    cmdline: &str,
    image: (GuestAddress, usize),
    initrd: Option<(GuestAddress, usize)>,
    android_fstab: Option<File>,
    is_gicv3: bool,
    use_pmu: bool,
    psci_version: PsciVersion,
    swiotlb: Option<(Option<GuestAddress>, u64)>,
    bat_mmio_base_and_irq: Option<(u64, u32)>,
    vmwdt_cfg: VmWdtConfig,
    dump_device_tree_blob: Option<PathBuf>,
    vm_generator: &impl Fn(&mut Fdt, &BTreeMap<&str, u32>) -> cros_fdt::Result<()>,
    dynamic_power_coefficient: BTreeMap<usize, u32>,
) -> Result<()> {
    let mut fdt = Fdt::new(&[]);
    let mut phandles = BTreeMap::new();

    // The whole thing is put into one giant node with some top level properties
    let root_node = fdt.begin_node("")?;
    fdt.set_prop("interrupt-parent", PHANDLE_GIC)?;
    phandles.insert("intc", PHANDLE_GIC);
    fdt.set_prop("compatible", "linux,dummy-virt")?;
    fdt.set_prop("#address-cells", 0x2u32)?;
    fdt.set_prop("#size-cells", 0x2u32)?;
    if let Some(android_fstab) = android_fstab {
        arch::android::create_android_fdt(&mut fdt, android_fstab)?;
    }
    create_chosen_node(&mut fdt, cmdline, initrd)?;
    create_config_node(&mut fdt, image)?;
    create_memory_node(&mut fdt, guest_mem)?;
    let dma_pool_phandle = match swiotlb {
        Some(x) => {
            let phandle = create_resv_memory_node(&mut fdt, x)?;
            phandles.insert("restricted_dma_reserved", phandle);
            Some(phandle)
        }
        None => None,
    };
    create_cpu_nodes(
        &mut fdt,
        num_cpus,
        cpu_clusters,
        cpu_capacity,
        dynamic_power_coefficient,
        cpu_frequencies.clone(),
    )?;
    create_gic_node(&mut fdt, is_gicv3, num_cpus as u64)?;
    create_timer_node(&mut fdt, num_cpus)?;
    if use_pmu {
        create_pmu_node(&mut fdt, num_cpus)?;
    }
    create_serial_nodes(&mut fdt)?;
    create_psci_node(&mut fdt, &psci_version)?;
    create_pci_nodes(&mut fdt, pci_irqs, pci_cfg, pci_ranges, dma_pool_phandle)?;
    create_rtc_node(&mut fdt)?;
    if let Some((bat_mmio_base, bat_irq)) = bat_mmio_base_and_irq {
        create_battery_node(&mut fdt, bat_mmio_base, bat_irq)?;
    }
    create_vmwdt_node(&mut fdt, vmwdt_cfg)?;
    create_kvm_cpufreq_node(&mut fdt)?;
    vm_generator(&mut fdt, &phandles)?;
    if !cpu_frequencies.is_empty() {
        create_virt_cpufreq_node(&mut fdt, num_cpus as u64)?;
    }
    // End giant node
    fdt.end_node(root_node)?;

    let fdt_final = fdt.finish()?;

    if let Some(file_path) = dump_device_tree_blob {
        std::fs::write(&file_path, &fdt_final)
            .map_err(|e| Error::FdtDumpIoError(e, file_path.clone()))?;
    }

    if fdt_final.len() > fdt_max_size {
        return Err(Error::TotalSizeTooLarge);
    }

    let written = guest_mem
        .write_at_addr(fdt_final.as_slice(), fdt_address)
        .map_err(|_| Error::FdtGuestMemoryWriteError)?;
    if written < fdt_final.len() {
        return Err(Error::FdtGuestMemoryWriteError);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn psci_compatible_v0_1() {
        assert_eq!(
            psci_compatible(&PsciVersion::new(0, 1).unwrap()),
            vec!["arm,psci"]
        );
    }

    #[test]
    fn psci_compatible_v0_2() {
        assert_eq!(
            psci_compatible(&PsciVersion::new(0, 2).unwrap()),
            vec!["arm,psci-0.2"]
        );
    }

    #[test]
    fn psci_compatible_v0_5() {
        // Only the 0.2 version supported by the kernel should be added.
        assert_eq!(
            psci_compatible(&PsciVersion::new(0, 5).unwrap()),
            vec!["arm,psci-0.2"]
        );
    }

    #[test]
    fn psci_compatible_v1_0() {
        // Both 1.0 and 0.2 should be listed, in that order.
        assert_eq!(
            psci_compatible(&PsciVersion::new(1, 0).unwrap()),
            vec!["arm,psci-1.0", "arm,psci-0.2"]
        );
    }

    #[test]
    fn psci_compatible_v1_5() {
        // Only the 1.0 and 0.2 versions supported by the kernel should be listed.
        assert_eq!(
            psci_compatible(&PsciVersion::new(1, 5).unwrap()),
            vec!["arm,psci-1.0", "arm,psci-0.2"]
        );
    }
}
