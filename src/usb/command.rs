//! UFI / SCSI command processing (hardware independent).
//!
//! Interprets command blocks received from the transport (Bulk-Only / CBI),
//! prepares response data, and returns the kind of data phase. The actual USB transfer is done by the caller.
//!
//! UFI (USB Floppy Interface, subclass 0x04) and SCSI transparent (subclass 0x06) share
//! most commands, so the differences are switched by [`CommandSet`].

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::storage::{Capacity, Disk};
use crate::storage::geometry::{self, Geometry};

/// Command set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandSet {
    /// UFI (floppy). Returns the Flexible Disk Page for MODE SENSE and supports FORMAT UNIT.
    Ufi,
    /// SCSI transparent (removable disk).
    Scsi,
}

/// Sense data (details of the last error).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Sense {
    pub key: u8,
    pub asc: u8,
    pub ascq: u8,
}

impl Sense {
    pub const NONE: Sense = Sense::new(0x00, 0x00, 0x00);
    pub const NOT_READY_NO_MEDIUM: Sense = Sense::new(0x02, 0x3A, 0x00);
    pub const MEDIUM_ERROR_READ: Sense = Sense::new(0x03, 0x11, 0x00);
    pub const MEDIUM_ERROR_WRITE: Sense = Sense::new(0x03, 0x0C, 0x00);
    pub const INVALID_COMMAND: Sense = Sense::new(0x05, 0x20, 0x00);
    pub const LBA_OUT_OF_RANGE: Sense = Sense::new(0x05, 0x21, 0x00);
    pub const INVALID_FIELD: Sense = Sense::new(0x05, 0x24, 0x00);
    pub const MEDIUM_CHANGED: Sense = Sense::new(0x06, 0x28, 0x00);
    pub const WRITE_PROTECTED: Sense = Sense::new(0x07, 0x27, 0x00);
    pub const DATA_PHASE_ERROR: Sense = Sense::new(0x0B, 0x4B, 0x00);

    pub const fn new(key: u8, asc: u8, ascq: u8) -> Self {
        Self { key, asc, ascq }
    }
}

/// Kind of data phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataPhase {
    /// No data.
    None,
    /// Send the first `n` bytes of `buf` to the host.
    In(usize),
    /// Receive `n` bytes from the host and pass them to [`Processor::data_out`].
    Out(usize),
    /// Read `blocks` sectors from `lba` and send them to the host.
    Read { lba: u32, blocks: u32 },
    /// Write `blocks` sectors received from the host starting at `lba`.
    Write { lba: u32, blocks: u32 },
}

/// Result of processing a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    pub phase: DataPhase,
    /// false means error (details in the sense data).
    pub ok: bool,
}

impl Outcome {
    const fn good(phase: DataPhase) -> Self {
        Self { phase, ok: true }
    }
    const fn failed() -> Self {
        Self {
            phase: DataPhase::None,
            ok: false,
        }
    }
}

/// Names returned by INQUIRY.
#[derive(Debug, Clone, Copy)]
pub struct Identity {
    pub vendor: [u8; 8],
    pub product: [u8; 16],
    pub revision: [u8; 4],
}

/// State of the exchange with the host. Written by the MSC task and read by the UI (the UI clears eject requests).
pub struct HostState {
    /// Medium removal prevented by PREVENT ALLOW MEDIUM REMOVAL.
    prevent_removal: AtomicBool,
    /// Eject requested by START STOP UNIT.
    eject_requested: AtomicBool,
    /// In the middle of a data transfer (no ejecting meanwhile).
    busy: AtomicBool,
    /// Configured by the host (SET_CONFIGURATION).
    configured: AtomicBool,
    /// The bus is suspended.
    suspended: AtomicBool,
    /// A USB bus reset happened and no command has arrived since (a new connection is starting).
    new_connection: AtomicBool,
    /// Diagnostic counters (shown on the device screen).
    pub stats: UsbStats,
}

/// USB state shown in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// Not connected to a host (or not configured).
    Detached,
    /// Recognized by the host and usable.
    Ready,
    /// Suspended by the host.
    Suspended,
}

