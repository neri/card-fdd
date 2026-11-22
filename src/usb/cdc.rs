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
use embassy_time::{Duration, with_timeout};
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
    loop {
        tx.wait_connection().await;
        loop {
            let n = LOG_PIPE.read(&mut buf).await;
            // Don't send if the port is closed (ModemManager etc. may open and close it briefly,
            // and data left half-sent would block all subsequent logs)
            if !tx.dtr() {
                break;
            }
            match with_timeout(WRITE_TIMEOUT, tx.write_packet(&buf[..n])).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) | Err(_) => break,
            }
        }
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
