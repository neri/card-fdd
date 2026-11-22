//! CBI (Control/Bulk/Interrupt) transport with command completion interrupt (protocol 0x00).
//!
//! Most real USB floppy drives use this. Windows treats UFI (subclass 0x04) as a floppy only over
//! CBI (no standard driver binds to Bulk-Only UFI).
//!
//! 1. The host sends the command block with the ADSC class request (bRequest = 0x00) over a control transfer
//! 2. Data is exchanged on Bulk IN / OUT if needed
//! 3. The device returns the completion status (for UFI, the 2 bytes ASC, ASCQ) on Interrupt IN

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_usb::Handler;
use embassy_usb::control::{InResponse, OutResponse, Recipient, Request, RequestType};
use embassy_usb::driver::{Driver, Endpoint, EndpointError, EndpointIn};
use embassy_usb::types::InterfaceNumber;

use super::command::{self, Processor, Sense, Stage};
use super::control_status;
use super::sense_trace;
use super::transfer::{self, DataPipe};
use crate::storage::Disk;

const REQ_ADSC: u8 = 0x00;

/// Command block received via ADSC.
#[derive(Clone, Copy)]
pub struct AdscCommand {
    cb: [u8; 16],
    len: usize,
    status_token: Option<u32>,
}

/// Channel that passes commands from the control transfer handler to the MSC task.
pub type AdscChannel = Channel<CriticalSectionRawMutex, AdscCommand, 1>;

/// Receives ADSC (Accept Device-Specific Command).
pub struct CbiControl {
    iface: InterfaceNumber,
    commands: &'static AdscChannel,
}

impl CbiControl {
    pub fn new(iface: InterfaceNumber, commands: &'static AdscChannel) -> Self {
        Self { iface, commands }
    }

    fn is_mine(&self, req: &Request) -> bool {
        req.request_type == RequestType::Class
            && req.recipient == Recipient::Interface
            && req.index == u16::from(self.iface.0)
    }
}

impl Handler for CbiControl {
    fn control_out(&mut self, req: Request, data: &[u8]) -> Option<OutResponse> {
        if !self.is_mine(&req) {
            return None;
        }
        if req.request != REQ_ADSC || data.is_empty() {
            return Some(OutResponse::Rejected);
        }
        let len = data.len().min(16);
        let status_token = (data.len() == 12
            && matches!(
                data[0],
                command::op::TEST_UNIT_READY
                    | command::op::READ_CAPACITY_10
                    | command::op::MODE_SENSE_10
                    | command::op::READ_10
            ))
        .then(control_status::defer);
        let mut cmd = AdscCommand { cb: [0; 16], len, status_token };
        cmd.cb[..len].copy_from_slice(&data[..len]);
        // Reject if the previous command is still being processed (the host should wait for completion before sending the next)
        match self.commands.try_send(cmd) {
            Ok(()) => Some(OutResponse::Accepted),
            Err(_) => {
                if status_token.is_some() {
                    control_status::cancel();
                }
                log::warn!("CBI: received ADSC while processing the previous command");
                super::command::UsbStats::inc(&super::HOST.stats.rejected);
                Some(OutResponse::Rejected)
            }
        }
    }

    fn control_in<'a>(&'a mut self, req: Request, _buf: &'a mut [u8]) -> Option<InResponse<'a>> {
        self.is_mine(&req).then_some(InResponse::Rejected)
    }

    fn configured(&mut self, configured: bool) {
        if !configured {
            sense_trace::end(sense_trace::State::Reset);
        }
        super::HOST.on_configured(configured);
    }

    fn suspended(&mut self, suspended: bool) {
        super::HOST.on_suspended(suspended);
    }

    fn reset(&mut self) {
        control_status::cancel();
        sense_trace::end(sense_trace::State::Reset);
        super::HOST.on_reset();
        // A new connection is starting, so discard any commands left over from the previous connection
        self.commands.clear();
    }
}

pub struct Cbi<'d, D: Driver<'d>> {
    pipe: DataPipe<'d, D>,
    ep_int: D::EndpointIn,
    commands: &'static AdscChannel,
    /// Discard unread data in an IN endpoint (driver-dependent; see [`super::discard_pending_in`]).
    discard_pending_in: fn(usize) -> bool,
}

