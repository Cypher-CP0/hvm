//! Loading a Linux bzImage and handing off via the 64-bit boot protocol.
//!
//! See Documentation/arch/x86/boot.rst in the kernel tree. In short: the
//! protected-mode part of the image is copied to guest memory, a
//! `struct boot_params` ("zero page") describes the machine, and the vCPU
//! starts in long mode 0x200 bytes into the loaded image with RSI pointing
//! at the zero page.

use std::io::{Read, Seek};

use linux_loader::cmdline::Cmdline;
use linux_loader::loader::bootparam::{boot_e820_entry, boot_params};
use linux_loader::loader::{load_cmdline, BzImage, KernelLoader};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap, ReadVolatile};

use super::layout::{
    CMDLINE_ADDR, CMDLINE_MAX_SIZE, EBDA_START, HIMEM_START, MAX_MEM_SIZE, ZERO_PAGE_ADDR,
};
use super::{Error, Result};

/// Offset of `startup_64` from the start of the loaded protected-mode image.
const ENTRY_64_OFFSET: u64 = 0x200;
/// First boot protocol version with `xloadflags`.
const MIN_BOOT_PROTOCOL: u16 = 0x020c;
/// `xloadflags`: the image has a 64-bit entry point at 0x200.
const XLF_KERNEL_64: u16 = 1 << 0;
/// `type_of_loader`: no ID assigned.
const LOADER_TYPE_UNDEFINED: u8 = 0xff;
/// Protocol default when the header does not say otherwise.
const DEFAULT_CMDLINE_SIZE: usize = 256;

const E820_RAM: u32 = 1;

pub struct LoadedKernel {
    /// Guest address of the 64-bit entry point.
    pub entry: u64,
    /// Guest address of `struct boot_params`, to be passed in RSI.
    pub boot_params: u64,
    /// One past the last byte of the loaded image.
    pub image_end: u64,
}

/// Guest RAM as the kernel should see it: `(start, size)` ranges.
///
/// Conventional memory below the EBDA, then everything from 1 MiB up. The
/// hole in between is where a PC keeps video memory and ROMs.
fn ram_ranges(mem_size: u64) -> [(u64, u64); 2] {
    debug_assert!(mem_size > HIMEM_START && mem_size <= MAX_MEM_SIZE);
    [(0, EBDA_START), (HIMEM_START, mem_size - HIMEM_START)]
}

