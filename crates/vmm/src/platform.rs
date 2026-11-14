//! The legacy PC platform: what lives at fixed I/O ports.
//!
//! COM1 is a real 16550A. The keyboard controller and CMOS are stubs that do
//! just enough for a kernel to boot and reset; they move behind a proper
//! device bus in week 2.

use std::collections::BTreeMap;
use std::io::{self, Write};

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

pub struct Platform<W> {
    com1: Serial<W>,
    /// Accesses nothing claimed, by address. Reported at exit so that we can
    /// see what the guest probes for.
    unhandled: BTreeMap<(Access, u64), u64>,
}

impl<W: Write> Platform<W> {
    pub fn new(serial: W) -> Self {
        Self {
            com1: Serial::new(serial),
            unhandled: BTreeMap::new(),
        }
    }

    /// Whether COM1 is asserting its interrupt line. The owner mirrors this
    /// onto [`COM1_IRQ`] after every port access.
    pub fn com1_interrupt(&self) -> bool {
        self.com1.interrupt_pending()
    }

    pub fn pio_write(&mut self, port: u16, data: &[u8]) -> io::Result<PioOutcome> {
        match port {
            COM1_BASE..=COM1_END => {
                // More than one byte means string I/O (`rep outsb`): each
                // byte is a separate write to the same register.
                for &byte in data {
                    self.com1.write((port - COM1_BASE) as u8, byte)?;
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

    pub fn pio_read(&mut self, port: u16, data: &mut [u8]) {
        let value = match port {
            COM1_BASE..=COM1_END => {
                for byte in data.iter_mut() {
                    *byte = self.com1.read((port - COM1_BASE) as u8);
                }
                return;
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
    }

    pub fn mmio_write(&mut self, addr: u64) {
        self.note_unhandled(Access::MmioWrite, addr);
    }

    pub fn mmio_read(&mut self, addr: u64, data: &mut [u8]) {
        self.note_unhandled(Access::MmioRead, addr);
        data.fill(0xff);
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.com1.flush()
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
    use super::*;

    fn read<W: Write>(p: &mut Platform<W>, port: u16) -> u8 {
        let mut b = [0xaa];
        p.pio_read(port, &mut b);
        b[0]
    }

    #[test]
    fn com1_ports_reach_the_uart() {
        let mut out = Vec::new();
        {
            let mut p = Platform::new(&mut out);
            assert_eq!(read(&mut p, COM1_BASE + 5) & 0x20, 0x20, "LSR: THR empty");
            p.pio_write(COM1_BASE, b"o").unwrap();
            // String I/O delivers several bytes for one port.
            p.pio_write(COM1_BASE, b"k\n").unwrap();
            assert!(p.unhandled().is_empty());

            // Enabling the THR-empty interrupt raises the line; reading IIR
            // acknowledges it.
            assert!(!p.com1_interrupt());
            p.pio_write(COM1_BASE + 1, &[0x02]).unwrap();
            assert!(p.com1_interrupt());
            assert_eq!(read(&mut p, COM1_BASE + 2), 0x02);
            assert!(!p.com1_interrupt());
        }
        assert_eq!(out, b"ok\n");
    }

    #[test]
    fn keyboard_controller_reset_is_reported() {
        let mut p = Platform::new(Vec::new());
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
        let mut p = Platform::new(Vec::new());
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
        let mut p = Platform::new(Vec::new());
        p.pio_write(CMOS_INDEX, &[0x0a]).unwrap();
        assert_eq!(read(&mut p, CMOS_DATA) & 0x80, 0);
    }
}
