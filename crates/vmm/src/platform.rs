//! The legacy PC platform: what lives at fixed I/O ports.
//!
//! COM1 is a real 16550A. The keyboard controller and CMOS are stubs that do
//! just enough for a kernel to boot and reset; they move behind a proper
//! device bus in week 2.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use hypervisor::IrqLine;

use crate::devices::serial::Serial;

const COM1_BASE: u16 = 0x3f8;
const COM1_END: u16 = 0x3ff;
/// ISA interrupt line of COM1.
pub const COM1_IRQ: u32 = 4;

const CMOS_INDEX: u16 = 0x70;
const CMOS_DATA: u16 = 0x71;
/// POST diagnostic port; Linux writes to it as an I/O delay.
const POST_PORT: u16 = 0x80;
const I8042_DATA: u16 = 0x60;
const I8042_COMMAND: u16 = 0x64;
/// Pulse the CPU reset line: how a PC reboots through the keyboard controller.
const I8042_CMD_RESET: u8 = 0xfe;

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

/// COM1: the UART together with its interrupt line.
///
/// Shared between the vCPU thread, which handles the guest's port accesses,
/// and the console input thread, which delivers received bytes.
pub struct Com1<W> {
    uart: Serial<W>,
    /// `None` when the VM has no interrupt controller to deliver to.
    irq: Option<Box<dyn IrqLine>>,
    /// The level the line was last driven to.
    asserted: bool,
}

impl<W: Write> Com1<W> {
    /// Bring the interrupt line in step with the UART. Must follow every
    /// operation that can change the UART's interrupt state.
    fn sync_irq(&mut self) -> io::Result<()> {
        let level = self.uart.interrupt_pending();
        if level != self.asserted {
            if let Some(irq) = &self.irq {
                irq.set_level(level).map_err(io::Error::other)?;
            }
            self.asserted = level;
        }
        Ok(())
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
        self.asserted
    }
}

pub struct Platform<W> {
    com1: Arc<Mutex<Com1<W>>>,
    /// Accesses nothing claimed, by address. Reported at exit so that we can
    /// see what the guest probes for.
    unhandled: BTreeMap<(Access, u64), u64>,
}

impl<W: Write> Platform<W> {
    /// `serial` receives what the guest transmits on COM1; `com1_irq` is the
    /// line the UART interrupts on.
    pub fn new(serial: W, com1_irq: Option<Box<dyn IrqLine>>) -> Self {
        Self {
            com1: Arc::new(Mutex::new(Com1 {
                uart: Serial::new(serial),
                irq: com1_irq,
                asserted: false,
            })),
            unhandled: BTreeMap::new(),
        }
    }

    /// A handle to COM1 for whoever supplies its input.
    pub fn com1(&self) -> Arc<Mutex<Com1<W>>> {
        Arc::clone(&self.com1)
    }

    pub fn pio_write(&mut self, port: u16, data: &[u8]) -> io::Result<PioOutcome> {
        match port {
            COM1_BASE..=COM1_END => {
                // More than one byte means string I/O (`rep outsb`): each
                // byte is a separate write to the same register.
                let mut com1 = self.com1.lock().unwrap();
                for &byte in data {
                    com1.write((port - COM1_BASE) as u8, byte)?;
                }
            }
            I8042_COMMAND if data.first() == Some(&I8042_CMD_RESET) => {
                return Ok(PioOutcome::Reset)
            }
            I8042_DATA | I8042_COMMAND | CMOS_INDEX | CMOS_DATA | POST_PORT => {}
            _ => self.note_unhandled(Access::PioWrite, port as u64),
        }
        Ok(PioOutcome::Continue)
    }

    pub fn pio_read(&mut self, port: u16, data: &mut [u8]) -> io::Result<()> {
        let value = match port {
            COM1_BASE..=COM1_END => {
                let mut com1 = self.com1.lock().unwrap();
                for byte in data.iter_mut() {
                    *byte = com1.read((port - COM1_BASE) as u8)?;
                }
                return Ok(());
            }
            // Keyboard controller: both buffers empty.
            I8042_DATA | I8042_COMMAND => 0,
            // No RTC. Zero keeps "update in progress" clear, so a guest
            // that waits on that bit does not spin.
            CMOS_INDEX | CMOS_DATA => 0,
            _ => {
                self.note_unhandled(Access::PioRead, port as u64);
                0xff // nothing drives the bus
            }
        };
        data.fill(value);
        Ok(())
    }

    pub fn mmio_write(&mut self, addr: u64) {
        self.note_unhandled(Access::MmioWrite, addr);
    }

    pub fn mmio_read(&mut self, addr: u64, data: &mut [u8]) {
        self.note_unhandled(Access::MmioRead, addr);
        data.fill(0xff);
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.com1.lock().unwrap().uart.flush()
    }

    fn note_unhandled(&mut self, access: Access, addr: u64) {
        *self.unhandled.entry((access, addr)).or_insert(0) += 1;
    }

    /// Unclaimed accesses, most frequent first.
    pub fn unhandled(&self) -> Vec<(Access, u64, u64)> {
        let mut v: Vec<_> = self
            .unhandled
            .iter()
            .map(|(&(access, addr), &count)| (access, addr, count))
            .collect();
        v.sort_by_key(|&(_, _, count)| std::cmp::Reverse(count));
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
        assert!(p.unhandled().is_empty());

        let mut out = Vec::new();
        std::mem::swap(&mut out, p.com1.lock().unwrap().uart.output_mut());
        assert_eq!(out, b"ok\n");
    }

    #[test]
    fn com1_interrupt_line_follows_the_uart() {
        let level = Arc::new(AtomicBool::new(false));
        let line = TestLine(Arc::clone(&level));
        let mut p = Platform::new(Vec::new(), Some(Box::new(line)));
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
    fn keyboard_controller_reset_is_reported() {
        let mut p = platform();
        assert_eq!(read(&mut p, I8042_COMMAND) & 0x02, 0, "input buffer empty");
        assert_eq!(
            p.pio_write(I8042_COMMAND, &[0xd1]).unwrap(),
            PioOutcome::Continue
        );
        assert_eq!(
            p.pio_write(I8042_COMMAND, &[I8042_CMD_RESET]).unwrap(),
            PioOutcome::Reset
        );
    }

    #[test]
    fn unknown_ports_float_high_and_are_counted() {
        let mut p = platform();
        assert_eq!(read(&mut p, 0xcfc), 0xff);
        assert_eq!(read(&mut p, 0xcfc), 0xff);
        p.pio_write(0xcf8, &[0, 0, 0, 0x80]).unwrap();

        assert_eq!(
            p.unhandled(),
            vec![(Access::PioRead, 0xcfc, 2), (Access::PioWrite, 0xcf8, 1)]
        );
    }

    #[test]
    fn cmos_never_reports_update_in_progress() {
        let mut p = platform();
        p.pio_write(CMOS_INDEX, &[0x0a]).unwrap();
        assert_eq!(read(&mut p, CMOS_DATA) & 0x80, 0);
    }
}
