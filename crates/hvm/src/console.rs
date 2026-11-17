//! The host side of the guest's serial console: the user's terminal.

use std::io::{self, Read, Write};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

/// Ctrl-A: the prefix for commands to the VMM itself, as in QEMU and screen.
const ESCAPE: u8 = 0x01;

/// How long output may sit in the buffer waiting for more to batch with.
const FLUSH_DELAY: Duration = Duration::from_millis(2);

/// The terminal settings to put back on exit, if we changed them.
static SAVED_TERMIOS: Mutex<Option<libc::termios>> = Mutex::new(None);

/// Restores the terminal when dropped.
pub struct RawModeGuard(());

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Switch the terminal on stdin to raw mode, so that every keystroke
/// (Ctrl-C included) goes to the guest unprocessed and unechoed.
///
/// Returns `None` if stdin is not a terminal, in which case nothing changes.
pub fn enter_raw_mode() -> io::Result<Option<RawModeGuard>> {
    let fd = libc::STDIN_FILENO;
    // SAFETY: isatty only inspects the descriptor.
    if unsafe { libc::isatty(fd) } != 1 {
        return Ok(None);
    }
    // SAFETY: an all-zero termios is a valid value for tcgetattr to fill in.
    let mut termios: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: `termios` is a valid, writable termios.
    if unsafe { libc::tcgetattr(fd, &mut termios) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let original = termios;

    // No line editing, echo or signal keys; no input translation or flow
    // control. Output processing stays on so that our own messages still
    // get "\n" turned into "\r\n".
    termios.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
    termios.c_iflag &= !(libc::IXON
        | libc::ICRNL
        | libc::INLCR
        | libc::IGNCR
        | libc::BRKINT
        | libc::INPCK
        | libc::ISTRIP);
    termios.c_cc[libc::VMIN] = 1;
    termios.c_cc[libc::VTIME] = 0;

    // Record the original first, so a failure half-way is still undone.
    *SAVED_TERMIOS.lock().unwrap() = Some(original);
    // SAFETY: `termios` is a valid termios obtained from tcgetattr.
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &termios) } != 0 {
        let err = io::Error::last_os_error();
        restore_terminal();
        return Err(err);
    }
    Ok(Some(RawModeGuard(())))
}

/// Put the terminal back as [`enter_raw_mode`] found it. Safe to call from
/// any thread, any number of times.
pub fn restore_terminal() {
    if let Some(original) = SAVED_TERMIOS.lock().unwrap().take() {
        // SAFETY: `original` came from tcgetattr on this descriptor.
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &original) };
    }
}

/// What to do with the input after escape processing.
#[derive(Debug, PartialEq, Eq)]
enum EscapeOutcome {
    /// This many bytes at the front of the buffer go to the guest.
    Forward(usize),
    /// The user asked to quit; bytes before the request are dropped.
    Quit,
}

/// Strips Ctrl-A command sequences out of terminal input.
///
/// - `Ctrl-A x` quits the VMM.
/// - `Ctrl-A Ctrl-A` sends a literal Ctrl-A to the guest.
/// - Ctrl-A followed by anything else is swallowed.
#[derive(Default)]
struct EscapeFilter {
    /// The previous byte was an unconsumed Ctrl-A.
    armed: bool,
}

impl EscapeFilter {
    /// Filter `buf` in place.
    fn process(&mut self, buf: &mut [u8]) -> EscapeOutcome {
        let mut kept = 0;
        for i in 0..buf.len() {
            let byte = buf[i];
            if self.armed {
                self.armed = false;
                match byte {
                    b'x' | b'X' => return EscapeOutcome::Quit,
                    ESCAPE => {}
                    _ => continue,
                }
            } else if byte == ESCAPE {
                self.armed = true;
                continue;
            }
            buf[kept] = byte;
            kept += 1;
        }
        EscapeOutcome::Forward(kept)
    }
}

/// Terminal input for the guest, with Ctrl-A commands handled.
pub struct TerminalInput<R> {
    inner: R,
    filter: EscapeFilter,
}

impl<R: Read> TerminalInput<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            filter: EscapeFilter::default(),
        }
    }
}

impl<R: Read> Read for TerminalInput<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let n = self.inner.read(buf)?;
            if n == 0 {
                return Ok(0);
            }
            match self.filter.process(&mut buf[..n]) {
                // Nothing left after filtering is not end of input.
                EscapeOutcome::Forward(0) => continue,
                EscapeOutcome::Forward(kept) => return Ok(kept),
                EscapeOutcome::Quit => {
                    // The vCPU thread may be blocked inside the guest and
                    // there is no way to interrupt it yet, so leave from
                    // here. Guest output still buffered is lost.
                    restore_terminal();
                    eprintln!("\nhvm: terminated by Ctrl-A x");
                    std::process::exit(0);
                }
            }
        }
    }
}

