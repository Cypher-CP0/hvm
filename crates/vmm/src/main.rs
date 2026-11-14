//! Day 1: create a VM, give it memory and one vCPU, run a few bytes of
//! real-mode code that write to the serial port and halt.

use std::io::Write;
use std::process::ExitCode;

use hypervisor::{ExitKind, Hypervisor, Vcpu, Vm, VmExit};
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap, GuestMemoryRegion};

const GUEST_MEM_SIZE: usize = 2 << 20; // 2 MiB is plenty for a real-mode stub
const CODE_ADDR: u16 = 0x1000;
const COM1: u16 = 0x3f8;

/// Hand-assembled 16-bit guest: print a NUL-terminated string to COM1, halt.
///
/// ```text
/// 1000: ba f8 03    mov  dx, 0x3f8
/// 1003: be 0f 10    mov  si, 0x100f      ; &msg
/// 1006: ac          lodsb                ; al = [si++]
/// 1007: 84 c0       test al, al
/// 1009: 74 03       jz   0x100e
/// 100b: ee          out  dx, al
/// 100c: eb f8       jmp  0x1006
/// 100e: f4          hlt
/// 100f: msg
/// ```
fn guest_code() -> Vec<u8> {
    let mut code = vec![
        0xba, 0xf8, 0x03, 0xbe, 0x0f, 0x10, 0xac, 0x84, 0xc0, 0x74, 0x03, 0xee, 0xeb, 0xf8, 0xf4,
    ];
    code.extend_from_slice(b"Hello from the guest!\n\0");
    code
}

#[derive(Default)]
struct ExitStats([u64; ExitKind::COUNT]);

impl ExitStats {
    fn record(&mut self, kind: ExitKind) {
        self.0[kind as usize] += 1;
    }

    fn report(&self) {
        eprintln!("--- vm exits ---");
        for kind in ExitKind::ALL {
            let n = self.0[kind as usize];
            if n > 0 {
                eprintln!("{kind:<12} {n}");
            }
        }
        eprintln!("{:<12} {}", "total", self.0.iter().sum::<u64>());
    }
}

fn run<H: Hypervisor>(hv: &H) -> Result<(), Box<dyn std::error::Error>> {
    // Guest RAM: one anonymous mmap region at guest physical address 0.
    // Declared before the VM so it is dropped after it: the mapping must
    // outlive every guest entry.
    let mem: GuestMemoryMmap<()> =
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), GUEST_MEM_SIZE)])?;

    let vm = hv.create_vm()?;
    for (slot, region) in mem.iter().enumerate() {
        // SAFETY: `mem` outlives `vm` (see above) and is never remapped.
        unsafe {
            vm.map_memory(
                slot as u32,
                region.start_addr().0,
                region.as_ptr(),
                region.len(),
            )?;
        }
    }
    mem.write_slice(&guest_code(), GuestAddress(CODE_ADDR as u64))?;

    let mut vcpu = vm.create_vcpu(0)?;
    vcpu.setup_real_mode(CODE_ADDR)?;

    let mut stats = ExitStats::default();
    let mut stdout = std::io::stdout().lock();
    loop {
        let exit = vcpu.run()?;
        stats.record(exit.kind());
        match exit {
            VmExit::IoOut { port: COM1, data } => stdout.write_all(data)?,
            VmExit::IoOut { port, data } => {
                eprintln!("unhandled pio write: port {port:#x} data {data:x?}")
            }
            VmExit::IoIn { port, data } => {
                eprintln!("unhandled pio read: port {port:#x}");
                data.fill(0xff); // floating bus
            }
            VmExit::MmioWrite { addr, data } => {
                eprintln!("unhandled mmio write: addr {addr:#x} data {data:x?}")
            }
            VmExit::MmioRead { addr, data } => {
                eprintln!("unhandled mmio read: addr {addr:#x}");
                data.fill(0xff);
            }
            VmExit::Interrupted => continue,
            VmExit::Hlt => {
                eprintln!("guest halted");
                break;
            }
            VmExit::Shutdown => {
                eprintln!("guest shut down (triple fault?)");
                break;
            }
        }
    }
    stdout.flush()?;
    stats.report();
    Ok(())
}

#[cfg(target_os = "linux")]
fn main() -> ExitCode {
    let hv = match hypervisor::kvm::KvmHypervisor::new() {
        Ok(hv) => hv,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!("kvm api version {}", hv.api_version());
    match run(&hv) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() -> ExitCode {
    eprintln!("no hypervisor backend for this OS yet; build and run inside WSL2 or Linux");
    ExitCode::FAILURE
}
