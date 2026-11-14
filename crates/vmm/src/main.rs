//! Day 4: boot Linux with a real 16550A serial console on COM1.

mod arch;
mod devices;
mod guest;
mod platform;

use std::error::Error;
use std::fs::File;
use std::io::Write;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use hypervisor::{ExitKind, Hypervisor, Vcpu, Vm, VmExit};
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap, GuestMemoryRegion};

use arch::x86_64::layout::{HIMEM_START, MAX_MEM_SIZE};
use arch::x86_64::{linux, regs};
use platform::{Access, PioOutcome, Platform, COM1_IRQ};

const DEFAULT_MEM_MIB: u64 = 512;
/// The built-in test guest writes at 64 MiB.
const MIN_MEM_MIB: u64 = 128;

/// - `console=ttyS0`: kernel log on COM1. Registering the console replays
///   everything logged before it came up, so nothing is lost.
/// - `reboot=k panic=-1`: with no root filesystem yet the kernel panics at
///   the end of boot; make it reset through the keyboard controller straight
///   away, which the VMM treats as "stop".
/// - `pci=off`: there is no PCI bus to probe.
const DEFAULT_CMDLINE: &str = "console=ttyS0 reboot=k panic=-1 pci=off";

const USAGE: &str = "\
usage: vmm [options]

With no --kernel, runs a built-in 64-bit test guest.

options:
  --kernel <path>    Linux bzImage to boot
  --cmdline <str>    kernel command line (replaces the default)
  --mem <MiB>        guest memory size (default 512)
  -h, --help         show this help";

struct Config {
    mem_mib: u64,
    kernel: Option<String>,
    cmdline: String,
}

impl Config {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Option<Self>, String> {
        let mut cfg = Config {
            mem_mib: DEFAULT_MEM_MIB,
            kernel: None,
            cmdline: DEFAULT_CMDLINE.to_string(),
        };
        while let Some(arg) = args.next() {
            let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
            match arg.as_str() {
                "--kernel" => cfg.kernel = Some(value("--kernel")?),
                "--cmdline" => cfg.cmdline = value("--cmdline")?,
                "--mem" => {
                    let v = value("--mem")?;
                    cfg.mem_mib = v
                        .parse()
                        .map_err(|_| format!("--mem: '{v}' is not a number of MiB"))?;
                }
                "-h" | "--help" => return Ok(None),
                other => return Err(format!("unknown argument '{other}'")),
            }
        }
        let max_mib = MAX_MEM_SIZE >> 20;
        if !(MIN_MEM_MIB..=max_mib).contains(&cfg.mem_mib) {
            return Err(format!(
                "--mem must be between {MIN_MEM_MIB} and {max_mib} MiB"
            ));
        }
        Ok(Some(cfg))
    }
}

