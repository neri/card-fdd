//! CDC-ACM (virtual serial port) for development.
//!
//! - Sends logs (written to the pipe by [`crate::logger`]) to the host
//! - Accepts one-line commands from the host:
//!   - `download` … reboot into download mode (for flashing)
//!   - `reboot`   … normal reboot
//!   - others     … passed to the UI via [`DEV_COMMANDS`] (`insert <name>`, `eject`, etc.; for testing)

use embassy_futures::join::join;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant, Timer, with_timeout};
use embassy_usb::class::cdc_acm::{CdcAcmClass, Receiver, Sender};
use heapless::Vec;

use super::UsbDriver;
use crate::board::system;
use crate::logger::LOG_PIPE;

/// Development command for the UI (one line).
pub type DevCommand = heapless::String<64>;
pub static DEV_COMMANDS: Channel<CriticalSectionRawMutex, DevCommand, 2> = Channel::new();

pub async fn run(class: CdcAcmClass<'static, UsbDriver>) -> ! {
    let (tx, rx) = class.split();
    join(send_logs(tx), receive_commands(rx)).await;
    unreachable!()
}

async fn send_logs(mut tx: Sender<'static, UsbDriver>) {
    /// If sending does not finish within this time, go back to waiting for a connection.
    const WRITE_TIMEOUT: Duration = Duration::from_secs(1);

    let mut buf = [0u8; 64];
    // Bytes taken from the pipe but not sent yet
    let mut len = 0;
    loop {
        // `wait_connection` only waits for the endpoint to be enabled (USB configured), not for
        // the port to be opened. Logs stay in the pipe until a reader opens the port, so the boot
        // log (written before USB starts) reaches the first reader. Once the pipe is full, newer
        // lines are dropped instead
        tx.wait_connection().await;
        wait_open(&tx).await;
        loop {
            if len == 0 {
                len = LOG_PIPE.read(&mut buf).await;
            }
            // Don't send if the port is closed (ModemManager etc. may open and close it briefly,
            // and data left half-sent would block all subsequent logs). Keep the bytes for the next reader
            if !tx.dtr() {
                break;
            }
            let result = with_timeout(WRITE_TIMEOUT, tx.write_packet(&buf[..len])).await;
            // On a timeout the packet may still be queued in the endpoint, so don't send it again
            len = 0;
            match result {
                Ok(Ok(())) => {}
                Ok(Err(_)) | Err(_) => break,
            }
        }
    }
}

/// Wait until the host has kept the port open (DTR asserted) for a while.
///
/// Right after enumeration, ModemManager etc. open the port briefly and close it again. Waiting out
/// such a probe keeps it from swallowing the boot log.
async fn wait_open(tx: &Sender<'static, UsbDriver>) {
    const POLL: Duration = Duration::from_millis(20);
    const SETTLE: Duration = Duration::from_millis(300);

    let mut open_since: Option<Instant> = None;
    loop {
        if tx.dtr() {
            let since = *open_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= SETTLE {
                return;
            }
        } else {
            open_since = None;
        }
        Timer::after(POLL).await;
    }
}

async fn receive_commands(mut rx: Receiver<'static, UsbDriver>) {
    /// If data stops for longer than this, discard the partially received line.
    const LINE_TIMEOUT: Duration = Duration::from_millis(200);

    let mut line: Vec<u8, 64> = Vec::new();
    let mut packet = [0u8; 64];
    loop {
        rx.wait_connection().await;
        line.clear();
        loop {
            let n = match with_timeout(LINE_TIMEOUT, rx.read_packet(&mut packet)).await {
                Ok(Ok(n)) => n,
                Ok(Err(_)) => break, // Disconnected
                Err(_) => {
                    line.clear();
                    continue;
                }
            };
            for &b in &packet[..n] {
                if b == b'\n' || b == b'\r' {
                    // The host terminal may echo logs back, so ignore log lines
                    if line.first() != Some(&b'[') {
                        run_command(&line);
                    }
                    line.clear();
                } else if line.push(b).is_err() {
                    line.clear();
                }
            }
        }
    }
}

fn run_command(line: &[u8]) {
    match line {
        b"download" => {
            log::info!("CDC: rebooting into download mode");
            system::reboot_to_download_mode();
        }
        b"reboot" => {
            log::info!("CDC: rebooting");
            system::reboot();
        }
        b"" => {}
        _ => {
            let Ok(text) = core::str::from_utf8(line) else {
                return;
            };
            let mut cmd = DevCommand::new();
            if cmd.push_str(text).is_err() || DEV_COMMANDS.try_send(cmd).is_err() {
                log::info!("CDC: cannot accept command");
            }
        }
    }
}
