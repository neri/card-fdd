//! Mass storage data phase (shared by BOT and CBI).
//!
//! Passes commands to [`Processor`] and exchanges data on Bulk IN / OUT according to the result.
//! Also handles cleanup when the transfer length differs from what the host expects (from the CBW for BOT, computed from the command for CBI).

use embassy_usb::driver::{Driver, EndpointError, EndpointIn, EndpointOut};

use core::sync::atomic::{AtomicU32, Ordering};

use embassy_time::{Instant, Timer};
use static_cell::ConstStaticCell;

use super::command::{DataPhase, Expected, Outcome, Processor};
use super::perf::{PERF, now_us};
use super::{Activity, notify_activity};
use crate::sound;
use crate::storage::Disk;
use crate::storage::geometry::Geometry;

/// Full Speed bulk packet size.
pub const MAX_PACKET: usize = 64;

/// Largest read / write chunk [bytes]: 8 blocks of 512 bytes (multi-block reads and writes,
/// [`Disk::max_read_blocks`] / [`Disk::max_write_blocks`]). Also covers one 1024-byte floppy sector.
const CHUNK_LEN: usize = 4096;

/// Work buffer for one chunk + parameters.
const BUF_LEN: usize = CHUNK_LEN + 64;

/// Result of the data phase of one command.
pub struct Transferred {
    /// Number of bytes actually transferred.
    pub done: usize,
    /// Whether the command succeeded (if false, the sense data has the error details).
    pub ok: bool,
}

/// Bulk IN / OUT endpoints and the work buffer.
pub struct DataPipe<'d, D: Driver<'d>> {
    pub ep_in: D::EndpointIn,
    pub ep_out: D::EndpointOut,
    buf: &'static mut [u8; BUF_LEN],
}

