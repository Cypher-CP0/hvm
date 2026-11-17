//! Boot-time vCPU register state.

use hypervisor::x86_64::{
    ApicDeliveryMode, FpuControl, StandardRegs, CR0_PE, CR0_PG, CR4_PAE, EFER_LMA, EFER_LME,
};
use hypervisor::Vcpu;
use vm_memory::GuestMemoryMmap;

use super::layout::BOOT_STACK_POINTER;
use super::{gdt, paging, Result};

const MSR_IA32_TSC: u32 = 0x10;
const MSR_IA32_SYSENTER_CS: u32 = 0x174;
const MSR_IA32_SYSENTER_ESP: u32 = 0x175;
const MSR_IA32_SYSENTER_EIP: u32 = 0x176;
const MSR_IA32_MISC_ENABLE: u32 = 0x1a0;
const MSR_MTRR_DEF_TYPE: u32 = 0x2ff;
const MSR_STAR: u32 = 0xc000_0081;
const MSR_LSTAR: u32 = 0xc000_0082;
const MSR_CSTAR: u32 = 0xc000_0083;
const MSR_SYSCALL_MASK: u32 = 0xc000_0084;
const MSR_KERNEL_GS_BASE: u32 = 0xc000_0102;

const MISC_ENABLE_FAST_STRING: u64 = 1 << 0;
/// MTRRs enabled, default memory type write-back. Without this the guest
/// sees every page as uncacheable-by-default and has to fix it up itself.
const MTRR_DEF_TYPE_ENABLED_WB: u64 = (1 << 11) | 0x6;

/// MSR values a kernel expects from firmware at hand-off.
const BOOT_MSRS: [(u32, u64); 11] = [
    (MSR_IA32_SYSENTER_CS, 0),
    (MSR_IA32_SYSENTER_ESP, 0),
    (MSR_IA32_SYSENTER_EIP, 0),
    (MSR_STAR, 0),
    (MSR_CSTAR, 0),
    (MSR_KERNEL_GS_BASE, 0),
    (MSR_SYSCALL_MASK, 0),
    (MSR_LSTAR, 0),
    (MSR_IA32_TSC, 0),
    (MSR_IA32_MISC_ENABLE, MISC_ENABLE_FAST_STRING),
    (MSR_MTRR_DEF_TYPE, MTRR_DEF_TYPE_ENABLED_WB),
];

/// x87 and SSE power-on defaults: all exceptions masked, round to nearest.
const BOOT_FPU: FpuControl = FpuControl {
    fcw: 0x37f,
    mxcsr: 0x1f80,
};

/// Put a freshly created vCPU directly into 64-bit long mode at `entry`.
///
/// A physical CPU has to climb from real mode through protected mode to get
/// here. A hypervisor can simply load the end state: flat 64-bit segments,
/// paging enabled over an identity map, and EFER.LME/LMA set.
///
/// `rsi` is handed to the guest untouched; the Linux boot protocol expects
/// the address of `struct boot_params` there.
pub fn setup_long_mode<V: Vcpu>(
    vcpu: &mut V,
    mem: &GuestMemoryMmap,
    entry: u64,
    rsi: u64,
) -> Result<()> {
    // CPUID first: the backend validates EFER, CR4 and MSRs against it.
    vcpu.init_cpuid()?;
    vcpu.set_msrs(&BOOT_MSRS)?;
    vcpu.set_fpu_control(BOOT_FPU)?;

    let segments = gdt::setup(mem)?;
    let cr3 = paging::setup_identity_map(mem)?;

    let mut sregs = vcpu.get_sregs()?;
    sregs.cs = segments.code;
    sregs.ds = segments.data;
    sregs.es = segments.data;
    sregs.fs = segments.data;
    sregs.gs = segments.data;
    sregs.ss = segments.data;
    sregs.tr = segments.tss;
    sregs.gdt = segments.gdt;
    sregs.idt = segments.idt;
    sregs.cr3 = cr3;
    sregs.cr4 |= CR4_PAE;
    sregs.cr0 |= CR0_PE | CR0_PG;
    sregs.efer |= EFER_LME | EFER_LMA;
    vcpu.set_sregs(&sregs)?;

    vcpu.set_regs(&StandardRegs {
        rip: entry,
        rsp: BOOT_STACK_POINTER,
        rbp: BOOT_STACK_POINTER,
        rsi,
        rflags: 0x2, // interrupts off; bit 1 is reserved and must be set
        ..Default::default()
    })?;
    Ok(())
}

/// Wire the boot CPU's local APIC the way PC firmware leaves it: LINT0 takes
/// interrupts from the 8259 PIC, LINT1 is NMI. Without this the guest never
/// sees the PIT tick and stalls calibrating its timers.
///
/// Requires the in-hypervisor interrupt controllers (`Vm::init_platform`).
pub fn setup_lapic<V: Vcpu>(vcpu: &mut V) -> Result<()> {
    vcpu.set_lapic_lint(ApicDeliveryMode::ExtInt, ApicDeliveryMode::Nmi)?;
    Ok(())
}