/// Diagnostic counters.
pub struct UsbStats {
    /// Number of commands processed.
    pub commands: AtomicU32,
    /// Number of failed commands.
    pub failed: AtomicU32,
    /// Last failed command (opcode << 24 | sense key << 16 | ASC << 8 | ASCQ).
    pub last_failure: AtomicU32,
    /// Last unsupported opcode received (+ 1; 0 means none).
    pub last_unknown: AtomicU32,
    /// Number of disk errors during read/write.
    pub disk_errors: AtomicU32,
    /// Number of CBI command block resets.
    pub resets: AtomicU32,
    /// Number of rejected ADSCs.
    pub rejected: AtomicU32,
    /// Media-related command failures reported via the ADSC status stage.
    pub adsc_stalls: AtomicU32,
    /// History of recent commands (opcode << 24 | 0x800000 if failed | sense key << 16 ...; see [`UsbStats::record`] below).
    pub history: [AtomicU32; HISTORY_LEN],
    /// Position of the next history entry to write (ever increasing).
    pub history_pos: AtomicU32,
    /// Current processing stage ([`Stage`]).
    pub stage: AtomicU32,
    /// Time [ms] when USB configuration (SET_CONFIGURATION) completed.
    pub configured_ms: AtomicU32,
    /// Time [ms] from the preceding bus reset to the last completed configuration.
    pub enumeration_ms: AtomicU32,
    /// Time [ms] when the first command after configuration arrived (0 if not yet).
    pub first_command_ms: AtomicU32,
    /// Time [ms] and opcode of the previous command.
    pub last_command_ms: AtomicU32,
    pub last_op: AtomicU32,
    /// Places where the gap between commands was at least [`SLOW_GAP_MS`] (previous opcode << 24 | opcode << 16 | gap [ms]).
    pub gaps: [AtomicU32; HISTORY_LEN],
    pub gap_pos: AtomicU32,
    /// Number of USB bus resets, and the time [ms] of the latest bus reset.
    pub bus_resets: AtomicU32,
    last_reset_ms: AtomicU32,
    /// Number of SET_CONFIGURATIONs.
    pub configurations: AtomicU32,
    /// Number of unread IN data discarded when a new command arrived (completion interrupt / Bulk IN).
    pub discarded_int: AtomicU32,
    pub discarded_bulk: AtomicU32,
    /// CBI endpoint numbers (completion interrupt / Bulk IN). Used by the diagnostics display to check for unread data.
    pub int_ep: AtomicU32,
    pub bulk_in_ep: AtomicU32,
    /// Number of USB suspends and resumes.
    pub suspends: AtomicU32,
    pub resumes: AtomicU32,
    /// Time [ms] of the last suspend.
    pub last_suspend_ms: AtomicU32,
}

pub const HISTORY_LEN: usize = 8;
/// Minimum command gap to record [ms].
pub const SLOW_GAP_MS: u32 = 200;

/// Transfer processing stage (diagnostics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Stage {
    /// Waiting for the next command.
    WaitCommand = 0,
    /// Transferring data.
    Data = 1,
    /// Sending status (CBI completion interrupt / BOT CSW).
    Status = 2,
}

impl Stage {
    pub fn from_u32(v: u32) -> Self {
        match v {
            1 => Stage::Data,
            2 => Stage::Status,
            _ => Stage::WaitCommand,
        }
    }
}

impl UsbStats {
    pub const fn new() -> Self {
        Self {
            commands: AtomicU32::new(0),
            failed: AtomicU32::new(0),
            last_failure: AtomicU32::new(0),
            last_unknown: AtomicU32::new(0),
            disk_errors: AtomicU32::new(0),
            resets: AtomicU32::new(0),
            rejected: AtomicU32::new(0),
            adsc_stalls: AtomicU32::new(0),
            history: [const { AtomicU32::new(0) }; HISTORY_LEN],
            history_pos: AtomicU32::new(0),
            stage: AtomicU32::new(0),
            configured_ms: AtomicU32::new(0),
            enumeration_ms: AtomicU32::new(0),
            first_command_ms: AtomicU32::new(0),
            last_command_ms: AtomicU32::new(0),
            last_op: AtomicU32::new(0),
            gaps: [const { AtomicU32::new(0) }; HISTORY_LEN],
            gap_pos: AtomicU32::new(0),
            bus_resets: AtomicU32::new(0),
            last_reset_ms: AtomicU32::new(0),
            configurations: AtomicU32::new(0),
            discarded_int: AtomicU32::new(0),
            discarded_bulk: AtomicU32::new(0),
            int_ep: AtomicU32::new(0),
            bulk_in_ep: AtomicU32::new(0),
            suspends: AtomicU32::new(0),
            resumes: AtomicU32::new(0),
            last_suspend_ms: AtomicU32::new(0),
        }
    }

