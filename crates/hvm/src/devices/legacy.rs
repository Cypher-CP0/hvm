//! Legacy ports that only need to exist.

use std::io;

use crate::bus::BusDevice;

/// The CMOS real-time clock's index and data ports, with no clock behind
/// them.
///
/// Reads return zero. That keeps the "update in progress" bit clear, so a
/// guest that waits on it before reading the time does not spin.
pub struct CmosStub;

impl BusDevice for CmosStub {
    fn read(&mut self, _base: u64, _offset: u64, data: &mut [u8]) -> io::Result<()> {
        data.fill(0);
        Ok(())
    }

    fn write(&mut self, _base: u64, _offset: u64, _data: &[u8]) -> io::Result<()> {
        Ok(())
    }
}

/// Port 0x80, the POST diagnostic port. Linux writes to it as a short I/O
/// delay; nothing is expected to happen.
pub struct PostPort;

impl BusDevice for PostPort {
    fn read(&mut self, _base: u64, _offset: u64, data: &mut [u8]) -> io::Result<()> {
        data.fill(0xff);
        Ok(())
    }

    fn write(&mut self, _base: u64, _offset: u64, _data: &[u8]) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmos_never_reports_update_in_progress() {
        let mut cmos = CmosStub;
        // Select status register A, then read it.
        cmos.write(0x70, 0, &[0x0a]).unwrap();
        let mut value = [0xff];
        cmos.read(0x70, 1, &mut value).unwrap();
        assert_eq!(value[0] & 0x80, 0);
    }

    #[test]
    fn post_port_swallows_writes() {
        let mut post = PostPort;
        post.write(0x80, 0, &[0x42]).unwrap();
        let mut value = [0];
        post.read(0x80, 0, &mut value).unwrap();
        assert_eq!(value, [0xff]);
    }
}
