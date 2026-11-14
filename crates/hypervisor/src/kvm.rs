//! KVM backend.

use kvm_bindings::{
    kvm_dtable, kvm_lapic_state, kvm_msr_entry, kvm_pit_config, kvm_regs, kvm_segment,
    kvm_userspace_memory_region, CpuId, Msrs, KVM_MAX_CPUID_ENTRIES, KVM_PIT_SPEAKER_DUMMY,
};
use kvm_ioctls::{Kvm, VcpuExit, VcpuFd, VmFd};

use crate::x86_64::{
    ApicDeliveryMode, DescriptorTable, FpuControl, Segment, SpecialRegs, StandardRegs,
};
use crate::{Error, Hypervisor, Result, Vcpu, VmExit};

// Fixed on Linux x86-64; not worth a libc dependency.
const EINTR: i32 = 4;
const EAGAIN: i32 = 11;

/// Three pages just below the BIOS area at the top of 4 GiB, where no guest
/// RAM is ever mapped. Intel VT-x needs somewhere to keep a real-mode TSS.
const TSS_ADDR: usize = 0xfffb_d000;

/// Local APIC register offsets within the KVM_GET_LAPIC page.
const APIC_LVT0: usize = 0x350;
const APIC_LVT1: usize = 0x360;
const APIC_LVT_DELIVERY_MODE_MASK: u32 = 0x700;

fn backend(op: &'static str) -> impl FnOnce(kvm_ioctls::Error) -> Error {
    move |e| Error::Backend {
        op,
        source: std::io::Error::from_raw_os_error(e.errno()),
    }
}

pub struct KvmHypervisor {
    kvm: Kvm,
}

impl KvmHypervisor {
    pub fn new() -> Result<Self> {
        let kvm = Kvm::new().map_err(|e| {
            Error::Unavailable(format!(
                "cannot open /dev/kvm ({e}); is KVM enabled and do you have rw access?"
            ))
        })?;
        Ok(Self { kvm })
    }

    /// KVM_GET_API_VERSION; always 12 on any kernel worth running.
    pub fn api_version(&self) -> i32 {
        self.kvm.get_api_version()
    }
}

impl Hypervisor for KvmHypervisor {
    type Vm = KvmVm;

    fn create_vm(&self) -> Result<KvmVm> {
        let fd = self.kvm.create_vm().map_err(backend("KVM_CREATE_VM"))?;
        fd.set_tss_address(TSS_ADDR)
            .map_err(backend("KVM_SET_TSS_ADDR"))?;
        let supported_cpuid = self
            .kvm
            .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
            .map_err(backend("KVM_GET_SUPPORTED_CPUID"))?;
        Ok(KvmVm {
            fd,
            supported_cpuid,
        })
    }
}

pub struct KvmVm {
    fd: VmFd,
    supported_cpuid: CpuId,
}

impl crate::Vm for KvmVm {
    type Vcpu = KvmVcpu;

    unsafe fn map_memory(
        &self,
        slot: u32,
        guest_addr: u64,
        host_addr: *mut u8,
        size: u64,
    ) -> Result<()> {
        let region = kvm_userspace_memory_region {
            slot,
            flags: 0,
            guest_phys_addr: guest_addr,
            memory_size: size,
            userspace_addr: host_addr as u64,
        };
        // SAFETY: the caller guarantees the host range is valid and outlives
        // the VM.
        unsafe { self.fd.set_user_memory_region(region) }
            .map_err(backend("KVM_SET_USER_MEMORY_REGION"))
    }

    fn init_platform(&self) -> Result<()> {
        self.fd
            .create_irq_chip()
            .map_err(backend("KVM_CREATE_IRQCHIP"))?;
        let pit = kvm_pit_config {
            // Emulate the PC speaker port (0x61) in the kernel as well, so
            // the guest's PIT channel 2 calibration does not exit to us.
            flags: KVM_PIT_SPEAKER_DUMMY,
            ..Default::default()
        };
        self.fd.create_pit2(pit).map_err(backend("KVM_CREATE_PIT2"))
    }

    fn set_irq_line(&self, irq: u32, level: bool) -> Result<()> {
        self.fd
            .set_irq_line(irq, level)
            .map_err(backend("KVM_IRQ_LINE"))
    }

    fn create_vcpu(&self, id: u32) -> Result<KvmVcpu> {
        let fd = self
            .fd
            .create_vcpu(id as u64)
            .map_err(backend("KVM_CREATE_VCPU"))?;
        Ok(KvmVcpu {
            fd,
            id,
            supported_cpuid: self.supported_cpuid.clone(),
        })
    }
}

pub struct KvmVcpu {
    fd: VcpuFd,
    id: u32,
    supported_cpuid: CpuId,
}