/// What to run in the guest.
enum Payload<'a> {
    /// Raw 64-bit code, loaded at 1 MiB. No interrupt controllers, so the
    /// guest ends by executing HLT.
    Raw(&'a [u8]),
    /// A Linux bzImage, booted on the full platform.
    Linux {
        image: &'a mut File,
        cmdline: &'a str,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StopReason {
    Halted,
    Reset,
}

#[derive(Default)]
struct ExitStats {
    counts: [u64; ExitKind::COUNT],
    run_time: Duration,
}

impl ExitStats {
    fn record(&mut self, kind: ExitKind) {
        self.counts[kind as usize] += 1;
    }

    fn count(&self, kind: ExitKind) -> u64 {
        self.counts[kind as usize]
    }

    fn report(&self) {
        eprintln!("--- vm exits ---");
        for kind in ExitKind::ALL {
            let n = self.count(kind);
            if n > 0 {
                eprintln!("{kind:<12} {n}");
            }
        }
        eprintln!("{:<12} {}", "total", self.counts.iter().sum::<u64>());
        eprintln!("{:<12} {:.3}s", "run time", self.run_time.as_secs_f64());
    }
}

fn report_unhandled<W: Write>(platform: &Platform<W>) {
    const SHOWN: usize = 12;
    let unhandled = platform.unhandled();
    if unhandled.is_empty() {
        return;
    }
    eprintln!("--- unhandled i/o (top {SHOWN} of {}) ---", unhandled.len());
    for (access, addr, count) in unhandled.into_iter().take(SHOWN) {
        let name = match access {
            Access::PioRead => "pio read",
            Access::PioWrite => "pio write",
            Access::MmioRead => "mmio read",
            Access::MmioWrite => "mmio write",
        };
        eprintln!("{name:<12} {addr:#8x}  x{count}");
    }
}

/// Print the registers that matter when a guest dies during early boot.
fn dump_state<V: Vcpu>(vcpu: &V) {
    eprintln!("--- vcpu state ---");
    match vcpu.get_regs() {
        Ok(r) => eprintln!(
            "rip {:#018x} rsp {:#018x} rflags {:#x}\nrax {:#018x} rbx {:#018x} rcx {:#018x} rdx {:#018x}\nrsi {:#018x} rdi {:#018x}",
            r.rip, r.rsp, r.rflags, r.rax, r.rbx, r.rcx, r.rdx, r.rsi, r.rdi
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

/// Build a VM with `mem_size` bytes of RAM, start one vCPU on `payload` in
/// long mode, and run it until it stops. Bytes the guest writes to COM1 go
/// to `serial`.
fn run_vm<H: Hypervisor>(
    hv: &H,
    mem_size: u64,
    payload: Payload<'_>,
    serial: &mut impl Write,
) -> Result<(StopReason, ExitStats), Box<dyn Error>> {
    // Guest RAM: one anonymous mmap region at guest physical address 0,
    // allocated lazily by the host. Declared before the VM so it is dropped
    // after it: the mapping must outlive every guest entry.
    let mem: GuestMemoryMmap<()> =
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), mem_size as usize)])?;

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

    // Whether the VM has interrupt controllers to deliver device IRQs to.
    let has_irqchip = matches!(payload, Payload::Linux { .. });
    let mut vcpu = match payload {
        Payload::Raw(code) => {
            mem.write_slice(code, GuestAddress(HIMEM_START))?;
            let mut vcpu = vm.create_vcpu(0)?;
            regs::setup_long_mode(&mut vcpu, &mem, HIMEM_START, 0)?;
            vcpu
        }
        Payload::Linux { image, cmdline } => {
            let kernel = linux::load_kernel(&mem, mem_size, image, cmdline)?;
            eprintln!(
                "kernel loaded at {HIMEM_START:#x}..{:#x}, entry {:#x}",
                kernel.image_end, kernel.entry
            );
            // Interrupt controllers must exist before the vCPU does.
            vm.init_platform()?;
            let mut vcpu = vm.create_vcpu(0)?;
            regs::setup_long_mode(&mut vcpu, &mem, kernel.entry, kernel.boot_params)?;
            regs::setup_lapic(&mut vcpu)?;
            vcpu
        }
    };

    let mut platform = Platform::new(serial);
    let mut com1_irq_level = false;
    let mut stats = ExitStats::default();
    let started = Instant::now();
    let result = loop {
        let exit = match vcpu.run() {
            Ok(exit) => exit,
            Err(e) => {
                dump_state(&vcpu);
                break Err(e.into());
            }
        };
        stats.record(exit.kind());
        match exit {
            VmExit::IoOut { port, data } => match platform.pio_write(port, data) {
                Ok(PioOutcome::Continue) => {}
                Ok(PioOutcome::Reset) => break Ok(StopReason::Reset),
                Err(e) => break Err(e.into()),
            },
            VmExit::IoIn { port, data } => platform.pio_read(port, data),
            VmExit::MmioWrite { addr, .. } => platform.mmio_write(addr),
            VmExit::MmioRead { addr, data } => platform.mmio_read(addr, data),
            VmExit::Interrupted => {}
            VmExit::Hlt => break Ok(StopReason::Halted),
            VmExit::Shutdown => {
                dump_state(&vcpu);
                break Err("guest shut down unexpectedly (triple fault?)".into());
            }
        }
        // Device state only changes on guest accesses for now, so this is
        // the one place the interrupt line can change.
        let level = platform.com1_interrupt();
        if has_irqchip && level != com1_irq_level {
            if let Err(e) = vm.set_irq_line(COM1_IRQ, level) {
                break Err(e.into());
            }
            com1_irq_level = level;
        }
    };
    stats.run_time = started.elapsed();
    platform.flush()?;
    report_unhandled(&platform);
    result.map(|reason| (reason, stats))
}

