//! i8042 keyboard controller, with nothing plugged into it.
//!
//! Guests probe this controller during boot and wait half a second for each
//! answer that never comes, so answering promptly matters more than what is
//! attached. It is also how a PC is reset: the controller's output port
//! drives the CPU reset line.
//!
//! No keyboard or mouse is emulated. A byte the guest sends to the keyboard
//! is refused with a "resend" reply, which is how a driver finds out at once
//! that nothing is listening. Silence would be just as truthful, but costs
//! the guest a 200 ms timeout per attempt.

// Controller commands, written to the command port.
const CMD_READ_CTR: u8 = 0x20;
const CMD_WRITE_CTR: u8 = 0x60;
const CMD_AUX_TEST: u8 = 0xa9;
const CMD_SELF_TEST: u8 = 0xaa;
const CMD_KBD_TEST: u8 = 0xab;
const CMD_KBD_DISABLE: u8 = 0xad;
const CMD_KBD_ENABLE: u8 = 0xae;
const CMD_READ_OUTPUT_PORT: u8 = 0xd0;
const CMD_WRITE_OUTPUT_PORT: u8 = 0xd1;
const CMD_AUX_LOOP: u8 = 0xd3;
/// Pulse output port bit 0, the CPU reset line.
const CMD_PULSE_RESET: u8 = 0xfe;

/// Status: the output buffer holds a byte for the guest.
const STATUS_OUTPUT_FULL: u8 = 0x01;
/// Status: the system passed its power-on self test.
const STATUS_SYSTEM: u8 = 0x04;
/// Status: the keyboard lock switch is open (not locked).
const STATUS_UNLOCKED: u8 = 0x10;

/// Control register: raise the keyboard interrupt when a byte from the
/// keyboard is waiting.
const CTR_KBD_INT: u8 = 0x01;
/// Control register: keyboard interface disabled.
const CTR_KBD_DISABLE: u8 = 0x10;
/// Power-on control register: keyboard interrupt on, system flag, scancode
/// translation on.
const CTR_DEFAULT: u8 = 0x45;

/// Output port: CPU reset line, active low.
const OUTPUT_PORT_RESET: u8 = 0x01;
/// Output port as read back: reset line high, A20 enabled.
const OUTPUT_PORT_DEFAULT: u8 = 0x03;

const SELF_TEST_PASSED: u8 = 0x55;
const KBD_TEST_PASSED: u8 = 0x00;
/// Port test result "clock line stuck low": no usable mouse port.
const AUX_TEST_FAILED: u8 = 0x01;
/// PS/2 "resend": the last byte was not received properly.
const KBD_RESEND: u8 = 0xfe;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum I8042Outcome {
    Continue,
    /// The guest pulled the CPU reset line.
    Reset,
}

pub struct I8042 {
    ctr: u8,
    /// The output buffer: one byte waiting for the guest to read from the
    /// data port. As on the real chip, a new reply replaces an unread one.
    output: Option<u8>,
    /// Whether the byte in the output buffer came from the keyboard port
    /// rather than from the controller itself.
    output_from_kbd: bool,
    /// A command that takes its argument from the next data port write.
    awaiting_data: Option<u8>,
}

impl Default for I8042 {
    fn default() -> Self {
        Self::new()
    }
}

impl I8042 {
    pub fn new() -> Self {
        Self {
            ctr: CTR_DEFAULT,
            output: None,
            output_from_kbd: false,
            awaiting_data: None,
        }
    }

    /// Read the status register (command port).
    pub fn read_status(&self) -> u8 {
        // The input buffer is never full: commands are handled at once.
        let obf = match self.output {
            Some(_) => STATUS_OUTPUT_FULL,
            None => 0,
        };
        STATUS_UNLOCKED | STATUS_SYSTEM | obf
    }

    /// Read the data port.
    pub fn read_data(&mut self) -> u8 {
        self.output_from_kbd = false;
        self.output.take().unwrap_or(0)
    }

    /// Whether the controller is asserting the keyboard interrupt (IRQ 1):
    /// a byte from the keyboard is waiting and the interrupt is enabled.
    pub fn interrupt_pending(&self) -> bool {
        self.output.is_some() && self.output_from_kbd && self.ctr & CTR_KBD_INT != 0
    }

