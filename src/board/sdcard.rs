//! MicroSD (SPI3).

use embedded_hal::spi::SpiBus;
use embedded_hal_bus::spi::ExclusiveDevice;
use embedded_sdmmc::SdCard;
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
