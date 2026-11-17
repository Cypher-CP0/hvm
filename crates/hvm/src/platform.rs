//! The machine's I/O front: the port and MMIO buses, and the legacy PC
//! devices that sit at fixed ports.
//!
//! The device models in `devices/` are plain state machines. The adapters
//! here connect one to the rest of the machine: its interrupt line, the
//! reset line, and the bus.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use hypervisor::IrqLine;

use crate::bus::{Bus, BusDevice, BusError};
use crate::devices::i8042::{I8042Outcome, I8042};
use crate::devices::legacy::{CmosStub, PostPort};
use crate::devices::serial::Serial;

const COM1_BASE: u64 = 0x3f8;
const COM1_LEN: u64 = 8;
/// ISA interrupt line of COM1.
pub const COM1_IRQ: u32 = 4;
/// ISA interrupt line of the keyboard controller.
pub const KEYBOARD_IRQ: u32 = 1;

const I8042_DATA: u64 = 0x60;
const I8042_COMMAND: u64 = 0x64;
const CMOS_BASE: u64 = 0x70;
const CMOS_LEN: u64 = 2;
const POST_PORT: u64 = 0x80;

/// What the run loop should do after an I/O access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoOutcome {
    Continue,
    /// The guest asked for a machine reset.
    Reset,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Access {
    PioRead,
    PioWrite,
    MmioRead,
    MmioWrite,
}

/// The machine's reset line. Any device may pull it; the run loop looks at
/// it after every access.
#[derive(Clone, Default)]
pub struct ResetLine(Arc<AtomicBool>);

