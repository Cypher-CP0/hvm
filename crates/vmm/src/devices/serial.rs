//! 16550A UART.
//!
//! A pure register-level model: the owner forwards guest accesses to
//! [`Serial::read`] and [`Serial::write`], pushes input with
//! [`Serial::enqueue_rx`], and mirrors [`Serial::interrupt_pending`] onto the
//! interrupt line after each of those.
//!
//! Transmission is instantaneous: a byte written to THR goes straight to the
//! output, so the transmitter always reports empty. Baud rate, parity and
//! framing are stored but have no effect.

use std::collections::VecDeque;
use std::io::{self, Write};

// Register offsets from the port base.
const DATA: u8 = 0; // RBR (read) / THR (write); DLL while DLAB is set
const IER: u8 = 1; // interrupt enable; DLM while DLAB is set
const IIR_FCR: u8 = 2; // interrupt identification (read) / FIFO control (write)
const LCR: u8 = 3;
const MCR: u8 = 4;
const LSR: u8 = 5;
const MSR: u8 = 6;
const SCR: u8 = 7;

const IER_RDI: u8 = 0x01; // received data available
const IER_THRI: u8 = 0x02; // transmitter holding register empty
const IER_MASK: u8 = 0x0f;

const IIR_NO_INT: u8 = 0x01;
const IIR_THRI: u8 = 0x02;
const IIR_RDI: u8 = 0x04;
const IIR_FIFO_ENABLED: u8 = 0xc0;

const FCR_ENABLE_FIFO: u8 = 0x01;
const FCR_CLEAR_RX: u8 = 0x02;

const LCR_DLAB: u8 = 0x80;

const MCR_DTR: u8 = 0x01;
const MCR_RTS: u8 = 0x02;
const MCR_OUT1: u8 = 0x04;
const MCR_OUT2: u8 = 0x08;
const MCR_LOOP: u8 = 0x10;
const MCR_MASK: u8 = 0x1f;

const LSR_DR: u8 = 0x01; // data ready
const LSR_THRE: u8 = 0x20; // transmitter holding register empty
const LSR_TEMT: u8 = 0x40; // transmitter empty

const MSR_CTS: u8 = 0x10;
const MSR_DSR: u8 = 0x20;
const MSR_RI: u8 = 0x40;
const MSR_DCD: u8 = 0x80;

const FIFO_SIZE: usize = 16;
/// 9600 baud with the standard 1.8432 MHz clock: the power-on divisor.
const DEFAULT_DIVISOR: u16 = 12;

pub struct Serial<W> {
    out: W,
    ier: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    divisor: u16,
    fifo_enabled: bool,
    /// The THR-empty interrupt condition. It is latched when the transmitter
    /// empties or the interrupt is enabled, and acknowledged by reading IIR.
    thr_ipending: bool,
    rx: VecDeque<u8>,
}

impl<W: Write> Serial<W> {
    pub fn new(out: W) -> Self {
        Self {
            out,
            ier: 0,
            lcr: 0,
            mcr: 0,
            scr: 0,
            divisor: DEFAULT_DIVISOR,
            fifo_enabled: false,
            thr_ipending: false,
            rx: VecDeque::with_capacity(FIFO_SIZE),
        }
    }

    fn dlab(&self) -> bool {
        self.lcr & LCR_DLAB != 0
    }

    fn rx_capacity(&self) -> usize {
        if self.fifo_enabled {
            FIFO_SIZE
        } else {
            1
        }
    }

    /// The highest-priority pending interrupt, as an IIR cause code.
    fn interrupt_cause(&self) -> u8 {
        if self.ier & IER_RDI != 0 && !self.rx.is_empty() {
            IIR_RDI
        } else if self.ier & IER_THRI != 0 && self.thr_ipending {
            IIR_THRI
        } else {
            IIR_NO_INT
        }
    }

    /// Whether the UART is asserting its interrupt line.
    pub fn interrupt_pending(&self) -> bool {
        self.interrupt_cause() != IIR_NO_INT
    }

    /// Offer received bytes to the guest. Returns how many were accepted;
    /// the rest should be offered again once the guest has read some.
    pub fn enqueue_rx(&mut self, data: &[u8]) -> usize {
        let room = self.rx_capacity().saturating_sub(self.rx.len());
        let accepted = data.len().min(room);
        self.rx.extend(&data[..accepted]);
        accepted
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }

    pub fn read(&mut self, offset: u8) -> u8 {
        match offset {
            DATA if self.dlab() => self.divisor as u8,
            DATA => self.rx.pop_front().unwrap_or(0),
            IER if self.dlab() => (self.divisor >> 8) as u8,
            IER => self.ier,
            IIR_FCR => {
                let cause = self.interrupt_cause();
                if cause == IIR_THRI {
                    // Reading IIR acknowledges a THR-empty interrupt.
                    self.thr_ipending = false;
                }
                if self.fifo_enabled {
                    cause | IIR_FIFO_ENABLED
                } else {
                    cause
                }
            }
            LCR => self.lcr,
            MCR => self.mcr,
            LSR => {
                let dr = if self.rx.is_empty() { 0 } else { LSR_DR };
                LSR_THRE | LSR_TEMT | dr
            }
            MSR if self.mcr & MCR_LOOP != 0 => {
                // Loopback wires the modem control outputs to the inputs.
                let wire = |out: u8, input: u8| if self.mcr & out != 0 { input } else { 0 };
                wire(MCR_RTS, MSR_CTS)
                    | wire(MCR_DTR, MSR_DSR)
                    | wire(MCR_OUT1, MSR_RI)
                    | wire(MCR_OUT2, MSR_DCD)
            }
            // A terminal is always attached and ready.
            MSR => MSR_DCD | MSR_DSR | MSR_CTS,
            SCR => self.scr,
            _ => 0xff,
        }
    }

