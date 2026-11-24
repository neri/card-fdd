#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use core::cell::RefCell;

use card_fdd::app::{App, SdImage, SharedSd};
use card_fdd::board::display::{self, DisplayPins};
use card_fdd::board::led::StatusLed;
use card_fdd::board::sdcard::{self, SdPins};
use card_fdd::board::audio::{self, AudioPins};
use card_fdd::board::v1::MatrixKeyboard;
use card_fdd::board::v1::keyboard::KeyboardPins;
use card_fdd::board::{self, AdcBattery, AnyKeyboard, Battery, Keyboard, Model, adv};
use card_fdd::config::{Config, Defaults};
use card_fdd::input::KeyEvent;
use card_fdd::logger;
use card_fdd::sound::{self, synth::Synth};
use card_fdd::storage::SharedDisk;
use card_fdd::storage::browser::Browser;
use card_fdd::storage::image::ImageDisk;
use card_fdd::usb::command::{Identity, Processor};
use card_fdd::usb::{self, MassStorage, UsbDriver, UsbIdentity, UsbMode};
use embassy_executor::Spawner;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant, with_timeout};
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::timer::timg::TimerGroup;
use heapless::String;
use log::info;
use static_cell::StaticCell;

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

static KEY_EVENTS: Channel<CriticalSectionRawMutex, KeyEvent, 16> = Channel::new();

/// Defaults for items missing from the config file. The USB mode and VID/PID can also be
/// changed with build-time environment variables (CARDFDD_USB_MODE / CARDFDD_USB_VID / CARDFDD_USB_PID).
#[allow(clippy::large_stack_frames, reason = "Initializes the full FAT path buffer before USB tasks start")]
fn config_defaults() -> Config {
    fn hex(value: Option<&str>, default: u16) -> u16 {
        value
            .and_then(|v| u16::from_str_radix(v.trim_start_matches("0x").trim_start_matches("0X"), 16).ok())
            .unwrap_or(default)
    }
    let id = UsbIdentity::DEFAULT;
    Config::new(&Defaults {
        usb_mode: option_env!("CARDFDD_USB_MODE").unwrap_or("ufi-cbi"),
        usb_vid: hex(option_env!("CARDFDD_USB_VID"), id.vid),
        usb_pid: hex(option_env!("CARDFDD_USB_PID"), id.pid),
        usb_manufacturer: id.manufacturer,
        usb_product: id.product,
    })
}

const IDENTITY: Identity = Identity {
    vendor: *b"CARDFDD ",
    product: *b"USB Floppy      ",
    revision: *b"0.1 ",
};

#[embassy_executor::task]
async fn keyboard_task(mut keyboard: AnyKeyboard) {
    loop {
        let event = keyboard.next_event().await;
        KEY_EVENTS.send(event).await;
    }
}

#[allow(
    clippy::large_stack_frames,
    reason = "About 1KB for the sound synthesis buffer etc. The main stack has plenty of headroom"
)]
#[embassy_executor::task]
async fn sound_task(pins: AudioPins) {
    let mut synth = Synth::new();
    audio::run(pins, |out| {
        while let Some(event) = sound::take_event() {
            synth.event(event);
        }
        synth.fill(out, sound::volume());
    })
    .await
}

#[embassy_executor::task]
async fn usb_device_task(device: &'static mut embassy_usb::UsbDevice<'static, UsbDriver>) {
    device.run().await;
}

#[allow(
    clippy::large_stack_frames,
    reason = "About 1-1.5KB for USB transfer packet buffers etc. The main stack has plenty of headroom"
)]
#[embassy_executor::task]
async fn msc_task(
    msc: &'static mut MassStorage,
    processor: &'static mut Processor,
    image: &'static RefCell<SdImage>,
) {
    let mut disk = SharedDisk(image);
    msc.run(processor, &mut disk).await
}

#[cfg(feature = "dev-cdc")]
#[allow(
    clippy::large_stack_frames,
    reason = "About 1-1.5KB for USB transfer packet buffers etc. The main stack has plenty of headroom"
)]
#[embassy_executor::task]
async fn cdc_task(class: embassy_usb::class::cdc_acm::CdcAcmClass<'static, UsbDriver>) {
    usb::cdc::run(class).await
}