#[cfg(target_os = "linux")]
fn run(cfg: &Config) -> Result<(), Box<dyn Error>> {
    let hv = hypervisor::kvm::KvmHypervisor::new()?;
    eprintln!("kvm api version {}", hv.api_version());

    let mem_size = cfg.mem_mib << 20;
    let mut stdout = std::io::stdout().lock();
    let (reason, stats) = match &cfg.kernel {
        None => run_vm(&hv, mem_size, Payload::Raw(&guest::HELLO64), &mut stdout)?,
        Some(path) => {
            let mut image =
                File::open(path).map_err(|e| format!("cannot open kernel '{path}': {e}"))?;
            let payload = Payload::Linux {
                image: &mut image,
                cmdline: &cfg.cmdline,
            };
            run_vm(&hv, mem_size, payload, &mut stdout)?
        }
    };
    match reason {
        StopReason::Halted => eprintln!("guest halted"),
        StopReason::Reset => eprintln!("guest requested reset"),
    }
    stats.report();
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn run(_cfg: &Config) -> Result<(), Box<dyn Error>> {
    Err("no hypervisor backend for this OS yet; build and run inside WSL2 or Linux".into())
}

fn main() -> ExitCode {
    let cfg = match Config::parse(std::env::args().skip(1)) {
        Ok(Some(cfg)) => cfg,
        Ok(None) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(&cfg) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Option<Config>, String> {
        Config::parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn config_defaults_and_overrides() {
        let cfg = parse(&[]).unwrap().unwrap();
        assert_eq!(cfg.mem_mib, DEFAULT_MEM_MIB);
        assert!(cfg.kernel.is_none());
        assert_eq!(cfg.cmdline, DEFAULT_CMDLINE);

        let cfg = parse(&["--kernel", "bzImage", "--mem", "256", "--cmdline", "quiet"])
            .unwrap()
            .unwrap();
        assert_eq!(cfg.kernel.as_deref(), Some("bzImage"));
        assert_eq!(cfg.mem_mib, 256);
        assert_eq!(cfg.cmdline, "quiet");
    }

    #[test]
    fn config_rejects_bad_input() {
        assert!(parse(&["--help"]).unwrap().is_none());
        assert!(parse(&["--kernel"]).is_err());
        assert!(parse(&["--mem", "lots"]).is_err());
        assert!(parse(&["--mem", "16"]).is_err());
        assert!(parse(&["--mem", "4096"]).is_err());
        assert!(parse(&["--bogus"]).is_err());
    }

    /// End-to-end tests need /dev/kvm, so they skip themselves without it.
    #[cfg(target_os = "linux")]
    mod kvm {
        use super::super::*;

        fn hypervisor() -> Option<hypervisor::kvm::KvmHypervisor> {
            let hv = hypervisor::kvm::KvmHypervisor::new().ok();
            if hv.is_none() {
                eprintln!("skipping: /dev/kvm is not available");
            }
            hv
        }

        #[test]
        fn long_mode_guest_prints_and_halts() {
            let Some(hv) = hypervisor() else { return };
            let mut serial = Vec::new();
            let (reason, stats) = run_vm(
                &hv,
                MIN_MEM_MIB << 20,
                Payload::Raw(&guest::HELLO64),
                &mut serial,
            )
            .unwrap();

            assert_eq!(reason, StopReason::Halted);
            assert_eq!(serial, guest::HELLO64_OUTPUT);
            assert_eq!(
                stats.count(ExitKind::IoOut),
                guest::HELLO64_OUTPUT.len() as u64
            );
            assert_eq!(stats.count(ExitKind::Hlt), 1);
        }

        /// Boots a real kernel. Opt in with
        /// `VMM_TEST_KERNEL=/path/to/bzImage cargo test`.
        #[test]
        fn linux_kernel_boots_to_panic_and_resets() {
            let Ok(path) = std::env::var("VMM_TEST_KERNEL") else {
                eprintln!("skipping: VMM_TEST_KERNEL is not set");
                return;
            };
            let Some(hv) = hypervisor() else { return };
            let mut image = File::open(&path).unwrap();
            let mut serial = Vec::new();
            let payload = Payload::Linux {
                image: &mut image,
                cmdline: DEFAULT_CMDLINE,
            };
            let (reason, _) = run_vm(&hv, DEFAULT_MEM_MIB << 20, payload, &mut serial).unwrap();

            let log = String::from_utf8_lossy(&serial);
            assert!(log.contains("Linux version"), "no kernel banner in:\n{log}");
            assert!(
                log.contains("ttyS0 at I/O 0x3f8") && log.contains("16550A"),
                "8250 driver did not detect COM1 as a 16550A in:\n{log}"
            );
            assert_eq!(reason, StopReason::Reset);
        }
    }
}
