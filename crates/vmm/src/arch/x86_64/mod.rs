//! x86-64 guest setup: memory layout, GDT, page tables and boot registers.

pub mod gdt;
pub mod layout;
pub mod paging;
pub mod regs;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("guest memory access failed: {0}")]
    Memory(#[from] vm_memory::GuestMemoryError),
    #[error(transparent)]
    Hypervisor(#[from] hypervisor::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
