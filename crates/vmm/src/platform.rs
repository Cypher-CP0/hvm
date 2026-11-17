//! The legacy PC platform: what lives at fixed I/O ports.
//!
//! COM1 is a 16550A and ports 0x60/0x64 are an i8042 keyboard controller.
//! CMOS is a stub. All of this moves behind a proper device bus in week 2.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use hypervisor::IrqLine;

use crate::devices::i8042::{I8042Outcome, I8042};
use crate::devices::serial::Serial;

const COM1_BASE: u16 = 0x3f8;
const COM1_END: u16 = 0x3ff;
/// ISA interrupt line of COM1.
pub const COM1_IRQ: u32 = 4;
/// ISA interrupt line of the keyboard controller.
pub const KEYBOARD_IRQ: u32 = 1;

const CMOS_INDEX: u16 = 0x70;
const CMOS_DATA: u16 = 0x71;
/// POST diagnostic port; Linux writes to it as an I/O delay.
const POST_PORT: u16 = 0x80;
const I8042_DATA: u16 = 0x60;
const I8042_COMMAND: u16 = 0x64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PioOutcome {
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

    fn read(&mut self, offset: u8) -> io::Result<u8> {
        let value = self.uart.read(offset);
        self.sync_irq()?;
        Ok(value)
    }

    fn write(&mut self, offset: u8, value: u8) -> io::Result<()> {
        self.uart.write(offset, value)?;
        self.sync_irq()
    }

    /// Deliver bytes from the outside world to the guest. Returns how many
    /// the UART had room for.
    pub fn receive(&mut self, data: &[u8]) -> io::Result<usize> {
        let accepted = self.uart.enqueue_rx(data);
        self.sync_irq()?;
        Ok(accepted)
    }

    /// Whether the interrupt line is currently driven high.
    #[cfg(test)]
    pub fn interrupt_asserted(&self) -> bool {
        self.irq.asserted
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
    com1: Arc<Mutex<Com1<W>>>,
    i8042: I8042,
    keyboard_irq: InterruptPin,
    /// Every access by address, and whether anything claimed it. Reported at
    /// exit: it shows where exits come from and what the guest probes for.
    accesses: BTreeMap<(Access, u64), (u64, bool)>,
}

impl<W: Write> Platform<W> {
    /// `serial` receives what the guest transmits on COM1. `irqs` connects
    /// the devices to the VM's interrupt controllers, if it has any.
    pub fn new(serial: W, irqs: Option<PlatformIrqs>) -> Self {
        let (com1_irq, keyboard_irq) = match irqs {
            Some(irqs) => (Some(irqs.com1), Some(irqs.keyboard)),
            None => (None, None),
        };
        Self {
            com1: Arc::new(Mutex::new(Com1 {
                uart: Serial::new(serial),
                irq: InterruptPin::new(com1_irq),
            })),
            i8042: I8042::new(),
            keyboard_irq: InterruptPin::new(keyboard_irq),
            accesses: BTreeMap::new(),
        }
    }

    /// A handle to COM1 for whoever supplies its input.
    pub fn com1(&self) -> Arc<Mutex<Com1<W>>> {
        Arc::clone(&self.com1)
    }

    pub fn pio_write(&mut self, port: u16, data: &[u8]) -> io::Result<PioOutcome> {
        let mut outcome = PioOutcome::Continue;
        let handled = match port {
            COM1_BASE..=COM1_END => {
                let mut com1 = self.com1.lock().unwrap();
                // More than one byte means string I/O (`rep outsb`): each
                // byte is a separate write to the same register.
                for &byte in data {
                    com1.write((port - COM1_BASE) as u8, byte)?;
                }
                true
            }
            I8042_DATA | I8042_COMMAND => {
                for &byte in data {
                    let result = if port == I8042_COMMAND {
                        self.i8042.write_command(byte)
                    } else {
                        self.i8042.write_data(byte)
                    };
                    if result == I8042Outcome::Reset {
                        outcome = PioOutcome::Reset;
                    }
                }
                self.keyboard_irq.set(self.i8042.interrupt_pending())?;
                true
            }
            CMOS_INDEX | CMOS_DATA | POST_PORT => true,
            _ => false,
        };
        self.note(Access::PioWrite, port as u64, handled);
        Ok(outcome)
    }

    pub fn pio_read(&mut self, port: u16, data: &mut [u8]) -> io::Result<()> {
        let handled = match port {
            COM1_BASE..=COM1_END => {
                let mut com1 = self.com1.lock().unwrap();
                for byte in data.iter_mut() {
                    *byte = com1.read((port - COM1_BASE) as u8)?;
                }
                true
            }
            I8042_DATA => {
                for byte in data.iter_mut() {
                    *byte = self.i8042.read_data();
                }
                self.keyboard_irq.set(self.i8042.interrupt_pending())?;
                true
            }
            I8042_COMMAND => {
                data.fill(self.i8042.read_status());
                true
            }
            // No RTC. Zero keeps "update in progress" clear, so a guest
            // that waits on that bit does not spin.
            CMOS_INDEX | CMOS_DATA => {
                data.fill(0);
                true
            }
            _ => {
                data.fill(0xff); // nothing drives the bus
                false
            }
        };
        self.note(Access::PioRead, port as u64, handled);
        Ok(())
    }

    pub fn mmio_write(&mut self, addr: u64) {
        self.note(Access::MmioWrite, addr, false);
    }

    pub fn mmio_read(&mut self, addr: u64, data: &mut [u8]) {
        self.note(Access::MmioRead, addr, false);
        data.fill(0xff);
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.com1.lock().unwrap().uart.flush()
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
    use std::sync::atomic::{AtomicBool, Ordering};

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
        Platform::new(Vec::new(), None)
    }

    fn read<W: Write>(p: &mut Platform<W>, port: u16) -> u8 {
        let mut b = [0xaa];
        p.pio_read(port, &mut b).unwrap();
        b[0]
    }

    #[test]
    fn com1_ports_reach_the_uart() {
        let mut p = platform();
        assert_eq!(read(&mut p, COM1_BASE + 5) & 0x20, 0x20, "LSR: THR empty");
        p.pio_write(COM1_BASE, b"o").unwrap();
        // String I/O delivers several bytes for one port.
        p.pio_write(COM1_BASE, b"k\n").unwrap();
        assert!(p.io_counts().iter().all(|c| c.handled));

        let mut out = Vec::new();
        std::mem::swap(&mut out, p.com1.lock().unwrap().uart.output_mut());
        assert_eq!(out, b"ok\n");
    }

    #[test]
    fn com1_interrupt_line_follows_the_uart() {
        let (irqs, level, _) = test_irqs();
        let mut p = Platform::new(Vec::new(), Some(irqs));
        let asserted = || level.load(Ordering::SeqCst);

        // Enabling the THR-empty interrupt raises the line; reading IIR
        // acknowledges it.
        p.pio_write(COM1_BASE + 1, &[0x02]).unwrap();
        assert!(asserted());
        assert_eq!(read(&mut p, COM1_BASE + 2), 0x02);
        assert!(!asserted());

        // Received data raises it from another thread's handle, and the
        // guest reading the byte lowers it.
        p.pio_write(COM1_BASE + 1, &[0x01]).unwrap();
        let com1 = p.com1();
        assert_eq!(com1.lock().unwrap().receive(b"a").unwrap(), 1);
        assert!(asserted());
        assert_eq!(read(&mut p, COM1_BASE), b'a');
        assert!(!asserted());
    }

    #[test]
    fn com1_works_without_an_interrupt_line() {
        let mut p = platform();
        p.pio_write(COM1_BASE + 1, &[0x02]).unwrap();
        assert!(p.com1().lock().unwrap().interrupt_asserted());
    }

    #[test]
    fn keyboard_controller_answers_and_resets() {
        let mut p = platform();
        assert_eq!(read(&mut p, I8042_COMMAND) & 0x03, 0, "buffers empty");

        // Read the control register: the reply shows up in the status
        // register and on the data port.
        p.pio_write(I8042_COMMAND, &[0x20]).unwrap();
        assert_eq!(read(&mut p, I8042_COMMAND) & 0x01, 0x01);
        assert_ne!(read(&mut p, I8042_DATA), 0);
        assert_eq!(read(&mut p, I8042_COMMAND) & 0x01, 0);

        assert_eq!(
            p.pio_write(I8042_COMMAND, &[0xae]).unwrap(),
            PioOutcome::Continue
        );
        assert_eq!(
            p.pio_write(I8042_COMMAND, &[0xfe]).unwrap(),
            PioOutcome::Reset
        );
    }

    #[test]
    fn keyboard_interrupt_line_follows_the_controller() {
        let (irqs, com1, keyboard) = test_irqs();
        let mut p = Platform::new(Vec::new(), Some(irqs));
        let asserted = || keyboard.load(Ordering::SeqCst);

        // A byte for the absent keyboard is refused through the interrupt.
        p.pio_write(I8042_DATA, &[0xf2]).unwrap();
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
        p.pio_write(0xcf8, &[0, 0, 0, 0x80]).unwrap();
        p.pio_write(POST_PORT, &[0]).unwrap();

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
    fn cmos_never_reports_update_in_progress() {
        let mut p = platform();
        p.pio_write(CMOS_INDEX, &[0x0a]).unwrap();
        assert_eq!(read(&mut p, CMOS_DATA) & 0x80, 0);
    }
}