    pub fn write(&mut self, offset: u8, value: u8) -> io::Result<()> {
        match offset {
            DATA if self.dlab() => self.divisor = (self.divisor & 0xff00) | value as u16,
            DATA => {
                if self.mcr & MCR_LOOP != 0 {
                    // Loopback: the byte comes back on the receiver. A full
                    // receiver drops it, as an overrun would.
                    self.enqueue_rx(&[value]);
                } else {
                    self.out.write_all(&[value])?;
                }
                // Sent at once, so the holding register is empty again.
                self.thr_ipending = true;
            }
            IER if self.dlab() => self.divisor = (self.divisor & 0x00ff) | (value as u16) << 8,
            IER => {
                let changed = self.ier ^ value;
                self.ier = value & IER_MASK;
                // Enabling the THR-empty interrupt while the register is
                // empty raises it straight away; drivers rely on this to
                // start transmitting.
                if changed & IER_THRI != 0 {
                    self.thr_ipending = self.ier & IER_THRI != 0;
                }
            }
            IIR_FCR => {
                self.fifo_enabled = value & FCR_ENABLE_FIFO != 0;
                if value & FCR_CLEAR_RX != 0 || !self.fifo_enabled {
                    self.rx.clear();
                }
            }
            LCR => self.lcr = value,
            MCR => self.mcr = value & MCR_MASK,
            SCR => self.scr = value,
            // LSR and MSR are read-only outside factory test mode.
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uart() -> Serial<Vec<u8>> {
        Serial::new(Vec::new())
    }

    #[test]
    fn transmits_to_output_and_always_reports_empty() {
        let mut s = uart();
        assert_eq!(s.read(LSR), LSR_THRE | LSR_TEMT);
        for b in b"hi\n" {
            s.write(DATA, *b).unwrap();
            assert_eq!(s.read(LSR), LSR_THRE | LSR_TEMT);
        }
        assert_eq!(s.out, b"hi\n");
    }

    #[test]
    fn divisor_latch_hides_data_and_ier() {
        let mut s = uart();
        s.write(IER, 0x05).unwrap();
        s.write(LCR, LCR_DLAB | 0x03).unwrap();
        // 115200 baud: divisor 1.
        s.write(DATA, 0x01).unwrap();
        s.write(IER, 0x00).unwrap();
        assert_eq!((s.read(DATA), s.read(IER)), (0x01, 0x00));
        s.write(LCR, 0x03).unwrap();

        assert!(s.out.is_empty(), "divisor bytes must not be transmitted");
        assert_eq!(s.read(IER), 0x05, "IER untouched by divisor writes");
        assert_eq!(s.read(LCR), 0x03);
    }

    #[test]
    fn receive_sets_data_ready_and_raises_interrupt_when_enabled() {
        let mut s = uart();
        assert_eq!(s.enqueue_rx(b"a"), 1);
        assert_eq!(s.read(LSR) & LSR_DR, LSR_DR);
        assert!(!s.interrupt_pending(), "RDI is not enabled yet");

        s.write(IER, IER_RDI).unwrap();
        assert!(s.interrupt_pending());
        assert_eq!(s.read(IIR_FCR), IIR_RDI);
        assert!(s.interrupt_pending(), "reading IIR does not clear RDI");

        assert_eq!(s.read(DATA), b'a');
        assert!(!s.interrupt_pending());
        assert_eq!(s.read(IIR_FCR), IIR_NO_INT);
        assert_eq!(s.read(LSR) & LSR_DR, 0);
    }

    #[test]
    fn receiver_holds_one_byte_without_fifo_and_sixteen_with() {
        let mut s = uart();
        assert_eq!(s.enqueue_rx(b"abc"), 1);
        assert_eq!(s.enqueue_rx(b"bc"), 0);
        assert_eq!(s.read(DATA), b'a');

        s.write(IIR_FCR, FCR_ENABLE_FIFO).unwrap();
        let input: Vec<u8> = (0..20).collect();
        assert_eq!(s.enqueue_rx(&input), FIFO_SIZE);
        for expected in 0..FIFO_SIZE as u8 {
            assert_eq!(s.read(DATA), expected);
        }
        assert_eq!(s.read(LSR) & LSR_DR, 0);
    }

    #[test]
    fn fifo_clear_discards_received_bytes() {
        let mut s = uart();
        s.write(IIR_FCR, FCR_ENABLE_FIFO).unwrap();
        s.enqueue_rx(b"stale");
        s.write(IIR_FCR, FCR_ENABLE_FIFO | FCR_CLEAR_RX).unwrap();
        assert_eq!(s.read(LSR) & LSR_DR, 0);
    }

    #[test]
    fn thr_empty_interrupt_follows_enable_ack_and_transmit() {
        let mut s = uart();
        s.write(DATA, b'x').unwrap();
        assert!(!s.interrupt_pending(), "THRI is not enabled");

        // Enabling it with an empty transmitter raises it at once.
        s.write(IER, IER_THRI).unwrap();
        assert!(s.interrupt_pending());
        // Reading IIR reports and acknowledges it.
        assert_eq!(s.read(IIR_FCR), IIR_THRI);
        assert!(!s.interrupt_pending());
        assert_eq!(s.read(IIR_FCR), IIR_NO_INT);
        // Each transmitted byte raises it again.
        s.write(DATA, b'y').unwrap();
        assert!(s.interrupt_pending());
        // Disabling drops the line.
        s.write(IER, 0).unwrap();
        assert!(!s.interrupt_pending());
    }

    #[test]
    fn received_data_outranks_thr_empty() {
        let mut s = uart();
        s.write(IER, IER_RDI | IER_THRI).unwrap();
        s.enqueue_rx(b"z");
        assert_eq!(s.read(IIR_FCR), IIR_RDI);
        assert_eq!(s.read(DATA), b'z');
        // The THR-empty interrupt was not consumed while RDI was reported.
        assert_eq!(s.read(IIR_FCR), IIR_THRI);
    }

    /// The probe in Linux's `autoconfig()` (drivers/tty/serial/8250), which
    /// decides whether a port exists and what kind of UART it is.
    #[test]
    fn passes_linux_8250_autoconfig() {
        let mut s = uart();

        // IER test: the low four bits must be writable, the rest not.
        s.write(IER, 0).unwrap();
        assert_eq!(s.read(IER) & 0x0f, 0);
        s.write(IER, 0x0f).unwrap();
        assert_eq!(s.read(IER) & 0x0f, 0x0f);
        s.write(IER, 0).unwrap();

        // Loopback test: RTS and OUT2 must appear as CTS and DCD.
        s.write(MCR, MCR_LOOP | MCR_OUT2 | MCR_RTS).unwrap();
        assert_eq!(s.read(MSR) & 0xf0, MSR_DCD | MSR_CTS);
        s.write(MCR, 0).unwrap();

        // EFR probes: with LCR = 0xBF or 0x80 an enhanced UART exposes EFR
        // at offset 2 and it reads zero. A plain 16550A must not.
        s.write(IIR_FCR, FCR_ENABLE_FIFO).unwrap();
        for conf_mode in [0x80, 0xbf] {
            s.write(LCR, conf_mode).unwrap();
            assert_ne!(s.read(IIR_FCR), 0);
        }
        s.write(LCR, 0).unwrap();

        // FIFO test: both top IIR bits set means 16550A.
        assert_eq!(s.read(IIR_FCR) >> 6, 3);
        // 16750 test: bit 5 must not follow the 64-byte FIFO enable.
        s.write(IIR_FCR, FCR_ENABLE_FIFO | 0x20).unwrap();
        assert_eq!(s.read(IIR_FCR) >> 5, 6);
        // XScale test: IER bit 6 must not stick.
        s.write(IER, 0x40).unwrap();
        assert_eq!(s.read(IER) & 0x40, 0);

        // With the FIFO off the top bits clear again.
        s.write(IIR_FCR, 0).unwrap();
        assert_eq!(s.read(IIR_FCR) >> 6, 0);
        assert!(s.out.is_empty());
    }

    /// `serial8250_do_startup()` checks that THRI can be raised twice in a
    /// row; a UART that fails gets a slow polling workaround.
    #[test]
    fn passes_linux_thre_interrupt_test() {
        let mut s = uart();
        s.write(IER, IER_THRI).unwrap();
        let iir1 = s.read(IIR_FCR);
        s.write(IER, 0).unwrap();
        s.write(IER, IER_THRI).unwrap();
        let iir2 = s.read(IIR_FCR);
        s.write(IER, 0).unwrap();

        assert_eq!(iir1 & IIR_NO_INT, 0);
        assert_eq!(iir2 & IIR_NO_INT, 0);
    }

    #[test]
    fn loopback_returns_transmitted_bytes_instead_of_sending_them() {
        let mut s = uart();
        s.write(MCR, MCR_LOOP).unwrap();
        s.write(DATA, 0x5a).unwrap();
        assert_eq!(s.read(LSR) & LSR_DR, LSR_DR);
        assert_eq!(s.read(DATA), 0x5a);
        assert!(s.out.is_empty());
    }

    #[test]
    fn modem_status_reports_a_ready_terminal() {
        let mut s = uart();
        assert_eq!(s.read(MSR), MSR_DCD | MSR_DSR | MSR_CTS);
    }

    #[test]
    fn scratch_register_round_trips() {
        let mut s = uart();
        s.write(SCR, 0xa5).unwrap();
        assert_eq!(s.read(SCR), 0xa5);
    }
}
