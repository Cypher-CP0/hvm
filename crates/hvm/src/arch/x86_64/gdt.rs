//! Boot GDT construction.

use hypervisor::x86_64::{DescriptorTable, Segment};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

use super::layout::{BOOT_GDT_ADDR, BOOT_IDT_ADDR};
use super::Result;

/// GDT indices. Selector = index * 8.
///
/// The Linux 64-bit boot protocol requires CS = `__BOOT_CS` (0x10) and
/// DS/ES/SS = `__BOOT_DS` (0x18), so entry 1 is left unused.
const CODE_INDEX: u16 = 2;
const DATA_INDEX: u16 = 3;
const TSS_INDEX: u16 = 4;

/// Encode a segment descriptor.
///
/// `flags` packs the access byte in bits 0..8 (type, S, DPL, P) and the
/// AVL/L/DB/G nibble in bits 12..16, exactly as they sit in the descriptor.
const fn gdt_entry(flags: u16, base: u32, limit: u32) -> u64 {
    ((base as u64 & 0xff00_0000) << 32)
        | ((flags as u64 & 0xf0ff) << 40)
        | ((limit as u64 & 0x000f_0000) << 32)
        | ((base as u64 & 0x00ff_ffff) << 16)
        | (limit as u64 & 0xffff)
}

const BOOT_GDT: [u64; 5] = [
    gdt_entry(0, 0, 0),            // null
    gdt_entry(0, 0, 0),            // unused
    gdt_entry(0xa09b, 0, 0xfffff), // 64-bit code: G, L, P, S, execute/read, accessed
    gdt_entry(0xc093, 0, 0xfffff), // data: G, DB, P, S, read/write, accessed
    gdt_entry(0x808b, 0, 0xfffff), // TSS: G, P, 64-bit TSS (busy)
];

/// Decode a descriptor into the hidden segment state the CPU would load.
fn segment_from_gdt(entry: u64, index: u16) -> Segment {
    let base = ((entry >> 16) & 0x00ff_ffff) | ((entry >> 32) & 0xff00_0000);
    let raw_limit = ((entry & 0xffff) | ((entry >> 32) & 0x000f_0000)) as u32;
    let g = (entry >> 55) & 1 != 0;
    let present = (entry >> 47) & 1 != 0;
    Segment {
        base,
        // Hardware checks the scaled limit against the G bit on VM entry.
        limit: if g {
            (raw_limit << 12) | 0xfff
        } else {
            raw_limit
        },
        selector: index * 8,
        type_: ((entry >> 40) & 0xf) as u8,
        present,
        dpl: ((entry >> 45) & 0x3) as u8,
        db: (entry >> 54) & 1 != 0,
        s: (entry >> 44) & 1 != 0,
        l: (entry >> 53) & 1 != 0,
        g,
        avl: (entry >> 52) & 1 != 0,
        unusable: !present,
    }
}

/// Segment and descriptor-table state matching the boot GDT.
pub struct BootSegments {
    pub code: Segment,
    pub data: Segment,
    pub tss: Segment,
    pub gdt: DescriptorTable,
    pub idt: DescriptorTable,
}

/// Write the boot GDT and IDT into guest memory.
pub fn setup(mem: &GuestMemoryMmap) -> Result<BootSegments> {
    for (i, entry) in BOOT_GDT.iter().enumerate() {
        mem.write_obj(*entry, GuestAddress(BOOT_GDT_ADDR + i as u64 * 8))?;
    }
    mem.write_obj(0u64, GuestAddress(BOOT_IDT_ADDR))?;

    Ok(BootSegments {
        code: segment_from_gdt(BOOT_GDT[CODE_INDEX as usize], CODE_INDEX),
        data: segment_from_gdt(BOOT_GDT[DATA_INDEX as usize], DATA_INDEX),
        tss: segment_from_gdt(BOOT_GDT[TSS_INDEX as usize], TSS_INDEX),
        gdt: DescriptorTable {
            base: BOOT_GDT_ADDR,
            limit: (std::mem::size_of_val(&BOOT_GDT) - 1) as u16,
        },
        idt: DescriptorTable {
            base: BOOT_IDT_ADDR,
            limit: (std::mem::size_of::<u64>() - 1) as u16,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_encode_to_known_values() {
        assert_eq!(BOOT_GDT[0], 0);
        assert_eq!(BOOT_GDT[1], 0);
        assert_eq!(BOOT_GDT[2], 0x00af_9b00_0000_ffff);
        assert_eq!(BOOT_GDT[3], 0x00cf_9300_0000_ffff);
        assert_eq!(BOOT_GDT[4], 0x008f_8b00_0000_ffff);
    }

    #[test]
    fn tables_do_not_overlap() {
        let gdt_end = BOOT_GDT_ADDR + std::mem::size_of_val(&BOOT_GDT) as u64;
        assert!(gdt_end <= BOOT_IDT_ADDR);
    }

    #[test]
    fn base_and_limit_round_trip() {
        let seg = segment_from_gdt(gdt_entry(0x4093, 0x1234_5678, 0xabcde), 2);
        assert_eq!(seg.base, 0x1234_5678);
        assert_eq!(seg.limit, 0xabcde);
        assert!(!seg.g);
    }

    #[test]
    fn code_segment_is_64_bit() {
        let cs = segment_from_gdt(BOOT_GDT[CODE_INDEX as usize], CODE_INDEX);
        assert_eq!(cs.selector, 0x10); // __BOOT_CS
        assert_eq!(cs.type_, 0xb);
        assert!(cs.l && !cs.db && cs.s && cs.present && cs.g);
        assert_eq!(cs.limit, 0xffff_ffff);
        assert_eq!(cs.dpl, 0);
    }

    #[test]
    fn data_and_tss_segments() {
        let ds = segment_from_gdt(BOOT_GDT[DATA_INDEX as usize], DATA_INDEX);
        assert_eq!(ds.selector, 0x18); // __BOOT_DS
        assert_eq!(ds.type_, 0x3);
        assert!(ds.db && !ds.l && ds.s && ds.present);

        let tr = segment_from_gdt(BOOT_GDT[TSS_INDEX as usize], TSS_INDEX);
        assert_eq!(tr.selector, 0x20);
        assert_eq!(tr.type_, 0xb);
        assert!(!tr.s && tr.present);
    }
}
