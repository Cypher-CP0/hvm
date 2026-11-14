//! Measurements: where VM exits come from, what they cost, and how long
//! boot takes. Every later optimisation is judged against these.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hypervisor::ExitKind;

/// Text the kernel logs when it starts the first user process, for example
/// "Run /init as init process".
const USERSPACE_MARKER: &[u8] = b"as init process";
/// Longest console line worth keeping to search for the marker.
const MAX_LINE: usize = 256;

pub const EVENT_VM_CREATED: &str = "vm created";
pub const EVENT_GUEST_LOADED: &str = "guest loaded";
pub const EVENT_VCPU_STARTED: &str = "vcpu started";
pub const EVENT_FIRST_OUTPUT: &str = "first console output";
pub const EVENT_USERSPACE: &str = "userspace started";
pub const EVENT_STOPPED: &str = "guest stopped";

/// Format a duration with a unit that keeps three or four significant
/// digits.
pub fn human(d: Duration) -> String {
    let ns = d.as_nanos();
    if ns < 10_000 {
        format!("{ns}ns")
    } else if ns < 10_000_000 {
        format!("{:.1}us", ns as f64 / 1e3)
    } else if ns < 10_000_000_000 {
        format!("{:.1}ms", ns as f64 / 1e6)
    } else {
        format!("{:.2}s", ns as f64 / 1e9)
    }
}

/// Counts and timings of VM exits, by kind.
#[derive(Default)]
pub struct ExitStats {
    counts: [u64; ExitKind::COUNT],
    /// Time the VMM spent handling exits of each kind.
    handling: [Duration; ExitKind::COUNT],
    /// Time spent inside the hypervisor's run call: guest code, plus exits
    /// the hypervisor handled without returning to the VMM.
    pub guest_time: Duration,
    /// Wall-clock time from first entry to stop.
    pub run_time: Duration,
}

impl ExitStats {
    pub fn record(&mut self, kind: ExitKind, in_guest: Duration, handling: Duration) {
        self.counts[kind as usize] += 1;
        self.handling[kind as usize] += handling;
        self.guest_time += in_guest;
    }

    pub fn count(&self, kind: ExitKind) -> u64 {
        self.counts[kind as usize]
    }

    pub fn total(&self) -> u64 {
        self.counts.iter().sum()
    }

    pub fn handling_time(&self) -> Duration {
        self.handling.iter().sum()
    }

    pub fn report(&self) {
        eprintln!("--- vm exits ---");
        eprintln!(
            "{:<12} {:>9} {:>10} {:>9}",
            "kind", "count", "vmm time", "each"
        );
        for kind in ExitKind::ALL {
            let n = self.count(kind);
            if n == 0 {
                continue;
            }
            let spent = self.handling[kind as usize];
            eprintln!(
                "{:<12} {:>9} {:>10} {:>9}",
                kind.to_string(),
                n,
                human(spent),
                human(Duration::from_nanos((spent.as_nanos() / n as u128) as u64))
            );
        }
        eprintln!(
            "{:<12} {:>9} {:>10}",
            "total",
            self.total(),
            human(self.handling_time())
        );
        eprintln!(
            "run time {} = guest and hypervisor {} + vmm {}",
            human(self.run_time),
            human(self.guest_time),
            human(self.run_time.saturating_sub(self.guest_time))
        );
    }
}

struct TimelineInner {
    start: Instant,
    events: Vec<(&'static str, Duration)>,
}

/// Named points in a VM's life, as offsets from when the VMM started.
/// Cloning gives another handle to the same timeline.
#[derive(Clone)]
pub struct Timeline(Arc<Mutex<TimelineInner>>);

impl Timeline {
    /// `start` is time zero: ideally the first thing the process did.
    pub fn new(start: Instant) -> Self {
        Self(Arc::new(Mutex::new(TimelineInner {
            start,
            events: Vec::new(),
        })))
    }

    /// Record that `event` happened now.
    pub fn mark(&self, event: &'static str) {
        let mut inner = self.0.lock().unwrap();
        let at = inner.start.elapsed();
        inner.events.push((event, at));
    }

    /// When `event` first happened, if it has.
    #[cfg(test)]
    pub fn get(&self, event: &str) -> Option<Duration> {
        let inner = self.0.lock().unwrap();
        inner
            .events
            .iter()
            .find(|(name, _)| *name == event)
            .map(|&(_, at)| at)
    }

