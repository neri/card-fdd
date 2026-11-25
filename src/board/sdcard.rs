//! MicroSD (SPI3).

use embedded_hal::spi::{SpiBus, SpiDevice};
use embedded_hal_bus::spi::ExclusiveDevice;
use embedded_sdmmc::SdCard;
use embedded_sdmmc::embedded_sdmmc_types::sdcard::{crc7, crc16};
use esp_hal::Blocking;
use esp_hal::delay::Delay;
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::peripherals::{GPIO12, GPIO14, GPIO39, GPIO40, SPI3};
use esp_hal::spi::Mode;
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::time::Rate;

/// SPI clock during initialization (400kHz or less per the SD spec).
const INIT_FREQ: Rate = Rate::from_khz(400);
/// SPI clock after initialization. SD in SPI mode supports up to 25MHz.
const RUN_FREQ: Rate = Rate::from_mhz(20);

type SpiDev = ExclusiveDevice<Spi<'static, Blocking>, Output<'static>, Delay>;
pub type Card = SdCard<SpiDev, Delay>;

pub struct SdPins {
    pub spi: SPI3<'static>,
    pub sck: GPIO40<'static>,
    pub miso: GPIO39<'static>,
    pub mosi: GPIO14<'static>,
    pub cs: GPIO12<'static>,
}

/// Set up the SPI bus and send dummy clocks to put the card into SPI mode.
///
/// The actual exchange with the card (CMD0 etc.) is done by embedded-sdmmc on first access.
/// Afterwards, raise the clock with [`speed_up`].
pub fn init(pins: SdPins) -> Card {
    let config = SpiConfig::default()
        .with_frequency(INIT_FREQ)
        .with_mode(Mode::_0);
    let mut spi = Spi::new(pins.spi, config)
        .expect("Failed to configure SPI3")
        .with_sck(pins.sck)
        .with_miso(pins.miso)
        .with_mosi(pins.mosi);

    // Send at least 74 clocks (10 bytes = 80 clocks) with CS held high.
    let cs = Output::new(pins.cs, Level::High, OutputConfig::default());
    let _ = SpiBus::write(&mut spi, &[0xFF; 10]);

    let dev = ExclusiveDevice::new(spi, cs, Delay::new()).expect("Failed to configure CS pin");
    SdCard::new(dev, Delay::new())
}

/// Raise the SPI clock after card initialization is done.
pub fn speed_up(card: &Card) {
    card.spi(|dev| {
        let config = SpiConfig::default().with_frequency(RUN_FREQ).with_mode(Mode::_0);
        if dev.bus_mut().apply_config(&config).is_err() {
            log::warn!("Could not raise the SD SPI clock");
        }
    });
}

/// Card identification register (CID). It holds the card's serial number, so it tells cards apart.
pub type Cid = [u8; 16];

/// Bytes to clock after a command while waiting for R1 (N_CR is at most 8 bytes).
const R1_WINDOW: usize = 8;
/// Bytes to clock while waiting for the CID data token. N_CX is at most 8 bytes; leave headroom.
const TOKEN_WINDOW: usize = 32;
const DATA_START_BLOCK: u8 = 0xFE;

/// Whether a card answers CMD0 (GO_IDLE_STATE). Used to notice insertion; there is no card-detect pin.
///
/// Runs at the init clock and leaves the card in the idle state, so call this only while no card is
/// mounted. With no card, MISO stays high and this returns false after about 0.5ms (unlike the
/// library's init, which retries for seconds).
pub fn probe(card: &Card) -> bool {
    card.spi(|dev| {
        let config = SpiConfig::default().with_frequency(INIT_FREQ).with_mode(Mode::_0);
        let _ = dev.bus_mut().apply_config(&config);
        // A freshly inserted card needs at least 74 clocks with CS high before CMD0
        let _ = SpiBus::write(dev.bus_mut(), &[0xFF; 10]);
        let mut buf = [0xFF; 6 + R1_WINDOW];
        buf[..6].copy_from_slice(&command(0, 0));
        let ok = SpiDevice::transfer_in_place(dev, &mut buf).is_ok();
        let _ = SpiBus::write(dev.bus_mut(), &[0xFF]);
        // R1 = idle state only (a stuck-low MISO reads 0x00 and does not count)
        ok && r1(&buf[6..]) == Some(0x01)
    })
}

/// Read the CID of the initialized card with CMD10. `None` when the card does not answer or the
/// data fails its CRC (the card was removed, or replaced by one that has not been initialized).
///
/// Takes well under 0.1ms at the run clock and leaves the card's state unchanged.
pub fn read_cid(card: &Card) -> Option<Cid> {
    card.spi(|dev| {
        // 1 idle byte, the command, R1, then the data token, 16 bytes of CID, and the CRC16
        let mut buf = [0xFF; 1 + 6 + R1_WINDOW + TOKEN_WINDOW + 16 + 2];
        buf[1..7].copy_from_slice(&command(10, 0));
        let ok = SpiDevice::transfer_in_place(dev, &mut buf).is_ok();
        let _ = SpiBus::write(dev.bus_mut(), &[0xFF]);
        if !ok {
            return None;
        }
        let rest = &buf[7..];
        let r1_at = rest[..R1_WINDOW].iter().position(|&b| b & 0x80 == 0)?;
        if rest[r1_at] != 0x00 {
            return None;
        }
        let after = &rest[r1_at + 1..];
        let token_at = after.iter().take(TOKEN_WINDOW).position(|&b| b != 0xFF)?;
        let data = after.get(token_at + 1..token_at + 1 + 16 + 2)?;
        if after[token_at] != DATA_START_BLOCK {
            return None;
        }
        let (cid, crc) = data.split_at(16);
        if crc16(cid) != u16::from_be_bytes([crc[0], crc[1]]) {
            return None;
        }
        cid.try_into().ok()
    })
}

/// A 6-byte SPI-mode command frame. The CRC is always valid because the driver turns CRC checking on.
fn command(index: u8, arg: u32) -> [u8; 6] {
    let a = arg.to_be_bytes();
    let mut frame = [0x40 | index, a[0], a[1], a[2], a[3], 0];
    frame[5] = (crc7(&frame[..5]) << 1) | 1;
    frame
}

/// The first R1 response (a byte with the top bit clear) in `window`.
fn r1(window: &[u8]) -> Option<u8> {
    window.iter().copied().find(|&b| b & 0x80 == 0)
}

/// Mark the card as uninitialized so it is re-initialized on next access (for retrying after reinsertion).
pub fn reset(card: &Card) {
    card.spi(|dev| {
        let config = SpiConfig::default().with_frequency(INIT_FREQ).with_mode(Mode::_0);
        let _ = dev.bus_mut().apply_config(&config);
        // Outside a transaction, so CS stays high.
        let _ = SpiBus::write(dev.bus_mut(), &[0xFF; 10]);
    });
    card.mark_card_uninit();
}
