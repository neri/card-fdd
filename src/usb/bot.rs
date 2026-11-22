//! Bulk-Only Transport (BOT, protocol 0x50).
//!
//! The host sends a CBW (31 bytes) on Bulk OUT, exchanges data on Bulk IN / OUT if needed,
//! and finally the device returns a CSW (13 bytes) on Bulk IN.

use core::sync::atomic::{AtomicBool, Ordering};

use embassy_usb::Handler;
use embassy_usb::control::{InResponse, OutResponse, Recipient, Request, RequestType};
use embassy_usb::driver::{Driver, Endpoint, EndpointError, EndpointIn, EndpointOut};
use embassy_usb::types::InterfaceNumber;

use super::command::{Processor, Stage};
use super::transfer::{DataPipe, MAX_PACKET};
use crate::storage::Disk;

const CBW_SIGNATURE: u32 = 0x4342_5355;
const CSW_SIGNATURE: u32 = 0x5342_5355;
const REQ_GET_MAX_LUN: u8 = 0xFE;
const REQ_MASS_STORAGE_RESET: u8 = 0xFF;

/// Handles BOT class-specific control requests (Get Max LUN, Mass Storage Reset).
pub struct BotControl {
    iface: InterfaceNumber,
    reset: &'static AtomicBool,
}

impl BotControl {
    pub fn new(iface: InterfaceNumber, reset: &'static AtomicBool) -> Self {
        Self { iface, reset }
    }

    fn is_mine(&self, req: &Request) -> bool {
        req.request_type == RequestType::Class
            && req.recipient == Recipient::Interface
            && req.index == u16::from(self.iface.0)
    }
}

impl Handler for BotControl {
    fn control_out(&mut self, req: Request, _data: &[u8]) -> Option<OutResponse> {
        if !self.is_mine(&req) {
            return None;
        }
        match req.request {
            REQ_MASS_STORAGE_RESET => {
                self.reset.store(true, Ordering::Release);
                Some(OutResponse::Accepted)
            }
            _ => Some(OutResponse::Rejected),
        }
    }

    fn control_in<'a>(&'a mut self, req: Request, buf: &'a mut [u8]) -> Option<InResponse<'a>> {
        if !self.is_mine(&req) {
            return None;
        }
        match req.request {
            REQ_GET_MAX_LUN => {
                buf[0] = 0; // Only one LUN
                Some(InResponse::Accepted(&buf[..1]))
            }
            _ => Some(InResponse::Rejected),
        }
    }

    fn configured(&mut self, configured: bool) {
        super::HOST.on_configured(configured);
    }

    fn suspended(&mut self, suspended: bool) {
        super::HOST.on_suspended(suspended);
    }

    fn reset(&mut self) {
        super::HOST.on_reset();
    }
}

pub struct BulkOnly<'d, D: Driver<'d>> {
    pipe: DataPipe<'d, D>,
    reset: &'static AtomicBool,
}

/// A received CBW.
struct Cbw {
    tag: u32,
    data_len: u32,
    dir_in: bool,
    cb: [u8; 16],
    cb_len: usize,
}

impl<'d, D: Driver<'d>> BulkOnly<'d, D> {
    pub fn new(ep_in: D::EndpointIn, ep_out: D::EndpointOut, reset: &'static AtomicBool) -> Self {
        Self {
            pipe: DataPipe::new(ep_in, ep_out),
            reset,
        }
    }

    /// Keep receiving and processing commands.
    pub async fn run(&mut self, processor: &mut Processor, disk: &mut impl Disk) -> ! {
        loop {
            self.pipe.ep_out.wait_enabled().await;
            log::info!("USB mass storage (BOT) enabled");
            loop {
                match self.transaction(processor, disk).await {
                    Ok(()) => {}
                    Err(EndpointError::Disabled) => break,
                    Err(EndpointError::BufferOverflow) => log::warn!("BOT: buffer overflow"),
                }
            }
            log::info!("USB mass storage (BOT) disabled");
        }
    }

    async fn transaction(&mut self, processor: &mut Processor, disk: &mut impl Disk) -> Result<(), EndpointError> {
        let mut packet = [0u8; MAX_PACKET];
        processor.host().stats.set_stage(Stage::WaitCommand);
        let n = self.pipe.ep_out.read(&mut packet).await?;
        processor.host().stats.set_stage(Stage::Data);
        if self.reset.swap(false, Ordering::AcqRel) {
            processor.reset();
        }
        let Some(cbw) = parse_cbw(&packet[..n]) else {
            log::warn!("BOT: invalid CBW ({} bytes)", n);
            return Ok(());
        };

        let host_len = cbw.data_len as usize;
        let result = self
            .pipe
            .execute(processor, disk, &cbw.cb[..cbw.cb_len], host_len, cbw.dir_in)
            .await?;

        processor.host().stats.set_stage(Stage::Status);
        let mut csw = [0u8; 13];
        csw[0..4].copy_from_slice(&CSW_SIGNATURE.to_le_bytes());
        csw[4..8].copy_from_slice(&cbw.tag.to_le_bytes());
        csw[8..12].copy_from_slice(&((host_len - result.done) as u32).to_le_bytes());
        csw[12] = if result.ok { 0 } else { 1 };
        self.pipe.ep_in.write(&csw).await
    }
}

fn parse_cbw(p: &[u8]) -> Option<Cbw> {
    if p.len() != 31 || u32::from_le_bytes([p[0], p[1], p[2], p[3]]) != CBW_SIGNATURE {
        return None;
    }
    let cb_len = usize::from(p[14] & 0x1F);
    if cb_len == 0 || cb_len > 16 {
        return None;
    }
    let mut cb = [0u8; 16];
    cb[..cb_len].copy_from_slice(&p[15..15 + cb_len]);
    Some(Cbw {
        tag: u32::from_le_bytes([p[4], p[5], p[6], p[7]]),
        data_len: u32::from_le_bytes([p[8], p[9], p[10], p[11]]),
        dir_in: p[12] & 0x80 != 0,
        cb,
        cb_len,
    })
}
