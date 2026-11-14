//! tvm: a virtual machine monitor for Linux guests on KVM.

mod arch;
#[cfg(unix)]
mod console;
mod devices;
mod guest;
mod metrics;
mod platform;

use std::error::Error;
use std::fs::File;
use std::io::{self, Read, Write};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use hypervisor::{Hypervisor, IrqLine, Vcpu, Vm, VmExit};
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap, GuestMemoryRegion};

use arch::x86_64::layout::{HIMEM_START, MAX_MEM_SIZE};
use arch::x86_64::{linux, regs};
use metrics::{BootWatch, ExitStats, Timeline};
use platform::{Access, Com1, IoCount, PioOutcome, Platform, COM1_IRQ};

const DEFAULT_MEM_MIB: u64 = 512;
/// The built-in test guest writes at 64 MiB.
const MIN_MEM_MIB: u64 = 128;

/// - `console=ttyS0`: kernel log, and the console of the first process, on
///   COM1. Registering the console replays everything logged before it came
///   up, so nothing is lost.
/// - `reboot=k panic=-1`: reset through the keyboard controller, which the
///   VMM treats as "stop", and do so at once on a panic. Without an initrd
///   that is how every boot ends: there is no root filesystem to mount.
/// - `pci=off`: there is no PCI bus to probe.
const DEFAULT_CMDLINE: &str = "console=ttyS0 reboot=k panic=-1 pci=off";

/// How often the input thread re-offers bytes the UART had no room for.
/// Polling is crude but only happens while the guest is behind on input; it
/// goes away with the event loop in week 2.
const INPUT_RETRY_INTERVAL: Duration = Duration::from_millis(1);

/// Exits per measurement in `--bench-exits`.
const BENCH_ITERATIONS: u32 = 100_000;
/// How many of the busiest I/O addresses to list at exit.
const IO_REPORT_ROWS: usize = 10;

const USAGE: &str = "\
usage: vmm [options]

With no --kernel, runs a built-in 64-bit test guest.

options:
  --kernel <path>    Linux bzImage to boot
  --initrd <path>    initial ramdisk to pass to the kernel
  --cmdline <str>    kernel command line (replaces the default)
  --append <str>     extra kernel command line options, added to the end
  --mem <MiB>        guest memory size (default 512)
  --bench-exits      measure the cost of a VM exit and stop
  -h, --help         show this help

While a kernel is running the terminal belongs to the guest:
  Ctrl-A x           quit the VMM
  Ctrl-A Ctrl-A      send Ctrl-A to the guest";

struct Config {
    mem_mib: u64,
    kernel: Option<String>,
    initrd: Option<String>,
    cmdline: String,
    bench_exits: bool,
}