impl ResetLine {
    pub fn assert(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_asserted(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// A device's connection to an interrupt line, which remembers the level it
/// last drove so that the hypervisor is only told about changes.
struct InterruptPin {
    /// `None` when the VM has no interrupt controller to deliver to.
    line: Option<Box<dyn IrqLine>>,
    asserted: bool,
}

impl InterruptPin {
    fn new(line: Option<Box<dyn IrqLine>>) -> Self {
        Self {
            line,
            asserted: false,
        }
    }

    fn set(&mut self, level: bool) -> io::Result<()> {
        if level != self.asserted {
            if let Some(line) = &self.line {
                line.set_level(level).map_err(io::Error::other)?;
            }
            self.asserted = level;
        }
        Ok(())
    }
}

/// COM1: the UART together with its interrupt line.
///
/// Shared between the vCPU thread, which handles the guest's port accesses,
/// and the console input thread, which delivers received bytes.
pub struct Com1<W> {
    uart: Serial<W>,
    irq: InterruptPin,
}

impl<W: Write> Com1<W> {
    /// Bring the interrupt line in step with the UART. Must follow every
    /// operation that can change the UART's interrupt state.
    fn sync_irq(&mut self) -> io::Result<()> {
        self.irq.set(self.uart.interrupt_pending())
    }

    /// Deliver bytes from the outside world to the guest. Returns how many
    /// the UART had room for.
    pub fn receive(&mut self, data: &[u8]) -> io::Result<usize> {
        let accepted = self.uart.enqueue_rx(data);
        self.sync_irq()?;
        Ok(accepted)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.uart.flush()
    }

    /// Whether the interrupt line is currently driven high.
    #[cfg(test)]
    pub fn interrupt_asserted(&self) -> bool {
        self.irq.asserted
    }
}

impl<W: Write + Send> BusDevice for Com1<W> {
    fn read(&mut self, _base: u64, offset: u64, data: &mut [u8]) -> io::Result<()> {
        for byte in data.iter_mut() {
            *byte = self.uart.read(offset as u8);
        }
        self.sync_irq()
    }

    fn write(&mut self, _base: u64, offset: u64, data: &[u8]) -> io::Result<()> {
        // More than one byte means string I/O (`rep outsb`): each byte is a
        // separate write to the same register.
        for &byte in data {
            self.uart.write(offset as u8, byte)?;
        }
        self.sync_irq()
    }
}

/// The i8042 together with its interrupt line and the machine's reset line.
/// It claims two one-port ranges and tells them apart by their base.
struct KeyboardController {
    i8042: I8042,
    irq: InterruptPin,
    reset: ResetLine,
}

impl BusDevice for KeyboardController {
    fn read(&mut self, base: u64, _offset: u64, data: &mut [u8]) -> io::Result<()> {
        for byte in data.iter_mut() {
            *byte = match base {
                I8042_COMMAND => self.i8042.read_status(),
                _ => self.i8042.read_data(),
            };
        }
        self.irq.set(self.i8042.interrupt_pending())
    }

    fn write(&mut self, base: u64, _offset: u64, data: &[u8]) -> io::Result<()> {
        for &byte in data {
            let outcome = match base {
                I8042_COMMAND => self.i8042.write_command(byte),
                _ => self.i8042.write_data(byte),
            };
            if outcome == I8042Outcome::Reset {
                self.reset.assert();
            }
        }
        self.irq.set(self.i8042.interrupt_pending())
    }
}

/// How often the guest touched one address in one direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoCount {
    pub access: Access,
    pub addr: u64,
    pub count: u64,
    /// Whether a device claimed the access.
    pub handled: bool,
}

/// The interrupt lines the platform's devices are wired to.
pub struct PlatformIrqs {
    /// [`COM1_IRQ`].
    pub com1: Box<dyn IrqLine>,
    /// [`KEYBOARD_IRQ`].
    pub keyboard: Box<dyn IrqLine>,
}

pub struct Platform<W> {
    pio: Bus,
    mmio: Bus,
    com1: Arc<Mutex<Com1<W>>>,
    reset: ResetLine,
    /// Every access by address, and whether anything claimed it. Reported at
    /// exit: it shows where exits come from and what the guest probes for.
    accesses: BTreeMap<(Access, u64), (u64, bool)>,
}

impl<W: Write + Send + 'static> Platform<W> {
    /// Build the buses and put the legacy devices on them. `serial`
    /// receives what the guest transmits on COM1. `irqs` connects the
    /// devices to the VM's interrupt controllers, if it has any.
    pub fn new(serial: W, irqs: Option<PlatformIrqs>) -> Result<Self, BusError> {
        let (com1_irq, keyboard_irq) = match irqs {
            Some(irqs) => (Some(irqs.com1), Some(irqs.keyboard)),
            None => (None, None),
        };
        let reset = ResetLine::default();
        let com1 = Arc::new(Mutex::new(Com1 {
            uart: Serial::new(serial),
            irq: InterruptPin::new(com1_irq),
        }));
        let keyboard = Arc::new(Mutex::new(KeyboardController {
            i8042: I8042::new(),
            irq: InterruptPin::new(keyboard_irq),
            reset: reset.clone(),
        }));

        let mut pio = Bus::new();
        pio.insert(COM1_BASE, COM1_LEN, com1.clone())?;
        // Port 0x61 in between belongs to the PIT's speaker gate, which the
        // hypervisor emulates.
        pio.insert(I8042_DATA, 1, keyboard.clone())?;
        pio.insert(I8042_COMMAND, 1, keyboard)?;
        pio.insert(CMOS_BASE, CMOS_LEN, Arc::new(Mutex::new(CmosStub)))?;
        pio.insert(POST_PORT, 1, Arc::new(Mutex::new(PostPort)))?;

        Ok(Self {
            pio,
            mmio: Bus::new(),
            com1,
            reset,
            accesses: BTreeMap::new(),
        })
    }

    /// A handle to COM1 for whoever supplies its input.
    pub fn com1(&self) -> Arc<Mutex<Com1<W>>> {
        Arc::clone(&self.com1)
    }

    /// The MMIO bus, for adding devices while the machine is being built.
    #[allow(dead_code)] // first used by the virtio-mmio transport
    pub fn mmio_bus_mut(&mut self) -> &mut Bus {
        &mut self.mmio
    }

    fn outcome(&self) -> IoOutcome {
        if self.reset.is_asserted() {
            IoOutcome::Reset
        } else {
            IoOutcome::Continue
        }
    }

    pub fn pio_write(&mut self, port: u16, data: &[u8]) -> io::Result<IoOutcome> {
        let handled = self.pio.write(port as u64, data)?;
        self.note(Access::PioWrite, port as u64, handled);
        Ok(self.outcome())
    }

    pub fn pio_read(&mut self, port: u16, data: &mut [u8]) -> io::Result<()> {
        let handled = self.pio.read(port as u64, data)?;
        if !handled {
            data.fill(0xff); // nothing drives the bus
        }
        self.note(Access::PioRead, port as u64, handled);
        Ok(())
    }

    pub fn mmio_write(&mut self, addr: u64, data: &[u8]) -> io::Result<IoOutcome> {
        let handled = self.mmio.write(addr, data)?;
        self.note(Access::MmioWrite, addr, handled);
        Ok(self.outcome())
    }

    pub fn mmio_read(&mut self, addr: u64, data: &mut [u8]) -> io::Result<()> {
        let handled = self.mmio.read(addr, data)?;
        if !handled {
            data.fill(0xff);
        }
        self.note(Access::MmioRead, addr, handled);
        Ok(())
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.com1.lock().unwrap().flush()
    }

    fn note(&mut self, access: Access, addr: u64, handled: bool) {
        let entry = self.accesses.entry((access, addr)).or_insert((0, handled));
        entry.0 += 1;
    }

    /// Every address the guest touched, most frequent first.
    pub fn io_counts(&self) -> Vec<IoCount> {
        let mut v: Vec<_> = self
            .accesses
            .iter()
            .map(|(&(access, addr), &(count, handled))| IoCount {
                access,
                addr,
                count,
                handled,
            })
            .collect();
        v.sort_by_key(|c| std::cmp::Reverse(c.count));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An interrupt line that records its level.
    struct TestLine(Arc<AtomicBool>);

    impl IrqLine for TestLine {
        fn set_level(&self, level: bool) -> hypervisor::Result<()> {
            self.0.store(level, Ordering::SeqCst);
            Ok(())
        }
    }

    /// Recording lines for both devices: `(irqs, com1 level, keyboard level)`.
    fn test_irqs() -> (PlatformIrqs, Arc<AtomicBool>, Arc<AtomicBool>) {
        let com1 = Arc::new(AtomicBool::new(false));
        let keyboard = Arc::new(AtomicBool::new(false));
        let irqs = PlatformIrqs {
            com1: Box::new(TestLine(Arc::clone(&com1))),
            keyboard: Box::new(TestLine(Arc::clone(&keyboard))),
        };
        (irqs, com1, keyboard)
    }

    fn platform() -> Platform<Vec<u8>> {
        Platform::new(Vec::new(), None).unwrap()
    }

    fn read(p: &mut Platform<Vec<u8>>, port: u64) -> u8 {
        let mut b = [0xaa];
        p.pio_read(port as u16, &mut b).unwrap();
        b[0]
    }

    fn write(p: &mut Platform<Vec<u8>>, port: u64, data: &[u8]) -> IoOutcome {
        p.pio_write(port as u16, data).unwrap()
    }

    #[test]
    fn com1_ports_reach_the_uart() {
        let mut p = platform();
        assert_eq!(read(&mut p, COM1_BASE + 5) & 0x20, 0x20, "LSR: THR empty");
        write(&mut p, COM1_BASE, b"o");
        // String I/O delivers several bytes for one port.
        write(&mut p, COM1_BASE, b"k\n");
        assert!(p.io_counts().iter().all(|c| c.handled));

        let mut out = Vec::new();
        std::mem::swap(&mut out, p.com1.lock().unwrap().uart.output_mut());
        assert_eq!(out, b"ok\n");
    }

    #[test]
    fn com1_claims_exactly_its_eight_ports() {
        let mut p = platform();
        read(&mut p, COM1_BASE - 1);
        read(&mut p, COM1_BASE + 7);
        read(&mut p, COM1_BASE + 8);

        let handled: Vec<_> = p.io_counts().iter().map(|c| (c.addr, c.handled)).collect();
        assert_eq!(handled, vec![(0x3f7, false), (0x3ff, true), (0x400, false)]);
    }

    #[test]
    fn com1_interrupt_line_follows_the_uart() {
        let (irqs, level, _) = test_irqs();
        let mut p = Platform::new(Vec::new(), Some(irqs)).unwrap();
        let asserted = || level.load(Ordering::SeqCst);

        // Enabling the THR-empty interrupt raises the line; reading IIR
        // acknowledges it.
        write(&mut p, COM1_BASE + 1, &[0x02]);
        assert!(asserted());
        assert_eq!(read(&mut p, COM1_BASE + 2), 0x02);
        assert!(!asserted());

        // Received data raises it from another thread's handle, and the
        // guest reading the byte lowers it.
        write(&mut p, COM1_BASE + 1, &[0x01]);
        let com1 = p.com1();
        assert_eq!(com1.lock().unwrap().receive(b"a").unwrap(), 1);
        assert!(asserted());
        assert_eq!(read(&mut p, COM1_BASE), b'a');
        assert!(!asserted());
    }

    #[test]
    fn com1_works_without_an_interrupt_line() {
        let mut p = platform();
        write(&mut p, COM1_BASE + 1, &[0x02]);
        assert!(p.com1().lock().unwrap().interrupt_asserted());
    }

    #[test]
    fn keyboard_controller_answers_on_both_its_ports() {
        let mut p = platform();
        assert_eq!(read(&mut p, I8042_COMMAND) & 0x03, 0, "buffers empty");

        // Read the control register: the reply shows up in the status
        // register and on the data port.
        write(&mut p, I8042_COMMAND, &[0x20]);
        assert_eq!(read(&mut p, I8042_COMMAND) & 0x01, 0x01);
        assert_ne!(read(&mut p, I8042_DATA), 0);
        assert_eq!(read(&mut p, I8042_COMMAND) & 0x01, 0);

        // The ports in between are not the controller's.
        assert_eq!(read(&mut p, 0x61), 0xff);
        assert!(p.io_counts().iter().any(|c| c.addr == 0x61 && !c.handled));
    }

    #[test]
    fn keyboard_controller_pulls_the_reset_line() {
        let mut p = platform();
        assert_eq!(write(&mut p, I8042_COMMAND, &[0xae]), IoOutcome::Continue);
        assert_eq!(write(&mut p, I8042_COMMAND, &[0xfe]), IoOutcome::Reset);
        // The line stays pulled: whatever the guest does next, it has reset.
        assert_eq!(write(&mut p, POST_PORT, &[0]), IoOutcome::Reset);
    }

    #[test]
    fn keyboard_interrupt_line_follows_the_controller() {
        let (irqs, com1, keyboard) = test_irqs();
        let mut p = Platform::new(Vec::new(), Some(irqs)).unwrap();
        let asserted = || keyboard.load(Ordering::SeqCst);

        // A byte for the absent keyboard is refused through the interrupt.
        write(&mut p, I8042_DATA, &[0xf2]);
        assert!(asserted());
        assert_eq!(read(&mut p, I8042_DATA), 0xfe);
        assert!(!asserted());
        assert!(!com1.load(Ordering::SeqCst), "COM1's line is untouched");
    }

    #[test]
    fn unknown_ports_float_high_and_are_counted() {
        let mut p = platform();
        assert_eq!(read(&mut p, 0xcfc), 0xff);
        assert_eq!(read(&mut p, 0xcfc), 0xff);
        write(&mut p, 0xcf8, &[0, 0, 0, 0x80]);
        write(&mut p, POST_PORT, &[0]);

        let unhandled: Vec<_> = p.io_counts().into_iter().filter(|c| !c.handled).collect();
        assert_eq!(
            unhandled,
            vec![
                IoCount {
                    access: Access::PioRead,
                    addr: 0xcfc,
                    count: 2,
                    handled: false
                },
                IoCount {
                    access: Access::PioWrite,
                    addr: 0xcf8,
                    count: 1,
                    handled: false
                },
            ]
        );
    }

    #[test]
    fn cmos_ports_are_claimed() {
        let mut p = platform();
        write(&mut p, CMOS_BASE, &[0x0a]);
        assert_eq!(read(&mut p, CMOS_BASE + 1) & 0x80, 0);
        assert!(p.io_counts().iter().all(|c| c.handled));
    }

    #[test]
    fn mmio_reaches_devices_added_to_its_bus() {
        /// One 32-bit register.
        struct Register(u32);

        impl BusDevice for Register {
            fn read(&mut self, _base: u64, offset: u64, data: &mut [u8]) -> io::Result<()> {
                assert_eq!(offset, 0x10);
                data.copy_from_slice(&self.0.to_le_bytes());
                Ok(())
            }

            fn write(&mut self, _base: u64, _offset: u64, data: &[u8]) -> io::Result<()> {
                self.0 = u32::from_le_bytes(data.try_into().unwrap());
                Ok(())
            }
        }

        let mut p = platform();
        p.mmio_bus_mut()
            .insert(0xd000_0000, 0x1000, Arc::new(Mutex::new(Register(0))))
            .unwrap();

        p.mmio_write(0xd000_0010, &0x7472_6976u32.to_le_bytes())
            .unwrap();
        let mut value = [0u8; 4];
        p.mmio_read(0xd000_0010, &mut value).unwrap();
        assert_eq!(u32::from_le_bytes(value), 0x7472_6976);

        // Outside the device, reads float and the access is recorded as
        // unclaimed.
        p.mmio_read(0xd000_1000, &mut value).unwrap();
        assert_eq!(value, [0xff; 4]);
        let unhandled: Vec<_> = p.io_counts().into_iter().filter(|c| !c.handled).collect();
        assert_eq!(unhandled.len(), 1);
        assert_eq!(unhandled[0].access, Access::MmioRead);
        assert_eq!(unhandled[0].addr, 0xd000_1000);
    }

    #[test]
    fn mmio_and_port_address_spaces_are_separate() {
        let mut p = platform();
        // COM1's port number means nothing as a memory address.
        let mut value = [0u8; 1];
        p.mmio_read(COM1_BASE + 5, &mut value).unwrap();
        assert_eq!(value, [0xff]);
        assert!(p.io_counts().iter().all(|c| !c.handled));
    }
}
