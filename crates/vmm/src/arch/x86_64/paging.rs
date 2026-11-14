//! Boot page tables.

use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

use super::layout::{PDPT_ADDR, PD_ADDR, PML4_ADDR};
use super::Result;

const PTE_PRESENT: u64 = 1 << 0;
const PTE_WRITABLE: u64 = 1 << 1;
const PTE_HUGE: u64 = 1 << 7;
const ENTRIES_PER_TABLE: u64 = 512;
const HUGE_PAGE_SIZE: u64 = 2 << 20;

/// Size of the identity-mapped region: one page directory of 2 MiB pages.
#[cfg(test)]
pub const IDENTITY_MAP_SIZE: u64 = ENTRIES_PER_TABLE * HUGE_PAGE_SIZE;

/// Identity-map the first 1 GiB of guest physical memory with 2 MiB pages.
///
/// This is only what the guest needs to start executing in long mode; a real
/// kernel replaces it with its own tables early in boot. Returns the value
/// for CR3.
pub fn setup_identity_map(mem: &GuestMemoryMmap) -> Result<u64> {
    // PML4[0] -> PDPT, PDPT[0] -> PD. Every other entry is not present.
    for table in [PML4_ADDR, PDPT_ADDR] {
        for i in 0..ENTRIES_PER_TABLE {
            mem.write_obj(0u64, GuestAddress(table + i * 8))?;
        }
    }
    mem.write_obj(
        PDPT_ADDR | PTE_PRESENT | PTE_WRITABLE,
        GuestAddress(PML4_ADDR),
    )?;
    mem.write_obj(
        PD_ADDR | PTE_PRESENT | PTE_WRITABLE,
        GuestAddress(PDPT_ADDR),
    )?;

    for i in 0..ENTRIES_PER_TABLE {
        let entry = (i * HUGE_PAGE_SIZE) | PTE_PRESENT | PTE_WRITABLE | PTE_HUGE;
        mem.write_obj(entry, GuestAddress(PD_ADDR + i * 8))?;
    }
    Ok(PML4_ADDR)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Software page walk, as the MMU would do it. `None` means a fault.
    fn translate(mem: &GuestMemoryMmap, cr3: u64, vaddr: u64) -> Option<u64> {
        const ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;
        let read = |addr: u64| -> u64 { mem.read_obj(GuestAddress(addr)).unwrap() };

        let pml4e = read((cr3 & ADDR_MASK) + ((vaddr >> 39) & 0x1ff) * 8);
        if pml4e & PTE_PRESENT == 0 {
            return None;
        }
        let pdpte = read((pml4e & ADDR_MASK) + ((vaddr >> 30) & 0x1ff) * 8);
        if pdpte & PTE_PRESENT == 0 {
            return None;
        }
        let pde = read((pdpte & ADDR_MASK) + ((vaddr >> 21) & 0x1ff) * 8);
        if pde & PTE_PRESENT == 0 {
            return None;
        }
        assert_ne!(pde & PTE_HUGE, 0, "boot tables only use 2 MiB pages");
        Some((pde & ADDR_MASK & !(HUGE_PAGE_SIZE - 1)) | (vaddr & (HUGE_PAGE_SIZE - 1)))
    }

    #[test]
    fn first_gigabyte_is_identity_mapped() {
        let mem: GuestMemoryMmap<()> =
            GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 1 << 20)]).unwrap();
        let cr3 = setup_identity_map(&mem).unwrap();

        for vaddr in [
            0,
            0x1000,
            0x8ff0,
            0x10_0000,
            0x1f_ffff,
            0x20_0000,
            0x400_0000,
            IDENTITY_MAP_SIZE - 1,
        ] {
            assert_eq!(translate(&mem, cr3, vaddr), Some(vaddr), "{vaddr:#x}");
        }
    }

    #[test]
    fn nothing_is_mapped_above_the_identity_map() {
        let mem: GuestMemoryMmap<()> =
            GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 1 << 20)]).unwrap();
        let cr3 = setup_identity_map(&mem).unwrap();

        assert_eq!(translate(&mem, cr3, IDENTITY_MAP_SIZE), None);
        assert_eq!(translate(&mem, cr3, 1 << 39), None);
    }
}
