//! USB device (ESP32-S3 built-in USB OTG, Full Speed).
//!
//! Composition:
//! - Mass storage (interface class 0x08). The mode is [`UsbMode`] (UFI+CBI / UFI+BOT / SCSI+BOT)
//! - Development CDC-ACM (feature `dev-cdc`). Used for log output and accepting reboot commands
//!
//! Using USB as OTG makes USB-Serial-JTAG unavailable, so CDC-ACM replaces it during development.

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_usb::{Builder, UsbDevice};
use esp_hal::peripherals::{GPIO19, GPIO20, USB_FS};
use esp_hal::usb::otg::Usb;
use esp_hal::usb::otg::embassy_usb_device::{Config as OtgConfig, Driver};
use static_cell::StaticCell;

pub mod bot;
pub mod cbi;
#[cfg(feature = "dev-cdc")]
pub mod cdc;
pub mod command;
mod control_status;
pub(crate) mod sense_trace;
pub mod transfer;

use bot::{BotControl, BulkOnly};
use cbi::{AdscChannel, Cbi, CbiControl};
use command::{CommandSet, Processor};
use crate::storage::Disk;

/// Mass storage mode (plan section 3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsbMode {
    /// UFI + CBI. Same as real USB floppy drives. Recognized as a floppy on Windows
    UfiCbi,
    /// UFI + Bulk-Only. No standard driver binds on Windows
    UfiBot,
    /// SCSI + Bulk-Only. Appears as an ordinary removable disk
    ScsiBot,
}

impl UsbMode {
    pub const fn command_set(self) -> CommandSet {
        match self {
            UsbMode::UfiCbi | UsbMode::UfiBot => CommandSet::Ufi,
            UsbMode::ScsiBot => CommandSet::Scsi,
        }
    }

    /// Convert from a config value ("ufi-cbi" etc.).
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "ufi-cbi" => Some(UsbMode::UfiCbi),
            "ufi-bot" => Some(UsbMode::UfiBot),
            "scsi" => Some(UsbMode::ScsiBot),
            _ => None,
        }
    }
}

pub type UsbDriver = control_status::StatusDriver<Driver<'static>>;

/// Names and IDs placed in the USB descriptors.
pub struct UsbIdentity {
    pub vid: u16,
    pub pid: u16,
    pub manufacturer: &'static str,
    pub product: &'static str,
    pub serial: &'static str,
}

impl UsbIdentity {
    /// Defaults. The VID/PID are pid.codes test IDs (plan Q9).
    pub const DEFAULT: UsbIdentity = UsbIdentity {
        vid: 0x1209,
        pid: 0x0001,
        manufacturer: "card-fdd",
        product: "USB Floppy",
        serial: "CARDFDD0001",
    };
}

/// Drive access (triggers the access lamp and seek sounds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Activity {
    pub lba: u32,
    pub write: bool,
}

/// State of the exchange with the host (eject prevented, eject requested, transferring).
pub static HOST: command::HostState = command::HostState::new();

/// The latest access. Taken by the UI.
pub static ACTIVITY: Signal<CriticalSectionRawMutex, Activity> = Signal::new();

pub(crate) fn notify_activity(activity: Activity) {
    ACTIVITY.signal(activity);
}