impl<'d, D: Driver<'d>> DataPipe<'d, D> {
    /// Call only once (there is one mass storage interface).
    pub fn new(ep_in: D::EndpointIn, ep_out: D::EndpointOut) -> Self {
        // In a static so the 4KB buffer never travels on the stack with the transport
        static BUF: ConstStaticCell<[u8; BUF_LEN]> = ConstStaticCell::new([0; BUF_LEN]);
        Self {
            ep_in,
            ep_out,
            buf: BUF.take(),
        }
    }

    /// Process a command and perform the data phase.
    ///
    /// `host_len` / `dir_in` are the transfer length and direction the host expects.
    pub async fn execute(
        &mut self,
        processor: &mut Processor,
        disk: &mut impl Disk,
        cb: &[u8],
        host_len: usize,
        dir_in: bool,
    ) -> Result<Transferred, EndpointError> {
        let outcome = self.prepare(processor, disk, cb);
        self.execute_prepared(processor, disk, cb, Expected { len: host_len, dir_in }, outcome).await
    }

    /// Process the command before starting endpoint I/O, so CBI can still stall its ADSC.
    pub(super) fn prepare(&mut self, processor: &mut Processor, disk: &mut impl Disk, cb: &[u8]) -> Outcome {
        processor.command(cb, &mut self.buf[..], disk)
    }

    pub(super) async fn execute_prepared(
        &mut self,
        processor: &mut Processor,
        disk: &mut impl Disk,
        cb: &[u8],
        expected: Expected,
        outcome: Outcome,
    ) -> Result<Transferred, EndpointError> {
        // Mark as transferring during the data phase so the medium is not ejected mid read/write
        let busy = matches!(outcome.phase, DataPhase::Read { .. } | DataPhase::Write { .. } | DataPhase::Out(_));
        processor.host().set_busy(busy);
        let result = self.data_phase(processor, disk, cb, expected.len, expected.dir_in, outcome).await;
        processor.host().set_busy(false);
        result
    }

    async fn data_phase(
        &mut self,
        processor: &mut Processor,
        disk: &mut impl Disk,
        cb: &[u8],
        host_len: usize,
        dir_in: bool,
        outcome: super::command::Outcome,
    ) -> Result<Transferred, EndpointError> {
        log::trace!("cmd {:#04x} len={} in={} -> {:?}", cb[0], host_len, dir_in, outcome);

        let mut done = 0usize;
        let mut ok = outcome.ok;
        let mut out_ended = false;

        match outcome.phase {
            DataPhase::None => {}
            DataPhase::In(len) => {
                if dir_in {
                    let len = len.min(host_len);
                    let (data, _) = self.buf.split_at(len);
                    send(&mut self.ep_in, data).await?;
                    done = len;
                }
            }
            DataPhase::Out(len) => {
                if !dir_in {
                    let (scratch, param) = self.buf.split_at_mut(BUF_LEN - 64);
                    let prefix = len.min(host_len).min(param.len());
                    let received = recv(&mut self.ep_out, &mut param[..prefix]).await?;
                    done = received.bytes.min(host_len);
                    out_ended = received.short;
                    let required = len.min(host_len);
                    if done < required && !out_ended {
                        let received = discard(&mut self.ep_out, required - done).await?;
                        done = (done + received.bytes).min(host_len);
                        out_ended = received.short;
                    }
                    if done < len {
                        processor.incomplete_data_out(cb[0]);
                        ok = false;
                    } else {
                        ok = processor.data_out(cb, &param[..prefix], scratch, disk);
                    }
                } else if len != 0 {
                    processor.incomplete_data_out(cb[0]);
                    ok = false;
                }
            }
            DataPhase::Read { lba, blocks } => {
                if dir_in {
                    let bs = block_size(disk);
                    notify_activity(Activity { lba, write: false });
                    let geometry = disk.geometry();
                    // Sectors per read: as many as the disk reads efficiently at once and the buffer holds
                    let per_read = disk.max_read_blocks().clamp(1, (CHUNK_LEN / bs).max(1) as u32);
                    let (mut sd_us, mut usb_us, mut count) = (0u32, 0u32, 0u32);
                    while count < blocks {
                        let room = ((host_len - done) / bs) as u32;
                        let n = per_read.min(blocks - count).min(room);
                        if n == 0 {
                            break;
                        }
                        let first = lba + count;
                        for k in 0..n {
                            mechanics(geometry, first + k, false).await;
                        }
                        let len = n as usize * bs;
                        let t0 = now_us();
                        let result = if n == 1 {
                            disk.read(first, &mut self.buf[..len])
                        } else {
                            disk.read_blocks(first, &mut self.buf[..len], bs)
                        };
                        if let Err(e) = result {
                            processor.transfer_failed(e);
                            ok = false;
                            break;
                        }
                        let t1 = now_us();
                        send(&mut self.ep_in, &self.buf[..len]).await?;
                        sd_us = sd_us.wrapping_add(t1.wrapping_sub(t0));
                        usb_us = usb_us.wrapping_add(now_us().wrapping_sub(t1));
                        count += n;
                        done += len;
                    }
                    PERF.read.record(count, sd_us, usb_us);
                }
            }
            DataPhase::Write { lba, blocks } => {
                if !dir_in {
                    let bs = block_size(disk);
                    notify_activity(Activity { lba, write: true });
                    let geometry = disk.geometry();
                    // Sectors per write: as many as the disk writes efficiently at once and the buffer holds
                    let per_write = disk.max_write_blocks().clamp(1, (CHUNK_LEN / bs).max(1) as u32);
                    let (mut sd_us, mut usb_us, mut count) = (0u32, 0u32, 0u32);
                    let mut next = 0u32;
                    while next < blocks {
                        let room = ((host_len - done) / bs) as u32;
                        let n = per_write.min(blocks - next).min(room);
                        if n == 0 {
                            processor.incomplete_data_out(cb[0]);
                            ok = false;
                            break;
                        }
                        let first = lba + next;
                        let len = n as usize * bs;
                        let t0 = now_us();
                        let received = recv(&mut self.ep_out, &mut self.buf[..len]).await?;
                        usb_us = usb_us.wrapping_add(now_us().wrapping_sub(t0));
                        done = (done + received.bytes).min(host_len);
                        // Write the whole sectors that arrived, even if the host stopped short
                        let whole = (received.bytes.min(len) / bs) as u32;
                        for k in 0..whole {
                            mechanics(geometry, first + k, true).await;
                        }
                        if ok && whole > 0 {
                            let wlen = whole as usize * bs;
                            let t1 = now_us();
                            let result = if whole == 1 {
                                disk.write(first, &self.buf[..wlen])
                            } else {
                                disk.write_blocks(first, &self.buf[..wlen], bs)
                            };
                            match result {
                                Ok(()) => count += whole,
                                Err(e) => {
                                    processor.transfer_failed(e);
                                    ok = false;
                                }
                            }
                            sd_us = sd_us.wrapping_add(now_us().wrapping_sub(t1));
                        }
                        if received.bytes != len {
                            processor.incomplete_data_out(cb[0]);
                            ok = false;
                            out_ended = received.short;
                            break;
                        }
                        next += n;
                    }
                    PERF.write.record(count, sd_us, usb_us);
                } else if blocks != 0 {
                    processor.incomplete_data_out(cb[0]);
                    ok = false;
                }
            }
        }

        // Cleanup when the length differs from what the host expected
        if dir_in {
            if done < host_len && done.is_multiple_of(MAX_PACKET) {
                // Signal the end of the transfer with a short (zero-length) packet
                self.ep_in.write(&[]).await?;
            }
        } else if done < host_len && !out_ended {
            discard(&mut self.ep_out, host_len - done).await?;
        }

        Ok(Transferred { done, ok })
    }
}

