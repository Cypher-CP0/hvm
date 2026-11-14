//! Boot-time vCPU register state.

use hypervisor::x86_64::{StandardRegs, CR0_PE, CR0_PG, CR4_PAE, EFER_LMA, EFER_LME};
use hypervisor::Vcpu;
use vm_memory::GuestMemoryMmap;

use super::layout::BOOT_STACK_POINTER;
use super::{gdt, paging, Result};

/// Put a freshly created vCPU directly into 64-bit long mode at `entry`.
///
/// A physical CPU has to climb from real mode through protected mode to get
/// here. A hypervisor can simply load the end state: flat 64-bit segments,
/// paging enabled over an identity map, and EFER.LME/LMA set.
pub fn setup_long_mode<V: Vcpu>(vcpu: &mut V, mem: &GuestMemoryMmap, entry: u64) -> Result<()> {
    // CPUID first: the backend validates EFER and CR4 against it.
    vcpu.init_cpuid()?;

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
        rflags: 0x2, // bit 1 is reserved and must be set
        ..Default::default()
    })?;
    Ok(())
}