struct OutputState {
    /// Bytes the guest has written that are not yet on the terminal.
    pending: Mutex<Vec<u8>>,
    /// Signalled when `pending` goes from empty to non-empty.
    ready: Condvar,
    /// Held while moving `pending` to the terminal, so that writes from the
    /// flusher thread and from `flush()` cannot interleave out of order.
    sink: Mutex<io::Stdout>,
}

impl OutputState {
    fn drain(&self) -> io::Result<()> {
        let mut sink = self.sink.lock().unwrap();
        let bytes = std::mem::take(&mut *self.pending.lock().unwrap());
        if bytes.is_empty() {
            return Ok(());
        }
        sink.write_all(&bytes)?;
        sink.flush()
    }
}

/// Guest console output to stdout.
///
/// The guest writes one byte at a time, and a prompt has no newline to
/// trigger a line-buffered flush. Writing each byte through would cost a
/// system call per character, so bytes are collected here and a background
/// thread pushes them out a moment later, a whole burst at a time.
pub struct TerminalOutput {
    state: Arc<OutputState>,
}

impl TerminalOutput {
    pub fn new() -> io::Result<Self> {
        let state = Arc::new(OutputState {
            pending: Mutex::new(Vec::new()),
            ready: Condvar::new(),
            sink: Mutex::new(io::stdout()),
        });
        let flusher = Arc::clone(&state);
        thread::Builder::new()
            .name("console-output".into())
            .spawn(move || loop {
                {
                    let mut pending = flusher.pending.lock().unwrap();
                    while pending.is_empty() {
                        pending = flusher.ready.wait(pending).unwrap();
                    }
                }
                // Let the rest of the burst arrive.
                thread::sleep(FLUSH_DELAY);
                if flusher.drain().is_err() {
                    return; // stdout is gone; nothing more to do
                }
            })?;
        Ok(Self { state })
    }
}

impl Write for TerminalOutput {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut pending = self.state.pending.lock().unwrap();
        let was_empty = pending.is_empty();
        pending.extend_from_slice(buf);
        if was_empty && !buf.is_empty() {
            self.state.ready.notify_one();
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.state.drain()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(chunks: &[&[u8]]) -> (Vec<u8>, bool) {
        let mut f = EscapeFilter::default();
        let mut out = Vec::new();
        for chunk in chunks {
            let mut buf = chunk.to_vec();
            match f.process(&mut buf) {
                EscapeOutcome::Forward(n) => out.extend_from_slice(&buf[..n]),
                EscapeOutcome::Quit => return (out, true),
            }
        }
        (out, false)
    }

    #[test]
    fn ordinary_input_passes_through() {
        assert_eq!(filter(&[b"ls -l\r"]), (b"ls -l\r".to_vec(), false));
        // Ctrl-C and Ctrl-D belong to the guest.
        assert_eq!(filter(&[&[0x03, 0x04]]), (vec![0x03, 0x04], false));
    }

    #[test]
    fn ctrl_a_x_quits() {
        assert_eq!(filter(&[b"ab\x01x"]), (vec![], true));
        assert_eq!(filter(&[b"\x01X"]), (vec![], true));
    }

    #[test]
    fn escape_sequence_may_span_reads() {
        // Typed by hand, the two keys arrive in separate reads.
        assert_eq!(filter(&[b"ab\x01", b"x"]), (b"ab".to_vec(), true));
    }

    #[test]
    fn double_ctrl_a_sends_one_to_the_guest() {
        assert_eq!(filter(&[b"a\x01\x01b"]), (b"a\x01b".to_vec(), false));
    }

    #[test]
    fn unknown_command_is_swallowed() {
        assert_eq!(filter(&[b"a\x01qb"]), (b"ab".to_vec(), false));
        // A lone x is just an x.
        assert_eq!(filter(&[b"x"]), (b"x".to_vec(), false));
    }

    #[test]
    fn terminal_input_skips_reads_that_filter_to_nothing() {
        /// Yields one byte per read, like a person typing.
        struct OneByOne(Vec<u8>);
        impl Read for OneByOne {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.0.is_empty() {
                    return Ok(0);
                }
                buf[0] = self.0.remove(0);
                Ok(1)
            }
        }
        let mut input = TerminalInput::new(OneByOne(b"\x01qhi".to_vec()));
        let mut out = Vec::new();
        input.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"hi");
    }
}