    /// Call when a USB suspend is detected.
    pub fn suspended(&self) {
        Self::inc(&self.suspends);
        self.last_suspend_ms.store(Self::now_ms(), Ordering::Relaxed);
    }

    /// Call on a USB bus reset.
    pub fn bus_reset(&self) {
        self.last_reset_ms.store(Self::now_ms(), Ordering::Relaxed);
        Self::inc(&self.bus_resets);
    }

    /// Current time [ms] (for the diagnostics display).
    pub fn now(&self) -> u32 {
        Self::now_ms()
    }

    fn now_ms() -> u32 {
        embassy_time::Instant::now().as_millis() as u32
    }

    /// Call when USB configuration completes.
    pub fn configured(&self) {
        let now = Self::now_ms();
        if self.bus_resets.load(Ordering::Relaxed) != 0 {
            let reset = self.last_reset_ms.load(Ordering::Relaxed);
            self.enumeration_ms.store(now.wrapping_sub(reset), Ordering::Relaxed);
        }
        Self::inc(&self.configurations);
        self.configured_ms.store(now, Ordering::Relaxed);
        self.first_command_ms.store(0, Ordering::Relaxed);
        self.last_command_ms.store(0, Ordering::Relaxed);
    }

    /// Call when a command arrives. Records long gaps.
    pub fn command_arrived(&self, op: u8) {
        let now = Self::now_ms().max(1);
        if self.first_command_ms.load(Ordering::Relaxed) == 0 {
            self.first_command_ms.store(now, Ordering::Relaxed);
        }
        let last = self.last_command_ms.swap(now, Ordering::Relaxed);
        let prev_op = self.last_op.swap(u32::from(op), Ordering::Relaxed);
        if last != 0 {
            let gap = now.wrapping_sub(last);
            if gap >= SLOW_GAP_MS {
                let v = (prev_op << 24) | (u32::from(op) << 16) | gap.min(0xFFFF);
                let pos = self.gap_pos.fetch_add(1, Ordering::Relaxed) as usize;
                self.gaps[pos % HISTORY_LEN].store(v, Ordering::Relaxed);
            }
        }
    }

    pub fn inc(counter: &AtomicU32) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn set_stage(&self, stage: Stage) {
        self.stage.store(stage as u32, Ordering::Relaxed);
    }

    /// Record the command result in the history. `sense` is `None` on success.
    ///
    /// Format: valid bit 0x8000_0000 | opcode << 16 | 0x8000 if failed | ASC << 8 | ASCQ
    pub fn record(&self, op: u8, sense: Option<Sense>) {
        let pos = self.history_pos.fetch_add(1, Ordering::Relaxed) as usize;
        self.history[pos % HISTORY_LEN].store(history_entry(op, sense), Ordering::Relaxed);
    }

    /// Replace the latest history entry (a command that was accepted but failed in its data phase).
    fn amend_last(&self, op: u8, sense: Sense) {
        let pos = self.history_pos.load(Ordering::Relaxed).wrapping_sub(1) as usize;
        self.history[pos % HISTORY_LEN].store(history_entry(op, Some(sense)), Ordering::Relaxed);
    }
}

fn history_entry(op: u8, sense: Option<Sense>) -> u32 {
    let mut v = 0x8000_0000 | (u32::from(op) << 16);
    if let Some(s) = sense {
        v |= 0x8000 | (u32::from(s.asc) << 8) | u32::from(s.ascq);
    }
    v
}

impl Default for UsbStats {
    fn default() -> Self {
        Self::new()
    }
}

impl Default for HostState {
    fn default() -> Self {
        Self::new()
    }
}

impl HostState {
    pub const fn new() -> Self {
        Self {
            prevent_removal: AtomicBool::new(false),
            eject_requested: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            configured: AtomicBool::new(false),
            suspended: AtomicBool::new(false),
            new_connection: AtomicBool::new(false),
            stats: UsbStats::new(),
        }
    }

    pub fn link_state(&self) -> LinkState {
        if !self.configured.load(Ordering::Acquire) {
            LinkState::Detached
        } else if self.suspended.load(Ordering::Acquire) {
            LinkState::Suspended
        } else {
            LinkState::Ready
        }
    }