impl Config {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Option<Self>, String> {
        let mut cfg = Config {
            mem_mib: DEFAULT_MEM_MIB,
            kernel: None,
            initrd: None,
            cmdline: DEFAULT_CMDLINE.to_string(),
            bench_exits: false,
        };
        let mut append = Vec::new();
        while let Some(arg) = args.next() {
            let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
            match arg.as_str() {
                "--kernel" => cfg.kernel = Some(value("--kernel")?),
                "--initrd" => cfg.initrd = Some(value("--initrd")?),
                "--cmdline" => cfg.cmdline = value("--cmdline")?,
                "--append" => append.push(value("--append")?),
                "--mem" => {
                    let v = value("--mem")?;
                    cfg.mem_mib = v
                        .parse()
                        .map_err(|_| format!("--mem: '{v}' is not a number of MiB"))?;
                }
                "--bench-exits" => cfg.bench_exits = true,
                "-h" | "--help" => return Ok(None),
                other => return Err(format!("unknown argument '{other}'")),
            }
        }
        for extra in append {
            cfg.cmdline.push(' ');
            cfg.cmdline.push_str(&extra);
        }
        if cfg.initrd.is_some() && cfg.kernel.is_none() {
            return Err("--initrd needs --kernel".to_string());
        }
        if cfg.bench_exits && cfg.kernel.is_some() {
            return Err("--bench-exits runs its own guest; drop --kernel".to_string());
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
        initrd: Option<&'a mut File>,
        cmdline: &'a str,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StopReason {
    Halted,
    Reset,
}

/// Everything a finished VM run has to report.
struct RunOutcome {
    reason: StopReason,
    stats: ExitStats,
    /// Addresses the guest accessed, most frequent first.
    io: Vec<IoCount>,
}

fn report_io(io: &[IoCount]) {
    if io.is_empty() {
        return;
    }
    let unhandled = io.iter().filter(|c| !c.handled).count();
    eprintln!(
        "--- i/o by address (top {IO_REPORT_ROWS} of {}, {unhandled} unhandled) ---",
        io.len()
    );
    for c in io.iter().take(IO_REPORT_ROWS) {
        let name = match c.access {
            Access::PioRead => "pio read",
            Access::PioWrite => "pio write",
            Access::MmioRead => "mmio read",
            Access::MmioWrite => "mmio write",
        };
        let note = if c.handled { "" } else { "  unhandled" };
        eprintln!("{name:<12} {:#8x} {:>9}{note}", c.addr, c.count);
    }
    // Unhandled accesses are worth seeing even when they are rare.
    for c in io.iter().skip(IO_REPORT_ROWS).filter(|c| !c.handled) {
        let name = match c.access {
            Access::PioRead => "pio read",
            Access::PioWrite => "pio write",
            Access::MmioRead => "mmio read",
            Access::MmioWrite => "mmio write",
        };
        eprintln!("{name:<12} {:#8x} {:>9}  unhandled", c.addr, c.count);
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

/// Start a thread that carries bytes from `input` to the guest's COM1.
///
/// The thread ends with the input, or with the process: it is never joined,
/// because it may be blocked reading a terminal.
fn spawn_console_input<W>(
    mut input: Box<dyn Read + Send>,
    com1: Arc<Mutex<Com1<W>>>,
) -> io::Result<()>
where
    W: Write + Send + 'static,
{
    thread::Builder::new()
        .name("console-input".into())
        .spawn(move || {
            let mut buf = [0u8; 256];
            loop {
                let n = match input.read(&mut buf) {
                    Ok(0) => return,
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        eprintln!("console input stopped: {e}");
                        return;
                    }
                };
                // The UART takes at most a FIFO's worth at a time. Offer the
                // rest again once the guest has had a chance to read.
                let mut pending = &buf[..n];
                while !pending.is_empty() {
                    match com1.lock().unwrap().receive(pending) {
                        Ok(accepted) => pending = &pending[accepted..],
                        Err(e) => {
                            eprintln!("console input stopped: {e}");
                            return;
                        }
                    }
                    if !pending.is_empty() {
                        thread::sleep(INPUT_RETRY_INTERVAL);
                    }
                }
            }
        })?;
    Ok(())
}

/// Build a VM with `mem_size` bytes of RAM, start one vCPU on `payload` in
/// long mode, and run it until it stops.
///
/// Bytes the guest writes to COM1 go to `serial`; bytes read from `input`
/// arrive on COM1. Milestones are recorded on `timeline`.
fn run_vm<H, W>(
    hv: &H,
    mem_size: u64,
    payload: Payload<'_>,
    serial: W,
    input: Option<Box<dyn Read + Send>>,
    timeline: &Timeline,
) -> Result<RunOutcome, Box<dyn Error>>
where
    H: Hypervisor,
    W: Write + Send + 'static,
{
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

    timeline.mark(metrics::EVENT_VM_CREATED);

    // The interrupt line for COM1, if the VM has interrupt controllers.
    let mut com1_irq: Option<Box<dyn IrqLine>> = None;
    let mut vcpu = match payload {
        Payload::Raw(code) => {
            mem.write_slice(code, GuestAddress(HIMEM_START))?;
            let mut vcpu = vm.create_vcpu(0)?;
            regs::setup_long_mode(&mut vcpu, &mem, HIMEM_START, 0)?;
            vcpu
        }
        Payload::Linux {
            image,
            initrd,
            cmdline,
        } => {
            let kernel = linux::load_kernel(&mem, mem_size, image, initrd, cmdline)?;
            eprintln!(
                "kernel loaded at {HIMEM_START:#x}..{:#x}, entry {:#x}",
                kernel.image_end, kernel.entry
            );
            if let Some((addr, size)) = kernel.initrd {
                eprintln!("initrd loaded at {addr:#x}..{:#x}", addr + size);
            }
            // Interrupt controllers must exist before the vCPU does.
            vm.init_platform()?;
            com1_irq = Some(Box::new(vm.irq_line(COM1_IRQ)?));
            let mut vcpu = vm.create_vcpu(0)?;
            regs::setup_long_mode(&mut vcpu, &mem, kernel.entry, kernel.boot_params)?;
            regs::setup_lapic(&mut vcpu)?;
            vcpu
        }
    };

    timeline.mark(metrics::EVENT_GUEST_LOADED);

    let serial = BootWatch::new(serial, timeline.clone());
    let mut platform = Platform::new(serial, com1_irq);
    if let Some(input) = input {
        spawn_console_input(input, platform.com1())?;
    }

    let mut stats = ExitStats::default();
    timeline.mark(metrics::EVENT_VCPU_STARTED);
    let started = Instant::now();
    let result: Result<StopReason, Box<dyn Error>> = loop {
        let entered = Instant::now();
        let exit = match vcpu.run() {
            Ok(exit) => exit,
            Err(e) => {
                dump_state(&vcpu);
                break Err(e.into());
            }
        };
        let exited = Instant::now();
        let kind = exit.kind();
        // `Ok(None)` means carry on.
        let step: Result<Option<StopReason>, Box<dyn Error>> = match exit {
            VmExit::IoOut { port, data } => match platform.pio_write(port, data) {
                Ok(PioOutcome::Continue) => Ok(None),
                Ok(PioOutcome::Reset) => Ok(Some(StopReason::Reset)),
                Err(e) => Err(e.into()),
            },
            VmExit::IoIn { port, data } => platform
                .pio_read(port, data)
                .map(|()| None)
                .map_err(Into::into),
            VmExit::MmioWrite { addr, .. } => {
                platform.mmio_write(addr);
                Ok(None)
            }
            VmExit::MmioRead { addr, data } => {
                platform.mmio_read(addr, data);
                Ok(None)
            }
            VmExit::Interrupted => Ok(None),
            VmExit::Hlt => Ok(Some(StopReason::Halted)),
            VmExit::Shutdown => {
                dump_state(&vcpu);
                Err("guest shut down unexpectedly (triple fault?)".into())
            }
        };
        stats.record(kind, exited - entered, exited.elapsed());
        match step {
            Ok(None) => {}
            Ok(Some(reason)) => break Ok(reason),
            Err(e) => break Err(e),
        }
    };
    stats.run_time = started.elapsed();
    timeline.mark(metrics::EVENT_STOPPED);
    platform.flush()?;
    Ok(RunOutcome {
        reason: result?,
        stats,
        io: platform.io_counts(),
    })
}

/// Measure what a VM exit costs on this host, with two guests that do
/// nothing but exit: once to the hypervisor only, once all the way to the
/// VMM and back.
fn bench_exits<H: Hypervisor>(hv: &H) -> Result<(), Box<dyn Error>> {
    let n = BENCH_ITERATIONS;
    let cases = [
        ("hypervisor only (cpuid)", guest::cpuid_loop(n), 0),
        ("to the vmm (port write)", guest::pio_loop(n), n as u64),
    ];
    println!("vm exit round trip, {n} exits each:");
    for (name, code, expected_vmm_exits) in cases {
        let timeline = Timeline::new(Instant::now());
        let outcome = run_vm(
            hv,
            MIN_MEM_MIB << 20,
            Payload::Raw(&code),
            io::sink(),
            None,
            &timeline,
        )?;
        // Each guest ends with one HLT exit on top of what it measures.
        let vmm_exits = outcome.stats.total() - 1;
        if outcome.reason != StopReason::Halted || vmm_exits != expected_vmm_exits {
            return Err(format!(
                "{name}: guest did not behave as expected ({:?}, {vmm_exits} exits to the vmm)",
                outcome.reason
            )
            .into());
        }
        println!(
            "  {name:<26} {:>9} each",
            metrics::human(outcome.stats.run_time / n)
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn run(cfg: &Config, timeline: &Timeline) -> Result<(), Box<dyn Error>> {
    let hv = hypervisor::kvm::KvmHypervisor::new()?;
    eprintln!("kvm api version {}", hv.api_version());
    if cfg.bench_exits {
        return bench_exits(&hv);
    }

    let mem_size = cfg.mem_mib << 20;
    let open = |what: &str, path: &str| {
        File::open(path).map_err(|e| format!("cannot open {what} '{path}': {e}"))
    };
    let outcome = match &cfg.kernel {
        None => run_vm(
            &hv,
            mem_size,
            Payload::Raw(&guest::HELLO64),
            io::stdout(),
            None,
            timeline,
        )?,
        Some(path) => {
            let mut image = open("kernel", path)?;
            let mut initrd = match &cfg.initrd {
                Some(path) => Some(open("initrd", path)?),
                None => None,
            };
            let payload = Payload::Linux {
                image: &mut image,
                initrd: initrd.as_mut(),
                cmdline: &cfg.cmdline,
            };
            let output = console::TerminalOutput::new()?;
            let input = console::TerminalInput::new(io::stdin());
            // Hand the terminal to the guest for as long as it runs.
            let _raw_mode = console::enter_raw_mode()?;
            run_vm(
                &hv,
                mem_size,
                payload,
                output,
                Some(Box::new(input)),
                timeline,
            )?
        }
    };
    match outcome.reason {
        StopReason::Halted => eprintln!("guest halted"),
        StopReason::Reset => eprintln!("guest requested reset"),
    }
    timeline.report();
    outcome.stats.report();
    report_io(&outcome.io);
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn run(_cfg: &Config, _timeline: &Timeline) -> Result<(), Box<dyn Error>> {
    Err("no hypervisor backend for this OS yet; build and run inside WSL2 or Linux".into())
}

fn main() -> ExitCode {
    // Time zero for the boot timeline: as early as we can observe.
    let timeline = Timeline::new(Instant::now());
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
    match run(&cfg, &timeline) {
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
        assert!(cfg.initrd.is_none());
        assert!(!cfg.bench_exits);
        assert_eq!(cfg.cmdline, DEFAULT_CMDLINE);
        assert!(parse(&["--bench-exits"]).unwrap().unwrap().bench_exits);

        let cfg = parse(&[
            "--kernel",
            "bzImage",
            "--initrd",
            "rd.img",
            "--mem",
            "256",
            "--cmdline",
            "quiet",
        ])
        .unwrap()
        .unwrap();
        assert_eq!(cfg.kernel.as_deref(), Some("bzImage"));
        assert_eq!(cfg.initrd.as_deref(), Some("rd.img"));
        assert_eq!(cfg.mem_mib, 256);
        assert_eq!(cfg.cmdline, "quiet");
    }

    #[test]
    fn config_append_extends_the_command_line() {
        let cfg = parse(&["--append", "rdinit=/bin/sh", "--append", "quiet"])
            .unwrap()
            .unwrap();
        assert_eq!(
            cfg.cmdline,
            format!("{DEFAULT_CMDLINE} rdinit=/bin/sh quiet")
        );

        // Order on the command line does not matter: appended options
        // always follow --cmdline.
        let cfg = parse(&["--append", "b", "--cmdline", "a"])
            .unwrap()
            .unwrap();
        assert_eq!(cfg.cmdline, "a b");
    }

    #[test]
    fn config_rejects_bad_input() {
        assert!(parse(&["--help"]).unwrap().is_none());
        assert!(parse(&["--kernel"]).is_err());
        assert!(
            parse(&["--initrd", "rd.img"]).is_err(),
            "initrd without kernel"
        );
        assert!(parse(&["--mem", "lots"]).is_err());
        assert!(parse(&["--mem", "16"]).is_err());
        assert!(parse(&["--mem", "4096"]).is_err());
        assert!(parse(&["--bench-exits", "--kernel", "bzImage"]).is_err());
        assert!(parse(&["--bogus"]).is_err());
    }

    /// End-to-end tests need /dev/kvm, so they skip themselves without it.
    #[cfg(target_os = "linux")]
    mod kvm {
        use super::super::*;
        use hypervisor::ExitKind;

        /// Something has gone wrong if a guest runs for this long. There is
        /// no way to stop a running vCPU yet, so give up on the whole test
        /// process rather than hang it.
        const GUEST_TIMEOUT: Duration = Duration::from_secs(120);

        fn start_watchdog(test: &'static str) {
            thread::spawn(move || {
                thread::sleep(GUEST_TIMEOUT);
                eprintln!("{test}: guest still running after {GUEST_TIMEOUT:?}, aborting");
                std::process::exit(101);
            });
        }

        fn hypervisor() -> Option<hypervisor::kvm::KvmHypervisor> {
            let hv = hypervisor::kvm::KvmHypervisor::new().ok();
            if hv.is_none() {
                eprintln!("skipping: /dev/kvm is not available");
            }
            hv
        }

        /// Serial output that the test can read back while the guest runs.
        #[derive(Clone, Default)]
        struct SharedBuf(Arc<Mutex<Vec<u8>>>);

        impl SharedBuf {
            fn contents(&self) -> String {
                String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
            }
        }

        impl Write for SharedBuf {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        /// Console input that waits for a shell prompt, then types `script`
        /// once.
        struct TypeAtPrompt {
            output: SharedBuf,
            /// Output that marks the start of user space. The kernel log
            /// before it could contain the prompt text by chance.
            after: &'static str,
            prompt: &'static str,
            script: Option<&'static [u8]>,
        }

        impl TypeAtPrompt {
            fn prompt_shown(&self) -> bool {
                let output = self.output.contents();
                output
                    .split_once(self.after)
                    .is_some_and(|(_, rest)| rest.contains(self.prompt))
            }
        }

        impl Read for TypeAtPrompt {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let Some(script) = self.script.take() else {
                    return Ok(0);
                };
                while !self.prompt_shown() {
                    thread::sleep(Duration::from_millis(10));
                }
                // Let the shell finish setting up its terminal.
                thread::sleep(Duration::from_millis(100));
                buf[..script.len()].copy_from_slice(script);
                Ok(script.len())
            }
        }

        #[test]
        fn long_mode_guest_prints_and_halts() {
            let Some(hv) = hypervisor() else { return };
            let serial = SharedBuf::default();
            let timeline = Timeline::new(Instant::now());
            let RunOutcome { reason, stats, io } = run_vm(
                &hv,
                MIN_MEM_MIB << 20,
                Payload::Raw(&guest::HELLO64),
                serial.clone(),
                None,
                &timeline,
            )
            .unwrap();

            assert_eq!(reason, StopReason::Halted);
            assert!(timeline.get(metrics::EVENT_FIRST_OUTPUT).is_some());
            assert!(timeline.get(metrics::EVENT_USERSPACE).is_none());
            // One address, COM1's data register, written once per byte.
            assert_eq!(io.len(), 1);
            assert_eq!(io[0].count, guest::HELLO64_OUTPUT.len() as u64);
            assert_eq!(serial.0.lock().unwrap().as_slice(), guest::HELLO64_OUTPUT);
            assert_eq!(
                stats.count(ExitKind::IoOut),
                guest::HELLO64_OUTPUT.len() as u64
            );
            assert_eq!(stats.count(ExitKind::Hlt), 1);
        }

        /// Boots a real kernel with no root filesystem. Opt in with
        /// `VMM_TEST_KERNEL=/path/to/bzImage cargo test`.
        #[test]
        fn linux_kernel_boots_to_panic_and_resets() {
            let Ok(path) = std::env::var("VMM_TEST_KERNEL") else {
                eprintln!("skipping: VMM_TEST_KERNEL is not set");
                return;
            };
            let Some(hv) = hypervisor() else { return };
            start_watchdog("linux_kernel_boots_to_panic_and_resets");
            let mut image = File::open(&path).unwrap();
            let serial = SharedBuf::default();
            let payload = Payload::Linux {
                image: &mut image,
                initrd: None,
                cmdline: DEFAULT_CMDLINE,
            };
            let timeline = Timeline::new(Instant::now());
            let outcome = run_vm(
                &hv,
                DEFAULT_MEM_MIB << 20,
                payload,
                serial.clone(),
                None,
                &timeline,
            )
            .unwrap();
            let reason = outcome.reason;

            let log = serial.contents();
            assert!(log.contains("Linux version"), "no kernel banner in:\n{log}");
            assert!(
                log.contains("ttyS0 at I/O 0x3f8") && log.contains("16550A"),
                "8250 driver did not detect COM1 as a 16550A in:\n{log}"
            );
            assert!(
                log.contains("serio: i8042 KBD port"),
                "keyboard controller probe did not succeed in:\n{log}"
            );
            assert_eq!(reason, StopReason::Reset);
        }

        /// The benchmark guests must do exactly the exits they claim to.
        #[test]
        fn benchmark_guests_exit_as_designed() {
            let Some(hv) = hypervisor() else { return };
            let run = |code: &[u8]| {
                let timeline = Timeline::new(Instant::now());
                run_vm(
                    &hv,
                    MIN_MEM_MIB << 20,
                    Payload::Raw(code),
                    io::sink(),
                    None,
                    &timeline,
                )
                .unwrap()
            };

            let pio = run(&guest::pio_loop(1000));
            assert_eq!(pio.reason, StopReason::Halted);
            assert_eq!(pio.stats.count(ExitKind::IoOut), 1000);
            assert_eq!(pio.stats.total(), 1001);

            let cpuid = run(&guest::cpuid_loop(1000));
            assert_eq!(cpuid.reason, StopReason::Halted);
            assert_eq!(cpuid.stats.total(), 1, "CPUID must not reach the VMM");
        }

        /// Boots to a shell in the initrd, types a command and reboots,
        /// which exercises console input and both UART interrupts. Opt in
        /// with `VMM_TEST_KERNEL` and `VMM_TEST_INITRD`; the initrd must
        /// contain a busybox-style `/bin/sh`.
        #[test]
        fn linux_shell_accepts_input_and_reboots() {
            let (Ok(kernel), Ok(initrd)) = (
                std::env::var("VMM_TEST_KERNEL"),
                std::env::var("VMM_TEST_INITRD"),
            ) else {
                eprintln!("skipping: VMM_TEST_KERNEL and VMM_TEST_INITRD are not both set");
                return;
            };
            let Some(hv) = hypervisor() else { return };
            start_watchdog("linux_shell_accepts_input_and_reboots");
            let mut image = File::open(&kernel).unwrap();
            let mut initrd = File::open(&initrd).unwrap();
            let serial = SharedBuf::default();
            let cmdline = format!("{DEFAULT_CMDLINE} rdinit=/bin/sh");
            let payload = Payload::Linux {
                image: &mut image,
                initrd: Some(&mut initrd),
                cmdline: &cmdline,
            };
            let input = TypeAtPrompt {
                output: serial.clone(),
                after: "as init process",
                prompt: "# ",
                // The shell computes 42, so "vmm-42" can only come from
                // the command having run, not from the echo of the input.
                // If there is no reboot applet, leaving the shell panics
                // the kernel, which resets the machine just the same.
                script: Some(b"echo vmm-$((6*7)); /bin/busybox reboot -f || exit\n"),
            };
            let timeline = Timeline::new(Instant::now());
            let outcome = run_vm(
                &hv,
                DEFAULT_MEM_MIB << 20,
                payload,
                serial.clone(),
                Some(Box::new(input)),
                &timeline,
            )
            .unwrap();

            let log = serial.contents();
            assert!(log.contains("vmm-42"), "command output missing in:\n{log}");
            assert_eq!(outcome.reason, StopReason::Reset);
            assert!(timeline.get(metrics::EVENT_USERSPACE).is_some());
        }
    }
}