impl Vcpu for KvmVcpu {
    fn init_cpuid(&mut self) -> Result<()> {
        // KVM reports what the host can virtualise, but leaves the per-CPU
        // identity fields as the host CPU's. Make them describe this vCPU.
        let mut cpuid = self.supported_cpuid.clone();
        for entry in cpuid.as_mut_slice() {
            match entry.function {
                0x1 => {
                    // EBX[31:24]: initial APIC ID. ECX[31]: running under a
                    // hypervisor.
                    entry.ebx = (entry.ebx & 0x00ff_ffff) | (self.id << 24);
                    entry.ecx |= 1 << 31;
                }
                // Extended topology leaves: EDX is the x2APIC ID.
                0xb | 0x1f => entry.edx = self.id,
                _ => {}
            }
        }
        self.fd
            .set_cpuid2(&cpuid)
            .map_err(backend("KVM_SET_CPUID2"))
    }

    fn get_regs(&self) -> Result<StandardRegs> {
        let r = self.fd.get_regs().map_err(backend("KVM_GET_REGS"))?;
        Ok(StandardRegs {
            rax: r.rax,
            rbx: r.rbx,
            rcx: r.rcx,
            rdx: r.rdx,
            rsi: r.rsi,
            rdi: r.rdi,
            rsp: r.rsp,
            rbp: r.rbp,
            r8: r.r8,
            r9: r.r9,
            r10: r.r10,
            r11: r.r11,
            r12: r.r12,
            r13: r.r13,
            r14: r.r14,
            r15: r.r15,
            rip: r.rip,
            rflags: r.rflags,
        })
    }

    fn set_regs(&mut self, r: &StandardRegs) -> Result<()> {
        let regs = kvm_regs {
            rax: r.rax,
            rbx: r.rbx,
            rcx: r.rcx,
            rdx: r.rdx,
            rsi: r.rsi,
            rdi: r.rdi,
            rsp: r.rsp,
            rbp: r.rbp,
            r8: r.r8,
            r9: r.r9,
            r10: r.r10,
            r11: r.r11,
            r12: r.r12,
            r13: r.r13,
            r14: r.r14,
            r15: r.r15,
            rip: r.rip,
            rflags: r.rflags,
        };
        self.fd.set_regs(&regs).map_err(backend("KVM_SET_REGS"))
    }

    fn get_sregs(&self) -> Result<SpecialRegs> {
        let s = self.fd.get_sregs().map_err(backend("KVM_GET_SREGS"))?;
        Ok(SpecialRegs {
            cs: segment_from_kvm(&s.cs),
            ds: segment_from_kvm(&s.ds),
            es: segment_from_kvm(&s.es),
            fs: segment_from_kvm(&s.fs),
            gs: segment_from_kvm(&s.gs),
            ss: segment_from_kvm(&s.ss),
            tr: segment_from_kvm(&s.tr),
            ldt: segment_from_kvm(&s.ldt),
            gdt: DescriptorTable {
                base: s.gdt.base,
                limit: s.gdt.limit,
            },
            idt: DescriptorTable {
                base: s.idt.base,
                limit: s.idt.limit,
            },
            cr0: s.cr0,
            cr2: s.cr2,
            cr3: s.cr3,
            cr4: s.cr4,
            cr8: s.cr8,
            efer: s.efer,
        })
    }

    fn set_sregs(&mut self, s: &SpecialRegs) -> Result<()> {
        // Start from the current state so that apic_base and the pending
        // interrupt bitmap, which SpecialRegs does not model, are preserved.
        let mut k = self.fd.get_sregs().map_err(backend("KVM_GET_SREGS"))?;
        k.cs = segment_to_kvm(&s.cs);
        k.ds = segment_to_kvm(&s.ds);
        k.es = segment_to_kvm(&s.es);
        k.fs = segment_to_kvm(&s.fs);
        k.gs = segment_to_kvm(&s.gs);
        k.ss = segment_to_kvm(&s.ss);
        k.tr = segment_to_kvm(&s.tr);
        k.ldt = segment_to_kvm(&s.ldt);
        k.gdt = kvm_dtable {
            base: s.gdt.base,
            limit: s.gdt.limit,
            ..Default::default()
        };
        k.idt = kvm_dtable {
            base: s.idt.base,
            limit: s.idt.limit,
            ..Default::default()
        };
        k.cr0 = s.cr0;
        k.cr2 = s.cr2;
        k.cr3 = s.cr3;
        k.cr4 = s.cr4;
        k.cr8 = s.cr8;
        k.efer = s.efer;
        self.fd.set_sregs(&k).map_err(backend("KVM_SET_SREGS"))
    }

    fn set_msrs(&mut self, msrs: &[(u32, u64)]) -> Result<()> {
        let entries: Vec<kvm_msr_entry> = msrs
            .iter()
            .map(|&(index, data)| kvm_msr_entry {
                index,
                data,
                ..Default::default()
            })
            .collect();
        let wrapper = Msrs::from_entries(&entries).map_err(|_| Error::Backend {
            op: "KVM_SET_MSRS",
            source: std::io::Error::from_raw_os_error(7), // E2BIG
        })?;
        // KVM stops at the first MSR it refuses and returns how many it set.
        let written = self
            .fd
            .set_msrs(&wrapper)
            .map_err(backend("KVM_SET_MSRS"))?;
        match msrs.get(written) {
            Some(&(index, _)) => Err(Error::MsrRejected { index }),
            None => Ok(()),
        }
    }