/// Load `image` into guest memory and build its boot parameters.
pub fn load_kernel<F>(
    mem: &GuestMemoryMmap,
    mem_size: u64,
    image: &mut F,
    cmdline: &str,
) -> Result<LoadedKernel>
where
    F: Read + ReadVolatile + Seek,
{
    let himem = GuestAddress(HIMEM_START);
    let loaded = BzImage::load(mem, Some(himem), image, Some(himem))?;
    // BzImage::load always returns the header it parsed.
    let mut hdr = loaded
        .setup_header
        .expect("bzImage loader returned no setup header");

    // setup_header is packed: copy fields out before using them.
    let version = hdr.version;
    if version < MIN_BOOT_PROTOCOL {
        return Err(Error::BootProtocolTooOld {
            major: (version >> 8) as u8,
            minor: version as u8,
        });
    }
    let xloadflags = hdr.xloadflags;
    if xloadflags & XLF_KERNEL_64 == 0 {
        return Err(Error::No64BitEntry);
    }

    // The kernel decompresses itself to its preferred address (or to where
    // we loaded it, if that is higher) and needs `init_size` bytes there.
    let pref_address = hdr.pref_address;
    let init_size = hdr.init_size as u64;
    let needed = (pref_address.max(HIMEM_START) + init_size).max(loaded.kernel_end);
    if needed > mem_size {
        return Err(Error::NotEnoughMemory {
            needed_mib: needed.div_ceil(1 << 20),
            available_mib: mem_size >> 20,
        });
    }

    // Command line. `cmdline_size` is the longest string the kernel accepts,
    // not counting the terminating NUL.
    let max_len = match hdr.cmdline_size as usize {
        0 => DEFAULT_CMDLINE_SIZE,
        n => n,
    };
    let capacity = (max_len + 1).min(CMDLINE_MAX_SIZE);
    let cmdline = Cmdline::try_from(cmdline, capacity)?;
    load_cmdline(mem, GuestAddress(CMDLINE_ADDR), &cmdline)?;

    // Zero page: the image's own setup header with the loader fields filled
    // in, plus the memory map.
    hdr.type_of_loader = LOADER_TYPE_UNDEFINED;
    hdr.cmd_line_ptr = CMDLINE_ADDR as u32;

    let mut params = boot_params {
        hdr,
        ..Default::default()
    };
    let ranges = ram_ranges(mem_size);
    for (slot, (addr, size)) in ranges.into_iter().enumerate() {
        params.e820_table[slot] = boot_e820_entry {
            addr,
            size,
            r#type: E820_RAM,
        };
    }
    params.e820_entries = ranges.len() as u8;
    mem.write_obj(params, GuestAddress(ZERO_PAGE_ADDR))?;

    Ok(LoadedKernel {
        entry: loaded.kernel_load.0 + ENTRY_64_OFFSET,
        boot_params: ZERO_PAGE_ADDR,
        image_end: loaded.kernel_end,
    })
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    const MEM_SIZE: u64 = 64 << 20;
    const PAYLOAD: &[u8] = b"not really a compressed kernel";

    // Field offsets from Documentation/arch/x86/boot.rst and zero-page.rst,
    // written out here so the test checks the layout independently of the
    // generated structs.
    const OFF_E820_ENTRIES: usize = 0x1e8;
    const OFF_SETUP_SECTS: usize = 0x1f1;
    const OFF_BOOT_FLAG: usize = 0x1fe;
    const OFF_HEADER: usize = 0x202;
    const OFF_VERSION: usize = 0x206;
    const OFF_TYPE_OF_LOADER: usize = 0x210;
    const OFF_LOADFLAGS: usize = 0x211;
    const OFF_CODE32_START: usize = 0x214;
    const OFF_CMD_LINE_PTR: usize = 0x228;
    const OFF_XLOADFLAGS: usize = 0x236;
    const OFF_CMDLINE_SIZE: usize = 0x238;
    const OFF_PREF_ADDRESS: usize = 0x258;
    const OFF_INIT_SIZE: usize = 0x260;
    const OFF_E820_TABLE: usize = 0x2d0;

    struct FakeImage {
        version: u16,
        xloadflags: u16,
        init_size: u32,
    }

    impl Default for FakeImage {
        fn default() -> Self {
            Self {
                version: 0x020f,
                xloadflags: XLF_KERNEL_64,
                init_size: 8 << 20,
            }
        }
    }

    impl FakeImage {
        /// A bzImage with one setup sector after the boot sector, followed
        /// by `PAYLOAD` as the protected-mode part.
        fn build(&self) -> Cursor<Vec<u8>> {
            let mut img = vec![0u8; 2 * 512];
            img[OFF_SETUP_SECTS] = 1;
            img[OFF_BOOT_FLAG..][..2].copy_from_slice(&0xaa55u16.to_le_bytes());
            img[OFF_HEADER..][..4].copy_from_slice(b"HdrS");
            img[OFF_VERSION..][..2].copy_from_slice(&self.version.to_le_bytes());
            img[OFF_LOADFLAGS] = 0x01; // LOADED_HIGH
            img[OFF_CODE32_START..][..4].copy_from_slice(&0x10_0000u32.to_le_bytes());
            img[OFF_XLOADFLAGS..][..2].copy_from_slice(&self.xloadflags.to_le_bytes());
            img[OFF_CMDLINE_SIZE..][..4].copy_from_slice(&2047u32.to_le_bytes());
            img[OFF_PREF_ADDRESS..][..8].copy_from_slice(&0x100_0000u64.to_le_bytes());
            img[OFF_INIT_SIZE..][..4].copy_from_slice(&self.init_size.to_le_bytes());
            img.extend_from_slice(PAYLOAD);
            Cursor::new(img)
        }
    }

    fn guest_memory() -> GuestMemoryMmap {
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), MEM_SIZE as usize)]).unwrap()
    }

    fn read<const N: usize>(mem: &GuestMemoryMmap, addr: u64) -> [u8; N] {
        let mut buf = [0u8; N];
        mem.read_slice(&mut buf, GuestAddress(addr)).unwrap();
        buf
    }

    #[test]
    fn loads_image_and_reports_entry() {
        let mem = guest_memory();
        let k = load_kernel(&mem, MEM_SIZE, &mut FakeImage::default().build(), "quiet").unwrap();

        assert_eq!(k.entry, 0x10_0200);
        assert_eq!(k.boot_params, ZERO_PAGE_ADDR);
        assert_eq!(k.image_end, HIMEM_START + PAYLOAD.len() as u64);
        assert_eq!(&read::<30>(&mem, HIMEM_START), PAYLOAD);
    }

    #[test]
    fn zero_page_has_header_loader_fields_and_cmdline() {
        let mem = guest_memory();
        let cmdline = "console=ttyS0 panic=-1";
        load_kernel(&mem, MEM_SIZE, &mut FakeImage::default().build(), cmdline).unwrap();
        let zp = |off: usize| ZERO_PAGE_ADDR + off as u64;

        // Copied from the image.
        assert_eq!(&read::<4>(&mem, zp(OFF_HEADER)), b"HdrS");
        assert_eq!(u16::from_le_bytes(read(&mem, zp(OFF_VERSION))), 0x020f);
        assert_eq!(u16::from_le_bytes(read(&mem, zp(OFF_BOOT_FLAG))), 0xaa55);
        assert_eq!(u32::from_le_bytes(read(&mem, zp(OFF_CMDLINE_SIZE))), 2047);
        // Filled in by us.
        assert_eq!(read::<1>(&mem, zp(OFF_TYPE_OF_LOADER)), [0xff]);
        assert_eq!(
            u32::from_le_bytes(read(&mem, zp(OFF_CMD_LINE_PTR))) as u64,
            CMDLINE_ADDR
        );

        let mut stored = vec![0u8; cmdline.len() + 1];
        mem.read_slice(&mut stored, GuestAddress(CMDLINE_ADDR))
            .unwrap();
        assert_eq!(&stored[..cmdline.len()], cmdline.as_bytes());
        assert_eq!(stored[cmdline.len()], 0);
    }

    #[test]
    fn e820_map_describes_guest_ram() {
        let mem = guest_memory();
        load_kernel(&mem, MEM_SIZE, &mut FakeImage::default().build(), "").unwrap();
        let zp = |off: usize| ZERO_PAGE_ADDR + off as u64;

        assert_eq!(read::<1>(&mem, zp(OFF_E820_ENTRIES)), [2]);
        // Each entry is packed: u64 addr, u64 size, u32 type.
        let entry = |i: usize| {
            let base = zp(OFF_E820_TABLE + i * 20);
            (
                u64::from_le_bytes(read(&mem, base)),
                u64::from_le_bytes(read(&mem, base + 8)),
                u32::from_le_bytes(read(&mem, base + 16)),
            )
        };
        assert_eq!(entry(0), (0, EBDA_START, E820_RAM));
        assert_eq!(entry(1), (HIMEM_START, MEM_SIZE - HIMEM_START, E820_RAM));
        assert_eq!(entry(2), (0, 0, 0));
    }

    #[test]
    fn rejects_kernels_it_cannot_boot() {
        let mem = guest_memory();

        let old = FakeImage {
            version: 0x020b,
            ..Default::default()
        };
        assert!(matches!(
            load_kernel(&mem, MEM_SIZE, &mut old.build(), ""),
            Err(Error::BootProtocolTooOld {
                major: 2,
                minor: 11
            })
        ));

        let no64 = FakeImage {
            xloadflags: 0,
            ..Default::default()
        };
        assert!(matches!(
            load_kernel(&mem, MEM_SIZE, &mut no64.build(), ""),
            Err(Error::No64BitEntry)
        ));

        // 16 MiB preferred address + 64 MiB init_size does not fit in 64 MiB.
        let big = FakeImage {
            init_size: 64 << 20,
            ..Default::default()
        };
        assert!(matches!(
            load_kernel(&mem, MEM_SIZE, &mut big.build(), ""),
            Err(Error::NotEnoughMemory {
                needed_mib: 80,
                available_mib: 64
            })
        ));

        let garbage = &mut Cursor::new(vec![0u8; 4096]);
        assert!(matches!(
            load_kernel(&mem, MEM_SIZE, garbage, ""),
            Err(Error::KernelLoad(_))
        ));
    }
}