    /// Notifications from the USB stack (called from each Handler method).
    pub fn on_configured(&self, configured: bool) {
        self.configured.store(configured, Ordering::Release);
        if configured {
            self.stats.configured();
        }
    }

    pub fn on_suspended(&self, suspended: bool) {
        self.suspended.store(suspended, Ordering::Release);
        if suspended {
            self.stats.suspended();
        } else {
            UsbStats::inc(&self.stats.resumes);
        }
    }

    pub fn on_reset(&self) {
        self.configured.store(false, Ordering::Release);
        self.suspended.store(false, Ordering::Release);
        self.prevent_removal.store(false, Ordering::Release);
        self.eject_requested.store(false, Ordering::Release);
        self.new_connection.store(true, Ordering::Release);
        self.stats.bus_reset();
    }

    pub fn prevent_removal(&self) -> bool {
        self.prevent_removal.load(Ordering::Acquire)
    }

    /// Read and clear the eject request.
    pub fn take_eject_request(&self) -> bool {
        self.eject_requested.swap(false, Ordering::AcqRel)
    }

    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }

    pub fn set_busy(&self, busy: bool) {
        self.busy.store(busy, Ordering::Release);
    }
}

/// Command processor. Holds the sense data and media change state.
pub struct Processor {
    set: CommandSet,
    identity: Identity,
    sense: Sense,
    /// Report UNIT ATTENTION (media change) on the next command.
    unit_attention: bool,
    /// Last seen medium generation number ([`Disk::media_generation`]).
    seen_generation: u32,
    host: &'static HostState,
}

/// Maximum drive capacity (for READ FORMAT CAPACITIES when no medium is present).
const DRIVE_MAX: Geometry = geometry::FORMATS[0].geometry;

/// Value used to fill the medium on FORMAT UNIT (initial value for IBM-format formatting).
const FORMAT_FILL: u8 = 0xF6;

impl Processor {
    pub const fn new(set: CommandSet, identity: Identity, host: &'static HostState) -> Self {
        Self {
            set,
            identity,
            sense: Sense::NONE,
            unit_attention: false,
            seen_generation: 0,
            host,
        }
    }

    pub fn command_set(&self) -> CommandSet {
        self.set
    }

