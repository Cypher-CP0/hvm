//! KVM backend.

use kvm_bindings::kvm_userspace_memory_region;
use kvm_ioctls::{Kvm, VcpuExit, VcpuFd, VmFd};

use crate::{Error, Hypervisor, Result, Vcpu, VmExit};

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
        Ok(KvmVm { fd })
    }
}

pub struct KvmVm {
    fd: VmFd,
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
        Ok(KvmVcpu { fd })
    }
}

pub struct KvmVcpu {
    fd: VcpuFd,
}

impl Vcpu for KvmVcpu {
    fn setup_real_mode(&mut self, entry: u16) -> Result<()> {
        // The vCPU comes out of KVM_CREATE_VCPU in the x86 reset state
        // (CS base 0xffff0000, IP 0xfff0). Flatten CS to base 0 so that
        // linear address == IP; the other segments already have base 0.
        let mut sregs = self.fd.get_sregs().map_err(backend("KVM_GET_SREGS"))?;
        sregs.cs.base = 0;
        sregs.cs.selector = 0;
        self.fd
            .set_sregs(&sregs)
            .map_err(backend("KVM_SET_SREGS"))?;

        let mut regs = self.fd.get_regs().map_err(backend("KVM_GET_REGS"))?;
        regs.rip = entry as u64;
        regs.rflags = 0x2; // bit 1 is reserved and must be set
        self.fd.set_regs(&regs).map_err(backend("KVM_SET_REGS"))
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
            Err(e) if e.errno() == libc_eintr() || e.errno() == libc_eagain() => {
                Ok(VmExit::Interrupted)
            }
            Err(e) => Err(backend("KVM_RUN")(e)),
        }
    }
}

// Avoid a libc dependency for two constants; values are fixed on Linux x86-64.
const fn libc_eintr() -> i32 {
    4
}
const fn libc_eagain() -> i32 {
    11
}
