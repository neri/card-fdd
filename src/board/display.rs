//! ST7789V2 1.14" 240x135 LCD.

use embedded_hal_bus::spi::ExclusiveDevice;
use esp_hal::Blocking;
use esp_hal::delay::Delay;
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::peripherals::{GPIO33, GPIO34, GPIO35, GPIO36, GPIO37, GPIO38, SPI2};
use esp_hal::spi::Mode;
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::time::Rate;
use mipidsi::interface::SpiInterface;
use mipidsi::models::ST7789;
use mipidsi::options::{ColorInversion, Orientation, Rotation};
use static_cell::StaticCell;

/// Screen width (landscape).
pub const WIDTH: u32 = 240;
/// Screen height (landscape).
pub const HEIGHT: u32 = 135;

/// SPI clock. The ST7789 supports writes up to 62.5MHz, but we use 40MHz for margin.
const SPI_FREQ_MHZ: u32 = 40;

type SpiDev = ExclusiveDevice<Spi<'static, Blocking>, Output<'static>, Delay>;
pub type Display =
    mipidsi::Display<SpiInterface<'static, SpiDev, Output<'static>>, ST7789, Output<'static>>;

/// Peripherals used by the LCD.
pub struct DisplayPins {
    pub spi: SPI2<'static>,
    pub mosi: GPIO35<'static>,
    pub sck: GPIO36<'static>,
    pub cs: GPIO37<'static>,
    pub dc: GPIO34<'static>,
    pub rst: GPIO33<'static>,
    pub backlight: GPIO38<'static>,
}

/// Initialize the LCD and turn on the backlight.
///
/// The second return value is the backlight pin (used to turn it off automatically).
pub fn init(pins: DisplayPins) -> (Display, Output<'static>) {
    static BUFFER: StaticCell<[u8; 512]> = StaticCell::new();

    let spi = Spi::new(
        pins.spi,
        SpiConfig::default()
            .with_frequency(Rate::from_mhz(SPI_FREQ_MHZ))
            .with_mode(Mode::_0),
    )
    .expect("Failed to configure SPI2")
    .with_sck(pins.sck)
    .with_mosi(pins.mosi);

    let cs = Output::new(pins.cs, Level::High, OutputConfig::default());
    let dc = Output::new(pins.dc, Level::Low, OutputConfig::default());
    let rst = Output::new(pins.rst, Level::High, OutputConfig::default());

    let spi_dev = ExclusiveDevice::new(spi, cs, Delay::new()).expect("Failed to configure CS pin");
    let di = SpiInterface::new(spi_dev, dc, BUFFER.init([0; 512]));

    // The panel is 240 tall x 135 wide, located at (52, 40) within the controller's
    // framebuffer (240x320). Rotate it 90 degrees to use it in landscape.
    let display = mipidsi::Builder::new(ST7789, di)
        .display_size(HEIGHT as u16, WIDTH as u16)
        .display_offset(52, 40)
        .invert_colors(ColorInversion::Inverted)
        .orientation(Orientation::new().rotate(Rotation::Deg90))
        .reset_pin(rst)
        .init(&mut Delay::new())
        .expect("Failed to initialize LCD");

    let backlight = Output::new(pins.backlight, Level::High, OutputConfig::default());

    (display, backlight)
}