/// Head position (cylinder), plus the time of the last sound event and of the last access [ms].
static HEAD: AtomicU32 = AtomicU32::new(0);
static LAST_SOUND_MS: AtomicU32 = AtomicU32::new(0);
static LAST_ACCESS_MS: AtomicU32 = AtomicU32::new(0);

/// Interval for re-sending sound events (to keep the motor running).
const SOUND_REPOST_MS: u32 = 100;
/// If there has been no access for longer than this, the motor is stopped (realistic speed mode).
const MOTOR_STOP_MS: u32 = 2000;
/// Motor spin-up, single-track step, and post-seek settling times [ms] (realistic speed mode).
const SPIN_UP_MS: u64 = 300;
const STEP_MS: u64 = 3;
const SETTLE_MS: u64 = 15;
/// Time per revolution [ms] (300rpm).
const ROTATION_MS: u64 = 200;

/// Call before reading/writing each sector. Notifies the sound of head movement and, in realistic speed mode, waits for the mechanical time.
async fn mechanics(geometry: Option<Geometry>, lba: u32, write: bool) {
    let Some(g) = geometry else {
        return;
    };
    let (cylinder, _, _) = g.chs(lba);
    let prev = HEAD.swap(u32::from(cylinder), Ordering::Relaxed) as u16;
    let now = Instant::now().as_millis() as u32;
    let last_sound = LAST_SOUND_MS.load(Ordering::Relaxed);
    if prev != cylinder || now.wrapping_sub(last_sound) >= SOUND_REPOST_MS {
        LAST_SOUND_MS.store(now, Ordering::Relaxed);
        sound::post(sound::Event::Access { cylinder, write });
    }
    let last_access = LAST_ACCESS_MS.swap(now, Ordering::Relaxed);

    if sound::speed_emulation() {
        let mut ms = ROTATION_MS / u64::from(g.sectors_per_track.max(1));
        if now.wrapping_sub(last_access) > MOTOR_STOP_MS {
            ms += SPIN_UP_MS;
        }
        if prev != cylinder {
            ms += STEP_MS * u64::from(prev.abs_diff(cylinder)) + SETTLE_MS;
        }
        Timer::after_millis(ms).await;
    }
}

pub fn block_size(disk: &impl Disk) -> usize {
    disk.capacity().map_or(512, |c| usize::from(c.block_size))
}

/// Send `data` split into packets.
async fn send<E: EndpointIn>(ep: &mut E, data: &[u8]) -> Result<(), EndpointError> {
    for chunk in data.chunks(MAX_PACKET) {
        ep.write(chunk).await?;
    }
    Ok(())
}

struct Received {
    bytes: usize,
    short: bool,
}

/// Receive until `buf` is full or a short packet ends the data phase.
async fn recv<E: EndpointOut>(ep: &mut E, buf: &mut [u8]) -> Result<Received, EndpointError> {
    let mut got = 0;
    let mut packet = [0u8; MAX_PACKET];
    while got < buf.len() {
        let n = ep.read(&mut packet).await?;
        let copied = n.min(buf.len() - got);
        buf[got..got + copied].copy_from_slice(&packet[..copied]);
        got += n;
        if n < MAX_PACKET {
            return Ok(Received { bytes: got, short: true });
        }
    }
    Ok(Received { bytes: got, short: false })
}

/// Receive and discard `len` bytes.
async fn discard<E: EndpointOut>(ep: &mut E, len: usize) -> Result<Received, EndpointError> {
    let mut packet = [0u8; MAX_PACKET];
    let mut got = 0usize;
    while got < len {
        let n = ep.read(&mut packet).await?;
        got = got.saturating_add(n);
        if n < MAX_PACKET {
            return Ok(Received { bytes: got, short: true });
        }
    }
    Ok(Received { bytes: got, short: false })
}
