//! Read-only sampling of the CBI REQUEST SENSE IN endpoints.

use core::sync::atomic::{AtomicU32, Ordering::Relaxed};

use embassy_usb_synopsys_otg::otg_v1::Otg;
use esp_hal::peripherals::USB_FS;

use super::command::Sense;

const HISTORY_LEN: usize = 3;
// Bits 0..23 hold key/ASC/ASCQ, then three bits per endpoint. A zero endpoint state means not queued.
const VALID: u32 = 1 << 31;
static HISTORY: [AtomicU32; HISTORY_LEN] = [const { AtomicU32::new(0) }; HISTORY_LEN];
static NEXT: AtomicU32 = AtomicU32::new(0);
/// Slot index plus one, or zero when there is no active REQUEST SENSE.
static CURRENT: AtomicU32 = AtomicU32::new(0);

pub(crate) static BULK: Transfer = Transfer::new(24);
pub(crate) static INTERRUPT: Transfer = Transfer::new(27);

#[derive(Clone, Copy)]
#[repr(u32)]
pub(crate) enum State {
    Pending = 1,
    Drained = 2,
    Superseded = 3,
    Reset = 4,
}

impl State {
    fn marker(bits: u32) -> char {
        match bits & 7 {
            1 => 'p',
            2 => '+',
            3 => 'x',
            4 => 'r',
            _ => '-',
        }
    }
}

pub(crate) fn begin(sense: Sense) {
    let slot = NEXT.load(Relaxed) as usize;
    let code = u32::from_be_bytes([0, sense.key, sense.asc, sense.ascq]);
    HISTORY[slot].store(VALID | code, Relaxed);
    CURRENT.store(slot as u32 + 1, Relaxed);
    NEXT.store(((slot + 1) % HISTORY_LEN) as u32, Relaxed);
}

/// Code and endpoint markers for the last three REQUEST SENSE commands, oldest first.
pub(crate) fn recent() -> impl Iterator<Item = (u32, char, char)> {
    let next = NEXT.load(Relaxed) as usize;
    (0..HISTORY_LEN).filter_map(move |offset| {
        let packed = HISTORY[(next + offset) % HISTORY_LEN].load(Relaxed);
        (packed & VALID != 0).then(|| {
            (
                packed & 0xff_ffff,
                State::marker(packed >> BULK.shift),
                State::marker(packed >> INTERRUPT.shift),
            )
        })
    })
}

pub(crate) struct Transfer {
    shift: u32,
    /// Endpoint with a queued REQUEST SENSE transfer (zero when idle).
    endpoint: AtomicU32,
    queued: AtomicU32,
    drained: AtomicU32,
}

impl Transfer {
    const fn new(shift: u32) -> Self {
        Self {
            shift,
            endpoint: AtomicU32::new(0),
            queued: AtomicU32::new(0),
            drained: AtomicU32::new(0),
        }
    }

    pub(crate) fn queued(&self, endpoint: usize) {
        let Some(record) = current_record() else {
            return;
        };
        self.set_state(record, State::Pending);
        self.queued.fetch_add(1, Relaxed);
        self.endpoint.store(endpoint as u32, Relaxed);
    }

    pub(crate) fn counts(&self) -> (u32, u32) {
        (self.drained.load(Relaxed), self.queued.load(Relaxed))
    }

    fn set_state(&self, record: &AtomicU32, state: State) {
        let packed = record.load(Relaxed);
        record.store(
            (packed & !(7 << self.shift)) | ((state as u32) << self.shift),
            Relaxed,
        );
    }

    fn sample(&self, regs: Otg, endpoint: usize, address: u8) {
        if endpoint == 0 {
            return;
        }
        let control = regs.diepctl(endpoint).read();
        let remaining = regs.dieptsiz(endpoint).read();
        if control.usbaep()
            && !control.epena()
            && remaining.pktcnt() == 0
            && remaining.xfrsiz() == 0
            && regs.dcfg().read().dad() == address
            && !regs.gintsts().read().usbrst()
        {
            if let Some(record) = current_record() {
                self.set_state(record, State::Drained);
            }
            self.endpoint.store(0, Relaxed);
            self.drained.fetch_add(1, Relaxed);
        }
    }
}

fn current_record() -> Option<&'static AtomicU32> {
    HISTORY.get(CURRENT.load(Relaxed).wrapping_sub(1) as usize)
}

/// Close outstanding observations before a flush or deconfiguration, retaining their states.
pub(crate) fn end(state: State) {
    if let Some(record) = current_record() {
        for transfer in [&BULK, &INTERRUPT] {
            if transfer.endpoint.swap(0, Relaxed) != 0 {
                transfer.set_state(record, state);
            }
        }
    }
    CURRENT.store(0, Relaxed);
}

/// Called from the application loop and just before the CBI task discards old IN data.
pub(crate) fn sample() {
    let bulk = BULK.endpoint.load(Relaxed) as usize;
    let interrupt = INTERRUPT.endpoint.load(Relaxed) as usize;
    if bulk == 0 && interrupt == 0 {
        return;
    }

    // SAFETY: this is the USB_FS register block. No registers or flags are modified.
    let regs = unsafe { Otg::from_ptr(USB_FS::PTR.cast_mut().cast::<()>()) };
    let address = regs.dcfg().read().dad();
    if address != 0 && !regs.gintsts().read().usbrst() {
        BULK.sample(regs, bulk, address);
        INTERRUPT.sample(regs, interrupt, address);
    }
}
