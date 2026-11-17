//! Device buses: which device answers at which address.
//!
//! A machine has two address spaces a guest can reach devices through, I/O
//! ports and memory-mapped I/O, and each is a [`Bus`]. A device claims one
//! or more address ranges on a bus; a guest access that exits to the VMM is
//! looked up by address and handed to the device that claimed it.

use std::fmt;
use std::io;
use std::sync::{Arc, Mutex};

/// A device that can sit on a [`Bus`].
///
/// `base` is the start of the range the access fell in and `offset` the
/// distance into it, so a device that claims several ranges can tell them
/// apart. `data.len()` is the access width, except for string I/O on ports
/// (`rep outsb`), where it is that many one-byte accesses to the same port.
pub trait BusDevice: Send {
    fn read(&mut self, base: u64, offset: u64, data: &mut [u8]) -> io::Result<()>;
    fn write(&mut self, base: u64, offset: u64, data: &[u8]) -> io::Result<()>;
}

/// A device as buses hold it: shared, because whoever feeds the device from
/// outside the guest (a terminal, a disk, a network) holds it too.
pub type SharedDevice = Arc<Mutex<dyn BusDevice>>;

#[derive(Debug, PartialEq, Eq)]
pub enum BusError {
    /// A range must cover at least one address.
    Empty { base: u64 },
    /// `base + len` does not fit in the address space.
    Overflow { base: u64, len: u64 },
    /// The range collides with one already on the bus.
    Overlap {
        base: u64,
        len: u64,
        existing_base: u64,
        existing_len: u64,
    },
}

impl fmt::Display for BusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            BusError::Empty { base } => write!(f, "empty bus range at {base:#x}"),
            BusError::Overflow { base, len } => {
                write!(
                    f,
                    "bus range {base:#x}+{len:#x} overflows the address space"
                )
            }
            BusError::Overlap {
                base,
                len,
                existing_base,
                existing_len,
            } => write!(
                f,
                "bus range {base:#x}+{len:#x} overlaps {existing_base:#x}+{existing_len:#x}"
            ),
        }
    }
}

impl std::error::Error for BusError {}

struct Range {
    base: u64,
    len: u64,
    device: SharedDevice,
}

/// One address space and the devices on it.
///
/// Ranges are fixed once the machine is built, so lookups need no lock; only
/// the device found is locked, for the length of the access.
#[derive(Default)]
pub struct Bus {
    /// Sorted by `base`, never overlapping.
    ranges: Vec<Range>,
}