impl<'d, D: Driver<'d>> Cbi<'d, D> {
    pub fn new(
        ep_in: D::EndpointIn,
        ep_out: D::EndpointOut,
        ep_int: D::EndpointIn,
        commands: &'static AdscChannel,
        discard_pending_in: fn(usize) -> bool,
    ) -> Self {
        Self {
            pipe: DataPipe::new(ep_in, ep_out),
            ep_int,
            commands,
            discard_pending_in,
        }
    }

    /// Keep receiving and processing commands.
    pub async fn run(&mut self, processor: &mut Processor, disk: &mut impl Disk) -> ! {
        loop {
            self.ep_int.wait_enabled().await;
            // Do not empty the channel here. Windows sends the first ADSC right after configuration, so
            // commands that arrived before this task woke up would be discarded, and after a ~20 second timeout
            // the connection would restart. Stale commands are discarded by CbiControl on bus reset.
            log::info!("USB mass storage (CBI) enabled");
            loop {
                match self.transaction(processor, disk).await {
                    Ok(()) => {}
                    Err(EndpointError::Disabled) => break,
                    Err(EndpointError::BufferOverflow) => log::warn!("CBI: buffer overflow"),
                }
            }
            log::info!("USB mass storage (CBI) disabled");
        }
    }

    async fn transaction(&mut self, processor: &mut Processor, disk: &mut impl Disk) -> Result<(), EndpointError> {
        let stats = &processor.host().stats;
        stats.set_stage(Stage::WaitCommand);
        let cmd = self.commands.receive().await;
        let cb = &cmd.cb[..cmd.len];
        stats.set_stage(Stage::Data);

        // A new command arrived = the host has finished the previous command. Discard the previous completion
        // interrupt left unread (Windows does not read the one for REQUEST SENSE) and any Bulk IN data
        let int_ep = self.ep_int.info().addr.index();
        let bulk_ep = self.pipe.ep_in.info().addr.index();
        stats.int_ep.store(int_ep as u32, core::sync::atomic::Ordering::Relaxed);
        stats.bulk_in_ep.store(bulk_ep as u32, core::sync::atomic::Ordering::Relaxed);
        sense_trace::sample();
        sense_trace::end(sense_trace::State::Superseded);
        let discarded_int = (self.discard_pending_in)(int_ep);
        let discarded_bulk = (self.discard_pending_in)(bulk_ep);
        if discarded_int {
            super::command::UsbStats::inc(&stats.discarded_int);
        }
        if discarded_bulk {
            super::command::UsbStats::inc(&stats.discarded_bulk);
        }
        if discarded_int || discarded_bulk {
            log::debug!("CBI: discarded unread IN data (int={}, bulk={})", discarded_int, discarded_bulk);
        }

        if is_command_block_reset(cb) {
            // CBI reset (a special form of SEND DIAGNOSTIC). No status is returned
            log::info!("CBI: command block reset");
            super::command::UsbStats::inc(&processor.host().stats.resets);
            processor.reset();
            return Ok(());
        }

        let expected = command::expected_transfer(cb, transfer::block_size(disk));
        let request_sense = cb[0] == command::op::REQUEST_SENSE;
        if request_sense {
            // Processing REQUEST SENSE copies and then clears the current sense data.
            sense_trace::begin(processor.sense());
        }
        let outcome = self.pipe.prepare(processor, disk, cb);
        if let Some(token) = cmd.status_token {
            let stall = !outcome.ok
                && matches!(processor.sense(), Sense::MEDIUM_CHANGED | Sense::NOT_READY_NO_MEDIUM);
            let delivered = control_status::reply(token, stall);
            // A stalled or abandoned ADSC has neither a bulk data phase nor a completion interrupt.
            if stall || !delivered {
                return Ok(());
            }
        }
        let result = self.pipe.execute_prepared(processor, disk, cb, expected, outcome).await?;
        if request_sense && result.done != 0 {
            sense_trace::BULK.queued(bulk_ep);
        }

        // UFI completion interrupt: 00 00 on success, otherwise the sense data's ASC / ASCQ
        processor.host().stats.set_stage(Stage::Status);
        let status = if result.ok {
            [0, 0]
        } else {
            let sense = processor.sense();
            [sense.asc, sense.ascq]
        };
        self.ep_int.write(&status).await?;
        if request_sense {
            sense_trace::INTERRUPT.queued(int_ep);
            sense_trace::sample();
        }
        Ok(())
    }
}

/// Command block reset: 1D 04 FF FF ... FF.
fn is_command_block_reset(cb: &[u8]) -> bool {
    cb.len() >= 2 && cb[0] == 0x1D && cb[1] == 0x04 && cb[2..].iter().all(|&b| b == 0xFF)
}