/// The mass storage transport implementation.
pub enum MassStorage {
    Bot(BulkOnly<'static, UsbDriver>),
    Cbi(Cbi<'static, UsbDriver>),
}

impl MassStorage {
    pub async fn run(&mut self, processor: &mut Processor, disk: &mut impl Disk) -> ! {
        match self {
            MassStorage::Bot(t) => t.run(processor, disk).await,
            MassStorage::Cbi(t) => t.run(processor, disk).await,
        }
    }
}

pub struct UsbParts {
    pub device: UsbDevice<'static, UsbDriver>,
    pub msc: MassStorage,
    #[cfg(feature = "dev-cdc")]
    pub cdc: embassy_usb::class::cdc_acm::CdcAcmClass<'static, UsbDriver>,
}

/// Discard data queued in an IN endpoint that the host has not read.
///
/// With CBI, Windows does not read the REQUEST SENSE completion interrupt (Linux does). If an unread
/// completion interrupt remains, the next command's completion interrupt cannot be sent and transfers stall, so
/// discard the leftovers when a new command arrives. This matches the behavior of real USB floppy drives.
///
/// embassy-usb has no API for this, so the registers are manipulated directly using the same steps the driver
/// uses when disabling an endpoint (NAK + disable -> flush the TX FIFO). The endpoint itself
/// stays enabled.
pub(crate) fn discard_pending_in(ep_index: usize) -> bool {
    use embassy_usb_synopsys_otg::otg_v1::Otg;
    /// Upper bound on polls waiting for disable/flush completion (normally finishes within a few µs).
    const SPIN_LIMIT: u32 = 100_000;

    // SAFETY: points to the USB_FS register block. Only called when the driver's send path is not running
    // (no write to this endpoint is being awaited).
    let regs = unsafe { Otg::from_ptr(USB_FS::PTR.cast_mut().cast::<()>()) };
    critical_section::with(|_| {
        if !regs.diepctl(ep_index).read().epena() {
            return false;
        }
        regs.diepctl(ep_index).modify(|w| {
            w.set_snak(true);
            w.set_epdis(true);
        });
        for _ in 0..SPIN_LIMIT {
            if !regs.diepctl(ep_index).read().epena() {
                break;
            }
        }
        regs.grstctl().write(|w| {
            w.set_txfflsh(true);
            w.set_txfnum(ep_index as _);
        });
        for _ in 0..SPIN_LIMIT {
            if !regs.grstctl().read().txfflsh() {
                break;
            }
        }
        true
    })
}

/// Whether an IN endpoint has data queued that the host has not read (diagnostics).
pub fn in_pending(ep_index: usize) -> bool {
    use embassy_usb_synopsys_otg::otg_v1::Otg;
    // SAFETY: read only.
    let regs = unsafe { Otg::from_ptr(USB_FS::PTR.cast_mut().cast::<()>()) };
    ep_index != 0 && regs.diepctl(ep_index).read().epena()
}

/// Raw value of an IN endpoint's interrupt status register (DIEPINT) (diagnostics).
pub fn in_interrupt_bits(ep_index: usize) -> u32 {
    use embassy_usb_synopsys_otg::otg_v1::Otg;
    // SAFETY: read only.
    let regs = unsafe { Otg::from_ptr(USB_FS::PTR.cast_mut().cast::<()>()) };
    if ep_index == 0 {
        return 0;
    }
    regs.diepint(ep_index).read().0
}

/// Remove the USB-Serial-JTAG D+ pull-up so the host sees a "disconnect" once.
///
/// Just switching the PHY to OTG keeps D+ low only briefly, so the host assumes the JTAG device is
/// still connected and gets communication errors (the OTG side is not enumerated).
fn detach_usb_serial_jtag() {
    /// Long enough for the host's hub to detect the disconnect.
    const DETACH_MS: u32 = 200;

    esp_hal::peripherals::USB_DEVICE::regs().conf0().modify(|_, w| {
        w.pad_pull_override().set_bit();
        w.dp_pullup().clear_bit();
        w.dm_pullup().clear_bit();
        w.dp_pulldown().set_bit();
        w.dm_pulldown().set_bit()
    });
    esp_hal::delay::Delay::new().delay_millis(DETACH_MS);
}

/// After switching the PHY to OTG, remove all pull-ups/pull-downs on the USB-Serial-JTAG side.
///
/// If the pull-down added for detaching remains on the shared pad, it can disturb OTG signal levels
/// and cause problems such as false suspend detection.
fn release_usb_serial_jtag_pulls() {
    esp_hal::peripherals::USB_DEVICE::regs().conf0().modify(|_, w| {
        w.pad_pull_override().set_bit();
        w.dp_pullup().clear_bit();
        w.dm_pullup().clear_bit();
        w.dp_pulldown().clear_bit();
        w.dm_pulldown().clear_bit()
    });
}

/// Build the USB device. Call only once.
pub fn build(usb: USB_FS<'static>, dp: GPIO20<'static>, dm: GPIO19<'static>, mode: UsbMode, id: &UsbIdentity) -> UsbParts {
    static EP_OUT_BUFFER: StaticCell<[u8; 1024]> = StaticCell::new();
    static CONFIG_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static BOS_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static CONTROL_BUF: StaticCell<[u8; 64]> = StaticCell::new();
    static BOT_CONTROL: StaticCell<BotControl> = StaticCell::new();
    static BOT_RESET: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
    static CBI_CONTROL: StaticCell<CbiControl> = StaticCell::new();
    static ADSC: AdscChannel = AdscChannel::new();

    detach_usb_serial_jtag();
    let usb = Usb::new_fs(usb, dp, dm);
    release_usb_serial_jtag_pulls();
    let driver = control_status::StatusDriver(Driver::new(usb, EP_OUT_BUFFER.init([0; 1024]), OtgConfig::default()));

    let mut config = embassy_usb::Config::new(id.vid, id.pid);
    config.manufacturer = Some(id.manufacturer);
    config.product = Some(id.product);
    config.serial_number = Some(id.serial);
    config.max_power = 100;
    config.max_packet_size_0 = 64;
    #[cfg(feature = "dev-cdc")]
    {
        // Composite devices combined with CDC-ACM use an IAD
        config.composite_with_iads = true;
        config.device_class = 0xEF;
        config.device_sub_class = 0x02;
        config.device_protocol = 0x01;
    }

    let mut builder = Builder::new(
        driver,
        config,
        CONFIG_DESC.init([0; 256]),
        BOS_DESC.init([0; 256]),
        &mut [],
        CONTROL_BUF.init([0; 64]),
    );

    // Mass storage
    let subclass = match mode.command_set() {
        CommandSet::Ufi => 0x04,
        CommandSet::Scsi => 0x06,
    };
    let protocol = match mode {
        UsbMode::UfiCbi => 0x00, // CBI (with command completion interrupt)
        UsbMode::UfiBot | UsbMode::ScsiBot => 0x50,
    };
    let mut function = builder.function(0x08, subclass, protocol);
    let mut interface = function.interface();
    let iface = interface.interface_number();
    let mut alt = interface.alt_setting(0x08, subclass, protocol, None);
    let ep_out = alt.endpoint_bulk_out(None, transfer::MAX_PACKET as u16);
    let ep_in = alt.endpoint_bulk_in(None, transfer::MAX_PACKET as u16);
    let msc = match mode {
        UsbMode::UfiCbi => {
            // The completion interrupt is 2 bytes. Poll every 1ms to reduce latency
            let ep_int = alt.endpoint_interrupt_in(None, 2, 1);
            drop(function);
            builder.handler(CBI_CONTROL.init(CbiControl::new(iface, &ADSC)));
            MassStorage::Cbi(Cbi::new(ep_in, ep_out, ep_int, &ADSC, discard_pending_in))
        }
        UsbMode::UfiBot | UsbMode::ScsiBot => {
            drop(function);
            builder.handler(BOT_CONTROL.init(BotControl::new(iface, &BOT_RESET)));
            MassStorage::Bot(BulkOnly::new(ep_in, ep_out, &BOT_RESET))
        }
    };

    #[cfg(feature = "dev-cdc")]
    let cdc = {
        use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
        static CDC_STATE: StaticCell<State> = StaticCell::new();
        CdcAcmClass::new(&mut builder, CDC_STATE.init(State::new()), 64)
    };

    UsbParts {
        device: builder.build(),
        msc,
        #[cfg(feature = "dev-cdc")]
        cdc,
    }
}