    fn set_fpu_control(&mut self, fpu: FpuControl) -> Result<()> {
        let mut k = self.fd.get_fpu().map_err(backend("KVM_GET_FPU"))?;
        k.fcw = fpu.fcw;
        k.mxcsr = fpu.mxcsr;
        self.fd.set_fpu(&k).map_err(backend("KVM_SET_FPU"))
    }

    fn set_lapic_lint(&mut self, lint0: ApicDeliveryMode, lint1: ApicDeliveryMode) -> Result<()> {
        let mut lapic = self.fd.get_lapic().map_err(backend("KVM_GET_LAPIC"))?;
        set_lvt_delivery_mode(&mut lapic, APIC_LVT0, lint0);
        set_lvt_delivery_mode(&mut lapic, APIC_LVT1, lint1);
        self.fd.set_lapic(&lapic).map_err(backend("KVM_SET_LAPIC"))
    }

    fn run(&mut self) -> Result<VmExit<'_>> {
        match self.fd.run() {
            Ok(VcpuExit::IoOut(port, data)) => Ok(VmExit::IoOut { port, data }),
            Ok(VcpuExit::IoIn(port, data)) => Ok(VmExit::IoIn { port, data }),
            Ok(VcpuExit::MmioWrite(addr, data)) => Ok(VmExit::MmioWrite { addr, data }),
            Ok(VcpuExit::MmioRead(addr, data)) => Ok(VmExit::MmioRead { addr, data }),
            Ok(VcpuExit::Hlt) => Ok(VmExit::Hlt),
            Ok(VcpuExit::Shutdown) => Ok(VmExit::Shutdown),
            Ok(VcpuExit::FailEntry(reason, _cpu)) => Err(Error::FailEntry { reason }),
            Ok(other) => Err(Error::UnsupportedExit(format!("{other:?}"))),
            Err(e) if e.errno() == EINTR || e.errno() == EAGAIN => Ok(VmExit::Interrupted),
            Err(e) => Err(backend("KVM_RUN")(e)),
        }
    }
}

fn segment_from_kvm(s: &kvm_segment) -> Segment {
    Segment {
        base: s.base,
        limit: s.limit,
        selector: s.selector,
        type_: s.type_,
        present: s.present != 0,
        dpl: s.dpl,
        db: s.db != 0,
        s: s.s != 0,
        l: s.l != 0,
        g: s.g != 0,
        avl: s.avl != 0,
        unusable: s.unusable != 0,
    }
}

fn segment_to_kvm(s: &Segment) -> kvm_segment {
    kvm_segment {
        base: s.base,
        limit: s.limit,
        selector: s.selector,
        type_: s.type_,
        present: s.present as u8,
        dpl: s.dpl,
        db: s.db as u8,
        s: s.s as u8,
        l: s.l as u8,
        g: s.g as u8,
        avl: s.avl as u8,
        unusable: s.unusable as u8,
        padding: 0,
    }
}

fn set_lvt_delivery_mode(lapic: &mut kvm_lapic_state, offset: usize, mode: ApicDeliveryMode) {
    let mut bytes = [0u8; 4];
    for (b, r) in bytes.iter_mut().zip(&lapic.regs[offset..offset + 4]) {
        *b = *r as u8;
    }
    let lvt = (u32::from_le_bytes(bytes) & !APIC_LVT_DELIVERY_MODE_MASK) | ((mode as u32) << 8);
    for (r, b) in lapic.regs[offset..offset + 4]
        .iter_mut()
        .zip(lvt.to_le_bytes())
    {
        *r = b as _;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lvt_delivery_mode_is_replaced_and_other_bits_kept() {
        let mut lapic = kvm_lapic_state::default();
        // Masked (bit 16), fixed delivery, vector 0x31.
        for (r, b) in lapic.regs[APIC_LVT0..APIC_LVT0 + 4]
            .iter_mut()
            .zip(0x0001_0031u32.to_le_bytes())
        {
            *r = b as _;
        }
        set_lvt_delivery_mode(&mut lapic, APIC_LVT0, ApicDeliveryMode::ExtInt);
        set_lvt_delivery_mode(&mut lapic, APIC_LVT1, ApicDeliveryMode::Nmi);

        let read = |off: usize| {
            let mut b = [0u8; 4];
            for (d, s) in b.iter_mut().zip(&lapic.regs[off..off + 4]) {
                *d = *s as u8;
            }
            u32::from_le_bytes(b)
        };
        assert_eq!(read(APIC_LVT0), 0x0001_0731);
        assert_eq!(read(APIC_LVT1), 0x0000_0400);
    }
}
