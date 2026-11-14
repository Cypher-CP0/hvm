//! x86-64 guest setup: memory layout, GDT, page tables, boot registers and
//! the Linux boot protocol.

pub mod gdt;
pub mod layout;
pub mod linux;
pub mod paging;
pub mod regs;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("guest memory access failed: {0}")]
    Memory(#[from] vm_memory::GuestMemoryError),
    #[error(transparent)]
    Hypervisor(#[from] hypervisor::Error),
    #[error("cannot load kernel image: {0}")]
    KernelLoad(#[from] linux_loader::loader::Error),
    #[error("kernel boot protocol {major}.{minor:02} is too old; 2.12 or newer is required")]
    BootProtocolTooOld { major: u8, minor: u8 },
    #[error("kernel has no 64-bit entry point (XLF_KERNEL_64 is not set)")]
    No64BitEntry,
    #[error(
        "kernel needs {needed_mib} MiB of guest memory but only {available_mib} MiB is configured"
    )]
    NotEnoughMemory { needed_mib: u64, available_mib: u64 },
    #[error("invalid kernel command line: {0}")]
    Cmdline(#[from] linux_loader::cmdline::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
