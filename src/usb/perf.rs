//! Transfer timing (SD reader plan R5a): where the time of READ / WRITE commands goes.
//!
//! For each direction, the time spent on the SD card (`Disk::read` / `Disk::write`) and on USB
//! (sending / receiving the sector's packets) is summed. The rest of the elapsed time since the last
//! reset is spent outside the data phase (waiting for the host, command and status stages).
//! Read with the `perf` development command; `perf reset` starts a new measurement.

use core::sync::atomic::{AtomicU32, Ordering};

use embassy_time::Instant;
use heapless::String;

/// Counters for one direction.
pub struct DirPerf {
    /// READ / WRITE commands with a data phase.
    pub commands: AtomicU32,
    /// Blocks (sectors) transferred.
    pub blocks: AtomicU32,
    /// Largest number of blocks in one command.
    pub max_blocks: AtomicU32,
    /// Time on the SD card [µs].
    pub sd_us: AtomicU32,
    /// Time on USB [µs].
    pub usb_us: AtomicU32,
}

impl DirPerf {
    const fn new() -> Self {
        Self {
            commands: AtomicU32::new(0),
            blocks: AtomicU32::new(0),
            max_blocks: AtomicU32::new(0),
            sd_us: AtomicU32::new(0),
            usb_us: AtomicU32::new(0),
        }
    }

    fn reset(&self) {
        for c in [&self.commands, &self.blocks, &self.max_blocks, &self.sd_us, &self.usb_us] {
            c.store(0, Ordering::Relaxed);
        }
    }

    /// Record one command's data phase.
    pub fn record(&self, blocks: u32, sd_us: u32, usb_us: u32) {
        if blocks == 0 {
            return;
        }
        self.commands.fetch_add(1, Ordering::Relaxed);
        self.blocks.fetch_add(blocks, Ordering::Relaxed);
        self.max_blocks.fetch_max(blocks, Ordering::Relaxed);
        self.sd_us.fetch_add(sd_us, Ordering::Relaxed);
        self.usb_us.fetch_add(usb_us, Ordering::Relaxed);
    }

    /// One report line, e.g. `R 120 cmd 2880 blk (max 8) | SD 1234 ms (428 us/blk) | USB ... | 260 KB/s`.
    fn line(&self, label: &str, block_size: u32) -> String<128> {
        use core::fmt::Write;
        let commands = self.commands.load(Ordering::Relaxed);
        let blocks = self.blocks.load(Ordering::Relaxed);
        let sd_us = self.sd_us.load(Ordering::Relaxed);
        let usb_us = self.usb_us.load(Ordering::Relaxed);
        let mut s = String::new();
        let _ = write!(
            s,
            "{} {} cmd {} blk (max {}) | SD {} ms ({} us/blk) | USB {} ms ({} us/blk) | {} KB/s",
            label,
            commands,
            blocks,
            self.max_blocks.load(Ordering::Relaxed),
            sd_us / 1000,
            per_block(sd_us, blocks),
            usb_us / 1000,
            per_block(usb_us, blocks),
            kb_per_s(u64::from(blocks) * u64::from(block_size), u64::from(sd_us) + u64::from(usb_us)),
        );
        s
    }
}

pub struct Perf {
    pub read: DirPerf,
    pub write: DirPerf,
    /// Time of the last reset [ms since boot].
    since_ms: AtomicU32,
}

pub static PERF: Perf = Perf {
    read: DirPerf::new(),
    write: DirPerf::new(),
    since_ms: AtomicU32::new(0),
};

impl Perf {
    pub fn reset(&self) {
        self.read.reset();
        self.write.reset();
        self.since_ms.store(Instant::now().as_millis() as u32, Ordering::Relaxed);
    }

    /// Report lines (for the log). Block size is assumed to be 512 bytes.
    pub fn lines(&self) -> [String<128>; 3] {
        use core::fmt::Write;
        let elapsed_ms = (Instant::now().as_millis() as u32).wrapping_sub(self.since_ms.load(Ordering::Relaxed));
        let busy_ms = [&self.read.sd_us, &self.read.usb_us, &self.write.sd_us, &self.write.usb_us]
            .iter()
            .map(|c| u64::from(c.load(Ordering::Relaxed)))
            .sum::<u64>()
            / 1000;
        let mut head = String::new();
        let _ = write!(
            head,
            "perf: {} ms since reset, {} ms in data phases",
            elapsed_ms, busy_ms
        );
        [head, self.read.line("R", 512), self.write.line("W", 512)]
    }

    pub fn log(&self) {
        for line in self.lines() {
            log::info!("{}", line);
        }
    }
}

/// Microseconds since boot, truncated to 32 bits (differences stay correct across the wrap).
pub fn now_us() -> u32 {
    Instant::now().as_micros() as u32
}

fn per_block(us: u32, blocks: u32) -> u32 {
    us.checked_div(blocks).unwrap_or(0)
}

fn kb_per_s(bytes: u64, us: u64) -> u64 {
    (bytes * 1_000_000).checked_div(us).unwrap_or(0) / 1024
}
