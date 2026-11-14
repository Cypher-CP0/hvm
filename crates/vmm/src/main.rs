//! Day 2: enter 64-bit long mode directly and run a small 64-bit program.

mod arch;
mod guest;

use std::error::Error;
use std::io::Write;
use std::process::ExitCode;

use hypervisor::{ExitKind, Hypervisor, Vcpu, Vm, VmExit};
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap, GuestMemoryRegion};

use arch::x86_64::layout::HIMEM_START;

/// Anonymous memory is allocated lazily, so this costs only what the guest
/// actually touches.
const GUEST_MEM_SIZE: usize = 128 << 20;
const COM1: u16 = 0x3f8;

#[derive(Default)]
struct ExitStats([u64; ExitKind::COUNT]);

impl ExitStats {
    fn record(&mut self, kind: ExitKind) {
        self.0[kind as usize] += 1;
    }

    fn count(&self, kind: ExitKind) -> u64 {
        self.0[kind as usize]
    }

    fn report(&self) {
        eprintln!("--- vm exits ---");
        for kind in ExitKind::ALL {
            let n = self.count(kind);
            if n > 0 {
                eprintln!("{kind:<12} {n}");
            }
        }
        eprintln!("{:<12} {}", "total", self.0.iter().sum::<u64>());
    }
}

/// Print the registers that matter when a guest dies during early boot.
fn dump_state<V: Vcpu>(vcpu: &V) {
    eprintln!("--- vcpu state ---");
    match vcpu.get_regs() {
        Ok(r) => eprintln!(
            "rip {:#018x} rsp {:#018x} rflags {:#x}\nrax {:#018x} rbx {:#018x} rcx {:#018x} rdx {:#018x}",
            r.rip, r.rsp, r.rflags, r.rax, r.rbx, r.rcx, r.rdx
        ),
        Err(e) => eprintln!("cannot read regs: {e}"),
    }
    match vcpu.get_sregs() {
        Ok(s) => eprintln!(
            "cr0 {:#x} cr2 {:#x} cr3 {:#x} cr4 {:#x} efer {:#x}\ncs  sel {:#x} base {:#x} limit {:#x} type {:#x} l {} db {}",
            s.cr0, s.cr2, s.cr3, s.cr4, s.efer,
            s.cs.selector, s.cs.base, s.cs.limit, s.cs.type_, s.cs.l as u8, s.cs.db as u8
        ),
        Err(e) => eprintln!("cannot read sregs: {e}"),
    }
}

/// Load `image` at 1 MiB, start one vCPU there in long mode, and run it until
/// it halts. Bytes the guest writes to COM1 go to `serial`.
fn run_guest<H: Hypervisor>(
    hv: &H,
    image: &[u8],
    serial: &mut impl Write,
) -> Result<ExitStats, Box<dyn Error>> {
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
    mem.write_slice(image, GuestAddress(HIMEM_START))?;

    let mut vcpu = vm.create_vcpu(0)?;
    arch::x86_64::regs::setup_long_mode(&mut vcpu, &mem, HIMEM_START)?;

    let mut stats = ExitStats::default();
    loop {
        let exit = match vcpu.run() {
            Ok(exit) => exit,
            Err(e) => {
                dump_state(&vcpu);
                return Err(e.into());
            }
        };
        stats.record(exit.kind());
        match exit {
            VmExit::IoOut { port: COM1, data } => serial.write_all(data)?,
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
            VmExit::Hlt => break,
            VmExit::Shutdown => {
                dump_state(&vcpu);
                return Err("guest shut down unexpectedly (triple fault?)".into());
            }
        }
    }
    serial.flush()?;
    Ok(stats)
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
    match run_guest(&hv, &guest::HELLO64, &mut std::io::stdout().lock()) {
        Ok(stats) => {
            eprintln!("guest halted");
            stats.report();
            ExitCode::SUCCESS
        }
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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// End-to-end: needs /dev/kvm, so it skips itself where there is none.
    #[test]
    fn long_mode_guest_prints_and_halts() {
        let Ok(hv) = hypervisor::kvm::KvmHypervisor::new() else {
            eprintln!("skipping: /dev/kvm is not available");
            return;
        };
        let mut serial = Vec::new();
        let stats = run_guest(&hv, &guest::HELLO64, &mut serial).unwrap();

        assert_eq!(serial, guest::HELLO64_OUTPUT);
        assert_eq!(
            stats.count(ExitKind::IoOut),
            guest::HELLO64_OUTPUT.len() as u64
        );
        assert_eq!(stats.count(ExitKind::Hlt), 1);
    }
}