    pub fn host(&self) -> &'static HostState {
        self.host
    }

    /// Report that the medium was swapped (insert/eject). UNIT ATTENTION is returned on the next command.
    pub fn media_changed(&mut self) {
        self.unit_attention = true;
    }

    /// Set the sense data when a read/write fails midway.
    pub fn transfer_failed(&mut self, error: crate::storage::DiskError) {
        UsbStats::inc(&self.host.stats.disk_errors);
        self.sense = match error {
            crate::storage::DiskError::Read => Sense::MEDIUM_ERROR_READ,
            crate::storage::DiskError::Write => Sense::MEDIUM_ERROR_WRITE,
            crate::storage::DiskError::NoMedium => Sense::NOT_READY_NO_MEDIUM,
        };
    }

    /// The host sent less data-out than the command needs (or the wrong direction).
    ///
    /// The command was already recorded as accepted, so this turns that history entry into a failure
    /// instead of adding a second one. A medium error reported earlier in the same transfer is kept.
    pub(super) fn incomplete_data_out(&mut self, op: u8) {
        if self.sense == Sense::NONE {
            self.sense = Sense::DATA_PHASE_ERROR;
        }
        let stats = &self.host.stats;
        UsbStats::inc(&stats.failed);
        stats.last_failure.store(failure_entry(op, self.sense), Ordering::Relaxed);
        stats.amend_last(op, self.sense);
    }

    /// Bus reset (e.g. BOT Mass Storage Reset).
    pub fn reset(&mut self) {
        self.sense = Sense::NONE;
    }

    /// Current sense data (used by the CBI interrupt transfer).
    pub fn sense(&self) -> Sense {
        self.sense
    }

    /// Process a command. Response data is written to `buf` (`buf` must be at least 64 bytes).
    #[inline(never)]
    pub fn command(&mut self, cb: &[u8], buf: &mut [u8], disk: &mut impl Disk) -> Outcome {
        let Some(&op) = cb.first() else {
            return self.fail(Sense::INVALID_COMMAND);
        };
        self.host.stats.command_arrived(op);
        let mut cmd = [0u8; 16];
        let n = cb.len().min(16);
        cmd[..n].copy_from_slice(&cb[..n]);
        let cb = &cmd;

        let generation = disk.media_generation();
        // A medium already present when the host connects is not a change from the host's view, so start
        // the connection without UNIT ATTENTION. Windows stops after a command fails in the first exchange
        // (the REQUEST SENSE that follows) and resets the bus after ~20 seconds, so a media change reported
        // here (e.g. for the image auto-inserted at boot) would cost one reset
        if self.host.new_connection.swap(false, Ordering::AcqRel) {
            self.seen_generation = generation;
            self.unit_attention = false;
        }

        // If the medium was swapped, report it as a media change
        if generation != self.seen_generation {
            self.seen_generation = generation;
            self.unit_attention = true;
            self.host.prevent_removal.store(false, Ordering::Release);
        }

        // Report the media change first for everything except INQUIRY and REQUEST SENSE
        if self.unit_attention && op != op::INQUIRY && op != op::REQUEST_SENSE {
            self.unit_attention = false;
            UsbStats::inc(&self.host.stats.commands);
            self.record_failure(op, Sense::MEDIUM_CHANGED);
            return self.fail(Sense::MEDIUM_CHANGED);
        }

        let outcome = match op {
            op::TEST_UNIT_READY => self.require_medium(disk).map(|_| DataPhase::None),
            op::REQUEST_SENSE => Ok(self.request_sense(cb[4], buf)),
            op::INQUIRY => Ok(self.inquiry(cb[4], buf)),
            op::READ_CAPACITY_10 => self.read_capacity(buf, disk),
            op::READ_FORMAT_CAPACITIES => Ok(self.read_format_capacities(be16(&cb[7..]), buf, disk)),
            op::MODE_SENSE_6 => self.mode_sense(false, cb[2] & 0x3F, usize::from(cb[4]), buf, disk),
            op::MODE_SENSE_10 => self.mode_sense(true, cb[2] & 0x3F, be16(&cb[7..]), buf, disk),
            op::MODE_SELECT_6 => Ok(DataPhase::Out(usize::from(cb[4]))),
            op::MODE_SELECT_10 => Ok(DataPhase::Out(be16(&cb[7..]))),
            op::PREVENT_ALLOW_MEDIUM_REMOVAL => {
                let prevent = cb[4] & 0x01 != 0;
                self.host.prevent_removal.store(prevent, Ordering::Release);
                log::debug!("PREVENT MEDIUM REMOVAL: {}", prevent);
                Ok(DataPhase::None)
            }
            op::START_STOP_UNIT => {
                let start = cb[4] & 0x01 != 0;
                let load_eject = cb[4] & 0x02 != 0;
                if load_eject && !start {
                    self.host.eject_requested.store(true, Ordering::Release);
                    log::info!("Eject requested by host");
                }
                Ok(DataPhase::None)
            }
            op::READ_10 => self.rw(false, be32(&cb[2..]), be16(&cb[7..]) as u32, disk),
            op::READ_12 => self.rw(false, be32(&cb[2..]), be32(&cb[6..]), disk),
            op::WRITE_10 => self.rw(true, be32(&cb[2..]), be16(&cb[7..]) as u32, disk),
            op::WRITE_12 => self.rw(true, be32(&cb[2..]), be32(&cb[6..]), disk),
            op::READ_6 if self.set == CommandSet::Scsi => {
                self.rw(false, lba6(cb), blocks6(cb), disk)
            }
            op::WRITE_6 if self.set == CommandSet::Scsi => {
                self.rw(true, lba6(cb), blocks6(cb), disk)
            }
            op::VERIFY_10 | op::SEEK_10 | op::REZERO => {
                self.require_medium(disk).map(|_| DataPhase::None)
            }
            op::SEND_DIAGNOSTIC | op::SYNCHRONIZE_CACHE => Ok(DataPhase::None),
            op::FORMAT_UNIT if self.set == CommandSet::Ufi => self.format_unit(cb, disk),
            _ => {
                log::debug!("Unsupported command {:#04x}", op);
                self.host.stats.last_unknown.store(u32::from(op) + 1, Ordering::Relaxed);
                Err(Sense::INVALID_COMMAND)
            }
        };

        UsbStats::inc(&self.host.stats.commands);
        match outcome {
            Ok(phase) => {
                if op != op::REQUEST_SENSE {
                    self.sense = Sense::NONE;
                }
                self.host.stats.record(op, None);
                Outcome::good(phase)
            }
            Err(sense) => {
                self.record_failure(op, sense);
                self.fail(sense)
            }
        }
    }

    /// Process data received with [`DataPhase::Out`].
    /// `scratch` is a work buffer at least one sector long.
    #[inline(never)]
    pub fn data_out(&mut self, cb: &[u8], data: &[u8], scratch: &mut [u8], disk: &mut impl Disk) -> bool {
        match cb.first().copied() {
            Some(op::FORMAT_UNIT) => match self.format_tracks(cb, data, scratch, disk) {
                Ok(()) => true,
                Err(sense) => {
                    self.sense = sense;
                    false
                }
            },
            // Accept and ignore MODE SELECT parameters
            _ => true,
        }
    }

    fn record_failure(&self, op: u8, sense: Sense) {
        let stats = &self.host.stats;
        UsbStats::inc(&stats.failed);
        stats.last_failure.store(failure_entry(op, sense), Ordering::Relaxed);
        stats.record(op, Some(sense));
    }

    fn fail(&mut self, sense: Sense) -> Outcome {
        self.sense = sense;
        Outcome::failed()
    }

    fn require_medium(&self, disk: &impl Disk) -> Result<Capacity, Sense> {
        disk.capacity().ok_or(Sense::NOT_READY_NO_MEDIUM)
    }

    /// Floppy geometry of the medium (for FORMAT UNIT).
    fn require_geometry(&self, disk: &impl Disk) -> Result<Geometry, Sense> {
        disk.geometry().ok_or(Sense::NOT_READY_NO_MEDIUM)
    }

    fn request_sense(&mut self, alloc: u8, buf: &mut [u8]) -> DataPhase {
        let r = &mut buf[..18];
        r.fill(0);
        r[0] = 0x70; // Current error, fixed format
        r[2] = self.sense.key;
        r[7] = 10; // Additional length
        r[12] = self.sense.asc;
        r[13] = self.sense.ascq;
        self.sense = Sense::NONE;
        DataPhase::In(18.min(usize::from(alloc)))
    }

    fn inquiry(&self, alloc: u8, buf: &mut [u8]) -> DataPhase {
        let r = &mut buf[..36];
        r.fill(0);
        r[0] = 0x00; // Direct access device
        r[1] = 0x80; // Removable
        match self.set {
            CommandSet::Ufi => {
                r[2] = 0x00;
                r[3] = 0x01;
            }
            CommandSet::Scsi => {
                r[2] = 0x04; // SPC-2
                r[3] = 0x02;
            }
        }
        r[4] = 31; // Additional length
        r[8..16].copy_from_slice(&self.identity.vendor);
        r[16..32].copy_from_slice(&self.identity.product);
        r[32..36].copy_from_slice(&self.identity.revision);
        DataPhase::In(36.min(usize::from(alloc)))
    }

    fn read_capacity(&self, buf: &mut [u8], disk: &impl Disk) -> Result<DataPhase, Sense> {
        let c = self.require_medium(disk)?;
        buf[0..4].copy_from_slice(&c.blocks.saturating_sub(1).to_be_bytes());
        buf[4..8].copy_from_slice(&u32::from(c.block_size).to_be_bytes());
        Ok(DataPhase::In(8))
    }

    fn read_format_capacities(&self, alloc: usize, buf: &mut [u8], disk: &impl Disk) -> DataPhase {
        let mut len = 4;
        let mut put = |buf: &mut [u8], blocks: u32, code: u8, block_len: u16| {
            buf[len..len + 4].copy_from_slice(&blocks.to_be_bytes());
            buf[len + 4] = code;
            buf[len + 5] = 0;
            buf[len + 6..len + 8].copy_from_slice(&block_len.to_be_bytes());
            len += 8;
        };
        buf[..4].fill(0);
        match disk.capacity() {
            Some(c) => {
                // Current capacity (formatted)
                put(buf, c.blocks, 0b10, c.block_size);
                if self.set == CommandSet::Ufi {
                    // Formattable capacities (currently only one, the same as the current medium)
                    put(buf, c.blocks, 0, c.block_size);
                }
            }
            None => put(buf, DRIVE_MAX.total_sectors(), 0b11, DRIVE_MAX.bytes_per_sector),
        }
        buf[3] = (len - 4) as u8;
        DataPhase::In(len.min(alloc))
    }

    fn mode_sense(
        &self,
        ten: bool,
        page: u8,
        alloc: usize,
        buf: &mut [u8],
        disk: &impl Disk,
    ) -> Result<DataPhase, Sense> {
        let geometry = disk.geometry();
        if self.set == CommandSet::Ufi && geometry.is_none() {
            return Err(Sense::NOT_READY_NO_MEDIUM);
        }
        let wp = if disk.write_protected() { 0x80 } else { 0x00 };
        let header = if ten { 8 } else { 4 };
        buf[..header].fill(0);
        let mut len = header;

        let want_flexible = page == 0x05 || page == 0x3F;
        if self.set == CommandSet::Ufi && want_flexible
            && let Some(g) = geometry
        {
            let p = &mut buf[len..len + 32];
            p.fill(0);
            p[0] = 0x05;
            p[1] = 0x1E;
            p[2..4].copy_from_slice(&transfer_rate_kbps(&g).to_be_bytes());
            p[4] = g.heads;
            p[5] = g.sectors_per_track;
            p[6..8].copy_from_slice(&g.bytes_per_sector.to_be_bytes());
            p[8..10].copy_from_slice(&g.cylinders.to_be_bytes());
            p[28..30].copy_from_slice(&300u16.to_be_bytes()); // Rotation speed [rpm]
            len += 32;
        } else if page != 0x3F && page != 0x05 && page != 0x08 && page != 0x1C {
            return Err(Sense::INVALID_FIELD);
        }

        if ten {
            buf[0..2].copy_from_slice(&((len - 2) as u16).to_be_bytes());
            buf[2] = medium_type(self.set, geometry);
            buf[3] = wp;
        } else {
            buf[0] = (len - 1) as u8;
            buf[1] = medium_type(self.set, geometry);
            buf[2] = wp;
        }
        Ok(DataPhase::In(len.min(alloc)))
    }

    fn rw(&self, write: bool, lba: u32, blocks: u32, disk: &impl Disk) -> Result<DataPhase, Sense> {
        let c = self.require_medium(disk)?;
        if u64::from(lba) + u64::from(blocks) > u64::from(c.blocks) {
            return Err(Sense::LBA_OUT_OF_RANGE);
        }
        if write {
            if disk.write_protected() {
                return Err(Sense::WRITE_PROTECTED);
            }
            Ok(DataPhase::Write { lba, blocks })
        } else {
            Ok(DataPhase::Read { lba, blocks })
        }
    }

    fn format_unit(&self, cb: &[u8; 16], disk: &impl Disk) -> Result<DataPhase, Sense> {
        self.require_medium(disk)?;
        if disk.write_protected() {
            return Err(Sense::WRITE_PROTECTED);
        }
        let param_len = be16(&cb[7..]);
        if cb[1] & 0x10 != 0 && param_len > 0 {
            // Process after receiving the parameter list (format descriptor)
            Ok(DataPhase::Out(param_len))
        } else {
            Ok(DataPhase::None)
        }
    }

    /// Fill tracks with [`FORMAT_FILL`] according to the FORMAT UNIT parameters.
    fn format_tracks(
        &self,
        cb: &[u8],
        data: &[u8],
        scratch: &mut [u8],
        disk: &mut impl Disk,
    ) -> Result<(), Sense> {
        let g = self.require_geometry(disk)?;
        if data.len() < 12 || cb.len() < 3 {
            return Err(Sense::INVALID_FIELD);
        }
        let single_track = data[1] & 0x10 != 0;
        let side = data[1] & 0x01;
        let spt = u32::from(g.sectors_per_track);
        let (first, count) = if single_track {
            let track = u32::from(cb[2]);
            if track >= u32::from(g.cylinders) || side >= g.heads {
                return Err(Sense::INVALID_FIELD);
            }
            ((track * u32::from(g.heads) + u32::from(side)) * spt, spt)
        } else {
            (0, g.total_sectors())
        };

        let sector = scratch
            .get_mut(..usize::from(g.bytes_per_sector))
            .ok_or(Sense::INVALID_FIELD)?;
        sector.fill(FORMAT_FILL);
        for lba in first..first + count {
            disk.write(lba, sector).map_err(|_| Sense::MEDIUM_ERROR_WRITE)?;
        }
        Ok(())
    }
}

