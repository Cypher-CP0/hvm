//! Legacy PC port I/O, just enough for a kernel to get through early boot.
//!
//! This is a stopgap: the serial port becomes a real 8250 on Day 4 and the
//! rest moves behind a proper device bus in week 2.

use std::collections::BTreeMap;
use std::io::{self, Write};

const COM1_BASE: u16 = 0x3f8;
const COM1_END: u16 = 0x3ff;
/// Transmit holding register, or divisor latch low byte while DLAB is set.
const COM1_THR: u16 = COM1_BASE;
/// Line control register; bit 7 is DLAB.
const COM1_LCR: u16 = COM1_BASE + 3;
/// Line status register.
const COM1_LSR: u16 = COM1_BASE + 5;
const LCR_DLAB: u8 = 0x80;
/// Transmitter empty and idle: the guest may always send.
const LSR_THRE_TEMT: u8 = 0x60;

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
    serial: W,
    com1_lcr: u8,
    /// Accesses nothing claimed, by address. Reported at exit so that we can
    /// see what the guest probes for.
    unhandled: BTreeMap<(Access, u64), u64>,
}

impl<W: Write> Platform<W> {
    pub fn new(serial: W) -> Self {
        Self {
            serial,
            com1_lcr: 0,
            unhandled: BTreeMap::new(),
        }
    }

    pub fn pio_write(&mut self, port: u16, data: &[u8]) -> io::Result<PioOutcome> {
        match port {
            COM1_BASE..=COM1_END => match port {
                // With DLAB set this register is the baud-rate divisor, not
                // a character to print.
                COM1_THR if self.com1_lcr & LCR_DLAB == 0 => self.serial.write_all(data)?,
                COM1_LCR => self.com1_lcr = data.first().copied().unwrap_or(0),
                _ => {}
            },
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
            COM1_LSR => LSR_THRE_TEMT,
            COM1_BASE..=COM1_END => 0,
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
        self.serial.flush()
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

    fn read(p: &mut Platform<Vec<u8>>, port: u16) -> u8 {
        let mut b = [0xaa];
        p.pio_read(port, &mut b);
        b[0]
    }

    #[test]
    fn serial_output_skips_divisor_latch_writes() {
        let mut p = Platform::new(Vec::new());
        // What the kernel's early serial init does: set DLAB, program the
        // divisor, clear DLAB, then print.
        p.pio_write(COM1_LCR, &[0x83]).unwrap();
        p.pio_write(COM1_THR, &[0x01]).unwrap();
        p.pio_write(COM1_BASE + 1, &[0x00]).unwrap();
        p.pio_write(COM1_LCR, &[0x03]).unwrap();
        p.pio_write(COM1_THR, b"o").unwrap();
        p.pio_write(COM1_THR, b"k").unwrap();

        assert_eq!(p.serial, b"ok");
        assert!(p.unhandled().is_empty());
    }

    #[test]
    fn serial_always_reports_ready_to_transmit() {
        let mut p = Platform::new(Vec::new());
        assert_eq!(read(&mut p, COM1_LSR) & 0x20, 0x20);
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
