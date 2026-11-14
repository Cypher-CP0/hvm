//! Backend-agnostic hypervisor abstraction.
//!
//! The VMM talks only to the traits in this crate. KVM is the first backend;
//! a WHPX backend can be added later without touching the device model or
//! the run loop.

use std::fmt;

#[cfg(target_os = "linux")]
pub mod kvm;
pub mod x86_64;

use x86_64::{ApicDeliveryMode, FpuControl, SpecialRegs, StandardRegs};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("hypervisor is not available: {0}")]
    Unavailable(String),
    #[error("{op} failed: {source}")]
    Backend {
        op: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("vCPU entry failed (hardware reason {reason:#x})")]
    FailEntry { reason: u64 },
    #[error("the hypervisor rejected MSR {index:#x}")]
    MsrRejected { index: u32 },
    #[error("unsupported vCPU exit: {0}")]
    UnsupportedExit(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Why the guest stopped running and returned control to the VMM.
///
/// Borrowed buffers point into the backend's shared run area: for `IoIn` and
/// `MmioRead` the VMM fills the buffer and the guest sees it on re-entry.
pub enum VmExit<'a> {
    IoOut {
        port: u16,
        data: &'a [u8],
    },
    IoIn {
        port: u16,
        data: &'a mut [u8],
    },
    MmioWrite {
        addr: u64,
        data: &'a [u8],
    },
    MmioRead {
        addr: u64,
        data: &'a mut [u8],
    },
    /// Guest executed HLT with no in-kernel interrupt controller to wake it.
    Hlt,
    /// Guest shut down or triple-faulted.
    Shutdown,
    /// The run was interrupted by a signal; just re-enter.
    Interrupted,
}

impl VmExit<'_> {
    /// Stable short name, used as the key for exit statistics.
    pub fn kind(&self) -> ExitKind {
        match self {
            VmExit::IoOut { .. } => ExitKind::IoOut,
            VmExit::IoIn { .. } => ExitKind::IoIn,
            VmExit::MmioWrite { .. } => ExitKind::MmioWrite,
            VmExit::MmioRead { .. } => ExitKind::MmioRead,
            VmExit::Hlt => ExitKind::Hlt,
            VmExit::Shutdown => ExitKind::Shutdown,
            VmExit::Interrupted => ExitKind::Interrupted,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(usize)]
pub enum ExitKind {
    IoOut,
    IoIn,
    MmioWrite,
    MmioRead,
    Hlt,
    Shutdown,
    Interrupted,
}

impl ExitKind {
    pub const COUNT: usize = 7;
    pub const ALL: [ExitKind; Self::COUNT] = [
        ExitKind::IoOut,
        ExitKind::IoIn,
        ExitKind::MmioWrite,
        ExitKind::MmioRead,
        ExitKind::Hlt,
        ExitKind::Shutdown,
        ExitKind::Interrupted,
    ];
}

impl fmt::Display for ExitKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ExitKind::IoOut => "io_out",
            ExitKind::IoIn => "io_in",
            ExitKind::MmioWrite => "mmio_write",
            ExitKind::MmioRead => "mmio_read",
            ExitKind::Hlt => "hlt",
            ExitKind::Shutdown => "shutdown",
            ExitKind::Interrupted => "interrupted",
        };
        f.write_str(s)
    }
}

/// Entry point to a hypervisor backend.
pub trait Hypervisor {
    type Vm: Vm;

    fn create_vm(&self) -> Result<Self::Vm>;
}

/// A virtual machine: an address space plus a set of vCPUs.
pub trait Vm {
    type Vcpu: Vcpu;

    /// Map `size` bytes of host memory at `host_addr` into the guest physical
    /// address space at `guest_addr`.
    ///
    /// # Safety
    /// `host_addr..host_addr + size` must be a valid, page-aligned mapping
    /// that stays alive and is not unmapped for as long as the VM exists.
    unsafe fn map_memory(
        &self,
        slot: u32,
        guest_addr: u64,
        host_addr: *mut u8,
        size: u64,
    ) -> Result<()>;

    /// Create the interrupt controllers and timer that the hypervisor
    /// emulates itself: PIC, IOAPIC, PIT and a local APIC per vCPU.
    ///
    /// Must be called before any vCPU is created. Once it has been, a guest
    /// HLT is handled inside the hypervisor and no longer produces
    /// [`VmExit::Hlt`].
    fn init_platform(&self) -> Result<()>;

    fn create_vcpu(&self, id: u32) -> Result<Self::Vcpu>;
}

/// A virtual CPU. Owned and driven by exactly one host thread.
pub trait Vcpu {
    /// Expose the host-supported CPUID leaves to the guest. Must be called
    /// before the special registers are set: the backend validates control
    /// register and EFER values against the guest's CPUID.
    fn init_cpuid(&mut self) -> Result<()>;

    fn get_regs(&self) -> Result<StandardRegs>;
    fn set_regs(&mut self, regs: &StandardRegs) -> Result<()>;

    fn get_sregs(&self) -> Result<SpecialRegs>;
    /// Set the special registers. State not covered by [`SpecialRegs`]
    /// (APIC base, pending interrupts) is left untouched.
    fn set_sregs(&mut self, sregs: &SpecialRegs) -> Result<()>;

    /// Write model-specific registers, given as `(index, value)` pairs.
    fn set_msrs(&mut self, msrs: &[(u32, u64)]) -> Result<()>;

    fn set_fpu_control(&mut self, fpu: FpuControl) -> Result<()>;

    /// Set the delivery mode of the local APIC's LINT0 and LINT1 pins.
    /// Requires [`Vm::init_platform`].
    fn set_lapic_lint(&mut self, lint0: ApicDeliveryMode, lint1: ApicDeliveryMode) -> Result<()>;

    /// Enter the guest and block until it exits.
    fn run(&mut self) -> Result<VmExit<'_>>;
}
