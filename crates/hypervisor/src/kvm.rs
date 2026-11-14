//! KVM backend.

use kvm_bindings::{
    kvm_dtable, kvm_regs, kvm_segment, kvm_userspace_memory_region, CpuId, KVM_MAX_CPUID_ENTRIES,
};
use kvm_ioctls::{Kvm, VcpuExit, VcpuFd, VmFd};

use crate::x86_64::{DescriptorTable, Segment, SpecialRegs, StandardRegs};
use crate::{Error, Hypervisor, Result, Vcpu, VmExit};

// Fixed on Linux x86-64; not worth a libc dependency.
const EINTR: i32 = 4;
const EAGAIN: i32 = 11;

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

    fn create_vcpu(&self, id: u32) -> Result<KvmVcpu> {
        let fd = self
            .fd
            .create_vcpu(id as u64)
            .map_err(backend("KVM_CREATE_VCPU"))?;
        Ok(KvmVcpu {
            fd,
            supported_cpuid: self.supported_cpuid.clone(),
        })
    }
}

pub struct KvmVcpu {
    fd: VcpuFd,
    supported_cpuid: CpuId,
}

impl Vcpu for KvmVcpu {
    fn init_cpuid(&mut self) -> Result<()> {
        self.fd
            .set_cpuid2(&self.supported_cpuid)
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