    /// Place a reply from the controller itself in the output buffer.
    fn reply(&mut self, value: u8) {
        self.output = Some(value);
        self.output_from_kbd = false;
    }

    /// Write the command port.
    pub fn write_command(&mut self, command: u8) -> I8042Outcome {
        self.awaiting_data = None;
        match command {
            CMD_READ_CTR => self.reply(self.ctr),
            CMD_WRITE_CTR | CMD_WRITE_OUTPUT_PORT | CMD_AUX_LOOP => {
                self.awaiting_data = Some(command)
            }
            CMD_SELF_TEST => self.reply(SELF_TEST_PASSED),
            CMD_KBD_TEST => self.reply(KBD_TEST_PASSED),
            CMD_AUX_TEST => self.reply(AUX_TEST_FAILED),
            CMD_KBD_DISABLE => self.ctr |= CTR_KBD_DISABLE,
            CMD_KBD_ENABLE => self.ctr &= !CTR_KBD_DISABLE,
            CMD_READ_OUTPUT_PORT => self.reply(OUTPUT_PORT_DEFAULT),
            CMD_PULSE_RESET => return I8042Outcome::Reset,
            // Everything else needs no reply.
            _ => {}
        }
        I8042Outcome::Continue
    }

    /// Write the data port: the argument of the last command, or else a
    /// byte for the keyboard.
    pub fn write_data(&mut self, value: u8) -> I8042Outcome {
        match self.awaiting_data.take() {
            Some(CMD_WRITE_CTR) => self.ctr = value,
            Some(CMD_WRITE_OUTPUT_PORT) if value & OUTPUT_PORT_RESET == 0 => {
                return I8042Outcome::Reset
            }
            // The byte would come back from the mouse port, flagged as mouse
            // data. There is no such port, so it comes back unflagged, which
            // tells the guest so without making it wait for a timeout.
            Some(CMD_AUX_LOOP) => self.reply(value),
            // A byte for the keyboard. With the interface enabled and no
            // keyboard on it, the answer is "resend".
            None if self.ctr & CTR_KBD_DISABLE == 0 => {
                self.output = Some(KBD_RESEND);
                self.output_from_kbd = true;
            }
            // Interface disabled, or an argument for a command that takes
            // none: dropped.
            _ => {}
        }
        I8042Outcome::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Status bit Linux checks on an AUX_LOOP reply.
    const STATUS_AUX_DATA: u8 = 0x20;
    /// Status bit Linux waits on before every write.
    const STATUS_INPUT_FULL: u8 = 0x02;

    #[test]
    fn idle_controller_has_empty_buffers() {
        let mut c = I8042::new();
        assert_eq!(
            c.read_status() & (STATUS_OUTPUT_FULL | STATUS_INPUT_FULL),
            0
        );
        assert_eq!(c.read_data(), 0);
    }

    #[test]
    fn replies_are_flagged_until_read() {
        let mut c = I8042::new();
        c.write_command(CMD_SELF_TEST);
        assert_eq!(c.read_status() & STATUS_OUTPUT_FULL, STATUS_OUTPUT_FULL);
        assert_eq!(c.read_data(), SELF_TEST_PASSED);
        assert_eq!(c.read_status() & STATUS_OUTPUT_FULL, 0);
    }

    #[test]
    fn control_register_reads_back_what_was_written() {
        let mut c = I8042::new();
        c.write_command(CMD_READ_CTR);
        assert_eq!(c.read_data(), CTR_DEFAULT);

        c.write_command(CMD_WRITE_CTR);
        assert_eq!(c.write_data(0x54), I8042Outcome::Continue);
        c.write_command(CMD_READ_CTR);
        assert_eq!(c.read_data(), 0x54);
    }

    /// The sequence in Linux's `i8042_controller_init()` and
    /// `i8042_check_aux()`: every step must get its answer immediately.
    #[test]
    fn linux_probe_finds_a_controller_with_no_mouse_port() {
        let mut c = I8042::new();

        // The control register must read the same twice in a row.
        c.write_command(CMD_READ_CTR);
        let first = c.read_data();
        c.write_command(CMD_READ_CTR);
        assert_eq!(c.read_data(), first);

        // Disable the keyboard interface and its interrupt while probing.
        c.write_command(CMD_WRITE_CTR);
        c.write_data((first | 0x10) & !0x01);
        assert_eq!(c.read_status() & STATUS_OUTPUT_FULL, 0);

        // Mouse port loopback: a reply must arrive, and must not be flagged
        // as coming from the mouse port.
        c.write_command(CMD_AUX_LOOP);
        c.write_data(0x5a);
        let status = c.read_status();
        assert_eq!(status & STATUS_OUTPUT_FULL, STATUS_OUTPUT_FULL);
        assert_eq!(status & STATUS_AUX_DATA, 0);
        // Linux gives up on the loopback there and leaves the byte unread.

        // Mouse port test: anything but 0x00, 0xfa or 0xff means "absent".
        // The reply must be this command's, not the stale loopback byte.
        c.write_command(CMD_AUX_TEST);
        let result = c.read_data();
        assert_eq!(result, AUX_TEST_FAILED);
        assert!(result != 0x00 && result != 0xfa && result != 0xff);
        assert_eq!(c.read_status() & STATUS_OUTPUT_FULL, 0);
    }

    /// What Linux's atkbd driver does when it probes the keyboard port:
    /// each command byte must be refused straight away, through the
    /// interrupt, so that the probe fails without waiting for a timeout.
    #[test]
    fn bytes_for_the_keyboard_are_refused_with_an_interrupt() {
        let mut c = I8042::new();
        assert!(!c.interrupt_pending());

        // "Identify keyboard", sent twice because the driver retries once.
        for _ in 0..2 {
            assert_eq!(c.write_data(0xf2), I8042Outcome::Continue);
            assert!(c.interrupt_pending());
            assert_eq!(c.read_status() & STATUS_OUTPUT_FULL, STATUS_OUTPUT_FULL);
            assert_eq!(c.read_data(), KBD_RESEND);
            assert!(!c.interrupt_pending(), "reading the byte drops the line");
        }
    }

    #[test]
    fn keyboard_interrupt_respects_the_control_register() {
        let mut c = I8042::new();

        // Interrupt disabled: the refusal is still readable, but silent.
        c.write_command(CMD_WRITE_CTR);
        c.write_data(CTR_DEFAULT & !CTR_KBD_INT);
        c.write_data(0xf2);
        assert!(!c.interrupt_pending());
        assert_eq!(c.read_data(), KBD_RESEND);

        // Interface disabled: the byte goes nowhere.
        c.write_command(CMD_KBD_DISABLE);
        c.write_data(0xf2);
        assert_eq!(c.read_status() & STATUS_OUTPUT_FULL, 0);
    }

    #[test]
    fn controller_replies_do_not_raise_the_keyboard_interrupt() {
        let mut c = I8042::new();
        c.write_command(CMD_READ_CTR);
        assert_eq!(c.read_status() & STATUS_OUTPUT_FULL, STATUS_OUTPUT_FULL);
        assert!(!c.interrupt_pending());

        // A controller reply that replaces a keyboard byte clears the line.
        c.read_data();
        c.write_data(0xf2);
        assert!(c.interrupt_pending());
        c.write_command(CMD_SELF_TEST);
        assert!(!c.interrupt_pending());
    }

    #[test]
    fn a_new_command_cancels_a_pending_argument() {
        let mut c = I8042::new();
        c.write_command(CMD_WRITE_CTR);
        c.write_command(CMD_KBD_ENABLE);
        // With no command pending this is a byte for the keyboard.
        c.write_data(0x00);
        assert_eq!(c.read_data(), KBD_RESEND);
        c.write_command(CMD_READ_CTR);
        assert_eq!(c.read_data(), CTR_DEFAULT, "0x00 was not taken as the CTR");
    }

    #[test]
    fn reset_line_can_be_pulled_two_ways() {
        let mut c = I8042::new();
        assert_eq!(c.write_command(CMD_PULSE_RESET), I8042Outcome::Reset);

        // Writing the output port with bit 0 clear holds the line low.
        c.write_command(CMD_WRITE_OUTPUT_PORT);
        assert_eq!(c.write_data(0xdf), I8042Outcome::Continue, "A20 on");
        c.write_command(CMD_WRITE_OUTPUT_PORT);
        assert_eq!(c.write_data(0xfe), I8042Outcome::Reset);
    }
}
