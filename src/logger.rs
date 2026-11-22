//! Log output.
//!
//! Destinations:
//! - UART on the Grove port (TX = G2, 115200bps)
//! - USB-Serial-JTAG (until USB is switched to OTG)
//! - Development CDC-ACM (feature `dev-cdc`, after USB is switched to OTG)
//!
//! Note: UART writes are currently done blocking inside a critical section.
//! If this stalls USB transfers, switch to a ring buffer + sender task.

use core::cell::RefCell;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};

use critical_section::Mutex;
use esp_hal::Blocking;
use esp_hal::peripherals::{GPIO2, UART1};
use esp_hal::uart::{Config as UartConfig, UartTx};
use log::{LevelFilter, Log, Metadata, Record};

static UART: Mutex<RefCell<Option<UartTx<'static, Blocking>>>> = Mutex::new(RefCell::new(None));
static LOGGER: Logger = Logger;
static JTAG_ENABLED: AtomicBool = AtomicBool::new(true);

/// Pipe for logs sent to CDC-ACM. Overflow is dropped.
#[cfg(feature = "dev-cdc")]
pub static LOG_PIPE: embassy_sync::pipe::Pipe<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    4096,
> = embassy_sync::pipe::Pipe::new();

/// Log level. Set with `ESP_LOG` in `.cargo/esp-config.toml` (default: info).
fn level_from_env() -> LevelFilter {
    match option_env!("ESP_LOG") {
        Some("error") => LevelFilter::Error,
        Some("warn") => LevelFilter::Warn,
        Some("debug") => LevelFilter::Debug,
        Some("trace") => LevelFilter::Trace,
        Some("off") => LevelFilter::Off,
        _ => LevelFilter::Info,
    }
}

/// Initialize the logger. Call exactly once, before anything else.
pub fn init(uart: UART1<'static>, tx: GPIO2<'static>) {
    let uart = UartTx::new(uart, UartConfig::default())
        .expect("Failed to configure UART1")
        .with_tx(tx);
    critical_section::with(|cs| UART.borrow_ref_mut(cs).replace(uart));

    // This only fails on double initialization, so it is safe to ignore.
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(level_from_env());
}

/// Stop output to USB-Serial-JTAG (call before switching USB to OTG).
pub fn disable_jtag() {
    JTAG_ENABLED.store(false, Ordering::Release);
}

struct Logger;

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let ms = embassy_time::Instant::now().as_millis();
        let mut line: heapless::String<256> = heapless::String::new();
        let overflow = write!(
            line,
            "[{:>6}.{:03}] {:<5} {}\r\n",
            ms / 1000,
            ms % 1000,
            record.level(),
            record.args()
        )
        .is_err();
        if overflow {
            // Truncate overly long lines (without cutting in the middle of a UTF-8 character)
            let mut cut = 250.min(line.len());
            while !line.is_char_boundary(cut) {
                cut -= 1;
            }
            line.truncate(cut);
            let _ = line.push_str("...\r\n");
        }

        if JTAG_ENABLED.load(Ordering::Acquire) {
            esp_println::print!("{}", line);
        }

        #[cfg(feature = "dev-cdc")]
        {
            let mut bytes = line.as_bytes();
            while !bytes.is_empty() {
                match LOG_PIPE.try_write(bytes) {
                    Ok(n) => bytes = &bytes[n..],
                    Err(_) => break, // Drop if full
                }
            }
        }

        critical_section::with(|cs| {
            if let Some(uart) = UART.borrow_ref_mut(cs).as_mut() {
                let mut bytes = line.as_bytes();
                while !bytes.is_empty() {
                    match uart.write(bytes) {
                        Ok(n) => bytes = &bytes[n..],
                        Err(_) => break,
                    }
                }
            }
        });
    }

    fn flush(&self) {}
}