/// Medium type returned by UFI MODE SENSE.
fn medium_type(set: CommandSet, geometry: Option<Geometry>) -> u8 {
    if set != CommandSet::Ufi {
        return 0x00;
    }
    match geometry.map(|g| g.total_bytes()) {
        Some(737_280) => 0x1E,   // 720KB
        Some(1_261_568) => 0x93, // 1.25MB
        Some(1_474_560) => 0x94, // 1.44MB
        _ => 0x00,
    }
}

/// Transfer rate of the Flexible Disk Page [kbit/s].
fn transfer_rate_kbps(g: &Geometry) -> u16 {
    match g.total_bytes() {
        b if b <= 737_280 => 250,
        b if b <= 1_474_560 => 500,
        _ => 1000,
    }
}

/// Packed form of [`UsbStats::last_failure`]: opcode << 24 | key << 16 | ASC << 8 | ASCQ.
fn failure_entry(op: u8, sense: Sense) -> u32 {
    (u32::from(op) << 24) | (u32::from(sense.key) << 16) | (u32::from(sense.asc) << 8) | u32::from(sense.ascq)
}

/// Direction and amount of data the host transfers after the command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Expected {
    /// true means device -> host.
    pub dir_in: bool,
    pub len: usize,
}

/// Determine the data transfer the host expects from the command block.
///
/// CBI has no transfer length field like the BOT CBW, so it is computed from the command contents.
/// This is needed because even if the command fails, data sent by the host must be received and discarded.
pub fn expected_transfer(cb: &[u8], block_size: usize) -> Expected {
    let mut c = [0u8; 16];
    let n = cb.len().min(16);
    c[..n].copy_from_slice(&cb[..n]);
    let input = |len: usize| Expected { dir_in: true, len };
    let output = |len: usize| Expected { dir_in: false, len };
    match c[0] {
        op::REQUEST_SENSE | op::INQUIRY | op::MODE_SENSE_6 => input(usize::from(c[4])),
        op::MODE_SENSE_10 | op::READ_FORMAT_CAPACITIES => input(be16(&c[7..])),
        op::READ_CAPACITY_10 => input(8),
        op::READ_10 => input(be16(&c[7..]) * block_size),
        op::READ_12 => input(be32(&c[6..]) as usize * block_size),
        op::WRITE_10 => output(be16(&c[7..]) * block_size),
        op::WRITE_12 => output(be32(&c[6..]) as usize * block_size),
        op::MODE_SELECT_6 => output(usize::from(c[4])),
        op::MODE_SELECT_10 => output(be16(&c[7..])),
        op::FORMAT_UNIT if c[1] & 0x10 != 0 => output(be16(&c[7..])),
        _ => input(0),
    }
}

