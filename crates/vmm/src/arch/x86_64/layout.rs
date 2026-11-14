//! Guest physical memory layout for boot-time structures.
//!
//! Everything lives in the first megabyte, below where a kernel is loaded.
//! The addresses follow the conventions used by other Linux-booting VMMs so
//! that register dumps look familiar.

/// Boot GDT (four 8-byte entries).
pub const BOOT_GDT_ADDR: u64 = 0x500;
/// Boot IDT (a single null entry; the guest installs its own).
pub const BOOT_IDT_ADDR: u64 = 0x520;
/// Initial stack pointer. The stack grows down from here.
pub const BOOT_STACK_POINTER: u64 = 0x8ff0;
/// Boot page tables: one page each.
pub const PML4_ADDR: u64 = 0x9000;
pub const PDPT_ADDR: u64 = 0xa000;
pub const PD_ADDR: u64 = 0xb000;
/// Where guest code (later: the kernel) is loaded. 1 MiB.
pub const HIMEM_START: u64 = 0x10_0000;