/// Turn a config string into a 'static string usable in USB descriptors.
fn leak_str(cell: &'static StaticCell<String<32>>, s: &str) -> &'static str {
    let mut out = String::new();
    let _ = out.push_str(s);
    cell.init(out).as_str()
}

#[allow(
    clippy::large_stack_frames,
    reason = "it's not unusual to allocate larger buffers etc. in main"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let p = esp_hal::init(config);

    logger::init(p.UART1, p.GPIO2);

    let timg0 = TimerGroup::new(p.TIMG0);
    esp_rtos::start(timg0.timer0, p.FROM_CPU_INTR0);

    info!("card-fdd {} starting", env!("CARGO_PKG_VERSION"));

    let (lcd, backlight) = display::init(DisplayPins {
        spi: p.SPI2,
        mosi: p.GPIO35,
        sck: p.GPIO36,
        cs: p.GPIO37,
        dc: p.GPIO34,
        rst: p.GPIO33,
        backlight: p.GPIO38,
    });
    let led = StatusLed::new(p.RMT, p.GPIO21);
    let mut battery = AdcBattery::new(p.ADC1, p.GPIO10);

    // Detect the model before touching the keyboard pins (G8 / G9 / G11 differ between models)
    let (mut i2c0, mut gpio8, mut gpio9) = (p.I2C0, p.GPIO8, p.GPIO9);
    let model = board::detect(i2c0.reborrow(), gpio8.reborrow(), gpio9.reborrow());
    board::set_model(model);
    info!("Board: {}", model.name());
    let keyboard = match model {
        Model::V1 => AnyKeyboard::V1(MatrixKeyboard::new(KeyboardPins {
            a0: gpio8,
            a1: gpio9,
            a2: p.GPIO11,
            in0: p.GPIO13,
            in1: p.GPIO15,
            in2: p.GPIO3,
            in3: p.GPIO4,
            in4: p.GPIO5,
            in5: p.GPIO6,
            in6: p.GPIO7,
        })),
        Model::Adv => {
            let mut bus = adv::i2c_bus(i2c0, gpio8, gpio9);
            // The codec must be configured before audio starts; the bus then belongs to the keyboard
            if let Err(e) = adv::codec::init(&mut bus) {
                log::warn!("ES8311 init failed: {:?}", e);
            }
            AnyKeyboard::Adv(adv::Tca8418Keyboard::new(bus, p.GPIO11))
        }
    };
    spawner.spawn(keyboard_task(keyboard).expect("Failed to spawn keyboard_task"));
    spawner.spawn(
        sound_task(AudioPins {
            i2s: p.I2S0,
            dma: p.DMA_CH0,
            bclk: p.GPIO41,
            ws: p.GPIO43,
            dout: p.GPIO42,
        })
        .expect("Failed to spawn sound_task"),
    );

    let card = sdcard::init(SdPins {
        spi: p.SPI3,
        sck: p.GPIO40,
        miso: p.GPIO39,
        mosi: p.GPIO14,
        cs: p.GPIO12,
    });
    // Holds the listing (up to 128 entries), so it lives in a static. Shared by the UI and USB
    static SD: StaticCell<SharedSd> = StaticCell::new();
    let sd: &'static SharedSd = SD.init(RefCell::new(Browser::new(card)));
    static IMAGE: StaticCell<RefCell<SdImage>> = StaticCell::new();
    let image: &'static RefCell<SdImage> = IMAGE.init(RefCell::new(ImageDisk::new(sd)));

    // UI (holds buffers, so it lives in a static). Reads the card and the config file
    static APP: StaticCell<App> = StaticCell::new();
    let app = APP.init(App::new(lcd, backlight, led, sd, image, config_defaults()));
    app.set_battery(battery.percent());
    app.mount_card();
    info!("Peripherals initialized");

    // USB (started with the settings from the config file)
    let usb_mode = UsbMode::from_name(&app.config.usb_mode).unwrap_or_else(|| {
        log::warn!("Unknown USB mode {}; falling back to ufi-cbi", app.config.usb_mode);
        UsbMode::UfiCbi
    });
    static MANUFACTURER: StaticCell<String<32>> = StaticCell::new();
    static PRODUCT: StaticCell<String<32>> = StaticCell::new();
    let identity = UsbIdentity {
        vid: app.config.usb_vid,
        pid: app.config.usb_pid,
        manufacturer: leak_str(&MANUFACTURER, &app.config.usb_manufacturer),
        product: leak_str(&PRODUCT, &app.config.usb_product),
        ..UsbIdentity::DEFAULT
    };
    info!(
        "Starting USB ({:?}, {:04x}:{:04x}). Further logs go to CDC-ACM and UART",
        usb_mode, identity.vid, identity.pid
    );
    logger::disable_jtag();
    let parts = usb::build(p.USB_FS, p.GPIO20, p.GPIO19, usb_mode, &identity);
    static USB_DEVICE: StaticCell<embassy_usb::UsbDevice<'static, UsbDriver>> = StaticCell::new();
    static MSC: StaticCell<MassStorage> = StaticCell::new();
    static PROCESSOR: StaticCell<Processor> = StaticCell::new();
    let device = USB_DEVICE.init(parts.device);
    let msc = MSC.init(parts.msc);
    let processor = PROCESSOR.init(Processor::new(usb_mode.command_set(), IDENTITY, &usb::HOST));
    spawner.spawn(usb_device_task(device).expect("Failed to spawn usb_device_task"));
    spawner.spawn(msc_task(msc, processor, image).expect("Failed to spawn msc_task"));
    #[cfg(feature = "dev-cdc")]
    spawner.spawn(cdc_task(parts.cdc).expect("Failed to spawn cdc_task"));

    // Insert the previous image
    app.auto_insert();

    let mut last_battery = Instant::now();
    loop {
        // Spin faster while a blank image is being created
        let wait = if app.is_busy() {
            Duration::from_millis(5)
        } else {
            Duration::from_millis(20)
        };
        if let Ok(event) = with_timeout(wait, KEY_EVENTS.receive()).await
            && event.pressed
        {
            app.on_key(&event);
        }
        #[cfg(feature = "dev-cdc")]
        if let Ok(cmd) = usb::cdc::DEV_COMMANDS.try_receive() {
            app.dev_command(&cmd);
        }
        app.tick();
        if last_battery.elapsed() >= Duration::from_secs(5) {
            last_battery = Instant::now();
            app.set_battery(battery.percent());
        }
    }
}