impl Bus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Give `device` the addresses `base..base + len`.
    pub fn insert(&mut self, base: u64, len: u64, device: SharedDevice) -> Result<(), BusError> {
        if len == 0 {
            return Err(BusError::Empty { base });
        }
        let end = base
            .checked_add(len)
            .ok_or(BusError::Overflow { base, len })?;
        // The first existing range that starts at or after the new one's end
        // marks where it goes; the one before that is the only candidate
        // for a collision.
        let index = self.ranges.partition_point(|r| r.base < end);
        if let Some(previous) = index.checked_sub(1).map(|i| &self.ranges[i]) {
            if previous.base + previous.len > base {
                return Err(BusError::Overlap {
                    base,
                    len,
                    existing_base: previous.base,
                    existing_len: previous.len,
                });
            }
        }
        self.ranges.insert(index, Range { base, len, device });
        Ok(())
    }

    /// The range containing `addr`, if any.
    fn find(&self, addr: u64) -> Option<&Range> {
        let index = self.ranges.partition_point(|r| r.base <= addr);
        let range = &self.ranges[index.checked_sub(1)?];
        (addr - range.base < range.len).then_some(range)
    }

    /// Hand a read to the device at `addr`. Returns `false`, leaving `data`
    /// untouched, if no device claims the address.
    ///
    /// The device is chosen by the access's first address alone.
    pub fn read(&self, addr: u64, data: &mut [u8]) -> io::Result<bool> {
        let Some(range) = self.find(addr) else {
            return Ok(false);
        };
        let mut device = range.device.lock().unwrap();
        device.read(range.base, addr - range.base, data)?;
        Ok(true)
    }

    /// Hand a write to the device at `addr`. Returns `false` if no device
    /// claims the address.
    pub fn write(&self, addr: u64, data: &[u8]) -> io::Result<bool> {
        let Some(range) = self.find(addr) else {
            return Ok(false);
        };
        let mut device = range.device.lock().unwrap();
        device.write(range.base, addr - range.base, data)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records every access it receives.
    #[derive(Default)]
    struct Recorder {
        reads: Vec<(u64, u64, usize)>,
        writes: Vec<(u64, u64, Vec<u8>)>,
    }

    impl BusDevice for Recorder {
        fn read(&mut self, base: u64, offset: u64, data: &mut [u8]) -> io::Result<()> {
            self.reads.push((base, offset, data.len()));
            data.fill(0x5a);
            Ok(())
        }

        fn write(&mut self, base: u64, offset: u64, data: &[u8]) -> io::Result<()> {
            self.writes.push((base, offset, data.to_vec()));
            Ok(())
        }
    }

    fn recorder() -> Arc<Mutex<Recorder>> {
        Arc::new(Mutex::new(Recorder::default()))
    }

    #[test]
    fn accesses_reach_the_device_with_base_and_offset() {
        let dev = recorder();
        let mut bus = Bus::new();
        bus.insert(0x3f8, 8, dev.clone()).unwrap();

        let mut buf = [0u8; 1];
        assert!(bus.read(0x3fd, &mut buf).unwrap());
        assert_eq!(buf, [0x5a]);
        assert!(bus.write(0x3f8, b"x").unwrap());

        let dev = dev.lock().unwrap();
        assert_eq!(dev.reads, vec![(0x3f8, 5, 1)]);
        assert_eq!(dev.writes, vec![(0x3f8, 0, b"x".to_vec())]);
    }

    #[test]
    fn range_boundaries_are_exact() {
        let dev = recorder();
        let mut bus = Bus::new();
        bus.insert(0x100, 0x10, dev.clone()).unwrap();

        let mut buf = [0u8; 1];
        assert!(!bus.read(0xff, &mut buf).unwrap());
        assert!(bus.read(0x100, &mut buf).unwrap());
        assert!(bus.read(0x10f, &mut buf).unwrap());
        assert!(!bus.read(0x110, &mut buf).unwrap());
        assert!(!bus.write(0x110, &buf).unwrap());
        assert_eq!(dev.lock().unwrap().reads.len(), 2);
    }

    #[test]
    fn unclaimed_reads_leave_the_buffer_alone() {
        let bus = Bus::new();
        let mut buf = [0x11u8; 4];
        assert!(!bus.read(0x1234, &mut buf).unwrap());
        assert_eq!(buf, [0x11; 4]);
    }

    #[test]
    fn lookup_picks_the_right_device_among_many() {
        let devs: Vec<_> = (0..4).map(|_| recorder()).collect();
        let mut bus = Bus::new();
        // Inserted out of order on purpose.
        for (i, base) in [(2, 0x300), (0, 0x100), (3, 0x400), (1, 0x200)] {
            bus.insert(base, 0x80, devs[i].clone()).unwrap();
        }

        for (i, dev) in devs.iter().enumerate() {
            let base = 0x100 * (i as u64 + 1);
            assert!(bus.write(base + 0x7f, &[i as u8]).unwrap());
            assert_eq!(
                dev.lock().unwrap().writes,
                vec![(base, 0x7f, vec![i as u8])]
            );
            // The gap between this range and the next belongs to no one.
            assert!(!bus.write(base + 0x80, &[0]).unwrap());
        }
    }

    #[test]
    fn one_device_may_claim_several_ranges() {
        let dev = recorder();
        let mut bus = Bus::new();
        bus.insert(0x60, 1, dev.clone()).unwrap();
        bus.insert(0x64, 1, dev.clone()).unwrap();

        bus.write(0x60, &[1]).unwrap();
        bus.write(0x64, &[2]).unwrap();
        assert!(!bus.write(0x61, &[3]).unwrap());
        assert_eq!(
            dev.lock().unwrap().writes,
            vec![(0x60, 0, vec![1]), (0x64, 0, vec![2])]
        );
    }

    #[test]
    fn overlapping_ranges_are_rejected() {
        let mut bus = Bus::new();
        bus.insert(0x100, 0x10, recorder()).unwrap();

        let overlap = |base, len| {
            Err(BusError::Overlap {
                base,
                len,
                existing_base: 0x100,
                existing_len: 0x10,
            })
        };
        let mut try_insert = |base, len| bus.insert(base, len, recorder());
        assert_eq!(try_insert(0x100, 0x10), overlap(0x100, 0x10)); // identical
        assert_eq!(try_insert(0x0f8, 0x10), overlap(0x0f8, 0x10)); // tail overlaps
        assert_eq!(try_insert(0x10f, 0x10), overlap(0x10f, 0x10)); // head overlaps
        assert_eq!(try_insert(0x104, 0x02), overlap(0x104, 0x02)); // inside
        assert_eq!(try_insert(0x0f0, 0x40), overlap(0x0f0, 0x40)); // around
                                                                   // Touching on either side is fine.
        assert_eq!(try_insert(0x0f0, 0x10), Ok(()));
        assert_eq!(try_insert(0x110, 0x10), Ok(()));
    }

    #[test]
    fn degenerate_ranges_are_rejected() {
        let mut bus = Bus::new();
        assert_eq!(
            bus.insert(0x100, 0, recorder()),
            Err(BusError::Empty { base: 0x100 })
        );
        assert_eq!(
            bus.insert(u64::MAX, 2, recorder()),
            Err(BusError::Overflow {
                base: u64::MAX,
                len: 2
            })
        );
    }
}