    pub fn report(&self) {
        let inner = self.0.lock().unwrap();
        eprintln!("--- timeline ---");
        let mut previous = Duration::ZERO;
        for &(name, at) in &inner.events {
            eprintln!(
                "{:<22} {:>9}  (+{})",
                name,
                human(at),
                human(at.saturating_sub(previous))
            );
            previous = at;
        }
    }
}

/// Passes guest console output through while watching it for boot
/// milestones.
pub struct BootWatch<W> {
    inner: W,
    timeline: Timeline,
    seen_output: bool,
    seen_userspace: bool,
    /// The current, unfinished line.
    line: Vec<u8>,
}

impl<W> BootWatch<W> {
    pub fn new(inner: W, timeline: Timeline) -> Self {
        Self {
            inner,
            timeline,
            seen_output: false,
            seen_userspace: false,
            line: Vec::new(),
        }
    }

    fn observe(&mut self, buf: &[u8]) {
        if !self.seen_output && !buf.is_empty() {
            self.seen_output = true;
            self.timeline.mark(EVENT_FIRST_OUTPUT);
        }
        if self.seen_userspace {
            return;
        }
        for &byte in buf {
            if byte != b'\n' {
                if self.line.len() < MAX_LINE {
                    self.line.push(byte);
                }
                continue;
            }
            let found = self
                .line
                .windows(USERSPACE_MARKER.len())
                .any(|window| window == USERSPACE_MARKER);
            self.line.clear();
            if found {
                self.seen_userspace = true;
                self.timeline.mark(EVENT_USERSPACE);
                // Nothing more to look for.
                self.line = Vec::new();
                return;
            }
        }
    }
}

impl<W: Write> Write for BootWatch<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.observe(buf);
        self.inner.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_pick_a_readable_unit() {
        assert_eq!(human(Duration::from_nanos(840)), "840ns");
        assert_eq!(human(Duration::from_nanos(12_345)), "12.3us");
        assert_eq!(human(Duration::from_micros(15_100)), "15.1ms");
        assert_eq!(human(Duration::from_millis(1_042)), "1042.0ms");
        assert_eq!(human(Duration::from_secs(30)), "30.00s");
    }

    #[test]
    fn exit_stats_accumulate_by_kind() {
        let mut s = ExitStats::default();
        let us = Duration::from_micros;
        s.record(ExitKind::IoOut, us(10), us(2));
        s.record(ExitKind::IoOut, us(10), us(4));
        s.record(ExitKind::IoIn, us(5), us(1));

        assert_eq!(s.count(ExitKind::IoOut), 2);
        assert_eq!(s.count(ExitKind::IoIn), 1);
        assert_eq!(s.count(ExitKind::Hlt), 0);
        assert_eq!(s.total(), 3);
        assert_eq!(s.handling_time(), us(7));
        assert_eq!(s.guest_time, us(25));
    }

    #[test]
    fn timeline_keeps_events_in_order_and_is_shared_between_clones() {
        let t = Timeline::new(Instant::now());
        let other = t.clone();
        t.mark("a");
        other.mark("b");

        let (a, b) = (t.get("a").unwrap(), t.get("b").unwrap());
        assert!(a <= b);
        assert!(t.get("c").is_none());
    }

    fn watch(chunks: &[&[u8]]) -> (Timeline, Vec<u8>) {
        let timeline = Timeline::new(Instant::now());
        let mut w = BootWatch::new(Vec::new(), timeline.clone());
        for chunk in chunks {
            w.write_all(chunk).unwrap();
        }
        (timeline, w.inner)
    }

    #[test]
    fn boot_watch_passes_output_through_unchanged() {
        let (_, out) = watch(&[b"[    0.0] Linux\n", b"Run /init as init process\n# "]);
        assert_eq!(out, b"[    0.0] Linux\nRun /init as init process\n# ");
    }

    #[test]
    fn boot_watch_marks_first_output_and_userspace() {
        let (t, _) = watch(&[b"[    0.0] Linux version\n"]);
        assert!(t.get(EVENT_FIRST_OUTPUT).is_some());
        assert!(t.get(EVENT_USERSPACE).is_none());

        // The console delivers one byte at a time.
        let line = b"[    0.9] Run /bin/sh as init process\n";
        let bytes: Vec<&[u8]> = line.chunks(1).collect();
        let (t, _) = watch(&bytes);
        assert!(t.get(EVENT_USERSPACE).is_some());
    }

    #[test]
    fn boot_watch_needs_the_whole_marker_on_one_line() {
        let (t, _) = watch(&[b"as init\n process\n"]);
        assert!(t.get(EVENT_USERSPACE).is_none());
    }
}