fn be16(b: &[u8]) -> usize {
    usize::from(u16::from_be_bytes([b[0], b[1]]))
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn lba6(cb: &[u8; 16]) -> u32 {
    u32::from_be_bytes([0, cb[1] & 0x1F, cb[2], cb[3]])
}

fn blocks6(cb: &[u8; 16]) -> u32 {
    if cb[4] == 0 { 256 } else { u32::from(cb[4]) }
}

/// Opcodes.
pub(super) mod op {
    pub const TEST_UNIT_READY: u8 = 0x00;
    pub const REZERO: u8 = 0x01;
    pub const REQUEST_SENSE: u8 = 0x03;
    pub const FORMAT_UNIT: u8 = 0x04;
    pub const READ_6: u8 = 0x08;
    pub const WRITE_6: u8 = 0x0A;
    pub const INQUIRY: u8 = 0x12;
    pub const MODE_SELECT_6: u8 = 0x15;
    pub const MODE_SENSE_6: u8 = 0x1A;
    pub const START_STOP_UNIT: u8 = 0x1B;
    pub const SEND_DIAGNOSTIC: u8 = 0x1D;
    pub const PREVENT_ALLOW_MEDIUM_REMOVAL: u8 = 0x1E;
    pub const READ_FORMAT_CAPACITIES: u8 = 0x23;
    pub const READ_CAPACITY_10: u8 = 0x25;
    pub const READ_10: u8 = 0x28;
    pub const WRITE_10: u8 = 0x2A;
    pub const SEEK_10: u8 = 0x2B;
    pub const VERIFY_10: u8 = 0x2F;
    pub const SYNCHRONIZE_CACHE: u8 = 0x35;
    pub const MODE_SELECT_10: u8 = 0x55;
    pub const MODE_SENSE_10: u8 = 0x5A;
    pub const READ_12: u8 = 0xA8;
    pub const WRITE_12: u8 = 0xAA;
}
